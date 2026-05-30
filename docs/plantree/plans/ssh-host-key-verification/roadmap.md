# SSH host key verification roadmap

## Done

- Inspect `russh::client::Handler::check_server_key` behavior and `ssh_key::PublicKey` encoding API from local crate sources.
- Implement `known_hosts` lookup and TOFU append path in `src/connection.rs`.
- Reject changed host keys for already-known hosts.
- Verify with `rtk cargo build` and `rtk cargo test`.

## In Progress

- None.

## Next

- Evaluate whether hashed-hostname matching should be added in a future hardening pass.

## Deferred

- Configurable strict unknown-host rejection mode via project config file.
