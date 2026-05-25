use crate::cli::ConnectArgs;
use crate::kernel::ServerState;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Proxy types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ProxyMode {
    LocalForward { local_addr: String, remote_host: String, remote_port: u16 },
    Socks5 { local_addr: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub status: Arc<Mutex<ProxyStatus>>,
    pub shutdown: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------------
// Create proxy
// ---------------------------------------------------------------------------

pub async fn daemon_proxy_create(
    args: &ConnectArgs,
    proxy_mode: &ProxyMode,
    state: &mut ServerState,
) -> Result<Value> {
    let resolved = crate::connection::resolve_connect_args(args)?;
    let (handle, _host, _port, _username) = crate::connection::connect_with_info(&resolved).await?;

    let connection_id = {
        let entry = state.next_connection_id;
        state.next_connection_id += 1;
        format!("c{}", entry)
    };

    state.connections.insert(
        connection_id.clone(),
        crate::connection::SharedConnection {
            handle: handle.clone(),
            refcount: 1,
        },
    );

    // Determine local bind address
    let local_addr = match proxy_mode {
        ProxyMode::LocalForward { local_addr, .. } => local_addr.clone(),
        ProxyMode::Socks5 { local_addr } => local_addr.clone(),
    };

    let local_addr_parsed: SocketAddr = local_addr
        .parse()
        .with_context(|| format!("parsing local address: {}", local_addr))?;

    let listener = TcpListener::bind(local_addr_parsed)
        .await
        .with_context(|| format!("binding to {}", local_addr))?;

    let proxy_id = {
        let entry = state.next_id;
        state.next_id += 1;
        format!("p{}", entry)
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(ProxyStatus::Running));
    let mode = proxy_mode.clone();

    // Spawn listener task
    let shutdown_clone = shutdown.clone();
    let status_clone = status.clone();
    let proxy_id_clone = proxy_id.clone();
    let handle_clone = handle;
    let listener_addr = local_addr.clone();

    tokio::spawn(async move {
        run_proxy_listener(
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

    let proxy = ProxyState {
        id: proxy_id.clone(),
        connection_id: connection_id.clone(),
        mode: proxy_mode.clone(),
        local_addr: local_addr.clone(),
        status,
        shutdown,
    };

    state.proxies.insert(proxy_id.clone(), proxy);

    Ok(json!({
        "ok": true,
        "proxy_id": proxy_id,
        "connection_id": connection_id,
        "local_addr": local_addr,
        "mode": serde_json::to_value(proxy_mode)?,
    }))
}

async fn run_proxy_listener(
    listener: TcpListener,
    _local_addr: &str,
    mode: &ProxyMode,
    handle: Arc<crate::connection::SharedConnectionHandle>,
    shutdown: Arc<AtomicBool>,
    status: Arc<Mutex<ProxyStatus>>,
    proxy_id: &str,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            *status.lock().await = ProxyStatus::Stopped;
            break;
        }

        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _peer)) => {
                        let mode = mode.clone();
                        let handle = handle.clone();
                        let shutdown = shutdown.clone();
                        let proxy_id = proxy_id.to_string();

                        tokio::spawn(async move {
                            if let Err(e) = handle_proxy_connection(
                                stream, &mode, &handle, &shutdown, &proxy_id,
                            )
                            .await
                            {
                                let _ = crate::util::log_daemon(&format!(
                                    "proxy {} connection error: {}",
                                    proxy_id, e
                                ));
                            }
                        });
                    }
                    Err(e) => {
                        *status.lock().await = ProxyStatus::Error(e.to_string());
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                // Periodic shutdown check
            }
        }
    }
}

async fn handle_proxy_connection(
    mut stream: TcpStream,
    mode: &ProxyMode,
    handle: &crate::connection::SharedConnectionHandle,
    shutdown: &AtomicBool,
    _proxy_id: &str,
) -> Result<()> {
    let (remote_host, remote_port) = match mode {
        ProxyMode::LocalForward { remote_host, remote_port, .. } => {
            (remote_host.clone(), *remote_port)
        }
        ProxyMode::Socks5 { .. } => {
            // Perform SOCKS5 handshake
            perform_socks5_handshake(&mut stream).await?
        }
    };

    if shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Open direct-tcpip channel to remote target
    let channel = handle
        .channel_open_direct_tcpip(
            &remote_host,
            remote_port.into(),
            "127.0.0.1",
            0,
        )
        .await
        .with_context(|| {
            format!(
                "opening direct-tcpip to {}:{}",
                remote_host, remote_port
            )
        })?;

    // Bidirectional forwarding via channel.into_stream()
    let mut channel_stream = channel.into_stream();
    let _ = io::copy_bidirectional(&mut stream, &mut channel_stream).await;

    Ok(())
}

// ---------------------------------------------------------------------------
// SOCKS5 handshake (RFC 1928)
// ---------------------------------------------------------------------------

async fn perform_socks5_handshake(stream: &mut TcpStream) -> Result<(String, u16)> {
    // Read version + nmethods
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    if buf[0] != 0x05 {
        bail!("not a SOCKS5 connection (version: {})", buf[0]);
    }
    let nmethods = buf[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    // Reply: version 5, no-auth (0x00)
    stream.write_all(&[0x05, 0x00]).await?;

    // Read connect request
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 0x05 {
        bail!("SOCKS5: expected version 5, got {}", header[0]);
    }
    if header[1] != 0x01 {
        bail!("SOCKS5: only CONNECT (0x01) supported, got {}", header[1]);
    }

    let target = match header[3] {
        // IPv4
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let ip = std::net::Ipv4Addr::from(addr);
            (ip.to_string(), port)
        }
        // Domain
        0x03 => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let domain_len = len_buf[0] as usize;
            let mut domain = vec![0u8; domain_len];
            stream.read_exact(&mut domain).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let domain_str = String::from_utf8_lossy(&domain).into_owned();
            (domain_str, port)
        }
        // IPv6
        0x04 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            let mut port_buf = [0u8; 2];
            stream.read_exact(&mut port_buf).await?;
            let port = u16::from_be_bytes(port_buf);
            let ip = std::net::Ipv6Addr::from(addr);
            (ip.to_string(), port)
        }
        atype => {
            bail!("SOCKS5: unsupported address type: {}", atype);
        }
    };

    // Reply: success
    stream
        .write_all(&[
            0x05, // version
            0x00, // success
            0x00, // reserved
            0x01, // IPv4
            0, 0, 0, 0, // bind addr
            0, 0, // bind port
        ])
        .await?;

    Ok(target)
}

// ---------------------------------------------------------------------------
// Proxy list / ping / close
// ---------------------------------------------------------------------------

pub fn proxy_list(proxies: &BTreeMap<String, ProxyState>) -> Value {
    let list: Vec<Value> = proxies
        .iter()
        .map(|(_, p)| {
            // Try to get status — if we can't lock, assume running
            let status_str = match p.status.try_lock() {
                Ok(s) => match &*s {
                    ProxyStatus::Running => "running".to_string(),
                    ProxyStatus::Stopped => "stopped".to_string(),
                    ProxyStatus::Error(e) => format!("error: {}", e),
                },
                Err(_) => "running".to_string(),
            };
            json!({
                "id": p.id,
                "connection_id": p.connection_id,
                "mode": serde_json::to_value(&p.mode).unwrap_or_default(),
                "local_addr": p.local_addr,
                "status": status_str,
            })
        })
        .collect();
    json!({"ok": true, "proxies": list})
}

pub fn proxy_ping(proxy_id: &str, proxies: &BTreeMap<String, ProxyState>) -> Result<Value> {
    let proxy = proxies
        .get(proxy_id)
        .ok_or_else(|| anyhow::anyhow!("proxy {} not found", proxy_id))?;
    let status_str = match &*proxy.status.blocking_lock() {
        ProxyStatus::Running => "running".to_string(),
        ProxyStatus::Stopped => "stopped".to_string(),
        ProxyStatus::Error(e) => e.clone(),
    };
    Ok(json!({"ok": true, "proxy_id": proxy_id, "status": status_str}))
}

pub async fn proxy_close(
    proxy_id: &str,
    all: bool,
    state: &mut ServerState,
) -> Result<Value> {
    let ids_to_close: Vec<String> = if all {
        state.proxies.keys().cloned().collect()
    } else {
        vec![proxy_id.to_string()]
    };

    let mut closed = Vec::new();
    for id in ids_to_close {
        if let Some(proxy) = state.proxies.remove(&id) {
            proxy.shutdown.store(true, Ordering::Relaxed);
            *proxy.status.lock().await = ProxyStatus::Stopped;

            // Decrement connection refcount
            if let Some(conn) = state.connections.get_mut(&proxy.connection_id) {
                conn.refcount = conn.refcount.saturating_sub(1);
                if conn.refcount == 0 {
                    state.connections.remove(&proxy.connection_id);
                }
            }

            closed.push(id);
        }
    }

    Ok(json!({"ok": true, "closed": closed}))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn proxy_mode_from_command(
    local: Option<&str>,
    remote: Option<&str>,
    socks5: Option<&str>,
) -> Result<ProxyMode> {
    match (local, remote, socks5) {
        (Some(local_addr), Some(remote_str), None) => {
            let (remote_host, remote_port) = parse_host_port(remote_str)?;
            Ok(ProxyMode::LocalForward {
                local_addr: local_addr.to_string(),
                remote_host,
                remote_port,
            })
        }
        (None, None, Some(socks5)) => Ok(ProxyMode::Socks5 { local_addr: socks5.to_string() }),
        _ => {
            bail!("specify --local + --remote for port forwarding, or --socks5 for SOCKS5 proxy")
        }
    }
}

pub fn parse_host_port(s: &str) -> Result<(String, u16)> {
    let parts: Vec<&str> = s.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        bail!("invalid host:port format: {}", s);
    }
    let port: u16 = parts[0]
        .parse()
        .with_context(|| format!("invalid port: {}", parts[0]))?;
    let host = parts[1].to_string();
    Ok((host, port))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host_port() {
        let (host, port) = parse_host_port("127.0.0.1:8080").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);

        let (host, port) = parse_host_port("example.com:22").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 22);
    }

    #[test]
    fn test_proxy_mode_from_command() {
        let mode = proxy_mode_from_command(
            Some("127.0.0.1:9999"),
            Some("10.0.0.1:80"),
            None,
        )
        .unwrap();
        match mode {
            ProxyMode::LocalForward { local_addr, remote_host, remote_port } => {
                assert_eq!(local_addr, "127.0.0.1:9999");
                assert_eq!(remote_host, "10.0.0.1");
                assert_eq!(remote_port, 80);
            }
            _ => panic!("expected LocalForward"),
        }

        let mode = proxy_mode_from_command(None, None, Some("127.0.0.1:1080")).unwrap();
        match mode {
            ProxyMode::Socks5 { local_addr } => assert_eq!(local_addr, "127.0.0.1:1080"),
            _ => panic!("expected Socks5"),
        }
    }

    #[test]
    fn test_proxy_summary() {
        let mut proxies = BTreeMap::new();
        let summary = proxy_list(&proxies);
        assert_eq!(summary["proxies"].as_array().unwrap().len(), 0);
    }
}
