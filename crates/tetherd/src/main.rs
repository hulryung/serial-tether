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
use tokio::net::UnixListener;

use crate::buffer::RingBuffer;
use crate::serial::{SerialConfig, SerialPort};
use crate::session::SessionManager;
use crate::state::DaemonState;

#[derive(Debug, Parser)]
#[command(version, about = "tetherd — serial port daemon")]
struct Args {
    /// Serial device path (e.g. /dev/ttyUSB0, or a PTY)
    #[arg(short = 'D', long)]
    device: String,

    /// Baud rate
    #[arg(short = 'b', long, default_value_t = 115200)]
    baud: u32,

    /// Unix socket path
    #[arg(short = 's', long, default_value = "/tmp/tetherd.sock")]
    socket: PathBuf,

    /// Ring buffer capacity in bytes
    #[arg(long, default_value_t = 65536)]
    buffer_capacity: usize,
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
    let cfg = SerialConfig {
        path: args.device.clone(),
        baud: args.baud,
    };

    let buffer = RingBuffer::new(args.buffer_capacity);
    let port = SerialPort::open(&cfg)
        .await
        .with_context(|| format!("opening serial {}", args.device))?;
    let (writer, _serial_task) = serial::spawn(port, buffer.clone());

    let state = DaemonState {
        buffer,
        writer,
        sessions: Arc::new(SessionManager::new()),
        config: cfg,
        lock: Arc::new(crate::state::WriterLock::default()),
    };

    // Clean up an existing socket file from a previous run.
    if args.socket.exists() {
        std::fs::remove_file(&args.socket)
            .with_context(|| format!("remove existing socket {:?}", args.socket))?;
    }
    let listener = UnixListener::bind(&args.socket)
        .with_context(|| format!("bind {:?}", args.socket))?;
    tracing::info!(socket=?args.socket, device=%args.device, baud=args.baud, "tetherd listening");

    loop {
        let (stream, _peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = conn::handle(stream, state).await {
                tracing::warn!(error=%e, "connection ended");
            }
        });
    }
}
