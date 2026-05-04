//! JSON-RPC method handlers.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};

use tether_protocol::message::LockState;
use tether_protocol::{
    AttachParams, AttachResult, BufferInfo, DetachParams, DeviceInfo, ExpectMatch, ExpectParams,
    HelloParams, HelloResult, ProtocolError, RunParams, SendParams, SendResult, StatusResult,
};
use tether_protocol::error::ErrorCode;

use crate::conn::ConnState;
use crate::session::{FlowControl, SessionMode};
use crate::state::DaemonState;

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
    let (head, tail) = state.buffer.snapshot_seqs();
    let result = HelloResult {
        server_version: SERVER_VERSION.to_string(),
        protocol_version: tether_protocol::PROTOCOL_VERSION.to_string(),
        device: DeviceInfo {
            path: state.config.path.clone(),
            baud: state.config.baud,
            data_bits: 8,
            parity: "none".into(),
            stop_bits: 1,
            connected: true,
        },
        buffer: BufferInfo {
            capacity_bytes: state.buffer.capacity() as u64,
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
    let (head, tail) = state.buffer.snapshot_seqs();
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
    let session = state.sessions.create(mode, flow, p.label.clone(), cursor);
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
    let n = bytes.len() as u64;
    let sent_at_seq = state
        .writer
        .write(bytes)
        .await
        .map_err(|e| err_with(ErrorCode::InternalError, e.to_string()))?;
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
    let from_seq = match p.from_seq {
        Some(s) => s,
        None => session.lock().consumer_cursor,
    };
    let result = expect_loop(
        state,
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

    // Acquire the writer lock for the duration of this transaction.
    acquire_lock(state, &p.session_id, &p.preempt).await?;
    let _guard = LockGuard {
        state: state.clone(),
        session_id: p.session_id.clone(),
    };

    let started = Instant::now();
    let sent_at_seq = state
        .writer
        .write(bytes.clone())
        .await
        .map_err(|e| err_with(ErrorCode::InternalError, e.to_string()))?;

    let strip_echo_bytes: Option<&[u8]> = if p.strip_echo {
        Some(bytes.as_slice())
    } else {
        None
    };

    let result = expect_loop(
        state,
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

pub async fn status(
    state: &DaemonState,
    _conn: &ConnState,
    _params: Option<Value>,
) -> Result<Value, ProtocolError> {
    let (head, tail) = state.buffer.snapshot_seqs();
    let holder = state.lock.holder.lock().clone();
    let device_connected = state.device_state.lock().connected;
    let result = StatusResult {
        device: DeviceInfo {
            path: state.config.path.clone(),
            baud: state.config.baud,
            data_bits: 8,
            parity: "none".into(),
            stop_bits: 1,
            connected: device_connected,
        },
        buffer: BufferInfo {
            capacity_bytes: state.buffer.capacity() as u64,
            head_seq: head,
            tail_seq: tail,
        },
        lock: LockState {
            holder_session_id: holder,
            acquired_at: None,
        },
        sessions: state.sessions.snapshot(head),
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
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Params {
        #[serde(default = "default_wait")]
        wait: bool,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u32,
    }
    fn default_wait() -> bool { true }
    fn default_timeout_ms() -> u32 { 5000 }

    let p: Params = match params {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| err_with(ErrorCode::InvalidParams, e.to_string()))?,
        None => Params { wait: true, timeout_ms: 5000 },
    };

    // Subscribe to the next "reconnected" pulse *before* we fire the kick,
    // so we can't miss the signal even if the serial task is fast.
    let waiter = state.reconnected.notified();
    tokio::pin!(waiter);
    state.force_reconnect.notify_waiters();

    let mut reconnected_ok = false;
    if p.wait {
        let timeout = std::time::Duration::from_millis(p.timeout_ms as u64);
        match tokio::time::timeout(timeout, waiter).await {
            Ok(()) => reconnected_ok = true,
            Err(_) => {
                // Maybe device was already up before we even had to reconnect;
                // double-check current state.
                reconnected_ok = state.device_state.lock().connected;
            }
        }
    }
    let device_connected = state.device_state.lock().connected;

    Ok(serde_json::json!({
        "triggered": true,
        "reconnected": reconnected_ok,
        "device_connected": device_connected,
    }))
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

async fn acquire_lock(
    state: &DaemonState,
    session_id: &str,
    preempt: &str,
) -> Result<(), ProtocolError> {
    loop {
        {
            let mut h = state.lock.holder.lock();
            if h.is_none() {
                *h = Some(session_id.to_string());
                return Ok(());
            }
            // Re-entry by the same session is allowed.
            if h.as_deref() == Some(session_id) {
                return Ok(());
            }
        }
        match preempt {
            "fail" => return Err(err(ErrorCode::LockContention)),
            "force" => {
                // v0.3 will track outstanding tasks so we can really abort the
                // current holder. For now we simply swap the holder.
                let mut h = state.lock.holder.lock();
                *h = Some(session_id.to_string());
                return Ok(());
            }
            _ => {
                // queue — wait for the next release notification.
                state.lock.released.notified().await;
            }
        }
    }
}

struct LockGuard {
    state: DaemonState,
    session_id: String,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        let mut h = self.state.lock.holder.lock();
        if h.as_deref() == Some(self.session_id.as_str()) {
            *h = None;
            self.state.lock.released.notify_waiters();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn expect_loop(
    state: &DaemonState,
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
        let (chunk, new_cursor, _lag) = state.buffer.read_from(cursor);
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
            let _ = tokio::time::timeout(remain, state.buffer.wait()).await;
        } else {
            state.buffer.wait().await;
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
}
