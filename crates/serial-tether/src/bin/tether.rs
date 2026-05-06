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
    ///
    /// Default: `/tmp/tetherd.sock`, or `/tmp/tetherd-<NAME>.sock` if
    /// `--name` is set.
    #[arg(short = 's', long, global = true, conflicts_with = "name")]
    socket: Option<String>,

    /// Connect to the named daemon at `/tmp/tetherd-<NAME>.sock`.
    ///
    /// Convenience alias when `tetherd` was started with the same `--name`.
    /// Mutually exclusive with `--socket` (use `-s tcp://...` for TCP).
    #[arg(long, global = true, value_name = "NAME")]
    name: Option<String>,

    /// Target device id within the daemon. Required when the daemon serves
    /// more than one device (otherwise the daemon answers `AmbiguousDevice`).
    /// Single-device daemons may omit this — it falls through to the only
    /// device. Distinct from `--name` (which selects which *daemon*).
    #[arg(short = 'd', long, global = true, value_name = "ID")]
    device_id: Option<String>,

    /// Emit raw JSON output instead of human-readable form.
    #[arg(long, global = true)]
    json: bool,

    /// Auth token for TCP transport (alternative to TETHER_AUTH_TOKEN env var).
    #[arg(long, global = true, env = "TETHER_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// On a `device_disconnected` reply, automatically issue `reconnect`
    /// and retry the original RPC once. Useful for long-running scripts
    /// that should ride out a USB hiccup. Off by default.
    #[arg(long, global = true)]
    auto_reconnect: bool,

    /// Standalone mode: also start a private `tetherd` for this device,
    /// run the requested command (or shell), then shut the daemon down
    /// when the client exits. Same UX as `tio /dev/ttyUSB0`.
    /// Cannot be combined with `-s tcp://...` or an explicit `-s` socket.
    #[arg(short = 'D', long, global = true, value_name = "DEVICE")]
    device: Option<String>,

    /// Baud rate for standalone mode (only used when `-D` is given).
    /// Not `global` so the `config` subcommand can have its own optional
    /// `--baud` flag for `set_device`.
    #[arg(short = 'b', long, default_value_t = 115200)]
    baud: u32,

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
    /// List serial ports the daemon machine knows about.
    ///
    /// AI-agent tip: pass `--json` to get a stable schema. Returns an empty
    /// `ports` array on platforms that can't enumerate (e.g. restricted
    /// containers).
    Ports,
    /// Show or change the live serial configuration.
    ///
    /// With no flags: prints the current device settings.
    /// With one or more of --baud/--data-bits/--parity/--stop-bits/--flow:
    /// applies the partial update via `set_device` and prints the new state.
    Config {
        /// New baud rate (e.g. 9600, 115200, 921600).
        #[arg(long)]
        baud: Option<u32>,
        /// 5, 6, 7, or 8.
        #[arg(long, value_parser = clap::value_parser!(u8).range(5..=8))]
        data_bits: Option<u8>,
        /// none | odd | even.
        #[arg(long, value_parser = ["none", "odd", "even"])]
        parity: Option<String>,
        /// 1 or 2.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=2))]
        stop_bits: Option<u8>,
        /// none | software | hardware.
        #[arg(long = "flow", value_parser = ["none", "software", "hardware"])]
        flow_control: Option<String>,
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
    /// List all devices managed by the daemon.
    ///
    /// AI-agent tip: pair with `--json`. Output also tells you which id is
    /// the default (used when `--device` is omitted on single-device daemons).
    #[command(name = "list-devices")]
    ListDevices,
    /// Send a serial BREAK pulse to the device.
    Break {
        /// Break duration. Default 250ms (matches tio).
        #[arg(long, default_value_t = 250)]
        duration_ms: u32,
    },
    /// Drive the DTR (Data Terminal Ready) output line.
    Dtr {
        /// `on` asserts the line; `off` deasserts.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Drive the RTS (Request To Send) output line.
    Rts {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Read the four input modem status lines (CTS / DSR / RI / DCD).
    Lines,
    /// Explicitly close the serial port. The daemon stops auto-reconnecting
    /// until `tether connect` lifts the hold.
    Disconnect,
    /// Reopen a port closed by `tether disconnect`. Has no effect if the
    /// device wasn't explicitly disconnected.
    Connect,
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

async fn run(mut cli: Cli) -> Result<(), CliError> {
    // Resolve --socket vs --name vs default into a single canonical endpoint
    // string, then keep using cli.socket as the source of truth (so
    // standalone mode can still rewrite it after spawning the daemon).
    let resolved = match (&cli.socket, &cli.name) {
        (Some(s), _) => s.clone(),
        (None, Some(n)) => format!("/tmp/tetherd-{n}.sock"),
        (None, None) => "/tmp/tetherd.sock".to_string(),
    };
    cli.socket = Some(resolved);

    // Standalone mode: spawn our own tetherd, then continue as a normal
    // client against its ephemeral UDS. The guard kills the child when
    // we exit, regardless of how (clean exit, error, panic).
    let _daemon_guard = if let Some(device) = cli.device.clone() {
        if matches!(endpoint_kind(cli.socket.as_deref().unwrap()), Endpoint::Tcp(_)) {
            return Err(CliError::Connection(
                "tether: -D and -s tcp://... are mutually exclusive (standalone mode is local UDS only)".into(),
            ));
        }
        Some(spawn_embedded_daemon(&device, cli.baud, &mut cli).await?)
    } else {
        None
    };

    match endpoint_kind(cli.socket.as_deref().unwrap()) {
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

/// RAII guard that owns the spawned daemon child. On drop, sends SIGTERM
/// (best-effort) and removes the temporary socket file.
struct DaemonGuard {
    child: Option<std::process::Child>,
    socket_path: std::path::PathBuf,
    log_path: Option<std::path::PathBuf>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGTERM first; if the daemon ignores it we'd ideally escalate,
            // but for a one-shot client exit "best effort" is fine.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        if let Some(p) = &self.log_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Find the `tetherd` binary, preferring the one next to the running
/// `tether` so that (`/usr/local/bin/tether`, `/usr/local/bin/tetherd`)
/// stays in sync.
fn find_tetherd() -> Result<std::path::PathBuf, CliError> {
    // 1. Same dir as our own binary.
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("tetherd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    // 2. Fall back to PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = std::path::Path::new(dir).join("tetherd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(CliError::Connection(
        "tether: -D needs the `tetherd` binary; install it with the same package as tether"
            .into(),
    ))
}

async fn spawn_embedded_daemon(
    device: &str,
    baud: u32,
    cli: &mut Cli,
) -> Result<DaemonGuard, CliError> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let tetherd = find_tetherd()?;
    let pid = std::process::id();
    let nonce: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let sock_path = std::path::PathBuf::from(format!("/tmp/tether-{pid}-{nonce}.sock"));
    let log_path = std::path::PathBuf::from(format!("/tmp/tether-{pid}-{nonce}.log"));

    let log_file = std::fs::File::create(&log_path).map_err(|e| {
        CliError::Connection(format!("tether: cannot create daemon log {log_path:?}: {e}"))
    })?;
    let log_for_err = log_file.try_clone().map_err(|e| {
        CliError::Connection(format!("tether: dup log fd: {e}"))
    })?;

    let child = Command::new(&tetherd)
        .arg("-D").arg(device)
        .arg("-b").arg(baud.to_string())
        .arg("-s").arg(&sock_path)
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_for_err))
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&log_path);
            CliError::Connection(format!(
                "tether: failed to spawn {tetherd:?}: {e}"
            ))
        })?;

    let mut guard = DaemonGuard {
        child: Some(child),
        socket_path: sock_path.clone(),
        log_path: Some(log_path.clone()),
    };

    // Wait up to 5s for the daemon to bind its socket. If the child dies
    // before then, surface the captured banner/log so the user can see why.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if sock_path.exists() {
            // Override the socket the rest of the client logic will dial.
            cli.socket = Some(sock_path.to_string_lossy().into_owned());
            return Ok(guard);
        }
        // Did the child exit early?
        if let Some(child) = guard.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                guard.child = None; // already reaped
                return Err(CliError::Connection(format!(
                    "tether: embedded tetherd exited (status {status}) before binding socket\n\n{log}"
                )));
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(CliError::Connection(format!(
        "tether: embedded tetherd did not bind {sock_path:?} within 5s"
    )))
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
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref()).await?;
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
            let v = call_or_retry(
                &mut framed,
                &mut next_id,
                "send",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
            )
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
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref()).await?;
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
            match call_or_retry(
                &mut framed,
                &mut next_id,
                "expect",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
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
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref()).await?;
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
            match call_or_retry(
                &mut framed,
                &mut next_id,
                "run",
                serde_json::to_value(p).unwrap(),
                cli.auto_reconnect,
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
            let session_id = attach(&mut framed, &mut next_id, &from, cli.device_id.as_deref()).await?;
            tail_loop(&mut framed, &session_id).await?;
        }
        Cmd::Shell { from } => {
            let session_id = attach(&mut framed, &mut next_id, &from, cli.device_id.as_deref()).await?;
            shell_loop(framed, session_id).await?;
            return Ok(());
        }
        Cmd::Ports => {
            let v = call(&mut framed, &mut next_id, "list_ports", json!({})).await?;
            print_ports(&v, cli.json);
        }
        Cmd::Config {
            baud,
            data_bits,
            parity,
            stop_bits,
            flow_control,
        } => {
            let any = baud.is_some()
                || data_bits.is_some()
                || parity.is_some()
                || stop_bits.is_some()
                || flow_control.is_some();
            if !any {
                // Read-only: pull current device info from `status`.
                let v = call(&mut framed, &mut next_id, "status", json!({})).await?;
                let device = v.get("device").cloned().unwrap_or(json!({}));
                print_device_config(&device, cli.json);
            } else {
                let mut p = serde_json::Map::new();
                if let Some(v) = baud { p.insert("baud".into(), json!(v)); }
                if let Some(v) = data_bits { p.insert("data_bits".into(), json!(v)); }
                if let Some(v) = parity { p.insert("parity".into(), json!(v)); }
                if let Some(v) = stop_bits { p.insert("stop_bits".into(), json!(v)); }
                if let Some(v) = flow_control { p.insert("flow_control".into(), json!(v)); }
                let v = call(&mut framed, &mut next_id, "set_device", Value::Object(p)).await?;
                let device = v.get("device").cloned().unwrap_or(json!({}));
                print_device_config(&device, cli.json);
            }
        }
        Cmd::Reconnect { nowait, timeout_ms } => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("wait".into(), json!(!nowait));
            p.insert("timeout_ms".into(), json!(timeout_ms));
            let v = call(&mut framed, &mut next_id, "reconnect", Value::Object(p)).await?;
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
        Cmd::ListDevices => {
            let v = call(&mut framed, &mut next_id, "list_devices", json!({})).await?;
            print_device_list(&v, cli.json);
        }
        Cmd::Break { duration_ms } => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("duration_ms".into(), json!(duration_ms));
            let v = call(&mut framed, &mut next_id, "send_break", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Dtr { state } => {
            let on = state == "on";
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("on".into(), json!(on));
            let v = call(&mut framed, &mut next_id, "set_dtr", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Rts { state } => {
            let on = state == "on";
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            p.insert("on".into(), json!(on));
            let v = call(&mut framed, &mut next_id, "set_rts", Value::Object(p)).await?;
            print_json_or_pairs(&v, cli.json);
        }
        Cmd::Lines => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "read_modem_status", Value::Object(p)).await?;
            print_modem_status(&v, cli.json);
        }
        Cmd::Disconnect => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "disconnect_device", Value::Object(p)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                eprintln!("tether: device disconnected (auto-reconnect paused)");
            }
        }
        Cmd::Connect => {
            let mut p = serde_json::Map::new();
            if let Some(d) = &cli.device_id { p.insert("device_id".into(), json!(d)); }
            let v = call(&mut framed, &mut next_id, "connect_device", Value::Object(p)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let connected = v.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
                if connected {
                    eprintln!("tether: device reopened");
                } else {
                    eprintln!("tether: connect requested but device is still closed");
                }
            }
        }
        Cmd::Sync { idle_ms, timeout_ms } => {
            let session_id = attach(&mut framed, &mut next_id, "now", cli.device_id.as_deref()).await?;
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
    device_id: Option<&str>,
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
        device_id: device_id.map(|s| s.to_string()),
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

/// Same as `call`, but if the daemon replies `device_disconnected` and
/// `auto_reconnect` is true, transparently kicks `reconnect` and retries
/// the original RPC once. The `reconnect` method itself is excluded so
/// we don't recurse.
async fn call_or_retry<S>(
    framed: &mut Framed<S, NdjsonCodec>,
    next_id: &mut i64,
    method: &str,
    params: Value,
    auto_reconnect: bool,
) -> Result<Value, CliError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let first = call(framed, next_id, method, params.clone()).await;
    if !auto_reconnect || method == "reconnect" {
        return first;
    }
    if !matches!(&first, Err(CliError::DeviceDisconnected)) {
        return first;
    }
    eprintln!("tether: device disconnected — attempting reconnect …");
    let recon = call(
        framed,
        next_id,
        "reconnect",
        json!({"wait": true, "timeout_ms": 10000}),
    )
    .await;
    match recon {
        Ok(v) if v.get("device_connected").and_then(|b| b.as_bool()).unwrap_or(false) => {
            eprintln!("tether: reconnected — retrying {method}");
            call(framed, next_id, method, params).await
        }
        Ok(_) => {
            eprintln!("tether: reconnect did not bring device back; failing");
            Err(CliError::DeviceDisconnected)
        }
        Err(e) => {
            eprintln!("tether: reconnect call failed: {e:?}");
            first
        }
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
            match method.as_str() {
                "data" => {
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
                "device" => {
                    if let Some(p) = params {
                        let kind = p.get("kind").and_then(|s| s.as_str()).unwrap_or("?");
                        let detail = p.get("detail").and_then(|s| s.as_str()).unwrap_or("");
                        let _ = stdout.flush();
                        if detail.is_empty() {
                            eprintln!("\n[device {kind}]");
                        } else {
                            eprintln!("\n[device {kind}: {detail}]");
                        }
                    }
                }
                _ => {}
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
            "\r\n[tether shell — Ctrl-A Q quit, Ctrl-A B/D/R/L (break/dtr/rts/lines), Ctrl-A C/V (config/ports), Ctrl-A ? help]\r\n\r\n"
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

    // Side channel for RPC responses sent in reaction to in-shell escapes
    // (Ctrl-A C, Ctrl-A V). Notifications still go to stdout/stderr; responses
    // are forwarded here so the escape handler can await the result.
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<tether_protocol::Response>();

    // Reader task: stream `data` notifications for our session straight to
    // stdout, and surface `device` (dis|re)connect events to stderr.
    let target = session_id.clone();
    let reader = tokio::spawn(async move {
        while let Some(item) = source.next().await {
            let Ok(msg) = item else { break };
            let msg = match msg {
                Message::Response(r) => {
                    let _ = resp_tx.send(r);
                    continue;
                }
                other => other,
            };
            let Message::Notification(Notification { method, params, .. }) = msg else {
                continue;
            };
            if method == "device" {
                if let Some(p) = params {
                    let kind = p.get("kind").and_then(|s| s.as_str()).unwrap_or("?");
                    let detail = p.get("detail").and_then(|s| s.as_str()).unwrap_or("");
                    if detail.is_empty() {
                        eprint!("\r\n[device {kind}]\r\n");
                    } else {
                        eprint!("\r\n[device {kind}: {detail}]\r\n");
                    }
                }
                continue;
            }
            if method != "data" {
                continue;
            }
            {
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
    // Track DTR / RTS state locally so Ctrl-A D / R can toggle. Both lines
    // are conventionally asserted on a freshly-opened port.
    let mut dtr_on = true;
    let mut rts_on = true;

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
             [Ctrl-A C  : show serial config]\r\n\
             [Ctrl-A V  : list available ports]\r\n\
             [Ctrl-A B  : send BREAK]\r\n\
             [Ctrl-A D  : toggle DTR]\r\n\
             [Ctrl-A R  : toggle RTS]\r\n\
             [Ctrl-A L  : show modem status (CTS/DSR/RI/DCD)]\r\n\
             [Ctrl-A ?  : help]\r\n\
             [Ctrl-A ^A : send literal Ctrl-A]\r\n\r\n"
        );
    };

    // Pretty-print a `set_device`-style result inside the raw-mode shell.
    let print_config_inline = |device: &Value| {
        let baud = device.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
        let data_bits = device.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
        let parity = device.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
        let stop_bits = device.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
        let flow = device.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
        let parity_letter = parity
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('?');
        eprint!(
            "\r\n[config: {baud} {data_bits}{parity_letter}{stop_bits} flow={flow}]\r\n"
        );
    };

    let print_ports_inline = |ports: &[Value]| {
        if ports.is_empty() {
            eprint!("\r\n[no serial ports detected]\r\n");
            return;
        }
        eprint!("\r\n[ports]\r\n");
        for port in ports {
            let path = port.get("path").and_then(|s| s.as_str()).unwrap_or("?");
            let kind = port.get("kind").and_then(|s| s.as_str()).unwrap_or("unknown");
            let desc = port
                .get("product")
                .and_then(|s| s.as_str())
                .or_else(|| port.get("manufacturer").and_then(|s| s.as_str()))
                .unwrap_or("");
            if desc.is_empty() {
                eprint!("  {path}  ({kind})\r\n");
            } else {
                eprint!("  {path}  ({kind})  {desc}\r\n");
            }
        }
        eprint!("\r\n");
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
                    b'c' | b'C' => {
                        // Flush any keystrokes accumulated before the escape so
                        // they don't get mixed into the post-escape output.
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(RpcId::Number(id), "status", json!({}));
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let device = result.get("device").cloned().unwrap_or(json!({}));
                                print_config_inline(&device);
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[config error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[config: timeout]\r\n"),
                        }
                    }
                    b'v' | b'V' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(RpcId::Number(id), "list_ports", json!({}));
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let empty = Vec::<Value>::new();
                                let ports = result
                                    .get("ports")
                                    .and_then(|p| p.as_array())
                                    .unwrap_or(&empty);
                                print_ports_inline(ports);
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[list_ports error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[list_ports: timeout]\r\n"),
                        }
                    }
                    b'b' | b'B' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "send_break",
                            json!({"duration_ms": 250}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!("\r\n[break: 250ms]\r\n");
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[break error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[break: timeout]\r\n"),
                        }
                    }
                    b'd' | b'D' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        dtr_on = !dtr_on;
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "set_dtr",
                            json!({"on": dtr_on}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!(
                                    "\r\n[DTR: {}]\r\n",
                                    if dtr_on { "asserted" } else { "deasserted" }
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                // Roll back local state — apply failed.
                                dtr_on = !dtr_on;
                                eprint!("\r\n[DTR error: {}]\r\n", error.message);
                            }
                            None => {
                                dtr_on = !dtr_on;
                                eprint!("\r\n[DTR: timeout]\r\n");
                            }
                        }
                    }
                    b'r' | b'R' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        rts_on = !rts_on;
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "set_rts",
                            json!({"on": rts_on}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { .. }) => {
                                eprint!(
                                    "\r\n[RTS: {}]\r\n",
                                    if rts_on { "asserted" } else { "deasserted" }
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                rts_on = !rts_on;
                                eprint!("\r\n[RTS error: {}]\r\n", error.message);
                            }
                            None => {
                                rts_on = !rts_on;
                                eprint!("\r\n[RTS: timeout]\r\n");
                            }
                        }
                    }
                    b'l' | b'L' => {
                        if !buffer.is_empty() {
                            let _ = send_bytes(&buffer, &mut next_id);
                            buffer.clear();
                        }
                        let id = next_id; next_id += 1;
                        let req = Request::new(
                            RpcId::Number(id),
                            "read_modem_status",
                            json!({}),
                        );
                        let _ = msg_tx.send(Message::Request(req));
                        match recv_response_for(id, &mut resp_rx, std::time::Duration::from_millis(2000)).await {
                            Some(tether_protocol::message::ResponsePayload::Ok { result }) => {
                                let bit = |k: &str| result.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
                                let m = |b| if b { "1" } else { "0" };
                                eprint!(
                                    "\r\n[lines: CTS={} DSR={} RI={} DCD={}]\r\n",
                                    m(bit("cts")), m(bit("dsr")), m(bit("ri")), m(bit("dcd"))
                                );
                            }
                            Some(tether_protocol::message::ResponsePayload::Err { error }) => {
                                eprint!("\r\n[lines error: {}]\r\n", error.message);
                            }
                            None => eprint!("\r\n[lines: timeout]\r\n"),
                        }
                    }
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

/// Pull responses off `resp_rx` until we see one whose id matches the request
/// we just sent, or `timeout` elapses. Foreign responses (e.g. an in-flight
/// `send` ack from the shell loop) are discarded. Returns `None` on timeout
/// or channel close.
async fn recv_response_for(
    want_id: i64,
    resp_rx: &mut tokio::sync::mpsc::UnboundedReceiver<tether_protocol::Response>,
    timeout: std::time::Duration,
) -> Option<tether_protocol::message::ResponsePayload> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            return None;
        }
        match tokio::time::timeout(remain, resp_rx.recv()).await {
            Ok(Some(resp)) => match &resp.id {
                RpcId::Number(n) if *n == want_id => return Some(resp.payload),
                _ => continue, // foreign response — keep waiting
            },
            _ => return None,
        }
    }
}

fn print_ports(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let empty = Vec::<Value>::new();
    let ports = v.get("ports").and_then(|p| p.as_array()).unwrap_or(&empty);
    if ports.is_empty() {
        eprintln!("(no ports detected)");
        return;
    }
    for port in ports {
        let path = port.get("path").and_then(|s| s.as_str()).unwrap_or("?");
        let kind = port.get("kind").and_then(|s| s.as_str()).unwrap_or("unknown");
        let mut details: Vec<String> = Vec::new();
        if let Some(s) = port.get("manufacturer").and_then(|s| s.as_str()) {
            details.push(format!("manufacturer={s}"));
        }
        if let Some(s) = port.get("product").and_then(|s| s.as_str()) {
            details.push(format!("product={s}"));
        }
        if let Some(s) = port.get("serial_number").and_then(|s| s.as_str()) {
            details.push(format!("serial={s}"));
        }
        if let (Some(vid), Some(pid)) = (
            port.get("vid").and_then(|s| s.as_str()),
            port.get("pid").and_then(|s| s.as_str()),
        ) {
            details.push(format!("usb={vid}:{pid}"));
        }
        if details.is_empty() {
            println!("{path}  ({kind})");
        } else {
            println!("{path}  ({kind})  {}", details.join(", "));
        }
    }
}

fn print_device_list(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let empty = Vec::<Value>::new();
    let devices = v.get("devices").and_then(|x| x.as_array()).unwrap_or(&empty);
    let default_id = v.get("default_device").and_then(|s| s.as_str()).unwrap_or("");
    if devices.is_empty() {
        eprintln!("(no devices)");
        return;
    }
    for d in devices {
        let id = d.get("id").and_then(|s| s.as_str()).unwrap_or("?");
        let path = d.get("path").and_then(|s| s.as_str()).unwrap_or("?");
        let baud = d.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
        let connected = d.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
        let exp_disc = d
            .get("explicitly_disconnected")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let parity = d.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
        let data_bits = d.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
        let stop_bits = d.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
        let flow = d.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
        let parity_letter = parity.chars().next().map(|c| c.to_ascii_uppercase()).unwrap_or('?');
        let marker = if id == default_id { " (default)" } else { "" };
        let status = if exp_disc {
            "disconnected (explicit)"
        } else if connected {
            "connected"
        } else {
            "disconnected"
        };
        println!(
            "{:10}  {} @ {} {}{}{} flow={}  [{}]{}",
            id, path, baud, data_bits, parity_letter, stop_bits, flow, status, marker
        );
    }
}

fn print_modem_status(v: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
        return;
    }
    let cts = v.get("cts").and_then(|b| b.as_bool()).unwrap_or(false);
    let dsr = v.get("dsr").and_then(|b| b.as_bool()).unwrap_or(false);
    let ri = v.get("ri").and_then(|b| b.as_bool()).unwrap_or(false);
    let dcd = v.get("dcd").and_then(|b| b.as_bool()).unwrap_or(false);
    let mark = |b| if b { "1" } else { "0" };
    println!("CTS={}  DSR={}  RI={}  DCD={}", mark(cts), mark(dsr), mark(ri), mark(dcd));
}

fn print_device_config(device: &Value, force_json: bool) {
    if force_json {
        println!("{}", serde_json::to_string_pretty(device).unwrap_or_default());
        return;
    }
    let path = device.get("path").and_then(|s| s.as_str()).unwrap_or("?");
    let baud = device.get("baud").and_then(|n| n.as_u64()).unwrap_or(0);
    let data_bits = device.get("data_bits").and_then(|n| n.as_u64()).unwrap_or(8);
    let parity = device.get("parity").and_then(|s| s.as_str()).unwrap_or("none");
    let stop_bits = device.get("stop_bits").and_then(|n| n.as_u64()).unwrap_or(1);
    let flow = device.get("flow_control").and_then(|s| s.as_str()).unwrap_or("none");
    let connected = device.get("connected").and_then(|b| b.as_bool()).unwrap_or(false);
    let parity_letter = parity
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('?');
    println!("path:         {path}");
    println!("baud:         {baud}");
    println!("framing:      {data_bits}{parity_letter}{stop_bits}");
    println!("flow_control: {flow}");
    println!("connected:    {connected}");
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
