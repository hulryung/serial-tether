mod buffer;
mod conn;
mod handlers;
mod serial;
mod session;
mod state;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::{TcpListener, UnixListener};

use crate::buffer::RingBuffer;
use crate::serial::{
    DataBits as SerialDataBits, FlowControl as SerialFlow, Parity as SerialParity, SerialConfig,
    SerialPort, StopBits as SerialStopBits,
};
use crate::session::SessionManager;
use crate::state::DaemonState;

#[derive(Debug, Parser)]
#[command(name = "tetherd", version, about = "tetherd — serial port daemon")]
struct Args {
    /// Serial device(s) to manage. Repeatable for multi-device daemons.
    ///
    /// Accepted forms (each `-D` flag):
    ///
    ///   `-D /dev/ttyUSB0`
    ///       Bare path. The device id is derived from the basename
    ///       (stripping `tty.` / `cu.` prefixes — e.g. `ttyUSB0`).
    ///
    ///   `-D board0=/dev/ttyUSB0`
    ///       Explicit id (`board0`). Recommended when running >1 board so
    ///       clients address them by friendly name.
    ///
    ///   `-D board0=/dev/ttyUSB0,baud=921600,parity=odd,data-bits=8,stop-bits=1,flow=none`
    ///       Per-device serial settings. Any setting omitted falls back
    ///       to the global `--baud` / `--parity` / etc. flag.
    #[arg(short = 'D', long = "device", value_name = "[ID=]PATH[,KEY=VAL...]", required = true)]
    devices: Vec<String>,

    /// Baud rate
    #[arg(short = 'b', long, default_value_t = 115200)]
    baud: u32,

    /// Data bits: 5, 6, 7, or 8.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(5..=8))]
    data_bits: u8,

    /// Parity: none | odd | even.
    #[arg(long, default_value = "none")]
    parity: String,

    /// Stop bits: 1 or 2.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    stop_bits: u8,

    /// Flow control: none | software | hardware.
    #[arg(long, default_value = "none")]
    flow_control: String,

    /// Unix socket path.
    ///
    /// Default: `/tmp/tetherd.sock`, or `/tmp/tetherd-<NAME>.sock` if `--name`
    /// is set. Use `--no-uds` to disable the UDS listener entirely (TCP only).
    #[arg(short = 's', long)]
    socket: Option<PathBuf>,

    /// Friendly name for this daemon instance.
    ///
    /// Defaults the UDS socket to `/tmp/tetherd-<NAME>.sock`, so multiple
    /// daemons (one per board) can run side-by-side without colliding.
    /// Clients reach this daemon with `tether --name <NAME> ...`. Ignored
    /// when `--socket` is given explicitly.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Disable the Unix socket listener (TCP only).
    #[arg(long)]
    no_uds: bool,

    /// Also listen on TCP. Use bare `--tcp` for the default `0.0.0.0:5557`
    /// (network-reachable) or `--tcp HOST:PORT` for an explicit endpoint
    /// (e.g. `--tcp 127.0.0.1:5557` for localhost only). Requires auth.
    #[arg(
        long,
        value_name = "HOST:PORT",
        num_args = 0..=1,
        default_missing_value = "0.0.0.0:5557",
    )]
    tcp: Option<String>,

    /// Auth token for TCP clients. Random token is generated if --tcp is set
    /// without this flag (and printed to stderr at startup).
    #[arg(long, value_name = "TOKEN")]
    auth_token: Option<String>,

    /// Ring buffer capacity in bytes
    #[arg(long, default_value_t = 65536)]
    buffer_capacity: usize,
}

fn random_token() -> String {
    // 128 bits, hex — uuid v4 is fine; we already depend on uuid.
    use uuid::Uuid;
    let u = Uuid::new_v4();
    u.simple().to_string()
}

/// Derive a friendly id for a serial device from its OS path. The path
/// basename strips any leading `/dev/` and dotted prefixes (`tty.`, `cu.`)
/// to give a short, stable name suitable as a `device_id`. Used when the
/// operator passes a bare path to `-D` instead of `name=path`.
fn derive_device_id(path: &str) -> String {
    let basename = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    for prefix in ["tty.", "cu."] {
        if let Some(rest) = basename.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    basename
}

/// Parsed `-D` spec — id, OS path, and any per-device overrides for the
/// global `--baud` / `--parity` / etc. flags.
#[derive(Debug)]
struct DeviceSpec {
    id: String,
    path: String,
    baud: Option<u32>,
    data_bits: Option<u8>,
    parity: Option<String>,
    stop_bits: Option<u8>,
    flow_control: Option<String>,
}

/// Parse one `-D` argument. Forms:
///
///   `path`
///   `id=path`
///   `id=path,key=value,key=value,...`
///   `path,key=value,key=value,...`        (id derived from path)
///
/// Recognised keys: `baud`, `data-bits` (or `data_bits`), `parity`,
/// `stop-bits` (or `stop_bits`), `flow` (or `flow-control` / `flow_control`).
fn parse_device_spec(s: &str) -> anyhow::Result<DeviceSpec> {
    let mut parts = s.split(',');
    let head = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty -D spec"))?;

    let (id, path) = if let Some((id, path)) = head.split_once('=') {
        if id.is_empty() {
            anyhow::bail!("empty id in -D spec {s:?}");
        }
        if path.is_empty() {
            anyhow::bail!("empty path in -D spec {s:?}");
        }
        (id.to_string(), path.to_string())
    } else {
        (derive_device_id(head), head.to_string())
    };

    let mut spec = DeviceSpec {
        id,
        path,
        baud: None,
        data_bits: None,
        parity: None,
        stop_bits: None,
        flow_control: None,
    };

    for kv in parts {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected key=value, got {kv:?} in -D spec"))?;
        match k.trim() {
            "baud" => {
                spec.baud = Some(
                    v.parse()
                        .map_err(|_| anyhow::anyhow!("invalid baud {v:?} in -D spec"))?,
                );
            }
            "data-bits" | "data_bits" => {
                spec.data_bits = Some(
                    v.parse()
                        .map_err(|_| anyhow::anyhow!("invalid data-bits {v:?}"))?,
                );
            }
            "parity" => spec.parity = Some(v.to_string()),
            "stop-bits" | "stop_bits" => {
                spec.stop_bits = Some(
                    v.parse()
                        .map_err(|_| anyhow::anyhow!("invalid stop-bits {v:?}"))?,
                );
            }
            "flow" | "flow-control" | "flow_control" => {
                spec.flow_control = Some(v.to_string());
            }
            other => anyhow::bail!("unknown key {other:?} in -D spec {s:?}"),
        }
    }
    Ok(spec)
}

/// Resolve the effective UDS socket path, given the (optional) explicit
/// `--socket` and (optional) `--name`. The two are layered:
///   1. `--socket PATH` always wins (most specific).
///   2. `--name NAME` → `/tmp/tetherd-<NAME>.sock` (multi-daemon convention).
///   3. Neither → `/tmp/tetherd.sock` (the historical default).
fn resolve_socket(socket: Option<&PathBuf>, name: Option<&str>) -> PathBuf {
    if let Some(p) = socket {
        return p.clone();
    }
    if let Some(n) = name {
        return PathBuf::from(format!("/tmp/tetherd-{n}.sock"));
    }
    PathBuf::from("/tmp/tetherd.sock")
}

/// Print a human-friendly startup banner to stderr summarising what the
/// daemon is listening on and how to reach it.
fn print_banner(
    args: &Args,
    devices: &std::collections::HashMap<String, Arc<crate::state::Device>>,
    default_device: &str,
    socket_path: &Path,
    tcp_addr: Option<SocketAddr>,
    auth_token: Option<&str>,
) {
    eprintln!();
    eprintln!("Serial Tether {}", env!("CARGO_PKG_VERSION"));
    if devices.len() == 1 {
        // Single-device daemon: keep the v0.7 one-line banner shape.
        let dev = devices.values().next().expect("present");
        let cfg = dev.config.lock().clone();
        eprintln!("  device     {} @ {} baud", cfg.path, cfg.baud);
        eprintln!(
            "  framing    {}{}{} flow={}",
            cfg.data_bits.as_u8(),
            match cfg.parity.as_str().chars().next() {
                Some('n') => 'N',
                Some('o') => 'O',
                Some('e') => 'E',
                _ => '?',
            },
            cfg.stop_bits.as_u8(),
            cfg.flow_control.as_str(),
        );
    } else {
        eprintln!("  devices");
        // Stable order: default first, then alphabetical.
        let mut ids: Vec<&String> = devices.keys().collect();
        ids.sort();
        ids.sort_by_key(|id| (id.as_str() != default_device, id.as_str().to_string()));
        for id in ids {
            let dev = &devices[id];
            let cfg = dev.config.lock().clone();
            let parity_letter = match cfg.parity.as_str().chars().next() {
                Some('n') => 'N',
                Some('o') => 'O',
                Some('e') => 'E',
                _ => '?',
            };
            let marker = if id == default_device { " (default)" } else { "" };
            eprintln!(
                "    {:10} {} @ {} baud  {}{}{} flow={}{}",
                id,
                cfg.path,
                cfg.baud,
                cfg.data_bits.as_u8(),
                parity_letter,
                cfg.stop_bits.as_u8(),
                cfg.flow_control.as_str(),
                marker,
            );
        }
    }
    eprintln!("  buffer     {} KiB", args.buffer_capacity / 1024);
    if let Some(tok) = auth_token {
        eprintln!("  auth       {tok}");
        eprintln!("             (clients: --auth-token {tok}  or  TETHER_AUTH_TOKEN={tok})");
    }
    eprintln!();
    eprintln!("Listening:");
    if !args.no_uds {
        if let Some(n) = &args.name {
            eprintln!("  unix       {}    (name: {n})", socket_path.display());
        } else {
            eprintln!("  unix       {}", socket_path.display());
        }
    }
    if let Some(bind) = tcp_addr {
        eprintln!("  tcp        {bind}    (auth required)");
        let reachable = enumerate_reachable_ips(bind);
        if !reachable.is_empty() {
            eprintln!("               reachable as:");
            for (ip, label) in reachable {
                let port = bind.port();
                if let Some(name) = label {
                    eprintln!("                 tcp://{ip}:{port:<5}  ({name})");
                } else {
                    eprintln!("                 tcp://{ip}:{port}");
                }
            }
        }
    }
    eprintln!();
    let example_target = match tcp_addr {
        Some(_) => format!(
            "TETHER_AUTH_TOKEN={} tether -s tcp://<host>:{}",
            auth_token.unwrap_or("..."),
            tcp_addr.unwrap().port()
        ),
        None => match &args.name {
            Some(n) => format!("tether --name {n}"),
            None => "tether".to_string(),
        },
    };
    eprintln!("  try:       {example_target} status");
    eprintln!();
}

/// For a given listening socket, return the set of IPs a client could
/// actually reach the daemon on. Wildcard binds (0.0.0.0 / ::) are expanded
/// into every up, non-link-local interface address; specific binds return
/// just that address.
fn enumerate_reachable_ips(bind: SocketAddr) -> Vec<(IpAddr, Option<String>)> {
    let want_v4 = bind.is_ipv4();
    let is_wildcard = bind.ip().is_unspecified();

    if !is_wildcard {
        return vec![(bind.ip(), None)];
    }

    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return vec![(bind.ip(), None)];
    };

    let mut out: Vec<(IpAddr, Option<String>)> = Vec::new();
    let mut loopback: Vec<(IpAddr, Option<String>)> = Vec::new();
    for iface in ifaces {
        let ip = iface.ip();
        if want_v4 && !ip.is_ipv4() {
            continue;
        }
        if !want_v4 && !ip.is_ipv6() {
            continue;
        }
        // Skip IPv6 link-local — they need a zone id and aren't useful here.
        if let IpAddr::V6(v6) = ip {
            let seg = v6.segments()[0];
            if seg & 0xffc0 == 0xfe80 {
                continue;
            }
        }
        let entry = (ip, Some(iface.name.clone()));
        if ip.is_loopback() {
            loopback.push(entry);
        } else {
            out.push(entry);
        }
    }
    // Loopback first, then routable addresses.
    loopback.extend(out);
    loopback
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    if args.no_uds && args.tcp.is_none() {
        anyhow::bail!("--no-uds requires --tcp; otherwise the daemon has no listener");
    }

    let socket_path = resolve_socket(args.socket.as_ref(), args.name.as_deref());

    // Global defaults — applied to any device whose -D spec didn't override.
    let g_data_bits = SerialDataBits::from_u8(args.data_bits)
        .ok_or_else(|| anyhow::anyhow!("invalid --data-bits {}", args.data_bits))?;
    let g_parity = SerialParity::parse(&args.parity)
        .ok_or_else(|| anyhow::anyhow!("invalid --parity {:?}", args.parity))?;
    let g_stop_bits = SerialStopBits::from_u8(args.stop_bits)
        .ok_or_else(|| anyhow::anyhow!("invalid --stop-bits {}", args.stop_bits))?;
    let g_flow = SerialFlow::parse(&args.flow_control)
        .ok_or_else(|| anyhow::anyhow!("invalid --flow-control {:?}", args.flow_control))?;

    // Parse every -D spec, then build one Device per spec. The first -D
    // becomes the default device.
    let specs: Vec<DeviceSpec> = args
        .devices
        .iter()
        .map(|s| parse_device_spec(s))
        .collect::<Result<_, _>>()?;
    if specs.is_empty() {
        anyhow::bail!("at least one -D / --device must be given");
    }
    {
        // Reject duplicate ids early — easy mistake when mixing implicit and
        // explicit forms.
        let mut seen = std::collections::HashSet::new();
        for s in &specs {
            if !seen.insert(&s.id) {
                anyhow::bail!(
                    "duplicate device id {:?} — use `id=path` form to disambiguate",
                    s.id
                );
            }
        }
    }
    let default_device_id = specs[0].id.clone();

    let mut devices_map: std::collections::HashMap<String, Arc<crate::state::Device>> =
        std::collections::HashMap::new();
    let mut serial_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    for spec in specs {
        let data_bits = match spec.data_bits {
            Some(n) => SerialDataBits::from_u8(n).ok_or_else(|| {
                anyhow::anyhow!("invalid data-bits {n} in -D {:?}", spec.id)
            })?,
            None => g_data_bits,
        };
        let parity = match &spec.parity {
            Some(s) => SerialParity::parse(s)
                .ok_or_else(|| anyhow::anyhow!("invalid parity {s:?} in -D {:?}", spec.id))?,
            None => g_parity,
        };
        let stop_bits = match spec.stop_bits {
            Some(n) => SerialStopBits::from_u8(n).ok_or_else(|| {
                anyhow::anyhow!("invalid stop-bits {n} in -D {:?}", spec.id)
            })?,
            None => g_stop_bits,
        };
        let flow_control = match &spec.flow_control {
            Some(s) => SerialFlow::parse(s)
                .ok_or_else(|| anyhow::anyhow!("invalid flow {s:?} in -D {:?}", spec.id))?,
            None => g_flow,
        };
        let cfg = SerialConfig {
            path: spec.path.clone(),
            baud: spec.baud.unwrap_or(args.baud),
            data_bits,
            parity,
            stop_bits,
            flow_control,
        };
        let cfg_shared = Arc::new(parking_lot::Mutex::new(cfg.clone()));

        // Fail fast on this device's open before starting the others.
        {
            let port = SerialPort::open(&cfg)
                .await
                .with_context(|| format!("opening serial {} for device {:?}", spec.path, spec.id))?;
            drop(port);
        }

        let buffer = RingBuffer::new(args.buffer_capacity);
        let device_state = Arc::new(parking_lot::Mutex::new(
            crate::state::DeviceState::new_disconnected(),
        ));
        let force_reconnect = Arc::new(tokio::sync::Notify::new());
        let reconnected = Arc::new(tokio::sync::Notify::new());
        let (events_tx, _) =
            tokio::sync::broadcast::channel::<crate::state::DeviceEvent>(32);
        let (writer, serial_control, task) = serial::spawn(
            cfg_shared.clone(),
            buffer.clone(),
            device_state.clone(),
            force_reconnect.clone(),
            reconnected.clone(),
            events_tx.clone(),
        );
        serial_tasks.push(task);

        let device = Arc::new(crate::state::Device {
            id: spec.id.clone(),
            buffer,
            writer,
            serial_control,
            config: cfg_shared,
            state: device_state,
            force_reconnect,
            reconnected,
            events: events_tx,
            lock: Arc::new(crate::state::WriterLock::default()),
        });
        devices_map.insert(spec.id.clone(), device);
    }
    let _serial_tasks = serial_tasks;

    // Resolve the auth token. Generate one if --tcp is enabled without --auth-token.
    let auth_token: Option<Arc<String>> = if args.tcp.is_some() {
        let token = args
            .auth_token
            .clone()
            .unwrap_or_else(random_token);
        Some(Arc::new(token))
    } else if args.auth_token.is_some() {
        // No --tcp but token given — accept it (allows pre-generation), but warn.
        tracing::warn!("--auth-token has no effect without --tcp");
        None
    } else {
        None
    };

    let state = DaemonState {
        devices: Arc::new(devices_map),
        default_device: default_device_id,
        sessions: Arc::new(SessionManager::new()),
        auth_token: auth_token.clone(),
    };

    let mut listener_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut tcp_bound: Option<SocketAddr> = None;

    // UDS listener.
    if !args.no_uds {
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("remove existing socket {:?}", socket_path))?;
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {:?}", socket_path))?;
        tracing::debug!(socket=?socket_path, "UDS listener bound");
        let state_uds = state.clone();
        listener_tasks.push(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let state = state_uds.clone();
                        tokio::spawn(async move {
                            if let Err(e) = conn::handle(stream, state, false).await {
                                tracing::warn!(error=%e, "UDS connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error=%e, "UDS accept error");
                        break;
                    }
                }
            }
        }));
    }

    // TCP listener.
    if let Some(addr) = args.tcp.clone() {
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind tcp:{addr}"))?;
        let local = listener.local_addr().ok();
        tcp_bound = local;
        tracing::debug!(addr=%addr, "TCP listener bound");
        let state_tcp = state.clone();
        listener_tasks.push(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        // disable nagling for low-latency RPC
                        let _ = stream.set_nodelay(true);
                        let state = state_tcp.clone();
                        tokio::spawn(async move {
                            if let Err(e) = conn::handle(stream, state, true).await {
                                tracing::warn!(error=%e, peer=%peer, "TCP connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!(error=%e, "TCP accept error");
                        break;
                    }
                }
            }
        }));
    }

    if listener_tasks.is_empty() {
        anyhow::bail!("no listener configured");
    }

    // Human-friendly summary to stderr.
    print_banner(
        &args,
        &state.devices,
        &state.default_device,
        &socket_path,
        tcp_bound,
        auth_token.as_deref().map(String::as_str),
    );

    // Wait for any listener task to exit (which means accept failed).
    for h in listener_tasks {
        let _ = h.await;
    }
    Ok(())
}
