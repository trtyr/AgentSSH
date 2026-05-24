use crate::cli::{ConnectArgs, ConnectCommand, ReadCommand, ResizeCommand, SessionExecCommand, SessionInputCommand, SignalCommand, SpawnCommand};
use crate::connection::{SessionIdentity, SharedConnection, connect_with_info, exec_channel, get_connection_mut, next_connection_id};
use crate::interactive::output_matches;
use crate::kernel::ServerState;
use crate::util::{MAX_BUFFER, log_daemon, now_ms, sleep_ms, strip_ansi};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use ssh2::Channel;
use std::io::{Read, Write};

const DEFAULT_WAIT_FOR_EXIT_TIMEOUT_MS: u64 = 60_000;

pub struct RemoteSession {
    pub id: String,
    pub connection_id: String,
    pub shell: Channel,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) output: Vec<u8>,
    pub(crate) cursor: usize,
    pub(crate) created_at: u128,
    pub(crate) updated_at: u128,
    pub(crate) status: String,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    pub(crate) exit_status: Option<i32>,
    pub(crate) exit_signal: Option<String>,
    pub(crate) connect_args: ConnectArgs,
    pub(crate) reconnect: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMetadataForTesting {
    pub id: String,
    pub connection_id: String,
}

pub fn create_session(
    state: &mut ServerState,
    connection_id: &str,
    identity: SessionIdentity,
    cols: u32,
    rows: u32,
    wait_ms: u64,
    limit: usize,
    connect_args: ConnectArgs,
    reconnect: bool,
) -> Result<Value> {
    let shell = {
        let connection = get_connection_mut(state, connection_id)?;
        connection.ssh.set_blocking(true);
        let mut shell = connection.ssh.channel_session()?;
        shell.request_pty("xterm-256color", None, Some((cols, rows, 0, 0)))?;
        shell.shell()?;
        connection.ssh.set_blocking(false);
        connection.refcount += 1;
        shell
    };

    state.next_id += 1;
    let id = format!("s{}", state.next_id);
    let created_at = now_ms();
    let mut session = RemoteSession {
        id: id.clone(),
        connection_id: connection_id.to_string(),
        shell,
        host: identity.host,
        port: identity.port,
        username: identity.username,
        output: Vec::new(),
        cursor: 0,
        created_at,
        updated_at: created_at,
        status: "running".to_string(),
        cols,
        rows,
        exit_status: None,
        exit_signal: None,
        connect_args,
        reconnect,
    };

    sleep_ms(wait_ms);
    refresh_session_state(&mut session);
    let page = page_output(&mut session, None, limit, true);
    state.sessions.insert(id.clone(), session);
    let all_sessions = state.sessions.values().map(session_metadata).collect::<Vec<_>>();
    let summary = session_summary(
        state.sessions.get(&id).expect("new session should be available"),
        &all_sessions,
    );
    Ok(serde_json::json!({
        "session_id": id,
        "session": summary,
        "output": page,
        "host": state.sessions.get(&id).expect("new session should be available").host.clone(),
        "port": state.sessions.get(&id).expect("new session should be available").port
    }))
}

pub fn ensure_session_connected(session: &RemoteSession) -> Result<()> {
    if session.status == "running" {
        return Ok(());
    }
    bail!("session '{}' is {}", session.id, session.status)
}

pub fn refresh_session_state(session: &mut RemoteSession) {
    let mut buf = [0_u8; 8192];
    let mut saw_error = false;
    loop {
        match session.shell.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                session.output.extend_from_slice(&buf[..n]);
                if session.output.len() > MAX_BUFFER {
                    let drain = session.output.len() - MAX_BUFFER;
                    session.output.drain(..drain);
                    session.cursor = session.cursor.saturating_sub(drain);
                }
                session.updated_at = now_ms();
            }
            Err(error) => {
                if error.kind() != std::io::ErrorKind::WouldBlock
                    && error.kind() != std::io::ErrorKind::TimedOut
                {
                    saw_error = true;
                }
                break;
            }
        }
    }
    let mut stderr = session.shell.stderr();
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                session.output.extend_from_slice(&buf[..n]);
                if session.output.len() > MAX_BUFFER {
                    let drain = session.output.len() - MAX_BUFFER;
                    session.output.drain(..drain);
                    session.cursor = session.cursor.saturating_sub(drain);
                }
                session.updated_at = now_ms();
            }
            Err(error) => {
                if error.kind() != std::io::ErrorKind::WouldBlock
                    && error.kind() != std::io::ErrorKind::TimedOut
                {
                    saw_error = true;
                }
                break;
            }
        }
    }
    if session.shell.eof() {
        session.status = "closed".to_string();
        if session.exit_status.is_none() {
            session.exit_status = session.shell.exit_status().ok();
        }
        if session.exit_signal.is_none() {
            session.exit_signal = session
                .shell
                .exit_signal()
                .ok()
                .and_then(|signal| signal.exit_signal);
        }
    } else if saw_error {
        session.status = "disconnected".to_string();
    }
}

pub fn page_output(
    session: &mut RemoteSession,
    offset: Option<usize>,
    limit: usize,
    strip_ansi_output: bool,
) -> Value {
    let start = offset.unwrap_or(session.cursor).min(session.output.len());
    let end = (start + limit).min(session.output.len());
    if offset.is_none() {
        session.cursor = end;
    }
    render_output_page(&session.output, start, end, strip_ansi_output)
}

pub fn session_summary(session: &RemoteSession, all_sessions: &[SessionMetadataForTesting]) -> Value {
    serde_json::json!({
        "id": session.id,
        "connection_id": session.connection_id,
        "shared_with": shared_with_ids(&session.id, &session.connection_id, all_sessions),
        "host": session.host,
        "port": session.port,
        "username": session.username,
        "status": session.status,
        "output_bytes": session.output.len(),
        "cursor": session.cursor,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "cols": session.cols,
        "rows": session.rows,
    })
}

pub fn session_metadata(session: &RemoteSession) -> SessionMetadataForTesting {
    SessionMetadataForTesting {
        id: session.id.clone(),
        connection_id: session.connection_id.clone(),
    }
}

pub fn get_session_mut<'a>(
    state: &'a mut ServerState,
    session_id: &str,
) -> Result<&'a mut RemoteSession> {
    state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))
}

/// Temporarily set the SSH connection to blocking mode for reliable I/O.
/// Returns the connection_id so the caller can restore non-blocking mode later.
fn begin_blocking_io(state: &mut ServerState, connection_id: &str, timeout_ms: u32) {
    if let Some(conn) = state.connections.get_mut(connection_id) {
        conn.ssh.set_timeout(timeout_ms);
        conn.ssh.set_blocking(true);
    }
}

/// Restore the SSH connection to non-blocking mode after blocking I/O.
fn end_blocking_io(state: &mut ServerState, connection_id: &str) {
    if let Some(conn) = state.connections.get_mut(connection_id) {
        conn.ssh.set_blocking(false);
    }
}

pub fn normalize_input(input: &str, crlf: bool) -> String {
    if crlf {
        input.replace('\n', "\r\n")
    } else {
        input.to_string()
    }
}

pub fn attempt_reconnect(session: &mut RemoteSession) -> Option<(ssh2::Session, Channel)> {
    if !session.reconnect {
        return None;
    }
    let old_id = session.id.clone();
    let _ = log_daemon(&format!("attempting reconnect for session {}", old_id));

    let (new_ssh, _host, _port, _username) = match connect_with_info(session.connect_args.clone()) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = log_daemon(&format!("reconnect failed for session {}: {e:#}", old_id));
            return None;
        }
    };

    new_ssh.set_blocking(true);
    let new_shell = match new_ssh.channel_session() {
        Ok(mut shell) => {
            if shell.request_pty("xterm-256color", None, Some((session.cols, session.rows, 0, 0))).is_err()
                || shell.shell().is_err()
            {
                let _ = log_daemon(&format!("reconnect PTY failed for session {}", old_id));
                return None;
            }
            shell
        }
        Err(e) => {
            let _ = log_daemon(&format!("reconnect PTY failed for session {}: {e:#}", old_id));
            return None;
        }
    };

    let mut old_shell = std::mem::replace(&mut session.shell, new_shell);
    let _ = old_shell.close();
    let _ = old_shell.wait_close();
    new_ssh.set_blocking(false);

    session.status = "running".to_string();
    session.updated_at = now_ms();
    let notice = format!("\r\n[AgentSSH] session {} reconnected\r\n", old_id);
    session.output.extend_from_slice(notice.as_bytes());
    session.cursor = session.output.len();

    let _ = log_daemon(&format!("session {} reconnected", old_id));
    Some((new_ssh, old_shell))
}

pub fn daemon_connect(command: ConnectCommand, state: &mut ServerState) -> Result<Value> {
    let connect_args = command.connect.clone();
    let (ssh, host, port, username) = connect_with_info(command.connect)?;
    let connection_id = next_connection_id(state);
    let identity = SessionIdentity { host, port, username };
    ssh.set_blocking(false);
    state
        .connections
        .insert(connection_id.clone(), SharedConnection { ssh, refcount: 0 });
    create_session(
        state,
        &connection_id,
        identity,
        command.cols,
        command.rows,
        command.wait_ms,
        command.limit,
        connect_args,
        command.reconnect,
    )
}

pub fn daemon_spawn(command: SpawnCommand, state: &mut ServerState) -> Result<Value> {
    let source = state
        .sessions
        .get(&command.from)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", command.from))?;

    let identity = SessionIdentity {
        host: source.host.clone(),
        port: source.port,
        username: source.username.clone(),
    };
    let connection_id = source.connection_id.clone();
    let connect_args = source.connect_args.clone();
    let reconnect = source.reconnect;
    create_session(
        state,
        &connection_id,
        identity,
        command.cols,
        command.rows,
        command.wait_ms,
        command.limit,
        connect_args,
        reconnect,
    )
}

pub fn daemon_exec(command: SessionExecCommand, state: &mut ServerState) -> Result<Value> {
    let command_line = command.command.join(" ");

    // Detach mode: write command to the PTY, drain output, return immediately
    if command.detach {
        let session = get_session_mut(state, &command.session_id)?;
        ensure_session_connected(session)?;
        session.shell.write_all(format!("{}\n", command_line).as_bytes())?;
        refresh_session_state(session);
        return Ok(serde_json::json!({
            "session_id": command.session_id,
            "command": command_line,
            "detached": true,
            "status": "dispatched"
        }));
    }

    // Get the session's stored ConnectArgs to open a dedicated connection.
    // The session's SSH transport has an active PTY channel (from `connect`)
    // that conflicts with opening new channels via libssh2. A fresh connection
    // eliminates the channel conflict at the cost of one extra TCP handshake
    // (~500ms), which is negligible compared to command execution time.
    let connect_args = {
        let session = get_session_mut(state, &command.session_id)?;
        ensure_session_connected(session)?;
        session.connect_args.clone()
    };

    let (ssh, _host, _port, _username) = connect_with_info(connect_args)?;
    let mut channel = ssh.channel_session()?;
    let (stdout, stderr, exit_status) =
        exec_channel(&ssh, &mut channel, &command_line, command.timeout)?;

    Ok(serde_json::json!({
        "session_id": command.session_id,
        "exit_status": exit_status,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

pub fn daemon_send(command: SessionInputCommand, state: &mut ServerState) -> Result<Value> {
    let expect_pairs = command.expect_pairs()?;

    // Get connection_id from session (separate borrow scope)
    let connection_id = {
        let session = get_session_mut(state, &command.session_id)?;
        ensure_session_connected(session)?;
        session.connection_id.clone()
    };

    // Switch to blocking mode for reliable PTY I/O (timeout = wait_ms to prevent hangs)
    let blocking_timeout = u32::try_from(command.wait_ms.max(2000)).unwrap_or(5000);
    begin_blocking_io(state, &connection_id, blocking_timeout);

    let result = daemon_send_inner(command, expect_pairs, state, &connection_id);

    // Always restore non-blocking mode
    end_blocking_io(state, &connection_id);

    result
}

fn daemon_send_inner(
    command: SessionInputCommand,
    expect_pairs: Vec<crate::cli::ExpectRespondPair>,
    state: &mut ServerState,
    _connection_id: &str,
) -> Result<Value> {
    let session = get_session_mut(state, &command.session_id)?;

    let input = normalize_input(&command.input, command.crlf);
    session.shell.write_all(input.as_bytes())?;
    sleep_ms(command.wait_ms);
    refresh_session_state(session);

    let mut auto_responses = Vec::new();
    for pair in expect_pairs {
        if output_matches(session, &pair.expect)? {
            session.shell.write_all(pair.respond.as_bytes())?;
            sleep_ms(command.wait_ms);
            refresh_session_state(session);
            auto_responses.push(serde_json::json!({
                "expect": pair.expect,
                "responded": true,
            }));
        } else {
            auto_responses.push(serde_json::json!({
                "expect": pair.expect,
                "responded": false,
            }));
        }
    }

    // wait-for-exit: mandatory 60s default timeout to prevent daemon deadlock
    if command.wait_for_exit {
        let timeout_ms = command.timeout.unwrap_or(DEFAULT_WAIT_FOR_EXIT_TIMEOUT_MS);
        let deadline = now_ms().saturating_add(u128::from(timeout_ms));
        while !matches!(session.status.as_str(), "closed" | "disconnected") {
            if now_ms() >= deadline {
                break;
            }
            sleep_ms(command.wait_ms);
            refresh_session_state(session);
        }
    }

    if let Some(idle_ms) = command.wait_idle {
        let deadline = command
            .timeout
            .map(|timeout_ms| now_ms().saturating_add(u128::from(timeout_ms)));
        let mut last_output_len = session.output.len();
        let mut idle_since = now_ms();
        loop {
            if let Some(deadline) = deadline {
                if now_ms() >= deadline {
                    break;
                }
            }
            sleep_ms(command.wait_ms);
            refresh_session_state(session);
            let current_len = session.output.len();
            if current_len != last_output_len {
                last_output_len = current_len;
                idle_since = now_ms();
            } else if now_ms().saturating_sub(idle_since) >= u128::from(idle_ms) {
                break;
            }
            if matches!(session.status.as_str(), "closed" | "disconnected") {
                break;
            }
        }
    }

    let page = page_output(session, None, command.limit, should_strip_ansi(command.raw));
    let mut response = daemon_output_response(
        &session.id,
        page,
        &session.status,
        session.exit_status,
        session.exit_signal.clone(),
    );
    if !auto_responses.is_empty() {
        response["auto_responses"] = serde_json::Value::Array(auto_responses);
    }
    Ok(response)
}

pub fn daemon_read(command: ReadCommand, state: &mut ServerState) -> Result<Value> {
    // Get connection_id (separate borrow scope)
    let connection_id = {
        let session = get_session_mut(state, &command.session_id)?;
        session.connection_id.clone()
    };

    // Switch to blocking mode for reliable PTY read
    let blocking_timeout = u32::try_from(command.wait_ms.max(2000)).unwrap_or(5000);
    begin_blocking_io(state, &connection_id, blocking_timeout);

    let session = get_session_mut(state, &command.session_id)?;
    sleep_ms(command.wait_ms);
    refresh_session_state(session);
    let page = page_output(session, command.offset, command.limit, should_strip_ansi(command.raw));
    let response = Ok(daemon_output_response(
        &session.id,
        page,
        &session.status,
        session.exit_status,
        session.exit_signal.clone(),
    ));

    end_blocking_io(state, &connection_id);
    response
}

pub fn daemon_resize(command: ResizeCommand, state: &mut ServerState) -> Result<Value> {
    let session = get_session_mut(state, &command.session_id)?;
    ensure_session_connected(session)?;
    session.cols = command.cols;
    session.rows = command.rows;
    session
        .shell
        .request_pty_size(command.cols, command.rows, None, None)?;
    session.updated_at = now_ms();
    Ok(serde_json::json!({ "session_id": session.id, "cols": command.cols, "rows": command.rows }))
}

pub fn daemon_signal(command: SignalCommand, state: &mut ServerState) -> Result<Value> {
    let session = get_session_mut(state, &command.session_id)?;
    ensure_session_connected(session)?;
    match command.signal.as_str() {
        "INT" | "SIGINT" => session.shell.write_all(b"\x03")?,
        "QUIT" | "SIGQUIT" => session.shell.write_all(b"\x1c")?,
        "TSTP" | "SIGTSTP" => session.shell.write_all(b"\x1a")?,
        "TERM" | "SIGTERM" | "KILL" | "SIGKILL" => {
            let _ = session.shell.close();
            session.status = "closed".to_string();
        }
        _ => bail!("unsupported signal '{}'. Supported signals: INT, QUIT, TSTP, TERM, KILL.", command.signal),
    }
    session.updated_at = now_ms();
    Ok(serde_json::json!({ "session_id": session.id, "signal": command.signal }))
}

pub fn daemon_status(session_id: &str, state: &mut ServerState) -> Result<Value> {
    {
        let session = get_session_mut(state, session_id)?;
        refresh_session_state(session);
    }
    let all_sessions = state.sessions.values().map(session_metadata).collect::<Vec<_>>();
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", session_id))?;
    Ok(session_summary(session, &all_sessions))
}

pub fn daemon_ping(session_id: &str, state: &mut ServerState) -> Result<Value> {
    let session = get_session_mut(state, session_id)?;
    refresh_session_state(session);
    Ok(serde_json::json!({
        "session_id": session.id,
        "alive": session.status == "running",
        "status": session.status,
    }))
}

pub fn daemon_output_response(
    session_id: &str,
    output: Value,
    status: &str,
    exit_status: Option<i32>,
    exit_signal: Option<String>,
) -> Value {
    let mut response = serde_json::json!({
        "session_id": session_id,
        "output": output,
        "status": status,
    });
    if let Some(code) = exit_status {
        response["exit_status"] = serde_json::json!(code);
    }
    if let Some(signal) = exit_signal {
        response["exit_signal"] = serde_json::json!(signal);
    }
    response
}

fn render_output_page(output: &[u8], start: usize, end: usize, strip_ansi_output: bool) -> Value {
    let text = String::from_utf8_lossy(&output[start..end]).to_string();
    serde_json::json!({
        "text": if strip_ansi_output { strip_ansi(&text) } else { text },
        "offset": start,
        "next_offset": end,
        "total": output.len(),
    })
}

fn should_strip_ansi(raw: bool) -> bool {
    !raw
}

pub fn shared_with_ids(
    session_id: &str,
    connection_id: &str,
    all_sessions: &[SessionMetadataForTesting],
) -> Vec<String> {
    all_sessions
        .iter()
        .filter(|session| session.connection_id == connection_id && session.id != session_id)
        .map(|session| session.id.clone())
        .collect()
}

#[cfg(test)]
pub fn session_summary_for_testing(
    session: &SessionMetadataForTesting,
    all_sessions: &[SessionMetadataForTesting],
) -> Value {
    serde_json::json!({
        "id": session.id,
        "connection_id": session.connection_id,
        "shared_with": shared_with_ids(&session.id, &session.connection_id, all_sessions),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        SessionMetadataForTesting, daemon_output_response, render_output_page, session_summary_for_testing,
        shared_with_ids, should_strip_ansi,
    };
    use crate::cli::ExpectRespondPair;
    use crate::util::strip_ansi;

    #[test]
    fn daemon_output_response_omits_terminal_fields_when_absent() {
        let response = daemon_output_response("s1", serde_json::json!({ "text": "" }), "running", None, None);

        assert_eq!(response["session_id"], "s1");
        assert_eq!(response["status"], "running");
        assert!(response.get("exit_status").is_none());
        assert!(response.get("exit_signal").is_none());
    }

    #[test]
    fn daemon_output_response_includes_terminal_fields_when_present() {
        let response = daemon_output_response(
            "s1",
            serde_json::json!({ "text": "" }),
            "closed",
            Some(1),
            Some("TERM".to_string()),
        );

        assert_eq!(response["exit_status"], 1);
        assert_eq!(response["exit_signal"], "TERM");
    }

    #[test]
    fn shared_with_ids_only_lists_other_sessions_on_same_connection() {
        let all_sessions = vec![
            SessionMetadataForTesting {
                id: "s1".to_string(),
                connection_id: "c1".to_string(),
            },
            SessionMetadataForTesting {
                id: "s2".to_string(),
                connection_id: "c1".to_string(),
            },
            SessionMetadataForTesting {
                id: "s3".to_string(),
                connection_id: "c2".to_string(),
            },
        ];

        let shared = shared_with_ids("s1", "c1", &all_sessions);
        assert_eq!(shared, vec!["s2".to_string()]);
    }

    #[test]
    fn session_summary_includes_connection_group_metadata() {
        let summary = session_summary_for_testing(
            &SessionMetadataForTesting {
                id: "s2".to_string(),
                connection_id: "c1".to_string(),
            },
            &[
                SessionMetadataForTesting {
                    id: "s1".to_string(),
                    connection_id: "c1".to_string(),
                },
                SessionMetadataForTesting {
                    id: "s2".to_string(),
                    connection_id: "c1".to_string(),
                },
            ],
        );

        assert_eq!(summary["connection_id"], "c1");
        assert_eq!(summary["shared_with"], serde_json::json!(["s1"]));
    }

    #[test]
    fn regex_fallback_to_substring_matching_is_case_insensitive() {
        let pair = ExpectRespondPair {
            expect: "[sudo] password".to_string(),
            respond: "secret\n".to_string(),
        };
        let output = "Prompt: [SUDO] PASSWORD for root:".to_string();
        let matched = {
            let output_lower = output.to_lowercase();
            let expect_lower = pair.expect.to_lowercase();
            let regex = regex::RegexBuilder::new(&pair.expect).case_insensitive(true).build();
            match regex {
                Ok(compiled) => compiled.is_match(&output) || output_lower.contains(&expect_lower),
                Err(_) => output_lower.contains(&expect_lower),
            }
        };

        assert!(matched);
    }

    #[test]
    fn render_output_page_strips_ansi_by_default() {
        let output = [104_u8, 101, 108, 108, 111, 32, 27, 91, 51, 49, 109, 114, 101, 100, 27, 91, 48, 109];

        let page = render_output_page(&output, 0, output.len(), true);

        assert_eq!(page["text"], "hello red");
        assert_eq!(page["total"], output.len());
    }

    #[test]
    fn render_output_page_preserves_ansi_when_raw_requested() {
        let output = [104_u8, 101, 108, 108, 111, 32, 27, 91, 51, 49, 109, 114, 101, 100, 27, 91, 48, 109];

        let page = render_output_page(&output, 0, output.len(), false);

        let expected = String::from_utf8_lossy(&output).to_string();
        assert_eq!(page["text"], expected);
        assert_eq!(strip_ansi(&expected), "hello red");
    }

    #[test]
    fn should_strip_ansi_defaults_to_true_unless_raw_requested() {
        assert!(should_strip_ansi(false));
        assert!(!should_strip_ansi(true));
    }
}
