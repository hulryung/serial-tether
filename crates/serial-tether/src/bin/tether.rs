//! tether — non-interactive CLI for agents and shell scripts.
//!
//! Exit codes (uniform across subcommands):
//!   0   ok / matched
//!   1   usage error (clap)
//!   2   protocol / RPC error (other JSON-RPC errors)
//!   3   connection error (failed to dial the socket)
//!   4   device disconnected
//!   5   buffer overflow (`max_bytes` exceeded)
//!   6   lock contention (`preempt=fail` clash)
//!   7   unauthorized (TCP auth token missing or wrong)
//!   124 timeout (coreutils convention)

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_util::codec::Framed;

use tether_protocol::message::{ResponsePayload, RpcId};
use tether_protocol::{
    AttachParams, ExpectParams, HelloParams, Message, NdjsonCodec, Notification, Request,
    Response, RunParams, SendParams, UntilSpec,
};

#[derive(Parser, Debug)]
#[command(name = "tether", version, about = "tether — non-interactive client for tetherd")]
struct Cli {
    /// Daemon endpoint. Either a UDS path (e.g. /tmp/tetherd.sock) or
    /// `tcp://host:port` / `tcp:host:port` for a remote daemon.
    #[arg(short = 's', long, default_value = "/tmp/tetherd.sock", global = true)]
    socket: String,

    /// Emit raw JSON output instead of human-readable form.
    #[arg(long, global = true)]
    json: bool,

    /// Auth token for TCP transport (alternative to TETHER_AUTH_TOKEN env var).
    #[arg(long, global = true, env = "TETHER_AUTH_TOKEN")]
    auth_token: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Send data to the device. Does not wait for a response.
    Send {
        data: String,
        /// Treat `data` as base64.
        #[arg(long)]
        base64: bool,
        /// Append a line terminator to the data before sending.
        #[arg(long, default_value = "none", value_parser = ["none", "lf", "cr", "crlf"])]
        newline: String,
    },
    /// Wait until a pattern appears.
    Expect {
        pattern: String,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u32,
        #[arg(long)]
        literal: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_ansi: bool,
        #[arg(long, default_value_t = 8192)]
        max_output_bytes: u64,
    },
    /// Atomic send + expect executed on the server (holds the writer lock).
    Run {
        data: String,
        #[arg(short = 'u', long)]
        until: String,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u32,
        #[arg(long)]
        literal: bool,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_ansi: bool,
        /// Strip the echoed command line from the response.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        strip_echo: bool,
        #[arg(long, default_value_t = 8192)]
        max_output_bytes: u64,
        /// Behaviour when the writer lock is contended.
        #[arg(long, default_value = "queue", value_parser = ["queue", "fail", "force"])]
        preempt: String,
        /// Append a line terminator to the data before sending.
        /// Most embedded shells (U-Boot, busybox, Linux login) want `crlf` or `lf`.
        #[arg(long, default_value = "none", value_parser = ["none", "lf", "cr", "crlf"])]
        newline: String,
    },
    /// Show daemon status.
    Status,
    /// Stream device output to stdout.
    Tail {
        #[arg(long, default_value = "now")]
        from: String,
    },
    /// Send CR and wait until the device goes idle; print the last line as a
    /// prompt candidate.
    Sync {
        #[arg(long, default_value_t = 300)]
        idle_ms: u32,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(run(cli));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Timeout) => ExitCode::from(124),
        Err(CliError::DeviceDisconnected) => ExitCode::from(4),
        Err(CliError::BufferOverflow) => ExitCode::from(5),
        Err(CliError::LockContention) => ExitCode::from(6),
        Err(CliError::Unauthorized(msg)) => {
            eprintln!("tether: unauthorized: {msg}");
            ExitCode::from(7)
        }
        Err(CliError::Protocol(msg)) => {
            eprintln!("protocol error: {msg}");
            ExitCode::from(2)
        }
        Err(CliError::Connection(msg)) => {
            eprintln!("connection error: {msg}");
            ExitCode::from(3)
        }
    }
}

#[derive(Debug)]
enum CliError {
    Timeout,
    DeviceDisconnected,
    BufferOverflow,
    LockContention,
    Unauthorized(String),
    Protocol(String),
    Connection(String),
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        CliError::Connection(e.to_string())
    }
}

/// Parse `-s` value into a connection target. Recognized forms:
///   /path/to/sock       → UDS
///   tcp://host:port     → TCP
///   tcp:host:port       → TCP
fn endpoint_kind(s: &str) -> Endpoint<'_> {
    if let Some(addr) = s.strip_prefix("tcp://") {
        Endpoint::Tcp(addr)
    } else if let Some(addr) = s.strip_prefix("tcp:") {
        Endpoint::Tcp(addr)
    } else {
        Endpoint::Uds(s)
    }
}

enum Endpoint<'a> {
    Uds(&'a str),
    Tcp(&'a str),
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match endpoint_kind(&cli.socket) {
        Endpoint::Uds(path) => {
            let stream = UnixStream::connect(path)
                .await
                .map_err(|e| CliError::Connection(format!("connect {path:?}: {e}")))?;
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
        Endpoint::Tcp(addr) => {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| CliError::Connection(format!("connect tcp:{addr}: {e}")))?;
            let _ = stream.set_nodelay(true);
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
    }
}

async fn run_with_stream<S>(mut framed: Framed<S, NdjsonCodec>, cli: Cli) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut next_id: i64 = 1;

    call(
        &mut framed,
        &mut next_id,
        "hello",
        serde_json::to_value(HelloParams {
            protocol_version: tether_protocol::PROTOCOL_VERSION.to_string(),
            client: tether_protocol::ClientInfo {
                name: "tether".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                kind: "agent".into(),
            },
            auth_token: cli.auth_token.clone(),
        })
        .unwrap(),
    )
    .await?;

    match cli.cmd {
        Cmd::Status => {
            let v = call(&mut framed, &mut next_id, "status", json!({})).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Send { data, base64: is_b64, newline } => {
            let session_id = attach(&mut framed, &mut next_id, "now").await?;
            let mut p = SendParams {
                session_id: session_id.clone(),
                data: None,
                data_text: None,
                eat_echo: false,
            };
            if is_b64 {
                p.data = Some(data);
            } else {
                p.data_text = Some(apply_newline(data, &newline));
            }
            let v = call(&mut framed, &mut next_id, "send", serde_json::to_value(p).unwrap())
                .await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Expect {
            pattern,
            timeout_ms,
            literal,
            strip_ansi,
            max_output_bytes,
        } => {
            let session_id = attach(&mut framed, &mut next_id, "now").await?;
            let p = ExpectParams {
                session_id,
                pattern: pattern.clone(),
                regex: !literal,
                timeout_ms: Some(timeout_ms),
                strip_ansi,
                strip_echo: None,
                from_seq: None,
                max_bytes: None,
                max_output_bytes: Some(max_output_bytes),
            };
            match call(
                &mut framed,
                &mut next_id,
                "expect",
                serde_json::to_value(p).unwrap(),
            )
            .await
            {
                Ok(v) => print_match_result(&v, cli.json),
                Err(CliError::Timeout) => {
                    eprintln!(
                        "tether: timeout after {timeout_ms}ms waiting for {pattern:?}"
                    );
                    return Err(CliError::Timeout);
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Run {
            data,
            until,
            timeout_ms,
            literal,
            strip_ansi,
            strip_echo,
            max_output_bytes,
            preempt,
            newline,
        } => {
            let session_id = attach(&mut framed, &mut next_id, "now").await?;
            let p = RunParams {
                session_id,
                data: None,
                data_text: Some(apply_newline(data, &newline)),
                until: UntilSpec {
                    pattern: until.clone(),
                    regex: !literal,
                    strip_ansi,
                },
                timeout_ms: Some(timeout_ms),
                preempt,
                strip_echo,
                max_bytes: None,
                max_output_bytes: Some(max_output_bytes),
            };
            match call(
                &mut framed,
                &mut next_id,
                "run",
                serde_json::to_value(p).unwrap(),
            )
            .await
            {
                Ok(v) => print_match_result(&v, cli.json),
                Err(CliError::Timeout) => {
                    eprintln!(
                        "tether: timeout after {timeout_ms}ms waiting for {until:?}"
                    );
                    return Err(CliError::Timeout);
                }
                Err(e) => return Err(e),
            }
        }
        Cmd::Tail { from } => {
            let session_id = attach(&mut framed, &mut next_id, &from).await?;
            tail_loop(&mut framed, &session_id).await?;
        }
        Cmd::Sync { idle_ms, timeout_ms } => {
            let session_id = attach(&mut framed, &mut next_id, "now").await?;
            // Send a single CR.
            call(
                &mut framed,
                &mut next_id,
                "send",
                serde_json::to_value(SendParams {
                    session_id: session_id.clone(),
                    data: None,
                    data_text: Some("\r".into()),
                    eat_echo: false,
                })
                .unwrap(),
            )
            .await?;
            // Wait for `idle_ms` of silence (capped by `timeout_ms`).
            let prompt = sync_until_idle(&mut framed, &mut next_id, &session_id, idle_ms, timeout_ms)
                .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "prompt_candidate": prompt,
                })).unwrap());
            } else {
                println!("{}", prompt);
            }
        }
    }
    Ok(())
}

async fn attach<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    replay: &str,
) -> Result<String, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let p = AttachParams {
        session_id: None,
        mode: "rw".into(),
        replay: tether_protocol::message::ReplaySpec::Named(replay.into()),
        label: Some("tether".into()),
        flow_control: "drop_oldest".into(),
    };
    let v = call(framed, next_id, "attach", serde_json::to_value(p).unwrap()).await?;
    let id = v
        .get("session_id")
        .and_then(|s| s.as_str())
        .ok_or_else(|| CliError::Protocol("attach missing session_id".into()))?;
    Ok(id.to_string())
}

async fn call<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    method: &str,
    params: Value,
) -> Result<Value, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let id = *next_id;
    *next_id += 1;
    let req = Request::new(RpcId::Number(id), method, params);
    framed
        .send(Message::Request(req))
        .await
        .map_err(|e| CliError::Connection(e.to_string()))?;
    loop {
        let msg = match tokio::time::timeout(Duration::from_secs(120), framed.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(CliError::Protocol(format!("codec: {e}"))),
            Ok(None) => return Err(CliError::Connection("daemon closed connection".into())),
            Err(_) => return Err(CliError::Connection(format!("rpc timeout for {method}"))),
        };
        if let Message::Response(Response { id: rid, payload, .. }) = msg {
            if rid != RpcId::Number(id) {
                continue;
            }
            return match payload {
                ResponsePayload::Ok { result } => Ok(result),
                ResponsePayload::Err { error } => Err(map_rpc_error(error.code, error.message)),
            };
        }
        // Ignore notifications.
    }
}

fn map_rpc_error(code: i32, msg: String) -> CliError {
    use tether_protocol::error::ErrorCode as E;
    if code == E::Timeout.as_i32() {
        CliError::Timeout
    } else if code == E::DeviceDisconnected.as_i32() {
        CliError::DeviceDisconnected
    } else if code == E::BufferOverflow.as_i32() {
        CliError::BufferOverflow
    } else if code == E::LockContention.as_i32() {
        CliError::LockContention
    } else if code == E::Unauthorized.as_i32() {
        CliError::Unauthorized(msg)
    } else {
        CliError::Protocol(format!("rpc {code}: {msg}"))
    }
}

async fn tail_loop<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    session_id: &str,
) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    while let Some(item) = framed.next().await {
        let msg = item.map_err(|e| CliError::Protocol(e.to_string()))?;
        if let Message::Notification(Notification { method, params, .. }) = msg {
            if method == "data" {
                if let Some(p) = params {
                    if let Some(sid) = p.get("session_id").and_then(|s| s.as_str()) {
                        if sid != session_id {
                            continue;
                        }
                    }
                    if let Some(b64) = p.get("data").and_then(|s| s.as_str()) {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                            stdout.write_all(&bytes).ok();
                            stdout.flush().ok();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn sync_until_idle<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    session_id: &str,
    idle_ms: u32,
    timeout_ms: u32,
) -> Result<String, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started = std::time::Instant::now();
    let timeout_total = Duration::from_millis(timeout_ms as u64);
    let idle = Duration::from_millis(idle_ms as u64);

    let mut accumulated: Vec<u8> = Vec::new();
    let mut last_growth = std::time::Instant::now();

    // Use short expects to gather chunks and decide when idle.
    loop {
        if started.elapsed() >= timeout_total {
            break;
        }
        let remain = timeout_total.saturating_sub(started.elapsed());
        let step = idle.min(remain);
        // expect ".+" from the current cursor — a failure means nothing arrived in this window.
        let p = ExpectParams {
            session_id: session_id.to_string(),
            pattern: "(?s).+".into(),
            regex: true,
            timeout_ms: Some(step.as_millis() as u32),
            strip_ansi: false,
            strip_echo: None,
            from_seq: None,
            max_bytes: None,
            max_output_bytes: None,
        };
        match call(framed, next_id, "expect", serde_json::to_value(p).unwrap()).await {
            Ok(v) => {
                if let Some(b64) = v.get("before").and_then(|s| s.as_str()) {
                    let mut chunk = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap_or_default();
                    accumulated.append(&mut chunk);
                }
                if let Some(m) = v.get("match").and_then(|s| s.as_str()) {
                    accumulated.extend_from_slice(m.as_bytes());
                }
                last_growth = std::time::Instant::now();
            }
            Err(CliError::Timeout) => {
                if last_growth.elapsed() >= idle && !accumulated.is_empty() {
                    break;
                }
                // Nothing yet — keep waiting.
            }
            Err(e) => return Err(e),
        }
    }

    // The last non-empty line is the prompt candidate.
    let s = String::from_utf8_lossy(&accumulated);
    let last = s
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    Ok(last)
}

// ---------- Output formatting ----------

fn print_json_or_pairs(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            println!("{k}: {}", val_compact(val));
        }
    } else {
        println!("{}", v);
    }
}

fn print_match_result(v: &Value, force_json: bool) {
    if force_json {
        // Add a decoded `output` field so an LLM can use it directly.
        let mut out = v.clone();
        if let Some(obj) = out.as_object_mut() {
            if let Some(b64) = obj.get("before").and_then(|s| s.as_str()) {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    obj.insert("output".into(), json!(text));
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }
    // Human-friendly: response text to stdout, metadata to stderr.
    let before_b64 = v.get("before").and_then(|s| s.as_str()).unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(before_b64)
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    let truncated = v.get("truncated").and_then(|b| b.as_bool()).unwrap_or(false);
    if truncated || std::io::stderr().is_terminal() {
        let m = v.get("match").and_then(|s| s.as_str()).unwrap_or("");
        let dur = v.get("duration_ms").and_then(|n| n.as_u64());
        let mut summary = format!("[matched: {m:?}");
        if let Some(d) = dur {
            summary.push_str(&format!(", {d}ms"));
        }
        if truncated {
            if let Some(orig) = v.get("original_bytes").and_then(|n| n.as_u64()) {
                summary.push_str(&format!(", truncated from {orig}B"));
            } else {
                summary.push_str(", truncated");
            }
        }
        summary.push(']');
        eprintln!("{summary}");
    }
}

/// Append a line terminator if requested.
fn apply_newline(data: String, kind: &str) -> String {
    match kind {
        "lf" => format!("{data}\n"),
        "cr" => format!("{data}\r"),
        "crlf" => format!("{data}\r\n"),
        _ => data, // "none" or unknown
    }
}

fn val_compact(v: &Value) -> String {
    if v.is_object() || v.is_array() {
        serde_json::to_string(v).unwrap_or_default()
    } else {
        v.to_string()
    }
}
