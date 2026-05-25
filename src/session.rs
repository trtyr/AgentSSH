use crate::cli::{ConnectArgs, ConnectCommand, ExpectRespondPair, SpawnCommand};
use crate::connection::{self, SharedConnection, SharedConnectionHandle};
use crate::util::{self, now_ms, strip_ansi};
use anyhow::{Context, Result, bail};
use russh::{client, Channel, ChannelMsg};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_BUFFER: usize = 1024 * 1024; // 1 MB

// ---------------------------------------------------------------------------
// Commands sent to the drain task via mpsc
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SessionCmd {
    Send { data: Vec<u8> },
    Resize { cols: u32, rows: u32 },
    Signal { signal: String },
    Close,
}

// ---------------------------------------------------------------------------
// Output buffer shared between drain task and daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct OutputBuffer {
    pub data: Vec<u8>,
    pub cursor: usize,
}

impl OutputBuffer {
    fn extend(&mut self, new_data: &[u8]) {
        self.data.extend_from_slice(new_data);
        if self.data.len() > MAX_BUFFER {
            let excess = self.data.len() - MAX_BUFFER;
            self.data.drain(..excess);
            self.cursor = self.cursor.saturating_sub(excess);
        }
    }

    fn page(&self, offset: usize, limit: usize, do_strip: bool) -> (String, usize, usize) {
        let start = offset.max(self.cursor);
        if start >= self.data.len() {
            return (String::new(), start, self.data.len());
        }
        let end = (start + limit).min(self.data.len());
        let raw = &self.data[start..end];
        let text = if do_strip {
            strip_ansi(&String::from_utf8_lossy(raw))
        } else {
            String::from_utf8_lossy(raw).into_owned()
        };
        (text, end, self.data.len())
    }
}

// ---------------------------------------------------------------------------
// SessionHandle — the daemon's view of a session
// ---------------------------------------------------------------------------

pub struct SessionHandle {
    pub id: String,
    pub connection_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub cols: u32,
    pub rows: u32,
    pub reconnect: bool,
    pub connect_args: ConnectArgs,
    pub created_at: u64,
    pub updated_at: Arc<Mutex<u64>>,

    // Shared state with drain task
    pub output: Arc<Mutex<OutputBuffer>>,
    pub status: Arc<Mutex<String>>,
    pub exit_status: Arc<Mutex<Option<i32>>>,
    pub exit_signal: Arc<Mutex<Option<String>>>,

    // Command channel to drain task
    pub cmd_tx: mpsc::UnboundedSender<SessionCmd>,

    // Set to true when drain task has exited
    pub drain_dead: Arc<Mutex<bool>>,
}

impl SessionHandle {
    pub fn page_output(&self, offset: usize, limit: usize, do_strip: bool) -> Value {
        let buf = self.output.lock().unwrap();
        let (text, next_offset, total) = buf.page(offset, limit, do_strip);
        json!({"text": text, "offset": next_offset, "total": total})
    }

    pub fn summary(&self, all_sessions: &BTreeMap<String, SessionHandle>) -> Value {
        let buf = self.output.lock().unwrap();
        let status = self.status.lock().unwrap();
        let exit_status = self.exit_status.lock().unwrap();
        let exit_signal = self.exit_signal.lock().unwrap();

        json!({
            "id": self.id,
            "connection_id": self.connection_id,
            "host": self.host,
            "port": self.port,
            "username": self.username,
            "status": *status,
            "output_size": buf.data.len(),
            "exit_status": *exit_status,
            "exit_signal": exit_signal.as_deref().unwrap_or(""),
            "reconnect": self.reconnect,
            "cols": self.cols,
            "rows": self.rows,
            "siblings": shared_with_ids(&self.id, &self.connection_id, all_sessions),
            "created_at": self.created_at,
            "updated_at": *self.updated_at.lock().unwrap(),
        })
    }
}

// ---------------------------------------------------------------------------
// Drain task — owns the Channel, continuously reads PTY output
// ---------------------------------------------------------------------------

pub async fn spawn_drain_task(
    channel: Channel<client::Msg>,
    output: Arc<Mutex<OutputBuffer>>,
    status: Arc<Mutex<String>>,
    exit_status: Arc<Mutex<Option<i32>>>,
    exit_signal: Arc<Mutex<Option<String>>>,
    drain_dead: Arc<Mutex<bool>>,
    updated_at: Arc<Mutex<u64>>,
) -> mpsc::UnboundedSender<SessionCmd> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();

    tokio::spawn(async move {
        *status.lock().unwrap() = "running".to_string();
        let mut channel = channel;

        loop {
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            output.lock().unwrap().extend(&data);
                            *updated_at.lock().unwrap() = now_ms();
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            output.lock().unwrap().extend(&data);
                            *updated_at.lock().unwrap() = now_ms();
                        }
                        Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                            *exit_status.lock().unwrap() = Some(code as i32);
                            *status.lock().unwrap() = "exited".to_string();
                            *updated_at.lock().unwrap() = now_ms();
                        }
                        Some(ChannelMsg::ExitSignal { signal_name, error_message, .. }) => {
                            let sig_str = match &signal_name {
                                russh::Sig::ABRT => "ABRT",
                                russh::Sig::ALRM => "ALRM",
                                russh::Sig::FPE => "FPE",
                                russh::Sig::HUP => "HUP",
                                russh::Sig::ILL => "ILL",
                                russh::Sig::INT => "INT",
                                russh::Sig::KILL => "KILL",
                                russh::Sig::PIPE => "PIPE",
                                russh::Sig::QUIT => "QUIT",
                                russh::Sig::SEGV => "SEGV",
                                russh::Sig::TERM => "TERM",
                                russh::Sig::USR1 => "USR1",
                                russh::Sig::Custom(s) => s,
                            };
                            *exit_signal.lock().unwrap() = Some(sig_str.to_string());
                            *status.lock().unwrap() = "exited".to_string();
                            if !error_message.is_empty() {
                                let msg = format!("\r\n[signal: {}]\r\n", error_message);
                                output.lock().unwrap().extend(msg.as_bytes());
                            }
                            *updated_at.lock().unwrap() = now_ms();
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                            let current = status.lock().unwrap().clone();
                            if current != "exited" {
                                *status.lock().unwrap() = "closed".to_string();
                            }
                            *updated_at.lock().unwrap() = now_ms();
                            break;
                        }
                        None => {
                            let current = status.lock().unwrap().clone();
                            if current == "running" {
                                *status.lock().unwrap() = "closed".to_string();
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SessionCmd::Send { data }) => {
                            if let Err(e) = channel.data(&data[..]).await {
                                *status.lock().unwrap() = format!("error: {}", e);
                                break;
                            }
                        }
                        Some(SessionCmd::Resize { cols, rows }) => {
                            let _ = channel.window_change(cols, rows, 0, 0).await;
                        }
                        Some(SessionCmd::Signal { signal }) => {
                            match signal.as_str() {
                                "INT" => { let _ = channel.data(&b"\x03"[..]).await; }
                                "QUIT" => { let _ = channel.data(&b"\x1c"[..]).await; }
                                "TSTP" => { let _ = channel.data(&b"\x1a"[..]).await; }
                                "TERM" | "KILL" => {
                                    let _ = channel.eof().await;
                                    *status.lock().unwrap() = "closed".to_string();
                                    break;
                                }
                                _ => {}
                            }
                        }
                        Some(SessionCmd::Close) => {
                            let _ = channel.eof().await;
                            *status.lock().unwrap() = "closed".to_string();
                            break;
                        }
                        None => break,
                    }
                }
            }
        }

        *drain_dead.lock().unwrap() = true;
    });

    cmd_tx
}

// ---------------------------------------------------------------------------
// Open a PTY session
// ---------------------------------------------------------------------------

pub async fn open_pty(
    handle: &SharedConnectionHandle,
    cols: u32,
    rows: u32,
) -> Result<Channel<client::Msg>> {
    let channel = handle
        .channel_open_session()
        .await
        .context("opening session channel")?;

    channel
        .request_pty(true, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .context("requesting PTY")?;

    channel
        .request_shell(true)
        .await
        .context("requesting shell")?;

    Ok(channel)
}

// ---------------------------------------------------------------------------
// Daemon session operations
// ---------------------------------------------------------------------------

pub async fn daemon_connect(
    cmd: &ConnectCommand,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let resolved = connection::resolve_connect_args(&cmd.connect)?;
    let (handle, host, port, username) = connection::connect_with_info(&resolved).await?;

    let channel = open_pty(&handle, cmd.cols, cmd.rows).await?;

    let connection_id = {
        let entry = state.next_connection_id;
        state.next_connection_id += 1;
        format!("c{}", entry)
    };

    state.connections.insert(
        connection_id.clone(),
        SharedConnection {
            handle: handle.clone(),
            refcount: 1,
        },
    );

    let session_id = {
        let entry = state.next_id;
        state.next_id += 1;
        format!("s{}", entry)
    };

    let output = Arc::new(Mutex::new(OutputBuffer::default()));
    let status = Arc::new(Mutex::new("running".to_string()));
    let exit_status = Arc::new(Mutex::new(None));
    let exit_signal = Arc::new(Mutex::new(None));
    let drain_dead = Arc::new(Mutex::new(false));
    let updated_at = Arc::new(Mutex::new(now_ms()));

    let cmd_tx = spawn_drain_task(
        channel,
        output.clone(),
        status.clone(),
        exit_status.clone(),
        exit_signal.clone(),
        drain_dead.clone(),
        updated_at.clone(),
    )
    .await;

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
    state.sessions.insert(session_id.clone(), session);

    Ok(json!({"ok": true, "session_id": session_id, "connection_id": connection_id, "session": summary}))
}

pub async fn daemon_spawn(
    cmd: &SpawnCommand,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let source = state
        .sessions
        .get(&cmd.from)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", cmd.from))?;

    let connection_id = source.connection_id.clone();
    let host = source.host.clone();
    let port = source.port;
    let username = source.username.clone();
    let connect_args = source.connect_args.clone();

    let conn = state
        .connections
        .get_mut(&connection_id)
        .ok_or_else(|| anyhow::anyhow!("connection {} not found", connection_id))?;

    let c = cmd.cols;
    let r = cmd.rows;
    let channel = open_pty(&conn.handle, c, r).await?;
    conn.refcount += 1;

    let new_id = {
        let entry = state.next_id;
        state.next_id += 1;
        format!("s{}", entry)
    };

    let output = Arc::new(Mutex::new(OutputBuffer::default()));
    let status = Arc::new(Mutex::new("running".to_string()));
    let exit_status = Arc::new(Mutex::new(None));
    let exit_signal = Arc::new(Mutex::new(None));
    let drain_dead = Arc::new(Mutex::new(false));
    let updated_at = Arc::new(Mutex::new(now_ms()));

    let cmd_tx = spawn_drain_task(
        channel,
        output.clone(),
        status.clone(),
        exit_status.clone(),
        exit_signal.clone(),
        drain_dead.clone(),
        updated_at.clone(),
    )
    .await;

    let session = SessionHandle {
        id: new_id.clone(),
        connection_id: connection_id.clone(),
        host,
        port,
        username,
        cols: c,
        rows: r,
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
    state.sessions.insert(new_id.clone(), session);

    Ok(json!({"ok": true, "session_id": new_id, "connection_id": connection_id, "session": summary}))
}

pub async fn daemon_exec(
    session_id: &str,
    cmd: &str,
    timeout_ms: Option<u64>,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    let conn = state
        .connections
        .get(&session.connection_id)
        .ok_or_else(|| anyhow::anyhow!("connection {} not found", session.connection_id))?;

    let mut channel = conn
        .handle
        .channel_open_session()
        .await
        .context("opening exec channel")?;

    let (stdout, stderr, exit_code) =
        connection::exec_channel(&mut channel, cmd, timeout_ms).await?;

    Ok(json!({"ok": true, "stdout": stdout, "stderr": stderr, "exit_code": exit_code}))
}

pub async fn daemon_send(
    session_id: &str,
    input: &str,
    crlf: bool,
    expect_pairs: Vec<ExpectRespondPair>,
    wait_ms: Option<u64>,
    wait_idle: Option<u64>,
    wait_for_exit: bool,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    let normalized = normalize_input(input, crlf);

    session
        .cmd_tx
        .send(SessionCmd::Send { data: normalized.into_bytes() })
        .map_err(|_| anyhow::anyhow!("drain task dead"))?;

    if let Some(ms) = wait_ms {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    if let Some(idle_ms) = wait_idle {
        tokio::time::sleep(Duration::from_millis(idle_ms)).await;
    }

    for pair in expect_pairs {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let matches = {
                let buf = session.output.lock().unwrap();
                let haystack = String::from_utf8_lossy(&buf.data);
                output_matches_str(&haystack, &pair.expect)?
            };
            if matches {
                let normalized_resp = normalize_input(&pair.respond, true);
                session
                    .cmd_tx
                    .send(SessionCmd::Send { data: normalized_resp.into_bytes() })
                    .map_err(|_| anyhow::anyhow!("drain task dead"))?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("timeout waiting for expect pattern: {}", pair.expect);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    if wait_for_exit {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let done = {
                let st = session.status.lock().unwrap();
                &*st != "running"
            };
            if done { break; }
            if tokio::time::Instant::now() >= deadline { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    let session = state.sessions.get(session_id).unwrap();
    let output_val = session.page_output(0, util::DEFAULT_LIMIT, true);
    let status = session.status.lock().unwrap().clone();
    let exit_status = *session.exit_status.lock().unwrap();
    let exit_signal = session.exit_signal.lock().unwrap().clone();

    Ok(daemon_output_response(session_id, &output_val, &status, exit_status, exit_signal))
}

pub async fn daemon_read(
    session_id: &str,
    offset: usize,
    limit: usize,
    wait_ms: Option<u64>,
    strip: Option<bool>,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    if let Some(ms) = wait_ms {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    let do_strip = strip.unwrap_or(true);
    let output_val = session.page_output(offset, limit, do_strip);
    let status = session.status.lock().unwrap().clone();
    let exit_status = *session.exit_status.lock().unwrap();
    let exit_signal = session.exit_signal.lock().unwrap().clone();

    Ok(daemon_output_response(session_id, &output_val, &status, exit_status, exit_signal))
}

pub async fn daemon_resize(
    session_id: &str,
    cols: u32,
    rows: u32,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    session.cols = cols;
    session.rows = rows;
    session
        .cmd_tx
        .send(SessionCmd::Resize { cols, rows })
        .map_err(|_| anyhow::anyhow!("drain task dead"))?;

    Ok(json!({"ok": true, "cols": cols, "rows": rows}))
}

pub async fn daemon_signal(
    session_id: &str,
    signal: &str,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    let sig = signal.to_uppercase();
    session
        .cmd_tx
        .send(SessionCmd::Signal { signal: sig.clone() })
        .map_err(|_| anyhow::anyhow!("drain task dead"))?;

    Ok(json!({"ok": true, "signal": sig}))
}

pub async fn daemon_close(
    session_id: &str,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    let connection_id = session.connection_id.clone();
    let _ = session.cmd_tx.send(SessionCmd::Close);

    state.sessions.remove(session_id);

    if let Some(conn) = state.connections.get_mut(&connection_id) {
        conn.refcount = conn.refcount.saturating_sub(1);
        if conn.refcount == 0 {
            state.connections.remove(&connection_id);
        }
    }

    Ok(json!({"ok": true, "closed": session_id}))
}

pub async fn daemon_status(
    session_id: &str,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    Ok(json!({"ok": true, "session": session.summary(&state.sessions)}))
}

pub async fn daemon_ping(
    session_id: &str,
    state: &mut crate::kernel::ServerState,
) -> Result<Value> {
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id))?;

    let alive = !*session.drain_dead.lock().unwrap()
        && *session.status.lock().unwrap() == "running";

    Ok(json!({"ok": true, "alive": alive, "session_id": session_id}))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn normalize_input(input: &str, crlf: bool) -> String {
    if crlf {
        input.replace('\n', "\r\n")
    } else {
        input.to_string()
    }
}

pub fn shared_with_ids(
    session_id: &str,
    connection_id: &str,
    all_sessions: &BTreeMap<String, SessionHandle>,
) -> Vec<String> {
    all_sessions
        .iter()
        .filter(|(id, s)| *id != session_id && s.connection_id == connection_id)
        .map(|(id, _)| id.clone())
        .collect()
}

pub fn daemon_output_response(
    session_id: &str,
    output: &Value,
    status: &str,
    exit_status: Option<i32>,
    exit_signal: Option<String>,
) -> Value {
    json!({
        "ok": true,
        "session_id": session_id,
        "output": output,
        "status": status,
        "exit_status": exit_status,
        "exit_signal": exit_signal.unwrap_or_default(),
    })
}

fn output_matches_str(haystack: &str, pattern: &str) -> Result<bool> {
    use regex::RegexBuilder;
    let haystack_lower = haystack.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let regex = RegexBuilder::new(pattern).case_insensitive(true).build();
    match regex {
        Ok(compiled) => Ok(compiled.is_match(haystack) || haystack_lower.contains(&pattern_lower)),
        Err(_) => Ok(haystack_lower.contains(&pattern_lower)),
    }
}
