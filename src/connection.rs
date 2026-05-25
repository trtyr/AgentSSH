use crate::cli::ConnectArgs;
use crate::util::{self, DEFAULT_EXEC_TIMEOUT_MS, DEFAULT_PORT};
use anyhow::{Context, Result, bail};
use russh::keys::{self, PrivateKeyWithHashAlg};
use russh::client::AuthResult;
use russh::MethodSet;
use russh::{client, Channel, ChannelMsg};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Client handler for russh (host key verification)
// ---------------------------------------------------------------------------

pub(crate) struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept all host keys (same behavior as ssh2 path which used
        // known_hosts lookup only when explicitly requested — agentssh
        // currently skips verification).
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Shared connection handle
// ---------------------------------------------------------------------------

pub type SharedConnectionHandle = client::Handle<ClientHandler>;

pub struct SharedConnection {
    pub handle: Arc<SharedConnectionHandle>,
    pub refcount: u32,
}

// ---------------------------------------------------------------------------
// Connect + authenticate
// ---------------------------------------------------------------------------

/// Connect to SSH server and authenticate.
/// Returns (Handle, host, port, username).
pub async fn connect_with_info(
    args: &ConnectArgs,
) -> Result<(Arc<SharedConnectionHandle>, String, u16, String)> {
    let host = args
        .host
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--host is required"))?
        .to_string();
    let port = args.port.unwrap_or(DEFAULT_PORT);
    let username = args
        .username
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--username is required"))?
        .to_string();

    let config = client::Config {
        keepalive_interval: Some(Duration::from_secs(60)),
        keepalive_max: 3,
        ..Default::default()
    };

    let handler = ClientHandler;
    let mut handle = client::connect(Arc::new(config), (host.as_str(), port), handler)
        .await
        .with_context(|| format!("connecting to {}:{}", host, port))?;

    authenticate(&mut handle, &username, args)
        .await
        .with_context(|| "authentication failed".to_string())?;

    Ok((Arc::new(handle), host, port, username))
}

/// Try authentication methods in order: key_env → key_file → password → ssh-agent.
async fn authenticate(
    handle: &mut SharedConnectionHandle,
    username: &str,
    args: &ConnectArgs,
) -> Result<()> {
    // 1. Key from env variable content
    if let Some(content) = key_content(args) {
        let key = keys::decode_secret_key(&content, passphrase(args).as_deref())
            .context("decoding private key from content")?;
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .context("getting supported RSA hash")?;
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg.flatten());
        let ok = handle
            .authenticate_publickey(username, key_with_alg)
            .await
            .context("publickey auth from content")?;
        if ok.success() {
            return Ok(());
        }
    }

    // 2. Pubkey file (--private-key-path)
    if let Some(key_path) = args.private_key_path.as_ref() {
        let expanded = util::expand_home(key_path);
        let key = keys::load_secret_key(expanded, passphrase(args).as_deref())
            .with_context(|| format!("loading private key from {:?}", key_path))?;
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .context("getting supported RSA hash")?;
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg.flatten());
        let ok = handle
            .authenticate_publickey(username, key_with_alg)
            .await
            .with_context(|| format!("publickey auth with {:?}", key_path))?;
        if ok.success() {
            return Ok(());
        }
    }

    // 3. Password (inline --password or from --password-env)
    let env_password = password_from_env(args);
    if let Some(pass) = args.password.as_deref().or(env_password.as_deref()) {
        let ok = handle
            .authenticate_password(username, pass)
            .await
            .context("password auth")?;
        if ok.success() {
            return Ok(());
        }
    }

    // 4. SSH agent (fallback when no explicit auth)
    if args.private_key_path.is_none() && args.password.is_none() && key_content(args).is_none() {
        let mut agent = keys::agent::client::AgentClient::connect_env()
            .await
            .context("connecting to SSH agent")?;
        let identities = agent
            .request_identities()
            .await
            .context("listing agent identities")?;
        for identity in identities {
            let pubkey = identity.public_key().into_owned();
            let hash_alg = handle
                .best_supported_rsa_hash()
                .await
                .context("getting supported RSA hash")?;
            let ok = handle
                .authenticate_publickey_with(username, pubkey, hash_alg.flatten(), &mut agent)
                .await
                .unwrap_or(AuthResult::Failure {
                    remaining_methods: MethodSet::empty(),
                    partial_success: false,
                });
            if ok.success() {
                return Ok(());
            }
        }
    }

    bail!("all authentication methods failed")
}

// ---------------------------------------------------------------------------
// Exec a command on a channel
// ---------------------------------------------------------------------------

/// Execute a command on an already-opened channel and return (stdout, stderr, exit_code).
pub async fn exec_channel(
    channel: &mut Channel<client::Msg>,
    cmd: &str,
    timeout_ms: Option<u64>,
) -> Result<(String, String, i32)> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_EXEC_TIMEOUT_MS));

    channel
        .exec(true, cmd)
        .await
        .with_context(|| format!("exec: {}", cmd))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code: i32 = -1;
    let mut saw_eof = false;
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("exec timed out after {}ms", timeout.as_millis());
        }

        match tokio::time::timeout(remaining, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                stdout.extend_from_slice(&data);
            }
            Ok(Some(ChannelMsg::ExtendedData { data, ext: 1 })) => {
                stderr.extend_from_slice(&data);
            }
            Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                exit_code = exit_status as i32;
                if saw_eof {
                    break;
                }
            }
            Ok(Some(ChannelMsg::Eof)) => {
                saw_eof = true;
                if exit_code != -1 {
                    break;
                }
            }
            Ok(None) => {
                break;
            }
            Ok(Some(_)) => {} // ignore other messages
            Err(_) => {
                bail!("exec timed out after {}ms", timeout.as_millis());
            }
        }
    }

    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve ConnectArgs by merging with profile. Inline args take precedence.
pub fn resolve_connect_args(args: &ConnectArgs) -> Result<ConnectArgs> {
    if let Some(profile_name) = &args.profile {
        let store = crate::profile::read_profiles()?;
        let profile_args = store
            .profiles
            .get(profile_name)
            .ok_or_else(|| anyhow::anyhow!("profile '{}' not found", profile_name))?
            .clone();
        Ok(merge_connect_args(&profile_args, args))
    } else {
        Ok(args.clone())
    }
}

/// Merge CLI connect args with profile defaults. Inline args take precedence.
pub fn merge_connect_args(base: &ConnectArgs, override_args: &ConnectArgs) -> ConnectArgs {
    ConnectArgs {
        profile: override_args.profile.clone().or_else(|| base.profile.clone()),
        host: override_args.host.clone().or_else(|| base.host.clone()),
        port: override_args.port.or(base.port),
        username: override_args
            .username
            .clone()
            .or_else(|| base.username.clone()),
        password: override_args
            .password
            .clone()
            .or_else(|| base.password.clone()),
        password_env: override_args
            .password_env
            .clone()
            .or_else(|| base.password_env.clone()),
        private_key_path: override_args
            .private_key_path
            .clone()
            .or_else(|| base.private_key_path.clone()),
        private_key_env: override_args
            .private_key_env
            .clone()
            .or_else(|| base.private_key_env.clone()),
        passphrase: override_args
            .passphrase
            .clone()
            .or_else(|| base.passphrase.clone()),
        passphrase_env: override_args
            .passphrase_env
            .clone()
            .or_else(|| base.passphrase_env.clone()),
        ready_timeout_ms: override_args.ready_timeout_ms.or(base.ready_timeout_ms),
        retry: override_args.retry.or(base.retry),
        retry_delay_ms: override_args.retry_delay_ms.or(base.retry_delay_ms),
    }
}

/// Get key content from --private-key-env environment variable.
fn key_content(args: &ConnectArgs) -> Option<String> {
    args.private_key_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
}

/// Get password from --password-env environment variable.
fn password_from_env(args: &ConnectArgs) -> Option<String> {
    args.password_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
}

/// Get passphrase (from env or direct).
fn passphrase(args: &ConnectArgs) -> Option<String> {
    args.passphrase
        .clone()
        .or_else(|| {
            args.passphrase_env
                .as_ref()
                .and_then(|var| std::env::var(var).ok())
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn merge_connect_args_preserves_retry_fields() {
        let base = ConnectArgs {
            profile: None,
            host: Some("base-host".into()),
            port: Some(2222),
            username: Some("base-user".into()),
            password: None,
            password_env: None,
            private_key_path: None,
            private_key_env: None,
            passphrase: None,
            passphrase_env: None,
            ready_timeout_ms: None,
            retry: Some(3),
            retry_delay_ms: Some(500),
        };
        let override_args = ConnectArgs {
            profile: None,
            host: Some("override-host".into()),
            port: None,
            username: None,
            password: None,
            password_env: None,
            private_key_path: None,
            private_key_env: None,
            passphrase: None,
            passphrase_env: None,
            ready_timeout_ms: None,
            retry: None,
            retry_delay_ms: None,
        };
        let merged = merge_connect_args(&base, &override_args);
        assert_eq!(merged.host.as_deref(), Some("override-host"));
        assert_eq!(merged.port, Some(2222));
        assert_eq!(merged.retry, Some(3));
        assert_eq!(merged.retry_delay_ms, Some(500));
    }
}
