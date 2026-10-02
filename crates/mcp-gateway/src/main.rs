//! The **capmesh MCP gateway daemon** (DESIGN §7.2): the single agent-facing `/mcp` endpoint that
//! federates the mesh's MCP servers behind one namespaced tool surface. It reads its route table +
//! bind address from a `--config` TOML (fleet mandate seq-1377: no env vars), connects each upstream
//! at startup, and serves [`mcp_gateway::http::router`]. capmeshd drives live route changes over the
//! gateway's control socket in a later slice; this binary is the static-config floor.

use std::path::PathBuf;
use std::sync::Arc;

use mcp_gateway::config::GatewayConfig;
use mcp_gateway::daemon::connect_upstreams;
use mcp_gateway::forward::{HttpForwarder, http_client};
use mcp_gateway::http::{GatewayState, router};
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
    let (fed, endpoints) = connect_upstreams(&client, &cfg.upstream).await;
    // Upstreams that didn't connect at startup (still booting, or ordered after the gateway) are
    // retried live in the background below, so the gateway need not start strictly after its
    // upstreams. `__`-invalid ids never federate, so they are not retry-eligible.
    let federated: std::collections::HashSet<String> =
        fed.upstream_ids().into_iter().map(str::to_string).collect();
    let pending: Vec<_> = cfg
        .upstream
        .iter()
        .filter(|u| !u.id.contains(mcp_gateway::NS_SEP) && !federated.contains(&u.id))
        .cloned()
        .collect();
    info!(
        federated = federated.len(),
        pending = pending.len(),
        "startup federation built"
    );

    // The federation + forwarder are shared: the /mcp server serves them, and the admin control
    // channel (de)federates them live — both see the same state.
    let federation = Arc::new(tokio::sync::RwLock::new(fed));
    let forwarder = Arc::new(HttpForwarder::new(endpoints));
    // Change notifier: the admin channel signals it on a (de)federate; each GET /mcp SSE stream
    // subscribes and pushes tools/list_changed.
    let (notifier, _) = tokio::sync::broadcast::channel(16);
    let state = GatewayState {
        federation: federation.clone(),
        forwarder: forwarder.clone(),
        notifier: notifier.clone(),
    };

    // Retry any upstream that didn't connect at startup, federating it live when it comes up (lifts
    // the gateway-after-upstreams start-ordering constraint). Best-effort background task.
    if !pending.is_empty() {
        warn!(
            pending = pending.len(),
            "retrying unconnected upstreams in the background until they come up"
        );
        let (client, federation, forwarder, notifier) = (
            client.clone(),
            federation.clone(),
            forwarder.clone(),
            notifier.clone(),
        );
        tokio::spawn(mcp_gateway::daemon::retry_pending_upstreams(
            client,
            federation,
            forwarder,
            notifier,
            pending,
            mcp_gateway::daemon::RETRY_INTERVAL,
            mcp_gateway::daemon::RETRY_MAX_ATTEMPTS,
        ));
    }

    // Control channel (DESIGN §7.2): capmeshd drives (de)federation over this loopback admin API.
    // Best-effort: a bind failure is logged, not fatal.
    if let Some(admin_addr) = cfg.admin_addr.clone() {
        let admin = mcp_gateway::admin::AdminState {
            client: client.clone(),
            federation: federation.clone(),
            forwarder: forwarder.clone(),
            notifier: notifier.clone(),
        };
        match tokio::net::TcpListener::bind(&admin_addr).await {
            Ok(listener) => {
                info!(addr = %admin_addr, "serving gateway control channel /admin");
                tokio::spawn(async move {
                    if let Err(e) =
                        axum::serve(listener, mcp_gateway::admin::admin_router(admin)).await
                    {
                        warn!("gateway control channel error: {e}");
                    }
                });
            }
            Err(e) => warn!(addr = %admin_addr, "control channel: bind failed: {e}"),
        }
    }

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
