//! Attach to a live session's browser over raw CDP.
//!
//! Behind the **`connect`** cargo feature (off by default) so the control-plane
//! surface stays dependency-light.
//!
//! There is no Rust port of `Solari.launch()`: the TypeScript SDK returns a live
//! *Playwright* `Browser`, and Playwright has no Rust binding. Rust drives the
//! session over the raw CDP endpoint instead, via [`chromiumoxide`] — which is
//! why this helper attaches to [`Session::cdp_endpoint`] and not
//! `Session::ws_endpoint` (the latter speaks the Playwright wire protocol, which
//! chromiumoxide does not understand).

use chromiumoxide::browser::Browser;
use chromiumoxide::handler::HandlerConfig;
use futures_util::StreamExt;
use tokio::task::JoinHandle;

use crate::error::SolariError;
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

    /// Drop the browser (which ends the CDP connection) and await the handler.
    ///
    /// This does **not** release the Solari session — call
    /// [`crate::Sessions::release`] for that.
    pub async fn disconnect(self) {
        drop(self.browser);
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
    let (browser, mut handler) =
        Browser::connect_with_config(cdp_endpoint.to_string(), HandlerConfig::default())
            .await
            .map_err(|e| SolariError::Connect {
                message: format!("Solari: failed to connect to {cdp_endpoint}: {e}"),
            })?;

    // Pump the handler; without this nothing on `browser` ever resolves.
    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    Ok(ConnectedBrowser {
        browser,
        handler: handle,
    })
}
