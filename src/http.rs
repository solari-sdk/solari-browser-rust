//! HTTP transport for SDK ⇆ Solari Browser API. Owns auth headers, timeouts,
//! and the retry policy. Mirrors `Solari.request()` in `sdk/src/index.ts`.
//!
//! Retry policy (deliberately narrow, matching the reference): only HTTP
//! 502/503/504 and transport errors are retried, `max_attempts` times total,
//! with a FIXED `backoff_ms` delay between attempts (not exponential). Every
//! other status — including 4xx and 500 — is returned to the caller as-is on
//! the first attempt.

use std::time::Duration;

use reqwest::Method;

use crate::error::SolariError;

/// A raw, already-buffered HTTP response. Non-2xx statuses are *not* errors at
/// this layer — callers decide, exactly as the TS `request()` returns a
/// `Response` regardless of `res.ok`.
#[derive(Debug, Clone)]
pub(crate) struct RawResponse {
    pub status: u16,
    pub body: String,
}

impl RawResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Only 502/503/504 are worth another attempt.
// Spelled as three distinct statuses rather than the equivalent `502..=504`
// to mirror the reference `isRetryableStatus` — this is an allowlist of
// specific gateway conditions, not a numeric range.
#[allow(clippy::manual_range_patterns)]
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 502 | 503 | 504)
}

pub(crate) struct HttpTransport {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    max_attempts: u32,
    backoff: Duration,
    timeout: Duration,
}

/// Hand-written so the API key is never printed. Do not derive this.
impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("max_attempts", &self.max_attempts)
            .field("backoff", &self.backoff)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl HttpTransport {
    pub fn new(
        api_key: String,
        base_url: String,
        max_attempts: u32,
        backoff_ms: u64,
        timeout_ms: u64,
    ) -> Result<Self, SolariError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| SolariError::config(format!("Solari: failed to build HTTP client: {e}")))?;
        Ok(HttpTransport {
            api_key,
            base_url,
            client,
            max_attempts: max_attempts.max(1),
            backoff: Duration::from_millis(backoff_ms),
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Issue an authenticated request against the API, retrying per the policy
    /// above. `body` is serialized as JSON; `None` sends no body at all.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<RawResponse, SolariError> {
        let m = Method::from_bytes(method.as_bytes())
            .map_err(|e| SolariError::config(format!("Solari: bad method {method}: {e}")))?;
        let url = format!("{}{}", self.base_url, path);
        let payload = body.map(|b| serde_json::to_string(b).unwrap_or_else(|_| "null".into()));

        let mut last_err: Option<String> = None;
        for attempt in 1..=self.max_attempts {
            let mut rb = self
                .client
                .request(m.clone(), &url)
                .timeout(self.timeout)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json");
            if let Some(p) = &payload {
                rb = rb.body(p.clone());
            }

            match rb.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    // Success, or a status we must not retry: hand it back.
                    if (200..300).contains(&status) || !is_retryable_status(status) {
                        return Ok(RawResponse { status, body });
                    }
                    last_err = Some(format!("Solari {method} {path}: {status}"));
                }
                Err(e) => {
                    // Every transport error is retryable (mirrors isRetryableError).
                    last_err = Some(format!("Solari {method} {path}: {e}"));
                }
            }

            if attempt < self.max_attempts {
                tokio::time::sleep(self.backoff).await;
            }
        }

        Err(SolariError::transport(format!(
            "Solari {method} {path}: exhausted {} attempts{}",
            self.max_attempts,
            last_err.map(|e| format!(" ({e})")).unwrap_or_default(),
        )))
    }

    /// GET an unauthenticated URL (a presigned S3 link) with its own timeout.
    /// No retries — presigned URLs are short-lived and single-shot.
    pub async fn get_presigned(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<RawResponse, SolariError> {
        let resp = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| SolariError::transport(format!("Solari: GET {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(RawResponse { status, body })
    }

    /// GET an unauthenticated URL, returning raw bytes (replay downloads).
    pub async fn get_presigned_bytes(&self, url: &str) -> Result<(u16, Vec<u8>), SolariError> {
        let resp = self
            .client
            .get(url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| SolariError::transport(format!("Solari: GET {url} failed: {e}")))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SolariError::transport(format!("Solari: reading {url} failed: {e}")))?;
        Ok((status, bytes.to_vec()))
    }
}
