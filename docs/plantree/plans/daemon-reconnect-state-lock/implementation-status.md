# Daemon reconnect and state lock fixes

## Status

Done — implemented on 2026-05-30.

## Completed

- Reconnect returns and installs the fresh drain-task `cmd_tx`.
- Reconnect updates the shared connection handle in `ServerState::connections`.
- `handle_client` passes `Arc<Mutex<ServerState>>` into request handling instead of holding a global mutable guard for the whole request.
- Slow SSH/SFTP/proxy operations now clone handles or reserve IDs under short locks, release the lock for I/O, then reacquire only to update state.
- Re-indexed Rust code map with `crabmap index .`.

## Verification

- `rtk cargo build` — passes, with existing dead-code warnings for legacy daemon helper functions now bypassed by lock-aware request handlers.
- `rtk cargo test` — 31 passed.
- Debug artifact scan for `dbg!`, `TODO`, `FIXME`, `HACK`, `todo!`, `unimplemented!` — clean.

## Follow-up

- Consider removing or refactoring now-unused legacy `daemon_*` helpers in `session.rs`, `sftp.rs`, and `proxy.rs` if warnings are not desired.
