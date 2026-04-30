//! Single ring buffer with per-session cursors.
//!
//! Holds raw bytes received from the serial port. Each attached session sees
//! only the bytes after its own cursor via fan-out. When a session falls so far
//! behind that its cursor would point before the ring's tail, it receives a lag
//! notification and its cursor jumps forward to the current tail.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct LagInfo {
    pub dropped_bytes: u64,
    pub dropped_range: (u64, u64),
    pub resume_seq: u64,
}

#[derive(Debug)]
struct Inner {
    /// We don't store (seq, byte) pairs; the seq of `buf[i]` is `tail_seq + i`.
    /// (tail = oldest, head = newest + 1)
    buf: VecDeque<u8>,
    capacity: usize,
    head_seq: u64,                // seq of the next byte to push (== last + 1)
    tail_seq: u64,                // seq of buf[0]
}

#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                buf: VecDeque::with_capacity(capacity),
                capacity,
                head_seq: 0,
                tail_seq: 0,
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        {
            let mut g = self.inner.lock();
            for &b in bytes {
                if g.buf.len() == g.capacity {
                    g.buf.pop_front();
                    g.tail_seq += 1;
                }
                g.buf.push_back(b);
            }
            g.head_seq += bytes.len() as u64;
        }
        self.notify.notify_waiters();
    }

    pub fn snapshot_seqs(&self) -> (u64, u64) {
        let g = self.inner.lock();
        (g.head_seq, g.tail_seq)
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().capacity
    }

    /// Return all bytes available after `from_seq` in a single read.
    /// Returns `(data, new_cursor_seq, lag)` — `lag.is_some()` when some bytes
    /// were dropped before `from_seq` could be served.
    pub fn read_from(&self, from_seq: u64) -> (Vec<u8>, u64, Option<LagInfo>) {
        let g = self.inner.lock();
        if from_seq >= g.head_seq {
            return (Vec::new(), from_seq, None);
        }
        let (effective_from, lag) = if from_seq < g.tail_seq {
            (
                g.tail_seq,
                Some(LagInfo {
                    dropped_bytes: g.tail_seq - from_seq,
                    dropped_range: (from_seq, g.tail_seq),
                    resume_seq: g.tail_seq,
                }),
            )
        } else {
            (from_seq, None)
        };
        let start = (effective_from - g.tail_seq) as usize;
        let end = (g.head_seq - g.tail_seq) as usize;
        let mut out = Vec::with_capacity(end - start);
        for i in start..end {
            out.push(g.buf[i]);
        }
        (out, g.head_seq, lag)
    }

    /// Wait until new data is pushed.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read() {
        let r = RingBuffer::new(8);
        r.push(b"hello");
        let (head, tail) = r.snapshot_seqs();
        assert_eq!(head, 5);
        assert_eq!(tail, 0);
        let (data, cursor, lag) = r.read_from(0);
        assert_eq!(data, b"hello");
        assert_eq!(cursor, 5);
        assert!(lag.is_none());
    }

    #[test]
    fn cursor_at_head_returns_empty() {
        let r = RingBuffer::new(8);
        r.push(b"abc");
        let (data, cursor, lag) = r.read_from(3);
        assert!(data.is_empty());
        assert_eq!(cursor, 3);
        assert!(lag.is_none());
    }

    #[test]
    fn overflow_drops_oldest_and_signals_lag() {
        let r = RingBuffer::new(4);
        r.push(b"abcd");           // seq 0..4
        r.push(b"efgh");           // seq 4..8, tail moves to 4
        let (head, tail) = r.snapshot_seqs();
        assert_eq!(head, 8);
        assert_eq!(tail, 4);
        let (data, cursor, lag) = r.read_from(2);
        assert_eq!(data, b"efgh");
        assert_eq!(cursor, 8);
        let l = lag.expect("should report lag");
        assert_eq!(l.dropped_bytes, 2);
        assert_eq!(l.dropped_range, (2, 4));
        assert_eq!(l.resume_seq, 4);
    }
}
