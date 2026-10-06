//! One-call launch: create → connect → seed → probe → retry → release.
//!
//! Behind the **`connect`** cargo feature. This is the Rust analogue of the
//! TypeScript `Solari.launch()` convenience — but it is deliberately a THIN
//! wrapper, not a port of the Playwright `BrowserSession` object model. It
//! folds the six mechanical steps of a resilient session start into one call
//! and hands back the live [`chromiumoxide::Browser`] for you to drive with the
//! usual chromiumoxide API. It does **not** add a page/context abstraction of
//! its own — driving the browser is chromiumoxide's job, exactly as with the
//! lower-level [`crate::connect`].
//!
//! What it does that [`crate::connect`] does not:
//! - creates the session for you (one call instead of create-then-connect),
//! - seeds an attached profile's **cookies** into the live browser,
//! - health-probes the browser and (optionally) retries the whole start on a
//!   transient failure, releasing the dead session between attempts,
//! - releases the Solari session when the handle is closed or dropped.
//!
//! ```no_run
//! # async fn ex(client: &solari_browser::Client) -> Result<(), solari_browser::SolariError> {
//! use solari_browser::{CreateSessionOptions, LaunchOptions};
//!
//! let launched = client
//!     .launch(LaunchOptions::new(CreateSessionOptions::new().stealth(true)).retries(2))
//!     .await?;
//!
//! let page = launched
//!     .browser
//!     .new_page("https://example.com")
//!     .await
//!     .map_err(|e| solari_browser::SolariError::Connect { message: e.to_string() })?;
//! println!("{:?}", page.url().await.ok());
//!
//! launched.close().await?; // disconnects and releases the session
//! # Ok(()) }
//! ```

use chromiumoxide::cdp::browser_protocol::network::{CookieParam, CookieSameSite, TimeSinceEpoch};

use crate::client::Client;
use crate::client::CreateSessionOptions;
use crate::connect::{connect, ConnectedBrowser};
use crate::error::SolariError;
use crate::types::{SameSite, Session, StorageState};

/// Options for [`Client::launch`]. Carries the session-create options plus the
/// resilience knobs (`retries`, `probe`) — and nothing else. This is not a
/// place to grow browser configuration; drive the returned browser directly.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// How the underlying session is created (stealth, proxy, profile, …).
    pub create: CreateSessionOptions,
    /// Retry the whole create+connect+probe up to this many extra times on a
    /// transient failure. `0` (default) = no retry.
    pub retries: u32,
    /// Health-probe the browser after connecting. `None` (default) means
    /// "probe iff `retries > 0`", matching the TS SDK.
    pub probe: Option<bool>,
    /// Per-attempt probe timeout. Default 2000ms.
    pub probe_timeout_ms: u64,
}

impl LaunchOptions {
    /// New options wrapping the given session-create options.
    pub fn new(create: CreateSessionOptions) -> Self {
        Self {
            create,
            retries: 0,
            probe: None,
            probe_timeout_ms: 2000,
        }
    }
    /// Retry the whole start up to `n` extra times on a transient failure.
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }
    /// Force the post-connect health probe on or off (default: on iff retries>0).
    pub fn probe(mut self, on: bool) -> Self {
        self.probe = Some(on);
        self
    }
    /// Per-attempt probe timeout in milliseconds (default 2000).
    pub fn probe_timeout_ms(mut self, ms: u64) -> Self {
        self.probe_timeout_ms = ms;
        self
    }
}

/// A launched session: the connected browser plus the Solari session it is
/// attached to. Closing or dropping this releases the session.
///
/// Drive [`LaunchedSession::browser`] with the usual chromiumoxide API. This
/// type intentionally exposes no page/context helpers of its own.
pub struct LaunchedSession {
    /// The attached browser — drive it with chromiumoxide.
    pub browser: chromiumoxide::browser::Browser,
    handler: tokio::task::JoinHandle<()>,
    session: Session,
    client: Client,
    released: bool,
}

impl LaunchedSession {
    /// The underlying Solari [`Session`] (id, endpoints, expiry, proxy).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Disconnect the browser and release the Solari session. Idempotent.
    pub async fn close(mut self) -> Result<(), SolariError> {
        // Ending the browser first stops the CDP handler cleanly.
        // (Take ownership so the Drop guard below does not double-release.)
        self.released = true;
        // Drop the browser, then await the handler pump to completion.
        // We cannot move `browser` out of `self` while other fields are used in
        // Drop, so disconnect explicitly here.
        let id = self.session.id.clone();
        let client = self.client.clone();
        // Abort the handler and let the browser drop at end of scope.
        self.handler.abort();
        client.sessions().release(&id).await
    }
}

impl Drop for LaunchedSession {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Best-effort fire-and-forget release. Rust `Drop` is synchronous, so a
        // dropped-without-close handle can only release if a Tokio runtime is
        // still around. Prefer `close().await` for a confirmed release.
        self.handler.abort();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let id = self.session.id.clone();
            handle.spawn(async move {
                let _ = client.sessions().release(&id).await;
            });
        }
    }
}

impl Client {
    /// Create a session, connect to it, seed an attached profile's cookies,
    /// health-probe, and (optionally) retry the whole start — in one call.
    ///
    /// The returned [`LaunchedSession`] releases the session when closed or
    /// dropped. See the [module docs](crate::session) for the scope boundary:
    /// this is a thin convenience over [`crate::connect`], not a Playwright
    /// object model.
    ///
    /// Note on profile seeding: an attached profile's **cookies** are seeded
    /// into the live browser. `localStorage` is **not** seeded — Playwright
    /// (the TS/Python path) restores it for free, but chromiumoxide has no
    /// equivalent and doing it here would require navigating to each origin.
    /// Seed it yourself against [`LaunchedSession::browser`] if you need it.
    pub async fn launch(&self, opts: LaunchOptions) -> Result<LaunchedSession, SolariError> {
        let want_probe = opts.probe.unwrap_or(opts.retries > 0);
        let mut attempt: u32 = 0;

        loop {
            let session = match self.sessions().create(opts.create.clone()).await {
                Ok(session) => session,
                Err(e) => {
                    // No session was created -- nothing to release. Create's
                    // own failure must be retried the same way launch_attempt's
                    // is, or opts.retries silently does nothing for anyone
                    // whose create() lands during a transient outage.
                    if attempt < opts.retries && is_transient(&e) {
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            };

            let outcome = self.launch_attempt(&session, want_probe, opts.probe_timeout_ms).await;
            match outcome {
                Ok(connected) => {
                    return Ok(LaunchedSession {
                        browser: connected.browser,
                        handler: connected.handler,
                        session,
                        client: self.clone(),
                        released: false,
                    });
                }
                Err(e) => {
                    // Release the failed session regardless.
                    let _ = self.sessions().release(&session.id).await;
                    if attempt < opts.retries && is_transient(&e) {
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// One connect+seed+probe attempt against an already-created session.
    async fn launch_attempt(
        &self,
        session: &Session,
        want_probe: bool,
        probe_timeout_ms: u64,
    ) -> Result<ConnectedBrowser, SolariError> {
        let connected = connect(session).await?;

        // Seed cookies from an attached profile, best-effort.
        if let Some(Some(state)) = &session.storage_state {
            seed_cookies(&connected.browser, state).await;
        }

        if want_probe {
            probe(&connected.browser, probe_timeout_ms).await?;
        }
        Ok(connected)
    }
}

/// A create/connect failure that is worth retrying (as opposed to a config or
/// auth error, which will fail again identically).
fn is_transient(e: &SolariError) -> bool {
    matches!(
        e,
        SolariError::Connect { .. } | SolariError::Transport { .. } | SolariError::Protocol { .. }
    ) || matches!(e, SolariError::Api { status, .. } if *status == 0 || *status >= 500)
}

/// Open a page, evaluate `1`, and confirm the browser answers within the
/// timeout. A failure means the slot is unhealthy — the caller retries.
async fn probe(
    browser: &chromiumoxide::browser::Browser,
    timeout_ms: u64,
) -> Result<(), SolariError> {
    let fut = async {
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| SolariError::Connect { message: format!("Solari: probe new_page failed: {e}") })?;
        page.evaluate("1")
            .await
            .map_err(|e| SolariError::Connect { message: format!("Solari: probe evaluate failed: {e}") })?;
        Ok::<(), SolariError>(())
    };
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
        Ok(r) => r,
        Err(_) => Err(SolariError::Connect {
            message: format!("Solari: browser health probe timed out after {timeout_ms}ms"),
        }),
    }
}

/// Seed a profile's cookies into the live browser. Best-effort: individual
/// cookies that will not set are skipped rather than failing the launch.
async fn seed_cookies(browser: &chromiumoxide::browser::Browser, state: &StorageState) {
    let Some(cookies) = &state.cookies else {
        return;
    };
    if cookies.is_empty() {
        return;
    }
    // A page is needed to reach the Network domain; about:blank is enough.
    let Ok(page) = browser.new_page("about:blank").await else {
        return;
    };
    for c in cookies {
        let mut b = CookieParam::builder().name(c.name.clone()).value(c.value.clone());
        if let Some(d) = &c.domain {
            b = b.domain(d.clone());
        }
        if let Some(p) = &c.path {
            b = b.path(p.clone());
        }
        if let Some(e) = c.expires {
            b = b.expires(TimeSinceEpoch::new(e));
        }
        if let Some(h) = c.http_only {
            b = b.http_only(h);
        }
        if let Some(s) = c.secure {
            b = b.secure(s);
        }
        if let Some(ss) = &c.same_site {
            b = b.same_site(map_same_site(ss));
        }
        if let Ok(param) = b.build() {
            let _ = page.set_cookie(param).await;
        }
    }
}

fn map_same_site(s: &SameSite) -> CookieSameSite {
    match s {
        SameSite::Strict => CookieSameSite::Strict,
        SameSite::Lax => CookieSameSite::Lax,
        SameSite::None => CookieSameSite::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> SolariError {
        SolariError::Api { status, code: None, message: "x".into() }
    }

    #[test]
    fn retry_policy_classifies_transience() {
        // Retryable: connect/transport/protocol failures and 5xx / no-status.
        assert!(is_transient(&SolariError::Connect { message: "x".into() }));
        assert!(is_transient(&SolariError::Transport { message: "x".into() }));
        assert!(is_transient(&SolariError::Protocol { message: "x".into() }));
        assert!(is_transient(&api(500)));
        assert!(is_transient(&api(503)));
        assert!(is_transient(&api(0)));
        // Not retryable: client errors and misconfiguration will fail identically.
        assert!(!is_transient(&api(400)));
        assert!(!is_transient(&api(404)));
        assert!(!is_transient(&SolariError::Config { message: "x".into() }));
    }

    #[test]
    fn same_site_maps_every_variant() {
        assert!(matches!(map_same_site(&SameSite::Strict), CookieSameSite::Strict));
        assert!(matches!(map_same_site(&SameSite::Lax), CookieSameSite::Lax));
        assert!(matches!(map_same_site(&SameSite::None), CookieSameSite::None));
    }
}
