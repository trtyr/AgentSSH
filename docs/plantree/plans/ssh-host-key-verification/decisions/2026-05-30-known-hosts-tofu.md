# Decision: use `~/.ssh/known_hosts` with TOFU for `russh` connections

Date: 2026-05-30

## Context

`src/connection.rs` previously accepted all SSH server host keys, leaving the `russh` client path vulnerable to man-in-the-middle attacks.

## Decision

- Validate host keys against `~/.ssh/known_hosts`.
- For default port 22, use plain `hostname` entries.
- For non-default ports, use OpenSSH bracketed syntax: `[hostname]:port`.
- If a host already exists with a different key, reject the connection.
- If a host is not present, append the observed key to `known_hosts` and trust it for future connections (TOFU).

## Consequences

- First connection remains trust-on-first-use rather than pre-pinned trust.
- Subsequent host key changes are detected and blocked.
- Hashed `known_hosts` hostnames are currently ignored for matching and preserved as-is.
