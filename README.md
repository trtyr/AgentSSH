# 🦾 AgentSSH

[![Crates.io](https://img.shields.io/crates/v/agentssh?style=flat-square&logo=rust)](https://crates.io/crates/agentssh)
[![Rust](https://img.shields.io/badge/rust-1.85+-ed8225?style=flat-square&logo=rust&logoColor=white)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-22C55E?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20|%20Linux-8B5CF6?style=flat-square)]()
[![Downloads](https://img.shields.io/crates/d/agentssh?style=flat-square&label=downloads)](https://crates.io/crates/agentssh)

[📖 中文文档](README_CN.md)

**AI-native SSH toolkit. Not a human terminal. Not an OpenSSH wrapper.**

AgentSSH speaks SSH directly through `russh` — a pure-Rust async SSH implementation. One binary: client + daemon + proxy. No C library to install. No shell wrapping. All output in structured JSON. Built for agents, also works for humans.

[🐙 GitHub](https://github.com/cft0808/agentssh) · [📦 crates.io](https://crates.io/crates/agentssh) · [🔧 Quick Start](#-quick-start) · [🏗️ Architecture](#-architecture) · [📋 Command Reference](#-command-reference)

---

## 🤖 AI Agent Skill

Drop [`SKILL.md`](SKILL.md) into your agent's skill directory to give it SSH superpowers.

Your agent gains: `exec`, `file upload/download`, session management, port forwarding, SOCKS5 proxy — all via structured JSON. See [SKILL.md](SKILL.md) for the full capability definition.

---

## 🆚 Why AgentSSH

|  | `ssh` | `paramiko` | `libssh2` | **AgentSSH** |
|---|---|---|---|---|
| **Output** | Raw terminal | Mixed string | App parses | **✅ Structured JSON** |
| **Connections** | One shot | Manual | Manual | **✅ Daemon-pooled + reuse** |
| **File transfer** | `scp` / `sftp` | Separate impl | Separate impl | **✅ SFTP → exec built-in** |
| **Port forward** | `-L` / `-D` flags | Manual coding | Manual coding | **✅ Daemon-managed** |
| **PTY model** | Screen-scraped | Blocking reads | Polling loops | **✅ Async drain task** |
| **Auth config** | `~/.ssh/config` | Inline params | Inline params | **✅ JSON profiles** |
| **C dependency** | Yes | Yes | **Yes** | **None — pure Rust** |
| **Agent-first** | ❌ | ❌ | ❌ | **✅ `--json` everywhere** |

> **The last two rows are the whole point.** No C library to fight with. No screen scraping. Programs call AgentSSH like they'd call an API.

---

## 🚀 Quick start

```bash
# Install from crates.io (recommended)
cargo install agentssh

# Or build from source
cargo build --release
# → target/release/agentssh
```

### 🔐 Save a profile

```bash
agentssh profile add prod \
  --host example.com \
  --username root \
  --private-key-path ~/.ssh/id_ed25519

agentssh profile list
# tencent   root@82.157.147.224:22
# prod      root@example.com:22
```

### ⚡ Run & go (one-shot)

```bash
agentssh --json exec --profile prod --retry 3 -- uptime
# {"ok":true,"data":{"exit_status":0,"stdout":"21:03:01 up 42 days\n","stderr":""}}
```

Commands run through `/bin/sh -c` — pipes, redirects, and shell chains work natively:

```bash
# Pipe and filter
agentssh --json exec --profile prod -- grep ERROR /var/log/syslog \| tail -5

# Redirect output to remote file
agentssh --json exec --profile prod -- cat \> /etc/config \</dev/null

# Chain commands
agentssh --json exec --profile prod -- ls /etc \&\& systemctl status nginx
```

> **Tip**: Escape `|`, `>`, `&&` with backslashes so your local shell doesn't eat them.

### 🔄 Long-lived sessions

```bash
agentssh connect --profile prod --reconnect
# → session_id: s1 (auto-reconnect on disconnect)

# Clean command execution (no PTY echo — structured JSON)
agentssh session exec --session-id s1 -- uname -a
# → {"exit_status":0, "stdout":"Linux ...\n", "stderr":""}

# Interactive PTY mode
agentssh session send --session-id s1 --input $'ls -la\n'
agentssh session send --session-id s1 \
  --input $'sudo systemctl restart nginx\n' \
  --expect "[sudo] password" \
  --respond $'mypassword\n'

> **⚠️ `--input` needs a real newline.** `"echo hello\n"` sends two literal characters `\` and `n` — the shell won't execute the command. Use `$'echo hello\n'` (ANSI-C quoting) or embed an actual line break so the shell gets a real Enter.
>
> ```bash
> # ✅ ANSI-C quoting — sends a real Enter to the PTY
> agentssh session send --session-id s1 --input $'ls -la\n'
>
> # ❌ This sends literal \ and n — shell just echoes, never runs
> agentssh session send --session-id s1 --input "ls -la\n"
> ```

agentssh session read --session-id s1          # latest output
agentssh session read --session-id s1 --follow # stream live
agentssh session spawn --from s1               # new PTY on same SSH conn

agentssh session ping --session-id s1
agentssh session close --session-id s1
```

### 📁 File transfer (multi-protocol)

```bash
# Default auto: SFTP → exec (works on Linux, macOS, and Windows!)
agentssh file upload --profile prod --local ./app --remote /opt/app
agentssh file download --profile prod --remote /var/log/syslog --local ./syslog
agentssh file ls --profile prod --remote /var/www

# Force a specific protocol
agentssh file upload --profile prod --method sftp --local ./app --remote /opt/app
```

### 🌐 Port forwarding & SOCKS5

```bash
# Local port forward: localhost:9999 → remote internal :8080
agentssh proxy create --profile prod \
  --local 127.0.0.1:9999 \
  --remote 127.0.0.1:8080

# SOCKS5 dynamic proxy: route all traffic through remote host
agentssh proxy create --profile prod --socks5 127.0.0.1:1080
curl --socks5 127.0.0.1:1080 http://internal-service/

agentssh proxy list
agentssh proxy ping --proxy-id p1
agentssh proxy close --proxy-id p1
agentssh proxy close --all
```

---

## 🏗️ Architecture

```
┌──────────────┐     Unix Socket      ┌──────────────────┐
│  CLI client  │ ◄──────────────────► │  Daemon (serve)   │
│  (one-shot)  │    JSON-line IPC     │                   │
└──────────────┘                      │  ┌─────────────┐  │
                                      │  │ sessions    │  │
┌──────────────┐                      │  │ proxies     │  │
│  CLI client  │◄────────────────────►│  │ connections │  │
│  (session)   │                      │  └─────────────┘  │
└──────────────┘                      └───────┬──────────┘
                                              │
                                     SSH (russh)
                                              │
                                    ┌─────────▼─────────┐
                                    │  Remote servers    │
                                    │  Linux · macOS ·   │
                                    │  Windows           │
                                    └───────────────────┘
```

**Daemon** — auto-starts on first session/proxy command. Survives CLI invocations. Handles heartbeat, session health, connection pooling.

**Connection pooling** — sessions can share one TCP/SSH connection. `session spawn --from <id>` opens a fresh PTY on the same underlying connection.

**Proxy threads** — each `proxy create` spawns a listener thread inside the daemon. Accepted connections get their own handler thread with non-blocking bidirectional forwarding.

---

## 📋 Command reference

```bash
# ⚡ One-shot
agentssh exec                          # Run command → stdout + exit code
agentssh shell                         # Interactive PTY (human use)

# 🔄 Sessions
agentssh connect                       # Open PTY → session_id
agentssh session send                  # Send input (± expect/respond pairs)
agentssh session spawn --from s1       # New PTY on s1's SSH connection
agentssh session read                  # Read output from cursor
agentssh session resize                # Change PTY dimensions
agentssh session signal                # Send signal (INT, TERM, KILL…)
agentssh session status                # Get session metadata
agentssh session ping                  # Check if session is alive
agentssh session list                  # List sessions + connection groups
agentssh session close                 # Close session

# 📁 Files
agentssh file upload                   # Upload (SFTP → exec)
agentssh file download                 # Download (SFTP → exec)
agentssh file ls                       # List remote directory

# 🌐 Proxy & tunnels
agentssh proxy create                  # -L forward or -D SOCKS5
agentssh proxy list                    # List active proxies
agentssh proxy ping                    # Check proxy health
agentssh proxy close                   # Close one or --all

# 🔐 Profiles
agentssh profile list | read | add | write | delete

# ⚙️ Daemon
agentssh daemon serve                  # Start daemon (auto-started)
agentssh daemon shutdown               # Stop daemon + cleanup
```

---

## 🎛️ Configuration

| Path | Purpose |
|---|---|
| `~/.config/agentssh/profiles.json` | SSH connection profiles |
| `$XDG_RUNTIME_DIR/agentssh-{user}.sock` | Daemon Unix socket |
| `/tmp/agentssh-daemon.log` | Daemon log (override: `AGENTSSH_LOG`) |

### Profile format

```json
{
  "profiles": {
    "prod": {
      "host": "example.com",
      "port": 22,
      "username": "root",
      "private_key_path": "~/.ssh/id_ed25519",
      "retry": 3,
      "retry_delay_ms": 250
    }
  }
}
```

---

## 🔧 Building

- **Rust** ≥ 1.85 (edition 2024)
- **Unix only** (uses Unix domain sockets)
- **No C library required** — russh is pure Rust

```bash
cargo build --release
# → target/release/agentssh
```

---

## 📄 License

MIT
