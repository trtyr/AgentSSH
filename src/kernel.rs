use crate::cli::*;
use crate::connection;
use crate::protocol::{WireRequest, WireResponse};
use crate::session::{self, SessionHandle};
use crate::util::{self, now_ms};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify};

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
                    let mut st = hb_state.lock().await;
                    if let Err(e) = heartbeat(&mut st).await {
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

        let mut st = state.lock().await;
        let response = handle_request(request, &mut st).await;
        drop(st);

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

async fn handle_request(request: WireRequest, state: &mut ServerState) -> WireResponse {
    match request {
        // -- Session lifecycle --
        WireRequest::Connect(cmd) => {
            match session::daemon_connect(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Spawn(cmd) => {
            match session::daemon_spawn(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Close(cmd) => {
            match session::daemon_close(&cmd.session_id, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        // -- Session I/O --
        WireRequest::Send(cmd) => {
            let pairs = cmd.expect_pairs().unwrap_or_default();
            match session::daemon_send(
                &cmd.session_id,
                &cmd.input,
                cmd.crlf,
                pairs,
                if cmd.wait_ms > 0 { Some(cmd.wait_ms) } else { None },
                cmd.wait_idle,
                cmd.wait_for_exit,
                state,
            )
            .await
            {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Read(cmd) => {
            let strip = if cmd.raw { Some(false) } else { Some(true) };
            let wait = if cmd.wait_ms > 0 { Some(cmd.wait_ms) } else { None };
            let offset = cmd.offset.unwrap_or(0);
            match session::daemon_read(
                &cmd.session_id,
                offset,
                cmd.limit,
                wait,
                strip,
                state,
            )
            .await
            {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Exec(cmd) => {
            let command = cmd.command.join(" ");
            match session::daemon_exec(
                &cmd.session_id,
                &command,
                Some(cmd.timeout),
                state,
            )
            .await
            {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Resize(cmd) => {
            match session::daemon_resize(&cmd.session_id, cmd.cols, cmd.rows, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Signal(cmd) => {
            match session::daemon_signal(&cmd.session_id, &cmd.signal, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Status(cmd) => {
            match session::daemon_status(&cmd.session_id, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Ping(cmd) => {
            match session::daemon_ping(&cmd.session_id, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::List => {
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
            match crate::proxy::daemon_proxy_create(&cmd.connect, &proxy_mode, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::ProxyList => {
            let list = crate::proxy::proxy_list(&state.proxies);
            WireResponse { ok: true, data: Some(list), error: None }
        }

        WireRequest::ProxyPing(cmd) => {
            match crate::proxy::proxy_ping(&cmd.proxy_id, &state.proxies) {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::ProxyClose(cmd) => {
            let proxy_id = cmd.proxy_id.unwrap_or_default();
            match crate::proxy::proxy_close(&proxy_id, cmd.all, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        // -- File transfer --
        WireRequest::Upload(cmd) => {
            match crate::sftp::daemon_upload(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Download(cmd) => {
            match crate::sftp::daemon_download(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Ls(cmd) => {
            match crate::sftp::daemon_ls(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::Write(cmd) => {
            match crate::sftp::daemon_write_file(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::ReadFile(cmd) => {
            match crate::sftp::daemon_read_file(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::DeleteFile(cmd) => {
            match crate::sftp::daemon_delete(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        WireRequest::EditFile(cmd) => {
            match crate::sftp::daemon_edit(&cmd, state).await {
                Ok(v) => WireResponse { ok: true, data: Some(v), error: None },
                Err(e) => WireResponse { ok: false, data: None, error: Some(e.to_string()) },
            }
        }

        // -- Daemon lifecycle --
        WireRequest::DaemonStatus => {
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

// ---------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------

async fn heartbeat(state: &mut ServerState) -> Result<()> {
    let session_ids: Vec<String> = state.sessions.keys().cloned().collect();
    for sid in &session_ids {
        if let Some(session) = state.sessions.get(sid) {
            let dead = *session.drain_dead.lock().unwrap();
            if dead {
                let status = session.status.lock().unwrap().clone();
                if status == "running" {
                    *session.status.lock().unwrap() = "disconnected".to_string();
                    util::log_daemon(&format!("session {} drain task died, marking disconnected", sid))?;
                }
            }

            let should_reconnect = session.reconnect && *session.status.lock().unwrap() == "disconnected";
            if should_reconnect {
                let connect_args = session.connect_args.clone();
                let cols = session.cols;
                let rows = session.rows;
                let output = session.output.clone();
                let status = session.status.clone();
                let exit_status = session.exit_status.clone();
                let exit_signal = session.exit_signal.clone();
                let drain_dead = session.drain_dead.clone();
                let updated_at = session.updated_at.clone();
                let session_id = sid.clone();
                let _ = session;
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
                    state,
                )
                .await
                {
                    Ok(()) => {
                        util::log_daemon(&format!("session {} reconnected", session_id))?;
                    }
                    Err(e) => {
                        util::log_daemon(&format!("reconnect failed for {}: {}", session_id, e))?;
                    }
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
    status: Arc<std::sync::Mutex<String>>,
    exit_status: Arc<std::sync::Mutex<Option<i32>>>,
    exit_signal: Arc<std::sync::Mutex<Option<String>>>,
    drain_dead: Arc<std::sync::Mutex<bool>>,
    updated_at: Arc<std::sync::Mutex<u64>>,
    _state: &mut ServerState,
) -> Result<()> {
    let (handle, _host, _port, _username) = connection::connect_with_info(connect_args).await?;
    let channel = session::open_pty(&handle, cols, rows).await?;
    *drain_dead.lock().unwrap() = false;
    *status.lock().unwrap() = "running".to_string();
    session::spawn_drain_task(channel, output, status, exit_status, exit_signal, drain_dead, updated_at).await;
    Ok(())
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
