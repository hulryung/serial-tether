mod buffer;
mod conn;
mod handlers;
mod serial;
mod session;
mod state;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
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
    /// Serial device path (e.g. /dev/ttyUSB0, or a PTY)
    #[arg(short = 'D', long)]
    device: String,

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

    /// Unix socket path. Use --no-uds to disable.
    #[arg(short = 's', long, default_value = "/tmp/tetherd.sock")]
    socket: PathBuf,

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

/// Print a human-friendly startup banner to stderr summarising what the
/// daemon is listening on and how to reach it.
fn print_banner(args: &Args, tcp_addr: Option<SocketAddr>, auth_token: Option<&str>) {
    eprintln!();
    eprintln!("Serial Tether {}", env!("CARGO_PKG_VERSION"));
    eprintln!("  device     {} @ {} baud", args.device, args.baud);
    eprintln!(
        "  framing    {}{}{} flow={}",
        args.data_bits,
        match args.parity.to_ascii_lowercase().chars().next() {
            Some('n') => 'N',
            Some('o') => 'O',
            Some('e') => 'E',
            _ => '?',
        },
        args.stop_bits,
        args.flow_control,
    );
    eprintln!("  buffer     {} KiB", args.buffer_capacity / 1024);
    if let Some(tok) = auth_token {
        eprintln!("  auth       {tok}");
        eprintln!("             (clients: --auth-token {tok}  or  TETHER_AUTH_TOKEN={tok})");
    }
    eprintln!();
    eprintln!("Listening:");
    if !args.no_uds {
        eprintln!("  unix       {}", args.socket.display());
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
        None => "tether".to_string(),
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

    let data_bits = SerialDataBits::from_u8(args.data_bits)
        .ok_or_else(|| anyhow::anyhow!("invalid --data-bits {}", args.data_bits))?;
    let parity = SerialParity::parse(&args.parity)
        .ok_or_else(|| anyhow::anyhow!("invalid --parity {:?}", args.parity))?;
    let stop_bits = SerialStopBits::from_u8(args.stop_bits)
        .ok_or_else(|| anyhow::anyhow!("invalid --stop-bits {}", args.stop_bits))?;
    let flow_control = SerialFlow::parse(&args.flow_control)
        .ok_or_else(|| anyhow::anyhow!("invalid --flow-control {:?}", args.flow_control))?;
    let cfg = SerialConfig {
        path: args.device.clone(),
        baud: args.baud,
        data_bits,
        parity,
        stop_bits,
        flow_control,
    };
    let cfg_shared = Arc::new(parking_lot::Mutex::new(cfg.clone()));

    let buffer = RingBuffer::new(args.buffer_capacity);

    // Verify the device opens at startup so we fail fast on bad config / wrong
    // path / permissions. The serial task will still own a fresh open after
    // this, plus reconnect forever if the device disappears.
    {
        let port = SerialPort::open(&cfg)
            .await
            .with_context(|| format!("opening serial {}", args.device))?;
        drop(port);
    }

    let device_state = Arc::new(parking_lot::Mutex::new(crate::state::DeviceState::new_disconnected()));
    let force_reconnect = Arc::new(tokio::sync::Notify::new());
    let reconnected = Arc::new(tokio::sync::Notify::new());
    // Capacity 32 is plenty for device transitions; subscribers that fall
    // behind get RecvError::Lagged which conn.rs treats as drop-and-resync.
    let (device_events_tx, _) = tokio::sync::broadcast::channel::<crate::state::DeviceEvent>(32);
    let (writer, serial_control, _serial_task) = serial::spawn(
        cfg_shared.clone(),
        buffer.clone(),
        device_state.clone(),
        force_reconnect.clone(),
        reconnected.clone(),
        device_events_tx.clone(),
    );

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
        buffer,
        writer,
        serial_control,
        sessions: Arc::new(SessionManager::new()),
        config: cfg_shared,
        lock: Arc::new(crate::state::WriterLock::default()),
        auth_token: auth_token.clone(),
        device_state,
        force_reconnect,
        reconnected,
        device_events: device_events_tx,
    };

    let mut listener_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut tcp_bound: Option<SocketAddr> = None;

    // UDS listener.
    if !args.no_uds {
        if args.socket.exists() {
            std::fs::remove_file(&args.socket)
                .with_context(|| format!("remove existing socket {:?}", args.socket))?;
        }
        let listener = UnixListener::bind(&args.socket)
            .with_context(|| format!("bind {:?}", args.socket))?;
        tracing::debug!(socket=?args.socket, "UDS listener bound");
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
    print_banner(&args, tcp_bound, auth_token.as_deref().map(String::as_str));

    // Wait for any listener task to exit (which means accept failed).
    for h in listener_tasks {
        let _ = h.await;
    }
    Ok(())
}
