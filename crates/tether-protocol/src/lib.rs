//! Serial Tether wire protocol — JSON-RPC 2.0 over NDJSON.
//!
//! See `docs/PROTOCOL.md` for the full specification.

pub mod codec;
pub mod error;
pub mod message;

pub use codec::NdjsonCodec;
pub use error::{ErrorCode, ProtocolError};
pub use message::{
    AttachParams, AttachResult, BufferInfo, CancelParams, CancelResult, ClientInfo, DataNotify,
    DetachParams, DeviceInfo, DeviceNotify, ExpectMatch, ExpectParams, ExpectResult, HelloParams,
    HelloResult, LagNotify, LockNotify, Message, Notification, Request, Response, RunParams,
    RunResult, SendParams, SendResult, SessionInfo, SessionNotify, StatusResult, UntilSpec,
};

pub const PROTOCOL_VERSION: &str = "1";
