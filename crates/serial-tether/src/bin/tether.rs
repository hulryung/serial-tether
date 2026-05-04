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

    /// If no subcommand is given, drops into the interactive shell.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug, Clone)]
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
    /// Interactive raw-mode shell. Type to send keystrokes, see live device
    /// output. Press Ctrl-A then Q to quit, Ctrl-A then Ctrl-A to send a
    /// literal Ctrl-A, Ctrl-A then ? for help.
    Shell {
        /// Replay buffer position when attaching.
        #[arg(long, default_value = "now", value_parser = ["start", "now"])]
        from: String,
    },
    /// Tell the daemon to drop and reopen the serial device. Useful when
    /// the bus is wedged but the daemon thinks it's still connected.
    Reconnect {
        /// Don't wait for the device to come back online.
        #[arg(long)]
        nowait: bool,
        /// How long to wait for the device to reopen.
        #[arg(long, default_value_t = 5000)]
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
            // The message itself is already self-contained (multi-line) for
            // the common cases — print it verbatim instead of mashing it on
            // one line behind a "connection error:" prefix.
            if msg.contains('\n') {
                eprintln!("{msg}");
            } else {
                eprintln!("tether: connection error: {msg}");
            }
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
                .map_err(|e| make_uds_connect_error(path, e))?;
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
        Endpoint::Tcp(addr) => {
            let stream = TcpStream::connect(addr)
                .await
                .map_err(|e| make_tcp_connect_error(addr, e))?;
            let _ = stream.set_nodelay(true);
            let framed = Framed::new(stream, NdjsonCodec::new());
            run_with_stream(framed, cli).await
        }
    }
}

/// Build a friendly multi-line error when the local Unix socket can't be
/// dialed — the most common cause is "the daemon hasn't been started yet".
fn make_uds_connect_error(path: &str, e: std::io::Error) -> CliError {
    use std::io::ErrorKind;
    let cause = match e.kind() {
        ErrorKind::NotFound => "socket file does not exist (daemon not running?)",
        ErrorKind::ConnectionRefused => "connection refused (stale socket from a crashed daemon?)",
        _ => return CliError::Connection(format!("connect {path}: {e}")),
    };
    let msg = format!(
        "tether: cannot connect to {path} — {cause}\n\
         \n\
         Start the daemon first:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200\n\
         \n\
         Or expose it for remote agents:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp\n\
         \n\
         (Run `tetherd --help` for all options.)"
    );
    CliError::Connection(msg)
}

fn make_tcp_connect_error(addr: &str, e: std::io::Error) -> CliError {
    let msg = format!(
        "tether: cannot connect to tcp://{addr} — {e}\n\
         \n\
         Check that the daemon is running on the remote host with:\n\
         \n\
         \x20\x20tetherd -D /dev/tty.usbserial-XXXX -b 115200 --tcp\n\
         \n\
         If a firewall is in the way, ensure the port is open.\n\
         For TCP, set TETHER_AUTH_TOKEN or pass --auth-token."
    );
    CliError::Connection(msg)
}

async fn run_with_stream<S>(mut framed: Framed<S, NdjsonCodec>, cli: Cli) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
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

    let cmd = cli.cmd.clone().unwrap_or(Cmd::Shell { from: "now".into() });
    match cmd {
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
        Cmd::Shell { from } => {
            let session_id = attach(&mut framed, &mut next_id, &from).await?;
            shell_loop(framed, session_id).await?;
            return Ok(());
        }
        Cmd::Reconnect { nowait, timeout_ms } => {
            let v = call(
                &mut framed,
                &mut next_id,
                "reconnect",
                json!({
                    "wait": !nowait,
                    "timeout_ms": timeout_ms,
                }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let reconnected = v.get("reconnected").and_then(|b| b.as_bool()).unwrap_or(false);
                let connected = v.get("device_connected").and_then(|b| b.as_bool()).unwrap_or(false);
                if connected {
                    eprintln!("tether: device reopened");
                } else if reconnected {
                    eprintln!("tether: reconnect succeeded but device state is uncertain");
                } else {
                    eprintln!(
                        "tether: reconnect requested but device is still disconnected after {timeout_ms}ms"
                    );
                    return Err(CliError::DeviceDisconnected);
                }
            }
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

/// Interactive raw-mode shell. Spins up:
/// - a sender task that drains an mpsc channel of outgoing JSON-RPC messages,
/// - a reader task that turns incoming `data` notifications into stdout writes,
/// - a stdin reader thread (blocking) that pushes raw bytes into a channel.
///
/// The main async loop pulls stdin chunks, scans for the Ctrl-A escape, and
/// sends keystrokes to the device as `send` RPCs. RAII guard restores the
/// terminal mode on any exit path (including panic).
async fn shell_loop<S>(framed: Framed<S, NdjsonCodec>, session_id: String) -> Result<(), CliError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use std::io::{IsTerminal as _, Write as _};

    /// Restores terminal mode whenever this guard drops (Ok, Err, or panic).
    struct RawModeGuard {
        active: bool,
    }
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            if self.active {
                let _ = disable_raw_mode();
                eprint!("\r\n");
            }
        }
    }

    let is_tty = std::io::stdin().is_terminal();
    let _guard = if is_tty {
        enable_raw_mode().map_err(|e| CliError::Connection(format!("raw mode: {e}")))?;
        RawModeGuard { active: true }
    } else {
        // Piped stdin (scripts, tests) — skip raw mode but everything else works.
        RawModeGuard { active: false }
    };

    if is_tty {
        eprint!(
            "\r\n[tether shell — Ctrl-A then Q to quit, Ctrl-A ? for help]\r\n\r\n"
        );
    }

    let (mut sink, mut source) = framed.split();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Sender task: serialise outgoing messages to the daemon.
    let sender = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Reader task: stream `data` notifications for our session straight to stdout.
    let target = session_id.clone();
    let reader = tokio::spawn(async move {
        while let Some(item) = source.next().await {
            let Ok(msg) = item else { break };
            if let Message::Notification(Notification { method, params, .. }) = msg {
                if method != "data" {
                    continue;
                }
                let Some(p) = params else { continue };
                if let Some(sid) = p.get("session_id").and_then(|s| s.as_str()) {
                    if sid != target {
                        continue;
                    }
                }
                if let Some(b64) = p.get("data").and_then(|s| s.as_str()) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
            }
        }
    });

    // Stdin reader thread (blocking). Pushes chunks into an mpsc.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut next_id: i64 = 1_000_000;
    let mut escape_pending = false;

    // Helper closure: send a chunk of bytes via the message channel.
    let send_bytes = |bytes: &[u8], next_id: &mut i64| -> bool {
        if bytes.is_empty() {
            return true;
        }
        let p = SendParams {
            session_id: session_id.clone(),
            data: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            data_text: None,
            eat_echo: false,
        };
        let id = *next_id;
        *next_id += 1;
        let req = Request::new(RpcId::Number(id), "send", serde_json::to_value(p).unwrap());
        msg_tx.send(Message::Request(req)).is_ok()
    };

    let print_help = || {
        eprint!(
            "\r\n[Ctrl-A Q  : quit]\r\n\
             [Ctrl-A ?  : help]\r\n\
             [Ctrl-A ^A : send literal Ctrl-A]\r\n\r\n"
        );
    };

    'main: while let Some(chunk) = stdin_rx.recv().await {
        let mut buffer: Vec<u8> = Vec::with_capacity(chunk.len());
        for &b in &chunk {
            if escape_pending {
                escape_pending = false;
                match b {
                    b'q' | b'Q' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                        }
                        break 'main;
                    }
                    0x01 => buffer.push(0x01), // literal Ctrl-A
                    b'?' | b'h' | b'H' => print_help(),
                    other => {
                        eprint!("\r\n[unknown escape: Ctrl-A {}]\r\n", other as char);
                    }
                }
            } else if b == 0x01 {
                escape_pending = true;
            } else {
                buffer.push(b);
            }
        }
        if !buffer.is_empty() && !send_bytes(&buffer, &mut next_id) {
            break;
        }
    }

    // Cleanup: drop sender (flushes remaining), then await tasks.
    drop(msg_tx);
    let _ = sender.await;
    reader.abort();
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
