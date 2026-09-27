use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use nmidid::mounts::{MidirMounter, MountRegistry};
use nmidid::ports::MidirPortProvider;
use nmidid::pump::RtpConnector;
use nmidid::{hotplug, server};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "nmidid")]
#[command(about = "MIDI data-plane daemon — serves the capmesh-ctl control socket")]
struct Args {
    /// Path of the Unix control socket to bind.
    #[arg(short, long, default_value = "/run/nmidid.sock")]
    socket: String,

    /// How often to poll for local MIDI port changes (hot-plug), in seconds.
    #[arg(long, default_value = "5")]
    monitor_interval: u64,

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let level = match args.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    let subscriber = FmtSubscriber::builder().with_max_level(level).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting nmidid, control socket at {}", args.socket);

    let ports = Arc::new(MidirPortProvider);
    let mounts = Arc::new(MountRegistry::new(
        Arc::new(MidirMounter),
        Arc::new(RtpConnector),
        ports.clone(),
    ));

    // Hot-plug notifications (§5, `hotplug-events`): watch local ports and emit
    // port-added/port-removed on the daemon's notification bus.
    let port_rx =
        nmidi_core::midi::start_port_monitor(Duration::from_secs(args.monitor_interval)).await;
    hotplug::spawn(port_rx, mounts.notifier());

    server::run(&args.socket, ports, mounts).await
}
