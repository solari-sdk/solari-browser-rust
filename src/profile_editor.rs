//! Editing an existing profile through the profile editor: [`Profiles::edit`](crate::Profiles::edit)
//! returns a [`ProfileEditor`] handle.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::client::{enc, Client};
use crate::error::{api_error, SolariError};

const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// State of a profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorStatus {
    Idle,
    Launching,
    Editing,
    Saving,
    Error,
}

/// Result of [`ProfileEditor::status`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorState {
    pub editor_status: EditorStatus,
    #[serde(default)]
    pub version: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Handoff {
    handoff_id: String,
    url: String,
    expires_at: Value,
    #[serde(default)]
    version: u64,
    profile_id: Option<String>,
}

/// A handle on an open profile editor. Send the person to [`url`](Self::url); it works until
/// [`expires_at`](Self::expires_at). There is no save without closing: [`save_and_close`](Self::save_and_close)
/// stores the profile and ends the editor, [`exit`](Self::exit) ends it without saving.
///
/// An expired or used link fails every call with an API error of status 404; an editor that is not running
/// with 409; a failed save with 502 (the editor stays open, so retry).
#[derive(Debug, Clone)]
pub struct ProfileEditor {
    client: Client,
    pub profile_id: String,
    pub handoff_id: String,
    /// The editor link to open in a browser.
    pub url: String,
    pub expires_at: SystemTime,
    /// The profile's version when the editor was opened.
    pub start_version: u64,
    /// How often [`wait_for_save`](Self::wait_for_save) polls. Default 2 s.
    pub poll_interval: Duration,
}

impl ProfileEditor {
    pub(crate) async fn open(
        client: Client,
        id: &str,
        reason: Option<&str>,
    ) -> Result<Self, SolariError> {
        let path = format!("/profiles/{}/login-handoff", enc(id));
        let body = match reason {
            Some(r) => json!({ "reason": r }),
            None => json!({}),
        };
        let res = client.http.request("POST", &path, Some(&body)).await?;
        if !res.ok() {
            return Err(api_error("POST", &path, res.status, &res.body));
        }
        let h: Handoff = serde_json::from_str(&res.body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected login-handoff response: {e}"))
        })?;
        let expires_at = parse_expires_at(&h.expires_at).ok_or_else(|| {
            SolariError::protocol(format!(
                "Solari: unparseable login-handoff expiresAt: {}",
                h.expires_at
            ))
        })?;
        Ok(ProfileEditor {
            client,
            profile_id: h.profile_id.unwrap_or_else(|| id.to_string()),
            handoff_id: h.handoff_id,
            url: h.url,
            expires_at,
            start_version: h.version,
            poll_interval: DEFAULT_POLL,
        })
    }

    async fn call(&self, method: &str, suffix: &str) -> Result<String, SolariError> {
        let path = format!("/profiles/{}/editor{}", enc(&self.profile_id), suffix);
        let res = self
            .client
            .http
            .request_full(
                method,
                &path,
                None,
                None,
                &[("X-Handoff-Id", &self.handoff_id)],
            )
            .await?;
        if !res.ok() {
            return Err(api_error(method, &path, res.status, &res.body));
        }
        Ok(res.body)
    }

    /// `GET /profiles/:id/editor`.
    pub async fn status(&self) -> Result<EditorState, SolariError> {
        let body = self.call("GET", "").await?;
        serde_json::from_str(&body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected editor status response: {e}"))
        })
    }

    /// `POST /profiles/:id/editor/save` — store the editor's browser state in the profile and close the
    /// editor. Returns the new version.
    pub async fn save_and_close(&self) -> Result<u64, SolariError> {
        let body = self.call("POST", "/save").await?;
        #[derive(Deserialize)]
        struct Saved {
            version: Option<u64>,
        }
        let s: Saved = serde_json::from_str(&body).map_err(|e| {
            SolariError::protocol(format!("Solari: unexpected editor save response: {e}"))
        })?;
        Ok(s.version.unwrap_or(0))
    }

    /// `DELETE /profiles/:id/editor` — close the editor without saving.
    pub async fn exit(&self) -> Result<(), SolariError> {
        self.call("DELETE", "").await.map(|_| ())
    }

    /// Poll the profile list until this profile's version exceeds [`start_version`](Self::start_version)
    /// (someone saved from the editor page) and return it. Fails with an API error of status 408 when
    /// `timeout` passes or the link expires first. `None` waits until the link expires.
    pub async fn wait_for_save(&self, timeout: Option<Duration>) -> Result<u64, SolariError> {
        let until_expiry = self
            .expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        let budget = timeout.map_or(until_expiry, |t| t.min(until_expiry));
        let deadline = Instant::now() + budget;
        loop {
            let list = self.client.profiles().list().await?;
            if let Some(p) = list.iter().find(|p| p.id == self.profile_id) {
                if p.version > self.start_version {
                    return Ok(p.version);
                }
            }
            if Instant::now() + self.poll_interval >= deadline {
                return Err(SolariError::Api {
                    status: 408,
                    code: None,
                    message: format!(
                        "Solari: timed out waiting for profile {} to be saved",
                        self.profile_id
                    ),
                });
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

/// ISO-8601 (`2030-01-01T00:30:00.000Z`) or epoch milliseconds.
fn parse_expires_at(v: &Value) -> Option<SystemTime> {
    if let Some(ms) = v.as_u64() {
        return Some(UNIX_EPOCH + Duration::from_millis(ms));
    }
    let s = v.as_str()?;
    if let Ok(ms) = s.parse::<u64>() {
        return Some(UNIX_EPOCH + Duration::from_millis(ms));
    }
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let mut rest = &s[19..];
    let mut nanos = 0u32;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        rest = &frac[digits.len()..];
        nanos = format!("{:0<9}", &digits[..digits.len().min(9)])
            .parse()
            .ok()?;
    }
    let offset_secs = match rest {
        "Z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o.get(1..3)?.parse::<i64>().ok()? * 3600
                + o.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec - offset_secs;
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::new(s, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_and_epoch_expiry() {
        let at = |s: SystemTime| s.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(
            at(parse_expires_at(&json!("2030-01-01T00:30:00.000Z")).unwrap()),
            1893457800
        );
        assert_eq!(
            at(parse_expires_at(&json!(4102444800000u64)).unwrap()),
            4102444800
        );
        assert_eq!(
            at(parse_expires_at(&json!("2030-01-01T01:30:00+01:00")).unwrap()),
            1893457800
        );
        assert!(parse_expires_at(&json!("soon")).is_none());
    }
}
