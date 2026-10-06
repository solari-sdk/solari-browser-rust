//! [`Client`] plus the `sessions` / `profiles` / `proxy` resource namespaces.
//! Ports `sdk/src/index.ts` (the TypeScript reference) minus `launch()`.
//!
//! One deliberate divergence from the reference: the TS SDK wraps every session
//! endpoint in a Node-side `LocalProxy` loopback rewrite. This crate returns the
//! UPSTREAM `wsEndpoint` / `cdpEndpoint` verbatim — there is no proxy to route
//! through, and callers connect straight to the gateway.

use std::time::Duration;

use serde_json::json;
use url::Url;

use crate::error::{api_error, SolariError, SolariErrorCode};
use crate::http::HttpTransport;
use crate::types::{
    CreateSessionRequest, CreateSessionResponse, Profile, ProfileSaveResponse, ProfileSaveResult,
    ProxyCountries, ProxySpec, ReplayUrl, ReplayUrlResponse, Session, SessionView, SolariRegion,
    StorageState,
};

pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 2;
pub(crate) const DEFAULT_BACKOFF_MS: u64 = 500;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 90_000;
const STORAGE_STATE_FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// Options for constructing a [`Client`].
///
/// There is no environment-variable fallback by design — the API key and base
/// URL are always explicit constructor arguments.
#[derive(Clone)]
pub struct ClientOptions {
    pub api_key: String,
    pub base_url: String,
    /// Total attempts per request (including the first). Default 2.
    pub max_attempts: u32,
    /// Fixed delay between attempts. Default 500ms.
    pub backoff_ms: u64,
    /// Per-request timeout. Default 90000ms.
    pub timeout_ms: u64,
}

/// Hand-written so the API key is never printed. Do not derive this.
impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("max_attempts", &self.max_attempts)
            .field("backoff_ms", &self.backoff_ms)
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

impl ClientOptions {
    /// Point the client at an explicit base URL (staging / self-hosted gateway).
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        ClientOptions {
            api_key: api_key.into(),
            base_url: base_url.into(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_ms: DEFAULT_BACKOFF_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// Point the client at a region's public API. [`SolariRegion::UsWest`]
    /// resolves to `https://api.getsolari.com`.
    pub fn for_region(api_key: impl Into<String>, region: SolariRegion) -> Self {
        ClientOptions::new(api_key, region.base_url())
    }

    /// Point the client at the default region (`us-west`).
    pub fn default_region(api_key: impl Into<String>) -> Self {
        ClientOptions::for_region(api_key, SolariRegion::default())
    }

    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    pub fn backoff_ms(mut self, ms: u64) -> Self {
        self.backoff_ms = ms;
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
}

/// Options accepted by `POST /sessions`.
///
/// Falsy fields are omitted from the request body; if every field is falsy the
/// request is sent with no body at all.
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    /// Attach a stored browser profile (cookies + localStorage).
    pub profile_id: Option<String>,
    /// Enable session recording. Off by default.
    pub recording: bool,
    /// Enable the runtime stealth shim. Off by default.
    pub stealth: bool,
    /// Enable managed captcha solving. Requires `stealth`.
    pub captcha: bool,
    /// Request managed proxy egress. Requires `stealth`.
    pub proxy: Option<ProxySpec>,
}

impl CreateSessionOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn profile_id(mut self, id: impl Into<String>) -> Self {
        self.profile_id = Some(id.into());
        self
    }

    pub fn recording(mut self, on: bool) -> Self {
        self.recording = on;
        self
    }

    pub fn stealth(mut self, on: bool) -> Self {
        self.stealth = on;
        self
    }

    pub fn captcha(mut self, on: bool) -> Self {
        self.captcha = on;
        self
    }

    pub fn proxy(mut self, spec: impl Into<ProxySpec>) -> Self {
        self.proxy = Some(spec.into());
        self
    }
}

/// Talks the Solari Browser control-plane REST API.
#[derive(Debug, Clone)]
pub struct Client {
    http: HttpTransport,
}

impl Client {
    pub fn new(opts: ClientOptions) -> Result<Self, SolariError> {
        if opts.api_key.is_empty() {
            return Err(SolariError::config("Solari: apiKey is required"));
        }
        if opts.base_url.is_empty() {
            return Err(SolariError::config("Solari: baseUrl is required"));
        }
        let base_url = opts.base_url.trim_end_matches('/').to_string();
        let http = HttpTransport::new(
            opts.api_key,
            base_url,
            opts.max_attempts,
            opts.backoff_ms,
            opts.timeout_ms,
        )?;
        Ok(Client { http })
    }

    /// Session lifecycle: create / get / release / replay.
    pub fn sessions(&self) -> Sessions<'_> {
        Sessions { client: self }
    }

    /// Stored browser profiles.
    pub fn profiles(&self) -> Profiles<'_> {
        Profiles { client: self }
    }

    /// Managed proxy metadata.
    pub fn proxy(&self) -> Proxy<'_> {
        Proxy { client: self }
    }

    /// The resolved API base URL (trailing slash stripped).
    pub fn base_url(&self) -> &str {
        self.http.base_url()
    }
}

// ---------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------

/// The `sessions` namespace. Obtain via [`Client::sessions`].
pub struct Sessions<'a> {
    client: &'a Client,
}

impl Sessions<'_> {
    /// `POST /sessions` — acquire a browser session.
    pub async fn create(&self, options: CreateSessionOptions) -> Result<Session, SolariError> {
        let req = CreateSessionRequest {
            // TS: `if (options?.profileId)` — falsy (unset OR empty) is omitted.
            profile_id: options
                .profile_id
                .clone()
                .filter(|p| !p.is_empty()),
            recording: options.recording,
            stealth: options.stealth,
            captcha: options.captcha,
            proxy: options.proxy.clone(),
        };
        let body = serde_json::to_value(&req)
            .map_err(|e| SolariError::protocol(format!("Solari: bad create body: {e}")))?;
        // Omit the body entirely when nothing was set.
        let body = match body.as_object() {
            Some(o) if o.is_empty() => None,
            _ => Some(body),
        };

        let res = self
            .client
            .http
            .request_with_key(
                "POST",
                "/sessions",
                body.as_ref(),
                // A create allocates a slot and launches a browser before it can
                // answer, so a response lost after that point would make a retry
                // a SECOND session.
                Some(&crate::http::new_idempotency_key()),
            )
            .await?;
        if !res.ok() {
            return Err(api_error("POST", "/sessions", res.status, &res.body));
        }

        let data: CreateSessionResponse = serde_json::from_str(&res.body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected session response: {e}"))
        })?;

        let (session_id, ws_endpoint) = match (data.session_id.clone(), data.ws_endpoint.clone()) {
            (Some(id), Some(ws)) if !id.is_empty() && !ws.is_empty() => (id, ws),
            _ => {
                return Err(SolariError::protocol(format!(
                    "Solari: unexpected session response: {}",
                    res.body
                )))
            }
        };

        let cdp_endpoint = data
            .cdp_endpoint
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| derive_cdp_from_ws(&ws_endpoint));

        let expires_at = data
            .expires_at
            .clone()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| iso8601_utc_in(Duration::from_secs(60 * 60)));

        let mut session = Session {
            id: session_id,
            ws_endpoint,
            cdp_endpoint,
            expires_at,
            storage_state: None,
            proxy: data.proxy,
        };

        // Storage state is only populated when a profile was requested — TS
        // gates on `options?.profileId !== undefined`.
        if options.profile_id.is_some() {
            session.storage_state = Some(match data.storage_state_url {
                // `storageStateUrl` present → fetch the presigned object.
                Some(Some(ssu)) => self.fetch_presigned_storage_state(ssu.url.as_deref()).await?,
                // Explicit null → profile exists but is empty.
                Some(None) => None,
                // Absent → fall back to any inline state (TS: `?? null`).
                None => data.storage_state.flatten(),
            });
        }

        Ok(session)
    }

    /// `GET /sessions/:id`.
    pub async fn get(&self, id: &str) -> Result<SessionView, SolariError> {
        let path = format!("/sessions/{}", enc(id));
        let res = self.client.http.request("GET", &path, None).await?;
        if !res.ok() {
            return Err(api_error("GET", &path, res.status, &res.body));
        }
        serde_json::from_str(&res.body)
            .map_err(|e| SolariError::protocol(format!("Solari: unexpected session view: {e}")))
    }

    /// `DELETE /sessions/:id` — release the session. A bare 404 is tolerated
    /// (the session already ended).
    ///
    /// A 404 carrying [`SolariErrorCode::InvalidSessionId`] is an error. The
    /// gateway acks 204 for any authentic session id, including one whose
    /// session has already ended — the handler is idempotent by design and never
    /// consults the pool before acking. So a 404 does not mean "already
    /// released"; it means the gateway refused the id (malformed, forged, or
    /// another org's) and released nothing, leaving the pool slot held until
    /// orphan-grace. Treating that as success is what made these releases leak
    /// slots silently.
    ///
    /// A bare 404 with no code stays tolerated — that is a pre-InvalidSessionId
    /// gateway, where a 404 may legitimately mean the session is already gone.
    /// Mirrors `releaseRejection` in sdk/src/index.ts.
    ///
    /// Unlike the TS SDK there is no fire-and-forget variant; this is the
    /// equivalent of `releaseAndWait`. Spawn it yourself if you don't want to
    /// await the round-trip.
    pub async fn release(&self, id: &str) -> Result<(), SolariError> {
        let path = format!("/sessions/{}", enc(id));
        let res = self.client.http.request("DELETE", &path, None).await?;
        if res.ok() {
            return Ok(());
        }
        let err = api_error("DELETE", &path, res.status, &res.body);
        if res.status == 404 && err.code() != Some(&SolariErrorCode::InvalidSessionId) {
            return Ok(());
        }
        Err(err)
    }

    /// `GET /sessions/:id/replay-url` — presigned URL for the session's replay.
    /// Available ~1-3s after [`Sessions::release`].
    pub async fn replay_url(&self, id: &str) -> Result<ReplayUrl, SolariError> {
        let path = format!("/sessions/{}/replay-url", enc(id));
        let res = self.client.http.request("GET", &path, None).await?;
        if !res.ok() {
            return Err(api_error("GET", &path, res.status, &res.body));
        }
        let data: ReplayUrlResponse = serde_json::from_str(&res.body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected replay-url response: {e}"))
        })?;
        let url = data.url.filter(|u| !u.is_empty()).ok_or_else(|| {
            SolariError::protocol(format!(
                "Solari: unexpected replay-url response: {}",
                res.body
            ))
        })?;
        Ok(ReplayUrl {
            url,
            expires_in_seconds: data.expires_in_seconds.unwrap_or(0),
            content_encoding: data
                .content_encoding
                .unwrap_or_else(|| "gzip".to_string()),
        })
    }

    /// Download the session's replay as NDJSON bytes. May or may not
    /// actually be gzip despite what was uploaded — GCS decompresses
    /// transparently on an ordinary GET, so check `content_encoding` (now
    /// provider-accurate) before attempting to decompress.
    pub async fn download_replay(&self, id: &str) -> Result<Vec<u8>, SolariError> {
        let replay = self.replay_url(id).await?;
        let (status, bytes) = self.client.http.get_presigned_bytes(&replay.url).await?;
        if !(200..300).contains(&status) {
            return Err(SolariError::Api {
                status,
                code: None,
                message: format!("Solari: replay download failed: {status}"),
            });
        }
        Ok(bytes)
    }

    /// GET a presigned storage-state object. `None` url → `None` state.
    async fn fetch_presigned_storage_state(
        &self,
        url: Option<&str>,
    ) -> Result<Option<StorageState>, SolariError> {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return Ok(None);
        };
        let res = self
            .client
            .http
            .get_presigned(url, STORAGE_STATE_FETCH_TIMEOUT)
            .await
            .map_err(|e| {
                SolariError::transport(format!("Solari: failed to fetch storageState: {e}"))
            })?;
        if !res.ok() {
            let snippet: String = res.body.chars().take(256).collect();
            return Err(SolariError::Api {
                status: res.status,
                code: None,
                message: format!(
                    "Solari: storageState fetch returned {}: {snippet}",
                    res.status
                ),
            });
        }
        serde_json::from_str(&res.body).map(Some).map_err(|e| {
            SolariError::protocol(format!(
                "Solari: storageState response was not valid JSON: {e}"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// profiles
// ---------------------------------------------------------------------------

/// The `profiles` namespace. Obtain via [`Client::profiles`].
pub struct Profiles<'a> {
    client: &'a Client,
}

impl Profiles<'_> {
    /// `GET /profiles`.
    pub async fn list(&self) -> Result<Vec<Profile>, SolariError> {
        let res = self.client.http.request("GET", "/profiles", None).await?;
        if !res.ok() {
            return Err(api_error("GET", "/profiles", res.status, &res.body));
        }
        serde_json::from_str(&res.body)
            .map_err(|e| SolariError::protocol(format!("Solari: unexpected profiles response: {e}")))
    }

    /// `POST /profiles`.
    pub async fn create(&self, name: impl Into<String>) -> Result<Profile, SolariError> {
        let body = json!({ "name": name.into() });
        let res = self
            .client
            .http
            .request("POST", "/profiles", Some(&body))
            .await?;
        if !res.ok() {
            return Err(api_error("POST", "/profiles", res.status, &res.body));
        }
        serde_json::from_str(&res.body)
            .map_err(|e| SolariError::protocol(format!("Solari: unexpected profile response: {e}")))
    }

    /// `DELETE /profiles/:id`. 404 is tolerated (already gone).
    pub async fn delete(&self, id: &str) -> Result<(), SolariError> {
        let path = format!("/profiles/{}", enc(id));
        let res = self.client.http.request("DELETE", &path, None).await?;
        if !res.ok() && res.status != 404 {
            return Err(api_error("DELETE", &path, res.status, &res.body));
        }
        Ok(())
    }

    /// `POST /profiles/:id/save` — persist a storage state onto the profile.
    pub async fn save(
        &self,
        id: &str,
        storage_state: &StorageState,
    ) -> Result<ProfileSaveResult, SolariError> {
        let path = format!("/profiles/{}/save", enc(id));
        let state = serde_json::to_value(storage_state)
            .map_err(|e| SolariError::protocol(format!("Solari: bad storageState: {e}")))?;
        let body = json!({ "storageState": state });
        let res = self.client.http.request("POST", &path, Some(&body)).await?;
        if !res.ok() {
            return Err(api_error("POST", &path, res.status, &res.body));
        }
        let data: ProfileSaveResponse = serde_json::from_str(&res.body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected profile save response: {e}"))
        })?;
        Ok(ProfileSaveResult {
            version: data.version.unwrap_or(0),
            size_bytes: data.size_bytes.unwrap_or(0),
        })
    }
}

// ---------------------------------------------------------------------------
// proxy
// ---------------------------------------------------------------------------

/// The `proxy` namespace. Obtain via [`Client::proxy`].
pub struct Proxy<'a> {
    client: &'a Client,
}

impl Proxy<'_> {
    /// `GET /proxy/countries` — supported egress countries, and whether managed
    /// proxy egress is configured on this gateway at all.
    pub async fn countries(&self) -> Result<ProxyCountries, SolariError> {
        let res = self
            .client
            .http
            .request("GET", "/proxy/countries", None)
            .await?;
        if !res.ok() {
            return Err(api_error("GET", "/proxy/countries", res.status, &res.body));
        }
        serde_json::from_str(&res.body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected proxy/countries response: {e}"))
        })
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Percent-encode a path segment. Session ids are HMAC composites (`.`-joined
/// base64url) and profile ids are opaque, so only a conservative set of
/// unreserved characters is passed through.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'!' | b'*'
            | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Rewrite a Playwright ws endpoint into its raw-CDP sibling: `/ws/<id>` →
/// `/cdp/<id>`. Mirrors `deriveCdpFromWs`; returns the input unchanged when it
/// does not parse or does not match.
pub(crate) fn derive_cdp_from_ws(ws_endpoint: &str) -> String {
    match Url::parse(ws_endpoint) {
        Ok(mut u) => {
            let path = u.path().to_string();
            if let Some(rest) = path.strip_prefix("/ws/") {
                u.set_path(&format!("/cdp/{rest}"));
                u.to_string()
            } else {
                ws_endpoint.to_string()
            }
        }
        Err(_) => ws_endpoint.to_string(),
    }
}

/// An ISO-8601 UTC timestamp `d` from now — the fallback when the API omits
/// `expiresAt` (TS: `new Date(Date.now() + 60*60_000).toISOString()`).
fn iso8601_utc_in(d: Duration) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_secs())
        .unwrap_or(0);
    iso8601_utc(now as i64 + d.as_secs() as i64)
}

/// Format a Unix timestamp as `YYYY-MM-DDTHH:MM:SS.000Z`.
pub(crate) fn iso8601_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let sod = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days-since-epoch → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
