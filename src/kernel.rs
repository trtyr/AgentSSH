use crate::cli::*;
use crate::connection;
use crate::protocol::{WireRequest, WireResponse};
use crate::session::{self, SessionHandle, SessionStatus};
use crate::util::{self, now_ms};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, mpsc};

// ---------------------------------------------------------------------------
// SharedConnection (re-export from connection module)
// ---------------------------------------------------------------------------

pub use crate::connection::SharedConnection;

// ---------------------------------------------------------------------------
// ServerState
// ---------------------------------------------------------------------------

pub struct ServerState {
    pub sessions: BTreeMap<String, SessionHandle>,
    pub connections: BTreeMap<String, SharedConnection>,
    pub proxies: BTreeMap<String, crate::proxy::ProxyState>,
    pub next_id: u64,
    pub next_connection_id: u64,
    pub started_at: u64,
}

impl ServerState {
    fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            connections: BTreeMap::new(),
            proxies: BTreeMap::new(),
            next_id: 1,
            next_connection_id: 1,
            started_at: now_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon server
// ---------------------------------------------------------------------------

/// Start the daemon server. Runs until shutdown.
pub fn run_server() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async { run_server_async().await })
}

async fn run_server_async() -> Result<()> {
    let socket_path = util::runtime_socket_path();
    let socket_dir = Path::new(&socket_path)
        .parent()
        .context("socket path has no parent")?;
    std::fs::create_dir_all(socket_dir)?;

    // Remove stale socket
    if Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path).ok();
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding socket at {}", socket_path.display()))?;

    let state = Arc::new(Mutex::new(ServerState::new()));
    let shutdown = Arc::new(Notify::new());

    util::log_daemon(&format!("daemon listening on {}", socket_path.display()))?;

    // Spawn heartbeat
    let hb_state = state.clone();
    let hb_shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = hb_shutdown.notified() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
                    if let Err(e) = heartbeat(hb_state.clone()).await {
                        let _ = util::log_daemon(&format!("heartbeat error: {}", e));
                    }
                }
            }
        }
    });

    // Accept loop
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                break;
            }
            accept_result = listener.accept() => match accept_result {
                Ok((stream, _addr)) => {
                    let state = state.clone();
                    let shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, state, shutdown).await {
                            let _ = util::log_daemon(&format!("client error: {}", e));
                        }
                    });
                }
                Err(e) => {
                    let _ = util::log_daemon(&format!("accept error: {}", e));
                }
            }
        }
    }

    std::fs::remove_file(&socket_path).ok();
    Ok(())
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<ServerState>>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // client disconnected
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: WireRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let resp = WireResponse {
                    ok: false,
                    data: None,
                    error: Some(format!("invalid request: {}", e)),
                };
                let out = serde_json::to_string(&resp)? + "\n";
                writer.write_all(out.as_bytes()).await?;
                continue;
            }
        };

        let should_shutdown = matches!(&request, WireRequest::Shutdown);

        let response = handle_request(request, state.clone()).await;

        let out = serde_json::to_string(&response)? + "\n";
        writer.write_all(out.as_bytes()).await?;

        if should_shutdown {
            shutdown.notify_waiters();
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Request handler
// ---------------------------------------------------------------------------

async fn handle_request(request: WireRequest, state: Arc<Mutex<ServerState>>) -> WireResponse {
    match request {
        // -- Session lifecycle --
        WireRequest::Connect(cmd) => {
            handle_connect_request(cmd, state).await
        }

        WireRequest::Spawn(cmd) => {
            handle_spawn_request(cmd, state).await
        }

        WireRequest::Close(cmd) => {
            let mut state = state.lock().await;
            match session::daemon_close(&cmd.session_id, &mut state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        // -- Session I/O --
        WireRequest::Send(cmd) => {
            handle_send_request(cmd, state).await
        }

        WireRequest::Read(cmd) => {
            handle_read_request(cmd, state).await
        }

        WireRequest::Exec(cmd) => {
            handle_exec_request(cmd, state).await
        }

        WireRequest::Resize(cmd) => {
            let mut state = state.lock().await;
            match session::daemon_resize(&cmd.session_id, cmd.cols, cmd.rows, &mut state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Signal(cmd) => {
            let mut state = state.lock().await;
            match session::daemon_signal(&cmd.session_id, &cmd.signal, &mut state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Status(cmd) => {
            let mut state = state.lock().await;
            match session::daemon_status(&cmd.session_id, &mut state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Ping(cmd) => {
            let mut state = state.lock().await;
            match session::daemon_ping(&cmd.session_id, &mut state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::List => {
            let state = state.lock().await;
            let sessions: Vec<Value> = state
                .sessions
                .iter()
                .map(|(_, s)| s.summary(&state.sessions))
                .collect();
            WireResponse { ok: true, data: Some(json!({"sessions": sessions})), error: None }
        }

        // -- Proxy --
        WireRequest::ProxyCreate(cmd) => {
            let proxy_mode = match crate::proxy::proxy_mode_from_command(
                cmd.local.as_deref(),
                cmd.remote.as_deref(),
                cmd.socks5.as_deref(),
            ) {
                Ok(m) => m,
                Err(e) => return WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            };
            handle_proxy_create_request(cmd.connect, proxy_mode, state).await
        }

        WireRequest::ProxyList => {
            let state = state.lock().await;
            let list = crate::proxy::proxy_list(&state.proxies);
            WireResponse { ok: true, data: Some(list), error: None }
        }

        WireRequest::ProxyPing(cmd) => {
            let state = state.lock().await;
            match crate::proxy::proxy_ping(&cmd.proxy_id, &state.proxies) {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::ProxyClose(cmd) => {
            let proxy_id = cmd.proxy_id.unwrap_or_default();
            handle_proxy_close_request(proxy_id, cmd.all, state).await
        }

        // -- File transfer --
        WireRequest::Upload(cmd) => {
            handle_upload_request(cmd, state).await
        }

        WireRequest::Download(cmd) => {
            handle_download_request(cmd, state).await
        }

        WireRequest::Ls(cmd) => {
            handle_ls_request(cmd, state).await
        }

        WireRequest::Write(cmd) => {
            handle_write_request(cmd, state).await
        }

        WireRequest::ReadFile(cmd) => {
            handle_read_file_request(cmd, state).await
        }

        WireRequest::DeleteFile(cmd) => {
            handle_delete_file_request(cmd, state).await
        }

        WireRequest::EditFile(cmd) => {
            handle_edit_file_request(cmd, state).await
        }

        // -- Daemon lifecycle --
        WireRequest::DaemonStatus => {
            let state = state.lock().await;
            WireResponse {
                ok: true,
                data: Some(json!({
                    "pid": std::process::id(),
                    "started_at": state.started_at,
                    "sessions": state.sessions.len(),
                    "connections": state.connections.len(),
                    "proxies": state.proxies.len(),
                })),
                error: None,
            }
        }

        WireRequest::Shutdown => {
            WireResponse { ok: true, data: Some(json!({"message": "shutting down"})), error: None }
        }
    }
}

fn ok(data: Value) -> WireResponse {
    WireResponse { ok: true, data: Some(data), error: None }
}

fn err(error: impl ToString) -> WireResponse {
    WireResponse { ok: false, data: None, error: Some(error.to_string()) }
}

async fn handle_connect_request(cmd: ConnectCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let (session_id, connection_id) = {
        let mut st = state.lock().await;
        let connection_id = format!("c{}", st.next_connection_id);
        st.next_connection_id += 1;
        let session_id = format!("s{}", st.next_id);
        st.next_id += 1;
        (session_id, connection_id)
    };

    let resolved = match connection::resolve_connect_args(&cmd.connect) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let (handle, host, port, username) = match connection::connect_with_info(&resolved).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let channel = match session::open_pty(&handle, cmd.cols, cmd.rows).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };

    let output = Arc::new(std::sync::Mutex::new(session::OutputBuffer::default()));
    let status = Arc::new(std::sync::Mutex::new(SessionStatus::Running));
    let exit_status = Arc::new(std::sync::Mutex::new(None));
    let exit_signal = Arc::new(std::sync::Mutex::new(None));
    let drain_dead = Arc::new(std::sync::Mutex::new(false));
    let updated_at = Arc::new(std::sync::Mutex::new(now_ms()));
    let cmd_tx = session::spawn_drain_task(
        channel,
        output.clone(),
        status.clone(),
        exit_status.clone(),
        exit_signal.clone(),
        drain_dead.clone(),
        updated_at.clone(),
    )
    .await;

    let mut st = state.lock().await;
    st.connections.insert(
        connection_id.clone(),
        SharedConnection { handle: handle.clone(), refcount: 1 },
    );
    let session = SessionHandle {
        id: session_id.clone(),
        connection_id: connection_id.clone(),
        host,
        port,
        username,
        cols: cmd.cols,
        rows: cmd.rows,
        reconnect: cmd.reconnect,
        connect_args: resolved,
        created_at: now_ms(),
        updated_at,
        output,
        status,
        exit_status,
        exit_signal,
        cmd_tx,
        drain_dead,
    };
    let summary = session.summary(&BTreeMap::new());
    st.sessions.insert(session_id.clone(), session);
    ok(json!({"ok": true, "session_id": session_id, "connection_id": connection_id, "session": summary}))
}

async fn handle_spawn_request(cmd: SpawnCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let (connection_id, host, port, username, connect_args, handle, new_id) = {
        let mut st = state.lock().await;
        let source = match st.sessions.get(&cmd.from) {
            Some(v) => v,
            None => return err(format!("session {} not found", cmd.from)),
        };
        let connection_id = source.connection_id.clone();
        let host = source.host.clone();
        let port = source.port;
        let username = source.username.clone();
        let connect_args = source.connect_args.clone();
        let handle = match st.connections.get(&connection_id) {
            Some(conn) => conn.handle.clone(),
            None => return err(format!("connection {} not found", connection_id)),
        };
        let new_id = format!("s{}", st.next_id);
        st.next_id += 1;
        (connection_id, host, port, username, connect_args, handle, new_id)
    };

    let channel = match session::open_pty(&handle, cmd.cols, cmd.rows).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let output = Arc::new(std::sync::Mutex::new(session::OutputBuffer::default()));
    let status = Arc::new(std::sync::Mutex::new(SessionStatus::Running));
    let exit_status = Arc::new(std::sync::Mutex::new(None));
    let exit_signal = Arc::new(std::sync::Mutex::new(None));
    let drain_dead = Arc::new(std::sync::Mutex::new(false));
    let updated_at = Arc::new(std::sync::Mutex::new(now_ms()));
    let cmd_tx = session::spawn_drain_task(
        channel,
        output.clone(),
        status.clone(),
        exit_status.clone(),
        exit_signal.clone(),
        drain_dead.clone(),
        updated_at.clone(),
    )
    .await;

    let mut st = state.lock().await;
    let Some(conn) = st.connections.get_mut(&connection_id) else {
        return err(format!("connection {} not found", connection_id));
    };
    conn.refcount += 1;
    let session = SessionHandle {
        id: new_id.clone(),
        connection_id: connection_id.clone(),
        host,
        port,
        username,
        cols: cmd.cols,
        rows: cmd.rows,
        reconnect: false,
        connect_args,
        created_at: now_ms(),
        updated_at,
        output,
        status,
        exit_status,
        exit_signal,
        cmd_tx,
        drain_dead,
    };
    let summary = session.summary(&BTreeMap::new());
    st.sessions.insert(new_id.clone(), session);
    ok(json!({"ok": true, "session_id": new_id, "connection_id": connection_id, "session": summary}))
}

async fn handle_exec_request(cmd: SessionExecCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = {
        let st = state.lock().await;
        let session = match st.sessions.get(&cmd.session_id) {
            Some(v) => v,
            None => return err(format!("session {} not found", cmd.session_id)),
        };
        match st.connections.get(&session.connection_id) {
            Some(conn) => conn.handle.clone(),
            None => return err(format!("connection {} not found", session.connection_id)),
        }
    };
    let mut channel = match handle.channel_open_session().await.context("opening exec channel") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let command = cmd.command.join(" ");
    match connection::exec_channel(&mut channel, &command, Some(cmd.timeout)).await {
        Ok((stdout, stderr, exit_code)) => ok(json!({"ok": true, "stdout": stdout, "stderr": stderr, "exit_code": exit_code})),
        Err(e) => err(e),
    }
}

async fn handle_send_request(cmd: SessionInputCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let pairs = cmd.expect_pairs().unwrap_or_default();
    let (cmd_tx, output, status, exit_status, exit_signal) = {
        let st = state.lock().await;
        let session = match st.sessions.get(&cmd.session_id) {
            Some(v) => v,
            None => return err(format!("session {} not found", cmd.session_id)),
        };
        (
            session.cmd_tx.clone(),
            session.output.clone(),
            session.status.clone(),
            session.exit_status.clone(),
            session.exit_signal.clone(),
        )
    };

    let normalized = session::normalize_input(&cmd.input, cmd.crlf);
    if cmd_tx.send(session::SessionCmd::Send { data: normalized.into_bytes() }).is_err() {
        return err("drain task dead");
    }

    if cmd.wait_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(cmd.wait_ms)).await;
    }
    if let Some(idle_ms) = cmd.wait_idle {
        tokio::time::sleep(std::time::Duration::from_millis(idle_ms)).await;
    }

    for pair in pairs {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let matches = {
                let buf = output.lock().unwrap();
                let haystack = String::from_utf8_lossy(&buf.data);
                match session::output_matches_str(&haystack, &pair.expect) {
                    Ok(v) => v,
                    Err(e) => return err(e),
                }
            };
            if matches {
                let normalized_resp = session::normalize_input(&pair.respond, true);
                if cmd_tx.send(session::SessionCmd::Send { data: normalized_resp.into_bytes() }).is_err() {
                    return err("drain task dead");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return err(format!("timeout waiting for expect pattern: {}", pair.expect));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    if cmd.wait_for_exit {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if *status.lock().unwrap() != SessionStatus::Running || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    let st = state.lock().await;
    let Some(session) = st.sessions.get(&cmd.session_id) else {
        return err(format!("session {} not found", cmd.session_id));
    };
    let output_val = session.page_output(0, util::DEFAULT_LIMIT, true);
    ok(session::daemon_output_response(
        &cmd.session_id,
        &output_val,
        &status.lock().unwrap().clone(),
        *exit_status.lock().unwrap(),
        exit_signal.lock().unwrap().clone(),
    ))
}

async fn handle_read_request(cmd: ReadCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let wait = if cmd.wait_ms > 0 { Some(cmd.wait_ms) } else { None };
    if let Some(ms) = wait {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    let st = state.lock().await;
    let Some(session) = st.sessions.get(&cmd.session_id) else {
        return err(format!("session {} not found", cmd.session_id));
    };
    let output_val = session.page_output(cmd.offset.unwrap_or(0), cmd.limit, !cmd.raw);
    let status = session.status.lock().unwrap().clone();
    let exit_status = *session.exit_status.lock().unwrap();
    let exit_signal = session.exit_signal.lock().unwrap().clone();
    ok(session::daemon_output_response(&cmd.session_id, &output_val, &status, exit_status, exit_signal))
}

fn daemon_handle_for_session(
    state: &ServerState,
    session_id: Option<&str>,
) -> Result<Arc<connection::SharedConnectionHandle>> {
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

async fn handle_upload_request(cmd: TransferCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_upload(&handle, &local, &remote, &cmd.method).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_download_request(cmd: TransferCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let local = cmd.local.to_string_lossy();
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_download(&handle, &local, &remote, &cmd.method).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_ls_request(cmd: ListCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_ls(&handle, &remote, &cmd.method).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_write_request(cmd: WriteCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_write(&handle, &remote, cmd.content.as_bytes(), &cmd.content.len(), "auto").await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_read_file_request(cmd: ReadFileCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_read_file(&handle, &remote).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_delete_file_request(cmd: DeleteFileCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_delete(&handle, &remote, cmd.recursive).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_edit_file_request(cmd: EditFileCommand, state: Arc<Mutex<ServerState>>) -> WireResponse {
    let handle = match {
        let st = state.lock().await;
        daemon_handle_for_session(&st, cmd.session_id.as_deref())
    } {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let remote = cmd.remote.to_string_lossy();
    match crate::sftp::do_edit(&handle, &remote, &cmd.find, &cmd.replace).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn handle_proxy_create_request(
    connect: ConnectArgs,
    proxy_mode: crate::proxy::ProxyMode,
    state: Arc<Mutex<ServerState>>,
) -> WireResponse {
    let (connection_id, proxy_id) = {
        let mut st = state.lock().await;
        let connection_id = format!("c{}", st.next_connection_id);
        st.next_connection_id += 1;
        let proxy_id = format!("p{}", st.next_id);
        st.next_id += 1;
        (connection_id, proxy_id)
    };
    let resolved = match connection::resolve_connect_args(&connect) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let (handle, _host, _port, _username) = match connection::connect_with_info(&resolved).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let local_addr = match &proxy_mode {
        crate::proxy::ProxyMode::LocalForward { local_addr, .. } => local_addr.clone(),
        crate::proxy::ProxyMode::Socks5 { local_addr } => local_addr.clone(),
    };
    let local_addr_parsed: std::net::SocketAddr = match local_addr.parse().with_context(|| format!("parsing local address: {}", local_addr)) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let listener = match tokio::net::TcpListener::bind(local_addr_parsed).await.with_context(|| format!("binding to {}", local_addr)) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status = Arc::new(Mutex::new(crate::proxy::ProxyStatus::Running));
    let mode = proxy_mode.clone();
    let shutdown_clone = shutdown.clone();
    let status_clone = status.clone();
    let proxy_id_clone = proxy_id.clone();
    let handle_clone = handle.clone();
    let listener_addr = local_addr.clone();
    tokio::spawn(async move {
        crate::proxy::run_proxy_listener(
            listener,
            &listener_addr,
            &mode,
            handle_clone,
            shutdown_clone,
            status_clone,
            &proxy_id_clone,
        )
        .await;
    });
    let mut st = state.lock().await;
    st.connections.insert(
        connection_id.clone(),
        SharedConnection { handle, refcount: 1 },
    );
    st.proxies.insert(
        proxy_id.clone(),
        crate::proxy::ProxyState {
            id: proxy_id.clone(),
            connection_id: connection_id.clone(),
            mode: proxy_mode.clone(),
            local_addr: local_addr.clone(),
            status,
            shutdown,
        },
    );
    let mode_value = match serde_json::to_value(&proxy_mode) {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    ok(json!({"ok": true, "proxy_id": proxy_id, "connection_id": connection_id, "local_addr": local_addr, "mode": mode_value}))
}

async fn handle_proxy_close_request(
    proxy_id: String,
    all: bool,
    state: Arc<Mutex<ServerState>>,
) -> WireResponse {
    let (closed, proxies) = {
        let mut st = state.lock().await;
        let ids_to_close: Vec<String> = if all {
            st.proxies.keys().cloned().collect()
        } else {
            vec![proxy_id]
        };

        let mut closed = Vec::new();
        let mut proxies = Vec::new();
        for id in ids_to_close {
            if let Some(proxy) = st.proxies.remove(&id) {
                if let Some(conn) = st.connections.get_mut(&proxy.connection_id) {
                    conn.refcount = conn.refcount.saturating_sub(1);
                    if conn.refcount == 0 {
                        st.connections.remove(&proxy.connection_id);
                    }
                }
                closed.push(id);
                proxies.push(proxy);
            }
        }
        (closed, proxies)
    };

    for proxy in proxies {
        proxy.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        *proxy.status.lock().await = crate::proxy::ProxyStatus::Stopped;
    }

    ok(json!({"ok": true, "closed": closed}))
}

// ---------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------

async fn heartbeat(state: Arc<Mutex<ServerState>>) -> Result<()> {
    let session_ids: Vec<String> = state.lock().await.sessions.keys().cloned().collect();
    for sid in &session_ids {
        let reconnect_target = {
            let st = state.lock().await;
            if let Some(session) = st.sessions.get(sid) {
            let dead = *session.drain_dead.lock().unwrap();
            if dead {
                let status = session.status.lock().unwrap().clone();
                if status == SessionStatus::Running {
                    *session.status.lock().unwrap() = SessionStatus::Disconnected;
                    util::log_daemon(&format!("session {} drain task died, marking disconnected", sid))?;
                }
            }

            let should_reconnect = session.reconnect && *session.status.lock().unwrap() == SessionStatus::Disconnected;
            if should_reconnect {
                Some((
                    sid.clone(),
                    session.connection_id.clone(),
                    session.connect_args.clone(),
                    session.cols,
                    session.rows,
                    session.output.clone(),
                    session.status.clone(),
                    session.exit_status.clone(),
                    session.exit_signal.clone(),
                    session.drain_dead.clone(),
                    session.updated_at.clone(),
                ))
            } else {
                None
            }
            } else {
                None
            }
        };

        if let Some((
            session_id,
            connection_id,
            connect_args,
            cols,
            rows,
            output,
            status,
            exit_status,
            exit_signal,
            drain_dead,
            updated_at,
        )) = reconnect_target {
            util::log_daemon(&format!("attempting reconnect for session {}", session_id))?;
            match attempt_reconnect(
                &connect_args,
                cols,
                rows,
                output,
                status,
                exit_status,
                exit_signal,
                drain_dead,
                updated_at,
            )
            .await
            {
                Ok((new_cmd_tx, new_handle)) => {
                    let mut st = state.lock().await;
                    if let Some(session) = st.sessions.get_mut(&session_id) {
                        session.cmd_tx = new_cmd_tx;
                    }
                    if let Some(conn) = st.connections.get_mut(&connection_id) {
                        conn.handle = new_handle;
                    }
                    util::log_daemon(&format!("session {} reconnected", session_id))?;
                }
                Err(e) => {
                    util::log_daemon(&format!("reconnect failed for {}: {}", session_id, e))?;
                }
            }
        }
    }
    Ok(())
}

async fn attempt_reconnect(
    connect_args: &ConnectArgs,
    cols: u32,
    rows: u32,
    output: Arc<std::sync::Mutex<session::OutputBuffer>>,
    status: Arc<std::sync::Mutex<SessionStatus>>,
    exit_status: Arc<std::sync::Mutex<Option<i32>>>,
    exit_signal: Arc<std::sync::Mutex<Option<String>>>,
    drain_dead: Arc<std::sync::Mutex<bool>>,
    updated_at: Arc<std::sync::Mutex<u64>>,
    ) -> Result<(mpsc::UnboundedSender<session::SessionCmd>, Arc<connection::SharedConnectionHandle>)> {
    let (handle, _host, _port, _username) = connection::connect_with_info(connect_args).await?;
    let channel = session::open_pty(&handle, cols, rows).await?;
    *drain_dead.lock().unwrap() = false;
    *status.lock().unwrap() = SessionStatus::Running;
    let cmd_tx = session::spawn_drain_task(channel, output, status, exit_status, exit_signal, drain_dead, updated_at).await;
    Ok((cmd_tx, handle))
}

// ---------------------------------------------------------------------------
// Client-side: talk to daemon
// ---------------------------------------------------------------------------

pub fn daemon_request(request: WireRequest) -> Result<WireResponse> {
    let socket_path = util::runtime_socket_path();
    let mut stream = StdUnixStream::connect(&socket_path)
        .with_context(|| format!("connecting to daemon at {}", socket_path.display()))?;

    use std::io::{BufRead, Write};
    let json = serde_json::to_string(&request)? + "\n";
    stream.write_all(json.as_bytes())?;

    let mut reader = std::io::BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;

    let response: WireResponse = serde_json::from_str(response_line.trim())
        .with_context(|| "parsing daemon response")?;

    Ok(response)
}

pub fn run_client(request: WireRequest, json_output: bool) -> Result<()> {
    ensure_daemon()?;
    let response = daemon_request(request)?;

    match response {
        WireResponse { ok: true, data: Some(data), .. } => {
            if json_output {
                util::print_json(&data)?;
            } else {
                print_human(&data);
            }
        }
        WireResponse { ok: false, error: Some(msg), .. } => {
            if json_output {
                println!("{}", serde_json::to_string(&json!({"ok": false, "error": msg}))?);
            } else {
                eprintln!("error: {}", msg);
            }
            std::process::exit(1);
        }
        WireResponse { ok: true, data: None, .. } => {
            if json_output {
                println!("{}", serde_json::to_string(&json!({"ok": true}))?);
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn run_read_command(command: ReadCommand, json_output: bool) -> Result<()> {
    ensure_daemon()?;

    if !command.follow {
        return run_client(WireRequest::Read(command), json_output);
    }

    let session_id = command.session_id.clone();
    let limit = command.limit;
    let raw = command.raw;
    let timeout_ms = command.timeout;
    let mut total_offset = command.offset.unwrap_or(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    loop {
        let request = WireRequest::Read(ReadCommand {
            session_id: session_id.clone(),
            offset: Some(total_offset),
            limit,
            wait_ms: 500,
            follow: false,
            raw,
            strip_ansi: false,
            timeout: timeout_ms,
        });

        let response = daemon_request(request)?;

        match response {
            WireResponse { ok: true, data: Some(data), .. } => {
                let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
                let output = data.get("output");
                let next_offset = output
                    .and_then(|o| o.get("offset"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(total_offset as u64) as usize;
                total_offset = next_offset;

                if json_output {
                    println!("{}", serde_json::to_string(&data)?);
                } else {
                    let text = output
                        .and_then(|o| o.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        print!("{}", text);
                    }
                }

                if status != "running" {
                    break;
                }
            }
            WireResponse { ok: false, error: Some(msg), .. } => {
                if json_output {
                    println!("{}", serde_json::to_string(&json!({"ok": false, "error": msg}))?);
                } else {
                    eprintln!("error: {}", msg);
                }
                break;
            }
            _ => break,
        }

        if std::time::Instant::now() > deadline {
            break;
        }
    }

    Ok(())
}

pub fn ensure_daemon() -> Result<()> {
    let socket_path = util::runtime_socket_path();

    if StdUnixStream::connect(&socket_path).is_ok() {
        return Ok(());
    }

    if Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path).ok();
    }

    let current_exe = std::env::current_exe().context("getting current executable path")?;
    let mut cmd = Command::new(current_exe);
    cmd.args(["daemon", "serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("spawning daemon")?;
    drop(child);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if StdUnixStream::connect(&socket_path).is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            bail!("daemon did not start within 5 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn print_human(data: &Value) {
    if let Some(text) = data.get("output").and_then(|o| o.get("text")).and_then(|v| v.as_str()) {
        if !text.is_empty() {
            print!("{}", text);
        }
    } else if let Some(stdout) = data.get("stdout").and_then(|v| v.as_str()) {
        if !stdout.is_empty() {
            print!("{}", stdout);
        }
        if let Some(stderr) = data.get("stderr").and_then(|v| v.as_str()) {
            if !stderr.is_empty() {
                eprint!("{}", stderr);
            }
        }
    } else if let Some(content) = data.get("content").and_then(|v| v.as_str()) {
        print!("{}", content);
    } else {
        println!("{}", serde_json::to_string_pretty(data).unwrap());
    }
}

pub fn daemon_install() -> Result<()> {
    let socket_path = util::runtime_socket_path();
    let exe = std::env::current_exe()?.display().to_string();
    println!("Daemon binary: {}", exe);
    println!("Socket path: {}", socket_path.display());
    println!("To install as systemd service, create a unit file.");
    Ok(())
}

pub fn daemon_uninstall() -> Result<()> {
    let socket_path = util::runtime_socket_path();
    if Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path)?;
    }
    println!("Daemon socket removed.");
    Ok(())
}
