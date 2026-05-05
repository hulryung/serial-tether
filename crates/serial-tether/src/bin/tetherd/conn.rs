//! Per-connection handler.
//!
//! Reads/writes NDJSON-framed messages and dispatches each request by method
//! name. Requests that may block (e.g. `expect`) are spawned on their own task
//! so they don't stall the rest of the connection.
//!
//! v0 has no `cancel` support — outstanding tasks are simply tracked and
//! aborted when the connection drops.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

use tether_protocol::{
    DataNotify, LagNotify, Message, NdjsonCodec, Notification, Response,
};

use crate::handlers;
use crate::state::DaemonState;

/// Per-connection ephemeral state.
pub struct ConnState {
    initialized: Mutex<bool>,
    client_kind: Mutex<Option<String>>,
    sessions: Mutex<HashSet<String>>,
    /// True for transports that require token authentication (TCP).
    /// False for transports authenticated by the OS (Unix domain sockets).
    requires_auth: bool,
}

impl ConnState {
    pub fn new(requires_auth: bool) -> Self {
        Self {
            initialized: Mutex::new(false),
            client_kind: Mutex::new(None),
            sessions: Mutex::new(HashSet::new()),
            requires_auth,
        }
    }
    pub fn requires_auth(&self) -> bool {
        self.requires_auth
    }
    pub fn set_initialized(&self, kind: String) {
        *self.initialized.lock() = true;
        *self.client_kind.lock() = Some(kind);
    }
    pub fn is_initialized(&self) -> bool {
        *self.initialized.lock()
    }
    pub fn add_session(&self, id: String) {
        self.sessions.lock().insert(id);
    }
    pub fn remove_session(&self, id: &str) {
        self.sessions.lock().remove(id);
    }
    pub fn has_session(&self, id: &str) -> bool {
        self.sessions.lock().contains(id)
    }
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.lock().iter().cloned().collect()
    }
}

pub async fn handle<S>(stream: S, state: DaemonState, requires_auth: bool) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let framed = Framed::new(stream, NdjsonCodec::new());
    let (mut sink, mut source) = framed.split();
    let conn = Arc::new(ConnState::new(requires_auth));

    // Outbound channel — handler tasks and the fan-out task push responses /
    // notifications here.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    // writer task
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Serial fan-out task — every time the buffer grows, compare each session's
    // notify cursor and emit any pending bytes as `data` notifications.
    let fanout_state = state.clone();
    let fanout_conn = conn.clone();
    let fanout_tx = out_tx.clone();
    let fanout_task = tokio::spawn(async move {
        loop {
            fanout_state.buffer.wait().await;
            for sid in fanout_conn.session_ids() {
                let Some(session) = fanout_state.sessions.get(&sid) else { continue };
                let cursor = session.lock().notify_cursor;
                let (chunk, new_cursor, lag) = fanout_state.buffer.read_from(cursor);
                if let Some(l) = lag {
                    let _ = fanout_tx.send(Message::Notification(Notification::new(
                        "lag",
                        serde_json::to_value(LagNotify {
                            session_id: sid.clone(),
                            dropped_bytes: l.dropped_bytes,
                            dropped_range: [l.dropped_range.0, l.dropped_range.1],
                            resume_seq: l.resume_seq,
                        })
                        .unwrap(),
                    )));
                }
                if !chunk.is_empty() {
                    let seq_start = new_cursor - chunk.len() as u64;
                    let _ = fanout_tx.send(Message::Notification(Notification::new(
                        "data",
                        serde_json::to_value(DataNotify {
                            session_id: sid.clone(),
                            seq: seq_start,
                            data: base64::engine::general_purpose::STANDARD
                                .encode(&chunk),
                        })
                        .unwrap(),
                    )));
                    session.lock().notify_cursor = new_cursor;
                }
            }
        }
    });

    // Device-event task — subscribe to broadcast and forward as the
    // `device` notification (PROTOCOL.md §7.5) so attached clients see
    // disconnect/reconnect transitions without polling status.
    let mut device_rx = state.device_events.subscribe();
    let device_tx = out_tx.clone();
    let device_task = tokio::spawn(async move {
        loop {
            match device_rx.recv().await {
                Ok(event) => {
                    let detail = event.detail.clone();
                    let mut params = serde_json::json!({
                        "kind": event.kind.as_str(),
                    });
                    if let Some(d) = detail {
                        params["detail"] = serde_json::Value::String(d);
                    }
                    let _ = device_tx.send(Message::Notification(Notification::new(
                        "device", params,
                    )));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(lagged = n, "device events lagged; resyncing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Message dispatch loop.
    while let Some(item) = source.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error=%e, "codec error; closing");
                break;
            }
        };
        let req = match msg {
            Message::Request(r) => r,
            Message::Notification(_) => continue, // v0 ignores client→server notifications
            Message::Response(_) => continue,
        };

        let state_cl = state.clone();
        let conn_cl = conn.clone();
        let out_tx_cl = out_tx.clone();
        // Run each request on its own task so a blocking `expect` doesn't
        // hold up subsequent requests.
        tokio::spawn(async move {
            let resp = dispatch(&state_cl, &conn_cl, &req.method, req.params).await;
            let response = match resp {
                Ok(value) => Response::ok(req.id, value),
                Err(e) => Response::err(req.id, e),
            };
            let _ = out_tx_cl.send(Message::Response(response));
        });
    }

    // Cleanup.
    fanout_task.abort();
    device_task.abort();
    drop(out_tx);
    let _ = writer_task.await;
    for sid in conn.session_ids() {
        state.sessions.remove(&sid);
    }
    Ok(())
}

async fn dispatch(
    state: &DaemonState,
    conn: &ConnState,
    method: &str,
    params: Option<Value>,
) -> Result<Value, tether_protocol::ProtocolError> {
    use tether_protocol::error::ErrorCode;
    match method {
        "hello" => handlers::hello(state, conn, params).await,
        "attach" => handlers::attach(state, conn, params).await,
        "detach" => handlers::detach(state, conn, params).await,
        "send" => handlers::send(state, conn, params).await,
        "expect" => handlers::expect(state, conn, params).await,
        "run" => handlers::run(state, conn, params).await,
        "status" => handlers::status(state, conn, params).await,
        "list_ports" => handlers::list_ports(state, conn, params).await,
        "set_device" => handlers::set_device(state, conn, params).await,
        "reconnect" => handlers::reconnect(state, conn, params).await,
        _ => Err(tether_protocol::ProtocolError::new(ErrorCode::MethodNotFound)
            .with_message(format!("method not found: {method}"))),
    }
}
