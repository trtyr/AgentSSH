use crate::cli::*;
use crate::connection;
use crate::kernel::ServerState;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------------------
// Daemon file operations (routed through daemon, use existing connection)
// ---------------------------------------------------------------------------

pub async fn daemon_upload(cmd: &TransferCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    do_upload(&handle, &local, &remote, &cmd.method).await
}

pub async fn daemon_download(cmd: &TransferCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    do_download(&handle, &local, &remote, &cmd.method).await
}

pub async fn daemon_ls(cmd: &ListCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let remote = cmd.remote.to_string_lossy();
    do_ls(&handle, &remote, &cmd.method).await
}

pub async fn daemon_write_file(cmd: &WriteCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let remote = cmd.remote.to_string_lossy();
    do_write(&handle, &remote, cmd.content.as_bytes(), &cmd.content.len(), &"auto").await
}

pub async fn daemon_read_file(cmd: &ReadFileCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let remote = cmd.remote.to_string_lossy();
    do_read_file(&handle, &remote).await
}

pub async fn daemon_delete(cmd: &DeleteFileCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let remote = cmd.remote.to_string_lossy();
    let recursive = cmd.recursive;
    do_delete(&handle, &remote, recursive).await
}

pub async fn daemon_edit(cmd: &EditFileCommand, state: &mut ServerState) -> Result<Value> {
    let handle = get_handle(cmd.session_id.as_deref(), state)?;
    let remote = cmd.remote.to_string_lossy();
    do_edit(&handle, &remote, &cmd.find, &cmd.replace).await
}

// ---------------------------------------------------------------------------
// One-shot operations (no daemon)
// ---------------------------------------------------------------------------

pub async fn upload_once(cmd: &TransferCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    let result = do_upload(&handle, &local, &remote, &cmd.method).await?;
    crate::util::print_json(&result)?;
    Ok(())
}

pub async fn download_once(cmd: &TransferCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    let result = do_download(&handle, &local, &remote, &cmd.method).await?;
    crate::util::print_json(&result)?;
    Ok(())
}

pub async fn ls_once(cmd: &ListCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let remote = cmd.remote.to_string_lossy();
    let result = do_ls(&handle, &remote, &cmd.method).await?;
    crate::util::print_json(&result)?;
    Ok(())
}

pub async fn write_once(cmd: &WriteCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let remote = cmd.remote.to_string_lossy();
    let result = do_write(&handle, &remote, cmd.content.as_bytes(), &cmd.content.len(), &"auto").await?;
    crate::util::print_json(&result)?;
    Ok(())
}

pub async fn read_once(cmd: &ReadFileCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let remote = cmd.remote.to_string_lossy();
    let result = do_read_file(&handle, &remote).await?;
    print!("{}", result.get("content").and_then(|v| v.as_str()).unwrap_or(""));
    Ok(())
}

pub async fn delete_once(cmd: &DeleteFileCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let remote = cmd.remote.to_string_lossy();
    let result = do_delete(&handle, &remote, cmd.recursive).await?;
    crate::util::print_json(&result)?;
    Ok(())
}

pub async fn edit_once(cmd: &EditFileCommand, _json: bool) -> Result<()> {
    let args = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, _, _, _) = connection::connect_with_info(&args).await?;
    let remote = cmd.remote.to_string_lossy();
    let result = do_edit(&handle, &remote, &cmd.find, &cmd.replace).await?;
    crate::util::print_json(&result)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Core implementation
// ---------------------------------------------------------------------------

async fn do_upload(
    handle: &connection::SharedConnectionHandle,
    local: &str,
    remote: &str,
    method: &str,
) -> Result<Value> {
    let local_path = crate::util::expand_home(Path::new(local));
    let data = std::fs::read(&local_path)
        .with_context(|| format!("reading {}", local))?;
    let file_name = local_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());

    match method {
        "sftp" | "auto" => {
            match sftp_write(handle, remote, &data).await {
                Ok(v) => return Ok(v),
                Err(e) if method == "auto" => {
                    exec_upload(handle, remote, &data, &file_name).await
                }
                Err(e) => Err(e),
            }
        }
        "exec" => exec_upload(handle, remote, &data, &file_name).await,
        _ => bail!("unsupported upload method: {}", method),
    }
}

async fn do_download(
    handle: &connection::SharedConnectionHandle,
    local: &str,
    remote: &str,
    method: &str,
) -> Result<Value> {
    match method {
        "sftp" | "auto" => {
            match sftp_read(handle, remote).await {
                Ok(data) => {
                    let local_path = crate::util::expand_home(Path::new(local));
                    if let Some(parent) = local_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&local_path, &data)?;
                    Ok(json!({"ok": true, "local": local, "remote": remote, "bytes": data.len(), "method": "sftp"}))
                }
                Err(e) if method == "auto" => exec_download(handle, local, remote).await,
                Err(e) => Err(e),
            }
        }
        "exec" => exec_download(handle, local, remote).await,
        _ => bail!("unsupported download method: {}", method),
    }
}

async fn do_ls(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    method: &str,
) -> Result<Value> {
    match method {
        "sftp" | "auto" => {
            match sftp_ls(handle, remote).await {
                Ok(v) => Ok(v),
                Err(e) if method == "auto" => exec_ls(handle, remote).await,
                Err(e) => Err(e),
            }
        }
        "exec" => exec_ls(handle, remote).await,
        _ => bail!("unsupported ls method: {}", method),
    }
}

async fn do_write(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    content: &[u8],
    _size: &usize,
    method: &str,
) -> Result<Value> {
    match method {
        "sftp" | "auto" => {
            match sftp_write(handle, remote, content).await {
                Ok(v) => Ok(v),
                Err(e) if method == "auto" => {
                    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, content);
                    exec_write(handle, remote, &encoded).await
                }
                Err(e) => Err(e),
            }
        }
        "exec" => {
            let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, content);
            exec_write(handle, remote, &encoded).await
        }
        _ => bail!("unsupported write method: {}", method),
    }
}

async fn do_read_file(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Value> {
    let data = sftp_read(handle, remote).await?;
    let content = String::from_utf8_lossy(&data).into_owned();
    Ok(json!({"ok": true, "path": remote, "content": content, "bytes": data.len()}))
}

async fn do_delete(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    _recursive: bool,
) -> Result<Value> {
    match sftp_delete(handle, remote).await {
        Ok(v) => Ok(v),
        Err(_) => exec_delete(handle, remote).await,
    }
}

async fn do_edit(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    find: &str,
    replace: &str,
) -> Result<Value> {
    let data = sftp_read(handle, remote).await?;
    let content = String::from_utf8_lossy(&data).into_owned();
    let edited = content.replace(find, replace);
    sftp_write(handle, remote, edited.as_bytes()).await?;
    Ok(json!({"ok": true, "path": remote, "size": edited.len()}))
}

// ---------------------------------------------------------------------------
// SFTP via russh-sftp
// ---------------------------------------------------------------------------

async fn open_sftp(
    handle: &connection::SharedConnectionHandle,
) -> Result<russh_sftp::client::SftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .context("opening SFTP channel")?;

    channel
        .request_subsystem(true, "sftp")
        .await
        .context("requesting SFTP subsystem")?;

    let sftp = russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .context("initializing SFTP session")?;

    Ok(sftp)
}

async fn sftp_write(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    data: &[u8],
) -> Result<Value> {
    let sftp = open_sftp(handle).await?;

    if let Some(parent) = Path::new(remote).parent() {
        let parent_str = parent.to_string_lossy();
        if !parent_str.is_empty() && parent_str != "/" {
            let _ = sftp.create_dir(parent_str.to_string()).await;
        }
    }

    use russh_sftp::protocol::OpenFlags;
    let mut file = sftp
        .open_with_flags(
            remote,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .with_context(|| format!("opening remote file for write: {}", remote))?;

    file.write_all(data).await?;
    file.shutdown().await?;

    Ok(json!({"ok": true, "remote": remote, "bytes": data.len(), "method": "sftp"}))
}

async fn sftp_read(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Vec<u8>> {
    let sftp = open_sftp(handle).await?;
    let data = sftp.read(remote).await?;
    Ok(data)
}

async fn sftp_ls(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Value> {
    let sftp = open_sftp(handle).await?;
    let entries = sftp.read_dir(remote).await?;

    let items: Vec<Value> = entries
        .map(|entry| {
            let metadata = entry.metadata();
            json!({
                "name": entry.file_name(),
                "path": entry.path(),
                "is_dir": metadata.is_dir(),
                "size": metadata.len(),
                "modified": metadata.mtime.map(|t| t.to_string()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(json!({"ok": true, "path": remote, "entries": items, "method": "sftp"}))
}

async fn sftp_delete(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Value> {
    let sftp = open_sftp(handle).await?;
    match sftp.remove_file(remote).await {
        Ok(()) => Ok(json!({"ok": true, "deleted": remote, "method": "sftp"})),
        Err(_) => {
            sftp.remove_dir(remote).await?;
            Ok(json!({"ok": true, "deleted": remote, "method": "sftp"}))
        }
    }
}

// ---------------------------------------------------------------------------
// Exec-based fallback
// ---------------------------------------------------------------------------

async fn exec_remote_command(
    handle: &connection::SharedConnectionHandle,
    cmd: &str,
) -> Result<(String, String, i32)> {
    let mut channel = handle
        .channel_open_session()
        .await
        .context("opening exec channel")?;
    connection::exec_channel(&mut channel, cmd, None).await
}

async fn exec_upload(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    data: &[u8],
    _file_name: &str,
) -> Result<Value> {
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
    let chunk_size = 30000;
    if encoded.len() <= chunk_size {
        let cmd = format!("echo '{}' | base64 -d > '{}'", encoded, remote);
        let (_, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
        if exit_code != 0 {
            bail!("exec upload failed: {}", stderr);
        }
    } else {
        let first = &encoded[..chunk_size];
        let cmd = format!("echo '{}' | base64 -d > '{}'", first, remote);
        let (_, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
        if exit_code != 0 { bail!("exec upload failed: {}", stderr); }

        let mut pos = chunk_size;
        while pos < encoded.len() {
            let end = (pos + chunk_size).min(encoded.len());
            let chunk = &encoded[pos..end];
            let cmd = format!("echo '{}' | base64 -d >> '{}'", chunk, remote);
            let (_, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
            if exit_code != 0 { bail!("exec upload chunk failed: {}", stderr); }
            pos = end;
        }
    }
    Ok(json!({"ok": true, "remote": remote, "bytes": data.len(), "method": "exec"}))
}

async fn exec_download(
    handle: &connection::SharedConnectionHandle,
    local: &str,
    remote: &str,
) -> Result<Value> {
    let cmd = format!("base64 '{}'", remote);
    let (stdout, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
    if exit_code != 0 { bail!("exec download failed: {}", stderr); }

    let data = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, stdout.trim())?;
    let local_path = crate::util::expand_home(Path::new(local));
    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&local_path, &data)?;
    Ok(json!({"ok": true, "local": local, "remote": remote, "bytes": data.len(), "method": "exec"}))
}

async fn exec_ls(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Value> {
    let cmd = format!("ls -la '{}'", remote);
    let (stdout, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
    if exit_code != 0 { bail!("exec ls failed: {}", stderr); }

    let entries: Vec<Value> = stdout
        .lines()
        .skip(1)
        .filter(|line| !line.is_empty())
        .filter_map(|line| parse_ls_line(line))
        .collect();

    Ok(json!({"ok": true, "path": remote, "entries": entries, "method": "exec"}))
}

async fn exec_write(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
    base64_content: &str,
) -> Result<Value> {
    let cmd = format!("echo '{}' | base64 -d > '{}'", base64_content, remote);
    let (_, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
    if exit_code != 0 { bail!("exec write failed: {}", stderr); }
    Ok(json!({"ok": true, "path": remote, "method": "exec"}))
}

async fn exec_delete(
    handle: &connection::SharedConnectionHandle,
    remote: &str,
) -> Result<Value> {
    let cmd = format!("rm -rf '{}'", remote);
    let (_, stderr, exit_code) = exec_remote_command(handle, &cmd).await?;
    if exit_code != 0 { bail!("exec delete failed: {}", stderr); }
    Ok(json!({"ok": true, "deleted": remote, "method": "exec"}))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_handle(
    session_id: Option<&str>,
    state: &ServerState,
) -> Result<std::sync::Arc<connection::SharedConnectionHandle>> {
    let sid = session_id.ok_or_else(|| anyhow::anyhow!("session-id required for daemon file operations"))?;
    let session = state
        .sessions
        .get(sid)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", sid))?;
    let conn = state
        .connections
        .get(&session.connection_id)
        .ok_or_else(|| anyhow::anyhow!("connection {} not found", session.connection_id))?;
    Ok(conn.handle.clone())
}

fn parse_ls_line(line: &str) -> Option<Value> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 { return None; }
    let perms = parts[0];
    let size: u64 = parts[4].parse().ok()?;
    let name = parts[8..].join(" ");
    let is_dir = perms.starts_with('d');
    Some(json!({"name": name, "permissions": perms, "size": size, "is_dir": is_dir}))
}

/// Resolve ConnectArgs by merging with profile if --profile is specified.
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ls_line() {
        let line = "-rw-r--r-- 1 root root 1234 Jan 01 12:00 test.txt";
        let entry = parse_ls_line(line).unwrap();
        assert_eq!(entry["name"], "test.txt");
        assert_eq!(entry["size"], 1234);
        assert_eq!(entry["is_dir"], false);
    }

    #[test]
    fn test_parse_ls_line_directory() {
        let line = "drwxr-xr-x 2 root root 4096 Jan 01 12:00 mydir";
        let entry = parse_ls_line(line).unwrap();
        assert_eq!(entry["name"], "mydir");
        assert_eq!(entry["is_dir"], true);
    }

    #[test]
    fn test_base64_roundtrip() {
        let data = b"hello world";
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &encoded).unwrap();
        assert_eq!(decoded, data);
    }
}
