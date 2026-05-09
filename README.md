# 🦾 AgentSSH

[![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20|%20Linux-lightgrey.svg)]()

**AI-native SSH toolkit.** Not a human terminal. Not an OpenSSH wrapper. A programmable backend that speaks JSON — built for agents, works for humans.

AgentSSH talks SSH directly through `libssh2`. No shell wrappers. No screen scraping. No heuristics.

---

## ✨ Why AgentSSH

|  | Traditional SSH | AgentSSH |
|---|---|---|
| 🎯 **Output** | Raw ANSI terminal text | Structured JSON with status codes |
| 🔌 **Connection** | Connect → run → disconnect | Long-lived daemon, session reuse |
| 📡 **Port forwarding** | `ssh -L` / `ssh -D` per terminal | Daemon-managed tunnels & SOCKS5 proxy |
| 🗂️ **File transfer** | Separate `scp`/`sftp` binary | Built-in SFTP → SCP → exec fallback chain |
| 🔐 **Auth config** | `~/.ssh/config` (custom grammar) | JSON profiles, machine-writable |
| 🪟 **Windows** | Works but clunky | Auto-detects & uses PowerShell fallback |
| 📟 **Output nav** | Scroll back in terminal | Cursor-based paging with `--offset` / `--limit` |

---

## 🚀 Quick start

```bash
# Prerequisites
brew install libssh2        # macOS
# apt install libssh2-dev   # Linux

# Build
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

### 🔄 Long-lived sessions

```bash
agentssh connect --profile prod
# → session_id: s1 (background daemon auto-starts)

agentssh session send --session-id s1 --input "ls -la\n"
agentssh session send --session-id s1 \
  --input "sudo systemctl restart nginx\n" \
  --expect "[sudo] password" \
  --respond "mypassword\n"

agentssh session read --session-id s1          # latest output
agentssh session read --session-id s1 --follow # stream live
agentssh session spawn --from s1               # new PTY on same SSH conn

agentssh session ping --session-id s1
agentssh session close --session-id s1
```

### 📁 File transfer (multi-protocol)

```bash
# Default auto: SFTP → SCP → exec (works on Linux, macOS, and Windows!)
agentssh file upload --profile prod --local ./app --remote /opt/app
agentssh file download --profile prod --remote /var/log/syslog --local ./syslog
agentssh file ls --profile prod --remote /var/www

# Force a specific protocol
agentssh file upload --profile windows --method scp --local ./app --remote C:/app
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
                                     SSH (libssh2)
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
agentssh file upload                   # Upload (SFTP → SCP → exec)
agentssh file download                 # Download (SFTP → SCP → exec)
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
- **libssh2** system library (`brew install libssh2` / `apt install libssh2-dev`)
- **Unix only** (uses Unix domain sockets)

```bash
cargo build --release
# → target/release/agentssh
```

---

## 📄 License

MIT
