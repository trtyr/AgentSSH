# Plan Tree

## Active Plans

| Plan | Status | Notes |
| --- | --- | --- |
| [Daemon reconnect and state lock fixes](plans/daemon-reconnect-state-lock/implementation-status.md) | Done | Reconnect now updates session command channel and connection handle; daemon request handling no longer holds global state lock across slow SSH/SFTP/proxy I/O. |
| [SSH host key verification](plans/ssh-host-key-verification/implementation-status.md) | Done | `russh` client path now verifies host keys against `~/.ssh/known_hosts`, rejects mismatches, and records first-seen keys with TOFU semantics. |

## Baseline

- Project context is documented in `AGENTS.md`.
- Rust code map is maintained with `crabmap index .`.
