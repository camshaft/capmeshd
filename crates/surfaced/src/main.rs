//! `surfaced` — serve durable browser surfaces over HTTP/SSE (DESIGN §10.1).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use surfaced::http::router_with_base;
use surfaced::inbox::SurfaceStore;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "surfaced")]
#[command(about = "Browser surface data-plane daemon — durable, scriptable display sinks")]
struct Args {
    /// Address the HTTP/SSE server binds (attachment pages + push path).
    #[arg(short = 'a', long, default_value = "127.0.0.1:8787")]
    http_addr: SocketAddr,

    /// Directory for durable per-surface inbox logs. Omit for in-memory only
    /// (surfaces do not survive a restart).
    #[arg(short = 'd', long)]
    state_dir: Option<String>,

    /// Mount the server under a URL prefix (e.g. `/surfaced`) for reverse-proxy
    /// deployment behind nginx. Empty (default) serves at the root. Proxy
    /// WITHOUT stripping the prefix: `location /surfaced/ { proxy_pass
    /// http://127.0.0.1:8787; }`.
    #[arg(short = 'b', long, env = "SURFACED_BASE_PATH", default_value = "")]
    base_path: String,

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

    let store = match &args.state_dir {
        Some(dir) => {
            info!("surfaced: durable state dir {dir}");
            Arc::new(SurfaceStore::with_state_dir(dir)?)
        }
        None => {
            info!("surfaced: in-memory only (no --state-dir; surfaces are not durable)");
            Arc::new(SurfaceStore::in_memory())
        }
    };

    let app = router_with_base(store, &args.base_path);
    let listener = tokio::net::TcpListener::bind(args.http_addr)
        .await
        .with_context(|| format!("binding {}", args.http_addr))?;
    if args.base_path.trim().trim_matches('/').is_empty() {
        info!("surfaced HTTP/SSE serving on http://{}", args.http_addr);
    } else {
        info!(
            "surfaced HTTP/SSE serving on http://{} under base path '{}'",
            args.http_addr, args.base_path
        );
    }
    axum::serve(listener, app).await.context("serving HTTP")?;
    Ok(())
}
