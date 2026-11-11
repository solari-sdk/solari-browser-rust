# Changelog

## 0.6.6
- **`connect` / `connect_endpoint`: a WebSocket handshake with no answer now fails at a bound instead of waiting for ever (hardening).** chromiumoxide has no dial timeout, so when the gateway accepted the connection but never completed the upgrade the call waited for ever unless you wrapped it. The handshake is now bounded by `DEFAULT_DIAL_TIMEOUT` (30 s); the `SolariError::Connect` message says "no WebSocket handshake answer within 30000ms". `connect_endpoint_timeout(endpoint, d)` sets another bound. It covers only the handshake: give every later call on the browser your own `tokio::time::timeout`.

## 0.6.5
- **The gateway's error body no longer carries the session id into error messages.** `GET /sessions/<id>/replay-url` answered `404 ReplayPending` with the full `sessionId` in its body and `SolariError::Api` appended that body. The id is the credential inside the ws/cdp URLs. The body is now redacted (the request path's id, in any spelling, and every `"sessionId"` field cut to 8 characters); `code()` is still parsed from the raw body.
- The dependency line in the crate docs (what docs.rs shows) said `version = "0.2"`, which does not exist as a fetchable release (HTTP 411 from the registry); it now says `"0.6"`, and a test keeps README.md and the crate docs in step with the crate version.

## 0.6.4
- README only: the Install section no longer names an internal path and the dependency lines use the current version (crates.io and docs.rs showed the old `"0.2"`).

## 0.6.3
- A session timeout above 2147483647 seconds (`session_timeout_ms` or the deprecated `timeout_seconds`) is refused before any request, matching the Go SDK and the gateway's int32 field.

## 0.6.2

- **Errors and `{:?}` output no longer print the session's credential URLs.** A failed `connect()` / `connect_endpoint()` used to put the full CDP endpoint (which carries the session's credential in its path) into the error text, and a failed replay download printed the signed URL. They now show `scheme://host/…` only, and `Session`'s `Debug` hides the session id and the endpoint paths. The session id in `/sessions/<id>` (the credential inside those URLs) is cut to its first 8 characters in REST error messages too.

## 0.6.1

- `launch`: the default health-probe timeout is now 15 s (was 2 s). The probe opens a page and evaluates in it, and on a cold pool slot that can take longer than 2 s, so a healthy browser could fail the probe and be retried or released. `probe_timeout_ms` still sets it. Go (`ProbeTimeoutMs`) and C++ (`probeTimeoutMs`) use the same 15 s bound for the connect step.

## 0.6.0

- `profiles().edit(id, reason)` opens the profile editor on an existing profile and returns a
  `ProfileEditor` with `url`, `expires_at`, `status()`, `save_and_close()`, `exit()` and `wait_for_save()`.
- `Profile::version` is now populated from `profiles().list()`.
