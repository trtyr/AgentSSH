use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
// Listener
// ---------------------------------------------------------------------------

pub(crate) async fn run_proxy_listener(
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
    let status_str = match proxy.status.try_lock() {
        Ok(s) => match &*s {
            ProxyStatus::Running => "running".to_string(),
            ProxyStatus::Stopped => "stopped".to_string(),
            ProxyStatus::Error(e) => e.clone(),
        },
        Err(_) => "unknown".to_string(),
    };
    Ok(json!({"ok": true, "proxy_id": proxy_id, "status": status_str}))
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

    #[test]
    fn test_proxy_ping_running() {
        let mut proxies = BTreeMap::new();
        proxies.insert(
            "p1".to_string(),
            ProxyState {
                id: "p1".into(),
                connection_id: "c1".into(),
                mode: ProxyMode::Socks5 { local_addr: "127.0.0.1:1080".into() },
                local_addr: "127.0.0.1:1080".into(),
                status: Arc::new(Mutex::new(ProxyStatus::Running)),
                shutdown: Arc::new(AtomicBool::new(false)),
            },
        );
        let result = proxy_ping("p1", &proxies).unwrap();
        assert_eq!(result["status"], "running");
        assert_eq!(result["proxy_id"], "p1");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn test_proxy_ping_stopped() {
        let mut proxies = BTreeMap::new();
        proxies.insert(
            "p1".to_string(),
            ProxyState {
                id: "p1".into(),
                connection_id: "c1".into(),
                mode: ProxyMode::Socks5 { local_addr: "127.0.0.1:1080".into() },
                local_addr: "127.0.0.1:1080".into(),
                status: Arc::new(Mutex::new(ProxyStatus::Stopped)),
                shutdown: Arc::new(AtomicBool::new(false)),
            },
        );
        let result = proxy_ping("p1", &proxies).unwrap();
        assert_eq!(result["status"], "stopped");
    }

    #[test]
    fn test_proxy_ping_error() {
        let mut proxies = BTreeMap::new();
        proxies.insert(
            "p1".to_string(),
            ProxyState {
                id: "p1".into(),
                connection_id: "c1".into(),
                mode: ProxyMode::Socks5 { local_addr: "127.0.0.1:1080".into() },
                local_addr: "127.0.0.1:1080".into(),
                status: Arc::new(Mutex::new(ProxyStatus::Error("bind failed".into()))),
                shutdown: Arc::new(AtomicBool::new(false)),
            },
        );
        let result = proxy_ping("p1", &proxies).unwrap();
        assert_eq!(result["status"], "bind failed");
    }

    #[test]
    fn test_proxy_ping_not_found() {
        let proxies = BTreeMap::new();
        let err = proxy_ping("nonexistent", &proxies).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_proxy_ping_lock_contended() {
        let mut proxies = BTreeMap::new();
        let status = Arc::new(Mutex::new(ProxyStatus::Running));
        proxies.insert(
            "p1".to_string(),
            ProxyState {
                id: "p1".into(),
                connection_id: "c1".into(),
                mode: ProxyMode::Socks5 { local_addr: "127.0.0.1:1080".into() },
                local_addr: "127.0.0.1:1080".into(),
                status: status.clone(),
                shutdown: Arc::new(AtomicBool::new(false)),
            },
        );
        // Hold the lock to simulate contention — try_lock() should return Err
        let _held = status.lock().await;
        let result = proxy_ping("p1", &proxies).unwrap();
        assert_eq!(result["status"], "unknown");
    }
}
