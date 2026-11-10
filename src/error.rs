//! Typed errors for the Solari Browser SDK.
//!
//! Mirrors the reference `SolariError` class in `sdk/src/index.ts`: one error
//! type carrying an optional HTTP `status` and an optional machine-readable
//! `code` parsed out of the API's `{ "code": … }` error body.

use serde::Deserialize;

/// Machine-readable error codes the API returns in a `{ "code": … }` body.
///
/// Mirrors the TypeScript `SolariErrorCode` union. Unknown codes are preserved
/// verbatim in [`SolariErrorCode::Other`] rather than dropped — the wire is
/// allowed to grow new codes without breaking this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolariErrorCode {
    /// The org's plan does not include the requested feature (e.g. captcha).
    FeatureRequiresPlan,
    /// The org is at its concurrent-session cap.
    ConcurrencyLimitExceeded,
    /// A plan quota (minutes, profiles, …) is exhausted.
    PlanLimitExceeded,
    /// The acquired browser failed its health check.
    BrowserUnhealthy,
    /// The gateway refused a session id (malformed, forged, or another org's)
    /// and acted on nothing. Only meaningful on a 404 — see [`crate::Sessions::release`]
    /// for why that is not blanket-success.
    InvalidSessionId,
    /// `POST /sessions` 503: a proxy tier you explicitly requested cannot be served right now. No session
    /// was created. The client does not retry it.
    ProxyTierUnavailable,
    /// A code this crate does not know about.
    Other(String),
}

impl SolariErrorCode {
    pub const FEATURE_REQUIRES_PLAN: &'static str = "FeatureRequiresPlan";
    pub const CONCURRENCY_LIMIT_EXCEEDED: &'static str = "ConcurrencyLimitExceeded";
    pub const PLAN_LIMIT_EXCEEDED: &'static str = "PlanLimitExceeded";
    pub const BROWSER_UNHEALTHY: &'static str = "BrowserUnhealthy";
    pub const INVALID_SESSION_ID: &'static str = "InvalidSessionId";
    pub const PROXY_TIER_UNAVAILABLE: &'static str = "ProxyTierUnavailable";

    /// The wire string for this code.
    pub fn as_str(&self) -> &str {
        match self {
            SolariErrorCode::FeatureRequiresPlan => Self::FEATURE_REQUIRES_PLAN,
            SolariErrorCode::ConcurrencyLimitExceeded => Self::CONCURRENCY_LIMIT_EXCEEDED,
            SolariErrorCode::PlanLimitExceeded => Self::PLAN_LIMIT_EXCEEDED,
            SolariErrorCode::BrowserUnhealthy => Self::BROWSER_UNHEALTHY,
            SolariErrorCode::InvalidSessionId => Self::INVALID_SESSION_ID,
            SolariErrorCode::ProxyTierUnavailable => Self::PROXY_TIER_UNAVAILABLE,
            SolariErrorCode::Other(s) => s,
        }
    }
}

impl From<&str> for SolariErrorCode {
    fn from(s: &str) -> Self {
        match s {
            Self::FEATURE_REQUIRES_PLAN => SolariErrorCode::FeatureRequiresPlan,
            Self::CONCURRENCY_LIMIT_EXCEEDED => SolariErrorCode::ConcurrencyLimitExceeded,
            Self::PLAN_LIMIT_EXCEEDED => SolariErrorCode::PlanLimitExceeded,
            Self::BROWSER_UNHEALTHY => SolariErrorCode::BrowserUnhealthy,
            Self::INVALID_SESSION_ID => SolariErrorCode::InvalidSessionId,
            Self::PROXY_TIER_UNAVAILABLE => SolariErrorCode::ProxyTierUnavailable,
            other => SolariErrorCode::Other(other.to_string()),
        }
    }
}

impl From<String> for SolariErrorCode {
    fn from(s: String) -> Self {
        SolariErrorCode::from(s.as_str())
    }
}

impl std::fmt::Display for SolariErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every error raised by this crate.
#[derive(Debug, thiserror::Error)]
pub enum SolariError {
    /// A non-2xx response from the Solari API.
    #[error("{message}")]
    Api {
        status: u16,
        code: Option<SolariErrorCode>,
        message: String,
    },

    /// A transport failure, or retries exhausted without a usable response.
    #[error("{message}")]
    Transport { message: String },

    /// A 2xx response whose body was missing required fields or was not JSON.
    #[error("{message}")]
    Protocol { message: String },

    /// The client was constructed with invalid options.
    #[error("{message}")]
    Config { message: String },

    /// Attaching to the browser over CDP failed (only with the `connect` feature).
    #[error("{message}")]
    Connect { message: String },
}

impl SolariError {
    pub(crate) fn transport(msg: impl Into<String>) -> Self {
        SolariError::Transport { message: msg.into() }
    }

    pub(crate) fn protocol(msg: impl Into<String>) -> Self {
        SolariError::Protocol { message: msg.into() }
    }

    pub(crate) fn config(msg: impl Into<String>) -> Self {
        SolariError::Config { message: msg.into() }
    }

    /// The HTTP status this error carries, when it came from the API.
    pub fn status(&self) -> Option<u16> {
        match self {
            SolariError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The machine-readable code parsed from the error body, when present.
    pub fn code(&self) -> Option<&SolariErrorCode> {
        match self {
            SolariError::Api { code, .. } => code.as_ref(),
            _ => None,
        }
    }
}

/// Shape the API's error bodies may take. Every field is optional — an error
/// body is best-effort and may not be JSON at all.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ApiErrorBody {
    pub code: Option<String>,
    pub error: Option<String>,
    pub message: Option<String>,
}

/// `body` with the session id of `path` (`/sessions/<id>/…`), in any spelling, and every `"sessionId":"…"` field cut to their
/// first 8 characters. The id is the credential inside the ws/cdp URLs and the gateway's error bodies used to echo it (the
/// replay-url 404), so it must not reach an error message through the body either.
pub(crate) fn redact_body(path: &str, body: &str) -> String {
    fn short(id: &str) -> String {
        let keep: String = id.chars().take(8).collect();
        if id.chars().count() > 8 { format!("{keep}…") } else { keep }
    }
    let mut out = body.to_string();
    let mut after_sessions = false;
    for seg in path.split('/') {
        if after_sessions && !seg.is_empty() {
            let raw = seg.split(['?', '#']).next().unwrap_or(seg);
            if raw.chars().count() > 8 {
                let plain = percent_decode(raw);
                for form in [raw.to_string(), percent_encode(&plain), plain.clone()] {
                    out = out.replace(&form, &short(&plain));
                }
            }
        }
        after_sessions = seg == "sessions";
    }
    // "sessionId":"<value>" fields, whatever the id.
    let key = "\"sessionId\"";
    let mut res = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find(key) {
        let (head, tail) = rest.split_at(at + key.len());
        res.push_str(head);
        let t = tail.trim_start();
        let ws = tail.len() - t.len();
        if let Some(t) = t.strip_prefix(':') {
            let t2 = t.trim_start();
            if let Some(v) = t2.strip_prefix('"') {
                if let Some(end) = v.find('"') {
                    res.push_str(&tail[..ws]);
                    res.push(':');
                    res.push_str(&t[..t.len() - t2.len()]);
                    res.push('"');
                    res.push_str(&short(&v[..end]));
                    rest = &v[end..];
                    continue;
                }
            }
        }
        rest = tail;
    }
    res.push_str(rest);
    res
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for c in s.bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// Build an [`SolariError::Api`] from a failed response, parsing `{code}` out of
/// the body when it is JSON. `message` mirrors the TS format:
/// `Solari <METHOD> <path> failed: <status> <body>`.
pub(crate) fn api_error(method: &str, path: &str, status: u16, body: &str) -> SolariError {
    let parsed: Option<ApiErrorBody> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|b| b.code.as_deref())
        .map(SolariErrorCode::from);
    SolariError::Api {
        status,
        code,
        message: format!(
            "Solari {method} {} failed: {status} {}",
            crate::http::redact_path(path),
            redact_body(path, body)
        ),
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::{api_error, redact_body};

    #[test]
    fn an_api_error_does_not_print_the_session_id() {
        let e = api_error("GET", "/sessions/pool:abc:org:123.IDSECRET", 404, "{}").to_string();
        assert!(!e.contains("IDSECRET"), "{e}");
        assert!(e.contains("/sessions/pool:abc…"), "{e}");
    }

    const ID: &str = "pool-abc:org-123.IDSECRETSIGNATURE";

    #[test]
    fn a_body_that_echoes_the_session_id_is_cut() {
        let path = format!("/sessions/{ID}/replay-url");
        for body in [
            format!(r#"{{"error":"Replay not ready yet","code":"ReplayPending","retryable":true,"sessionId":"{ID}"}}"#),
            format!(r#"{{"error":"no session {ID} here"}}"#),
            r#"{"error":"no session pool-abc%3Aorg-123.IDSECRETSIGNATURE here"}"#.to_string(),
            format!(r#"{{ "sessionId" : "{ID}" }}"#),
        ] {
            let e = api_error("GET", &path, 404, &body);
            let text = e.to_string();
            assert!(!text.contains("IDSECRET"), "{text}");
            assert_eq!(e.status(), Some(404));
        }
    }

    #[test]
    fn the_code_is_still_parsed_from_the_raw_body() {
        let body = format!(r#"{{"code":"ReplayPending","sessionId":"{ID}"}}"#);
        let e = api_error("GET", &format!("/sessions/{ID}/replay-url"), 404, &body);
        assert_eq!(e.code().map(|c| c.to_string()), Some("ReplayPending".to_string()));
    }

    #[test]
    fn redact_body_leaves_short_ids_and_unrelated_text_alone() {
        assert_eq!(redact_body("/sessions/short", r#"{"x":"short"}"#), r#"{"x":"short"}"#);
        assert_eq!(redact_body("/profiles/abcdefghijkl", "abcdefghijkl"), "abcdefghijkl");
    }
}
