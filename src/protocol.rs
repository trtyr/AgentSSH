use crate::cli::{
    ConnectCommand, ListCommand, ProxyCloseCommand, ProxyCreateCommand, ProxyPingCommand,
    ReadCommand, ResizeCommand, SessionExecCommand, SessionInputCommand, SignalCommand,
    SpawnCommand, StatusCommand, TransferCommand, WriteCommand,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
pub struct WireResponse {
    pub ok: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WireRequest {
    Connect(ConnectCommand),
    Send(SessionInputCommand),
    Spawn(SpawnCommand),
    Exec(SessionExecCommand),
    Read(ReadCommand),
    Resize(ResizeCommand),
    Signal(SignalCommand),
    Status(StatusCommand),
    Ping(StatusCommand),
    List,
    Close(StatusCommand),
    Upload(TransferCommand),
    Download(TransferCommand),
    Ls(ListCommand),
    Write(WriteCommand),
    ProxyCreate(ProxyCreateCommand),
    ProxyList,
    ProxyClose(ProxyCloseCommand),
    ProxyPing(ProxyPingCommand),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_proxy_list_request_with_action_tag() {
        let value = serde_json::to_value(WireRequest::ProxyList).expect("proxy list should serialize");
        assert_eq!(value.get("action").and_then(Value::as_str), Some("proxy_list"));
    }

    #[test]
    fn serializes_proxy_close_request_with_payload() {
        let value = serde_json::to_value(WireRequest::ProxyClose(crate::cli::ProxyCloseCommand {
            proxy_id: Some("p1".to_string()),
            all: false,
        }))
        .expect("proxy close should serialize");

        assert_eq!(value.get("action").and_then(Value::as_str), Some("proxy_close"));
        assert_eq!(value.get("proxy_id").and_then(Value::as_str), Some("p1"));
        assert_eq!(value.get("all").and_then(Value::as_bool), Some(false));
    }
}
