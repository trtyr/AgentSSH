use crate::cli::ReadCommand;
use crate::protocol::{WireRequest, WireResponse};
use crate::ssh_backend::{
    ProxyState, RemoteSession, SharedConnection, daemon_connect, daemon_download, daemon_ls,
    daemon_ping, daemon_proxy_close, daemon_proxy_create, daemon_proxy_list, daemon_proxy_ping,
    daemon_read, daemon_resize, daemon_send, daemon_signal, daemon_spawn, daemon_status,
    daemon_upload, refresh_session_state, session_summary,
};
use crate::util::{log_daemon, now_ms, runtime_socket_path, sleep_ms};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const HEARTBEAT_INTERVAL_MS: u64 = 60_000;
const FOLLOW_POLL_MS: u64 = 100;

#[derive(Default)]
pub struct ServerState {
    pub sessions: BTreeMap<String, RemoteSession>,
    pub connections: BTreeMap<String, SharedConnection>,
    pub proxies: BTreeMap<String, ProxyState>,
    pub next_id: u64,
    pub next_connection_id: u64,
}

pub fn run_server() -> Result<()> {
    let socket = runtime_socket_path();
    if socket.exists() {
        let _ = fs::remove_file(&socket);
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    let _ = log_daemon(&format!("daemon started, socket at {}", socket.display()));
    let state = Arc::new(Mutex::new(ServerState::default()));
    spawn_heartbeat(state.clone());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if handle_client(stream, &state)? {
                    break;
                }
            }
            Err(error) => {
                let _ = log_daemon(&format!("request error: accept error: {error}"));
                eprintln!("accept error: {error}");
            }
        }
    }
    let _ = log_daemon("daemon shutdown");
    let _ = fs::remove_file(socket);
    Ok(())
}

pub fn run_client(request: WireRequest, json: bool) -> Result<()> {
    let response = daemon_request(request)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    print_human(response.data.unwrap_or(Value::Null))
}

pub fn run_read_command(command: ReadCommand, json: bool) -> Result<()> {
    if !command.follow {
        return run_client(WireRequest::Read(command), json);
    }
    if !json {
        bail!("agentssh session read --follow requires --json");
    }

    let started_at = now_ms();
    loop {
        let response = daemon_request(WireRequest::Read(command.clone()))?;
        let data = response.data.unwrap_or(Value::Null);
        println!("{}", serde_json::to_string(&serde_json::json!({
            "session_id": data.get("session_id").cloned().unwrap_or(Value::Null),
            "output": data.get("output").cloned().unwrap_or(Value::Null),
        }))?);

        let status = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if matches!(status, "closed" | "disconnected") {
            return Ok(());
        }

        if let Some(timeout_ms) = command.timeout {
            if now_ms().saturating_sub(started_at) >= u128::from(timeout_ms) {
                return Ok(());
            }
        }

        sleep_ms(FOLLOW_POLL_MS);
    }
}

fn handle_client(mut stream: UnixStream, state: &Arc<Mutex<ServerState>>) -> Result<bool> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    if line.trim().is_empty() {
        return Ok(false);
    }
    let request: WireRequest = serde_json::from_str(line.trim()).context("parse daemon request")?;
    if matches!(request, WireRequest::Shutdown) {
        write_wire(&mut stream, Ok(serde_json::json!({ "event": "shutdown" })))?;
        return Ok(true);
    }
    let result = {
        let mut guard = state.lock().map_err(|_| anyhow::anyhow!("daemon state lock poisoned"))?;
        handle_request(request, &mut guard)
    };
    write_wire(&mut stream, result)?;
    Ok(false)
}

fn handle_request(request: WireRequest, state: &mut ServerState) -> Result<Value> {
    match request {
        WireRequest::Connect(command) => {
            let response = daemon_connect(command, state)?;
            if let (Some(session_id), Some(host), Some(port)) = (
                response.get("session_id").and_then(Value::as_str),
                response.get("host").and_then(Value::as_str),
                response.get("port").and_then(Value::as_u64),
            ) {
                let _ = log_daemon(&format!("session {} created on {}:{}", session_id, host, port));
            }
            Ok(response)
        },
        WireRequest::Send(command) => daemon_send(command, state),
        WireRequest::Spawn(command) => daemon_spawn(command, state),
        WireRequest::Read(command) => daemon_read(command, state),
        WireRequest::Resize(command) => daemon_resize(command, state),
        WireRequest::Signal(command) => daemon_signal(command, state),
        WireRequest::Status(command) => daemon_status(&command.session_id, state),
        WireRequest::Ping(command) => daemon_ping(&command.session_id, state),
        WireRequest::List => {
            let all_sessions = state
                .sessions
                .values()
                .map(|session| crate::ssh_backend::SessionMetadataForTesting {
                    id: session.id.clone(),
                    connection_id: session.connection_id.clone(),
                })
                .collect::<Vec<_>>();
            Ok(serde_json::json!({
                "sessions": state
                    .sessions
                    .values()
                    .map(|session| session_summary(session, &all_sessions))
                    .collect::<Vec<_>>()
            }))
        }
        WireRequest::Close(command) => {
            let mut session = state
                .sessions
                .remove(&command.session_id)
                .ok_or_else(|| anyhow::anyhow!("session '{}' not found. Use 'agentssh session list' to see active sessions.", command.session_id))?;
            let connection_id = session.connection_id.clone();
            let _ = session.shell.close();
            let _ = session.shell.wait_close();
            if let Some(connection) = state.connections.get_mut(&connection_id) {
                connection.refcount = connection.refcount.saturating_sub(1);
                if connection.refcount == 0 {
                    state.connections.remove(&connection_id);
                }
            }
            let _ = log_daemon(&format!("session {} closed", command.session_id));
            Ok(serde_json::json!({ "session_id": command.session_id, "status": "closed" }))
        }
        WireRequest::Upload(command) => daemon_upload(command, state),
        WireRequest::Download(command) => daemon_download(command, state),
        WireRequest::Ls(command) => daemon_ls(command, state),
        WireRequest::ProxyCreate(command) => {
            let response = daemon_proxy_create(command, state)?;
            if let (Some(proxy_id), Some(local_addr)) = (
                response.get("proxy_id").and_then(Value::as_str),
                response.get("local_addr").and_then(Value::as_str),
            ) {
                let _ = log_daemon(&format!("proxy {} listening on {}", proxy_id, local_addr));
            }
            Ok(response)
        }
        WireRequest::ProxyList => daemon_proxy_list(state),
        WireRequest::ProxyClose(command) => daemon_proxy_close(command, state),
        WireRequest::ProxyPing(command) => daemon_proxy_ping(command, state),
        WireRequest::Shutdown => unreachable!(),
    }
}

fn daemon_request(request: WireRequest) -> Result<WireResponse> {
    ensure_daemon()?;
    let socket = runtime_socket_path();
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connect daemon {}", socket.display()))?;
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: WireResponse = serde_json::from_str(line.trim()).context("parse daemon response")?;
    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "daemon request failed".to_string())
        );
    }
    Ok(response)
}

fn ensure_daemon() -> Result<()> {
    let socket = runtime_socket_path();
    if UnixStream::connect(&socket).is_ok() {
        return Ok(());
    }
    if socket.exists() {
        let _ = fs::remove_file(&socket);
    }
    ProcessCommand::new(std::env::current_exe()?)
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start agentssh daemon. Is agentssh installed and in PATH?")?;
    for _ in 0..50 {
        if UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "agentssh daemon failed to start at {}. Check if another instance is already running.",
        socket.display()
    )
}

fn spawn_heartbeat(state: Arc<Mutex<ServerState>>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
        let Ok(mut guard) = state.lock() else {
            break;
        };
        let mut alive = 0_usize;
        let checked = guard.sessions.len();
        for session in guard.sessions.values_mut() {
            let previous_status = session.status.clone();
            refresh_session_state(session);
            if session.status == "running" {
                alive += 1;
            }
            if previous_status != "disconnected" && session.status == "disconnected" {
                let _ = log_daemon(&format!("session {} disconnected", session.id));
            }
        }
        let _ = log_daemon(&format!("heartbeat: {} sessions checked, {} alive", checked, alive));
    });
}

fn write_wire(stream: &mut UnixStream, result: Result<Value>) -> Result<()> {
    let response = match result {
        Ok(data) => WireResponse {
            ok: true,
            data: Some(data),
            error: None,
        },
        Err(error) => {
            let rendered = format!("{error:#}");
            let _ = log_daemon(&format!("request error: {rendered}"));
            WireResponse {
                ok: false,
                data: None,
                error: Some(rendered),
            }
        }
    };
    stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn print_human(data: Value) -> Result<()> {
    if let Some(output) = data.pointer("/output/text").and_then(Value::as_str) {
        if !output.is_empty() {
            println!("{output}");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&data)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_read_requires_json_output() {
        let command = ReadCommand {
            session_id: "s1".to_string(),
            offset: None,
            limit: 10,
            wait_ms: 0,
            follow: true,
            raw: false,
            strip_ansi: false,
            timeout: Some(1),
        };

        let error = run_read_command(command, false).expect_err("follow without json should fail");
        assert!(error.to_string().contains("requires --json"));
    }
}
