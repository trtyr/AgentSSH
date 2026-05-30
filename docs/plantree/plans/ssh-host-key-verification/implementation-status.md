# SSH host key verification

## Status

Done — implemented on 2026-05-30.

## Completed

- `ClientHandler` now carries `host` and `port` so `check_server_key` can validate per target.
- `check_server_key` now loads `~/.ssh/known_hosts`, accepts matching keys, rejects mismatched keys, and appends first-seen keys with TOFU semantics.
- Known-host writes use temp-file + rename for atomic replacement and set restrictive permissions on Unix (`0700` for `.ssh`, `0600` for `known_hosts`).
- Added unit tests for host matching, mismatch rejection, non-default port formatting, and append behavior.

## Verification

- `rtk cargo build` — passes.
- `rtk cargo test` — 35 passed.
- `crabmap index .` — refreshed after Rust source change.

## Follow-up

- Current implementation preserves but does not match hashed `known_hosts` hostnames (`|1|...`) because the project does not yet carry the extra crypto support needed for OpenSSH-style host hashing.
- If product requirements need stricter behavior for unknown hosts, add a config-file-controlled strict mode instead of environment variables.
