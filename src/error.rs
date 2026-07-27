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
    /// A code this crate does not know about.
    Other(String),
}

impl SolariErrorCode {
    pub const FEATURE_REQUIRES_PLAN: &'static str = "FeatureRequiresPlan";
    pub const CONCURRENCY_LIMIT_EXCEEDED: &'static str = "ConcurrencyLimitExceeded";
    pub const PLAN_LIMIT_EXCEEDED: &'static str = "PlanLimitExceeded";
    pub const BROWSER_UNHEALTHY: &'static str = "BrowserUnhealthy";

    /// The wire string for this code.
    pub fn as_str(&self) -> &str {
        match self {
            SolariErrorCode::FeatureRequiresPlan => Self::FEATURE_REQUIRES_PLAN,
            SolariErrorCode::ConcurrencyLimitExceeded => Self::CONCURRENCY_LIMIT_EXCEEDED,
            SolariErrorCode::PlanLimitExceeded => Self::PLAN_LIMIT_EXCEEDED,
            SolariErrorCode::BrowserUnhealthy => Self::BROWSER_UNHEALTHY,
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
        message: format!("Solari {method} {path} failed: {status} {body}"),
    }
}
