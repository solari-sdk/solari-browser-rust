//! Wire types for the Solari Browser control plane. serde field names match the
//! JSON wire EXACTLY (camelCase where the API is camelCase).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Regions
// ---------------------------------------------------------------------------

/// Supported Solari regions. More coming soon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SolariRegion {
    /// `https://api.getsolari.com`
    #[default]
    UsWest,
}

impl SolariRegion {
    /// The API base URL this region resolves to.
    pub fn base_url(&self) -> &'static str {
        match self {
            SolariRegion::UsWest => "https://api.getsolari.com",
        }
    }
}

// ---------------------------------------------------------------------------
// Storage state (cookies + localStorage)
// ---------------------------------------------------------------------------

/// Cookie `sameSite` policy, as Playwright spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

/// One cookie in a [`StorageState`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
}

/// One `localStorage` key/value pair.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalStorageEntry {
    pub name: String,
    pub value: String,
}

/// One origin's `localStorage` in a [`StorageState`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageOrigin {
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_storage: Option<Vec<LocalStorageEntry>>,
}

/// A browser profile's persisted state — Playwright's `storageState` shape.
/// Unknown keys round-trip through [`StorageState::extra`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookies: Option<Vec<Cookie>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origins: Option<Vec<StorageOrigin>>,
    /// Any additional keys the wire carried (TS index signature).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Managed proxy
// ---------------------------------------------------------------------------

/// Managed proxy tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyTier {
    /// Rotating residential IPs (default).
    Residential,
    /// Fixed ISP IP.
    Static,
    /// Carrier / mobile IPs.
    Mobile,
}

/// Managed proxy egress request.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRequest {
    /// ISO-3166-1 alpha-2, lowercase. Defaults to `"us"` server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Proxy tier. Defaults to [`ProxyTier::Residential`] server-side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<ProxyTier>,
    /// Pin egress to a specific ASN (e.g. `"20057"` for AT&T Mobility).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
    /// Sticky-session id (alnum + dash, <=32 chars). Pins the egress IP for
    /// `session_duration` minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Sticky lifetime in minutes (1-30, default 10). Only with `session`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_duration: Option<u32>,
    /// US-only geo narrowing (e.g. `"california"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// US-only city pin (e.g. `"los_angeles"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
}

impl ProxyRequest {
    /// A request pinned to a country code.
    pub fn country(code: impl Into<String>) -> Self {
        ProxyRequest {
            country: Some(code.into()),
            ..Default::default()
        }
    }

    pub fn tier(mut self, tier: ProxyTier) -> Self {
        self.tier = Some(tier);
        self
    }

    pub fn asn(mut self, asn: impl Into<String>) -> Self {
        self.asn = Some(asn.into());
        self
    }

    /// Pin the egress IP under a sticky-session id.
    pub fn session(mut self, id: impl Into<String>) -> Self {
        self.session = Some(id.into());
        self
    }

    pub fn session_duration(mut self, minutes: u32) -> Self {
        self.session_duration = Some(minutes);
        self
    }

    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    pub fn city(mut self, city: impl Into<String>) -> Self {
        self.city = Some(city.into());
        self
    }
}

/// What to send as the `proxy` field of `POST /sessions`.
///
/// Mirrors the TS union `string | ProxyRequest | "off" | "smart"`:
///
/// ```
/// # use solari_browser::{ProxySpec, ProxyRequest, ProxyTier};
/// ProxySpec::country("us");                                    // proxy: "us"
/// ProxySpec::off();                                            // proxy: "off"
/// ProxySpec::smart();                                          // proxy: "smart"
/// ProxySpec::from(ProxyRequest::country("us").tier(ProxyTier::Static));
/// ```
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ProxySpec {
    /// A bare string: a country code, or the literals `"off"` / `"smart"`.
    Shorthand(String),
    /// A full egress request object.
    Request(ProxyRequest),
}

impl ProxySpec {
    /// Shorthand country pin, e.g. `ProxySpec::country("gb")`.
    pub fn country(code: impl Into<String>) -> Self {
        ProxySpec::Shorthand(code.into())
    }

    /// Explicitly disable managed proxy egress (`proxy: "off"`).
    pub fn off() -> Self {
        ProxySpec::Shorthand("off".to_string())
    }

    /// Let the gateway sweep proxy strategies on block (`proxy: "smart"`).
    pub fn smart() -> Self {
        ProxySpec::Shorthand("smart".to_string())
    }
}

impl From<ProxyRequest> for ProxySpec {
    fn from(r: ProxyRequest) -> Self {
        ProxySpec::Request(r)
    }
}

/// Resolved proxy credentials returned on the session response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedProxyConfig {
    pub server: String,
    pub username: String,
    pub password: String,
    pub timezone_id: String,
    pub country: String,
    #[serde(default)]
    pub tier: Option<ProxyTier>,
}

/// `GET /proxy/countries` — supported egress countries, plus whether the
/// gateway actually has residential credentials configured.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProxyCountries {
    /// False when this gateway has no residential proxy credentials — sending a
    /// `proxy` spec will fail.
    #[serde(default)]
    pub enabled: bool,
    /// Supported ISO-3166-1 alpha-2 country codes, lowercase.
    #[serde(default)]
    pub countries: Vec<String>,
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// A live browser session.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    /// Playwright wire-protocol endpoint (upstream, as returned by the API).
    pub ws_endpoint: String,
    /// Raw CDP endpoint — for `chromiumoxide` / Puppeteer. Derived from
    /// `ws_endpoint` when the API does not return one.
    pub cdp_endpoint: String,
    /// Plan-tier deadline (ISO 8601 UTC); the session auto-releases at this point.
    pub expires_at: String,
    /// Three-state, mirroring the TS `storageState?: StorageState | null`:
    /// - `None` — no profile attached to this session.
    /// - `Some(None)` — a profile is attached but has no saved state yet.
    /// - `Some(Some(state))` — the profile's state.
    pub storage_state: Option<Option<StorageState>>,
    /// Present only when a managed proxy was requested.
    pub proxy: Option<ResolvedProxyConfig>,
}

/// `GET /sessions/:id`. The API proxies the pool's view through verbatim, so
/// every field is optional and unknown keys land in [`SessionView::extra`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Presigned URL for a session's replay.
#[derive(Debug, Clone)]
pub struct ReplayUrl {
    pub url: String,
    /// Lifetime of the presigned URL. `0` when the API omits it.
    pub expires_in_seconds: u64,
    /// Content encoding of the object behind `url`. `"gzip"` when omitted.
    pub content_encoding: String,
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// A stored browser profile (cookies + localStorage).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Any additional keys the wire carried (TS index signature).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Result of `POST /profiles/:id/save`.
#[derive(Debug, Clone, Default)]
pub struct ProfileSaveResult {
    /// Monotonic version of the saved state. `0` when the API omits it.
    pub version: u64,
    /// Size of the saved state in bytes. `0` when the API omits it.
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Internal wire shapes
// ---------------------------------------------------------------------------

/// Wire body for `POST /sessions`. Falsy/unset fields are omitted; when every
/// field is omitted the request is sent with no body at all.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub recording: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub stealth: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub captcha: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub web_bot_auth: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxySpec>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Presigned pointer to a profile's storage state, returned on session create.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageStateUrl {
    #[serde(default)]
    pub url: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
}

/// `201` response from `POST /sessions`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateSessionResponse {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub ws_endpoint: Option<String>,
    #[serde(default)]
    pub cdp_endpoint: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Absent vs explicit `null` are distinguished.
    #[serde(default, deserialize_with = "double_option")]
    pub storage_state_url: Option<Option<StorageStateUrl>>,
    /// Legacy inline state, for gateways that predate `storageStateUrl`.
    #[serde(default, deserialize_with = "double_option")]
    pub storage_state: Option<Option<StorageState>>,
    #[serde(default)]
    pub proxy: Option<ResolvedProxyConfig>,
}

/// `GET /sessions/:id/replay-url`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplayUrlResponse {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub expires_in_seconds: Option<u64>,
    #[serde(default)]
    pub content_encoding: Option<String>,
}

/// `POST /profiles/:id/save`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileSaveResponse {
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Distinguish "key absent" (`None`) from "key present and null" (`Some(None)`).
/// Requires `#[serde(default)]` alongside it.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}
