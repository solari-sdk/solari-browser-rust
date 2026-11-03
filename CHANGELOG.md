# Changelog

## 0.6.0

- `profiles().edit(id, reason)` opens the profile editor on an existing profile and returns a
  `ProfileEditor` with `url`, `expires_at`, `status()`, `save_and_close()`, `exit()` and `wait_for_save()`.
- `Profile::version` is now populated from `profiles().list()`.
