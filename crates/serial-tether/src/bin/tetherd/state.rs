use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::buffer::RingBuffer;
use crate::serial::{SerialConfig, SerialControl, SerialWriter};
use crate::session::SessionManager;

/// Liveness state of an underlying serial device. The serial task is the
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
    /// Set by `disconnect_device`; the serial owner task closes the port
    /// and stops auto-reconnecting until `connect_device` clears it.
    pub explicitly_disconnected: bool,
}

impl DeviceState {
    pub fn new_disconnected() -> Self {
        Self {
            connected: false,
            last_open_at: None,
            last_disconnect_reason: None,
            disconnect_count: 0,
            consecutive_open_failures: 0,
            explicitly_disconnected: false,
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
    /// Live serial settings changed (set_device). The device wasn't dropped;
    /// `detail` carries a short summary of the new configuration for the UI.
    ConfigChanged,
}

impl DeviceEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Reconnected => "reconnected",
            Self::ConfigChanged => "config_changed",
        }
    }
}

/// Static per-device console personality, set once from the `-D` spec and
/// surfaced in `status` / `list_devices`. It never changes at runtime, so it
/// lives outside the config mutex. Drives client-side behaviour: how `exec`
/// frames commands and picks a line terminator, and the default `-u` prompt
/// for `run` / `sync`.
#[derive(Clone, Debug)]
pub struct ConsolePersonality {
    /// `"posix"` | `"uboot"` | `"none"`. Validated at `-D` parse time.
    pub shell: String,
    /// Default `-u` prompt regex for `run` / `sync`; `None` if unset.
    pub prompt: Option<String>,
    /// Default line terminator (`"lf"` | `"cr"` | `"crlf"` | `"none"`); `None`
    /// if unset. `uboot` defaults this to `"cr"` unless overridden.
    pub newline: Option<String>,
}

impl Default for ConsolePersonality {
    fn default() -> Self {
        Self {
            shell: "posix".into(),
            prompt: None,
            newline: None,
        }
    }
}

/// A held writer lock: who holds it, and whether the hold is *exclusive*.
///
/// `run` takes a transient, non-exclusive hold for the duration of its
/// send+expect transaction — this only serialises against other sessions'
/// `run`/`lock` calls, matching pre-v0.11 behaviour so existing scripts that
/// interleave `send` with a concurrent `run` don't regress. The explicit
/// `lock` RPC takes an exclusive hold, which additionally gates plain `send`
/// (see `send` in handlers.rs) — that's what makes `lock` safe to flash
/// under.
#[derive(Clone, Debug)]
pub struct LockHold {
    pub session_id: String,
    pub exclusive: bool,
}

/// Per-device writer lock used by `run` transactions and the explicit
/// `lock`/`unlock` RPCs.
/// `holder == None` means the lock is free; `Some(hold)` means session
/// `hold.session_id` owns it. Each `Device` has its own lock — concurrent
/// transactions on different devices never serialise against each other.
#[derive(Default)]
pub struct WriterLock {
    pub holder: Mutex<Option<LockHold>>,
    pub released: Notify,
}

/// One serial port owned by the daemon. All per-device state (ring buffer,
/// writer, serial-control mpsc, config, liveness, lock, event broadcast)
/// lives here. `DaemonState` holds a `HashMap<id, Arc<Device>>` so handlers
/// can route by `device_id`.
pub struct Device {
    /// Operator-chosen alias (e.g. "board0"). Used in CLI flags, log lines,
    /// and as the key in `DaemonState.devices`.
    pub id: String,
    pub buffer: RingBuffer,
    pub writer: SerialWriter,
    pub serial_control: SerialControl,
    /// The serial owner task is the authoritative writer; RPC handlers read
    /// (status/hello) and propose changes via `serial_control`.
    pub config: Arc<Mutex<SerialConfig>>,
    pub state: Arc<Mutex<DeviceState>>,
    pub force_reconnect: Arc<Notify>,
    pub reconnected: Arc<Notify>,
    pub events: broadcast::Sender<DeviceEvent>,
    pub lock: Arc<WriterLock>,
    /// Console personality (shell/prompt/newline) from the `-D` spec. Static.
    pub console: ConsolePersonality,
}

#[derive(Clone)]
pub struct DaemonState {
    /// All devices the daemon currently owns. Static after startup in v0.8;
    /// hot add/remove may come in a later release.
    pub devices: Arc<HashMap<String, Arc<Device>>>,
    /// Device id picked when a client omits `device_id`. With multiple
    /// devices the handler returns `AmbiguousDevice` instead of silently
    /// using this; `default_device` only resolves when devices.len() == 1.
    pub default_device: String,
    pub sessions: Arc<SessionManager>,
    /// Required by clients connecting over a transport with `requires_auth=true`
    /// (i.e., TCP). `None` means TCP listening is disabled.
    pub auth_token: Option<Arc<String>>,
}

impl DaemonState {
    /// Resolve a target device given an optional `device_id` from the wire.
    ///
    /// - `Some(id)` → look it up; `DeviceNotFound` if missing.
    /// - `None` and exactly one device → that device.
    /// - `None` and multiple devices → `AmbiguousDevice` error.
    pub fn resolve_device(
        &self,
        id: Option<&str>,
    ) -> Result<Arc<Device>, tether_protocol::ProtocolError> {
        use tether_protocol::error::ErrorCode;
        use tether_protocol::ProtocolError;
        if let Some(name) = id {
            return self.devices.get(name).cloned().ok_or_else(|| {
                ProtocolError::new(ErrorCode::DeviceNotFound)
                    .with_message(format!("device {name:?} not managed by this daemon"))
            });
        }
        if self.devices.len() == 1 {
            return Ok(self
                .devices
                .get(&self.default_device)
                .expect("default device always present in devices map")
                .clone());
        }
        Err(ProtocolError::new(ErrorCode::AmbiguousDevice).with_message(format!(
            "daemon serves {} devices; pass device_id to select one",
            self.devices.len()
        )))
    }

    /// Look up a session and return both the session handle and the device
    /// it's bound to. Used by `send`/`expect`/`run` (which take a session_id
    /// rather than a device_id).
    pub fn device_for_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<Device>, tether_protocol::ProtocolError> {
        use tether_protocol::error::ErrorCode;
        use tether_protocol::ProtocolError;
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| ProtocolError::new(ErrorCode::SessionNotFound))?;
        let device_id = session.lock().device_id.clone();
        self.devices.get(&device_id).cloned().ok_or_else(|| {
            ProtocolError::new(ErrorCode::DeviceNotFound)
                .with_message(format!("session bound to missing device {device_id:?}"))
        })
    }
}
