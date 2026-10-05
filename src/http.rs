//! HTTP transport for SDK ⇆ Solari Browser API. Owns auth headers, timeouts,
//! and the retry policy. Mirrors `Solari.request()` in `sdk/src/index.ts`.
//!
//! Retry policy (deliberately narrow, matching the reference): HTTP 502/503/504
//! and transport errors are retried, as is any IDEMPOTENT request the gateway
//! explicitly marked `"retryable": true`. Retries run `max_attempts` times
//! total, with a FIXED `backoff_ms` delay between attempts (not exponential).
//! Every other status — including 4xx and 500 — is returned to the caller
//! as-is on the first attempt.

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
    // 507 is listed although THIS gateway does not emit one (censused 2026-09-22:
    // browser emits 501/502/503 only). Desktop's InsufficientCapacity 507 is
    // transient, and a client's correctness must not depend on which gateway build
    // it reaches. Inert today, deliberately — do not remove it as dead code.
    matches!(status, 502 | 503 | 504 | 507)
}

/// Whether THIS REQUEST may be sent again. Deliberately a per-request question,
/// not a per-method one: a `POST` is not idempotent by verb, but a `POST`
/// carrying an `Idempotency-Key` is safe to replay, because the server answers
/// the second copy from the first one's result instead of creating twice.
///
/// RETRACTED REASON, kept deliberately: this used to be method-only, because
/// "the browser API issues no Idempotency-Key, so a re-sent `POST /sessions`
/// could leave a second live session behind". Creates now mint one, so the
/// condition was removed rather than worked around.
fn is_safe_to_replay(method: &str, idempotency_key: Option<&str>) -> bool {
    is_idempotent_method(method) || idempotency_key.is_some()
}

/// A key identifies the CALL, not the attempt: minted once and reused by every
/// retry of that call. A key per attempt would make each retry a fresh create
/// and fix nothing.
///
/// Deliberately dependency-free — adding `uuid` to a published crate for one
/// opaque string is not worth the supply-chain surface. The server treats the
/// key as opaque and scopes it to (org, key), so process-uniqueness is enough.
pub(crate) fn new_idempotency_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // RandomState is seeded per process, so this differs between processes
    // started in the same nanosecond.
    let seed = {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(nanos);
        h.finish()
    };
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("slr-{nanos:x}-{seed:x}-{n:x}")
}

/// Methods safe to send twice by verb alone.
fn is_idempotent_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "DELETE" | "PUT"
    )
}

/// Whether the gateway explicitly marked this response retryable. The flag can
/// appear on a status OUTSIDE the 5xx allowlist — today `404 ReplayPending`,
/// where the recording upload is still in flight.
fn says_retryable(body: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Hint {
        retryable: Option<bool>,
    }
    // A non-JSON body carries no hint.
    serde_json::from_str::<Hint>(body)
        .ok()
        .and_then(|h| h.retryable)
        .unwrap_or(false)
}

#[derive(Clone)]
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
        self.request_with_key(method, path, body, None).await
    }

    /// As [`Self::request`], but sends `Idempotency-Key` on EVERY attempt and
    /// therefore treats the request as safe to replay.
    pub async fn request_with_key(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        idempotency_key: Option<&str>,
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
            if let Some(k) = idempotency_key {
                rb = rb.header("Idempotency-Key", k);
            }
            if let Some(p) = &payload {
                rb = rb.body(p.clone());
            }

            match rb.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    // Success, or a status we must not retry: hand it back.
                    let retry_this = is_retryable_status(status)
                        || (is_safe_to_replay(method, idempotency_key)
                            && says_retryable(&body));
                    if (200..300).contains(&status) || !retry_this {
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

#[cfg(test)]
mod tests {
    use super::{is_retryable_status, is_safe_to_replay};

    /// 507 is in the allowlist although this gateway emits none (censused
    /// 2026-09-22). Asserted so a "dead code" cleanup reddens a test rather
    /// than silently making correctness depend on which gateway build is
    /// reached.
    #[test]
    fn retryable_status_includes_507() {
        for s in [502u16, 503, 504, 507] {
            assert!(is_retryable_status(s), "expected {s} retryable");
        }
        // Negative control: permanent statuses stay out.
        for s in [400u16, 429, 500, 501] {
            assert!(!is_retryable_status(s), "expected {s} NOT retryable");
        }
    }

    #[test]
    fn a_post_is_replayable_only_with_a_key() {
        assert!(!is_safe_to_replay("POST", None));
        assert!(is_safe_to_replay("POST", Some("k")));
        assert!(is_safe_to_replay("GET", None));
    }
}
