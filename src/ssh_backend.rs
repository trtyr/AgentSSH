use crate::cli::ConnectArgs;
use crate::util;
use anyhow::Result;
use serde_json::json;

/// One-shot exec: connect, run command, return output.
pub fn run_exec(command: crate::cli::ExecCommand, json_output: bool) -> Result<()> {
    let args = crate::connection::resolve_connect_args(&command.connect)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (handle, _, _, _) = crate::connection::connect_with_info(&args).await?;

        let mut channel = handle
            .channel_open_session()
            .await?;

        let cmd = command.command.join(" ");
        let (stdout, stderr, exit_code) =
            crate::connection::exec_channel(&mut channel, &cmd, Some(command.timeout)).await?;

        let result = json!({
            "ok": true,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
        });

        if json_output {
            util::print_json(&result)?;
        } else {
            if !stdout.is_empty() {
                print!("{}", stdout);
            }
            if !stderr.is_empty() {
                eprint!("{}", stderr);
            }
        }

        if exit_code != 0 {
            std::process::exit(exit_code);
        }

        Ok(())
    })
}

/// One-shot shell: connect, open PTY, relay stdin/stdout.
pub fn run_shell(connect: ConnectArgs, _json: bool) -> Result<()> {
    let args = crate::connection::resolve_connect_args(&connect)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let (handle, _, _, _) = crate::connection::connect_with_info(&args).await?;

        let cols = util::DEFAULT_COLS;
        let rows = util::DEFAULT_ROWS;

        let mut channel = crate::session::open_pty(&handle, cols, rows).await?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use russh::ChannelMsg;

        let mut stdin = tokio::io::stdin();
        let mut stdin_buf = [0u8; 4096];

        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            tokio::io::stdout().write_all(&data).await?;
                            tokio::io::stdout().flush().await?;
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            tokio::io::stderr().write_all(&data).await?;
                        }
                        Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::Eof) | None => {
                            break;
                        }
                        _ => {}
                    }
                }
                n = stdin.read(&mut stdin_buf) => {
                    match n {
                        Ok(0) => {
                            let _ = channel.eof().await;
                            break;
                        }
                        Ok(n) => {
                            channel.data(&stdin_buf[..n]).await?;
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Sync wrappers for one-shot file operations (called from cli.rs)
// ---------------------------------------------------------------------------

pub fn run_upload_once(cmd: crate::cli::TransferCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::upload_once(&cmd, json).await })
}

pub fn run_download_once(cmd: crate::cli::TransferCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::download_once(&cmd, json).await })
}

pub fn run_ls_once(cmd: crate::cli::ListCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::ls_once(&cmd, json).await })
}

pub fn run_write_once(cmd: crate::cli::WriteCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::write_once(&cmd, json).await })
}

pub fn run_read_once(cmd: crate::cli::ReadFileCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::read_once(&cmd, json).await })
}

pub fn run_delete_once(cmd: crate::cli::DeleteFileCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::delete_once(&cmd, json).await })
}

pub fn run_edit_once(cmd: crate::cli::EditFileCommand, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { crate::sftp::edit_once(&cmd, json).await })
}
