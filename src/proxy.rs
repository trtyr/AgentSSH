use crate::cli::{ProxyCloseCommand, ProxyCreateCommand, ProxyPingCommand};
use crate::connection::{SharedConnection, connect_with_info, next_connection_id};
use crate::kernel::ServerState;
use crate::util::{log_daemon, sleep_ms};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use ssh2::Session;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};

const PROXY_POLL_MS: u64 = 25;
const SOCKS5_VERSION: u8 = 5;
const SOCKS5_METHOD_NO_AUTH: u8 = 0;
const SOCKS5_CMD_CONNECT: u8 = 1;
const SOCKS5_ATYP_IPV4: u8 = 1;
const SOCKS5_ATYP_DOMAIN: u8 = 3;
const SOCKS5_ATYP_IPV6: u8 = 4;
const SOCKS5_REPLY_SUCCEEDED: u8 = 0;
const SOCKS5_REPLY_COMMAND_NOT_SUPPORTED: u8 = 7;
const SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 8;
const SOCKS5_REPLY_GENERAL_FAILURE: u8 = 1;

#[derive(Clone, Debug)]
pub enum ProxyMode {
    LocalForward { remote_host: String, remote_port: u16 },
    Socks5,
}

#[derive(Clone, Debug)]
pub enum ProxyStatus {
    Running,
    Stopped,
    Error(String),
}

pub struct ProxyState {
    pub id: String,
    pub connection_id: String,
    pub mode: ProxyMode,
    pub local_addr: String,
    pub status: ProxyStatus,
    pub shutdown: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
}

pub fn daemon_proxy_create(command: ProxyCreateCommand, state: &mut ServerState) -> Result<Value> {
    let (mode, requested_bind_addr) = proxy_mode_from_command(&command)?;
    let listener = TcpListener::bind(&requested_bind_addr)
        .with_context(|| format!("bind proxy listener at {requested_bind_addr}"))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set nonblocking listener {requested_bind_addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("read bound proxy listener address")?
        .to_string();

    let (ssh, host, port, username) = connect_with_info(command.connect)?;
    ssh.set_blocking(false);

    let connection_id = next_connection_id(state);
    let session_for_thread = ssh.clone();
    state.connections.insert(
        connection_id.clone(),
        SharedConnection {
            ssh,
            refcount: 1,
        },
    );

    state.next_id += 1;
    let proxy_id = format!("p{}", state.next_id);
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let thread_mode = mode.clone();
    let thread_id = proxy_id.clone();
    let thread_addr = local_addr.clone();

    let thread = thread::spawn(move || {
        let result = run_proxy_listener(listener, thread_addr.clone(), thread_mode, session_for_thread, thread_shutdown);
        match result {
            Ok(()) => {
                let _ = log_daemon(&format!("proxy {thread_id} listener stopped"));
            }
            Err(error) => {
                let _ = log_daemon(&format!("proxy {thread_id} listener error: {error:#}"));
            }
        }
    });

    let summary = json!({
        "proxy_id": proxy_id,
        "connection_id": connection_id,
        "local_addr": local_addr,
        "mode": proxy_mode_name(&mode),
        "status": proxy_status_name(&ProxyStatus::Running),
    });

    state.proxies.insert(
        proxy_id.clone(),
        ProxyState {
            id: proxy_id.clone(),
            connection_id: connection_id.clone(),
            mode,
            local_addr: local_addr.clone(),
            status: ProxyStatus::Running,
            shutdown,
            thread: Some(thread),
        },
    );

    let _ = log_daemon(&format!(
        "proxy {proxy_id} created on {local_addr} via {host}:{port} as {username}"
    ));
    Ok(summary)
}

pub fn daemon_proxy_list(state: &mut ServerState) -> Result<Value> {
    let proxies = state
        .proxies
        .values_mut()
        .map(|proxy| {
            refresh_proxy_status(proxy);
            proxy_summary(proxy)
        })
        .collect::<Vec<_>>();
    Ok(json!({ "proxies": proxies }))
}

pub fn daemon_proxy_ping(command: ProxyPingCommand, state: &mut ServerState) -> Result<Value> {
    let proxy = state
        .proxies
        .get_mut(&command.proxy_id)
        .ok_or_else(|| anyhow!("proxy '{}' not found. Use 'agentssh proxy list' to see active proxies.", command.proxy_id))?;
    refresh_proxy_status(proxy);
    let alive = proxy_alive(proxy);
    let status = match &proxy.status {
        ProxyStatus::Running => "running",
        ProxyStatus::Stopped => "stopped",
        ProxyStatus::Error(msg) => msg.as_str(),
    };
    Ok(json!({
        "proxy_id": proxy.id,
        "alive": alive,
        "status": status,
        "local_addr": proxy.local_addr,
        "mode": proxy_mode_name(&proxy.mode),
    }))
}

pub fn daemon_proxy_close(command: ProxyCloseCommand, state: &mut ServerState) -> Result<Value> {
    if command.all {
        let proxy_ids = state.proxies.keys().cloned().collect::<Vec<_>>();
        let mut closed = Vec::with_capacity(proxy_ids.len());
        for proxy_id in proxy_ids {
            closed.push(close_proxy(&proxy_id, state)?);
        }
        return Ok(json!({ "closed": closed }));
    }

    let proxy_id = command
        .proxy_id
        .ok_or_else(|| anyhow!("provide --proxy-id or --all"))?;
    close_proxy(&proxy_id, state)
}

fn close_proxy(proxy_id: &str, state: &mut ServerState) -> Result<Value> {
    let mut proxy = state
        .proxies
        .remove(proxy_id)
        .ok_or_else(|| anyhow!("proxy '{}' not found. Use 'agentssh proxy list' to see active proxies.", proxy_id))?;

    proxy.shutdown.store(true, Ordering::Relaxed);
    if let Some(thread) = proxy.thread.take() {
        let _ = thread.join();
    }

    if let Some(connection) = state.connections.get_mut(&proxy.connection_id) {
        connection.refcount = connection.refcount.saturating_sub(1);
        if connection.refcount == 0 {
            state.connections.remove(&proxy.connection_id);
        }
    }

    proxy.status = ProxyStatus::Stopped;
    let _ = log_daemon(&format!("proxy {} closed", proxy.id));
    Ok(json!({
        "proxy_id": proxy.id,
        "local_addr": proxy.local_addr,
        "mode": proxy_mode_name(&proxy.mode),
        "status": "closed",
    }))
}

fn run_proxy_listener(
    listener: TcpListener,
    local_addr: String,
    mode: ProxyMode,
    session: Session,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer_addr)) => {
                let connection_mode = mode.clone();
                let connection_session = session.clone();
                let connection_shutdown = shutdown.clone();
                let listener_addr = local_addr.clone();
                thread::spawn(move || {
                    let result = handle_proxy_connection(stream, connection_session, connection_mode, connection_shutdown);
                    if let Err(error) = result {
                        let _ = log_daemon(&format!(
                            "proxy connection on {listener_addr} from {peer_addr} failed: {error:#}"
                        ));
                    }
                });
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => sleep_ms(PROXY_POLL_MS),
            Err(error) => return Err(error).with_context(|| format!("accept proxy connection on {local_addr}")),
        }
    }
    Ok(())
}

fn handle_proxy_connection(
    mut stream: TcpStream,
    session: Session,
    mode: ProxyMode,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let is_socks5 = matches!(&mode, ProxyMode::Socks5);
    let (target_host, target_port) = match mode {
        ProxyMode::LocalForward {
            remote_host,
            remote_port,
        } => (remote_host, remote_port),
        ProxyMode::Socks5 => perform_socks5_handshake(&mut stream)?,
    };

    session.set_blocking(true);
    let mut channel = match session.channel_direct_tcpip(&target_host, target_port, None) {
        Ok(channel) => channel,
        Err(error) => {
            if is_socks5 {
                let _ = write_socks5_reply(&mut stream, SOCKS5_REPLY_GENERAL_FAILURE);
            }
            return Err(error).with_context(|| format!("open direct-tcpip channel to {target_host}:{target_port}"));
        }
    };

    if is_socks5 {
        write_socks5_reply(&mut stream, SOCKS5_REPLY_SUCCEEDED)?;
    }

    session.set_blocking(false);
    forward_data(&mut stream, &mut channel, &shutdown)?;
    let _ = channel.close();
    let _ = channel.wait_close();
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn forward_data(stream: &mut TcpStream, channel: &mut ssh2::Channel, shutdown: &Arc<AtomicBool>) -> Result<()> {
    stream
        .set_nonblocking(true)
        .context("set local proxy stream nonblocking")?;

    let mut local_closed = false;
    let mut remote_closed = false;
    let mut local_buf = [0_u8; 8192];
    let mut remote_buf = [0_u8; 8192];

    while !shutdown.load(Ordering::Relaxed) {
        let mut progressed = false;

        if !local_closed {
            match stream.read(&mut local_buf) {
                Ok(0) => {
                    local_closed = true;
                    let _ = channel.send_eof();
                }
                Ok(read_bytes) => {
                    write_all_channel(channel, &local_buf[..read_bytes], shutdown)?;
                    progressed = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("read from local proxy client"),
            }
        }

        if !remote_closed {
            match channel.read(&mut remote_buf) {
                Ok(0) => {
                    remote_closed = true;
                    let _ = stream.shutdown(Shutdown::Write);
                }
                Ok(read_bytes) => {
                    write_all_stream(stream, &remote_buf[..read_bytes], shutdown)?;
                    progressed = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("read from SSH direct-tcpip channel"),
            }
        }

        if channel.eof() {
            remote_closed = true;
        }

        if local_closed && remote_closed {
            break;
        }

        if !progressed {
            sleep_ms(PROXY_POLL_MS);
        }
    }

    Ok(())
}

fn write_all_channel(channel: &mut ssh2::Channel, mut bytes: &[u8], shutdown: &Arc<AtomicBool>) -> Result<()> {
    while !bytes.is_empty() && !shutdown.load(Ordering::Relaxed) {
        match channel.write(bytes) {
            Ok(0) => bail!("SSH channel write returned 0 bytes"),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => sleep_ms(PROXY_POLL_MS),
            Err(error) => return Err(error).context("write to SSH direct-tcpip channel"),
        }
    }
    Ok(())
}

fn write_all_stream(stream: &mut TcpStream, mut bytes: &[u8], shutdown: &Arc<AtomicBool>) -> Result<()> {
    while !bytes.is_empty() && !shutdown.load(Ordering::Relaxed) {
        match stream.write(bytes) {
            Ok(0) => bail!("local proxy stream write returned 0 bytes"),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => sleep_ms(PROXY_POLL_MS),
            Err(error) => return Err(error).context("write to local proxy client"),
        }
    }
    Ok(())
}

fn perform_socks5_handshake(stream: &mut TcpStream) -> Result<(String, u16)> {
    stream
        .set_nonblocking(false)
        .context("set stream blocking for SOCKS5 handshake")?;

    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).context("read SOCKS5 greeting header")?;
    if greeting[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS version {}", greeting[0]);
    }

    let mut methods = vec![0_u8; greeting[1] as usize];
    stream.read_exact(&mut methods).context("read SOCKS5 auth methods")?;
    if !methods.contains(&SOCKS5_METHOD_NO_AUTH) {
        stream.write_all(&[SOCKS5_VERSION, 0xff]).context("write SOCKS5 no-method reply")?;
        bail!("SOCKS5 client does not support no-auth method");
    }
    stream
        .write_all(&[SOCKS5_VERSION, SOCKS5_METHOD_NO_AUTH])
        .context("write SOCKS5 auth selection")?;

    let mut request = [0_u8; 4];
    stream.read_exact(&mut request).context("read SOCKS5 request header")?;
    if request[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS request version {}", request[0]);
    }
    if request[1] != SOCKS5_CMD_CONNECT {
        write_socks5_reply(stream, SOCKS5_REPLY_COMMAND_NOT_SUPPORTED)?;
        bail!("unsupported SOCKS5 command {}", request[1]);
    }

    let host = match request[3] {
        SOCKS5_ATYP_IPV4 => {
            let mut octets = [0_u8; 4];
            stream.read_exact(&mut octets).context("read SOCKS5 IPv4 address")?;
            Ipv4Addr::from(octets).to_string()
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len = [0_u8; 1];
            stream.read_exact(&mut len).context("read SOCKS5 domain length")?;
            let mut bytes = vec![0_u8; len[0] as usize];
            stream.read_exact(&mut bytes).context("read SOCKS5 domain")?;
            String::from_utf8(bytes).context("decode SOCKS5 domain as UTF-8")?
        }
        SOCKS5_ATYP_IPV6 => {
            let mut octets = [0_u8; 16];
            stream.read_exact(&mut octets).context("read SOCKS5 IPv6 address")?;
            Ipv6Addr::from(octets).to_string()
        }
        atyp => {
            write_socks5_reply(stream, SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED)?;
            bail!("unsupported SOCKS5 address type {atyp}");
        }
    };

    let mut port_bytes = [0_u8; 2];
    stream.read_exact(&mut port_bytes).context("read SOCKS5 target port")?;
    let port = u16::from_be_bytes(port_bytes);
    Ok((host, port))
}

fn write_socks5_reply(stream: &mut TcpStream, reply_code: u8) -> Result<()> {
    stream
        .write_all(&[SOCKS5_VERSION, reply_code, 0, SOCKS5_ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .context("write SOCKS5 reply")
}

fn proxy_mode_from_command(command: &ProxyCreateCommand) -> Result<(ProxyMode, String)> {
    match (&command.local, &command.remote, &command.socks5) {
        (Some(local_addr), Some(remote_addr), None) => {
            let (remote_host, remote_port) = parse_host_port(remote_addr)?;
            Ok((
                ProxyMode::LocalForward {
                    remote_host,
                    remote_port,
                },
                local_addr.clone(),
            ))
        }
        (None, None, Some(socks5_addr)) => Ok((ProxyMode::Socks5, socks5_addr.clone())),
        (None, None, None) => bail!("provide either --local/--remote or --socks5"),
        (Some(_), None, None) => bail!("--remote is required when using --local"),
        _ => bail!("--local and --socks5 are mutually exclusive"),
    }
}

fn parse_host_port(input: &str) -> Result<(String, u16)> {
    if let Some(rest) = input.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| anyhow!("invalid address '{input}'. Expected [host]:port"))?;
        return Ok((host.to_string(), parse_port(port, input)?));
    }

    let (host, port) = input
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid address '{input}'. Expected host:port"))?;
    if host.is_empty() {
        bail!("invalid address '{input}'. Host must not be empty");
    }
    Ok((host.to_string(), parse_port(port, input)?))
}

fn parse_port(port: &str, original: &str) -> Result<u16> {
    port.parse::<u16>()
        .with_context(|| format!("invalid port in address '{original}'"))
}

fn proxy_summary(proxy: &ProxyState) -> Value {
    json!({
        "proxy_id": proxy.id,
        "connection_id": proxy.connection_id,
        "local_addr": proxy.local_addr,
        "mode": proxy_mode_name(&proxy.mode),
        "status": if proxy_alive(proxy) { "running" } else { proxy_status_name(&proxy.status) },
    })
}

fn proxy_alive(proxy: &ProxyState) -> bool {
    !proxy.shutdown.load(Ordering::Relaxed)
        && proxy
            .thread
            .as_ref()
            .map(|thread| !thread.is_finished())
            .unwrap_or(false)
}

fn refresh_proxy_status(proxy: &mut ProxyState) {
    if matches!(proxy.status, ProxyStatus::Error(_)) {
        return;
    }
    let shutdown_set = proxy.shutdown.load(Ordering::Relaxed);
    let thread_finished = proxy
        .thread
        .as_ref()
        .map(|thread| thread.is_finished())
        .unwrap_or(false);
    if !shutdown_set && thread_finished {
        proxy.status = ProxyStatus::Error("listener thread exited unexpectedly".to_string());
    }
}

fn proxy_mode_name(mode: &ProxyMode) -> &'static str {
    match mode {
        ProxyMode::LocalForward { .. } => "local",
        ProxyMode::Socks5 => "socks5",
    }
}

fn proxy_status_name(status: &ProxyStatus) -> &str {
    match status {
        ProxyStatus::Running => "running",
        ProxyStatus::Stopped => "stopped",
        ProxyStatus::Error(_) => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_supports_bracketed_ipv6() {
        let parsed = parse_host_port("[2001:db8::1]:8080").expect("ipv6 address should parse");
        assert_eq!(parsed, ("2001:db8::1".to_string(), 8080));
    }

    #[test]
    fn proxy_mode_from_command_requires_remote_for_local_forwarding() {
        let error = proxy_mode_from_command(&ProxyCreateCommand {
            connect: Default::default(),
            local: Some("127.0.0.1:9999".to_string()),
            remote: None,
            socks5: None,
        })
        .expect_err("missing remote should fail");

        assert!(error.to_string().contains("--remote"));
    }

    #[test]
    fn proxy_summary_reports_running_listener() {
        let proxy = ProxyState {
            id: "p1".to_string(),
            connection_id: "c1".to_string(),
            mode: ProxyMode::Socks5,
            local_addr: "127.0.0.1:1080".to_string(),
            status: ProxyStatus::Running,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        };

        let summary = proxy_summary(&proxy);
        assert_eq!(summary["proxy_id"], "p1");
        assert_eq!(summary["mode"], "socks5");
    }
}
