use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::buffer::RingBuffer;
use crate::serial::{SerialConfig, SerialWriter};
use crate::session::SessionManager;

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
}
