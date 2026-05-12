# AGENTS.md — agentssh

Standalone Rust SSH/SFTP CLI for AI-agent SSH and SFTP workflows.

## Build & run

```bash
cargo build                    # debug build
cargo run -- <subcommand> ...  # run any command
cargo build --release          # release binary at target/release/agentssh
```

**Rust edition 2024** — needs Rust ≥ 1.85. If `cargo build` fails with edition errors, update rustup.

**libssh2 C library** — the `ssh2` crate wraps libssh2. On macOS: `brew install libssh2`. On Linux: `libssh2-dev` (or equivalent). If linking fails, install the system package, not a vendored build.

**Unix only.** Uses `UnixListener`/`UnixStream`; will not compile on Windows.

## Architecture

Microkernel design. The CLI binary is both client and daemon — the same binary dispatched by subcommand:

| Command group   | Path                                                                 | Notes                                                                     |
| --------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| `exec`, `shell` | `ssh_backend::run_exec/shell()`                                      | One-shot; no daemon needed. `exec` joins args with spaces and runs via `/bin/sh -c` so pipes/redirects/&& work. |
| `connect`       | `kernel::run_client(WireRequest::Connect)`                           | Starts a long-lived PTY session and a shared SSH connection entry         |
| `session *`     | `kernel::run_client()` / `kernel::run_read_command()`                | Daemon-backed session lifecycle, spawn, exec, health, and streaming reads |
| `file *`        | daemon or one-shot SSH depending on `--session-id`                   | Shared transfer structs, dual-path routing                                |
| `proxy *`       | `kernel::run_client()` → daemon-managed threads                      | Local port forwarding (`-L`) and SOCKS5 (`-D`) via `channel_direct_tcpip` |
| `profile *`     | `profile::run_profile()`                                             | Direct file I/O, no daemon                                                |
| `daemon *`      | `kernel::run_server()` / `kernel::run_client(WireRequest::Shutdown)` | Daemon lifecycle commands                                                 |

Key modules:

- `main.rs` — only boots `cli::run()`
- `cli.rs` — clap parser + grouped command dispatch
- `kernel.rs` — daemon lifecycle, Unix-socket IPC, `ServerState`, heartbeat thread, follow-read client loop
- `protocol.rs` — `WireRequest`/`WireResponse`, JSON-line wire protocol over Unix socket
- `ssh_backend.rs` — SSH/PTY/SFTP logic, session health refresh, ping handling
- `proxy.rs` — local port forwarding and SOCKS5 proxy, listener threads, SOCKS5 handshake (RFC 1928)
- `profile.rs` — CRUD for `~/.config/agentssh/profiles.json`
- `util.rs` — constants (defaults), `config_dir()`, `runtime_socket_path()`, `expand_home()`, JSON helpers

## Daemon lifecycle

- Client commands that need a long-lived session (`connect`, `session send`, `session read`, etc.) call `ensure_daemon()` which spawns `agentssh daemon serve` as a background process if the socket isn't alive.
- Socket path: `$XDG_RUNTIME_DIR/agentssh-{sanitized_user}.sock` (falls back to temp dir).
- The daemon stays up until `agentssh daemon shutdown` or killed. Sessions are in-memory only — restart = all sessions lost.
- Wire protocol: one JSON `WireRequest` per line → one JSON `WireResponse` per line. Stateless per-request; session state lives in `ServerState::sessions`.
- `ServerState::connections` tracks shared SSH `Session` objects by `connection_id`; each entry holds the authenticated SSH session plus a channel refcount.
- `connect` creates both a session record and a new pooled connection. `session spawn --from <id>` opens a fresh PTY channel on the same pooled SSH connection, increments the refcount, and creates another session record.
- `session exec --session-id <id> -- <command>` runs a single command on the session's SSH connection via `channel.exec()` — no PTY, returns clean stdout/stderr/exit_code. Command args are joined with spaces and wrapped in `/bin/sh -c`, so shell metacharacters (`|`, `>`, `&&`, `;`) are interpreted by the remote shell. Contrast with `session send` which sends raw text through the PTY channel (suitable for interactive use).
- `session close` decrements the connection refcount and drops the pooled SSH session when the last channel closes.
- A background heartbeat thread wakes every 60 seconds and drains PTY output for each session.
- When `--reconnect` is passed to `connect`, the heartbeat also watches for SSH transport disconnections and automatically re-establishes the connection with a fresh PTY channel. Reconnect uses the stored `ConnectArgs` for authentication and appends a `[AgentSSH] session <id> reconnected` notice to the output buffer.

## Profiles

- Stored at `~/.config/agentssh/profiles.json` (overridable via `AGENTSSH_CONFIG_DIR` env var).
- Format: `{"profiles": {"name": {ConnectArgs fields ...}}}` — a flat JSON object of named `ConnectArgs`.
- `--profile prod` merges with any inline `--host`/`--username`/etc.: inline args take precedence.

## JSON output

Pass `--json` _before_ the subcommand: `agentssh --json exec --profile prod -- id`. Most commands wrap output in `{"ok":true,"data":...}`. `agentssh --json session read --follow ...` instead emits one compact JSON object per line so clients can stream updates incrementally.

## Exec command shell model

Both one-shot `exec` and `session exec` join all trailing args with spaces and run them through `/bin/sh -c` on the remote host. This means:

- Shell metacharacters (`|`, `>`, `&&`, `;`) are interpreted by the remote shell
- Escaping metacharacters on the CLI prevents the local shell from eating them: `\|`, `\>`, `\&\&`
- Or quote the entire command: `agentssh exec -- "grep ERROR /var/log/syslog | tail -5"`
- This matches `ssh`, `kubectl exec`, and `docker exec` behavior

## Session output model

The daemon buffers PTY output in `RemoteSession::output` with a cursor. Each `session send`/`session read` drains the channel, appends to the buffer, and returns a page (text from cursor, limited by `--limit`, default 8000 bytes). The cursor advances so repeated reads return new output. Buffer is capped at `MAX_BUFFER` (1 MB) — oldest data is trimmed when the limit is hit.

`session send` also supports up to three expect/respond pairs. After sending the primary input, the daemon scans the accumulated output buffer for each `expect` pattern using case-insensitive regex matching when the pattern compiles, or case-insensitive substring fallback otherwise. On match, it sends the paired `respond` text and drains output again before returning.

`session read --follow` is implemented client-side: it repeatedly issues normal `Read` requests, prints one JSON line per page, and stops on shell exit, disconnect, SIGINT, or timeout.

### Output cleaning

PTY output is scrubbed before being returned to callers (`util::strip_ansi`). The cleaning pipeline removes:

- ANSI escape sequences (CSI, OSC, simple ESC codes)
- Private Use Area characters (`U+E000`–`U+F8FF`) — eliminates Nerd Font icon garbage
- Non-printable control characters except `\n`, `\r`, `\t`

This keeps session output readable for AI agents even when the remote shell uses fancy prompts. Pass `--raw` on `session send` or `session read` to bypass cleaning and get the raw PTY bytes.

## Session health

- `session ping --session-id <id>` refreshes the target session and returns `alive` plus current `status`.
- Health for spawned sessions is shared at the SSH transport level because all channels in the same `connection_id` use one pooled `ssh2::Session`.
- Dead-but-not-closed sessions are marked `disconnected`.

## SFTP dual path (important)

`file upload`, `file download`, and `file ls` each have two code paths:

1. **With `--session-id`** → routed through daemon (`kernel::run_client`), reuses the existing SSH session.
2. **Without `--session-id`** → one-shot SSH connection (`ssh_backend::run_*_once`), connects + does the operation + disconnects.

The `TransferCommand` and `ListCommand` structs carry both `ConnectArgs` and optional `session_id`. The routing decision lives in `cli.rs`.

### Transfer method fallback

`--method auto` (default) tries protocols in order:

1. **SFTP** — full-featured; works on most Linux servers.
2. **SCP** — simpler protocol via `scp_send`/`scp_recv`; works when SFTP subsystem is disabled.
3. **exec** — base64 + PowerShell (`[IO.File]::WriteAllBytes` / `[IO.File]::ReadAllBytes`); handles Windows OpenSSH servers where neither SFTP nor SCP is available.

SCP upload requires `wait_eof()` before `wait_close()` to avoid `LIBSSH2_ERROR_CHANNEL_WAIT_CLOSED`. Exec upload is limited to ~22 KB of raw file data (base64-encoded command ≤ 30,000 chars). Use `--method sftp` or `--method scp` to bypass fallback and force a specific protocol.

## Proxy (port forwarding & SOCKS5)

`agentssh proxy` provides SSH tunneled proxies managed by the daemon, with two modes:

- **Local port forwarding** (`-L`): `--local <host:port> --remote <host:port>` — listens locally, forwards each connection through `channel_direct_tcpip` to the remote target.
- **SOCKS5** (`-D`): `--socks5 <host:port>` — local SOCKS5 proxy, every connection performs a SOCKS5 handshake (RFC 1928, no-auth CONNECT only) then tunnels the target through `channel_direct_tcpip`.

Each proxy lives in its own listener thread inside the daemon. Accepted connections spawn per-connection threads that run a non-blocking bidirectional forward loop between the local TCP stream and the SSH direct-tcpip channel. Proxy lifecycle mirrors sessions: `create`/`list`/`ping`/`close` (with `--all`). Each proxy increments the connection refcount on its pooled SSH session.

On macOS, `TcpListener::set_nonblocking(true)` causes accepted `TcpStream` sockets to inherit the non-blocking flag. The SOCKS5 handshake explicitly sets the stream back to blocking before reading the handshake frames, then switches to non-blocking for the forwarding loop.

## Testing

Small unit tests live in `cli.rs`, `kernel.rs`, and `ssh_backend.rs`. For manual smoke testing:

```bash
# Profile CRUD
cargo run -- profile write test --data '{"host":"localhost","username":"root"}'
cargo run -- profile list

# One-shot exec (needs a real SSH target)
cargo run -- exec --profile test -- uname -a

# Daemon session
cargo run -- connect --profile test
cargo run -- session send --session-id s1 --input "ls\n"
cargo run -- session exec --session-id s1 -- uname -a
cargo run -- session read --session-id s1
cargo run -- session ping --session-id s1
cargo run -- session close --session-id s1
cargo run -- daemon shutdown

# Proxy (port forwarding & SOCKS5)
cargo run -- proxy create --profile test --local 127.0.0.1:9999 --remote 127.0.0.1:8080
cargo run -- proxy list
cargo run -- proxy ping --proxy-id p1
cargo run -- proxy close --proxy-id p1
cargo run -- proxy create --profile test --socks5 127.0.0.1:1080
cargo run -- proxy close --all
```

## Code conventions

- `anyhow::Result<()>` throughout; errors propagated with `.context()`.
- Serde derive macros on nearly all command structs — they double as wire format.
- PTY dimensions default to 120×40 (`util.rs` constants).
- `ssh.set_blocking(true)` before blocking SFTP operations, restored to `false` after — because the daemon keeps sessions in non-blocking mode for async I/O.
