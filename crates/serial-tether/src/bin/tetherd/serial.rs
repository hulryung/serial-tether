//! Serial-device abstraction.
//!
//! v0: a single task owns the device — reads push into the ring buffer,
//! writes arrive on an mpsc channel.
//!
//! Two backends:
//! - `Real`: tokio-serial. Real serial ports that accept termios setup.
//! - `Fd`: a plain fd wrapped with `O_NONBLOCK` + `AsyncFd`. Used for PTYs,
//!   socat pairs, and other non-strict-termios devices. `tokio::fs::File`
//!   is cancel-unsafe on PTYs, so we don't use it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{Context, Result};
use tokio::io::{unix::AsyncFd, AsyncReadExt, AsyncWriteExt, Interest};
use tokio::sync::mpsc;

use crate::buffer::RingBuffer;

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
            &tokio_serial::new(&cfg.path, cfg.baud)
                .timeout(std::time::Duration::from_millis(0)),
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

/// Spawn the task that owns the serial device.
pub fn spawn(port: SerialPort, buffer: RingBuffer) -> (SerialWriter, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<WriteJob>(64);
    let writer = SerialWriter { tx };

    let handle = tokio::spawn(async move {
        match port {
            SerialPort::Real(mut s) => {
                let mut read_buf = [0u8; 4096];
                loop {
                    tokio::select! {
                        biased;
                        job = rx.recv() => {
                            let Some(job) = job else { break };
                            let WriteJob { data, ack } = job;
                            let (head_before, _) = buffer.snapshot_seqs();
                            let res = s.write_all(&data).await.map(|_| head_before);
                            let _ = ack.send(res);
                        }
                        read = s.read(&mut read_buf) => {
                            match read {
                                Ok(0) => { tracing::warn!("serial EOF"); break; }
                                Ok(n) => buffer.push(&read_buf[..n]),
                                Err(e) => { tracing::error!(error=%e, "serial read error"); break; }
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
                        job = rx.recv() => {
                            let Some(job) = job else { break };
                            let WriteJob { data, ack } = job;
                            let (head_before, _) = buffer.snapshot_seqs();
                            let res = p.write_all(&data).await.map(|_| head_before);
                            let _ = ack.send(res);
                        }
                        read = p.read(&mut read_buf) => {
                            match read {
                                Ok(0) => { tracing::warn!("serial EOF"); break; }
                                Ok(n) => buffer.push(&read_buf[..n]),
                                Err(e) => { tracing::error!(error=%e, "serial read error"); break; }
                            }
                        }
                    }
                }
            }
        }
    });

    (writer, handle)
}

