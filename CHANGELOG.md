# Changelog

## 0.6.4
- README only: the Install section no longer names an internal path and the dependency lines use the current version (crates.io and docs.rs showed the old `"0.2"`).

## 0.6.3
- A session timeout above 2147483647 seconds (`session_timeout_ms` or the deprecated `timeout_seconds`) is refused before any request, matching the Go SDK and the gateway's int32 field.
