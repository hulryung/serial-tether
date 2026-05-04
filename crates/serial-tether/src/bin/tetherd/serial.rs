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
use tokio::sync::{mpsc, Notify};

use crate::buffer::RingBuffer;
use crate::state::DeviceState;

#[derive(Debug, Clone)]
pub struct SerialConfig {
    pub path: String,
    pub baud: u32,
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
            Ok(s) => Ok(SerialPort::Real(s)),
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
/// Returns the writer handle. Callers update `state` and trigger
/// `force_reconnect` from the outside (e.g., the `reconnect` RPC).
pub fn spawn(
    cfg: SerialConfig,
    buffer: RingBuffer,
    state: Arc<Mutex<DeviceState>>,
    force_reconnect: Arc<Notify>,
    reconnected: Arc<Notify>,
) -> (SerialWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<WriteJob>(64);
    let writer = SerialWriter { tx };

    let handle = tokio::spawn(async move {
        let initial_backoff = Duration::from_millis(500);
        let max_backoff = Duration::from_secs(10);
        let mut backoff = initial_backoff;

        loop {
            // Try to open the device. While in retry, drain pending write
            // jobs immediately with a NotConnected error so clients don't wait.
            let port = loop {
                match SerialPort::open(&cfg).await {
                    Ok(p) => {
                        tracing::info!(path = %cfg.path, "serial opened");
                        {
                            let mut s = state.lock();
                            s.connected = true;
                            s.last_open_at = Some(Instant::now());
                            s.last_disconnect_reason = None;
                            s.consecutive_open_failures = 0;
                        }
                        reconnected.notify_waiters();
                        backoff = initial_backoff;
                        break p;
                    }
                    Err(e) => {
                        let reason = format!("{e}");
                        tracing::warn!(
                            path = %cfg.path, error = %reason,
                            backoff_ms = backoff.as_millis() as u64,
                            "serial open failed; will retry"
                        );
                        {
                            let mut s = state.lock();
                            s.connected = false;
                            s.last_disconnect_reason = Some(reason);
                            s.consecutive_open_failures += 1;
                        }
                        // While waiting, fail any pending writes immediately
                        // and react to a force_reconnect kick.
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
                            }
                        }
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }
            };

            // Run one IO session with the open port. Returns a string reason
            // explaining why we exited (EOF, write error, force, etc.).
            let reason = run_io_session(port, &buffer, &mut rx, &force_reconnect).await;
            tracing::warn!(reason = %reason, "serial session ended");
            {
                let mut s = state.lock();
                s.connected = false;
                s.last_disconnect_reason = Some(reason);
                s.disconnect_count += 1;
            }
            // Brief gap before next reopen attempt so a flapping device
            // doesn't pin a CPU.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    (writer, handle)
}

/// Per-session IO loop. Returns when the device disconnects (or we're forced
/// to reconnect). The string is a short reason for diagnostics.
async fn run_io_session(
    port: SerialPort,
    buffer: &RingBuffer,
    rx: &mut mpsc::Receiver<WriteJob>,
    force_reconnect: &Notify,
) -> String {
    match port {
        SerialPort::Real(mut s) => {
            let mut read_buf = [0u8; 4096];
            loop {
                tokio::select! {
                    biased;
                    _ = force_reconnect.notified() => return "forced reconnect".to_string(),
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
