use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use nmidid::config::{Config, DEFAULT_CONFIG_PATH};
use nmidid::mounts::{MidirMounter, MountRegistry};
use nmidid::ports::MidirPortProvider;
use nmidid::pump::RtpConnector;
use nmidid::server::PeerPolicy;
use nmidid::{hotplug, server};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "nmidid")]
#[command(about = "MIDI data-plane daemon — serves the capmesh-ctl control socket")]
struct Args {
    /// Path of the `--config` TOML that configures the daemon (fleet mandate
    /// seq-1377: config comes from this file, never env vars). When the file is
    /// present it is the source of truth; the CLI flags below are a transitional
    /// fallback (used only if the file is absent) and will be removed once the
    /// NixOS module renders the TOML.
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: String,

    /// [transitional] Path of the Unix control socket to bind. Prefer `socket` in
    /// the `--config` TOML.
    #[arg(short, long, default_value = "/run/nmidid.sock")]
    socket: String,

    /// [transitional] How often to poll for local MIDI port changes (hot-plug),
    /// in seconds. Prefer `monitor-interval` in the `--config` TOML.
    #[arg(long, default_value = "5")]
    monitor_interval: u64,

    /// [transitional] Permit control connections from this uid (repeatable).
    /// Prefer `allow-uids` in the `--config` TOML. §1.1
    #[arg(long)]
    allow_uid: Vec<u32>,

    /// [transitional] Permit control connections from this gid (repeatable).
    /// Prefer `allow-gids` in the `--config` TOML. §1.1
    #[arg(long)]
    allow_gid: Vec<u32>,

    /// [transitional] Permit control connections from this group NAME
    /// (repeatable). Prefer `allow-groups` in the `--config` TOML. §1.1
    #[arg(long)]
    allow_group: Vec<String>,

    /// [transitional] Log env-filter directive (e.g. `info`). Prefer `log` in the
    /// `--config` TOML.
    #[arg(short, long, default_value = "info")]
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
/// module renders the `--config` TOML).
fn config_from_flags(args: &Args) -> Config {
    Config {
        socket: args.socket.clone(),
        monitor_interval: args.monitor_interval,
        log: args.log_level.clone(),
        allow_uids: args.allow_uid.clone(),
        allow_gids: args.allow_gid.clone(),
        allow_groups: args.allow_group.clone(),
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
    info!("Starting nmidid, control socket at {}", config.socket);

    let ports = Arc::new(MidirPortProvider);
    let mounts = Arc::new(MountRegistry::new(Arc::new(MidirMounter), Arc::new(RtpConnector)));

    // Hot-plug notifications (§5, `hotplug-events`): watch local ports and emit
    // port-added/port-removed on the daemon's notification bus.
    let port_rx =
        nmidi_core::midi::start_port_monitor(Duration::from_secs(config.monitor_interval)).await;
    hotplug::spawn(port_rx, mounts.notifier());

    // Resolve any allow-group names to gids and merge them into the gid
    // allow-list. An unresolved group is a hard error rather than a silent
    // drop: silently dropping the only allow-rule would fail *open* (enforcement
    // off), so we fail closed and loud instead.
    let mut allow_gids = config.allow_gids;
    for name in &config.allow_groups {
        match server::resolve_group_gid(name) {
            Some(gid) => {
                info!("authorizing control group {name} (gid {gid})");
                allow_gids.push(gid);
            }
            None => anyhow::bail!("allow-group {name}: no such group in /etc/group"),
        }
    }

    let peers = PeerPolicy::new(config.allow_uids, allow_gids);
    server::run(&config.socket, ports, mounts, peers).await
}
