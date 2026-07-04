//! JSON-RPC method handlers.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use tether_protocol::message::LockState;
use tether_protocol::{
    AckResult, AttachParams, AttachResult, BufferInfo, ConnectDeviceResult, DetachParams,
    DeviceInfo, DeviceSummary, DeviceTarget, DisconnectDeviceResult, ExpectMatch, ExpectParams,
    HelloParams, HelloResult, ListDevicesResult, ListPortsResult, LockParams, LockResult,
    PortInfo, ProtocolError, ReadModemStatusResult, RunParams, SendBreakParams, SendParams,
    SendResult, SetDeviceParams, SetDeviceResult, SetLineParams, StatusResult, UnlockParams,
    UnlockResult,
};
use tether_protocol::error::ErrorCode;

use crate::conn::ConnState;
use crate::serial::{
    DataBits as SerialDataBits, FlowControl as SerialFlow, Parity as SerialParity, SerialConfig,
    StopBits as SerialStopBits,
};
use crate::session::{FlowControl, SessionMode};
use crate::state::DaemonState;

/// Read a snapshot of the shared serial config and convert it into a wire
/// `DeviceInfo`. Used by `hello`, `status`, and `set_device`. Phase 2 will
/// thread the device id through here; for now it stays None.
fn device_info_from(
    cfg: &SerialConfig,
    connected: bool,
    console: &crate::state::ConsolePersonality,
) -> DeviceInfo {
    DeviceInfo {
        path: cfg.path.clone(),
        baud: cfg.baud,
        data_bits: cfg.data_bits.as_u8(),
        parity: cfg.parity.as_str().into(),
        stop_bits: cfg.stop_bits.as_u8(),
        flow_control: cfg.flow_control.as_str().into(),
        connected,
        id: None,
        shell: console.shell.clone(),
        prompt: console.prompt.clone(),
        newline: console.newline.clone(),
    }
}

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn err(code: ErrorCode) -> ProtocolError {
    ProtocolError::new(code)
}
fn err_with(code: ErrorCode, msg: impl Into<String>) -> ProtocolError {
    ProtocolError::new(code).with_message(msg)
}

fn parse_params<T: serde::de::DeserializeOwned>(p: Option<Value>) -> Result<T, ProtocolError> {
    let v = p.unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| err_with(ErrorCode::InvalidParams, e.to_string()))
}

/// Map an io::Error from the serial writer into the right protocol error.
/// `NotConnected` is the precise kind we set in `serial.rs` while waiting
/// for the device to reopen, so it maps to the public DeviceDisconnected
/// code instead of generic InternalError.
fn writer_error_to_proto(e: std::io::Error) -> ProtocolError {
    if e.kind() == std::io::ErrorKind::NotConnected {
        ProtocolError::new(ErrorCode::DeviceDisconnected).with_message(e.to_string())
    } else {
        err_with(ErrorCode::InternalError, e.to_string())
    }
}

fn b64_decode(s: &str) -> Result<Vec<u8>, ProtocolError> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| err_with(ErrorCode::InvalidParams, format!("base64: {e}")))
}
fn b64_encode(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub async fn hello(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: HelloParams = parse_params(params)?;
    if p.protocol_version != tether_protocol::PROTOCOL_VERSION {
        return Err(err_with(
            ErrorCode::UnsupportedProtocol,
            format!(
                "server supports {}, got {}",
                tether_protocol::PROTOCOL_VERSION,
                p.protocol_version
            ),
        ));
    }
    if conn.requires_auth() {
        let expected = state.auth_token.as_deref().map(String::as_str);
        let supplied = p.auth_token.as_deref();
        let ok = matches!(
            (expected, supplied),
            (Some(e), Some(g)) if constant_time_eq(e.as_bytes(), g.as_bytes())
        );
        if !ok {
            return Err(err_with(
                ErrorCode::Unauthorized,
                "auth token missing or incorrect",
            ));
        }
    }
    conn.set_initialized(p.client.kind.clone());
    // For multi-device daemons: hello returns the *default* device's info
    // for backwards compatibility. v0.7 clients see the same shape they
    // always did. v0.8 clients should call `list_devices` to enumerate.
    let dev = state
        .devices
        .get(&state.default_device)
        .expect("default device always present");
    let (head, tail) = dev.buffer.snapshot_seqs();
    let device_connected = dev.state.lock().connected;
    let mut device = device_info_from(&dev.config.lock(), device_connected, &dev.console);
    device.id = Some(dev.id.clone());
    let result = HelloResult {
        server_version: SERVER_VERSION.to_string(),
        protocol_version: tether_protocol::PROTOCOL_VERSION.to_string(),
        device,
        buffer: BufferInfo {
            capacity_bytes: dev.buffer.capacity() as u64,
            head_seq: head,
            tail_seq: tail,
        },
    };
    Ok(serde_json::to_value(result).unwrap())
}

pub async fn attach(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    if !conn.is_initialized() {
        return Err(err(ErrorCode::NotInitialized));
    }
    let p: AttachParams = parse_params(params)?;
    let mode = SessionMode::parse(&p.mode)
        .ok_or_else(|| err_with(ErrorCode::InvalidParams, format!("bad mode: {}", p.mode)))?;
    let flow = FlowControl::parse(&p.flow_control).ok_or_else(|| {
        err_with(
            ErrorCode::InvalidParams,
            format!("bad flow_control: {}", p.flow_control),
        )
    })?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    let (head, tail) = device.buffer.snapshot_seqs();
    let cursor = match p.replay {
        tether_protocol::message::ReplaySpec::Named(s) if s == "start" => tail,
        tether_protocol::message::ReplaySpec::Named(s) if s == "now" => head,
        tether_protocol::message::ReplaySpec::Named(s) => {
            return Err(err_with(
                ErrorCode::InvalidParams,
                format!("bad replay: {s}"),
            ))
        }
        tether_protocol::message::ReplaySpec::FromSeq { seq } => seq,
    };
    if p.session_id.is_some() {
        return Err(err_with(
            ErrorCode::SessionNotFound,
            "session restore not supported in v0",
        ));
    }
    let session = state
        .sessions
        .create(device.id.clone(), mode, flow, p.label.clone(), cursor);
    let id = session.lock().id.clone();
    conn.add_session(id.clone());
    let result = AttachResult {
        session_id: id,
        cursor_seq: cursor,
        restored: false,
    };
    Ok(serde_json::to_value(result).unwrap())
}

pub async fn detach(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: DetachParams = parse_params(params)?;
    if !conn.has_session(&p.session_id) {
        return Err(err(ErrorCode::SessionNotAttached));
    }
    // Release any writer lock this session held — otherwise an exclusive
    // lock taken via `lock` would outlive the session with no way to `unlock`
    // it.
    release_lock_for_session(state, &p.session_id);
    state.sessions.remove(&p.session_id);
    conn.remove_session(&p.session_id);
    Ok(json!({}))
}

pub async fn send(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: SendParams = parse_params(params)?;
    let bytes = collect_send_bytes(p.data.as_deref(), p.data_text.as_deref())?;
    let session = check_session(state, conn, &p.session_id, true)?;
    let device = state.device_for_session(&p.session_id)?;
    reject_if_exclusively_locked(&device.lock, &p.session_id)?;
    let n = bytes.len() as u64;
    let sent_at_seq = device
        .writer
        .write(bytes)
        .await
        .map_err(writer_error_to_proto)?;
    if p.eat_echo {
        session.lock().consumer_cursor = sent_at_seq + n;
    }
    let result = SendResult {
        bytes_written: n,
        sent_at_seq,
    };
    Ok(serde_json::to_value(result).unwrap())
}

pub async fn expect(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: ExpectParams = parse_params(params)?;
    let session = check_session(state, conn, &p.session_id, false)?;
    let device = state.device_for_session(&p.session_id)?;
    let from_seq = match p.from_seq {
        Some(s) => s,
        None => session.lock().consumer_cursor,
    };
    let result = expect_loop(
        &device,
        from_seq,
        &p.pattern,
        p.regex,
        p.strip_ansi,
        p.strip_echo.as_deref().map(str::as_bytes),
        p.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
        p.max_bytes.unwrap_or(1024 * 1024),
        p.max_output_bytes,
    )
    .await?;
    session.lock().consumer_cursor = result.end_seq;
    Ok(serde_json::to_value(result).unwrap())
}

pub async fn run(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: RunParams = parse_params(params)?;
    let bytes = collect_send_bytes(p.data.as_deref(), p.data_text.as_deref())?;
    let session = check_session(state, conn, &p.session_id, true)?;
    let device = state.device_for_session(&p.session_id)?;

    // Acquire the device's writer lock for the duration of this transaction.
    // `run`'s hold is non-exclusive — see `LockGuard`'s doc comment for why
    // its drop is safe even when an explicit `lock` overlaps this hold.
    acquire_lock(&device.lock, &p.session_id, &p.preempt, false).await?;
    let _guard = LockGuard {
        lock: device.lock.clone(),
        session_id: p.session_id.clone(),
    };

    let started = Instant::now();
    let sent_at_seq = device
        .writer
        .write(bytes.clone())
        .await
        .map_err(writer_error_to_proto)?;

    let strip_echo_bytes: Option<&[u8]> = if p.strip_echo {
        Some(bytes.as_slice())
    } else {
        None
    };

    let result = expect_loop(
        &device,
        sent_at_seq,
        &p.until.pattern,
        p.until.regex,
        p.until.strip_ansi,
        strip_echo_bytes,
        p.timeout_ms.map(|ms| Duration::from_millis(ms as u64)),
        p.max_bytes.unwrap_or(1024 * 1024),
        p.max_output_bytes,
    )
    .await?;
    session.lock().consumer_cursor = result.end_seq;

    // The wire shape for `run` is `ExpectMatch` augmented with `duration_ms`,
    // which preserves the truncation fields. (`RunResult` exists in the
    // protocol crate as the canonical name but v0.2 keeps the unified shape.)
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut v = serde_json::to_value(&result).unwrap();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("duration_ms".into(), json!(duration_ms));
    }
    Ok(v)
}

/// Take exclusive, session-held possession of the device's writer lock. Unlike
/// `run`'s internal transient hold, this persists until `unlock`, session
/// detach, or connection teardown — and it also blocks plain `send` from
/// other sessions (see `reject_if_exclusively_locked`). Meant for a session
/// that's about to flash the device out-of-band and needs the wire to itself.
pub async fn lock(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: LockParams = parse_params(params)?;
    check_session(state, conn, &p.session_id, true)?;
    let device = resolve_locked_device(state, &p.session_id, p.device_id.as_deref())?;
    acquire_lock(&device.lock, &p.session_id, &p.preempt, true).await?;
    Ok(serde_json::to_value(LockResult { locked: true }).unwrap())
}

/// Release an exclusive lock this session holds. Idempotent — releasing an
/// already-free lock succeeds; releasing a lock held by *another* session
/// fails with `LockContention` rather than silently stealing it.
pub async fn unlock(
    state: &DaemonState,
    conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: UnlockParams = parse_params(params)?;
    check_session(state, conn, &p.session_id, true)?;
    let device = resolve_locked_device(state, &p.session_id, p.device_id.as_deref())?;
    release_lock_if_holder(&device.lock, &p.session_id)?;
    Ok(serde_json::to_value(UnlockResult { unlocked: true }).unwrap())
}

pub async fn status(
    state: &DaemonState,
    _conn: &ConnState,
    _params: Option<Value>,
) -> Result<Value, ProtocolError> {
    use tether_protocol::{DeviceStatus, SessionInfo};
    // Build per-device status rows for the new `devices` array.
    let mut device_rows: Vec<DeviceStatus> = Vec::with_capacity(state.devices.len());
    for dev in state.devices.values() {
        let (head, tail) = dev.buffer.snapshot_seqs();
        let connected = dev.state.lock().connected;
        let explicitly_disconnected = dev.state.lock().explicitly_disconnected;
        let mut info = device_info_from(&dev.config.lock(), connected, &dev.console);
        info.id = Some(dev.id.clone());
        let holder = dev.lock.holder.lock().as_ref().map(|h| h.session_id.clone());
        // Filter the global session list down to ones bound to this device.
        let dev_id = dev.id.clone();
        let dev_sessions: Vec<SessionInfo> = state
            .sessions
            .snapshot_for_device(head, &dev_id);
        device_rows.push(DeviceStatus {
            id: dev.id.clone(),
            device: info,
            buffer: BufferInfo {
                capacity_bytes: dev.buffer.capacity() as u64,
                head_seq: head,
                tail_seq: tail,
            },
            lock: LockState {
                holder_session_id: holder,
                acquired_at: None,
            },
            sessions: dev_sessions,
            explicitly_disconnected,
        });
    }

    // Backwards-compat top-level fields: pick the default device.
    let default_dev = state
        .devices
        .get(&state.default_device)
        .expect("default device always present");
    let (head, tail) = default_dev.buffer.snapshot_seqs();
    let device_connected = default_dev.state.lock().connected;
    let mut device =
        device_info_from(&default_dev.config.lock(), device_connected, &default_dev.console);
    device.id = Some(default_dev.id.clone());
    let holder = default_dev.lock.holder.lock().as_ref().map(|h| h.session_id.clone());
    let result = StatusResult {
        device,
        buffer: BufferInfo {
            capacity_bytes: default_dev.buffer.capacity() as u64,
            head_seq: head,
            tail_seq: tail,
        },
        lock: LockState {
            holder_session_id: holder,
            acquired_at: None,
        },
        sessions: state.sessions.snapshot(head),
        devices: device_rows,
        default_device: Some(state.default_device.clone()),
    };
    Ok(serde_json::to_value(result).unwrap())
}

/// Force the daemon to drop the current device handle and reopen it.
/// Useful when the serial bus is wedged (USB driver hang, board reset
/// half-completed) and a `tether status` shows `connected:true` even though
/// nothing is responding.
///
/// Params (all optional):
///   `wait`: bool — block until the device is back open (default true)
///   `timeout_ms`: u32 — how long to wait for reconnect (default 5000)
///
/// Result:
///   `triggered: bool` — always true; we always notify the serial task.
///   `reconnected: bool` — true if the device came back within the timeout.
///   `device_connected: bool` — final device state on return.
pub async fn reconnect(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: tether_protocol::ReconnectParams = match params {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| err_with(ErrorCode::InvalidParams, e.to_string()))?,
        None => tether_protocol::ReconnectParams {
            device_id: None,
            wait: true,
            timeout_ms: 5000,
        },
    };
    let device = state.resolve_device(p.device_id.as_deref())?;

    // Subscribe to the next "reconnected" pulse *before* we fire the kick,
    // so we can't miss the signal even if the serial task is fast.
    let waiter = device.reconnected.notified();
    tokio::pin!(waiter);
    device.force_reconnect.notify_waiters();

    let mut reconnected_ok = false;
    if p.wait {
        let timeout = std::time::Duration::from_millis(p.timeout_ms as u64);
        match tokio::time::timeout(timeout, waiter).await {
            Ok(()) => reconnected_ok = true,
            Err(_) => {
                // Maybe device was already up before we even had to reconnect;
                // double-check current state.
                reconnected_ok = device.state.lock().connected;
            }
        }
    }
    let (device_connected, reason) = {
        let s = device.state.lock();
        (s.connected, s.last_disconnect_reason.clone())
    };

    let mut result = serde_json::json!({
        "triggered": true,
        "reconnected": reconnected_ok,
        "device_connected": device_connected,
    });
    // Surface *why* the port is still down (e.g. "No such file or directory")
    // so the client can give a precise diagnostic instead of a bare timeout.
    if !device_connected {
        if let Some(reason) = reason {
            result["reason"] = serde_json::Value::String(reason);
        }
    }
    Ok(result)
}

/// Enumerate the serial ports the daemon machine knows about.
///
/// Failures of the underlying `serialport::available_ports()` are not fatal:
/// some platforms (notably Linux without udev metadata, or restricted
/// containers) refuse to enumerate, and tio's behaviour there is to keep
/// going rather than error. We log a warning and return an empty array so
/// AI-agent callers see a stable shape.
pub async fn list_ports(
    _state: &DaemonState,
    _conn: &ConnState,
    _params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let ports = match tokio_serial::available_ports() {
        Ok(list) => list
            .into_iter()
            .map(|p| {
                let (kind, manufacturer, product, serial_number, vid, pid) = match p.port_type {
                    tokio_serial::SerialPortType::UsbPort(info) => (
                        "usb",
                        info.manufacturer,
                        info.product,
                        info.serial_number,
                        Some(format!("{:04x}", info.vid)),
                        Some(format!("{:04x}", info.pid)),
                    ),
                    tokio_serial::SerialPortType::PciPort => ("pci", None, None, None, None, None),
                    tokio_serial::SerialPortType::BluetoothPort => {
                        ("bluetooth", None, None, None, None, None)
                    }
                    tokio_serial::SerialPortType::Unknown => {
                        ("unknown", None, None, None, None, None)
                    }
                };
                PortInfo {
                    path: p.port_name,
                    kind: kind.to_string(),
                    manufacturer,
                    product,
                    serial_number,
                    vid,
                    pid,
                }
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(error=%e, "available_ports failed; returning empty list");
            Vec::new()
        }
    };
    Ok(serde_json::to_value(ListPortsResult { ports }).unwrap())
}

/// Apply a partial update to the live serial settings.
///
/// All fields are optional; absent fields keep their current value. The
/// daemon updates the live `tokio_serial::SerialStream` in place when the
/// backend supports termios (USB serial, real UART). For PTY/pipe-style
/// devices that don't accept termios, the call returns
/// `UnsupportedSerialOp` and the device's settings stay unchanged.
pub async fn set_device(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: SetDeviceParams = parse_params(params)?;

    // Validate every requested change first, so we never half-apply a partial
    // update because (e.g.) a typo'd parity name slipped through.
    let baud = p.baud;
    let data_bits = match p.data_bits {
        Some(n) => Some(SerialDataBits::from_u8(n).ok_or_else(|| {
            err_with(
                ErrorCode::InvalidSerialSetting,
                format!("data_bits must be 5..=8, got {n}"),
            )
        })?),
        None => None,
    };
    let parity = match p.parity.as_deref() {
        Some(s) => Some(SerialParity::parse(s).ok_or_else(|| {
            err_with(
                ErrorCode::InvalidSerialSetting,
                format!("parity must be none|odd|even, got {s:?}"),
            )
        })?),
        None => None,
    };
    let stop_bits = match p.stop_bits {
        Some(n) => Some(SerialStopBits::from_u8(n).ok_or_else(|| {
            err_with(
                ErrorCode::InvalidSerialSetting,
                format!("stop_bits must be 1 or 2, got {n}"),
            )
        })?),
        None => None,
    };
    let flow_control = match p.flow_control.as_deref() {
        Some(s) => Some(SerialFlow::parse(s).ok_or_else(|| {
            err_with(
                ErrorCode::InvalidSerialSetting,
                format!("flow_control must be none|software|hardware, got {s:?}"),
            )
        })?),
        None => None,
    };

    if baud.is_none()
        && data_bits.is_none()
        && parity.is_none()
        && stop_bits.is_none()
        && flow_control.is_none()
    {
        return Err(err_with(
            ErrorCode::InvalidParams,
            "set_device requires at least one of baud/data_bits/parity/stop_bits/flow_control",
        ));
    }

    let target = state.resolve_device(p.device_id.as_deref())?;
    target
        .serial_control
        .apply(baud, data_bits, parity, stop_bits, flow_control)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::Unsupported => {
                ProtocolError::new(ErrorCode::UnsupportedSerialOp).with_message(e.to_string())
            }
            std::io::ErrorKind::NotConnected => {
                ProtocolError::new(ErrorCode::DeviceDisconnected).with_message(e.to_string())
            }
            _ => err_with(ErrorCode::InvalidSerialSetting, e.to_string()),
        })?;

    let device_connected = target.state.lock().connected;
    let mut device = device_info_from(&target.config.lock(), device_connected, &target.console);
    device.id = Some(target.id.clone());

    // Notify all attached clients about the live config change so UIs can
    // refresh without polling. Mirrors the existing connect/disconnect
    // notifications.
    let _ = target.events.send(crate::state::DeviceEvent {
        kind: crate::state::DeviceEventKind::ConfigChanged,
        detail: Some(format!(
            "config changed: {} {}{}{} flow={}",
            device.baud,
            device.data_bits,
            device
                .parity
                .chars()
                .next()
                .map(|c| c.to_ascii_uppercase())
                .unwrap_or('?'),
            device.stop_bits,
            device.flow_control,
        )),
    });

    Ok(serde_json::to_value(SetDeviceResult { device }).unwrap())
}

/// Enumerate the devices this daemon currently owns. Daemon-wide RPC, no
/// `device_id` parameter. Returned shape mirrors `StatusResult.devices` but
/// without buffer/lock/sessions detail — meant for "which devices do you
/// have?" queries.
pub async fn list_devices(
    state: &DaemonState,
    _conn: &ConnState,
    _params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let mut summaries: Vec<DeviceSummary> = state
        .devices
        .values()
        .map(|d| {
            let cfg = d.config.lock().clone();
            let st = d.state.lock().clone();
            DeviceSummary {
                id: d.id.clone(),
                path: cfg.path,
                baud: cfg.baud,
                data_bits: cfg.data_bits.as_u8(),
                parity: cfg.parity.as_str().to_string(),
                stop_bits: cfg.stop_bits.as_u8(),
                flow_control: cfg.flow_control.as_str().to_string(),
                connected: st.connected,
                explicitly_disconnected: st.explicitly_disconnected,
                shell: d.console.shell.clone(),
                prompt: d.console.prompt.clone(),
                newline: d.console.newline.clone(),
            }
        })
        .collect();
    // Stable order: default first, then alphabetical.
    summaries.sort_by(|a, b| {
        let a_default = a.id == state.default_device;
        let b_default = b.id == state.default_device;
        b_default.cmp(&a_default).then_with(|| a.id.cmp(&b.id))
    });
    Ok(serde_json::to_value(ListDevicesResult {
        devices: summaries,
        default_device: state.default_device.clone(),
    })
    .unwrap())
}

/// Map an io::Error from a serial-control RPC into the right protocol
/// error code. Used by send_break / set_dtr / set_rts / read_modem.
fn ctrl_io_to_proto(e: std::io::Error) -> ProtocolError {
    match e.kind() {
        std::io::ErrorKind::Unsupported => {
            ProtocolError::new(ErrorCode::UnsupportedSerialOp).with_message(e.to_string())
        }
        std::io::ErrorKind::NotConnected => {
            ProtocolError::new(ErrorCode::DeviceDisconnected).with_message(e.to_string())
        }
        _ => err_with(ErrorCode::InternalError, e.to_string()),
    }
}

pub async fn send_break(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: SendBreakParams = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    device
        .serial_control
        .send_break(p.duration_ms)
        .await
        .map_err(ctrl_io_to_proto)?;
    Ok(serde_json::to_value(AckResult { ok: true }).unwrap())
}

pub async fn set_dtr(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: SetLineParams = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    device
        .serial_control
        .set_dtr(p.on)
        .await
        .map_err(ctrl_io_to_proto)?;
    Ok(serde_json::to_value(AckResult { ok: true }).unwrap())
}

pub async fn set_rts(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: SetLineParams = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    device
        .serial_control
        .set_rts(p.on)
        .await
        .map_err(ctrl_io_to_proto)?;
    Ok(serde_json::to_value(AckResult { ok: true }).unwrap())
}

pub async fn read_modem_status(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: DeviceTarget = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    let status = device
        .serial_control
        .read_modem()
        .await
        .map_err(ctrl_io_to_proto)?;
    Ok(serde_json::to_value(ReadModemStatusResult {
        cts: status.cts,
        dsr: status.dsr,
        ri: status.ri,
        dcd: status.dcd,
    })
    .unwrap())
}

pub async fn disconnect_device(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: DeviceTarget = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    device
        .serial_control
        .disconnect()
        .await
        .map_err(ctrl_io_to_proto)?;
    let device_connected = device.state.lock().connected;
    let mut info = device_info_from(&device.config.lock(), device_connected, &device.console);
    info.id = Some(device.id.clone());
    Ok(serde_json::to_value(DisconnectDeviceResult { device: info }).unwrap())
}

pub async fn connect_device(
    state: &DaemonState,
    _conn: &ConnState,
    params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let p: DeviceTarget = parse_params(params)?;
    let device = state.resolve_device(p.device_id.as_deref())?;
    device
        .serial_control
        .connect()
        .await
        .map_err(ctrl_io_to_proto)?;
    // Wait briefly for the reopen so the response reflects the result.
    let waiter = device.reconnected.notified();
    tokio::pin!(waiter);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(2000), waiter).await;
    let device_connected = device.state.lock().connected;
    let mut info = device_info_from(&device.config.lock(), device_connected, &device.console);
    info.id = Some(device.id.clone());
    Ok(serde_json::to_value(ConnectDeviceResult {
        device: info,
        connected: device_connected,
    })
    .unwrap())
}

// ---------- Internal helpers ----------

fn collect_send_bytes(
    data_b64: Option<&str>,
    data_text: Option<&str>,
) -> Result<Vec<u8>, ProtocolError> {
    match (data_b64, data_text) {
        (Some(b64), _) => b64_decode(b64),
        (None, Some(t)) => Ok(t.as_bytes().to_vec()),
        (None, None) => Err(err_with(
            ErrorCode::InvalidParams,
            "either data or data_text required",
        )),
    }
}

fn check_session(
    state: &DaemonState,
    conn: &ConnState,
    session_id: &str,
    require_rw: bool,
) -> Result<std::sync::Arc<parking_lot::Mutex<crate::session::Session>>, ProtocolError> {
    if !conn.has_session(session_id) {
        return Err(err(ErrorCode::SessionNotAttached));
    }
    let session = state
        .sessions
        .get(session_id)
        .ok_or_else(|| err(ErrorCode::SessionNotFound))?;
    if require_rw && session.lock().mode != SessionMode::Rw {
        return Err(err(ErrorCode::ModeViolation));
    }
    Ok(session)
}

/// Acquire (or re-enter) the device's writer lock for `session_id`.
///
/// `exclusive` marks the kind of hold being requested: `run` always passes
/// `false` (a transient hold that only serialises against other
/// `run`/`lock` callers, released by `LockGuard` when the transaction ends);
/// the `lock` RPC passes `true` (a hold that also gates plain `send` — see
/// `reject_if_exclusively_locked`) and is released only by `unlock` or
/// teardown. Re-entering an existing hold never downgrades it from exclusive
/// to non-exclusive, but does upgrade a non-exclusive hold to exclusive if
/// the caller asked for one — so a session already inside a `run` can `lock`
/// the same device without racing itself.
async fn acquire_lock(
    lock: &crate::state::WriterLock,
    session_id: &str,
    preempt: &str,
    exclusive: bool,
) -> Result<(), ProtocolError> {
    loop {
        {
            let mut h = lock.holder.lock();
            match h.as_mut() {
                None => {
                    *h = Some(crate::state::LockHold {
                        session_id: session_id.to_string(),
                        exclusive,
                    });
                    return Ok(());
                }
                // Re-entry by the same session is allowed.
                Some(hold) if hold.session_id == session_id => {
                    if exclusive {
                        hold.exclusive = true;
                    }
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        match preempt {
            "fail" => return Err(err(ErrorCode::LockContention)),
            "force" => {
                // v0.3 will track outstanding tasks so we can really abort the
                // current holder. For now we simply swap the holder.
                let mut h = lock.holder.lock();
                *h = Some(crate::state::LockHold {
                    session_id: session_id.to_string(),
                    exclusive,
                });
                return Ok(());
            }
            _ => {
                // queue — wait for the next release notification.
                lock.released.notified().await;
            }
        }
    }
}

/// Releases `run`'s transient writer-lock hold when its transaction ends.
/// On drop, only clears the holder if it's still (a) this session and (b)
/// still non-exclusive — if an explicit `lock` upgraded the hold to
/// exclusive (whether that happened before `run` started or while it was in
/// flight), the guard steps aside and leaves the release to `unlock` /
/// teardown instead of severing whatever the exclusive holder was doing.
struct LockGuard {
    lock: std::sync::Arc<crate::state::WriterLock>,
    session_id: String,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        release_transient_hold(&self.lock, &self.session_id);
    }
}

/// The pure state transition behind `LockGuard::drop`, pulled out so it can
/// be unit-tested without spinning up a `Device`: release the hold iff it's
/// still (a) this session and (b) still non-exclusive.
fn release_transient_hold(lock: &crate::state::WriterLock, session_id: &str) {
    let mut h = lock.holder.lock();
    let ours_and_transient = matches!(
        h.as_ref(),
        Some(hold) if hold.session_id == session_id && !hold.exclusive
    );
    if ours_and_transient {
        *h = None;
        lock.released.notify_waiters();
    }
}

/// Resolve the device a `lock`/`unlock` call targets. The lock is keyed by
/// session (not connection), so the device always comes from the session's
/// binding; an explicit `device_id` is accepted only as a consistency check
/// against that binding, guarding against a caller that mixed up sessions
/// across devices.
fn resolve_locked_device(
    state: &DaemonState,
    session_id: &str,
    device_id: Option<&str>,
) -> Result<std::sync::Arc<crate::state::Device>, ProtocolError> {
    let device = state.device_for_session(session_id)?;
    if let Some(id) = device_id {
        if id != device.id {
            return Err(err_with(
                ErrorCode::DeviceNotFound,
                format!("session bound to device {:?}, not {id:?}", device.id),
            ));
        }
    }
    Ok(device)
}

/// Reject a raw write when the device's writer lock is held *exclusively* by
/// a different session (taken via `lock` — typically flashing). `run`'s own
/// internal hold is never exclusive, so a plain `run` in progress doesn't
/// trigger this; it already serialises against other `run`/`lock` callers
/// through `acquire_lock`'s queueing.
fn reject_if_exclusively_locked(
    lock: &crate::state::WriterLock,
    session_id: &str,
) -> Result<(), ProtocolError> {
    let h = lock.holder.lock();
    if let Some(hold) = h.as_ref() {
        if hold.exclusive && hold.session_id != session_id {
            return Err(err_with(
                ErrorCode::LockContention,
                "device locked by another session (flashing?); try again after unlock",
            ));
        }
    }
    Ok(())
}

/// Release the device's writer lock if `session_id` is the current holder.
/// Idempotent when nobody holds it; refuses (`LockContention`) to release a
/// lock a *different* session holds.
fn release_lock_if_holder(
    lock: &crate::state::WriterLock,
    session_id: &str,
) -> Result<(), ProtocolError> {
    let mut h = lock.holder.lock();
    match h.as_ref() {
        None => Ok(()),
        Some(hold) if hold.session_id == session_id => {
            *h = None;
            lock.released.notify_waiters();
            Ok(())
        }
        Some(_) => Err(err_with(ErrorCode::LockContention, "not the lock holder")),
    }
}

/// Best-effort lock release at teardown (session `detach`, connection
/// close): if `session_id` held the device's writer lock, free it so it
/// doesn't outlive the session with no way to `unlock` it. Silently does
/// nothing if the session never held a lock, already released it, or its
/// device can't be resolved (e.g. already removed).
pub(crate) fn release_lock_for_session(state: &DaemonState, session_id: &str) {
    if let Ok(device) = state.device_for_session(session_id) {
        let _ = release_lock_if_holder(&device.lock, session_id);
    }
}

#[allow(clippy::too_many_arguments)]
async fn expect_loop(
    device: &crate::state::Device,
    from_seq: u64,
    pattern: &str,
    use_regex: bool,
    strip_ansi: bool,
    strip_echo: Option<&[u8]>,
    timeout: Option<Duration>,
    max_bytes: u64,
    max_output_bytes: Option<u64>,
) -> Result<ExpectMatch, ProtocolError> {
    let regex = if use_regex {
        Some(
            regex::bytes::Regex::new(pattern)
                .map_err(|e| err_with(ErrorCode::InvalidParams, format!("regex: {e}")))?,
        )
    } else {
        None
    };

    let started = Instant::now();
    let mut accumulated: Vec<u8> = Vec::new();
    let mut cursor = from_seq;

    loop {
        let (chunk, new_cursor, _lag) = device.buffer.read_from(cursor);
        cursor = new_cursor;
        if !chunk.is_empty() {
            accumulated.extend_from_slice(&chunk);
        }

        let haystack: Vec<u8> = if strip_ansi {
            strip_ansi_bytes(&accumulated)
        } else {
            accumulated.clone()
        };

        let m = if let Some(re) = &regex {
            re.find(&haystack).map(|m| (m.start(), m.end()))
        } else {
            find_substring(&haystack, pattern.as_bytes())
        };

        if let Some((start, end)) = m {
            let mut before_bytes = haystack[..start].to_vec();
            if let Some(echo) = strip_echo {
                before_bytes = strip_leading_echo(&before_bytes, echo);
            }
            let original_len = before_bytes.len();
            let truncated = match max_output_bytes {
                Some(limit) if (original_len as u64) > limit => {
                    let keep = limit as usize;
                    let cut = original_len - keep;
                    before_bytes = before_bytes[cut..].to_vec();
                    true
                }
                _ => false,
            };
            let match_text = String::from_utf8_lossy(&haystack[start..end]).to_string();
            return Ok(ExpectMatch {
                matched: true,
                match_text,
                before: b64_encode(&before_bytes),
                match_seq: from_seq + start as u64,
                end_seq: from_seq + end as u64,
                truncated,
                original_bytes: if truncated { Some(original_len as u64) } else { None },
            });
        }

        if accumulated.len() as u64 > max_bytes {
            return Err(err(ErrorCode::BufferOverflow));
        }

        if let Some(to) = timeout {
            let elapsed = started.elapsed();
            if elapsed >= to {
                return Err(err(ErrorCode::Timeout).with_data(json!({
                    "buffered": b64_encode(&accumulated),
                    "buffered_seq_range": [from_seq, cursor]
                })));
            }
            let remain = to - elapsed;
            let _ = tokio::time::timeout(remain, device.buffer.wait()).await;
        } else {
            device.buffer.wait().await;
        }
    }
}

fn find_substring(hay: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    if hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some((i, i + needle.len()));
        }
    }
    None
}

/// Constant-time bytewise comparison to avoid timing leaks on token check.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Strip CSI escapes plus single-character ESC sequences.
fn strip_ansi_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            i += 1;
            if i >= input.len() { break; }
            if input[i] == b'[' {
                i += 1;
                while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                    i += 1;
                }
                if i < input.len() { i += 1; }
            } else {
                i += 1;
            }
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

/// If `before` starts with an echo of `echo` (the bytes just sent), strip it.
/// CR/LF tolerant: ignores CR vs LF vs CRLF differences. Assumes a line
/// terminator always follows the echo.
fn strip_leading_echo(before: &[u8], echo: &[u8]) -> Vec<u8> {
    // Body of `echo` with any trailing CR/LF removed.
    let echo_body: &[u8] = {
        let mut end = echo.len();
        while end > 0 && (echo[end - 1] == b'\r' || echo[end - 1] == b'\n') {
            end -= 1;
        }
        &echo[..end]
    };
    if echo_body.is_empty() {
        return before.to_vec();
    }
    // Extract the first line of `before` (everything up to the first CR/LF).
    let mut line_end = 0;
    while line_end < before.len() && before[line_end] != b'\n' && before[line_end] != b'\r' {
        line_end += 1;
    }
    let first_line = &before[..line_end];
    if first_line == echo_body {
        // Consume the line terminator too — at most a single CRLF.
        let mut consumed = line_end;
        while consumed < before.len() && (before[consumed] == b'\r' || before[consumed] == b'\n') {
            consumed += 1;
            if consumed - line_end >= 2 { break; }
        }
        before[consumed..].to_vec()
    } else {
        before.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_crlf_echo() {
        let before = b"version\r\nU-Boot 2025\r\n";
        let echo = b"version\r\n";
        let r = strip_leading_echo(before, echo);
        assert_eq!(r, b"U-Boot 2025\r\n");
    }

    #[test]
    fn strips_lf_echo_with_crlf_response() {
        let before = b"version\r\nU-Boot\r\n";
        let echo = b"version\n";
        let r = strip_leading_echo(before, echo);
        assert_eq!(r, b"U-Boot\r\n");
    }

    #[test]
    fn no_strip_when_mismatch() {
        let before = b"hello world";
        let echo = b"version\n";
        let r = strip_leading_echo(before, echo);
        assert_eq!(r, before);
    }

    #[test]
    fn ansi_strip_basic() {
        let s = b"\x1b[31mred\x1b[0m text";
        assert_eq!(strip_ansi_bytes(s), b"red text");
    }

    // ---------- Writer-lock state transitions ----------
    //
    // These exercise `acquire_lock` / `release_transient_hold` /
    // `reject_if_exclusively_locked` / `release_lock_if_holder` directly
    // against a bare `WriterLock`, without needing a full `Device`.

    use crate::state::{LockHold, WriterLock};

    #[tokio::test]
    async fn acquire_lock_reentry_never_downgrades_exclusive() {
        let lock = WriterLock::default();

        // Fresh, non-exclusive acquire (like `run`).
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();
        assert!(!lock.holder.lock().as_ref().unwrap().exclusive);

        // Same session re-enters non-exclusively: still non-exclusive.
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();
        assert!(!lock.holder.lock().as_ref().unwrap().exclusive);

        // Same session upgrades to exclusive (like `lock` while a `run` is
        // already holding the lock).
        acquire_lock(&lock, "s1", "queue", true).await.unwrap();
        assert!(lock.holder.lock().as_ref().unwrap().exclusive);

        // A later non-exclusive re-entry (another `run`) must not downgrade it.
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();
        assert!(lock.holder.lock().as_ref().unwrap().exclusive);
    }

    #[tokio::test]
    async fn acquire_lock_fail_preempt_reports_contention() {
        let lock = WriterLock::default();
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();

        let err = acquire_lock(&lock, "s2", "fail", true).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::LockContention.as_i32());
    }

    #[tokio::test]
    async fn acquire_lock_force_preempt_swaps_holder() {
        let lock = WriterLock::default();
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();

        acquire_lock(&lock, "s2", "force", true).await.unwrap();
        let holder = lock.holder.lock().clone().unwrap();
        assert_eq!(holder.session_id, "s2");
        assert!(holder.exclusive);
    }

    #[tokio::test]
    async fn acquire_lock_queue_waits_for_release() {
        let lock = std::sync::Arc::new(WriterLock::default());
        acquire_lock(&lock, "s1", "queue", false).await.unwrap();

        let waiter_lock = lock.clone();
        let waiter = tokio::spawn(async move {
            acquire_lock(&waiter_lock, "s2", "queue", true).await
        });

        // Give the waiter a chance to park on `released` before we release.
        tokio::time::sleep(Duration::from_millis(20)).await;
        release_lock_if_holder(&lock, "s1").unwrap();

        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should complete once released")
            .unwrap()
            .unwrap();
        let holder = lock.holder.lock().clone().unwrap();
        assert_eq!(holder.session_id, "s2");
        assert!(holder.exclusive);
    }

    #[test]
    fn release_transient_hold_clears_own_non_exclusive_hold() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s1".into(), exclusive: false });
        release_transient_hold(&lock, "s1");
        assert!(lock.holder.lock().is_none());
    }

    #[test]
    fn release_transient_hold_leaves_exclusive_hold_alone() {
        // An explicit `lock` upgraded the hold while `run` was in flight (or
        // held it before `run` even started) — `run`'s guard must not clear it.
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s1".into(), exclusive: true });
        release_transient_hold(&lock, "s1");
        assert!(lock.holder.lock().is_some());
    }

    #[test]
    fn release_transient_hold_ignores_other_sessions() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s2".into(), exclusive: false });
        release_transient_hold(&lock, "s1");
        assert!(lock.holder.lock().is_some());
    }

    #[test]
    fn reject_if_exclusively_locked_allows_free_and_non_exclusive() {
        let lock = WriterLock::default();
        assert!(reject_if_exclusively_locked(&lock, "s1").is_ok());

        *lock.holder.lock() = Some(LockHold { session_id: "s2".into(), exclusive: false });
        assert!(reject_if_exclusively_locked(&lock, "s1").is_ok());
    }

    #[test]
    fn reject_if_exclusively_locked_allows_own_exclusive_hold() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s1".into(), exclusive: true });
        assert!(reject_if_exclusively_locked(&lock, "s1").is_ok());
    }

    #[test]
    fn reject_if_exclusively_locked_blocks_other_sessions_send() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s2".into(), exclusive: true });
        let err = reject_if_exclusively_locked(&lock, "s1").unwrap_err();
        assert_eq!(err.code, ErrorCode::LockContention.as_i32());
    }

    #[test]
    fn release_lock_if_holder_is_idempotent_when_free() {
        let lock = WriterLock::default();
        assert!(release_lock_if_holder(&lock, "s1").is_ok());
    }

    #[test]
    fn release_lock_if_holder_releases_own_hold() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s1".into(), exclusive: true });
        release_lock_if_holder(&lock, "s1").unwrap();
        assert!(lock.holder.lock().is_none());
    }

    #[test]
    fn release_lock_if_holder_refuses_other_sessions() {
        let lock = WriterLock::default();
        *lock.holder.lock() = Some(LockHold { session_id: "s2".into(), exclusive: true });
        let err = release_lock_if_holder(&lock, "s1").unwrap_err();
        assert_eq!(err.code, ErrorCode::LockContention.as_i32());
        // Left untouched — still held by s2.
        assert_eq!(lock.holder.lock().as_ref().unwrap().session_id, "s2");
    }
}
