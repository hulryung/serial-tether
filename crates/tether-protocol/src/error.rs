use serde::{Deserialize, Serialize};

/// JSON-RPC standard codes plus tetherd-specific extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,

    Timeout = -32001,
    SessionNotAttached = -32002,
    ModeViolation = -32003,
    LockContention = -32004,
    DeviceDisconnected = -32005,
    BufferOverflow = -32006,

    UnsupportedProtocol = -32010,
    NotInitialized = -32011,
    SessionNotFound = -32012,
    ModeConflict = -32013,

    Cancelled = -32800,
}

impl ErrorCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::ParseError => "parse error",
            Self::InvalidRequest => "invalid request",
            Self::MethodNotFound => "method not found",
            Self::InvalidParams => "invalid params",
            Self::InternalError => "internal error",
            Self::Timeout => "timeout",
            Self::SessionNotAttached => "session not attached",
            Self::ModeViolation => "mode violation",
            Self::LockContention => "lock contention",
            Self::DeviceDisconnected => "device disconnected",
            Self::BufferOverflow => "buffer overflow",
            Self::UnsupportedProtocol => "unsupported protocol",
            Self::NotInitialized => "not initialized",
            Self::SessionNotFound => "session not found",
            Self::ModeConflict => "mode conflict",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ProtocolError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code: code.as_i32(),
            message: code.message().to_string(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_message(mut self, msg: impl Into<String>) -> Self {
        self.message = msg.into();
        self
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ProtocolError {}
