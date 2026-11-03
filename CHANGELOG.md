# Changelog

## 0.6.2

- **Errors and `{:?}` output no longer print the session's credential URLs.** A failed `connect()` / `connect_endpoint()` used to put the full CDP endpoint (which carries the session's credential in its path) into the error text, and a failed replay download printed the signed URL. They now show `scheme://host/…` only, and `Session`'s `Debug` hides the session id and the endpoint paths.

## 0.6.1

- `launch`: the default health-probe timeout is now 15 s (was 2 s). The probe opens a page and evaluates in it, and on a cold pool slot that can take longer than 2 s, so a healthy browser could fail the probe and be retried or released. `probe_timeout_ms` still sets it. Go (`ProbeTimeoutMs`) and C++ (`probeTimeoutMs`) use the same 15 s bound for the connect step.

## 0.6.0

- `profiles().edit(id, reason)` opens the profile editor on an existing profile and returns a
  `ProfileEditor` with `url`, `expires_at`, `status()`, `save_and_close()`, `exit()` and `wait_for_save()`.
- `Profile::version` is now populated from `profiles().list()`.
