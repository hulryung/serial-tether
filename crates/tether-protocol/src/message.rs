use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProtocolError;

/// JSON-RPC 2.0 id — integer or string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    Text(String),
}

/// One message per line — exactly one of Request / Response / Notification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    pub id: RpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: JsonRpcVersion,
    pub id: RpcId,
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    Ok { result: Value },
    Err { error: ProtocolError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Always serializes as the literal string "2.0".
#[derive(Debug, Clone, Default)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: String = String::deserialize(d)?;
        if v == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!("expected jsonrpc \"2.0\", got {v:?}")))
        }
    }
}

// ---------- Method parameters / results ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
    /// "human" | "agent" | "logger"
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloParams {
    pub protocol_version: String,
    pub client: ClientInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub path: String,
    pub baud: u32,
    #[serde(default = "default_data_bits")]
    pub data_bits: u8,
    #[serde(default = "default_parity")]
    pub parity: String,
    #[serde(default = "default_stop_bits")]
    pub stop_bits: u8,
    #[serde(default = "default_serial_flow_control")]
    pub flow_control: String,
    #[serde(default = "default_connected")]
    pub connected: bool,
}

fn default_data_bits() -> u8 { 8 }
fn default_parity() -> String { "none".into() }
fn default_stop_bits() -> u8 { 1 }
fn default_serial_flow_control() -> String { "none".into() }
fn default_connected() -> bool { true }

// ---------- list_ports ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortInfo {
    pub path: String,
    /// "usb" | "pci" | "bluetooth" | "unknown"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    /// USB Vendor ID, lowercase 4-hex (e.g. "10c4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vid: Option<String>,
    /// USB Product ID, lowercase 4-hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPortsResult {
    pub ports: Vec<PortInfo>,
}

// ---------- set_device ----------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetDeviceParams {
    /// New baud rate (e.g. 9600, 115200, 921600). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<u32>,
    /// 5..=8. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_bits: Option<u8>,
    /// "none" | "odd" | "even". Optional. (mark/space not supported by serialport-rs.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parity: Option<String>,
    /// 1 or 2. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_bits: Option<u8>,
    /// "none" | "software" | "hardware". Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_control: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetDeviceResult {
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferInfo {
    pub capacity_bytes: u64,
    pub head_seq: u64,
    pub tail_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    pub server_version: String,
    pub protocol_version: String,
    pub device: DeviceInfo,
    pub buffer: BufferInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReplaySpec {
    Named(String),                // "start" | "now"
    FromSeq { seq: u64 },
}

impl Default for ReplaySpec {
    fn default() -> Self { Self::Named("now".into()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub mode: String,             // "rw" | "ro"
    #[serde(default)]
    pub replay: ReplaySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "default_flow_control")]
    pub flow_control: String,     // "drop_oldest" | "disconnect"
}

fn default_flow_control() -> String { "drop_oldest".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session_id: String,
    pub cursor_seq: u64,
    #[serde(default)]
    pub restored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendParams {
    pub session_id: String,
    /// Exactly one of `data` (base64) or `data_text` (UTF-8) is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_text: Option<String>,
    #[serde(default)]
    pub eat_echo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub bytes_written: u64,
    pub sent_at_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectParams {
    pub session_id: String,
    pub pattern: String,
    #[serde(default = "default_true")]
    pub regex: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    #[serde(default)]
    pub strip_ansi: bool,
    /// Strip these bytes (CR/LF tolerantly) from the start of `before` when matched.
    /// Typically you pass the bytes you just sent to remove the echoed command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_echo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    /// Maximum bytes that may accumulate while waiting for a match. Exceeding this
    /// raises BufferOverflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Maximum size of the `before` field returned in the response. When exceeded,
    /// only the trailing N bytes are returned and `truncated` is set. `None` = no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectMatch {
    pub matched: bool,
    #[serde(rename = "match")]
    pub match_text: String,
    /// Bytes accumulated up to the match, base64-encoded. Reflects strip_ansi / strip_echo.
    pub before: String,
    pub match_seq: u64,
    pub end_seq: u64,
    /// True when `before` was capped by `max_output_bytes`.
    #[serde(default)]
    pub truncated: bool,
    /// Original length before truncation (only set when `truncated` is true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bytes: Option<u64>,
}

pub type ExpectResult = ExpectMatch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UntilSpec {
    pub pattern: String,
    #[serde(default = "default_true")]
    pub regex: bool,
    #[serde(default)]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_text: Option<String>,
    pub until: UntilSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    #[serde(default = "default_preempt")]
    pub preempt: String,          // "queue" | "fail" | "force"
    /// Strip the echoed command line from `before` (for devices that echo input).
    #[serde(default = "default_true")]
    pub strip_echo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
}

fn default_preempt() -> String { "queue".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub output: String,           // base64
    #[serde(rename = "match")]
    pub match_text: String,
    pub duration_ms: u64,
    pub match_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelParams {
    pub id: RpcId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResult {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub mode: String,
    pub cursor_seq: u64,
    pub lag_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquired_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub device: DeviceInfo,
    pub buffer: BufferInfo,
    pub lock: LockState,
    pub sessions: Vec<SessionInfo>,
}

// ---------- Notification parameters ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataNotify {
    pub session_id: String,
    pub seq: u64,
    pub data: String,             // base64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LagNotify {
    pub session_id: String,
    pub dropped_bytes: u64,
    pub dropped_range: [u64; 2],
    pub resume_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockNotify {
    pub kind: String,             // "acquired" | "released" | "queued" | "preempted"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_session_id: Option<String>,
    #[serde(default)]
    pub queue_depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionNotify {
    pub id: String,
    pub kind: String,             // "attached" | "detached" | "preempted"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceNotify {
    pub kind: String,             // "disconnected" | "reconnected" | "config_changed"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ---------- Helpers ----------

impl Request {
    pub fn new(id: RpcId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            method: method.into(),
            params: Some(params),
        }
    }
}

impl Response {
    pub fn ok(id: RpcId, result: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            payload: ResponsePayload::Ok { result },
        }
    }

    pub fn err(id: RpcId, error: ProtocolError) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            id,
            payload: ResponsePayload::Err { error },
        }
    }
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion,
            method: method.into(),
            params: Some(params),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let r = Request::new(
            RpcId::Number(1),
            "hello",
            serde_json::json!({"protocol_version":"1"}),
        );
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        let _: Request = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn response_ok_roundtrip() {
        let r = Response::ok(RpcId::Number(1), serde_json::json!({"x":1}));
        let s = serde_json::to_string(&r).unwrap();
        let parsed: Response = serde_json::from_str(&s).unwrap();
        match parsed.payload {
            ResponsePayload::Ok { .. } => (),
            _ => panic!("expected ok"),
        }
    }

    #[test]
    fn response_err_roundtrip() {
        use crate::error::{ErrorCode, ProtocolError};
        let r = Response::err(RpcId::Number(2), ProtocolError::new(ErrorCode::Timeout));
        let s = serde_json::to_string(&r).unwrap();
        let parsed: Response = serde_json::from_str(&s).unwrap();
        match parsed.payload {
            ResponsePayload::Err { error } => assert_eq!(error.code, -32001),
            _ => panic!("expected err"),
        }
    }

    #[test]
    fn message_dispatch() {
        let line = r#"{"jsonrpc":"2.0","method":"data","params":{"session_id":"x","seq":1,"data":"YWI="}}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        match m {
            Message::Notification(n) => assert_eq!(n.method, "data"),
            _ => panic!("expected notification"),
        }
    }
}
