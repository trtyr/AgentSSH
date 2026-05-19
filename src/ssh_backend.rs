use crate::cli::{ConnectArgs, ExecCommand};
use crate::connection::connect_with_info;
use crate::connection::exec_channel;
use crate::util::print_json;
use anyhow::{Result, bail};
use std::io::{self, Read, Write};

#[allow(unused_imports)]
pub use crate::connection::{
    SessionIdentity, SharedConnection, authenticate, get_connection_mut, key_content,
    merge_connect_args, next_connection_id, passphrase, password,
};
#[allow(unused_imports)]
pub use crate::interactive::{expect_pairs, output_matches};
#[allow(unused_imports)]
pub use crate::proxy::{
    ProxyMode, ProxyState, ProxyStatus, daemon_proxy_close, daemon_proxy_create, daemon_proxy_list,
    daemon_proxy_ping,
};
#[allow(unused_imports)]
pub use crate::session::{
    RemoteSession, SessionMetadataForTesting, attempt_reconnect, create_session, daemon_connect,
    daemon_exec, daemon_output_response, daemon_ping, daemon_read, daemon_resize, daemon_send,
    daemon_signal, daemon_spawn, daemon_status, ensure_session_connected, get_session_mut,
    normalize_input, page_output, refresh_session_state, session_metadata, session_summary,
    shared_with_ids,
};
#[allow(unused_imports)]
pub use crate::sftp::{
    daemon_file_delete, daemon_file_edit, daemon_file_read, daemon_download, daemon_ls,
    daemon_upload, daemon_write, run_delete_once, run_download_once, run_edit_once, run_ls_once,
    run_read_once, run_upload_once, run_write_once,
};

pub fn run_exec(command: ExecCommand, json: bool) -> Result<()> {
    let (session, _, _, _) = connect_with_info(command.connect)?;
    let command_line = command.command.join(" ");
    let mut channel = session.channel_session()?;
    let (stdout, stderr, exit_status) =
        exec_channel(&session, &mut channel, &command_line, command.timeout)?;
    if json {
        print_json(
            &serde_json::json!({ "exit_status": exit_status, "stdout": stdout, "stderr": stderr }),
        )?;
        return Ok(());
    }
    print!("{stdout}");
    eprint!("{stderr}");
    if exit_status != 0 {
        bail!("remote command exited with status {exit_status}");
    }
    Ok(())
}

pub fn run_shell(connect_args: ConnectArgs, json: bool) -> Result<()> {
    if json {
        bail!("interactive shell does not support --json");
    }
    let (session, _, _, _) = connect_with_info(connect_args)?;
    let mut channel = session.channel_session()?;
    channel.request_pty("xterm-256color", None, Some((120, 40, 0, 0)))?;
    channel.shell()?;
    println!("Connected. Type commands, then Ctrl-D to close stdin.");
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    channel.write_all(input.as_bytes())?;
    channel.send_eof()?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    print!("{output}");
    channel.wait_close()?;
    Ok(())
}
