use crate::cli::ConnectArgs;
use crate::util::{self, DEFAULT_PORT};
use anyhow::{Context, Result, bail};
use russh::client::AuthResult;
use russh::keys::{self, PrivateKeyWithHashAlg};
use russh::MethodSet;
use russh::{client, Channel, ChannelMsg};
use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Client handler for russh (host key verification)
// ---------------------------------------------------------------------------

pub(crate) struct ClientHandler {
    pub host: String,
    pub port: u16,
}

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        verify_or_trust_host_key(&self.host, self.port, server_public_key)
            .with_context(|| format!("verifying SSH host key for {}:{}", self.host, self.port))
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
// known_hosts TOFU verification
// ---------------------------------------------------------------------------

fn verify_or_trust_host_key(
    host: &str,
    port: u16,
    server_public_key: &keys::PublicKey,
) -> Result<bool> {
    let known_hosts_path = known_hosts_path()?;
    let host_patterns = known_hosts_patterns(host, port);

    let existing = match fs::read_to_string(&known_hosts_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading {}", known_hosts_path.display()));
        }
    };

    if known_hosts_has_key(&existing, &host_patterns, server_public_key)? {
        return Ok(true);
    }

    append_known_host_atomically(&known_hosts_path, &existing, &host_patterns, server_public_key)?;
    Ok(true)
}

fn known_hosts_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not locate home directory"))?;
    Ok(home.join(".ssh").join("known_hosts"))
}

fn known_hosts_patterns(host: &str, port: u16) -> Vec<String> {
    if port == DEFAULT_PORT {
        vec![host.to_string()]
    } else {
        vec![format!("[{host}]:{port}")]
    }
}

fn known_hosts_has_key(
    contents: &str,
    host_patterns: &[String],
    server_public_key: &keys::PublicKey,
) -> Result<bool> {
    let wanted_key = canonical_public_key(server_public_key)?;

    for line in contents.lines() {
        let Some((patterns, public_key)) = parse_known_hosts_line(line) else {
            continue;
        };
        if !host_patterns.iter().any(|host| patterns_match(&patterns, host)) {
            continue;
        }

        let known_key = canonical_public_key(&public_key)?;
        if known_key == wanted_key {
            return Ok(true);
        }

        bail!(
            "SSH host key mismatch for {}; known_hosts contains a different key",
            host_patterns.join(",")
        );
    }

    Ok(false)
}

fn parse_known_hosts_line(line: &str) -> Option<(Vec<String>, keys::PublicKey)> {
    let line = strip_known_hosts_comment(line).trim();
    if line.is_empty() {
        return None;
    }

    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    let hosts = if first.starts_with('@') {
        parts.next()?
    } else {
        first
    };
    let algorithm = parts.next()?;
    let key = parts.next()?;
    let public_key = keys::PublicKey::from_openssh(&format!("{algorithm} {key}")).ok()?;

    Some((hosts.split(',').map(str::to_string).collect(), public_key))
}

fn strip_known_hosts_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(before_comment, _)| before_comment)
}

fn patterns_match(patterns: &[String], host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if host_pattern_matches(negated, host) {
                return false;
            }
        } else if host_pattern_matches(pattern, host) {
            matched = true;
        }
    }
    matched
}

fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    if pattern.starts_with("|1|") {
        // OpenSSH hashed hostnames require HMAC-SHA1 support. AgentSSH preserves
        // those entries but cannot match them without adding a crypto dependency.
        return false;
    }
    if pattern.contains(['*', '?']) {
        glob_match(pattern.as_bytes(), host.as_bytes())
    } else {
        pattern == host
    }
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }

    match pattern[0] {
        b'*' => glob_match(&pattern[1..], text) || (!text.is_empty() && glob_match(pattern, &text[1..])),
        b'?' => !text.is_empty() && glob_match(&pattern[1..], &text[1..]),
        byte => !text.is_empty() && byte == text[0] && glob_match(&pattern[1..], &text[1..]),
    }
}

fn append_known_host_atomically(
    path: &Path,
    existing: &str,
    host_patterns: &[String],
    server_public_key: &keys::PublicKey,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("known_hosts path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    set_secure_dir_permissions(parent)?;

    let mut updated = String::with_capacity(existing.len() + 256);
    updated.push_str(existing);
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&host_patterns.join(","));
    updated.push(' ');
    updated.push_str(&canonical_public_key(server_public_key)?);
    updated.push('\n');

    let tmp_path = temporary_known_hosts_path(path);
    fs::write(&tmp_path, updated).with_context(|| format!("writing {}", tmp_path.display()))?;
    set_secure_file_permissions(&tmp_path)?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    set_secure_file_permissions(path)?;
    Ok(())
}

fn temporary_known_hosts_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("known_hosts");
    path.with_file_name(format!(".{file_name}.agentssh.tmp"))
}

fn canonical_public_key(public_key: &keys::PublicKey) -> Result<String> {
    let encoded = public_key.to_openssh().context("encoding server public key")?;
    let mut parts = encoded.split_whitespace();
    let algorithm = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("encoded public key missing algorithm"))?;
    let key = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("encoded public key missing key data"))?;
    Ok(format!("{algorithm} {key}"))
}

fn set_secure_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

fn set_secure_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
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

    let handler = ClientHandler {
        host: host.clone(),
        port,
    };
    let mut handle = client::connect(Arc::new(config), (host.as_str(), port), handler)
        .await
        .with_context(|| format!("connecting to {}:{}", host, port))?;

    authenticate(&mut handle, &username, args)
        .await
        .with_context(|| "authentication failed".to_string())?;

    Ok((Arc::new(handle), host, port, username))
}

/// Try authentication methods in order: private_key → password → ssh-agent.
async fn authenticate(
    handle: &mut SharedConnectionHandle,
    username: &str,
    args: &ConnectArgs,
) -> Result<()> {
    // 1. Private key (--private-key: env var $ prefix, file path, or inline content)
    if let Some(content) = resolve_private_key(args) {
        // If it's a file path, load it; otherwise decode inline content
        let key = if let Some(path) = resolve_private_key_path(args) {
            keys::load_secret_key(&path, resolve_passphrase(args).as_deref())
                .with_context(|| format!("loading private key from {:?}", path))?
        } else {
            keys::decode_secret_key(&content, resolve_passphrase(args).as_deref())
                .context("decoding private key from content")?
        };
        let hash_alg = handle
            .best_supported_rsa_hash()
            .await
            .context("getting supported RSA hash")?;
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg.flatten());
        let ok = handle
            .authenticate_publickey(username, key_with_alg)
            .await
            .context("publickey auth")?;
        if ok.success() {
            return Ok(());
        }
    }

    // 2. Password (--password: env var $ prefix or literal)
    if let Some(pass) = resolve_password(args) {
        let ok = handle
            .authenticate_password(username, &pass)
            .await
            .context("password auth")?;
        if ok.success() {
            return Ok(());
        }
    }

    // 3. SSH agent (fallback when no explicit auth)
    if args.private_key.is_none() && args.password.is_none() {
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
/// Execute a command and buffer all output (used for JSON mode).
pub async fn exec_channel(
    channel: &mut Channel<client::Msg>,
    cmd: &str,
    timeout_ms: Option<u64>,
) -> Result<(String, String, i32)> {
    let timeout_ms = timeout_ms.unwrap_or(0);
    let has_timeout = timeout_ms > 0;
    let deadline = if has_timeout {
        Some(tokio::time::Instant::now() + Duration::from_millis(timeout_ms))
    } else {
        None
    };

    channel
        .exec(true, cmd)
        .await
        .with_context(|| format!("exec: {}", cmd))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code: i32 = -1;
    let mut saw_eof = false;

    loop {
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("exec timed out after {}ms", timeout_ms);
            }
            match tokio::time::timeout(remaining, channel.wait()).await {
                Ok(msg) => {
                    if !handle_exec_msg(msg, &mut stdout, &mut stderr, &mut exit_code, &mut saw_eof)
                    {
                        break;
                    }
                }
                Err(_) => bail!("exec timed out after {}ms", timeout_ms),
            }
        } else {
            match channel.wait().await {
                Some(msg) => {
                    if !handle_exec_msg(
                        Some(msg),
                        &mut stdout,
                        &mut stderr,
                        &mut exit_code,
                        &mut saw_eof,
                    ) {
                        break;
                    }
                }
                None => break,
            }
        }
    }

    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    ))
}

/// Execute a command and stream output to writers in real-time (used for text mode).
#[allow(dead_code)]
pub async fn exec_channel_streaming(
    channel: &mut Channel<client::Msg>,
    cmd: &str,
    timeout_ms: Option<u64>,
    stdout_writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    stderr_writer: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<i32> {
    use tokio::io::AsyncWriteExt;

    let timeout_ms = timeout_ms.unwrap_or(0);
    let has_timeout = timeout_ms > 0;
    let deadline = if has_timeout {
        Some(tokio::time::Instant::now() + Duration::from_millis(timeout_ms))
    } else {
        None
    };

    channel
        .exec(true, cmd)
        .await
        .with_context(|| format!("exec: {}", cmd))?;

    let mut exit_code: i32 = -1;
    let mut saw_eof = false;

    loop {
        let msg = if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                let _ = stdout_writer.flush().await;
                let _ = stderr_writer.flush().await;
                bail!("exec timed out after {}ms", timeout_ms);
            }
            match tokio::time::timeout(remaining, channel.wait()).await {
                Ok(m) => m,
                Err(_) => {
                    let _ = stdout_writer.flush().await;
                    let _ = stderr_writer.flush().await;
                    bail!("exec timed out after {}ms", timeout_ms);
                }
            }
        } else {
            channel.wait().await
        };

        match msg {
            Some(ChannelMsg::Data { data }) => {
                stdout_writer.write_all(&data).await?;
                stdout_writer.flush().await?;
            }
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                stderr_writer.write_all(&data).await?;
                stderr_writer.flush().await?;
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                exit_code = exit_status as i32;
                if saw_eof {
                    break;
                }
            }
            Some(ChannelMsg::Eof) => {
                saw_eof = true;
                if exit_code != -1 {
                    break;
                }
            }
            None => break,
            _ => {} // ignore other messages
        }
    }

    Ok(exit_code)
}

/// Handle a single exec channel message. Returns false when done.
pub fn handle_exec_msg(
    msg: Option<ChannelMsg>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    exit_code: &mut i32,
    saw_eof: &mut bool,
) -> bool {
    match msg {
        Some(ChannelMsg::Data { data }) => {
            stdout.extend_from_slice(&data);
            true
        }
        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
            stderr.extend_from_slice(&data);
            true
        }
        Some(ChannelMsg::ExitStatus { exit_status }) => {
            *exit_code = exit_status as i32;
            if *saw_eof {
                false
            } else {
                true
            }
        }
        Some(ChannelMsg::Eof) => {
            *saw_eof = true;
            if *exit_code != -1 {
                false
            } else {
                true
            }
        }
        None => false,
        Some(_) => true, // ignore other messages
    }
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
        private_key: override_args
            .private_key
            .clone()
            .or_else(|| base.private_key.clone()),
        passphrase: override_args
            .passphrase
            .clone()
            .or_else(|| base.passphrase.clone()),
        ready_timeout_ms: override_args.ready_timeout_ms.or(base.ready_timeout_ms),
        retry: override_args.retry.or(base.retry),
        retry_delay_ms: override_args.retry_delay_ms.or(base.retry_delay_ms),
    }
}

/// Resolve a value that may be an env var reference ($ prefix) or literal.
fn resolve_env(val: &str) -> Option<String> {
    if let Some(var_name) = val.strip_prefix('$') {
        std::env::var(var_name).ok()
    } else {
        Some(val.to_string())
    }
}

/// Get password: if starts with `$` read from env, otherwise literal.
fn resolve_password(args: &ConnectArgs) -> Option<String> {
    args.password.as_deref().and_then(resolve_env)
}

/// Get private key content: `$` prefix → env var, existing file → read file, otherwise inline key.
fn resolve_private_key(args: &ConnectArgs) -> Option<String> {
    let val = args.private_key.as_ref()?;
    if let Some(var_name) = val.strip_prefix('$') {
        return std::env::var(var_name).ok();
    }
    let expanded = util::expand_home(std::path::Path::new(val));
    if expanded.exists() {
        std::fs::read_to_string(&expanded).ok()
    } else {
        Some(val.clone())
    }
}

/// Get private key as file path (only if it's an actual file path, not env/inline).
fn resolve_private_key_path(args: &ConnectArgs) -> Option<std::path::PathBuf> {
    let val = args.private_key.as_ref()?;
    if val.starts_with('$') {
        return None; // env var, not a path
    }
    let expanded = util::expand_home(std::path::Path::new(val));
    if expanded.exists() {
        Some(expanded)
    } else {
        None // inline key content, not a path
    }
}

/// Get passphrase: if starts with `$` read from env, otherwise literal.
fn resolve_passphrase(args: &ConnectArgs) -> Option<String> {
    args.passphrase.as_deref().and_then(resolve_env)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti test-a";
    const SAMPLE_KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB9dG4kjRhQTtWTVzd2t27+t0DEHBPW7iOD23TUiYLio test-b";

    #[test]
    fn merge_connect_args_preserves_retry_fields() {
        let base = ConnectArgs {
            profile: None,
            host: Some("base-host".into()),
            port: Some(2222),
            username: Some("base-user".into()),
            password: None,
            private_key: None,
            passphrase: None,
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
            private_key: None,
            passphrase: None,
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

    #[test]
    fn known_hosts_accepts_matching_default_port_key() {
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key");
        let known_hosts = "example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti\n";

        let matched = known_hosts_has_key(known_hosts, &known_hosts_patterns("example.com", 22), &key)
            .expect("lookup succeeds");

        assert!(matched);
    }

    #[test]
    fn known_hosts_rejects_changed_key_for_same_host() {
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_B).expect("valid sample key");
        let known_hosts = "example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti\n";

        let error = known_hosts_has_key(known_hosts, &known_hosts_patterns("example.com", 22), &key)
            .expect_err("mismatched key must fail");

        assert!(error.to_string().contains("host key mismatch"));
    }

    #[test]
    fn known_hosts_uses_bracketed_host_for_non_default_port() {
        let patterns = known_hosts_patterns("example.com", 2222);
        assert_eq!(patterns, vec!["[example.com]:2222"]);
    }

    #[test]
    fn append_known_host_writes_new_line() {
        let temp_dir = std::env::temp_dir().join(format!("agentssh-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).expect("temp dir");
        let known_hosts_path = temp_dir.join("known_hosts");
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key");

        append_known_host_atomically(&known_hosts_path, "", &known_hosts_patterns("example.com", 22), &key)
            .expect("append succeeds");

        let written = fs::read_to_string(&known_hosts_path).expect("known_hosts readable");
        assert!(written.starts_with("example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti"));
        assert!(written.ends_with('\n'));

        let _ = fs::remove_file(&known_hosts_path);
        let _ = fs::remove_dir(&temp_dir);
    }

    #[test]
    fn known_hosts_returns_none_for_unknown_host() {
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key");
        let known_hosts = "";
        let result = known_hosts_has_key(known_hosts, &known_hosts_patterns("unknown.host", 22), &key)
            .expect("lookup succeeds");
        assert!(!result, "empty known_hosts should return false (not found)");
    }

    #[test]
    fn known_hosts_ignores_hashed_entries() {
        // |1|<base64salt>|<base64hash> is the OpenSSH hashed-hostname format.
        // AgentSSH preserves them but cannot match them, so the lookup must
        // report the host as unknown.
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key");
        let known_hosts =
            "|1|c2FsdA==|aGFzaA== ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti hashed-host\n";
        let result = known_hosts_has_key(
            known_hosts,
            &known_hosts_patterns("hashed-host", 22),
            &key,
        )
        .expect("lookup succeeds");
        assert!(!result, "hashed entries should not be matched");
    }

    #[test]
    fn known_hosts_skips_malformed_lines() {
        let key = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key");
        let known_hosts = "\
# comment line

only-host-no-algo
example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti ok
";
        // The valid line should still be found despite surrounding garbage.
        let result = known_hosts_has_key(
            known_hosts,
            &known_hosts_patterns("example.com", 22),
            &key,
        )
        .expect("lookup succeeds");
        assert!(result, "valid entry must be found among malformed lines");

        // Lookup for an unrelated host should return false (not panic).
        let result2 = known_hosts_has_key(
            known_hosts,
            &known_hosts_patterns("other.host", 22),
            &key,
        )
        .expect("lookup succeeds");
        assert!(!result2);
    }

    #[test]
    fn known_hosts_multiple_hosts_first_matches() {
        let key_a = keys::PublicKey::from_openssh(SAMPLE_KEY_A).expect("valid sample key A");
        let key_b = keys::PublicKey::from_openssh(SAMPLE_KEY_B).expect("valid sample key B");
        let known_hosts = "\
host-one ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti first
host-two ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIB9dG4kjRhQTtWTVzd2t27+t0DEHBPW7iOD23TUiYLio second
";
        // Looking up host-one should match key_a.
        let result = known_hosts_has_key(
            known_hosts,
            &known_hosts_patterns("host-one", 22),
            &key_a,
        )
        .expect("lookup succeeds");
        assert!(result, "first host must match its key");

        // Looking up host-two with key_a should fail (key mismatch).
        let err = known_hosts_has_key(
            known_hosts,
            &known_hosts_patterns("host-two", 22),
            &key_a,
        )
        .expect_err("wrong key for host-two must error");
        assert!(err.to_string().contains("host key mismatch"));
    }

    #[test]
    fn known_hosts_default_port_uses_plain_hostname() {
        let patterns = known_hosts_patterns("example.com", 22);
        assert_eq!(patterns, vec!["example.com"]);
        // Must NOT produce bracketed form.
        assert!(!patterns[0].contains('['), "default port should use plain hostname");
        assert!(!patterns[0].contains(':'), "default port should use plain hostname");
    }
}
