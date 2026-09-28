//! `surfaced` — serve durable browser surfaces over HTTP/SSE (DESIGN §10.1).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use surfaced::ctl;
use surfaced::http::router_with_base;
use surfaced::inbox::SurfaceStore;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "surfaced")]
#[command(version)]
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

    /// Path of the Unix control socket capmeshd drives (`surface-ctl`). Omit to
    /// run HTTP-only (no mesh control plane).
    #[arg(short = 's', long, env = "SURFACED_SOCKET")]
    socket: Option<String>,

    /// Bearer token required on the `/mcp` agent endpoint. Omit to leave `/mcp`
    /// open (trust the LAN or a reverse proxy). Also read from SURFACED_MCP_TOKEN.
    #[arg(short = 'm', long, env = "SURFACED_MCP_TOKEN")]
    mcp_token: Option<String>,

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

    let ctl_store = Arc::clone(&store);
    let app = router_with_base(store, &args.base_path, args.mcp_token.clone());
    if args.mcp_token.is_some() {
        info!("surfaced MCP endpoint at /mcp (bearer-token gated)");
    } else {
        info!("surfaced MCP endpoint at /mcp (open — no --mcp-token)");
    }
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

    let http = async move { axum::serve(listener, app).await.context("serving HTTP") };

    // Serve the control socket alongside HTTP when configured; both share the
    // one SurfaceStore, so a control-socket push and an HTTP push are identical.
    match args.socket {
        Some(sock) => {
            info!("surfaced control socket (surface-ctl) at {sock}");
            let ctl = ctl::run(sock, ctl_store);
            tokio::try_join!(http, ctl)?;
        }
        None => {
            info!("surfaced: HTTP-only (no --socket; mesh control plane disabled)");
            http.await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid_and_exposes_version() {
        // Validates the whole arg definition (catches a clap misconfiguration).
        Args::command().debug_assert();
        // `--version` is wired, so a deployed binary can be identified from the
        // shell (complements the `/health` version field).
        assert!(Args::command().get_version().is_some());
    }
}
