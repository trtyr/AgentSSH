use crate::cli::ConnectArgs;
use crate::connection::SharedConnectionHandle;
use crate::util::{now_ms, strip_ansi};
use anyhow::{Context, Result};
use russh::{client, Channel, ChannelMsg};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Session status — type-safe enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Running,
    Exited,
    Closed,
    Disconnected,
    Error(String),
}

impl serde::Serialize for SessionStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Exited => write!(f, "exited"),
            Self::Closed => write!(f, "closed"),
            Self::Disconnected => write!(f, "disconnected"),
            Self::Error(msg) => write!(f, "error: {}", msg),
        }
    }
}

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
    pub fn extend(&mut self, new_data: &[u8]) {
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
    pub status: Arc<Mutex<SessionStatus>>,
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
            "status": (*status).to_string(),
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
    status: Arc<Mutex<SessionStatus>>,
    exit_status: Arc<Mutex<Option<i32>>>,
    exit_signal: Arc<Mutex<Option<String>>>,
    drain_dead: Arc<Mutex<bool>>,
    updated_at: Arc<Mutex<u64>>,
) -> mpsc::UnboundedSender<SessionCmd> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();

    tokio::spawn(async move {
        *status.lock().unwrap() = SessionStatus::Running;
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
                            *status.lock().unwrap() = SessionStatus::Exited;
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
                            *status.lock().unwrap() = SessionStatus::Exited;
                            if !error_message.is_empty() {
                                let msg = format!("\r\n[signal: {}]\r\n", error_message);
                                output.lock().unwrap().extend(msg.as_bytes());
                            }
                            *updated_at.lock().unwrap() = now_ms();
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                            let current = status.lock().unwrap().clone();
                            if current != SessionStatus::Exited {
                                *status.lock().unwrap() = SessionStatus::Closed;
                            }
                            *updated_at.lock().unwrap() = now_ms();
                            break;
                        }
                        None => {
                            let current = status.lock().unwrap().clone();
                            if current == SessionStatus::Running {
                                *status.lock().unwrap() = SessionStatus::Closed;
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
                                *status.lock().unwrap() = SessionStatus::Error(e.to_string());
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
                                    *status.lock().unwrap() = SessionStatus::Closed;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        Some(SessionCmd::Close) => {
                            let _ = channel.eof().await;
                            *status.lock().unwrap() = SessionStatus::Closed;
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
        && *session.status.lock().unwrap() == SessionStatus::Running;

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
    status: &SessionStatus,
    exit_status: Option<i32>,
    exit_signal: Option<String>,
) -> Value {
    json!({
        "ok": true,
        "session_id": session_id,
        "output": output,
        "status": status.to_string(),
        "exit_status": exit_status,
        "exit_signal": exit_signal.unwrap_or_default(),
    })
}

pub(crate) fn output_matches_str(haystack: &str, pattern: &str) -> Result<bool> {
    use regex::RegexBuilder;
    let haystack_lower = haystack.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let regex = RegexBuilder::new(pattern).case_insensitive(true).build();
    match regex {
        Ok(compiled) => Ok(compiled.is_match(haystack) || haystack_lower.contains(&pattern_lower)),
        Err(_) => Ok(haystack_lower.contains(&pattern_lower)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Display trait tests
    #[test]
    fn session_status_display_running() {
        assert_eq!(SessionStatus::Running.to_string(), "running");
    }

    #[test]
    fn session_status_display_exited() {
        assert_eq!(SessionStatus::Exited.to_string(), "exited");
    }

    #[test]
    fn session_status_display_closed() {
        assert_eq!(SessionStatus::Closed.to_string(), "closed");
    }

    #[test]
    fn session_status_display_disconnected() {
        assert_eq!(SessionStatus::Disconnected.to_string(), "disconnected");
    }

    #[test]
    fn session_status_display_error() {
        assert_eq!(SessionStatus::Error("timeout".into()).to_string(), "error: timeout");
    }

    // Serialize tests — JSON output must be plain string, not object
    #[test]
    fn session_status_serialize_running() {
        let json = serde_json::to_string(&SessionStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");
    }

    #[test]
    fn session_status_serialize_error() {
        let json = serde_json::to_string(&SessionStatus::Error("killed".into())).unwrap();
        assert_eq!(json, "\"error: killed\"");
    }

    // Deserialize tests — roundtrip
    #[test]
    fn session_status_deserialize_roundtrip() {
        for status in [SessionStatus::Running, SessionStatus::Exited, SessionStatus::Closed, SessionStatus::Disconnected] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, status);
        }
    }

    // PartialEq
    #[test]
    fn session_status_equality() {
        assert_eq!(SessionStatus::Running, SessionStatus::Running);
        assert_ne!(SessionStatus::Running, SessionStatus::Exited);
        assert_eq!(SessionStatus::Error("x".into()), SessionStatus::Error("x".into()));
        assert_ne!(SessionStatus::Error("x".into()), SessionStatus::Error("y".into()));
    }

    // Clone
    #[test]
    fn session_status_clone() {
        let original = SessionStatus::Error("test".into());
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }
}
