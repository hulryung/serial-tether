//! Serial Tether wire protocol — JSON-RPC 2.0 over NDJSON.
//!
//! See `docs/PROTOCOL.md` for the full specification.

pub mod codec;
pub mod error;
pub mod message;

pub use codec::NdjsonCodec;
pub use error::{ErrorCode, ProtocolError};
pub use message::{
    AckResult, AttachParams, AttachResult, BufferInfo, CancelParams, CancelResult, ClientInfo,
    ConnectDeviceResult, DataNotify, DetachParams, DeviceInfo, DeviceNotify, DeviceStatus,
    DeviceSummary, DeviceTarget, DisconnectDeviceResult, ExpectMatch, ExpectParams, ExpectResult,
    HelloParams, HelloResult, LagNotify, ListDevicesResult, ListPortsResult, LockNotify, Message,
    Notification, PortInfo, ReadModemStatusResult, ReconnectParams, ReconnectResult, Request,
    Response, RunParams, RunResult, SendBreakParams, SendParams, SendResult, SessionInfo,
    SessionNotify, SetDeviceParams, SetDeviceResult, SetLineParams, StatusResult, UntilSpec,
};

pub const PROTOCOL_VERSION: &str = "1";
