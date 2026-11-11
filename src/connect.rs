//! Attach to a live session's browser over raw CDP.
//!
//! Behind the **`connect`** cargo feature (off by default) so the control-plane
//! surface stays dependency-light.
//!
//! Playwright has no Rust binding, so there is no port of the TypeScript
//! `BrowserSession` object model — Rust drives the session over the raw CDP
//! endpoint via [`chromiumoxide`], which is why this helper attaches to
//! [`Session::cdp_endpoint`] and not `Session::ws_endpoint` (the latter speaks
//! the Playwright wire protocol, which chromiumoxide does not understand). For a
//! one-call create+connect+seed+probe+retry+release convenience on top of this,
//! see [`crate::Client::launch`].

use chromiumoxide::browser::Browser;
use chromiumoxide::handler::HandlerConfig;
use futures_util::StreamExt;
use tokio::task::JoinHandle;

use crate::error::SolariError;
use crate::http::{redact_url, scrub_url_from};
use crate::types::Session;

/// A browser attached to a Solari session.
///
/// Holds the [`chromiumoxide::Browser`] plus the spawned task pumping its event
/// handler. The handler must keep running for as long as the browser is used —
/// every `browser` call resolves through it. It ends on its own once `browser`
/// is dropped.
pub struct ConnectedBrowser {
    /// The attached browser. Drive it with the usual chromiumoxide API.
    pub browser: Browser,
    /// The spawned handler pump.
    pub handler: JoinHandle<()>,
}

impl ConnectedBrowser {
    /// Split into `(browser, handler)`.
    pub fn into_parts(self) -> (Browser, JoinHandle<()>) {
        (self.browser, self.handler)
    }

    /// Drop the browser, end the handler task (which closes the CDP connection) and wait for it.
    ///
    /// This does **not** release the Solari session — call
    /// [`crate::Sessions::release`] for that.
    ///
    /// The handler does not end on its own when the browser is dropped: it keeps pumping the open WebSocket, so awaiting it
    /// without aborting it hung forever.
    pub async fn disconnect(self) {
        drop(self.browser);
        self.handler.abort();
        let _ = self.handler.await;
    }
}

/// Attach to `session.cdp_endpoint` and return the connected browser plus its
/// handler task.
///
/// ```no_run
/// # async fn ex(client: &solari_browser::Client) -> Result<(), solari_browser::SolariError> {
/// use solari_browser::CreateSessionOptions;
///
/// let session = client
///     .sessions()
///     .create(CreateSessionOptions::new().stealth(true))
///     .await?;
///
/// let connected = solari_browser::connect(&session).await?;
/// let page = connected
///     .browser
///     .new_page("https://example.com")
///     .await
///     .map_err(|e| solari_browser::SolariError::Connect { message: e.to_string() })?;
/// println!("{:?}", page.url().await.ok());
///
/// connected.disconnect().await;
/// client.sessions().release(&session.id).await?;
/// # Ok(()) }
/// ```
pub async fn connect(session: &Session) -> Result<ConnectedBrowser, SolariError> {
    connect_endpoint(&session.cdp_endpoint).await
}

/// Attach to an explicit CDP endpoint. Useful when you persisted a session's
/// `cdp_endpoint` and no longer hold the [`Session`].
pub async fn connect_endpoint(cdp_endpoint: &str) -> Result<ConnectedBrowser, SolariError> {
    connect_endpoint_timeout(cdp_endpoint, DEFAULT_DIAL_TIMEOUT).await
}

/// How long [`connect`] / [`connect_endpoint`] wait for the WebSocket handshake. The dial has no bound of its own in
/// chromiumoxide, so a gateway that accepts the connection and never completes the upgrade used to make the call wait for
/// ever. This bounds only the handshake: give every later call on the browser your own `tokio::time::timeout`.
pub const DEFAULT_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// [`connect_endpoint`] with an explicit bound on the WebSocket handshake. A handshake with no answer inside `dial_timeout` is a
/// [`SolariError::Connect`] that says so.
pub async fn connect_endpoint_timeout(
    cdp_endpoint: &str,
    dial_timeout: std::time::Duration,
) -> Result<ConnectedBrowser, SolariError> {
    let dial = Browser::connect_with_config(cdp_endpoint.to_string(), HandlerConfig::default());
    let (browser, mut handler) = match tokio::time::timeout(dial_timeout, dial).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            return Err(SolariError::Connect {
                message: format!(
                    "Solari: failed to connect to {}: {}",
                    redact_url(cdp_endpoint),
                    scrub_url_from(&e.to_string(), cdp_endpoint)
                ),
            })
        }
        Err(_) => {
            return Err(SolariError::Connect {
                message: format!(
                    "Solari: failed to connect to {}: no WebSocket handshake answer within {}ms (the gateway accepted the connection but did not complete the upgrade); retry, or raise the bound with connect_endpoint_timeout",
                    redact_url(cdp_endpoint),
                    dial_timeout.as_millis()
                ),
            })
        }
    };

    // Pump the handler; without this nothing on `browser` ever resolves.
    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    Ok(ConnectedBrowser {
        browser,
        handler: handle,
    })
}
