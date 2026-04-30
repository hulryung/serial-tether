//! Session manager.
//!
//! A session bundles identity, a pair of read cursors, mode, and the
//! flow-control policy. v0 supports only connection-owned sessions; lock
//! preemption and 30-second resume land in v0.3.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use uuid::Uuid;

use tether_protocol::SessionInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Rw,
    Ro,
}

impl SessionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rw" => Some(Self::Rw),
            "ro" => Some(Self::Ro),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rw => "rw",
            Self::Ro => "ro",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl {
    DropOldest,
    Disconnect,
}

impl FlowControl {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "drop_oldest" => Some(Self::DropOldest),
            "disconnect" => Some(Self::Disconnect),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub label: Option<String>,
    pub mode: SessionMode,
    /// Stored for v0.3 (used by the planned `disconnect` flow-control policy).
    #[allow(dead_code)]
    pub flow_control: FlowControl,
    /// Cursor used by RPCs that return data in the response (`expect`, `run`).
    pub consumer_cursor: u64,
    /// Cursor that tracks `data` notification fan-out progress.
    pub notify_cursor: u64,
    /// Stored for v0.3 (session GC after detach).
    #[allow(dead_code)]
    pub created_at: Instant,
}

impl Session {
    pub fn to_info(&self, head_seq: u64) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            mode: self.mode.as_str().to_string(),
            cursor_seq: self.consumer_cursor,
            lag_bytes: head_seq.saturating_sub(self.notify_cursor),
        }
    }
}

#[derive(Default)]
pub struct SessionManager {
    inner: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

impl SessionManager {
    pub fn new() -> Self { Self::default() }

    pub fn create(
        &self,
        mode: SessionMode,
        flow_control: FlowControl,
        label: Option<String>,
        cursor_seq: u64,
    ) -> Arc<Mutex<Session>> {
        let id = Uuid::now_v7().to_string();
        let s = Arc::new(Mutex::new(Session {
            id: id.clone(),
            label,
            mode,
            flow_control,
            consumer_cursor: cursor_seq,
            notify_cursor: cursor_seq,
            created_at: Instant::now(),
        }));
        self.inner.lock().insert(id, s.clone());
        s
    }

    pub fn get(&self, id: &str) -> Option<Arc<Mutex<Session>>> {
        self.inner.lock().get(id).cloned()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Mutex<Session>>> {
        self.inner.lock().remove(id)
    }

    pub fn snapshot(&self, head_seq: u64) -> Vec<SessionInfo> {
        self.inner
            .lock()
            .values()
            .map(|s| s.lock().to_info(head_seq))
            .collect()
    }
}
