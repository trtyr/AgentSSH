# AGENTS.md — agentssh

**Generated:** 2026-05-31 · **Commit:** `935b4fe` · **Branch:** `main`

## OVERVIEW

Standalone Rust SSH/SFTP CLI for AI-agent workflows. Microkernel design: the same `agentssh` binary is both client and daemon, dispatched by subcommand.

**Stack:** Rust edition 2024 (≥1.85) · `russh` 0.61 · `tokio` · `clap` (derive) · `serde_json`
**Pure Rust — no C deps. Unix only** (`UnixListener`/`UnixStream`).

## STRUCTURE

```
.
├── src/               # 12 flat .rs modules, no subdirectories
│   ├── main.rs        # boots cli::run()
│   ├── cli.rs         # clap parser + command dispatch (967 loc)
│   ├── kernel.rs      # daemon, Unix-socket IPC, ServerState (1148 loc — largest)
│   ├── connection.rs  # SSH connect pool, known_hosts TOFU (711 loc)
│   ├── session.rs     # RemoteSession, OutputBuffer, SessionStatus
│   ├── sftp.rs        # SFTP/SCP/exec file transfer with method fallback
│   ├── proxy.rs       # port forwarding + SOCKS5 (RFC 1928)
│   ├── ssh_backend.rs # one-shot exec/shell/file ops
│   ├── protocol.rs    # WireRequest/WireResponse, JSON-line wire protocol
│   ├── profile.rs     # CRUD for ~/.config/agentssh/profiles.json
│   └── util.rs        # constants, ANSI stripping, socket paths
├── tests/e2e.rs       # single integration test file (94 tests total across project)
├── docs/plantree/     # planning documents
└── Cargo.toml         # no [lints], no [profile], no [dev-dependencies]
```

## CODE MAP

**Entry points (crabmap):**
| Entry | File | Role |
|-------|------|------|
| `cli::run()` | `src/cli.rs` | top-level dispatch |
| `kernel::run_server()` | `src/kernel.rs` | daemon server loop |
| `kernel::run_client()` | `src/kernel.rs` | client → daemon IPC |
| `ssh_backend::run_shell()` | `src/ssh_backend.rs` | interactive PTY shell |

**Hot symbols (most connected):**
| Symbol | Kind | File | Degree | Role |
|--------|------|------|--------|------|
| `ServerState::new` | method | kernel.rs | 78 | daemon state init |
| `ConnectArgs` | struct | cli.rs | 47 | SSH connection config |
| `WireRequest` | enum | protocol.rs | 30 | IPC wire format |

**Fan-out (crabmap):**
| File | Fan-in | Fan-out | Total |
|------|--------|---------|-------|
| kernel.rs | 10 | 8 | 18 |
| cli.rs | 11 | 6 | 17 |
| connection.rs | 6 | 4 | 10 |
| util.rs | 8 | 1 | 9 |

## HEALTH (crabmap)

- **Score:** 100/100
- **Cycles:** none
- **God modules:** none
- **Dead code:** none

## ARCHITECTURE

Microkernel: CLI binary is client + daemon. Subcommands route to one-shot SSH or daemon-managed sessions:

| Command group | Path | Notes |
|---------------|------|-------|
| `exec` | `kernel::run_client(WireRequest::ExecOnce)` | daemon-managed with auto-suspend |
| `shell` | `ssh_backend::run_shell()` | one-shot; no daemon |
| `connect` | `kernel::run_client(WireRequest::Connect)` | long-lived PTY + pooled connection |
| `session *` | `kernel::run_client` / `run_read_command` | daemon-backed lifecycle |
| `file *` | daemon or one-shot via `--session-id` | dual-path routing |
| `proxy *` | `kernel::run_client` → async listener tasks | `-L` forward + `-D` SOCKS5 |
| `profile *` | `profile::run_profile()` | direct file I/O |
| `daemon *` | `kernel::run_server` / `Shutdown` | daemon lifecycle |

### Exec auto-suspend

`exec` goes through daemon. Default `--suspend-timeout 30000` (30s):
- Command finishes within timeout → return result immediately
- Still running after timeout → auto-suspend, return session ID + instructions
- `--suspend-timeout 0` → never suspend, run to completion

Suspended session commands:
```bash
agentssh session read --session-id s7        # get output
agentssh session status --session-id s7      # check status
agentssh session read --session-id s7 --follow  # wait for completion
```

### Daemon lifecycle

- `ensure_daemon()` spawns `agentssh daemon serve` if socket not alive.
- Socket: `$XDG_RUNTIME_DIR/agentssh-{user}.sock` (temp dir fallback).
- Wire protocol: one JSON `WireRequest` per line → one `WireResponse` per line. Stateless per-request.
- `connect` creates session + pooled connection. `session spawn --from <id>` reuses the same SSH connection (refcount).
- `session close` decrements refcount, drops SSH session when last channel closes.
- Heartbeat: every 60s drains PTY output. `--reconnect` auto-reconnects on disconnect.

### Session output model

- Daemon buffers PTY output with cursor. Each `send`/`read` returns page from cursor (default 8000 bytes limit).
- Buffer capped at 1 MB; oldest data trimmed on overflow.
- `send` supports up to 3 expect/respond pairs (case-insensitive regex or substring).
- `read --follow` is client-side: polls until exit, disconnect, SIGINT, or timeout.
- ANSI/PUA/control chars stripped (`util::strip_ansi`). Pass `--raw` to bypass.

### SFTP dual path

- **With `--session-id`** → daemon, reuses SSH session.
- **Without** → one-shot connection (`ssh_backend::run_*_once`).
- `--method auto` fallback: SFTP → SCP → exec (base64 + PS for Windows). SCP upload needs `wait_eof()` before `wait_close()`.

### Proxy (port forwarding & SOCKS5)

- `--local host:port --remote host:port` → local port forward via `channel_direct_tcpip`.
- `--socks5 host:port` → SOCKS5 proxy, no-auth CONNECT only (RFC 1928).
- macOS: `set_nonblocking(true)` on listener → accepted streams inherit. SOCKS5 handshake sets blocking for handshake frames, then non-blocking for forward loop.

## SECURITY

- **TOFU host keys:** `~/.ssh/known_hosts`. First connection appends, subsequent verify. Mismatch aborts. Non-standard ports use `[host]:port`.
- **Exec fallback escaping:** `shell_words::quote()` prevents shell injection in file transfer exec path.

## JSON OUTPUT

Pass `--output json` before subcommand: `agentssh --output json exec --profile prod -- id`. Most commands: `{"ok":true,...}`. `session read --follow`: one compact JSON object per line.

## EXEC SHELL MODEL

`exec` and `session exec` join args with spaces → `/bin/sh -c`. Shell metacharacters (`|`, `>`, `&&`, `;`) interpreted by remote shell. Escape on CLI: `\|`, `\>`, `\&\&`, or quote: `agentssh exec -- "cmd1 | cmd2"`.

## TESTING

94 tests total: 61 unit (inline `#[cfg(test)]` in 8 src files) + 33 E2E (`tests/e2e.rs`).

**E2E test tiers:**
| Tier | Count | Scope | Run |
|------|-------|-------|-----|
| P0 | 19 | CLI basics, profile CRUD, JSON output | `cargo test --test e2e` |
| P1 | 5 | daemon status, flag validation, file method | `cargo test --test e2e` |
| P2 | 9 | SSH-required (exec, shell, sessions) | `cargo test --test e2e -- --ignored` |

**E2E conventions:**
- Tests run binary via `cargo run -q -- <args>` subprocess — tests real CLI.
- Profile isolation: each test creates `AGENTSSH_CONFIG_DIR` temp dir.
- P2 tests marked `#[ignore]` + runtime env check (`AGENTSSH_E2E_SSH_HOST`).
- E2E requires: `AGENTSSH_E2E_SSH_HOST` (and optionally `_USER`, `_PORT`).

Run all (no SSH): `cargo test`

## CONVENTIONS

- `anyhow::Result<()>` throughout; errors with `.context()`.
- Serde derive on all command structs — doubles as wire format.
- PTY defaults: 120×40 (`DEFAULT_COLS`/`DEFAULT_ROWS` in `util.rs`).
- No `[lints]`, no `rustfmt.toml`, no CI/CD, no `[profile.release]` — all tool defaults.
- `WireRequest` uses `#[serde(tag = "action", rename_all = "snake_case")]`.
- `use crate::cli::*` glob imports in `kernel.rs` and `sftp.rs` — CLI types leak across modules.
