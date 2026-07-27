# solari-browser (Rust)

Rust binding for the **Solari Browser** platform — the **control plane**: acquire
and release pooled stealth Chromium sessions, manage stored browser profiles,
pull session replays, and inspect managed-proxy egress. It speaks the same wire
contract as the reference `@solarisdk/browser` TypeScript package
(`sdk/src/index.ts`).

`reqwest` (rustls) for REST, `serde` for the wire types, `thiserror` for
`SolariError`. Optionally [`chromiumoxide`] for driving the browser.

The crate is `solari-browser`; the root module is `solari_browser`. (Distinct
from the desktop/sandbox crate, which is `solari-sdk` / `solari`.)

## Scope: no `launch()`, and why

The TypeScript SDK's `Solari.launch()` returns a live **Playwright** `Browser`.
Playwright has no Rust binding, so there is no Rust port of it, and there won't
be one.

Rust drives sessions over the **raw CDP** endpoint instead, via
[`chromiumoxide`] — enable the `connect` feature and use
`solari_browser::connect()`. Note the distinction on every `Session`:

| field          | protocol                  | use from Rust                          |
| -------------- | ------------------------- | -------------------------------------- |
| `ws_endpoint`  | Playwright wire protocol  | ❌ chromiumoxide can't speak this      |
| `cdp_endpoint` | raw CDP                   | ✅ this is the one — Puppeteer-shaped  |

One other deliberate divergence: the TS SDK proxies both endpoints through a
Node-side loopback `LocalProxy`. This crate returns the **upstream** gateway
endpoints verbatim — there is nothing to route through.

## Install

Control plane only (dependency-light — `reqwest`, `serde`, `thiserror`, `url`):

```toml
[dependencies]
solari-browser = { path = "sdk/rust" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

With browser automation (pulls in `chromiumoxide`):

```toml
[dependencies]
solari-browser = { path = "sdk/rust", features = ["connect"] }
```

## Usage

```rust
use solari_browser::{Client, ClientOptions, CreateSessionOptions, ProxySpec};

#[tokio::main]
async fn main() -> Result<(), solari_browser::SolariError> {
    // Default region "us-west" → https://api.getsolari.com
    let client = Client::new(ClientOptions::default_region("slr_live_…"))?;

    // Or point at staging / a self-hosted gateway:
    //   Client::new(ClientOptions::new("slr_live_…", "https://gw.example.com"))?
    // There is no env-var fallback — the key and URL are always explicit.

    let session = client
        .sessions()
        .create(
            CreateSessionOptions::new()
                .stealth(true)
                .proxy(ProxySpec::country("us")),
        )
        .await?;

    println!("cdp {}", session.cdp_endpoint);
    println!("expires {}", session.expires_at);

    client.sessions().release(&session.id).await?;
    Ok(())
}
```

### Driving the browser (feature `connect`)

```rust
use solari_browser::CreateSessionOptions;

let session = client
    .sessions()
    .create(CreateSessionOptions::new().stealth(true))
    .await?;

// Attaches to session.cdp_endpoint; returns the chromiumoxide Browser plus the
// spawned task pumping its event handler (every call resolves through it).
let connected = solari_browser::connect(&session).await?;

let page = connected.browser.new_page("https://example.com").await.unwrap();
println!("{:?}", page.url().await.ok());

connected.disconnect().await;          // drops the browser, awaits the handler
client.sessions().release(&session.id).await?;
```

### Profiles

```rust
use solari_browser::{CreateSessionOptions, StorageState};

let profile = client.profiles().create("amazon").await?;

// Attach it — the session's storage_state is fetched from its presigned URL.
let session = client
    .sessions()
    .create(CreateSessionOptions::new().profile_id(&profile.id))
    .await?;

match &session.storage_state {
    None => println!("no profile attached"),
    Some(None) => println!("profile attached, but empty"),
    Some(Some(state)) => println!("{} cookie(s)", state.cookies.as_ref().map_or(0, |c| c.len())),
}

// …drive the browser, then persist what it accumulated.
client.profiles().save(&profile.id, &StorageState::default()).await?;

for p in client.profiles().list().await? {
    println!("{} {}", p.id, p.name);
}
```

`storage_state` is three-state, mirroring the TS `storageState?: StorageState | null`:
`None` = no profile attached, `Some(None)` = profile attached but empty,
`Some(Some(_))` = the profile's state.

### Managed proxy

```rust
use solari_browser::{ProxyRequest, ProxySpec, ProxyTier};

ProxySpec::country("gb");                          // proxy: "gb"
ProxySpec::smart();                                // gateway sweeps on block
ProxySpec::off();                                  // explicitly disabled
ProxySpec::from(
    ProxyRequest::country("us")
        .tier(ProxyTier::Mobile)                   // residential | static | mobile
        .session("warm-1")                         // sticky egress IP
        .session_duration(15),                     // minutes (1–30)
);
```

Managed proxy requires `stealth(true)`. `client.proxy().countries()` lists the
supported egress countries and reports whether the gateway has proxy credentials
configured at all — this endpoint has no TypeScript equivalent.

### Replays

```rust
client.sessions().release(id).await?;             // replays land ~1–3s after release

let replay = client.sessions().replay_url(id).await?;
println!("{} ({}, {}s)", replay.url, replay.content_encoding, replay.expires_in_seconds);

let bytes = client.sessions().download_replay(id).await?;   // NDJSON, gzip by default
```

## Errors

Every fallible method returns `Result<T, SolariError>`:

| variant     | when                                                        |
| ----------- | ----------------------------------------------------------- |
| `Api`       | non-2xx from the API — carries `status` + parsed `code`      |
| `Transport` | network failure, or retries exhausted                       |
| `Protocol`  | a 2xx body that was malformed or missing required fields    |
| `Config`    | invalid client options (empty api key, bad base URL)        |
| `Connect`   | CDP attach failed (feature `connect`)                       |

`err.status()` and `err.code()` work on any error. `code()` yields the typed
`SolariErrorCode` — `FeatureRequiresPlan`, `ConcurrencyLimitExceeded`,
`PlanLimitExceeded`, `BrowserUnhealthy` — with unknown wire codes preserved as
`Other(String)` rather than dropped.

```rust
use solari_browser::SolariErrorCode;

match err.code() {
    Some(SolariErrorCode::ConcurrencyLimitExceeded) => { /* back off and retry */ }
    Some(SolariErrorCode::FeatureRequiresPlan) => { /* upgrade required */ }
    _ => {}
}
```

## Retries

Defaults: `max_attempts = 2`, `backoff_ms = 500` (a **fixed** delay, not
exponential), `timeout_ms = 90_000`. Only **502 / 503 / 504** and transport
errors are retried — every other status, including 4xx and 500, returns on the
first attempt. Tune per-client:

```rust
use solari_browser::{Client, ClientOptions};

let client = Client::new(
    ClientOptions::default_region("slr_live_…")
        .max_attempts(3)
        .backoff_ms(250)
        .timeout_ms(30_000),
)?;
```

`Debug` for `Client` and `ClientOptions` is hand-written to redact the API key —
neither will print your credentials.

## Testing

Fully offline — the API is mocked with an in-process TCP HTTP server; no live
gateway is needed.

```
cargo build
cargo test
```

The `connect` feature is off by default, so the core build never pulls in
`chromiumoxide`. To verify it too: `cargo build --features connect`.

[`chromiumoxide`]: https://docs.rs/chromiumoxide
