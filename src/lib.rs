//! # solari-browser
//!
//! Rust binding for the **Solari Browser** platform — the control plane:
//! acquire and release pooled stealth Chromium sessions, manage stored browser
//! profiles, pull session replays, and inspect managed-proxy egress. It speaks
//! the same wire contract as the reference `@solarisdk/browser` TypeScript
//! package (`sdk/src/index.ts`).
//!
//! ```no_run
//! use solari_browser::{Client, ClientOptions, CreateSessionOptions, ProxySpec};
//!
//! # async fn ex() -> Result<(), solari_browser::SolariError> {
//! let client = Client::new(ClientOptions::default_region("slr_live_…"))?;
//!
//! let session = client
//!     .sessions()
//!     .create(
//!         CreateSessionOptions::new()
//!             .stealth(true)
//!             .proxy(ProxySpec::country("us")),
//!     )
//!     .await?;
//!
//! println!("ws  {}", session.ws_endpoint);   // Playwright wire protocol
//! println!("cdp {}", session.cdp_endpoint);  // raw CDP — use this from Rust
//!
//! client.sessions().release(&session.id).await?;
//! # Ok(()) }
//! ```
//!
//! ## Scope
//!
//! This crate is the **control plane plus a `connect()` helper**. There is no
//! port of the TypeScript `Solari.launch()`, which hands back a live Playwright
//! `Browser` — Playwright has no Rust binding. To drive a session from Rust,
//! enable the `connect` feature and attach to [`Session::cdp_endpoint`] with
//! [`chromiumoxide`](https://docs.rs/chromiumoxide):
//!
//! ```toml
//! solari-browser = { path = "sdk/rust", features = ["connect"] }
//! ```
//!
//! ## Endpoints vs. the TypeScript SDK
//!
//! The TS SDK routes `wsEndpoint` / `cdpEndpoint` through a Node-side loopback
//! `LocalProxy`. This crate has no such indirection: [`Session::ws_endpoint`]
//! and [`Session::cdp_endpoint`] are the **upstream** gateway endpoints.

mod client;
mod error;
mod http;
mod types;

#[cfg(feature = "connect")]
mod connect;

pub use client::{Client, ClientOptions, CreateSessionOptions, Profiles, Proxy, Sessions};
pub use error::{ApiErrorBody, SolariError, SolariErrorCode};
pub use types::{
    Cookie, LocalStorageEntry, Profile, ProfileSaveResult, ProxyCountries, ProxyRequest, ProxySpec,
    ProxyTier, ReplayUrl, ResolvedProxyConfig, SameSite, Session, SessionView, SolariRegion,
    StorageOrigin, StorageState,
};

#[cfg(feature = "connect")]
pub use connect::{connect, connect_endpoint, ConnectedBrowser};

#[cfg(test)]
mod tests {
    use super::client::{derive_cdp_from_ws, iso8601_utc};
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // -----------------------------------------------------------------------
    // Mock HTTP server: serves a scripted list of responses, one per accepted
    // connection, and captures every raw request it saw.
    // -----------------------------------------------------------------------

    async fn bind_mock() -> (TcpListener, u16) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    /// Serve `responses` in order — one per connection — capturing raw requests.
    fn serve(listener: TcpListener, responses: Vec<(u16, String)>) -> Arc<Mutex<Vec<String>>> {
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                // Read headers, then the Content-Length body.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                while let Ok(n) = sock.read(&mut tmp).await {
                    if n == 0 {
                        break; // peer closed before we saw a full request
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(pos) = find_subslice(&buf, b"\r\n\r\n") else {
                        continue; // headers still incomplete
                    };
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let clen = content_length(&head).unwrap_or(0);
                    let body_start = pos + 4;
                    while buf.len() < body_start + clen {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    break;
                }
                cap.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());

                let reason = match status {
                    200 => "OK",
                    201 => "Created",
                    204 => "No Content",
                    400 => "Bad Request",
                    402 => "Payment Required",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    _ => "Status",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        captured
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(head: &str) -> Option<usize> {
        for line in head.lines() {
            if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                return v.trim().parse().ok();
            }
        }
        None
    }

    fn split_req(raw: &str) -> (String, String) {
        match raw.split_once("\r\n\r\n") {
            Some((h, b)) => (h.to_string(), b.to_string()),
            None => (raw.to_string(), String::new()),
        }
    }

    fn client_for(port: u16) -> Client {
        Client::new(
            ClientOptions::new("slr_live_id_secret", format!("http://127.0.0.1:{port}"))
                .backoff_ms(0),
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    /// POST /sessions request shape: method/path, auth + content-type headers,
    /// only the truthy options serialized, everything else omitted.
    #[tokio::test]
    async fn create_session_request_shape() {
        let (l, port) = bind_mock().await;
        let body = r#"{"sessionId":"s_1","wsEndpoint":"wss://gw.example.com/ws/s_1","cdpEndpoint":"wss://gw.example.com/cdp/s_1","expiresAt":"2026-01-01T00:00:00Z"}"#;
        let captured = serve(l, vec![(201, body.to_string())]);

        let client = client_for(port);
        let session = client
            .sessions()
            .create(
                CreateSessionOptions::new()
                    .stealth(true)
                    .proxy(ProxySpec::country("us")),
            )
            .await
            .unwrap();

        assert_eq!(session.id, "s_1");
        assert_eq!(session.ws_endpoint, "wss://gw.example.com/ws/s_1");
        assert_eq!(session.cdp_endpoint, "wss://gw.example.com/cdp/s_1");
        assert_eq!(session.expires_at, "2026-01-01T00:00:00Z");
        // No profile requested → storage state stays absent (not "empty").
        assert!(session.storage_state.is_none());
        assert!(session.proxy.is_none());

        let raw = captured.lock().unwrap()[0].clone();
        let (head, body) = split_req(&raw);
        assert!(head.starts_with("POST /sessions "), "head: {head}");
        let lower = head.to_lowercase();
        assert!(
            lower.contains("authorization: bearer slr_live_id_secret"),
            "head: {head}"
        );
        assert!(lower.contains("content-type: application/json"), "head: {head}");

        let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(json.get("stealth").unwrap(), true);
        assert_eq!(json.get("proxy").unwrap(), "us");
        // Falsy / unset options are omitted entirely.
        assert!(json.get("recording").is_none(), "body: {body}");
        assert!(json.get("captcha").is_none(), "body: {body}");
        assert!(json.get("profileId").is_none(), "body: {body}");
    }

    /// With no options at all, the body is omitted entirely (not `{}`).
    #[tokio::test]
    async fn create_session_omits_empty_body() {
        let (l, port) = bind_mock().await;
        let body = r#"{"sessionId":"s_2","wsEndpoint":"wss://gw.example.com/ws/s_2"}"#;
        let captured = serve(l, vec![(201, body.to_string())]);

        let client = client_for(port);
        client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap();

        let raw = captured.lock().unwrap()[0].clone();
        let (head, body) = split_req(&raw);
        assert!(body.trim().is_empty(), "expected no body, got: {body}");
        assert!(
            !head.to_lowercase().contains("content-length: 2"),
            "head: {head}"
        );
    }

    /// A full ProxyRequest serializes camelCase, with unset fields omitted.
    #[tokio::test]
    async fn create_session_proxy_request_shape() {
        let (l, port) = bind_mock().await;
        let body = r#"{"sessionId":"s_3","wsEndpoint":"wss://gw.example.com/ws/s_3","proxy":{"server":"http://p:1","username":"u","password":"p","timezoneId":"America/Los_Angeles","country":"us","tier":"mobile"}}"#;
        let captured = serve(l, vec![(201, body.to_string())]);

        let client = client_for(port);
        let session = client
            .sessions()
            .create(CreateSessionOptions::new().stealth(true).proxy(
                ProxyRequest::country("us")
                    .tier(ProxyTier::Mobile)
                    .session("warm-1")
                    .session_duration(15),
            ))
            .await
            .unwrap();

        let proxy = session.proxy.expect("resolved proxy");
        assert_eq!(proxy.country, "us");
        assert_eq!(proxy.tier, Some(ProxyTier::Mobile));
        assert_eq!(proxy.timezone_id, "America/Los_Angeles");

        let raw = captured.lock().unwrap()[0].clone();
        let (_, body) = split_req(&raw);
        let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        let p = json.get("proxy").unwrap();
        assert_eq!(p.get("country").unwrap(), "us");
        assert_eq!(p.get("tier").unwrap(), "mobile");
        assert_eq!(p.get("session").unwrap(), "warm-1");
        assert_eq!(p.get("sessionDuration").unwrap(), 15);
        assert!(p.get("asn").is_none());
        assert!(p.get("city").is_none());
    }

    /// The gateway marks `404 ReplayPending` retryable: the recording upload is
    /// still in flight. An idempotent GET must honour that hint even though 404
    /// is nowhere near the 5xx allowlist.
    #[tokio::test]
    async fn retryable_hint_retries_idempotent_get() {
        let (l, port) = bind_mock().await;
        let pending =
            r#"{"error":"replay still uploading","code":"ReplayPending","retryable":true}"#;
        let ok = r#"{"url":"https://s3/replay.ndjson.gz"}"#;
        let captured = serve(
            l,
            vec![(404, pending.to_string()), (200, ok.to_string())],
        );

        let client = client_for(port);
        let link = client.sessions().replay_url("sess-1").await.unwrap();

        assert_eq!(link.url, "https://s3/replay.ndjson.gz");
        // Two attempts: the hint is what produced the second.
        assert_eq!(captured.lock().unwrap().len(), 2);
    }

    /// A 404 WITHOUT the hint is terminal -- the FLAG changes the outcome, not
    /// the status.
    #[tokio::test]
    async fn no_hint_means_no_retry() {
        let (l, port) = bind_mock().await;
        let terminal = r#"{"error":"no replay","code":"ReplayUnavailable"}"#;
        let captured = serve(
            l,
            vec![
                (404, terminal.to_string()),
                (200, r#"{"url":"never reached"}"#.to_string()),
            ],
        );

        let client = client_for(port);
        assert!(client.sessions().replay_url("sess-1").await.is_err());
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    /// RETRACTED REASON, kept deliberately. This asserted the opposite, because
    /// "the browser API issues no Idempotency-Key, so a re-sent create could
    /// leave a second live session behind". Creates now mint a key, so the
    /// gateway answers the retry from the first result instead of creating
    /// twice -- the condition was removed, not worked around.
    ///
    /// This is the case the gateway-side guard produces: a duplicate arriving
    /// while the first create is still running is answered 409 + retryable.
    #[tokio::test]
    async fn retryable_hint_honoured_on_keyed_create() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![
                (409, r#"{"error":"in progress","retryable":true}"#.to_string()),
                (201, r#"{"sessionId":"s_ok","wsEndpoint":"wss://x/ws/s_ok"}"#.to_string()),
            ],
        );

        let client = client_for(port);
        assert!(client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .is_ok());
        assert_eq!(
            captured.lock().unwrap().len(),
            2,
            "the hint must be honoured on a keyed create"
        );
    }

    /// A key identifies the CALL: two separate creates must not share one.
    #[tokio::test]
    async fn idempotency_keys_differ_between_calls() {
        assert_ne!(
            crate::http::new_idempotency_key(),
            crate::http::new_idempotency_key()
        );
    }

    /// 503 is retried up to max_attempts and can succeed on the second try.
    #[tokio::test]
    async fn retries_on_503() {
        let (l, port) = bind_mock().await;
        let ok = r#"{"sessionId":"s_ok","wsEndpoint":"wss://gw.example.com/ws/s_ok"}"#;
        let captured = serve(
            l,
            vec![
                (503, r#"{"error":"no capacity"}"#.to_string()),
                (201, ok.to_string()),
            ],
        );

        let client = client_for(port);
        let session = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap();

        assert_eq!(session.id, "s_ok");
        // Two attempts were actually made.
        assert_eq!(captured.lock().unwrap().len(), 2);
    }

    /// Retries are exhausted after max_attempts → a Transport error.
    #[tokio::test]
    async fn retries_exhausted_on_persistent_503() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![
                (503, "{}".to_string()),
                (503, "{}".to_string()),
                (201, "{}".to_string()), // never reached: max_attempts is 2
            ],
        );

        let client = client_for(port);
        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();

        assert!(matches!(err, SolariError::Transport { .. }), "{err:?}");
        assert!(err.to_string().contains("exhausted 2 attempts"), "{err}");
        assert_eq!(captured.lock().unwrap().len(), 2);
    }

    /// 400 is NOT retried — it comes straight back on the first attempt.
    #[tokio::test]
    async fn does_not_retry_on_400() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![
                (400, r#"{"error":"bad proxy country"}"#.to_string()),
                (201, "{}".to_string()), // must NOT be consumed
            ],
        );

        let client = client_for(port);
        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();

        assert_eq!(err.status(), Some(400));
        assert!(err.to_string().contains("bad proxy country"), "{err}");
        // Exactly one request was made.
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    /// `{code}` is parsed out of error bodies into the typed enum; unknown codes
    /// survive as `Other`.
    #[tokio::test]
    async fn parses_error_codes() {
        let (l, port) = bind_mock().await;
        serve(
            l,
            vec![
                (
                    402,
                    r#"{"code":"FeatureRequiresPlan","error":"captcha requires a paid plan"}"#
                        .to_string(),
                ),
                (
                    429,
                    r#"{"code":"ConcurrencyLimitExceeded"}"#.to_string(),
                ),
                (400, r#"{"code":"SomethingNew"}"#.to_string()),
                (400, "not json at all".to_string()),
            ],
        );

        let client = client_for(port);

        let err = client
            .sessions()
            .create(CreateSessionOptions::new().captcha(true))
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(402));
        assert_eq!(err.code(), Some(&SolariErrorCode::FeatureRequiresPlan));
        assert!(err.to_string().contains("captcha requires a paid plan"));

        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(429));
        assert_eq!(err.code(), Some(&SolariErrorCode::ConcurrencyLimitExceeded));

        // Unknown code → preserved verbatim, not dropped.
        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            Some(&SolariErrorCode::Other("SomethingNew".to_string()))
        );

        // Non-JSON body → no code, but still a typed API error with the status.
        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.status(), Some(400));
        assert_eq!(err.code(), None);
    }

    /// GET /profiles parses the list, including unknown keys via `extra`.
    #[tokio::test]
    async fn profiles_list_parses() {
        let (l, port) = bind_mock().await;
        let body = r#"[{"id":"p_1","name":"amazon","createdAt":"2026-01-01T00:00:00Z"},{"id":"p_2","name":"gmail"}]"#;
        let captured = serve(l, vec![(200, body.to_string())]);

        let client = client_for(port);
        let profiles = client.profiles().list().await.unwrap();

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id, "p_1");
        assert_eq!(profiles[0].name, "amazon");
        // Index-signature keys survive.
        assert_eq!(
            profiles[0].extra.get("createdAt").unwrap(),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(profiles[1].id, "p_2");
        assert!(profiles[1].extra.is_empty());

        let (head, _) = split_req(&captured.lock().unwrap()[0]);
        assert!(head.starts_with("GET /profiles "), "head: {head}");
    }

    /// POST /profiles sends `{name}` and parses the created profile.
    #[tokio::test]
    async fn profiles_create_and_save() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![
                (201, r#"{"id":"p_9","name":"shopify"}"#.to_string()),
                (200, r#"{"version":3,"sizeBytes":2048}"#.to_string()),
                (200, "{}".to_string()), // save with defaults
            ],
        );

        let client = client_for(port);
        let profile = client.profiles().create("shopify").await.unwrap();
        assert_eq!(profile.id, "p_9");
        assert_eq!(profile.name, "shopify");

        let state = StorageState {
            cookies: Some(vec![Cookie {
                name: "sid".into(),
                value: "abc".into(),
                domain: Some(".shopify.com".into()),
                same_site: Some(SameSite::Lax),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let saved = client.profiles().save("p_9", &state).await.unwrap();
        assert_eq!(saved.version, 3);
        assert_eq!(saved.size_bytes, 2048);

        // Missing fields default to 0.
        let saved = client.profiles().save("p_9", &state).await.unwrap();
        assert_eq!(saved.version, 0);
        assert_eq!(saved.size_bytes, 0);

        let reqs = captured.lock().unwrap().clone();
        let (head, body) = split_req(&reqs[0]);
        assert!(head.starts_with("POST /profiles "), "head: {head}");
        let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(json.get("name").unwrap(), "shopify");

        // save wraps the state under `storageState` and keeps camelCase inside.
        let (head, body) = split_req(&reqs[1]);
        assert!(head.starts_with("POST /profiles/p_9/save "), "head: {head}");
        let json: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        let cookie = &json["storageState"]["cookies"][0];
        assert_eq!(cookie.get("name").unwrap(), "sid");
        assert_eq!(cookie.get("domain").unwrap(), ".shopify.com");
        assert_eq!(cookie.get("sameSite").unwrap(), "Lax");
        // Unset cookie fields are omitted, not sent as null.
        assert!(cookie.get("httpOnly").is_none(), "cookie: {cookie}");
    }

    /// DELETE tolerates 404 on both sessions and profiles.
    #[tokio::test]
    async fn delete_tolerates_404() {
        let (l, port) = bind_mock().await;
        serve(
            l,
            vec![
                (404, r#"{"error":"Not Found"}"#.to_string()),
                (404, r#"{"error":"Not Found"}"#.to_string()),
                (500, r#"{"error":"boom"}"#.to_string()),
            ],
        );

        let client = client_for(port);
        client.sessions().release("s_gone").await.unwrap();
        client.profiles().delete("p_gone").await.unwrap();
        // But a real failure still propagates.
        let err = client.sessions().release("s_bad").await.unwrap_err();
        assert_eq!(err.status(), Some(500));
    }

    /// replay-url defaults: expiresInSeconds → 0, contentEncoding → "gzip".
    #[tokio::test]
    async fn replay_url_defaults() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![
                (200, r#"{"url":"https://s3.example.com/r1"}"#.to_string()),
                (
                    200,
                    r#"{"url":"https://s3.example.com/r2","expiresInSeconds":600,"contentEncoding":"identity"}"#
                        .to_string(),
                ),
                (200, r#"{"expiresInSeconds":600}"#.to_string()),
            ],
        );

        let client = client_for(port);

        let replay = client.sessions().replay_url("s_1").await.unwrap();
        assert_eq!(replay.url, "https://s3.example.com/r1");
        assert_eq!(replay.expires_in_seconds, 0);
        assert_eq!(replay.content_encoding, "gzip");

        let replay = client.sessions().replay_url("s_2").await.unwrap();
        assert_eq!(replay.expires_in_seconds, 600);
        assert_eq!(replay.content_encoding, "identity");

        // A body with no url is a protocol error, not a silent empty string.
        let err = client.sessions().replay_url("s_3").await.unwrap_err();
        assert!(matches!(err, SolariError::Protocol { .. }), "{err:?}");

        let (head, _) = split_req(&captured.lock().unwrap()[0]);
        assert!(head.starts_with("GET /sessions/s_1/replay-url "), "head: {head}");
    }

    /// GET /proxy/countries.
    #[tokio::test]
    async fn proxy_countries() {
        let (l, port) = bind_mock().await;
        let captured = serve(
            l,
            vec![(
                200,
                r#"{"enabled":true,"countries":["br","de","gb","us"]}"#.to_string(),
            )],
        );

        let client = client_for(port);
        let res = client.proxy().countries().await.unwrap();
        assert!(res.enabled);
        assert_eq!(res.countries, vec!["br", "de", "gb", "us"]);

        let (head, _) = split_req(&captured.lock().unwrap()[0]);
        assert!(head.starts_with("GET /proxy/countries "), "head: {head}");
    }

    /// GET /sessions/:id.
    #[tokio::test]
    async fn session_get_parses() {
        let (l, port) = bind_mock().await;
        serve(
            l,
            vec![(
                200,
                r#"{"sessionId":"s_1","status":"busy","expiresAt":"2026-01-01T00:00:00Z","slot":3}"#
                    .to_string(),
            )],
        );

        let client = client_for(port);
        let view = client.sessions().get("s_1").await.unwrap();
        assert_eq!(view.session_id.as_deref(), Some("s_1"));
        assert_eq!(view.status.as_deref(), Some("busy"));
        assert_eq!(view.expires_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(view.extra.get("slot").unwrap(), 3);
    }

    /// cdpEndpoint is derived from wsEndpoint when the API omits it.
    #[test]
    fn cdp_derivation() {
        assert_eq!(
            derive_cdp_from_ws("wss://api.getsolari.com/ws/abc.def"),
            "wss://api.getsolari.com/cdp/abc.def"
        );
        assert_eq!(
            derive_cdp_from_ws("ws://127.0.0.1:3000/ws/s_1?token=x"),
            "ws://127.0.0.1:3000/cdp/s_1?token=x"
        );
        // Non-/ws/ paths pass through untouched.
        assert_eq!(
            derive_cdp_from_ws("wss://api.getsolari.com/other/abc"),
            "wss://api.getsolari.com/other/abc"
        );
        // Unparseable input passes through untouched.
        assert_eq!(derive_cdp_from_ws("not a url"), "not a url");
    }

    /// A create response without cdpEndpoint derives one from wsEndpoint.
    #[tokio::test]
    async fn create_session_derives_cdp() {
        let (l, port) = bind_mock().await;
        let body = r#"{"sessionId":"s_1","wsEndpoint":"wss://gw.example.com/ws/s_1"}"#;
        serve(l, vec![(201, body.to_string())]);

        let client = client_for(port);
        let session = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap();

        assert_eq!(session.ws_endpoint, "wss://gw.example.com/ws/s_1");
        assert_eq!(session.cdp_endpoint, "wss://gw.example.com/cdp/s_1");
        // expiresAt was omitted → a ~1h ISO-8601 UTC fallback was synthesized.
        assert!(session.expires_at.ends_with('Z'), "{}", session.expires_at);
        assert_eq!(session.expires_at.len(), 24, "{}", session.expires_at);
    }

    /// A malformed create response (no sessionId) is a protocol error.
    #[tokio::test]
    async fn create_session_rejects_bad_response() {
        let (l, port) = bind_mock().await;
        serve(l, vec![(201, r#"{"wsEndpoint":"wss://x/ws/1"}"#.to_string())]);

        let client = client_for(port);
        let err = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, SolariError::Protocol { .. }), "{err:?}");
    }

    /// With a profileId, `storageStateUrl` is followed to the presigned object.
    #[tokio::test]
    async fn storage_state_fetched_from_presigned_url() {
        let (l, port) = bind_mock().await;
        let create = format!(
            r#"{{"sessionId":"s_1","wsEndpoint":"wss://gw.example.com/ws/s_1","storageStateUrl":{{"url":"http://127.0.0.1:{port}/presigned","expiresInSeconds":60}}}}"#
        );
        let state = r#"{"cookies":[{"name":"sid","value":"abc","domain":".example.com","sameSite":"Lax"}],"origins":[{"origin":"https://example.com","localStorage":[{"name":"k","value":"v"}]}]}"#;
        serve(l, vec![(201, create), (200, state.to_string())]);

        let client = client_for(port);
        let session = client
            .sessions()
            .create(CreateSessionOptions::new().profile_id("p_1"))
            .await
            .unwrap();

        let state = session
            .storage_state
            .expect("profile attached → Some")
            .expect("presigned state → Some");
        let cookies = state.cookies.unwrap();
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].same_site, Some(SameSite::Lax));
        let origins = state.origins.unwrap();
        assert_eq!(origins[0].origin, "https://example.com");
        assert_eq!(origins[0].local_storage.as_ref().unwrap()[0].value, "v");
    }

    /// The three storage-state states are distinguished: absent vs empty vs set.
    #[tokio::test]
    async fn storage_state_absent_vs_empty() {
        let (l, port) = bind_mock().await;
        serve(
            l,
            vec![
                // profileId requested, storageStateUrl.url is null → empty profile.
                (
                    201,
                    r#"{"sessionId":"s_1","wsEndpoint":"wss://x/ws/s_1","storageStateUrl":{"url":null}}"#
                        .to_string(),
                ),
                // profileId requested, no storageStateUrl at all → empty profile.
                (
                    201,
                    r#"{"sessionId":"s_2","wsEndpoint":"wss://x/ws/s_2"}"#.to_string(),
                ),
                // profileId requested, legacy inline storageState → populated.
                (
                    201,
                    r#"{"sessionId":"s_3","wsEndpoint":"wss://x/ws/s_3","storageState":{"cookies":[]}}"#
                        .to_string(),
                ),
                // No profileId → absent entirely.
                (
                    201,
                    r#"{"sessionId":"s_4","wsEndpoint":"wss://x/ws/s_4"}"#.to_string(),
                ),
            ],
        );

        let client = client_for(port);

        let s = client
            .sessions()
            .create(CreateSessionOptions::new().profile_id("p_1"))
            .await
            .unwrap();
        assert!(matches!(s.storage_state, Some(None)), "null url → Some(None)");

        let s = client
            .sessions()
            .create(CreateSessionOptions::new().profile_id("p_1"))
            .await
            .unwrap();
        assert!(matches!(s.storage_state, Some(None)), "no url → Some(None)");

        let s = client
            .sessions()
            .create(CreateSessionOptions::new().profile_id("p_1"))
            .await
            .unwrap();
        let inline = s.storage_state.expect("Some").expect("Some");
        assert_eq!(inline.cookies.unwrap().len(), 0);

        let s = client
            .sessions()
            .create(CreateSessionOptions::default())
            .await
            .unwrap();
        assert!(s.storage_state.is_none(), "no profile → None");
    }

    /// Client construction: defaults, validation, region resolution.
    #[test]
    fn client_options_and_defaults() {
        let opts = ClientOptions::default_region("k");
        assert_eq!(opts.base_url, "https://api.getsolari.com");
        assert_eq!(opts.max_attempts, 2);
        assert_eq!(opts.backoff_ms, 500);
        assert_eq!(opts.timeout_ms, 90_000);

        assert_eq!(
            ClientOptions::for_region("k", SolariRegion::UsWest).base_url,
            "https://api.getsolari.com"
        );

        // Trailing slashes are stripped.
        let c = Client::new(ClientOptions::new("k", "https://gw.example.com/")).unwrap();
        assert_eq!(c.base_url(), "https://gw.example.com");

        // An empty api key is rejected.
        let err = Client::new(ClientOptions::new("", "https://gw.example.com")).unwrap_err();
        assert!(matches!(err, SolariError::Config { .. }), "{err:?}");
    }

    /// The ISO-8601 fallback formatter. Expectations cross-checked against
    /// `date -u -d @<ts>`.
    #[test]
    fn iso8601_formatting() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_utc(1_768_000_000), "2026-01-09T23:06:40.000Z");
        assert_eq!(iso8601_utc(1_000_000_000), "2001-09-09T01:46:40.000Z");
        // 2024-02-29 (leap day) 12:00:00Z.
        assert_eq!(iso8601_utc(1_709_208_000), "2024-02-29T12:00:00.000Z");
        // 2000 is a leap year (divisible by 400) — the century special case.
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00.000Z");
        // 2100 is NOT a leap year (divisible by 100, not 400).
        assert_eq!(iso8601_utc(4_102_444_800), "2100-01-01T00:00:00.000Z");
        // Pre-epoch must floor, not truncate toward zero.
        assert_eq!(iso8601_utc(-1), "1969-12-31T23:59:59.000Z");
    }

    /// Error codes round-trip through their wire strings.
    #[test]
    fn error_code_round_trip() {
        for code in [
            SolariErrorCode::FeatureRequiresPlan,
            SolariErrorCode::ConcurrencyLimitExceeded,
            SolariErrorCode::PlanLimitExceeded,
            SolariErrorCode::BrowserUnhealthy,
        ] {
            assert_eq!(SolariErrorCode::from(code.as_str()), code);
        }
        assert_eq!(
            SolariErrorCode::from("BrowserUnhealthy"),
            SolariErrorCode::BrowserUnhealthy
        );
        assert_eq!(
            SolariErrorCode::Other("Nope".into()).to_string(),
            "Nope".to_string()
        );
    }
}
