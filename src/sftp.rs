use crate::cli::{ListCommand, TransferCommand, WriteCommand};
use crate::connection::connect_with_info;
use crate::kernel::ServerState;
use crate::util::{emit_message, stat_json};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const EXEC_UPLOAD_BASE64_LIMIT: usize = 30_000;

pub fn daemon_upload(command: TransferCommand, state: &mut ServerState) -> Result<Value> {
    let session_id = command
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("--session-id is required for this operation. Get one with 'agentssh connect'."))?;
    let connection_id = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))?
        .connection_id
        .clone();

    let connection = state
        .connections
        .get_mut(&connection_id)
        .ok_or_else(|| anyhow!("connection '{}' not found", connection_id))?;
    connection.ssh.set_blocking(true);
    let result = upload_with_method(&connection.ssh, &command, Some(&session_id));
    connection.ssh.set_blocking(false);
    result
}

pub fn daemon_download(command: TransferCommand, state: &mut ServerState) -> Result<Value> {
    let session_id = command
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("--session-id is required for this operation. Get one with 'agentssh connect'."))?;
    let connection_id = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))?
        .connection_id
        .clone();

    let connection = state
        .connections
        .get_mut(&connection_id)
        .ok_or_else(|| anyhow!("connection '{}' not found", connection_id))?;
    connection.ssh.set_blocking(true);
    let result = download_with_method(&connection.ssh, &command, Some(&session_id));
    connection.ssh.set_blocking(false);
    result
}

pub fn daemon_ls(command: ListCommand, state: &mut ServerState) -> Result<Value> {
    let session_id = command
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("--session-id is required for this operation. Get one with 'agentssh connect'."))?;
    let connection_id = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))?
        .connection_id
        .clone();

    let connection = state
        .connections
        .get_mut(&connection_id)
        .ok_or_else(|| anyhow!("connection '{}' not found", connection_id))?;
    connection.ssh.set_blocking(true);
    let result = ls_with_method(&connection.ssh, &command, Some(&session_id));
    connection.ssh.set_blocking(false);
    result
}

pub fn run_upload_once(command: TransferCommand, json: bool) -> Result<()> {
    let (session, _, _, _) = connect_with_info(command.connect.clone())?;
    upload_with_method(&session, &command, None)?;
    emit_message(
        json,
        "uploaded",
        &format!("{} -> {} ({})", command.local.display(), command.remote.display(), command.method),
    )
}

pub fn run_download_once(command: TransferCommand, json: bool) -> Result<()> {
    let (session, _, _, _) = connect_with_info(command.connect.clone())?;
    download_with_method(&session, &command, None)?;
    emit_message(
        json,
        "downloaded",
        &format!("{} -> {} ({})", command.remote.display(), command.local.display(), command.method),
    )
}

pub fn run_ls_once(command: ListCommand, json: bool) -> Result<()> {
    let (session, _, _, _) = connect_with_info(command.connect.clone())?;
    let result = ls_with_method(&session, &command, None)?;
    if json {
        crate::util::print_json(&result)?;
        return Ok(());
    }

    if let Some(entries) = result.get("entries").and_then(|value| value.as_array()) {
        for entry in entries {
            let stat = &entry["stat"];
            let path = entry["path"].as_str().unwrap_or_default();
            println!(
                "{}\t{}\t{}",
                stat["perm"].as_u64().unwrap_or_default(),
                stat["size"].as_u64().unwrap_or_default(),
                path
            );
        }
        return Ok(());
    }

    if let Some(output) = result.get("output").and_then(|value| value.as_array()) {
        for line in output {
            if let Some(line) = line.as_str() {
                println!("{line}");
            }
        }
    }
    Ok(())
}

pub fn daemon_write(command: WriteCommand, state: &mut ServerState) -> Result<Value> {
    let session_id = command
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("--session-id is required for this operation. Get one with 'agentssh connect'."))?;
    let connection_id = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))?
        .connection_id
        .clone();

    let connection = state
        .connections
        .get_mut(&connection_id)
        .ok_or_else(|| anyhow!("connection '{}' not found", connection_id))?;
    connection.ssh.set_blocking(true);
    let result = write_with_method(&connection.ssh, &command, Some(&session_id));
    connection.ssh.set_blocking(false);
    result
}

pub fn run_write_once(command: WriteCommand, json: bool) -> Result<()> {
    let (session, _, _, _) = connect_with_info(command.connect.clone())?;
    write_with_method(&session, &command, None)?;
    emit_message(
        json,
        "written",
        &format!("{} bytes -> {} (sftp)", command.content.len(), command.remote.display()),
    )
}

fn upload_with_method(session: &ssh2::Session, command: &TransferCommand, session_id: Option<&str>) -> Result<Value> {
    match command.method.as_str() {
        "sftp" => sftp_upload(session, &command.local, &command.remote, session_id),
        "scp" => scp_upload(session, &command.local, &command.remote, session_id),
        "auto" => sftp_upload(session, &command.local, &command.remote, session_id).or_else(|sftp_error| {
            if !is_sftp_error(&sftp_error) {
                return Err(sftp_error);
            }

            scp_upload(session, &command.local, &command.remote, session_id).or_else(|scp_error| {
                exec_upload(session, &command.local, &command.remote, session_id).with_context(|| {
                    format!(
                        "auto upload fallback exhausted after SFTP error: {sftp_error}; SCP error: {scp_error}"
                    )
                })
            })
        }),
        other => Err(anyhow!("unsupported transfer method '{other}'")),
    }
}

fn download_with_method(session: &ssh2::Session, command: &TransferCommand, session_id: Option<&str>) -> Result<Value> {
    match command.method.as_str() {
        "sftp" => sftp_download(session, &command.remote, &command.local, session_id),
        "scp" => scp_download(session, &command.remote, &command.local, session_id),
        "auto" => sftp_download(session, &command.remote, &command.local, session_id).or_else(|sftp_error| {
            if !is_sftp_error(&sftp_error) {
                return Err(sftp_error);
            }

            scp_download(session, &command.remote, &command.local, session_id).or_else(|scp_error| {
                exec_download(session, &command.remote, &command.local, session_id).with_context(|| {
                    format!(
                        "auto download fallback exhausted after SFTP error: {sftp_error}; SCP error: {scp_error}"
                    )
                })
            })
        }),
        other => Err(anyhow!("unsupported transfer method '{other}'")),
    }
}

fn ls_with_method(session: &ssh2::Session, command: &ListCommand, session_id: Option<&str>) -> Result<Value> {
    match command.method.as_str() {
        "sftp" => sftp_ls(session, &command.remote, session_id),
        "scp" => exec_ls(session, &command.remote, session_id),
        "auto" => sftp_ls(session, &command.remote, session_id).or_else(|error| {
            if is_sftp_error(&error) {
                exec_ls(session, &command.remote, session_id)
            } else {
                Err(error)
            }
        }),
        other => Err(anyhow!("unsupported transfer method '{other}'")),
    }
}

fn write_with_method(session: &ssh2::Session, command: &WriteCommand, session_id: Option<&str>) -> Result<Value> {
    sftp_write(session, &command.remote, &command.content, session_id).or_else(|sftp_error| {
        if !is_sftp_error(&sftp_error) {
            return Err(sftp_error);
        }
        exec_write(session, &command.remote, &command.content, session_id)
            .with_context(|| format!("write fallback exhausted after SFTP error: {sftp_error}"))
    })
}

fn sftp_upload(session: &ssh2::Session, local: &Path, remote: &Path, session_id: Option<&str>) -> Result<Value> {
    let sftp = session
        .sftp()
        .context("SFTP is not available on this server. The SSH server may not have the sftp subsystem enabled.")?;
    sftp.create(remote)
        .with_context(|| format!("create remote file {}", remote.display()))?
        .write_all(&fs::read(local).with_context(|| format!("read local file {}", local.display()))?)?;
    Ok(transfer_result_json(local, remote, "sftp", session_id))
}

fn scp_upload(session: &ssh2::Session, local: &Path, remote: &Path, session_id: Option<&str>) -> Result<Value> {
    let data = fs::read(local).with_context(|| format!("read local file {}", local.display()))?;
    let size = data.len() as u64;
    let mut channel = session
        .scp_send(remote, 0o644, size, None)
        .with_context(|| format!("scp send remote file {}", remote.display()))?;
    channel.write_all(&data)?;
    channel.send_eof()?;
    channel.wait_eof()?;
    channel.wait_close()?;
    Ok(transfer_result_json(local, remote, "scp", session_id))
}

fn exec_upload(session: &ssh2::Session, local: &Path, remote: &Path, session_id: Option<&str>) -> Result<Value> {
    session.set_blocking(true);
    let data = fs::read(local).with_context(|| format!("read local file {}", local.display()))?;
    let encoded = B64.encode(data);
    let command = build_exec_upload_command(remote, &encoded)?;
    exec_powershell(session, &command).with_context(|| format!("exec upload remote file {}", remote.display()))?;
    Ok(transfer_result_json(local, remote, "exec", session_id))
}

fn sftp_write(session: &ssh2::Session, remote: &Path, content: &str, session_id: Option<&str>) -> Result<Value> {
    let sftp = session
        .sftp()
        .context("SFTP is not available on this server. The SSH server may not have the sftp subsystem enabled.")?;
    sftp.create(remote)
        .with_context(|| format!("create remote file {}", remote.display()))?
        .write_all(content.as_bytes())?;
    Ok(write_result_json(remote, content.len(), "sftp", session_id))
}

fn exec_write(session: &ssh2::Session, remote: &Path, content: &str, session_id: Option<&str>) -> Result<Value> {
    session.set_blocking(true);
    let encoded = B64.encode(content.as_bytes());
    if encoded.len() >= EXEC_UPLOAD_BASE64_LIMIT {
        return Err(anyhow!(
            "exec write payload too large: base64 length {} exceeds limit {}",
            encoded.len(),
            EXEC_UPLOAD_BASE64_LIMIT
        ));
    }
    let command = build_exec_upload_command(remote, &encoded)?;
    exec_powershell(session, &command)
        .with_context(|| format!("exec write remote file {}", remote.display()))?;
    Ok(write_result_json(remote, content.len(), "exec", session_id))
}

fn sftp_download(session: &ssh2::Session, remote: &Path, local: &Path, session_id: Option<&str>) -> Result<Value> {
    let sftp = session
        .sftp()
        .context("SFTP is not available on this server. The SSH server may not have the sftp subsystem enabled.")?;
    let mut remote_file = sftp
        .open(remote)
        .with_context(|| format!("open remote file {}", remote.display()))?;
    let mut data = Vec::new();
    remote_file.read_to_end(&mut data)?;
    write_local_file(local, &data)?;
    Ok(download_result_json(remote, local, "sftp", session_id))
}

fn scp_download(session: &ssh2::Session, remote: &Path, local: &Path, session_id: Option<&str>) -> Result<Value> {
    let (mut channel, _stat) = session
        .scp_recv(remote)
        .with_context(|| format!("scp recv remote file {}", remote.display()))?;
    let mut data = Vec::new();
    channel.read_to_end(&mut data)?;
    channel.send_eof()?;
    channel.wait_eof()?;
    channel.wait_close()?;
    write_local_file(local, &data)?;
    Ok(download_result_json(remote, local, "scp", session_id))
}

fn exec_download(session: &ssh2::Session, remote: &Path, local: &Path, session_id: Option<&str>) -> Result<Value> {
    session.set_blocking(true);
    let command = build_exec_download_command(remote);
    let output = exec_powershell(session, &command)
        .with_context(|| format!("exec download remote file {}", remote.display()))?;
    let data = decode_exec_download_output(&output)?;
    write_local_file(local, &data)?;
    Ok(download_result_json(remote, local, "exec", session_id))
}

fn sftp_ls(session: &ssh2::Session, remote: &Path, session_id: Option<&str>) -> Result<Value> {
    let sftp = session
        .sftp()
        .context("SFTP is not available on this server. The SSH server may not have the sftp subsystem enabled.")?;
    let entries = sftp
        .readdir(remote)
        .with_context(|| format!("read remote directory {}", remote.display()))?;
    let mut result = serde_json::json!({
        "remote": remote,
        "entries": entries
            .into_iter()
            .map(|(path, stat)| serde_json::json!({ "path": path, "stat": stat_json(stat) }))
            .collect::<Vec<_>>(),
        "method": "sftp"
    });
    if let Some(session_id) = session_id {
        result["session_id"] = serde_json::json!(session_id);
    }
    Ok(result)
}

fn exec_ls(session: &ssh2::Session, remote: &Path, session_id: Option<&str>) -> Result<Value> {
    let command = format!("ls -la {}", shell_words::quote(&remote.to_string_lossy()));
    let output = exec_remote_command(session, &command).context("remote ls command failed")?;
    let lines = output.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut result = serde_json::json!({
        "remote": remote,
        "output": lines,
        "method": "exec"
    });
    if let Some(session_id) = session_id {
        result["session_id"] = serde_json::json!(session_id);
    }
    Ok(result)
}

fn build_exec_upload_command(remote: &Path, encoded: &str) -> Result<String> {
    if encoded.len() >= EXEC_UPLOAD_BASE64_LIMIT {
        return Err(anyhow!(
            "exec upload payload too large: base64 length {} exceeds limit {}",
            encoded.len(),
            EXEC_UPLOAD_BASE64_LIMIT
        ));
    }

    Ok(format!(
        "[IO.File]::WriteAllBytes({}, [Convert]::FromBase64String('{}'))",
        powershell_single_quote(remote),
        encoded
    ))
}

fn build_exec_download_command(remote: &Path) -> String {
    format!(
        "[Convert]::ToBase64String([IO.File]::ReadAllBytes({}))",
        powershell_single_quote(remote)
    )
}

fn decode_exec_download_output(output: &str) -> Result<Vec<u8>> {
    let trimmed = output.trim();
    B64.decode(trimmed)
        .with_context(|| format!("decode base64 download payload from {} chars of output", trimmed.len()))
}

fn powershell_single_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn exec_powershell(session: &ssh2::Session, script: &str) -> Result<String> {
    let escaped = script.replace('"', "\"\"");
    let command = format!("powershell -Command \"{escaped}\"");
    exec_remote_command(session, &command)
}

fn exec_remote_command(session: &ssh2::Session, command: &str) -> Result<String> {
    session.set_blocking(true);
    let mut channel = session.channel_session()?;
    channel.exec(command)?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    let mut stderr = String::new();
    channel.stderr().read_to_string(&mut stderr)?;
    channel.wait_close()?;
    let exit_status = channel.exit_status()?;
    if exit_status != 0 {
        let detail = stderr.trim();
        if detail.is_empty() {
            return Err(anyhow!("remote command failed with exit status {exit_status}"));
        }
        return Err(anyhow!("remote command failed with exit status {exit_status}: {detail}"));
    }
    Ok(output)
}

fn transfer_result_json(local: &Path, remote: &Path, method: &str, session_id: Option<&str>) -> Value {
    let mut result = serde_json::json!({
        "local": local,
        "remote": remote,
        "method": method
    });
    if let Some(session_id) = session_id {
        result["session_id"] = serde_json::json!(session_id);
    }
    result
}

fn write_result_json(remote: &Path, bytes: usize, method: &str, session_id: Option<&str>) -> Value {
    let mut result = serde_json::json!({
        "remote": remote,
        "bytes": bytes,
        "method": method
    });
    if let Some(session_id) = session_id {
        result["session_id"] = serde_json::json!(session_id);
    }
    result
}

fn download_result_json(remote: &Path, local: &Path, method: &str, session_id: Option<&str>) -> Value {
    let mut result = serde_json::json!({
        "remote": remote,
        "local": local,
        "method": method
    });
    if let Some(session_id) = session_id {
        result["session_id"] = serde_json::json!(session_id);
    }
    result
}

fn write_local_file(local: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = local.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(local, data).with_context(|| format!("write local file {}", local.display()))
}

fn is_sftp_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("SFTP") || message.contains("sftp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_exec_upload_command_uses_single_quoted_path() {
        let command = build_exec_upload_command(Path::new(r"C:\temp\o'hare.txt"), "QUJD")
            .expect("command should build");

        assert_eq!(
            command,
            r"[IO.File]::WriteAllBytes('C:\temp\o''hare.txt', [Convert]::FromBase64String('QUJD'))"
        );
    }

    #[test]
    fn build_exec_upload_command_rejects_large_base64_payloads() {
        let payload = "A".repeat(30_000);
        let error = build_exec_upload_command(Path::new("C:/temp/file.txt"), &payload)
            .expect_err("payload should be rejected");

        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn build_exec_download_command_uses_single_quoted_path() {
        let command = build_exec_download_command(Path::new(r"C:\temp\o'hare.txt"));

        assert_eq!(
            command,
            r"[Convert]::ToBase64String([IO.File]::ReadAllBytes('C:\temp\o''hare.txt'))"
        );
    }

    #[test]
    fn decode_exec_download_output_trims_trailing_whitespace() {
        let data = decode_exec_download_output("SGVsbG8=\r\n").expect("base64 should decode");

        assert_eq!(data, b"Hello");
    }
}
