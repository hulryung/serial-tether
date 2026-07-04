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
    /// Daemon-internal device id (operator-chosen, e.g. `"board0"`). Absent
    /// in single-device daemons that pre-date multi-device support; new
    /// daemons always populate this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Console personality: `"posix"` | `"uboot"` | `"none"` (see the `-D
    /// shell=` device setting). Drives how `exec` frames commands and what
    /// hints the client prints. Defaults to `"posix"` for daemons that
    /// pre-date the setting.
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Default `-u` prompt regex for `run` / `sync` when the caller omits
    /// `-u`. `None` means the caller must supply one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Default line terminator (`"lf"` | `"cr"` | `"crlf"` | `"none"`) applied
    /// by `run` when `--newline` is not given. `None` means no per-device
    /// default (the command's own default applies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newline: Option<String>,
}

/// Compact summary used in `list_devices` and the new `StatusResult.devices`
/// field. Mirrors `DeviceInfo` but drops the redundant defaults so the list
/// stays small when N is large.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub id: String,
    pub path: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: u8,
    pub flow_control: String,
    pub connected: bool,
    /// True when the operator (or `disconnect_device` RPC) has explicitly
    /// closed the port; auto-reconnect is paused until `connect_device`.
    #[serde(default)]
    pub explicitly_disconnected: bool,
    /// Console personality: `"posix"` | `"uboot"` | `"none"`. See
    /// [`DeviceInfo::shell`].
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Default `-u` prompt regex for `run` / `sync`. See
    /// [`DeviceInfo::prompt`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Default line terminator for `run`. See [`DeviceInfo::newline`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDevicesResult {
    pub devices: Vec<DeviceSummary>,
    /// The daemon's "default" device — the one selected when a client omits
    /// `device_id`. Only meaningful when `devices.len() == 1`; for N>1
    /// daemons, clients must specify `device_id` explicitly.
    pub default_device: String,
}

fn default_data_bits() -> u8 { 8 }
fn default_shell() -> String { "posix".into() }
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
    /// Which device to apply to. Optional in single-device daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
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

// ---------- Reconnect ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconnectParams {
    /// Optional device target. Required on multi-device daemons (>1 device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Block until the device reopens (default true).
    #[serde(default = "default_true")]
    pub wait: bool,
    /// How long to wait before giving up. Default 5000.
    #[serde(default = "default_reconnect_timeout_ms")]
    pub timeout_ms: u32,
}

fn default_reconnect_timeout_ms() -> u32 { 5000 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectResult {
    pub triggered: bool,
    pub reconnected: bool,
    pub device_connected: bool,
}

// ---------- Tio-style line / break / modem control (since v0.8) ----------

/// Parameters that just identify a target device. Used by the simpler
/// per-device control RPCs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// Empty acknowledgement returned by control RPCs that don't carry a value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AckResult {
    /// Always `true` on success; carries no extra info today, but having a
    /// non-empty struct lets us add fields later without breaking clients.
    #[serde(default = "default_true")]
    pub ok: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendBreakParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Break duration in milliseconds. Defaults to 250 (matches tio).
    #[serde(default = "default_break_ms")]
    pub duration_ms: u32,
}

fn default_break_ms() -> u32 { 250 }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetLineParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// `true` asserts the line; `false` deasserts.
    pub on: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadModemStatusResult {
    /// Clear To Send.
    pub cts: bool,
    /// Data Set Ready.
    pub dsr: bool,
    /// Ring Indicator.
    pub ri: bool,
    /// Data Carrier Detect.
    pub dcd: bool,
}

// ---------- Explicit connect / disconnect (since v0.8) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisconnectDeviceResult {
    pub device: DeviceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectDeviceResult {
    pub device: DeviceInfo,
    /// True if the port was successfully reopened. Mirrors the existing
    /// `reconnect` semantics; `device.connected` reflects the final state.
    pub connected: bool,
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
    /// Which daemon-managed device to attach to. Single-device daemons may
    /// omit this. Multi-device daemons return `AmbiguousDevice` when
    /// missing and the daemon serves >1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
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

// ---------- Explicit writer lock (since v0.11) ----------
//
// Unlike the transient hold `run` takes internally (released the instant its
// send+expect transaction completes), `lock` grants *exclusive* possession of
// the device's writer lock that persists until `unlock`, session detach, or
// connection teardown. Exclusive holds additionally gate plain `send` from
// other sessions — see PROTOCOL.md's writer-lock section.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// "queue" | "fail" | "force". Same semantics as `RunParams::preempt`.
    #[serde(default = "default_preempt")]
    pub preempt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockResult {
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnlockParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnlockResult {
    pub unlocked: bool,
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
    /// Default device's info, kept for v0.7 backwards compatibility.
    /// Multi-device daemons populate this with the device tagged as default.
    pub device: DeviceInfo,
    /// Default device's buffer state — same backwards-compat reasoning.
    pub buffer: BufferInfo,
    pub lock: LockState,
    pub sessions: Vec<SessionInfo>,
    /// Per-device snapshot for multi-device daemons. Single-device daemons
    /// populate this with one entry (the same device as `device` above).
    /// Absent on pre-v0.8 daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceStatus>,
    /// Daemon-internal id of the default device. Absent on pre-v0.8 daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_device: Option<String>,
}

/// One row of `StatusResult.devices` — same structure as `device + buffer +
/// lock + sessions`, scoped to a single device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub id: String,
    pub device: DeviceInfo,
    pub buffer: BufferInfo,
    pub lock: LockState,
    pub sessions: Vec<SessionInfo>,
    /// True when the operator has explicitly disconnected this device
    /// (via `disconnect_device`); auto-reconnect is paused.
    #[serde(default)]
    pub explicitly_disconnected: bool,
}

// ---------- Notification parameters ----------
//
// Every notification gained an optional `device_id` field in v0.8 so
// multi-device daemons can route them. Single-device daemons may omit it
// (clients ignore unknown / absent fields per the JSON-RPC etiquette).

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataNotify {
    pub session_id: String,
    pub seq: u64,
    pub data: String,             // base64
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LagNotify {
    pub session_id: String,
    pub dropped_bytes: u64,
    pub dropped_range: [u64; 2],
    pub resume_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockNotify {
    pub kind: String,             // "acquired" | "released" | "queued" | "preempted"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_session_id: Option<String>,
    #[serde(default)]
    pub queue_depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionNotify {
    pub id: String,
    pub kind: String,             // "attached" | "detached" | "preempted"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceNotify {
    pub kind: String,             // "disconnected" | "reconnected" | "config_changed"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
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
