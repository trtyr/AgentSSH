---
name: agentssh
version: 0.2.0
description: |
  AI-agent SSH capability. Use when the agent needs to run commands on remote
  servers, transfer files, manage long-lived SSH sessions, or set up SSH
  tunnels. Wraps the agentssh CLI into a first-class agent capability.

  All output (success AND error) is structured JSON when --json is active.
  Short flags available: -p (profile), -H (host), -P (port), -u (username),
  -s (session-id).

  Triggers: SSH into server, deploy to remote, check server status, scp/sftp
  file transfer, remote diagnostics, port forwarding, SOCKS5 proxy.
---

# AgentSSH — AI-Agent SSH Capability

This skill gives you the ability to operate remote servers via SSH. You run
`agentssh` commands through your shell — the same way you run `git` or `npm`.

## Installation

```bash
cargo install agentssh          # latest from crates.io
agentssh --version              # verify it works
```

Prerequisites: `libssh2` (macOS: `brew install libssh2`, Linux: `libssh2-dev`).

## Mental model

agentssh has a **daemon** that stays alive in the background. Connections and
sessions live inside it. You talk to it through CLI commands.

```
you  ──[CLI]──►  agentssh daemon  ──[SSH]──►  remote server
```

Three tiers of operations:

| Tier | What | Persistent? | Use for... |
|------|------|-------------|------------|
| **One-shot** | `exec`, `file ls/upload/download/read/write/edit/delete` | No | Quick commands, file ops |
| **Session** | `connect`, `session send/read/exec/spawn` | Yes | Interactive shells, stateful work |
| **Proxy** | `proxy create` | Yes | Tunnels, SOCKS5 |

## First time: save a profile

You need a profile before connecting. Store credentials once, reuse by name.

```bash
# The simplest form
agentssh profile add my-server \
  --host 192.168.1.100 \
  --username root

# With SSH key (recommended)
agentssh profile add prod \
  --host app.prod.example.com \
  --username deploy \
  --private-key-path ~/.ssh/id_ed25519

# With password (last resort — you'll be prompted interactively)
agentssh profile add staging \
  --host staging.example.com \
  --username admin
```

Profiles live at `~/.config/agentssh/profiles.json`. Override with inline args:
```bash
agentssh exec --profile prod --host emergency.example.com -- uptime
# inline --host wins over profile
```

**Set a default profile** to skip `--profile` on every command:
```bash
export AGENTSSH_DEFAULT_PROFILE=prod
agentssh --json exec -- uptime  # --profile inferred from env var
```
Inline `--profile` still overrides the default.

## Output modes

agentssh supports two output modes for `exec` and `session exec`:

- **`--json`** (recommended for agents): structured JSON — `{"ok": true, "data": {"exit_status": 0, "stdout": "...", "stderr": ""}}`
- **`--output json`** (same as `--json`)
- **No flag** / **`--output text`** (for humans): prints stdout directly, stderr to stderr. Non-zero exit code produces an error.

**All errors are also JSON when `--json` is active:**

```json
{"ok": false, "error": "command timed out after 30000ms"}
{"ok": false, "error": "profile 'nonexistent' not found. Run 'agentssh profile list' to see saved profiles."}
```

You never need to check "is this JSON or plain text?" — when `--json` is passed, output is always valid JSON. Parse `ok` to determine success/failure.

```bash
agentssh exec --profile prod -- ls              # plain text output (default)
agentssh exec --profile prod --output text -- ls # explicit plain text
agentssh --json exec --profile prod -- ls        # JSON output
agentssh exec --profile prod --output json -- ls # explicit JSON
```

All other commands (`session read`, `session list`, etc.) still require `--json` for structured output.

## Command reference

### Tier 1: One-shot (no daemon)

**Run a single command and get the result:**

```bash
agentssh exec --profile <name> -- <command> [args...]

# Examples
agentssh --json exec --profile prod -- uptime
agentssh --json exec --profile prod -- docker ps
agentssh --json exec --profile prod --retry 3 -- systemctl status nginx
agentssh --json exec --profile prod --timeout 30000 -- "long_running_script.sh"
```

Output: `{"exit_status": <int>, "stdout": "<string>", "stderr": "<string>"}`.

`exec` has a **default 30-second timeout**. Override with `--timeout <ms>`. When
a command times out, `--json` output is `{"ok": false, "error": "command timed out after 30000ms"}`.

Use `exec` when you need a single answer and don't need state. No daemon
required — connects, runs, disconnects. Fast.

**Open an interactive shell (raw PTY):**

```bash
agentssh shell --profile <name>
```

This is a direct TTY passthrough to your terminal. Not for agent use — only
when the human wants an interactive shell.

**File operations (one-shot mode):**

```bash
# Upload
agentssh file upload --profile prod \
  --local ./deploy.tar.gz \
  --remote /tmp/deploy.tar.gz

# Download
agentssh file download --profile prod \
  --remote /var/log/app.log \
  --local ./app.log

# List directory
agentssh --json file ls --profile prod --remote /var/www
```

File transfer auto-negotiates protocol: SFTP → SCP → exec (base64 for
Windows). Force a method with `--method sftp` or `--method scp`.

**Direct file operations (read/write/edit/delete):**

```bash
# Write (overwrite)
agentssh file write --profile prod --remote /etc/config.ini --content "[app]\nport=8080\n"

# Write (append)
agentssh file write --profile prod --remote /var/log/app.log --content "new entry\n" --append

# Read (plain text, no --json needed)
agentssh file read --profile prod --remote /etc/config.ini

# Read (JSON with metadata)
agentssh --json file read --profile prod --remote /etc/config.ini

# Edit (literal find/replace)
agentssh file edit --profile prod --remote /etc/config.ini \
  --find "port=8080" --replace "port=9090"

# Edit (regex)
agentssh file edit --profile prod --remote /etc/config.ini \
  --find "port=\\d+" --replace "port=9090" --regex

# Delete
agentssh file delete --profile prod --remote /tmp/old.log

# Delete (recursive)
agentssh file delete --profile prod --remote /tmp/build --recursive
```

`file read` without `--json` prints the file content directly — no JSON wrapping, no \n escaping.

### Tier 2: Long-lived sessions (daemon-backed)

Sessions keep an SSH connection alive. You can send multiple commands into the
same shell context (environment variables, working directory, etc. persist).

**Start a session:**

```bash
agentssh connect --profile <name>
# → session_id: s1

# With auto-reconnect (recommended for long-running tasks)
agentssh connect --profile <name> --reconnect
```

The daemon auto-starts if not running. Use `--reconnect` to survive network
glitches — the daemon reconnects automatically.

**Run a clean command (no PTY noise):**

```bash
# Works on any session, including connect-created sessions
agentssh --json session exec --session-id s1 -- <command>
# Returns clean {"exit_status": n, "stdout": "...", "stderr": ""}

# With timeout (milliseconds):
agentssh --json session exec --session-id s1 --timeout 60000 -- "dnf install nginx"
```

Use `session exec` for commands where you need structured output. No shell
prompt garbage, no ANSI codes. Opens a dedicated SSH connection using the
session's stored credentials — no PTY channel conflict. For maximum speed
on repeated commands, use one-shot `exec` with `AGENTSSH_DEFAULT_PROFILE`.

**Fire-and-forget with --detach:**

```bash
# Start a background service without waiting for it to finish
agentssh --json session exec --session-id s1 --detach -- "nohup python app.py &"
# → {"ok": true, "data": {"detached": true, "status": "dispatched"}}
```

`--detach` writes the command to the session's PTY and returns immediately.
Useful for starting long-running services, background jobs, or daemons.

**Send input into the PTY (interactive mode):**

```bash
agentssh session send --session-id s1 --input "ls -la\n"

# With expect/respond pairs (up to 3)
agentssh session send --session-id s1 \
  --input "sudo systemctl restart nginx\n" \
  --expect "[sudo] password" \
  --respond "thepassword\n"
```

**Read session output:**

```bash
agentssh --json session read --session-id s1           # latest output
agentssh --json session read --session-id s1 --follow  # stream until exit
```

Output is cleaned by default: ANSI codes, Nerd Font icons, and control
characters are stripped. Pass `--raw` to get raw PTY bytes.

**Spawn another PTY on the same SSH connection:**

```bash
agentssh session spawn --from s1
# → session_id: s2 (shares SSH connection with s1, separate shell)
```

**Check health:**

```bash
agentssh --json session ping --session-id s1
# → {"alive": true, "status": "running"}
```

**List all sessions:**

```bash
agentssh --json session list
```

**Close a session:**

```bash
agentssh session close --session-id s1
```

Always close sessions when done. The daemon stays alive — only the session
goes away.

### Tier 3: Port forwarding & SOCKS5 proxy

**Local port forward (access remote internal services):**

```bash
agentssh proxy create --profile prod \
  --local 127.0.0.1:5432 \
  --remote 127.0.0.1:5432
# → proxy_id: p1
# Now localhost:5432 → remote PostgreSQL

# With a human-readable name (easier to identify later)
agentssh proxy create --profile prod \
  --local 127.0.0.1:5432 \
  --remote 127.0.0.1:5432 \
  --name pg-tunnel
# → proxy_id: pg-tunnel
```

# Use the forwarded port
psql -h 127.0.0.1 -p 5432 -U app
```

**SOCKS5 dynamic proxy (route all traffic through remote):**

```bash
agentssh proxy create --profile prod --socks5 127.0.0.1:1080
# → proxy_id: p1

# Use with any SOCKS5-aware tool
curl --socks5 127.0.0.1:1080 http://internal-service/
```

**Manage proxies:**

```bash
agentssh --json proxy list
agentssh --json proxy ping --proxy-id p1
agentssh proxy close --proxy-id p1
agentssh proxy close --all
```

### Daemon lifecycle

```bash
agentssh daemon shutdown    # stop the daemon (loses all sessions)
agentssh daemon status      # PID, uptime, active sessions/proxies
agentssh daemon install     # install as systemd user service (Linux only)
agentssh daemon uninstall   # remove the systemd service
```

The daemon auto-starts when you run `connect`, `session`, or `proxy` commands.
Only shut it down when you're completely done.

`daemon status` returns JSON when `--json` is passed:
`{"running": true, "pid": 12345, "uptime_seconds": 3600, "sessions": 2, "proxies": 1}`.

## Common workflows

### Deploy an application

```bash
agentssh --json exec --profile prod --retry 3 -- "docker ps"
# → check current state

agentssh file upload --profile prod --local ./app.tar.gz --remote /tmp/app.tar.gz

agentssh --json exec --profile prod -- "
  cd /opt/app &&
  tar xzf /tmp/app.tar.gz &&
  docker compose up -d --build &&
  docker compose ps
"
```

### Multi-server check

```bash
for profile in prod staging backup; do
  echo "=== $profile ==="
  agentssh --json exec --profile "$profile" -- "df -h / && free -m"
done
```

### Interactive debugging session

```bash
agentssh connect --profile prod --reconnect
# → s1

agentssh session send --session-id s1 --input "cd /var/log\n"
agentssh session send --session-id s1 --input "tail -100 app.log\n"
agentssh --json session read --session-id s1

# Run a clean diagnostic
agentssh --json session exec --session-id s1 -- "journalctl -u nginx --since '5 min ago'"

agentssh session close --session-id s1
```

### Long-running task with reconnect

```bash
# Start with auto-reconnect
agentssh connect --profile prod --reconnect
# s1

# Kick off a long build
agentssh session send --session-id s1 --input "make -j8 2>&1 | tee /tmp/build.log\n"

# Read output periodically (survives network drops)
sleep 60
agentssh --json session read --session-id s1

sleep 60
agentssh --json session read --session-id s1

# Check if reconnect happened
agentssh session read --session-id s1 | grep "\[AgentSSH\]"
```

### Access internal service through SOCKS5

```bash
agentssh proxy create --profile bastion --socks5 127.0.0.1:1080
# p1

# API calls through the tunnel
curl --socks5 127.0.0.1:1080 http://internal-api/health

# Database access
psql "host=127.0.0.1 port=5432"  # if you have a separate port forward

agentssh proxy close --all
```

## Session output model

Session output is buffered in the daemon with a read cursor:

- `session send` returns new output since last read
- `session read` returns new output since last read
- Cursor advances — repeated reads get only fresh data
- Buffer capped at 1 MB (oldest trimmed)

`session read --follow` streams output continuously until the shell exits,
SSH disconnects, or you interrupt.

## Error handling

When `--json` is active, **all output is JSON** — both success and failure:

```json
// Success
{"ok": true, "data": {"exit_status": 0, "stdout": "hello\n", "stderr": ""}}

// Error — timeout
{"ok": false, "error": "command timed out after 30000ms"}

// Error — connection refused
{"ok": false, "error": "failed to connect to 10.0.0.1:2222: Connection refused"}

// Error — profile not found
{"ok": false, "error": "profile 'nonexistent' not found. Run 'agentssh profile list' to see saved profiles."}
```

**You never need to handle mixed JSON/text output.** Parse `ok` to determine
success vs failure. No need to check "is this valid JSON?" — it always is.

Common failures:
- **Connection refused**: server unreachable or SSH port closed
- **Authentication failed**: wrong key, wrong password
- **Session not found**: session ID doesn't exist (already closed?)
- **Profile not found**: check `agentssh profile list`

For retryable failures, use `--retry <n>` on `exec` commands.

## Safety rules

1. **Never hardcode passwords in commands.** Use `--private-key-path` with SSH
   keys. If you must use a password, use `--password` with a secret manager
   variable, or let agentssh prompt interactively.

2. **Always close sessions when done.** `session close` prevents connection
   leaks.

3. **Use --reconnect for sessions that run > 5 minutes.** Network drops are
   normal.

4. **Prefer `session exec` over `session send`** when you want structured
   command output. `session exec` returns clean JSON — no shell prompt noise.

5. **Prefer one-shot `exec` over sessions** for single-command tasks. Faster,
   no daemon state to manage.

6. **The daemon is in-memory only.** `daemon shutdown` or machine restart
   loses all sessions. Plan accordingly.

## Output cleaning

By default, session output has:
- ANSI escape sequences removed
- Nerd Font / Private Use Area characters stripped
- Control characters removed (except `\n`, `\r`, `\t`)

Use `--raw` on `session send` or `session read` to get unmodified PTY output.

## Quick reference card

```bash
# Profiles
agentssh profile add <name> --host <h> --username <u>
agentssh profile list
agentssh profile delete <name>
# Set default profile to avoid repeating --profile
export AGENTSSH_DEFAULT_PROFILE=my-server
# Short flags: -p (profile), -H (host), -P (port), -u (username), -s (session-id)
agentssh -p prod --json exec -- uptime
# Explicit output format control
agentssh --json exec --profile prod -- <cmd>
agentssh --output text exec --profile prod -- <cmd>

# One-shot (default 30s timeout)
agentssh --json exec -p <profile> -- <cmd>
agentssh --json exec -p <profile> --timeout 60000 -- <cmd>
agentssh --json exec -p <profile> --retry 3 -- <cmd>
agentssh --json file ls -p <profile> --remote <path>
agentssh file upload -p <profile> --local <l> --remote <r>
agentssh file download -p <profile> --remote <r> --local <l>
agentssh file write -p <profile> --remote <r> --content <c> [--append]
agentssh file read -p <profile> --remote <r>
agentssh file edit -p <profile> --remote <r> --find <f> --replace <r> [--regex]
agentssh file delete -p <profile> --remote <r> [--recursive]

# Sessions
agentssh connect -p <profile> --reconnect
agentssh --json session exec -s <id> -- <cmd>
agentssh --json session exec -s <id> --timeout 60000 -- <cmd>
agentssh --json session exec -s <id> --detach -- "background_job &"
agentssh session send -s <id> --input "<text>"
agentssh --json session read -s <id> [--follow]
agentssh session spawn --from <id>
agentssh --json session list
agentssh --json session ping -s <id>
agentssh session close -s <id>

# Proxy
agentssh proxy create -p <profile> --local <l> --remote <r> [--name <name>]
agentssh proxy create -p <profile> --socks5 <addr> [--name <name>]
agentssh --json proxy list
agentssh proxy close --all

# Daemon
agentssh daemon status
agentssh daemon shutdown
agentssh daemon install      # Linux only, systemd user service
agentssh daemon uninstall    # Linux only
```
