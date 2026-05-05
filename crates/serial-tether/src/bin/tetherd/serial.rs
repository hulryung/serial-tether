//! Serial-device abstraction with auto-reconnect.
//!
//! A single owner task wraps an "outer" reconnect loop around the per-session
//! IO loop. On any disconnect (EOF, error, or an explicit `force_reconnect`
//! signal) the task closes the device, fails any in-flight writes, then sleeps
//! a short backoff and tries to reopen. The outer loop runs forever.
//!
//! Two backends:
//! - `Real`: tokio-serial. Real serial ports that accept termios setup.
//! - `Fd`: a plain fd wrapped with `O_NONBLOCK` + `AsyncFd`. Used for PTYs,
//!   socat pairs, and other non-strict-termios devices. `tokio::fs::File` is
//!   cancel-unsafe on PTYs, so we don't use it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::io::{unix::AsyncFd, AsyncReadExt, AsyncWriteExt, Interest};
use tokio::sync::{broadcast, mpsc, Notify};

use crate::buffer::RingBuffer;
use crate::state::{DeviceEvent, DeviceEventKind, DeviceState};

#[derive(Debug, Clone)]
pub struct SerialConfig {
    pub path: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

/// Mirror of tokio_serial / serialport enums, but as our own type so the
/// protocol crate stays decoupled from serialport-rs version churn and we
/// can serialise to/from JSON-friendly strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBits { Five, Six, Seven, Eight }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity { None, Odd, Even }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits { One, Two }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl { None, Software, Hardware }

impl DataBits {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self { Self::Five => "5", Self::Six => "6", Self::Seven => "7", Self::Eight => "8" }
    }
    pub fn as_u8(self) -> u8 {
        match self { Self::Five => 5, Self::Six => 6, Self::Seven => 7, Self::Eight => 8 }
    }
    pub fn from_u8(n: u8) -> Option<Self> {
        match n { 5 => Some(Self::Five), 6 => Some(Self::Six), 7 => Some(Self::Seven), 8 => Some(Self::Eight), _ => None }
    }
    pub fn to_serial(self) -> tokio_serial::DataBits {
        match self {
            Self::Five => tokio_serial::DataBits::Five,
            Self::Six => tokio_serial::DataBits::Six,
            Self::Seven => tokio_serial::DataBits::Seven,
            Self::Eight => tokio_serial::DataBits::Eight,
        }
    }
}

impl Parity {
    pub fn as_str(self) -> &'static str {
        match self { Self::None => "none", Self::Odd => "odd", Self::Even => "even" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "n" => Some(Self::None),
            "odd" | "o" => Some(Self::Odd),
            "even" | "e" => Some(Self::Even),
            _ => None,
        }
    }
    pub fn to_serial(self) -> tokio_serial::Parity {
        match self {
            Self::None => tokio_serial::Parity::None,
            Self::Odd => tokio_serial::Parity::Odd,
            Self::Even => tokio_serial::Parity::Even,
        }
    }
}

impl StopBits {
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self { Self::One => "1", Self::Two => "2" }
    }
    pub fn as_u8(self) -> u8 {
        match self { Self::One => 1, Self::Two => 2 }
    }
    pub fn from_u8(n: u8) -> Option<Self> {
        match n { 1 => Some(Self::One), 2 => Some(Self::Two), _ => None }
    }
    pub fn to_serial(self) -> tokio_serial::StopBits {
        match self {
            Self::One => tokio_serial::StopBits::One,
            Self::Two => tokio_serial::StopBits::Two,
        }
    }
}

impl FlowControl {
    pub fn as_str(self) -> &'static str {
        match self { Self::None => "none", Self::Software => "software", Self::Hardware => "hardware" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Some(Self::None),
            "software" | "soft" | "xon" | "xon/xoff" => Some(Self::Software),
            "hardware" | "hard" | "rts/cts" | "rtscts" => Some(Self::Hardware),
            _ => None,
        }
    }
    pub fn to_serial(self) -> tokio_serial::FlowControl {
        match self {
            Self::None => tokio_serial::FlowControl::None,
            Self::Software => tokio_serial::FlowControl::Software,
            Self::Hardware => tokio_serial::FlowControl::Hardware,
        }
    }
}

/// Control commands sent into the serial owner task. Used by `set_device`,
/// (and later v0.8) `set_break`, `set_dtr`, `set_rts`, `read_modem_status`.
pub enum ControlMsg {
    Apply {
        baud: Option<u32>,
        data_bits: Option<DataBits>,
        parity: Option<Parity>,
        stop_bits: Option<StopBits>,
        flow_control: Option<FlowControl>,
        ack: tokio::sync::oneshot::Sender<Result<(), std::io::Error>>,
    },
}

#[derive(Clone)]
pub struct SerialControl {
    tx: mpsc::Sender<ControlMsg>,
}

impl SerialControl {
    pub async fn apply(
        &self,
        baud: Option<u32>,
        data_bits: Option<DataBits>,
        parity: Option<Parity>,
        stop_bits: Option<StopBits>,
        flow_control: Option<FlowControl>,
    ) -> Result<(), std::io::Error> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(ControlMsg::Apply {
                baud,
                data_bits,
                parity,
                stop_bits,
                flow_control,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "serial task gone",
            ));
        }
        ack_rx.await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "serial task dropped")
        })?
    }
}

pub enum SerialPort {
    Real(tokio_serial::SerialStream),
    Fd(FdPort),
}

pub struct FdPort {
    fd: AsyncFd<OwnedFd>,
}

impl FdPort {
    fn open(path: &str) -> Result<Self> {
        use std::ffi::CString;
        let cpath = CString::new(path).context("path has NUL")?;
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK;
        // SAFETY: open(2) is FFI; we check the result before use.
        let raw: RawFd = unsafe { libc::open(cpath.as_ptr(), flags) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("open {path}"));
        }
        // SAFETY: raw was just produced by open(2) and is owned by this process.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let fd = AsyncFd::with_interest(owned, Interest::READABLE | Interest::WRITABLE)?;
        Ok(Self { fd })
    }

    async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            let raw = self.fd.get_ref().as_raw_fd();
            // SAFETY: raw fd is valid; buf is a writable slice.
            let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(e);
        }
    }

    async fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = self.fd.writable().await?;
            let raw = self.fd.get_ref().as_raw_fd();
            // SAFETY: raw fd is valid; data is a readable slice.
            let n = unsafe { libc::write(raw, data.as_ptr() as *const _, data.len()) };
            if n >= 0 {
                data = &data[n as usize..];
                continue;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Err(e);
        }
        Ok(())
    }
}

impl SerialPort {
    pub async fn open(cfg: &SerialConfig) -> Result<Self> {
        match tokio_serial::SerialStream::open(
            &tokio_serial::new(&cfg.path, cfg.baud).timeout(Duration::from_millis(0)),
        ) {
            Ok(mut s) => {
                // termios-capable backend: apply the full config so a non-default
                // data_bits/parity/stop_bits/flow_control sticks across reconnects.
                use tokio_serial::SerialPort as _;
                if let Err(e) = s.set_data_bits(cfg.data_bits.to_serial()) {
                    tracing::warn!(error=%e, "set_data_bits failed (continuing)");
                }
                if let Err(e) = s.set_parity(cfg.parity.to_serial()) {
                    tracing::warn!(error=%e, "set_parity failed (continuing)");
                }
                if let Err(e) = s.set_stop_bits(cfg.stop_bits.to_serial()) {
                    tracing::warn!(error=%e, "set_stop_bits failed (continuing)");
                }
                if let Err(e) = s.set_flow_control(cfg.flow_control.to_serial()) {
                    tracing::warn!(error=%e, "set_flow_control failed (continuing)");
                }
                Ok(SerialPort::Real(s))
            }
            Err(e) => {
                tracing::warn!(
                    path = %cfg.path, error = %e,
                    "tokio-serial open failed; falling back to AsyncFd"
                );
                Ok(SerialPort::Fd(FdPort::open(&cfg.path)?))
            }
        }
    }
}

/// Handle the daemon uses to write to the serial device.
#[derive(Clone)]
pub struct SerialWriter {
    tx: mpsc::Sender<WriteJob>,
}

struct WriteJob {
    data: Vec<u8>,
    /// Returns the buffer's head_seq immediately before the write so the
    /// caller can pass it to a race-free `expect`.
    ack: tokio::sync::oneshot::Sender<Result<u64, std::io::Error>>,
}

impl SerialWriter {
    pub async fn write(&self, data: Vec<u8>) -> Result<u64, std::io::Error> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(WriteJob { data, ack: ack_tx })
            .await
            .is_err()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "serial task gone",
            ));
        }
        ack_rx.await.map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "serial task dropped")
        })?
    }
}

/// Spawn the serial owner task. The task runs forever, reconnecting whenever
/// the device disappears (EOF, error, or `force_reconnect`).
///
/// `cfg` is shared mutable state: the owner task is the authoritative writer
/// (it stores a successful Apply back here so the next reconnect uses the
/// updated settings); other code reads it for `status` / `hello`.
///
/// Returns (writer, control, join_handle).
pub fn spawn(
    cfg: Arc<Mutex<SerialConfig>>,
    buffer: RingBuffer,
    state: Arc<Mutex<DeviceState>>,
    force_reconnect: Arc<Notify>,
    reconnected: Arc<Notify>,
    device_events: broadcast::Sender<DeviceEvent>,
) -> (SerialWriter, SerialControl, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<WriteJob>(64);
    let writer = SerialWriter { tx };
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ControlMsg>(8);
    let control = SerialControl { tx: ctrl_tx };

    let handle = tokio::spawn(async move {
        let initial_backoff = Duration::from_millis(500);
        let max_backoff = Duration::from_secs(10);
        let mut backoff = initial_backoff;
        // We don't broadcast a Disconnected on the *first* open failure (no
        // one was attached yet, and the daemon refuses to start if the
        // first open fails anyway). After we've been connected once, every
        // transition emits an event.
        let mut emit_disconnect = false;

        loop {
            // Snapshot the latest config before each open, so a `set_device`
            // applied while we were waiting to reconnect actually takes effect.
            let snapshot = cfg.lock().clone();
            // Try to open the device. While in retry, drain pending write
            // jobs immediately with a NotConnected error so clients don't wait.
            let port = loop {
                match SerialPort::open(&snapshot).await {
                    Ok(p) => {
                        tracing::info!(path = %snapshot.path, "serial opened");
                        let was_connected = {
                            let mut s = state.lock();
                            let prev = s.connected;
                            s.connected = true;
                            s.last_open_at = Some(Instant::now());
                            s.last_disconnect_reason = None;
                            s.consecutive_open_failures = 0;
                            prev
                        };
                        if !was_connected {
                            // First open or coming back up — clients want to know.
                            let _ = device_events.send(DeviceEvent {
                                kind: DeviceEventKind::Reconnected,
                                detail: None,
                            });
                        }
                        reconnected.notify_waiters();
                        emit_disconnect = true;
                        backoff = initial_backoff;
                        break p;
                    }
                    Err(e) => {
                        let reason = format!("{e}");
                        tracing::warn!(
                            path = %snapshot.path, error = %reason,
                            backoff_ms = backoff.as_millis() as u64,
                            "serial open failed; will retry"
                        );
                        {
                            let mut s = state.lock();
                            s.connected = false;
                            s.last_disconnect_reason = Some(reason.clone());
                            s.consecutive_open_failures += 1;
                        }
                        // While waiting, fail any pending writes immediately
                        // and react to a force_reconnect kick. Control messages
                        // mutate the shared config and request a reconnect so
                        // the next open uses the new settings.
                        let sleep = tokio::time::sleep(backoff);
                        tokio::pin!(sleep);
                        loop {
                            tokio::select! {
                                _ = &mut sleep => break,
                                _ = force_reconnect.notified() => break,
                                Some(job) = rx.recv() => {
                                    let _ = job.ack.send(Err(std::io::Error::new(
                                        std::io::ErrorKind::NotConnected,
                                        "device disconnected",
                                    )));
                                }
                                Some(msg) = ctrl_rx.recv() => {
                                    let ControlMsg::Apply { baud, data_bits, parity, stop_bits, flow_control, ack } = msg;
                                    {
                                        let mut c = cfg.lock();
                                        if let Some(v) = baud { c.baud = v; }
                                        if let Some(v) = data_bits { c.data_bits = v; }
                                        if let Some(v) = parity { c.parity = v; }
                                        if let Some(v) = stop_bits { c.stop_bits = v; }
                                        if let Some(v) = flow_control { c.flow_control = v; }
                                    }
                                    let _ = ack.send(Ok(()));
                                    // Restart the open loop with the new config
                                    // immediately rather than waiting for backoff.
                                    break;
                                }
                            }
                        }
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }
            };

            // Run one IO session with the open port. Returns a string reason
            // explaining why we exited (EOF, write error, force, etc.).
            let reason = run_io_session(
                port,
                &buffer,
                &mut rx,
                &mut ctrl_rx,
                &cfg,
                &force_reconnect,
            )
            .await;
            tracing::warn!(reason = %reason, "serial session ended");
            {
                let mut s = state.lock();
                s.connected = false;
                s.last_disconnect_reason = Some(reason.clone());
                s.disconnect_count += 1;
            }
            if emit_disconnect {
                let _ = device_events.send(DeviceEvent {
                    kind: DeviceEventKind::Disconnected,
                    detail: Some(reason),
                });
            }
            // Brief gap before next reopen attempt so a flapping device
            // doesn't pin a CPU.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    (writer, control, handle)
}

/// Apply a (partial) settings update to the live `tokio_serial::SerialStream`.
/// Returns the first error encountered, or Ok if every requested change took.
fn apply_to_real(
    s: &mut tokio_serial::SerialStream,
    baud: Option<u32>,
    data_bits: Option<DataBits>,
    parity: Option<Parity>,
    stop_bits: Option<StopBits>,
    flow_control: Option<FlowControl>,
) -> std::io::Result<()> {
    use tokio_serial::SerialPort as _;
    if let Some(v) = baud {
        s.set_baud_rate(v).map_err(serialport_to_io)?;
    }
    if let Some(v) = data_bits {
        s.set_data_bits(v.to_serial()).map_err(serialport_to_io)?;
    }
    if let Some(v) = parity {
        s.set_parity(v.to_serial()).map_err(serialport_to_io)?;
    }
    if let Some(v) = stop_bits {
        s.set_stop_bits(v.to_serial()).map_err(serialport_to_io)?;
    }
    if let Some(v) = flow_control {
        s.set_flow_control(v.to_serial()).map_err(serialport_to_io)?;
    }
    Ok(())
}

fn serialport_to_io(e: tokio_serial::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}

/// Per-session IO loop. Returns when the device disconnects (or we're forced
/// to reconnect). The string is a short reason for diagnostics.
async fn run_io_session(
    port: SerialPort,
    buffer: &RingBuffer,
    rx: &mut mpsc::Receiver<WriteJob>,
    ctrl_rx: &mut mpsc::Receiver<ControlMsg>,
    cfg: &Arc<Mutex<SerialConfig>>,
    force_reconnect: &Notify,
) -> String {
    match port {
        SerialPort::Real(mut s) => {
            let mut read_buf = [0u8; 4096];
            loop {
                tokio::select! {
                    biased;
                    _ = force_reconnect.notified() => return "forced reconnect".to_string(),
                    msg = ctrl_rx.recv() => {
                        let Some(msg) = msg else { return "control channel closed".into() };
                        let ControlMsg::Apply { baud, data_bits, parity, stop_bits, flow_control, ack } = msg;
                        match apply_to_real(&mut s, baud, data_bits, parity, stop_bits, flow_control) {
                            Ok(()) => {
                                {
                                    let mut c = cfg.lock();
                                    if let Some(v) = baud { c.baud = v; }
                                    if let Some(v) = data_bits { c.data_bits = v; }
                                    if let Some(v) = parity { c.parity = v; }
                                    if let Some(v) = stop_bits { c.stop_bits = v; }
                                    if let Some(v) = flow_control { c.flow_control = v; }
                                }
                                let _ = ack.send(Ok(()));
                            }
                            Err(e) => { let _ = ack.send(Err(e)); }
                        }
                    }
                    job = rx.recv() => {
                        let Some(job) = job else { return "writer channel closed".into() };
                        let WriteJob { data, ack } = job;
                        let (head_before, _) = buffer.snapshot_seqs();
                        let res = s.write_all(&data).await.map(|_| head_before);
                        let failed = res.is_err();
                        let _ = ack.send(res);
                        if failed {
                            return "write error".to_string();
                        }
                    }
                    read = s.read(&mut read_buf) => {
                        match read {
                            Ok(0) => return "EOF".to_string(),
                            Ok(n) => buffer.push(&read_buf[..n]),
                            Err(e) => return format!("read error: {e}"),
                        }
                    }
                }
            }
        }
        SerialPort::Fd(p) => {
            let mut read_buf = [0u8; 4096];
            loop {
                tokio::select! {
                    biased;
                    _ = force_reconnect.notified() => return "forced reconnect".to_string(),
                    msg = ctrl_rx.recv() => {
                        let Some(msg) = msg else { return "control channel closed".into() };
                        let ControlMsg::Apply { ack, .. } = msg;
                        // The Fd backend covers PTYs and pipes that don't accept
                        // termios changes. Surface this as UnsupportedSerialOp
                        // to the caller.
                        let _ = ack.send(Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "device does not support runtime serial config changes",
                        )));
                    }
                    job = rx.recv() => {
                        let Some(job) = job else { return "writer channel closed".into() };
                        let WriteJob { data, ack } = job;
                        let (head_before, _) = buffer.snapshot_seqs();
                        let res = p.write_all(&data).await.map(|_| head_before);
                        let failed = res.is_err();
                        let _ = ack.send(res);
                        if failed {
                            return "write error".to_string();
                        }
                    }
                    read = p.read(&mut read_buf) => {
                        match read {
                            Ok(0) => return "EOF".to_string(),
                            Ok(n) => buffer.push(&read_buf[..n]),
                            Err(e) => return format!("read error: {e}"),
                        }
                    }
                }
            }
        }
    }
}
