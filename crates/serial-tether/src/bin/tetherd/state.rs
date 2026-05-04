use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::buffer::RingBuffer;
use crate::serial::{SerialConfig, SerialWriter};
use crate::session::SessionManager;

/// Liveness state of the underlying serial device. The serial task is the
/// sole writer; other code reads it (for the `status` RPC, the banner, etc.).
#[derive(Debug, Clone)]
pub struct DeviceState {
    /// True when the daemon currently holds the device open.
    pub connected: bool,
    /// Last successful open. None until the first open.
    pub last_open_at: Option<Instant>,
    /// Last disconnect/EOF/error reason. None while connected.
    pub last_disconnect_reason: Option<String>,
    /// Cumulative count of disconnects since daemon start (for diagnostics).
    pub disconnect_count: u64,
    /// Consecutive open failures during the current outage. Resets on success.
    pub consecutive_open_failures: u64,
}

impl DeviceState {
    pub fn new_disconnected() -> Self {
        Self {
            connected: false,
            last_open_at: None,
            last_disconnect_reason: None,
            disconnect_count: 0,
            consecutive_open_failures: 0,
        }
    }
}

/// Device-state transition broadcast across all connections, surfaced to
/// clients as the `device` notification (see PROTOCOL.md §7.5).
#[derive(Clone, Debug)]
pub struct DeviceEvent {
    pub kind: DeviceEventKind,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum DeviceEventKind {
    Disconnected,
    Reconnected,
}

impl DeviceEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Reconnected => "reconnected",
        }
    }
}

/// Single writer lock used by `run` transactions.
/// `holder == None` means the lock is free; `Some(id)` means session `id`
/// owns it.
#[derive(Default)]
pub struct WriterLock {
    pub holder: Mutex<Option<String>>,
    pub released: Notify,
}

#[derive(Clone)]
pub struct DaemonState {
    pub buffer: RingBuffer,
    pub writer: SerialWriter,
    pub sessions: Arc<SessionManager>,
    pub config: SerialConfig,
    pub lock: Arc<WriterLock>,
    /// Required by clients connecting over a transport with `requires_auth=true`
    /// (i.e., TCP). `None` means TCP listening is disabled.
    pub auth_token: Option<Arc<String>>,
    /// Live tracking of whether the serial device is currently open. The
    /// serial task updates this; everyone else just reads.
    pub device_state: Arc<Mutex<DeviceState>>,
    /// Notify the serial task to drop the current device handle and reopen
    /// (used by the `reconnect` RPC).
    pub force_reconnect: Arc<Notify>,
    /// Notify when device_state transitions to connected (used by `reconnect`
    /// to wait for the reopen to complete).
    pub reconnected: Arc<Notify>,
    /// Broadcast channel for device-state transitions. Each connection
    /// subscribes once and emits `device` notifications to its clients.
    pub device_events: broadcast::Sender<DeviceEvent>,
}
