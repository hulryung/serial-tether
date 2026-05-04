mod buffer;
mod conn;
mod handlers;
mod serial;
mod session;
mod state;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::{TcpListener, UnixListener};

use crate::buffer::RingBuffer;
use crate::serial::{SerialConfig, SerialPort};
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

    /// Unix socket path. Use --no-uds to disable.
    #[arg(short = 's', long, default_value = "/tmp/tetherd.sock")]
    socket: PathBuf,

    /// Disable the Unix socket listener (TCP only).
    #[arg(long)]
    no_uds: bool,

    /// Also listen on TCP at HOST:PORT (e.g. 0.0.0.0:5557). Requires auth.
    #[arg(long, value_name = "HOST:PORT")]
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

    let cfg = SerialConfig {
        path: args.device.clone(),
        baud: args.baud,
    };

    let buffer = RingBuffer::new(args.buffer_capacity);
    let port = SerialPort::open(&cfg)
        .await
        .with_context(|| format!("opening serial {}", args.device))?;
    let (writer, _serial_task) = serial::spawn(port, buffer.clone());

    // Resolve the auth token. Generate one if --tcp is enabled without --auth-token.
    let auth_token = if args.tcp.is_some() {
        let token = args.auth_token.unwrap_or_else(|| {
            let t = random_token();
            eprintln!("tetherd: generated auth token (use --auth-token TOKEN to set explicitly):");
            eprintln!("tetherd:   {t}");
            t
        });
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
        sessions: Arc::new(SessionManager::new()),
        config: cfg,
        lock: Arc::new(crate::state::WriterLock::default()),
        auth_token,
    };

    let mut listener_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // UDS listener.
    if !args.no_uds {
        if args.socket.exists() {
            std::fs::remove_file(&args.socket)
                .with_context(|| format!("remove existing socket {:?}", args.socket))?;
        }
        let listener = UnixListener::bind(&args.socket)
            .with_context(|| format!("bind {:?}", args.socket))?;
        tracing::info!(socket=?args.socket, device=%args.device, baud=args.baud, "tetherd listening (UDS)");
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
        tracing::info!(addr=%addr, device=%args.device, baud=args.baud, "tetherd listening (TCP, auth required)");
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

    // Wait for any listener task to exit (which means accept failed).
    for h in listener_tasks {
        let _ = h.await;
    }
    Ok(())
}
