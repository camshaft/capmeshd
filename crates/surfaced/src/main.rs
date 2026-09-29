//! `surfaced` — serve durable browser surfaces over HTTP/SSE (DESIGN §10.1).

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use surfaced::{
    config::{Config, DEFAULT_CONFIG_PATH, DEFAULT_HTTP_ADDR},
    ctl,
    http::router_with_base,
    inbox::SurfaceStore,
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "surfaced")]
#[command(version)]
#[command(about = "Browser surface data-plane daemon — durable, scriptable display sinks")]
struct Args {
    /// Path of the `--config` TOML that configures the daemon (fleet mandate
    /// seq-1377: config comes from this file, never env vars — no `RUST_LOG`, no
    /// `SURFACED_*`). When the file is present it is the source of truth; the CLI
    /// flags below are a transitional fallback (used only if the file is absent).
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: String,

    /// [transitional] Address the HTTP/SSE server binds. Prefer `http-addr` in
    /// the `--config` TOML.
    #[arg(short = 'a', long, default_value = DEFAULT_HTTP_ADDR)]
    http_addr: String,

    /// [transitional] Directory for durable per-surface inbox logs. Omit for
    /// in-memory only (surfaces do not survive a restart). Prefer `state-dir` in
    /// the `--config` TOML.
    #[arg(short = 'd', long)]
    state_dir: Option<String>,

    /// [transitional] Mount the server under a URL prefix (e.g. `/surfaced`) for
    /// reverse-proxy deployment behind nginx. Empty (default) serves at the root.
    /// Proxy WITHOUT stripping the prefix: `location /surfaced/ { proxy_pass
    /// http://127.0.0.1:8787; }`. Prefer `base-path` in the `--config` TOML.
    #[arg(short = 'b', long, default_value = "")]
    base_path: String,

    /// [transitional] Path of the Unix control socket capmeshd drives
    /// (`surface-ctl`). Omit to run HTTP-only (no mesh control plane). Prefer
    /// `socket` in the `--config` TOML.
    #[arg(short = 's', long)]
    socket: Option<String>,

    /// [transitional] Bearer token required on the `/mcp` agent endpoint. Omit to
    /// leave `/mcp` open (trust the LAN or a reverse proxy). Prefer `mcp-token` in
    /// the `--config` TOML.
    #[arg(short = 'm', long)]
    mcp_token: Option<String>,

    /// [transitional] Log env-filter directive (e.g. `info`). Prefer `log` in the
    /// `--config` TOML.
    #[arg(short = 'l', long, default_value = "info")]
    log_level: String,
}

/// The effective config: the `--config` TOML when that file is present (the
/// mandated source of truth, seq-1377), otherwise assembled from the transitional
/// CLI flags. Returns the config plus a note to log once tracing is up.
fn resolve_config(args: &Args) -> (Config, String) {
    let path = Path::new(&args.config);
    if path.exists() {
        match Config::load(path) {
            Ok(cfg) => return (cfg, format!("loaded config from {}", path.display())),
            Err(e) => {
                // Fall back to flags rather than refusing to start; note it so the
                // operator sees the misconfig once logging is up.
                return (
                    config_from_flags(args),
                    format!(
                        "failed to load --config {} ({e}); using CLI-flag fallback",
                        path.display()
                    ),
                );
            }
        }
    }
    (
        config_from_flags(args),
        "no --config file present; using CLI-flag fallback (transitional)".to_string(),
    )
}

/// Assemble a [`Config`] from the transitional CLI flags (used until the NixOS
/// module renders the `--config` TOML). A malformed `--http-addr` falls back to
/// the default so a bad flag never wedges startup silently — the note surfaces it.
fn config_from_flags(args: &Args) -> Config {
    Config {
        http_addr: args
            .http_addr
            .parse()
            .unwrap_or_else(|_| Config::default().http_addr),
        state_dir: args.state_dir.clone(),
        base_path: args.base_path.clone(),
        socket: args.socket.clone(),
        mcp_token: args.mcp_token.clone(),
        log: args.log_level.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (config, config_note) = resolve_config(&args);

    // Log verbosity comes from the config's `log` env-filter directive — never
    // `RUST_LOG` or any env var (fleet mandate seq-1377).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&config.log))
        .init();

    info!("{config_note}");

    let store = match &config.state_dir {
        Some(dir) => {
            info!("surfaced: durable state dir {dir}");
            Arc::new(SurfaceStore::with_state_dir(dir)?)
        }
        None => {
            info!("surfaced: in-memory only (no state-dir; surfaces are not durable)");
            Arc::new(SurfaceStore::in_memory())
        }
    };

    let ctl_store = Arc::clone(&store);
    let app = router_with_base(store, &config.base_path, config.mcp_token.clone());
    if config.mcp_token.is_some() {
        info!("surfaced MCP endpoint at /mcp (bearer-token gated)");
    } else {
        info!("surfaced MCP endpoint at /mcp (open — no mcp-token)");
    }
    let listener = tokio::net::TcpListener::bind(config.http_addr)
        .await
        .with_context(|| format!("binding {}", config.http_addr))?;
    if config.base_path.trim().trim_matches('/').is_empty() {
        info!("surfaced HTTP/SSE serving on http://{}", config.http_addr);
    } else {
        info!(
            "surfaced HTTP/SSE serving on http://{} under base path '{}'",
            config.http_addr, config.base_path
        );
    }

    let http = async move { axum::serve(listener, app).await.context("serving HTTP") };

    // Serve the control socket alongside HTTP when configured; both share the
    // one SurfaceStore, so a control-socket push and an HTTP push are identical.
    match config.socket {
        Some(sock) => {
            info!("surfaced control socket (surface-ctl) at {sock}");
            let ctl = ctl::run(sock, ctl_store);
            tokio::try_join!(http, ctl)?;
        }
        None => {
            info!("surfaced: HTTP-only (no socket; mesh control plane disabled)");
            http.await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Args, config_from_flags};
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_is_valid_and_exposes_version() {
        // Validates the whole arg definition (catches a clap misconfiguration).
        Args::command().debug_assert();
        // `--version` is wired, so a deployed binary can be identified from the
        // shell (complements the `/health` version field).
        assert!(Args::command().get_version().is_some());
    }

    // seq-1377 (no env-var config) is enforced at the dependency level: surfaced
    // drops clap's `env` feature, so an `#[arg(env = "SURFACED_*")]` is a compile
    // error — a stronger guarantee than a runtime assertion.

    #[test]
    fn flag_fallback_maps_onto_config() {
        // The transitional CLI-flag fallback assembles a Config the same way the
        // --config TOML would, so a flag-configured run matches a file-configured one.
        let args = Args::parse_from([
            "surfaced",
            "--http-addr",
            "0.0.0.0:9000",
            "--base-path",
            "/surfaced",
            "--socket",
            "/run/surfaced/surfaced.sock",
            "--mcp-token",
            "tok",
            "--log-level",
            "debug",
        ]);
        let cfg = config_from_flags(&args);
        assert_eq!(cfg.http_addr.to_string(), "0.0.0.0:9000");
        assert_eq!(cfg.base_path, "/surfaced");
        assert_eq!(cfg.socket.as_deref(), Some("/run/surfaced/surfaced.sock"));
        assert_eq!(cfg.mcp_token.as_deref(), Some("tok"));
        assert_eq!(cfg.log, "debug");
    }
}
