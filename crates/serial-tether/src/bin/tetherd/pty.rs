//! Daemon-side virtual serial port (PTY) bridge.
//!
//! For each device configured with `pty=<link>`, we `openpty()`, publish the
//! slave under a stable symlink, and bridge raw bytes both directions:
//!
//! ```text
//!   host serial tool → PTY slave → [bridge] → real device (SerialWriter)
//!   real device RX (ring) → [bridge] → PTY master → PTY slave → host tool
//! ```
//!
//! This lets a non-tether tool (minicom / screen / pyserial / a UART flasher)
//! open the *same* physical port that `tetherd` already holds, while tether
//! clients keep tailing and sending concurrently — no second process fighting
//! for `/dev/cu.*`.
//!
//! **Scope (phase 1): data only.** A PTY has no real UART — no modem lines and
//! no line rate. So DTR/RTS auto-reset and mid-stream baud changes are *not*
//! forwarded to the real port yet (planned phase 2, OS-permitting). What works
//! today: console sharing, and flashing where the board is put into its
//! download mode out-of-band (boot strap / button / command) and the host tool
//! then just streams bytes over UART at a fixed rate.

use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{unix::AsyncFd, Interest};

use crate::buffer::RingBuffer;
use crate::serial::SerialWriter;
use crate::state::WriterLock;

/// Async wrapper around the PTY master fd (mirrors the daemon's `FdPort`).
struct PtyMaster {
    fd: AsyncFd<OwnedFd>,
}

impl PtyMaster {
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

/// Create a PTY pair: returns the async master, the slave's `/dev/...` path,
/// and the slave fd (kept open by the daemon so the pty stays alive and the
/// master never sees spurious EOF when the host tool closes/reopens).
fn create_pty() -> Result<(AsyncFd<OwnedFd>, String, OwnedFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // Raw line discipline: pass bytes through verbatim, no echo/canonical
    // cooking. The host tool will set its own termios on top; this is the
    // sane default when nothing is connected.
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::cfmakeraw(&mut tio) };
    // SAFETY: out-params are valid; termios is initialized; win size null.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            &mut tio,
            std::ptr::null_mut(),
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("openpty");
    }

    // Resolve the slave device path (e.g. /dev/ttys012) to publish.
    let mut name_buf = [0 as libc::c_char; 256];
    // SAFETY: slave is a valid tty fd; buffer is sized.
    let rc = unsafe { libc::ttyname_r(slave, name_buf.as_mut_ptr(), name_buf.len()) };
    if rc != 0 {
        // Clean up the fds we just opened before bailing.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(std::io::Error::from_raw_os_error(rc)).context("ttyname_r on pty slave");
    }
    // SAFETY: ttyname_r NUL-terminated the buffer on success.
    let slave_name = unsafe { CStr::from_ptr(name_buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // Non-blocking master so AsyncFd can drive it.
    // SAFETY: master is a valid fd.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // SAFETY: master/slave were just produced by openpty and are owned here.
    let master_owned = unsafe { OwnedFd::from_raw_fd(master) };
    let slave_owned = unsafe { OwnedFd::from_raw_fd(slave) };
    let afd = AsyncFd::with_interest(master_owned, Interest::READABLE | Interest::WRITABLE)
        .context("AsyncFd for pty master")?;
    Ok((afd, slave_name, slave_owned))
}

/// Default symlink path for a device id when `pty` is given without a value.
pub fn default_link(id: &str) -> String {
    format!("/tmp/tether-{id}.pty")
}

/// Stand up a virtual serial port for one device and start the bridge tasks.
/// On success the host can open `link` like a real serial device. Errors are
/// returned so the caller can warn and continue without the pty.
pub fn spawn_pty_bridge(
    id: String,
    link: String,
    buffer: RingBuffer,
    writer: SerialWriter,
    lock: Arc<WriterLock>,
) -> Result<()> {
    let (master_fd, slave_name, slave_keep) = create_pty()?;

    // Publish a stable symlink → the (dynamic) slave path. Replace any stale
    // link from a previous run.
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&slave_name, &link)
        .with_context(|| format!("symlink {link} -> {slave_name}"))?;

    tracing::info!(device = %id, link = %link, slave = %slave_name, "virtual serial port ready");

    let master = Arc::new(PtyMaster { fd: master_fd });

    // Keep the slave fd open for the daemon's lifetime so the pty doesn't get
    // torn down between host-tool open/close cycles. Intentionally leaked.
    std::mem::forget(slave_keep);

    // host tool → device: read bytes off the master, write them to the real
    // port. `writer.write().await` blocks until the byte hits the wire, so this
    // naturally backpressures at the device's baud rate.
    {
        let master = master.clone();
        let writer = writer.clone();
        let lock = lock.clone();
        let id = id.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let mut dropped_bytes: u64 = 0;
            let mut last_warn: Option<Instant> = None;
            loop {
                match master.read(&mut buf).await {
                    // No reader currently attached to the slave; idle briefly.
                    Ok(0) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Ok(n) => {
                        // The bridge has no session of its own — it never
                        // *takes* the writer lock, but it must back off while
                        // another session holds it exclusively (a `lock` RPC,
                        // typically guarding a flash over the real port).
                        // Writing tool bytes underneath that would interleave
                        // two writers on the same wire.
                        let exclusively_locked =
                            lock.holder.lock().as_ref().is_some_and(|h| h.exclusive);
                        if exclusively_locked {
                            dropped_bytes += n as u64;
                            let now = Instant::now();
                            let should_warn = match last_warn {
                                Some(t) => now.duration_since(t) >= Duration::from_secs(5),
                                None => true,
                            };
                            if should_warn {
                                tracing::warn!(
                                    device = %id, dropped_bytes,
                                    "virtual port writes blocked while device is locked"
                                );
                                last_warn = Some(now);
                            }
                            continue;
                        }
                        if let Err(e) = writer.write(buf[..n].to_vec()).await {
                            tracing::debug!(device = %id, error = %e, "pty→device write failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(device = %id, error = %e, "pty master read error");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
    }

    // device → host tool: fan the RX ring out to the master, starting at the
    // current head (only new bytes, like a fresh `tail`).
    {
        let master = master.clone();
        tokio::spawn(async move {
            let (mut cursor, _tail) = buffer.snapshot_seqs();
            let mut dropped_bytes: u64 = 0;
            let mut last_warn: Option<Instant> = None;
            loop {
                let (chunk, new_cursor, _lag) = buffer.read_from(cursor);
                cursor = new_cursor;
                if chunk.is_empty() {
                    buffer.wait().await;
                    continue;
                }
                // Bound how long we'll wait for the host tool to drain the
                // pty. macOS's pty kernel buffer is tiny (~1KB), and we keep
                // the slave fd open for the daemon's lifetime (see
                // `slave_keep` above) so the master never sees EOF — with no
                // reader attached, an unbounded `write_all` would park this
                // task forever, freezing the cursor until a tool eventually
                // attaches and then replaying a stale burst followed by a
                // gap. Timing out and dropping the chunk instead keeps the
                // bridge live; a freshly attached tool may still see up to
                // ~1KB of buffered history from the kernel's own pty buffer,
                // which is bounded and acceptable.
                match tokio::time::timeout(Duration::from_millis(200), master.write_all(&chunk))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(device = %id, error = %e, "device→pty write failed");
                    }
                    Err(_) => {
                        // Timed out mid-write. `write_all` may have already
                        // written a *prefix* of `chunk` before we cancelled
                        // it — we have no way to know how much landed, so we
                        // must not retry (that would risk duplicating
                        // whatever prefix got through). Drop the whole chunk;
                        // the cursor already advanced above, so the next
                        // iteration moves on rather than getting stuck here.
                        dropped_bytes += chunk.len() as u64;
                        let now = Instant::now();
                        let should_warn = match last_warn {
                            Some(t) => now.duration_since(t) >= Duration::from_secs(5),
                            None => true,
                        };
                        if should_warn {
                            tracing::warn!(
                                device = %id, dropped_bytes,
                                "no reader on virtual port; dropping device output"
                            );
                            last_warn = Some(now);
                        }
                    }
                }
            }
        });
    }

    Ok(())
}
