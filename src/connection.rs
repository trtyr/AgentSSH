use crate::cli::ConnectArgs;
use crate::kernel::ServerState;
use crate::profile::read_profiles;
use crate::util::{DEFAULT_PORT, DEFAULT_READY_TIMEOUT_MS, expand_home, sleep_ms};
use anyhow::{Context, Result, anyhow, bail};
use ssh2::{Channel, Session};
use std::io::Read;
use std::net::TcpStream;

pub struct SessionIdentity {
    pub host: String,
    pub port: u16,
    pub username: String,
}

pub struct SharedConnection {
    pub ssh: Session,
    pub refcount: u32,
}

pub fn next_connection_id(state: &mut ServerState) -> String {
    state.next_connection_id += 1;
    format!("c{}", state.next_connection_id)
}

pub fn get_connection_mut<'a>(
    state: &'a mut ServerState,
    connection_id: &str,
) -> Result<&'a mut SharedConnection> {
    state
        .connections
        .get_mut(connection_id)
        .ok_or_else(|| anyhow!("connection '{}' not found", connection_id))
}

pub fn connect_with_info(mut args: ConnectArgs) -> Result<(Session, String, u16, String)> {
    let profile_name = args.profile.clone().or_else(|| {
        std::env::var("AGENTSSH_DEFAULT_PROFILE").ok()
    });
    if let Some(profile_name) = profile_name {
        let profile = read_profiles()?
            .profiles
            .get(&profile_name)
            .ok_or_else(|| {
                anyhow!(
                    "profile '{}' not found. Run 'agentssh profile list' to see saved profiles.",
                    profile_name
                )
            })?
            .clone();
        args = merge_connect_args(args, profile);
    }
    let host = args
        .host
        .clone()
        .ok_or_else(|| anyhow!("host is required. Provide --host or use --profile with a saved profile."))?;
    let username = args
        .username
        .clone()
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "root".to_string()));
    let port = args.port.unwrap_or(DEFAULT_PORT);
    let ready_timeout_ms = args.ready_timeout_ms.unwrap_or(DEFAULT_READY_TIMEOUT_MS);
    let retries = args.retry.unwrap_or(0);
    let retry_delay_ms = args.retry_delay_ms.unwrap_or(0);

    for attempt in 0..=retries {
        let stream = match TcpStream::connect((host.as_str(), port)).with_context(|| {
            format!(
                "failed to connect to {host}:{port}. Check that the host is reachable and the port is correct."
            )
        }) {
            Ok(stream) => stream,
            Err(error) => {
                if attempt < retries {
                    sleep_ms(retry_delay_ms);
                    continue;
                }
                return Err(error);
            }
        };
        stream.set_read_timeout(Some(std::time::Duration::from_millis(ready_timeout_ms)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_millis(ready_timeout_ms)))?;
        let mut session = Session::new()?;
        session.set_tcp_stream(stream);
        match session.handshake() {
            Ok(()) => {
                authenticate(&session, &username, &args)?;
                if !session.authenticated() {
                    bail!(
                        "authentication failed for {}@{}:{}. Check your credentials, key path, or SSH agent.",
                        username,
                        host,
                        port
                    );
                }
                return Ok((session, host, port, username));
            }
            Err(error) => {
                if attempt < retries {
                    sleep_ms(retry_delay_ms);
                    continue;
                }
                return Err(error.into());
            }
        }
    }

    unreachable!("retry loop should return or error")
}

pub fn authenticate(session: &Session, username: &str, args: &ConnectArgs) -> Result<()> {
    if let Some(key) = key_content(args)? {
        session.userauth_pubkey_memory(username, None, &key, passphrase(args)?.as_deref())?;
        return Ok(());
    }
    if let Some(path) = args.private_key_path.as_ref() {
        session.userauth_pubkey_file(
            username,
            None,
            &expand_home(path),
            passphrase(args)?.as_deref(),
        )?;
        return Ok(());
    }
    if let Some(password) = password(args)? {
        session.userauth_password(username, &password)?;
        return Ok(());
    }
    session
        .userauth_agent(username)
        .context("ssh-agent authentication failed. Is ssh-agent running? Try 'ssh-add -l' to verify.")
}

pub fn merge_connect_args(input: ConnectArgs, profile: ConnectArgs) -> ConnectArgs {
    ConnectArgs {
        profile: input.profile,
        host: input.host.or(profile.host),
        port: input.port.or(profile.port),
        username: input.username.or(profile.username),
        password: input.password.or(profile.password),
        password_env: input.password_env.or(profile.password_env),
        private_key_path: input.private_key_path.or(profile.private_key_path),
        private_key_env: input.private_key_env.or(profile.private_key_env),
        passphrase: input.passphrase.or(profile.passphrase),
        passphrase_env: input.passphrase_env.or(profile.passphrase_env),
        ready_timeout_ms: input.ready_timeout_ms.or(profile.ready_timeout_ms),
        retry: input.retry.or(profile.retry),
        retry_delay_ms: input.retry_delay_ms.or(profile.retry_delay_ms),
    }
}

pub fn key_content(args: &ConnectArgs) -> Result<Option<String>> {
    match args.private_key_env.as_ref() {
        Some(name) => Ok(Some(std::env::var(name).with_context(|| format!("read env {name}"))?)),
        None => Ok(None),
    }
}

pub fn password(args: &ConnectArgs) -> Result<Option<String>> {
    if let Some(value) = args.password.clone() {
        return Ok(Some(value));
    }
    if let Some(name) = args.password_env.as_ref() {
        return Ok(Some(std::env::var(name).with_context(|| format!("read env {name}"))?));
    }
    Ok(None)
}

pub fn passphrase(args: &ConnectArgs) -> Result<Option<String>> {
    if let Some(value) = args.passphrase.clone() {
        return Ok(Some(value));
    }
    if let Some(name) = args.passphrase_env.as_ref() {
        return Ok(Some(std::env::var(name).with_context(|| format!("read env {name}"))?));
    }
    Ok(None)
}

/// Execute a command on a channel, optionally with a timeout.
/// When timeout is None, uses blocking reads (original behavior).
/// When timeout is Some(ms), uses non-blocking polling and returns
/// an error if the command exceeds the timeout.
///
/// The session's blocking mode is modified by this function.
/// Callers should restore the desired blocking mode after this returns.
pub fn exec_channel(
    session: &Session,
    channel: &mut Channel,
    command_line: &str,
    timeout: Option<u64>,
) -> Result<(String, String, i32)> {
    // channel.exec() may return EAGAIN (-37) when the SSH transport has
    // pending data from other channels (e.g. a PTY from `connect`). Retry.
    let start = std::time::Instant::now();
    loop {
        match channel.exec(command_line) {
            Ok(()) => break,
            Err(e) if e.code() == ssh2::ErrorCode::Session(-37) => {
                if start.elapsed().as_millis() > 5000 {
                    return Err(anyhow::Error::from(e)
                        .context("exec timed out after 5s — transport may be stalled"));
                }
                sleep_ms(100);
            }
            Err(e) => return Err(e.into()),
        }
    }

    if timeout.is_none() {
        session.set_blocking(true);
        let mut stdout = String::new();
        let mut stderr = String::new();
        channel.read_to_string(&mut stdout)?;
        channel.stderr().read_to_string(&mut stderr)?;
        channel.wait_close()?;
        let exit_status = channel.exit_status()?;
        return Ok((stdout, stderr, exit_status));
    }

    let timeout_ms = timeout.unwrap();
    session.set_blocking(false);

    let mut stdout = Vec::new();
    let start = std::time::Instant::now();
    let mut buf = [0u8; 8192];

    loop {
        let elapsed = start.elapsed().as_millis();
        if elapsed > timeout_ms as u128 {
            let _ = channel.close();
            bail!("command timed out after {timeout_ms}ms");
        }

        match channel.read(&mut buf) {
            Ok(0) if channel.eof() => break,
            Ok(0) => {}
            Ok(n) => {
                stdout.extend_from_slice(&buf[..n]);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        sleep_ms(100);
    }

    let mut stderr = String::new();
    channel.stderr().read_to_string(&mut stderr)?;
    channel.wait_close()?;
    let exit_status = channel.exit_status()?;

    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
        exit_status,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_connect_args_preserves_retry_fields() {
        let input = ConnectArgs {
            retry: Some(3),
            retry_delay_ms: Some(250),
            ..Default::default()
        };
        let profile = ConnectArgs {
            retry: Some(1),
            retry_delay_ms: Some(1000),
            ..Default::default()
        };

        let merged = merge_connect_args(input, profile);

        assert_eq!(merged.retry, Some(3));
        assert_eq!(merged.retry_delay_ms, Some(250));
    }
}
