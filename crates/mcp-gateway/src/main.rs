//! The **capmesh MCP gateway daemon** (DESIGN §7.2): the single agent-facing `/mcp` endpoint that
//! federates the mesh's MCP servers behind one namespaced tool surface. It reads its route table +
//! bind address from a `--config` TOML (fleet mandate seq-1377: no env vars), connects each upstream
//! at startup, and serves [`mcp_gateway::http::router`]. capmeshd drives live route changes over the
//! gateway's control socket in a later slice; this binary is the static-config floor.

use std::path::PathBuf;
use std::sync::Arc;

use mcp_gateway::config::GatewayConfig;
use mcp_gateway::daemon::connect_upstreams;
use mcp_gateway::forward::{http_client, HttpForwarder};
use mcp_gateway::http::{router, GatewayState};
use tracing::{info, warn};

/// Default config path (overridable with `--config`).
const DEFAULT_CONFIG_PATH: &str = "/etc/mcp-gateway/mcp-gateway.toml";

#[tokio::main]
async fn main() {
    // `--config <path>` (the only flag). Kept tiny on purpose — all config is the TOML (seq-1377).
    let config_path = parse_config_flag().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    // A missing/unparseable config is not fatal: come up on defaults (bind + no upstreams) so the
    // daemon is deployable before its config is rendered, matching capmeshd.
    let cfg = match GatewayConfig::load(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            // Logging isn't up yet; this goes to stderr via the default subscriber below.
            eprintln!("mcp-gateway: using default config ({e})");
            GatewayConfig::parse("").expect("empty config parses")
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(cfg.log.clone()))
        .init();

    let bind = cfg.bind.clone();
    info!(bind = %bind, upstreams = cfg.upstream.len(), "mcp-gateway starting");

    let client = http_client();
    let (federation, endpoints) = connect_upstreams(&client, &cfg.upstream).await;
    info!(federated = federation.upstream_ids().len(), "startup federation built");

    let state = GatewayState::new(federation, Arc::new(HttpForwarder::new(endpoints)));

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            warn!(bind = %bind, "failed to bind /mcp endpoint: {e}");
            std::process::exit(1);
        }
    };
    info!(bind = %bind, "serving /mcp");
    if let Err(e) = axum::serve(listener, router(state)).await {
        warn!("mcp-gateway server error: {e}");
        std::process::exit(1);
    }
}

/// Minimal `--config <path>` parse (no clap: the binary has exactly one flag).
fn parse_config_flag() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}
