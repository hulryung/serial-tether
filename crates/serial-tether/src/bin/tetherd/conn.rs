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

    // Serial fan-out tasks — one per device. Each watches that device's
    // buffer and emits `data` / `lag` notifications for any session bound
    // to it. Phase 6 will pull this into a tighter helper, but the current
    // shape already works for N devices.
    //
    // Cadence: we always try to drain the buffer first, then only `wait`
    // when there's nothing to emit. This sidesteps a notify-race where
    // `notify_waiters()` from `RingBuffer::push` is lost while this task
    // is mid-emit — without it, fast bursts of serial data become a
    // sawtooth ("plays then pauses then plays") because we'd sleep on
    // wait() with un-emitted data already buffered.
    let mut fanout_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for dev in state.devices.values() {
        let fanout_state = state.clone();
        let fanout_conn = conn.clone();
        let fanout_tx = out_tx.clone();
        let dev = dev.clone();
        fanout_tasks.push(tokio::spawn(async move {
            loop {
                let mut emitted_anything = false;
                for sid in fanout_conn.session_ids() {
                    let Some(session) = fanout_state.sessions.get(&sid) else { continue };
                    // Skip sessions bound to a different device.
                    if session.lock().device_id != dev.id {
                        continue;
                    }
                    let cursor = session.lock().notify_cursor;
                    let (chunk, new_cursor, lag) = dev.buffer.read_from(cursor);
                    if let Some(l) = lag {
                        let _ = fanout_tx.send(Message::Notification(Notification::new(
                            "lag",
                            serde_json::to_value(LagNotify {
                                session_id: sid.clone(),
                                dropped_bytes: l.dropped_bytes,
                                dropped_range: [l.dropped_range.0, l.dropped_range.1],
                                resume_seq: l.resume_seq,
                                device_id: Some(dev.id.clone()),
                            })
                            .unwrap(),
                        )));
                        emitted_anything = true;
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
                                device_id: Some(dev.id.clone()),
                            })
                            .unwrap(),
                        )));
                        session.lock().notify_cursor = new_cursor;
                        emitted_anything = true;
                    }
                }
                // Only sleep on `wait()` when no session had pending data.
                // If any session emitted, immediately loop again to pick up
                // bytes that arrived while we were encoding/sending — those
                // arrivals' `notify_waiters()` would have been a no-op.
                if !emitted_anything {
                    dev.buffer.wait().await;
                }
            }
        }));
    }

    // Device-event tasks — one per device. Forward broadcast events as the
    // `device` notification (PROTOCOL.md §7.5) tagged with `device_id` so
    // multi-device clients can route them.
    let mut device_event_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for dev in state.devices.values() {
        let mut device_rx = dev.events.subscribe();
        let device_tx = out_tx.clone();
        let dev_id = dev.id.clone();
        device_event_tasks.push(tokio::spawn(async move {
            loop {
                match device_rx.recv().await {
                    Ok(event) => {
                        let detail = event.detail.clone();
                        let mut params = serde_json::json!({
                            "kind": event.kind.as_str(),
                            "device_id": dev_id,
                        });
                        if let Some(d) = detail {
                            params["detail"] = serde_json::Value::String(d);
                        }
                        let _ = device_tx.send(Message::Notification(Notification::new(
                            "device", params,
                        )));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(device=%dev_id, lagged = n, "device events lagged; resyncing");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

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

        // Ordering matters. Requests on a single connection must take effect
        // in the order they arrive — otherwise a pasted multi-line block or a
        // rapid burst of `send`s races at the device writer and reaches the
        // wire out of order (observed as a rotated / garbled command line, and
        // worse with longer input since more chunks race). We therefore handle
        // requests inline, in arrival order, and only spawn the handful of
        // methods that block on device output, so a long `expect` still
        // doesn't stall the rest of the connection.
        let state_cl = state.clone();
        let conn_cl = conn.clone();
        if is_blocking_method(&req.method) {
            let out_tx_cl = out_tx.clone();
            tokio::spawn(async move {
                let resp = dispatch(&state_cl, &conn_cl, &req.method, req.params).await;
                let response = match resp {
                    Ok(value) => Response::ok(req.id, value),
                    Err(e) => Response::err(req.id, e),
                };
                let _ = out_tx_cl.send(Message::Response(response));
            });
        } else {
            let resp = dispatch(&state_cl, &conn_cl, &req.method, req.params).await;
            let response = match resp {
                Ok(value) => Response::ok(req.id, value),
                Err(e) => Response::err(req.id, e),
            };
            let _ = out_tx.send(Message::Response(response));
        }
    }

    // Cleanup.
    for t in &fanout_tasks { t.abort(); }
    for t in &device_event_tasks { t.abort(); }
    drop(out_tx);
    let _ = writer_task.await;
    for sid in conn.session_ids() {
        // Free a writer lock this session held before it's gone — otherwise
        // an exclusive `lock` from a client that crashed/disconnected mid-
        // flash would strand the device locked forever.
        handlers::release_lock_for_session(&state, &sid);
        state.sessions.remove(&sid);
    }
    Ok(())
}

/// Methods that may block for a long time waiting on device output (a pattern
/// match or a reconnect) — or, for `lock` with `preempt:"queue"`, on another
/// session's release. These run on their own task so they don't stall
/// in-order processing of the rest of the connection. Everything else — most
/// importantly `send` — is dispatched inline, in arrival order, so writes reach
/// the device in exactly the order the client issued them.
fn is_blocking_method(method: &str) -> bool {
    matches!(method, "expect" | "run" | "reconnect" | "lock")
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
        "lock" => handlers::lock(state, conn, params).await,
        "unlock" => handlers::unlock(state, conn, params).await,
        "status" => handlers::status(state, conn, params).await,
        "list_ports" => handlers::list_ports(state, conn, params).await,
        "list_devices" => handlers::list_devices(state, conn, params).await,
        "set_device" => handlers::set_device(state, conn, params).await,
        "reconnect" => handlers::reconnect(state, conn, params).await,
        "send_break" => handlers::send_break(state, conn, params).await,
        "set_dtr" => handlers::set_dtr(state, conn, params).await,
        "set_rts" => handlers::set_rts(state, conn, params).await,
        "read_modem_status" => handlers::read_modem_status(state, conn, params).await,
        "disconnect_device" => handlers::disconnect_device(state, conn, params).await,
        "connect_device" => handlers::connect_device(state, conn, params).await,
        _ => Err(tether_protocol::ProtocolError::new(ErrorCode::MethodNotFound)
            .with_message(format!("method not found: {method}"))),
    }
}
