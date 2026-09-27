//! capmeshd — the LAN capability mesh control-plane daemon (DESIGN.md).
//!
//! M0b slice: stateless control plane, MIDI-first. This entrypoint loads the §4.1 TOML
//! config, advertises this host's configured capability kinds over `_capmesh._tcp`, and
//! browses the mesh — logging discovered peers by the IP from their mDNS record (§5).
//! The data-plane control-socket client (the `nmidi-ctl` midi adapter), the desired-mount
//! reconciler, and the control API land in following slices.

mod config;
mod discovery;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use discovery::{CapabilityAdvert, ServiceAdvertiser};
use mdns_sd::ServiceEvent;
use std::path::PathBuf;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "capmeshd",
    version,
    about = "LAN capability mesh control-plane daemon"
)]
struct Args {
    /// Path to the §4.1 TOML config.
    #[arg(long, default_value = config::DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    /// Override the advertised control-endpoint port.
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // A missing config is not fatal at M0b — we run with defaults (hostname, no
    // dataplanes) so `capmeshd` is deployable before the config is rendered.
    let mut cfg = match Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("using default config ({e:#})");
            Config::parse("").expect("empty config parses")
        }
    };
    if let Some(port) = args.port {
        cfg.advertise_port = port;
    }

    let host_id = cfg.resolved_host_id();
    info!(host = %host_id, port = cfg.advertise_port, "capmeshd starting");

    let advertiser = ServiceAdvertiser::new().context("start mDNS advertiser")?;
    // Coarse host-level advert per configured data-plane kind. Per-port records with
    // real directions arrive once the ctl client can query the daemon's `list-ports`.
    let mut adverts = Vec::new();
    for (kind, dp) in &cfg.dataplane {
        let id = format!("{host_id}-{kind}");
        let advert = CapabilityAdvert {
            cap: kind.clone(),
            dir: "duplex".to_string(),
            id: id.clone(),
            host: host_id.clone(),
            ep: cfg.advertise_port,
            descr: format!("/caps/{id}"),
        };
        match advertiser.advertise(&advert) {
            Ok(handle) => {
                let via = dp
                    .socket
                    .as_ref()
                    .map(|s| s.display().to_string())
                    .or_else(|| dp.endpoint.clone())
                    .unwrap_or_default();
                info!(cap = %kind, protocol = %dp.protocol, %via, "advertising capability");
                adverts.push(handle);
            }
            Err(e) => warn!(cap = %kind, "failed to advertise: {e:#}"),
        }
    }
    if adverts.is_empty() {
        info!("no data-plane kinds configured; browsing only");
    }

    let events = discovery::browse().context("start mDNS browse")?;
    info!("browsing {} for peers", discovery::SERVICE_TYPE);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
            ev = events.recv_async() => {
                match ev {
                    Ok(ServiceEvent::ServiceResolved(svc)) => {
                        match discovery::resolved_addr(&svc) {
                            Some(addr) => {
                                let cap = discovery::advert_from_resolved(&svc)
                                    .map(|a| format!("{}/{}", a.cap, a.dir))
                                    .unwrap_or_else(|_| "unknown".to_string());
                                info!(
                                    fullname = %svc.get_fullname(),
                                    %addr,
                                    port = svc.get_port(),
                                    %cap,
                                    "resolved capmesh peer"
                                );
                            }
                            None => warn!(
                                fullname = %svc.get_fullname(),
                                "peer resolved with no IP address; skipping"
                            ),
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_ty, fullname)) => {
                        info!(%fullname, "capmesh peer removed");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("mDNS browse channel closed: {e}");
                        break;
                    }
                }
            }
        }
    }

    // Adverts unregister on drop.
    drop(adverts);
    Ok(())
}
