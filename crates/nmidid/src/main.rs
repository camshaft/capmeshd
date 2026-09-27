use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use nmidid::ports::MidirPortProvider;
use nmidid::server;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "nmidid")]
#[command(about = "MIDI data-plane daemon — serves the capmesh-ctl control socket")]
struct Args {
    /// Path of the Unix control socket to bind.
    #[arg(short, long, default_value = "/run/nmidid.sock")]
    socket: String,

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
    server::run(&args.socket, ports).await
}
