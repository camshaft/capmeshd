//! capmeshd — the LAN capability mesh control-plane daemon (DESIGN.md).
//!
//! Stateless control plane, MIDI-first. The default invocation runs the daemon: it loads the
//! §4.1 TOML config, advertises this host's configured capability kinds over `_capmesh._tcp`,
//! browses the mesh (correlating each peer's `_apple-midi._udp` control port by IP, §5), and
//! auto-mounts any discovered peer matching an `[[automount]]` rule — fetching the peer's typed
//! descriptor, negotiating a format, and issuing the mount to the local data-plane daemon, then
//! tearing `while-advertised` mounts down when the source's advert is removed (§6.1). The
//! subcommands drive a data-plane daemon's `capmesh-ctl` socket ([`ctl`]) directly: `probe-ctl`
//! (hello + list-ports), `connect`/`connect-discover` (mount by explicit coordinates or by mesh
//! selector), `mount-status`, and `unmount`. Desired (`permanent`) mounts are reconciled by
//! [`reconcile::Reconciler`].

mod config;
mod control_ops;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
};
use capmesh_ctl::{
    CtlClient, CtlError, Format, LocalEndpoint, McpRoute, McpTransport, MountRole, MountSpec,
    RemoteEndpoint,
};
use capmesh_daemon::automount::{
    self, ActiveAutoMount, AutoMountOutcome, Lifetime, Remote, teardown_on_unadvertise,
};
use capmesh_daemon::gateway_driver::{self, GatewayCommand, GatewayClient};
use capmesh_daemon::mcp_routes::{McpRouteRegistry, RegisterOutcome, RouteRequest, RouteResponse};
use capmesh_daemon::negotiate;
use capmesh_daemon::reconcile::{self, Reconciler};
use capmesh_discovery::{self as discovery, AppleMidiPeers, PendingAutoMounts, PendingPeer};
use capmesh_mesh::server::CapabilityProvider;
use capmesh_model::{CapabilityDescriptor, PortDescriptor};
use clap::{Parser, Subcommand};
use config::{Automount, Config};
use discovery::{CapabilityAdvert, ServiceAdvertiser};
use mdns_sd::ServiceEvent;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

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

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Connect to a data-plane daemon's capmesh-ctl socket, run hello + list-ports, and
    /// print what it reports — the midi adapter's integration probe against `nmidid`.
    ProbeCtl {
        /// Path to the daemon's Unix control socket (e.g. /run/nmidid.sock).
        #[arg(long)]
        socket: PathBuf,
        /// After hello + list-ports, keep listening and print daemon notifications (§5).
        #[arg(long)]
        watch: bool,
    },
    /// Browse the mesh and list discovered capmesh capabilities (§7 discover).
    Discover {
        /// How long to browse before printing, in seconds.
        #[arg(long, default_value_t = 3)]
        timeout_secs: u64,
        /// Only show this capability kind (midi | audio | …).
        #[arg(long)]
        kind: Option<String>,
        /// Only show this direction (source | sink | duplex | control).
        #[arg(long)]
        dir: Option<String>,
        /// Only show capabilities on this host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Connect a remote source to a local mount, negotiating the format first (§4, §7).
    Connect {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        remote_host: String,
        /// Remote peer IP from the mDNS record (never a `.local`/`.lan` name).
        #[arg(long)]
        remote_addr: IpAddr,
        #[arg(long)]
        remote_port: u16,
        #[arg(long)]
        remote_port_id: String,
        #[arg(long, default_value = "mirror-source")]
        role: String,
        #[arg(long)]
        local_name: Option<String>,
        /// Local codecs in preference order (the consuming side); defaults to [midi1].
        #[arg(long = "local-codec")]
        local_codecs: Vec<String>,
        /// The remote source's advertised codecs; defaults to [midi1]. (Until the mesh
        /// descriptor fetch lands, the remote formats are supplied here.)
        #[arg(long = "remote-codec")]
        remote_codecs: Vec<String>,
        #[arg(long)]
        mount_id: Option<String>,
    },
    /// Discover a capability by selector and connect it in one command (§6.1/§7): browses the
    /// mesh, selects the single matching capability, learns its address, port, control port, and
    /// codecs by itself, then mounts. The discovery-driven form of `connect` — no manual
    /// `--remote-addr`/`--remote-port`/`--remote-port-id`/`--remote-codec`.
    ConnectDiscover {
        /// Path to the local data-plane daemon's Unix control socket (e.g. /run/nmidid.sock).
        #[arg(long)]
        socket: PathBuf,
        /// Select by capability kind (e.g. `midi`); omit to match any kind.
        #[arg(long)]
        kind: Option<String>,
        /// Select by advertising host-id; omit to match any host.
        #[arg(long)]
        host: Option<String>,
        /// Select by exact capability id; pins one capability when kind/host are ambiguous.
        #[arg(long)]
        id: Option<String>,
        /// Restrict to a remote port direction (`source | sink`); defaults to the direction the
        /// role implies (mirror-source→source, mirror-sink→sink), so a device exposing both binds
        /// the right one.
        #[arg(long)]
        dir: Option<String>,
        /// Bind a specific remote port-id; defaults to the first port matching kind + direction.
        #[arg(long)]
        port: Option<String>,
        /// The role the local daemon materializes (§3.1).
        #[arg(long, default_value = "mirror-source")]
        role: String,
        /// Display name for the local virtual device (mirror roles).
        #[arg(long)]
        local_name: Option<String>,
        /// Local codecs in preference order (the consuming side); defaults to [midi1].
        #[arg(long = "local-codec")]
        local_codecs: Vec<String>,
        /// How long to browse for the capability + its data-plane record before giving up.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
        /// Mount id (idempotency key); defaults to the capability id.
        #[arg(long)]
        mount_id: Option<String>,
    },
    /// Establish a mount on a daemon (§3): create/attach a p2p link to a remote port.
    Mount(MountArgs),
    /// Tear a mount down (§3).
    Unmount {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        mount_id: String,
    },
    /// Print live mounts on a daemon (§3).
    MountStatus {
        #[arg(long)]
        socket: PathBuf,
        /// Restrict to one mount id.
        #[arg(long)]
        mount_id: Option<String>,
        /// Emit the result as machine-readable JSON on stdout (stable wire field/enum names)
        /// instead of human log lines — for scripts, agents, and the rehearsal harness.
        #[arg(long)]
        json: bool,
    },
    /// Reconcile a desired mount against a daemon (DESIGN §6): issue it if absent/failed,
    /// leave it if live. With --interval-secs > 0, run the reconcile loop.
    Reconcile {
        #[command(flatten)]
        mount: MountArgs,
        /// Poll interval in seconds; 0 = reconcile once and exit (ignored with --watch).
        #[arg(long, default_value_t = 0)]
        interval_secs: u64,
        /// After the initial pass, react to daemon mount-state/hotplug notifications (§5)
        /// and re-reconcile on drift — event-driven, no polling.
        #[arg(long)]
        watch: bool,
    },
}

/// The inputs that describe one desired mount — shared by `mount` and `reconcile`.
#[derive(clap::Args, Debug)]
struct MountArgs {
    /// Path to the daemon's Unix control socket (e.g. /run/nmidid.sock).
    #[arg(long)]
    socket: PathBuf,
    /// Remote peer host-id (from discovery).
    #[arg(long)]
    remote_host: String,
    /// Remote peer IP — the address from the mDNS record (never a `.local`/`.lan` name).
    #[arg(long)]
    remote_addr: IpAddr,
    /// Remote peer data-plane port.
    #[arg(long)]
    remote_port: u16,
    /// Remote port-id to mount.
    #[arg(long)]
    remote_port_id: String,
    /// Which end the daemon materializes: mirror-source | mirror-sink | link.
    #[arg(long, default_value = "mirror-source")]
    role: String,
    /// Display name for the local virtual device (mirror roles).
    #[arg(long)]
    local_name: Option<String>,
    /// Chosen wire-format codec.
    #[arg(long, default_value = "midi1")]
    codec: String,
    /// Mount id (idempotency key); defaults to <remote-host>-<remote-port-id>.
    #[arg(long)]
    mount_id: Option<String>,
}

impl MountArgs {
    /// Build the `MountSpec` this describes (§3.1).
    fn to_spec(&self) -> Result<MountSpec> {
        let role = parse_role(&self.role)?;
        let mount_id = self
            .mount_id
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.remote_host, self.remote_port_id));
        Ok(MountSpec {
            mount_id,
            role,
            local: LocalEndpoint {
                // Mirror roles materialize a local virtual endpoint; `link` uses a real port.
                is_virtual: !matches!(role, MountRole::Link),
                name: self.local_name.clone(),
                // CLI connect does not yet name a local real port for `link` (follow-on).
                port_id: None,
            },
            remote: RemoteEndpoint {
                host: self.remote_host.clone(),
                addr: self.remote_addr,
                port: self.remote_port,
                port_id: self.remote_port_id.clone(),
            },
            format: Format {
                codec: self.codec.clone(),
                params: Default::default(),
            },
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Log verbosity is TOML config, not an env var (fleet mandate seq-1377: no `RUST_LOG`). Read
    // it from `--config` before anything logs; a missing/unparseable config falls back to `info`
    // (the daemon path below re-loads and warns), so logging always comes up.
    let log_directive = Config::load(&args.config)
        .map(|c| c.log)
        .unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_directive))
        .init();

    match &args.cmd {
        Some(Cmd::ProbeCtl { socket, watch }) => return probe_ctl(socket, *watch).await,
        Some(Cmd::Discover {
            timeout_secs,
            kind,
            dir,
            host,
        }) => {
            return cmd_discover(
                *timeout_secs,
                kind.as_deref(),
                dir.as_deref(),
                host.as_deref(),
            )
            .await;
        }
        Some(Cmd::Connect {
            socket,
            remote_host,
            remote_addr,
            remote_port,
            remote_port_id,
            role,
            local_name,
            local_codecs,
            remote_codecs,
            mount_id,
        }) => {
            return cmd_connect(
                socket,
                remote_host,
                *remote_addr,
                *remote_port,
                remote_port_id,
                role,
                local_name.clone(),
                local_codecs,
                remote_codecs,
                mount_id.clone(),
            )
            .await;
        }
        Some(Cmd::ConnectDiscover {
            socket,
            kind,
            host,
            id,
            dir,
            port,
            role,
            local_name,
            local_codecs,
            timeout_secs,
            mount_id,
        }) => {
            return cmd_connect_discover(
                socket,
                kind.as_deref(),
                host.as_deref(),
                id.as_deref(),
                dir.as_deref(),
                port.as_deref(),
                role,
                local_name.clone(),
                local_codecs,
                *timeout_secs,
                mount_id.clone(),
            )
            .await;
        }
        Some(Cmd::Mount(m)) => return cmd_mount(m).await,
        Some(Cmd::Unmount { socket, mount_id }) => return cmd_unmount(socket, mount_id).await,
        Some(Cmd::MountStatus { socket, mount_id, json }) => {
            return cmd_mount_status(socket, mount_id.as_deref(), *json).await;
        }
        Some(Cmd::Reconcile {
            mount,
            interval_secs,
            watch,
        }) => return cmd_reconcile(mount, *interval_secs, *watch).await,
        None => {}
    }

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
    // Advertise capmesh's own control-tools MCP server (§7.1) as `cap=mcp` when it is served, so
    // peers and the gateway discover + federate it. Best-effort: a bad addr / publish is logged.
    if let Some(serve_addr) = &cfg.mcp_serve_addr {
        match mcp_capability_advert(&host_id, serve_addr) {
            Ok(advert) => match advertiser.advertise(&advert) {
                Ok(handle) => {
                    info!(port = advert.ep, "advertising cap=mcp control server");
                    adverts.push(handle);
                }
                Err(e) => warn!("failed to advertise cap=mcp: {e:#}"),
            },
            Err(e) => warn!("cap=mcp advert skipped: {e:#}"),
        }
    }

    if adverts.is_empty() {
        info!("no data-plane kinds configured; browsing only");
    }

    // Reconcile permanent mounts (DESIGN §9) against the local MIDI data-plane daemon at
    // startup. Best-effort: if the daemon socket is down (e.g. nmidid not up yet), log and
    // carry on — a later tick's reconcile / the daemon coming up converges it.
    reconcile_permanent_mounts(&cfg).await;

    // Seed the MCP route registry (DESIGN §7.2) from the statically-declared `[[mcp-route]]`
    // entries — the "explicit floor" for known upstreams. The runtime register endpoint and
    // `cap=mcp` auto-discovery mutate the same registry; the gateway is driven off it.
    let registry = build_mcp_route_registry(&cfg);
    info!(routes = registry.len(), "mcp route registry seeded from config");
    // The registry is shared unconditionally: the runtime control endpoint (below), the manual
    // `register` API, AND the discovery browse loop's `cap=mcp` auto-registration all mutate it.
    let shared: SharedMcpRoutes = Arc::new(Mutex::new(registry));
    // When a gateway admin URL is configured, drive it live on every route change (§7.2) — from the
    // control endpoint and from `cap=mcp` auto-discovery alike.
    let gateway = cfg.gateway_admin_url.clone().map(|base_url| {
        info!(%base_url, "driving mcp gateway on route changes");
        GatewayDriver {
            client: gateway_driver::gateway_client(),
            base_url,
        }
    });

    // Serve the runtime MCP route control endpoint (DESIGN §7.2 `register`) when configured, so an
    // operator can add/remove routes live. It shares the seeded registry + gateway driver.
    // Best-effort: a bind failure is logged, not fatal.
    if let Some(addr) = cfg.mcp_control_addr.clone() {
        let state = McpControlState {
            routes: shared.clone(),
            gateway: gateway.clone(),
        };
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                info!(%addr, "mcp route control endpoint listening");
                let router = mcp_control_router(state);
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, router).await {
                        warn!("mcp route control endpoint server error: {e}");
                    }
                });
            }
            Err(e) => warn!(%addr, "mcp route control endpoint: bind failed: {e}"),
        }
    }

    // Serve capmesh's OWN control-tools MCP server (DESIGN §7.1) when configured, so the gateway can
    // federate capmesh as a `cap=mcp` upstream. Its control ops drive the configured data-plane
    // socket (MIDI-first: the `midi` dataplane's socket, else the first configured socket).
    // Best-effort: a bind failure is logged, not fatal.
    if let Some(addr) = cfg.mcp_serve_addr.clone() {
        let socket = cfg
            .dataplane
            .get("midi")
            .and_then(|dp| dp.socket.clone())
            .or_else(|| cfg.dataplane.values().find_map(|dp| dp.socket.clone()));
        let ops = control_ops::CapmeshControlOps::new(socket);
        let executor = capmesh_daemon::control_exec::OpsExecutor::new(Arc::new(ops));
        let state = capmesh_daemon::control_http::ControlState::new(Arc::new(executor));
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                info!(%addr, "control-tools MCP server listening");
                let router = capmesh_daemon::control_http::control_mcp_router(state);
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, router).await {
                        warn!("control-tools MCP server error: {e}");
                    }
                });
            }
            Err(e) => warn!(%addr, "control-tools MCP server: bind failed: {e}"),
        }
    }

    // Serve the mesh control endpoint (docs/MESH-PROTOCOL.md) on the advertised port so peers
    // can resolve this host's `descr` pointers into full capability descriptors. The provider
    // projects the configured data-plane daemons' live `list-ports` on each request.
    let provider = Arc::new(DataplaneCaps {
        host_id: host_id.clone(),
        kinds: cfg
            .dataplane
            .iter()
            .filter_map(|(k, dp)| dp.socket.clone().map(|s| (k.clone(), s)))
            .collect(),
    });
    match tokio::net::TcpListener::bind(("0.0.0.0", cfg.advertise_port)).await {
        Ok(listener) => {
            info!(port = cfg.advertise_port, "serving mesh control endpoint");
            tokio::spawn(capmesh_mesh::server::serve(listener, provider));
        }
        Err(e) => warn!(
            port = cfg.advertise_port,
            "failed to bind mesh control endpoint: {e:#}"
        ),
    }

    let events = discovery::browse().context("start mDNS browse")?;
    info!("browsing {} for peers", discovery::SERVICE_TYPE);

    // Also browse `_apple-midi._udp` to learn MIDI peers' data-plane control ports, correlated
    // to capmesh nodes by IP (DESIGN §5); auto-mount reads this when issuing a MIDI mount.
    let apple_events = discovery::browse_apple_midi().context("start AppleMIDI browse")?;
    let apple_peers = Arc::new(Mutex::new(AppleMidiPeers::new()));

    // The auto-mount rules + per-kind control sockets + local codecs, shared into each spawned
    // issuance task (§6.1).
    let auto = Arc::new(AutoMount {
        own_host: host_id.clone(),
        rules: cfg.automount.clone(),
        sockets: cfg
            .dataplane
            .iter()
            .filter_map(|(k, dp)| dp.socket.clone().map(|s| (k.clone(), s)))
            .collect(),
        local_codecs: vec![Format {
            codec: "midi1".to_string(),
            params: serde_json::Map::new(),
        }],
    });

    // Auto-mounts issued so far, keyed by the source peer's mDNS fullname, so a `ServiceRemoved`
    // can tear down its `while-advertised` mounts (§6.1). `permanent` mounts stay tracked and
    // mounted across advert removal.
    let active_mounts: ActiveMounts = Arc::new(Mutex::new(AutoMountRegistry::default()));

    // Peers whose auto-mount matched but await their AppleMIDI control port (§6.1). Drained +
    // re-tried when the peer's `_apple-midi._udp` record resolves, so a capmesh advert that
    // arrives before the apple-midi record does not wait for the next mDNS re-resolve.
    let pending: Arc<Mutex<PendingAutoMounts>> = Arc::new(Mutex::new(PendingAutoMounts::new()));

    // The shared context cloned into each spawned auto-mount task (Arc clones are cheap).
    let ctx = MountCtx {
        auto: Arc::clone(&auto),
        apple_peers: Arc::clone(&apple_peers),
        active_mounts: Arc::clone(&active_mounts),
        pending: Arc::clone(&pending),
    };

    // Registered `cap=mcp` peers, keyed by mDNS fullname → the route id auto-registration used, so a
    // `ServiceRemoved` can defederate the right upstream (§7.2 teardown — the MCP analogue of a
    // while-advertised auto-mount teardown). Loop-local: both browse arms run in this single task, so
    // no shared lock is needed. `register`/`unregister` on `shared` stay the source of truth.
    let mut mcp_peers: HashMap<String, String> = HashMap::new();

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
                                match discovery::advert_from_resolved(&svc) {
                                    Ok(advert) => {
                                        info!(
                                            fullname = %svc.get_fullname(),
                                            %addr,
                                            port = svc.get_port(),
                                            cap = %format!("{}/{}", advert.cap, advert.dir),
                                            "resolved capmesh peer"
                                        );
                                        if advert.cap == "mcp" {
                                            // A `cap=mcp` peer is an MCP server, not a data-plane
                                            // capability to mount: auto-register it into the route
                                            // registry + federate it on the gateway (§7.2). No
                                            // descriptor fetch / mount planning applies. Remember the
                                            // fullname → route-id so a later removal defederates it.
                                            auto_register_mcp(&shared, &gateway, &advert, addr);
                                            mcp_peers.insert(svc.get_fullname().to_string(), advert.id.clone());
                                        } else {
                                            // Mark the advert live *before* spawning, so an issuance
                                            // that outraces a `ServiceRemoved` sees the removal (§6.1).
                                            let fullname = svc.get_fullname().to_string();
                                            active_mounts.lock().unwrap().live.insert(fullname.clone());
                                            // Issue any matching auto-mounts off the browse loop:
                                            // fetching the descriptor is network I/O.
                                            tokio::spawn(try_auto_mount(
                                                ctx.clone(),
                                                advert,
                                                addr,
                                                svc.get_port(),
                                                fullname,
                                            ));
                                        }
                                    }
                                    Err(e) => info!(
                                        fullname = %svc.get_fullname(),
                                        %addr,
                                        port = svc.get_port(),
                                        "resolved capmesh peer (advert parse failed: {e:#})"
                                    ),
                                }
                            }
                            None => warn!(
                                fullname = %svc.get_fullname(),
                                "peer resolved with no IP address; skipping"
                            ),
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_ty, fullname)) => {
                        info!(%fullname, "capmesh peer removed");
                        // A removed `cap=mcp` peer: drop its route + defederate it on the gateway
                        // (§7.2 teardown, symmetric with the resolve-time auto-registration).
                        if let Some(route_id) = mcp_peers.remove(&fullname) {
                            auto_unregister_mcp(&shared, &gateway, &route_id);
                        }
                        // Tear down this peer's `while-advertised` auto-mounts (§6.1); retain any
                        // `permanent` ones (they outlive the advert, like a permanent mount).
                        let torn = {
                            let mut reg = active_mounts.lock().unwrap();
                            reg.live.remove(&fullname);
                            match reg.active.remove(&fullname) {
                                Some(mounts) => {
                                    let (to_unmount, retain) = teardown_on_unadvertise(mounts);
                                    if !retain.is_empty() {
                                        reg.active.insert(fullname.clone(), retain);
                                    }
                                    to_unmount
                                }
                                None => Vec::new(),
                            }
                        };
                        for m in torn {
                            if let Some(socket) = auto.sockets.get(&m.kind) {
                                let socket = socket.clone();
                                tokio::spawn(async move {
                                    issue_unmount(&socket, &m.mount_id).await;
                                });
                            } else {
                                warn!(kind = %m.kind, mount_id = %m.mount_id,
                                    "auto-mount teardown: no local socket for this kind; skipping");
                            }
                        }
                        // Drop any auto-mounts of this peer still awaiting a control port: its
                        // advert is gone, so they can no longer be issued (§6.1).
                        pending.lock().unwrap().forget_fullname(&fullname);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("mDNS browse channel closed: {e}");
                        break;
                    }
                }
            }
            aev = apple_events.recv_async() => {
                match aev {
                    Ok(ServiceEvent::ServiceResolved(svc)) => {
                        if let Some(addr) = discovery::resolved_addr(&svc) {
                            let control_port = svc.get_port();
                            apple_peers.lock().unwrap().observe(addr, control_port);
                            debug!(%addr, control_port, "observed AppleMIDI peer");
                            // Re-try any auto-mounts that matched this peer but were awaiting its
                            // control port — now known — instead of waiting for a capmesh re-resolve.
                            let waiting = pending.lock().unwrap().take(&addr);
                            for p in waiting {
                                debug!(%addr, fullname = %p.fullname,
                                    "auto-mount: control port learned, re-trying pending peer");
                                tokio::spawn(try_auto_mount(
                                    ctx.clone(),
                                    p.advert,
                                    addr,
                                    p.ep,
                                    p.fullname,
                                ));
                            }
                        }
                    }
                    // A removed record leaves a stale entry until re-observed; harmless — a mount
                    // to a vanished peer just fails and the reconciler/dead-peer detection handle it.
                    Ok(_) => {}
                    Err(e) => warn!("AppleMIDI browse channel closed: {e}"),
                }
            }
        }
    }

    // Adverts unregister on drop.
    drop(adverts);
    Ok(())
}

/// This control plane's auto-mount registry (§6.1): which source adverts are currently live on
/// the mesh, and the mounts issued so far per source peer's mDNS fullname. Both live behind one
/// lock so a `ServiceRemoved` and a still-in-flight `try_auto_mount` for the same peer serialize
/// — closing the race where a mount issued *after* its advert was removed would otherwise be
/// tracked but never torn down (the removal's teardown already ran against an empty entry).
///
/// Distinct from the data-plane daemon's `nmidid::mounts::MountRegistry`: this tracks *auto-mount
/// decisions* the control plane made, not the live data-plane mounts themselves.
#[derive(Default)]
struct AutoMountRegistry {
    /// Fullnames of adverts currently present on the mesh (inserted on `ServiceResolved`, removed
    /// on `ServiceRemoved`). Consulted at issuance to detect an advert that vanished mid-mount.
    live: std::collections::HashSet<String>,
    /// Issued auto-mounts keyed by the source peer's fullname — consulted by `ServiceRemoved` to
    /// honor each mount's [`Lifetime`].
    active: HashMap<String, Vec<ActiveAutoMount>>,
}
/// Shared across the browse loop and every spawned issuance task (cheap `Arc` clone).
type ActiveMounts = Arc<Mutex<AutoMountRegistry>>;

/// The shared, cheaply-cloneable context every auto-mount task needs: the rules+sockets+codecs,
/// the AppleMIDI control-port map, the issued-mount registry (for lifetime teardown), and the
/// awaiting-control-port parking (for the apple-midi re-trigger). Cloned into each spawned task.
#[derive(Clone)]
struct MountCtx {
    auto: Arc<AutoMount>,
    apple_peers: Arc<Mutex<AppleMidiPeers>>,
    active_mounts: ActiveMounts,
    pending: Arc<Mutex<PendingAutoMounts>>,
}

/// The auto-mount inputs shared into each spawned issuance task: this host's own id (to skip
/// self-adverts), the config's rules, the per-kind `capmesh-ctl` sockets to issue against, and
/// the local side's preferred codecs.
struct AutoMount {
    own_host: String,
    rules: Vec<Automount>,
    sockets: HashMap<String, PathBuf>,
    local_codecs: Vec<Format>,
}

/// The auto-mount rules that apply to a freshly-resolved advert (§6.1): none if the advert is
/// this daemon's own — a node never mirrors the capability it itself advertises, and it would
/// otherwise discover its own `_capmesh._tcp` record over multicast and try to mount itself —
/// otherwise the rules whose `kind` coarse-matches. `dir`/`port` are deliberately not matched
/// here (the coarse advert can't carry them); they are applied against the fetched descriptor's
/// real ports by `plan_mount` in `evaluate`.
fn rules_for_advert<'a>(
    rules: &'a [Automount],
    own_host: &str,
    advert: &CapabilityAdvert,
) -> Vec<&'a Automount> {
    if advert.host == own_host {
        return Vec::new();
    }
    rules
        .iter()
        .filter(|r| r.selector.coarse_matches(&advert.cap))
        .collect()
}

/// Discovery-driven auto-mount (§6.1): for a freshly-resolved capmesh peer, issue any matching
/// auto-mount rule. Fetches the peer's descriptor over the mesh endpoint (MESH-PROTOCOL.md),
/// plans + assembles the mount ([`automount::evaluate`]) using the AppleMIDI control port
/// correlated by IP, and issues it against the local data-plane daemon's `capmesh-ctl` socket.
///
/// If the peer's control port is not known yet (its `_apple-midi._udp` record has not resolved),
/// the peer is parked in `pending`; the apple-midi resolve handler re-runs this the moment the
/// port is learned, so a capmesh advert arriving first does not wait for the next mDNS re-resolve.
async fn try_auto_mount(
    ctx: MountCtx,
    advert: CapabilityAdvert,
    addr: IpAddr,
    ep: u16,
    fullname: String,
) {
    let MountCtx {
        auto,
        apple_peers,
        active_mounts,
        pending,
    } = ctx;

    // The rules that apply to this advert: skip our own advert, then coarse-filter on kind
    // (dir/port defer to the descriptor stage). See [`rules_for_advert`].
    let matched = rules_for_advert(&auto.rules, &auto.own_host, &advert);
    if matched.is_empty() {
        return;
    }

    let Some(socket) = auto.sockets.get(&advert.cap) else {
        warn!(kind = %advert.cap, "auto-mount: no local data-plane socket configured for this kind");
        return;
    };

    let cap = match capmesh_mesh::fetch_capability(addr, ep, &advert.descr).await {
        Ok(cap) => cap,
        Err(e) => {
            warn!(%addr, descr = %advert.descr, "auto-mount: descriptor fetch failed: {e:#}");
            return;
        }
    };

    // The AppleMIDI control port for this peer (by IP); may be unknown until its record resolves.
    let control_port = apple_peers.lock().unwrap().control_port(&addr);

    let mut awaiting_control_port = false;
    for rule in matched {
        let remote = Remote {
            mount_id: advert.id.clone(),
            host: advert.host.clone(),
            addr,
            control_port,
            local_name: Some(format!("{} {}", advert.host, advert.cap)),
        };
        match automount::evaluate(
            &rule.action,
            rule.selector.kind.as_deref(),
            rule.selector.dir.as_deref(),
            rule.selector.port.as_deref(),
            &cap.ports,
            &auto.local_codecs,
            remote,
        ) {
            AutoMountOutcome::Mount(spec) => {
                let lifetime = Lifetime::from_config(rule.lifetime.as_deref());
                let mount_id = spec.mount_id.clone();
                info!(mount_id = %mount_id, ?lifetime, "auto-mount: issuing");
                // Track the mount only once it is actually issued, so a `while-advertised`
                // teardown never chases a mount that never landed (§6.1).
                if issue_mount(socket, spec).await {
                    // Decide against advert liveness under the registry lock, so this serializes
                    // with a racing `ServiceRemoved` (§6.1): if the source advert vanished during
                    // the fetch/mount, undo a `while-advertised` mount rather than orphan it.
                    let outcome = {
                        let mut reg = active_mounts.lock().unwrap();
                        let live = reg.live.contains(&fullname);
                        let outcome = automount::resolve_issue_race(live, lifetime);
                        if outcome == automount::RaceOutcome::Track {
                            automount::track_active_mount(
                                reg.active.entry(fullname.clone()).or_default(),
                                ActiveAutoMount {
                                    mount_id: mount_id.clone(),
                                    kind: advert.cap.clone(),
                                    lifetime,
                                },
                            );
                        }
                        outcome
                    };
                    if outcome == automount::RaceOutcome::CompensateUnmount {
                        warn!(mount_id = %mount_id, %fullname,
                            "auto-mount: source advert removed during issuance; unmounting the orphaned mount");
                        issue_unmount(socket, &mount_id).await;
                    }
                }
            }
            AutoMountOutcome::AwaitingControlPort => {
                awaiting_control_port = true;
                info!(
                    %addr,
                    host = %advert.host,
                    "auto-mount: matched, awaiting the peer's _apple-midi._udp record to learn its control port"
                );
            }
            AutoMountOutcome::Skip(e) => {
                debug!(action = %rule.action, "auto-mount: rule does not apply: {e}")
            }
        }
    }

    // Park the peer so the `_apple-midi._udp` resolve re-triggers it the moment the control port
    // is known, rather than waiting for the next capmesh re-resolve (§6.1).
    if awaiting_control_port {
        pending.lock().unwrap().record(
            addr,
            PendingPeer {
                advert,
                ep,
                fullname,
            },
        );
    }
}

/// Issue one mount against a data-plane daemon's `capmesh-ctl` socket. Best-effort: the mount is
/// idempotent on its `mount-id`, so a transient failure is retried by the next resolve tick.
/// Returns `true` if the mount was issued (so the caller can track it for lifetime teardown).
async fn issue_mount(socket: &Path, spec: MountSpec) -> bool {
    let mount_id = spec.mount_id.clone();
    let result = async {
        let mut client = CtlClient::connect(socket).await?;
        client.hello().await?;
        client.mount(&spec).await
    }
    .await;
    match result {
        Ok(r) => {
            info!(mount_id = %r.mount_id, state = ?r.state, "auto-mount issued");
            true
        }
        Err(e) => {
            warn!(%mount_id, socket = %socket.display(), "auto-mount: mount failed: {e:#}");
            false
        }
    }
}

/// Tear one auto-mount down against a data-plane daemon's `capmesh-ctl` socket (§6.1, the
/// `while-advertised` path). Best-effort, mirroring [`issue_mount`]: a transient failure is
/// logged; the reconciler's drift pass is the backstop.
async fn issue_unmount(socket: &Path, mount_id: &str) {
    let result = async {
        let mut client = CtlClient::connect(socket).await?;
        client.hello().await?;
        client.unmount(mount_id).await
    }
    .await;
    match result {
        Ok(()) => info!(%mount_id, "auto-mount torn down (source unadvertised)"),
        Err(e) => warn!(%mount_id, socket = %socket.display(),
            "auto-mount teardown: unmount failed: {e:#}"),
    }
}

/// The mesh control endpoint's capability provider (docs/MESH-PROTOCOL.md §4): one capability
/// per configured socket-based data-plane kind, its ports projected live from the daemon's
/// `list-ports` on each request. capmeshd holds no descriptor state — this is a read projection.
struct DataplaneCaps {
    host_id: String,
    /// `(kind, ctl-socket-path)` for each socket-based advertised kind.
    kinds: Vec<(String, PathBuf)>,
}

impl CapabilityProvider for DataplaneCaps {
    async fn caps(&self) -> Vec<CapabilityDescriptor> {
        let mut caps = Vec::new();
        for (kind, socket) in &self.kinds {
            match caps_for_kind(&self.host_id, kind, socket).await {
                Ok(cap) => caps.push(cap),
                // A daemon that is down (e.g. nmidid not up yet) drops out of the projection
                // for this request; it reappears once the socket answers. Best-effort by design.
                Err(e) => warn!(%kind, "mesh: skipping kind, list-ports failed: {e:#}"),
            }
        }
        caps
    }
}

/// Build one kind's capability descriptor from its data-plane daemon's live `list-ports`
/// (§3): `id = "{host}-{kind}"` to match the advert, ports carried verbatim.
async fn caps_for_kind(
    host_id: &str,
    kind: &str,
    socket: &Path,
) -> Result<CapabilityDescriptor, CtlError> {
    let mut client = CtlClient::connect(socket).await?;
    client.hello().await?;
    let ports = client.list_ports().await?.ports;
    Ok(CapabilityDescriptor {
        id: format!("{host_id}-{kind}"),
        host: host_id.to_string(),
        kind: kind.to_string(),
        // Coarse host-level direction (matches the advert); per-port `dir` lives in `ports`.
        dir: "duplex".to_string(),
        ports,
    })
}

/// Drive a data-plane daemon's `capmesh-ctl` socket: connect, handshake, enumerate ports.
/// This is the `midi` adapter's first integration path against a live `nmidid` (§1.2, §3).
async fn probe_ctl(socket: &Path, watch: bool) -> Result<()> {
    info!(socket = %socket.display(), "connecting to capmesh-ctl socket");
    let mut client = CtlClient::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;

    let hello = client.hello().await.context("hello handshake")?;
    info!(
        daemon = %hello.daemon,
        protocol = %hello.protocol,
        capabilities = ?hello.capabilities,
        "hello ok"
    );

    let ports = client.list_ports().await.context("list-ports")?;
    if ports.ports.is_empty() {
        info!("daemon reported no ports");
    }
    for p in &ports.ports {
        let codecs: Vec<&str> = p.formats.iter().map(|f| f.codec.as_str()).collect();
        info!(
            port_id = %p.port_id,
            kind = %p.kind,
            dir = ?p.dir,
            r#type = %p.type_,
            name = %p.name,
            virtualizable = p.virtualizable,
            formats = ?codecs,
            "port"
        );
    }

    // Round-trip describe-port on the first port to exercise the full descriptor fetch.
    if let Some(first) = ports.ports.first() {
        let d = client
            .describe_port(&first.port_id)
            .await
            .context("describe-port")?;
        info!(port_id = %d.port_id, name = %d.name, "describe-port ok");
    }

    // Listen for unsolicited daemon notifications (§5): mount-state + hotplug events.
    if watch {
        info!("watching for daemon notifications (ctrl-c to stop)");
        loop {
            match client.next_notification().await {
                Ok(n) => info!(notification = ?n, "daemon notification"),
                Err(e) => {
                    warn!("notification stream ended: {e:#}");
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Browse `_capmesh._tcp` for `timeout_secs`, then print the discovered capabilities that
/// match the optional filters (§7 discover). Peers are keyed by the IP from their mDNS
/// record (§5). Deduped by capability id (a peer may resolve more than once).
/// A capability found on the mesh by [`discover_capabilities`]: the coarse advert, where it was
/// resolved (IP + mesh-endpoint port), its fetched typed descriptor (`None` if the peer's mesh
/// endpoint could not be reached), and the descriptor ports that matched the requested `dir`.
#[derive(Debug, Clone)]
struct DiscoveredCapability {
    advert: CapabilityAdvert,
    addr: IpAddr,
    port: u16,
    descriptor: Option<CapabilityDescriptor>,
    ports: Vec<PortDescriptor>,
}

/// The descriptor ports that satisfy an optional `dir` selector (§7). Pulled out of the discover
/// loop as a pure helper so the coarse-advert-vs-descriptor direction split is testable: the
/// coarse `_capmesh._tcp` advert can't carry a usable `dir`, so direction is matched here against
/// the fetched descriptor's real ports, never at the advert stage.
fn matching_ports(cap: &CapabilityDescriptor, dir: Option<&str>) -> Vec<PortDescriptor> {
    cap.ports
        .iter()
        .filter(|p| p.matches_dir(dir))
        .cloned()
        .collect()
}

/// Browse the mesh for up to `timeout_secs` and return the capabilities matching `kind`/`host`
/// (the coarse advert filter) with their fetched descriptors and `dir`-matched ports (§5, §7).
/// Direction can only be confirmed from the descriptor, so a `dir` filter drops a capability
/// whose descriptor exposes no port in that direction — and also one whose descriptor could not be
/// fetched; an unfiltered browse keeps a descriptor-less capability as its coarse advert. This is
/// the shared substrate for the `discover` CLI verb and (M2) the `discover`/`describe` MCP tools.
async fn discover_capabilities(
    timeout_secs: u64,
    kind: Option<&str>,
    dir: Option<&str>,
    host: Option<&str>,
) -> Result<Vec<DiscoveredCapability>> {
    let events = discovery::browse().context("start mDNS browse")?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen: std::collections::BTreeMap<String, (CapabilityAdvert, IpAddr, u16)> =
        std::collections::BTreeMap::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = events.recv_async() => match ev {
                Ok(ServiceEvent::ServiceResolved(svc)) => {
                    // Filter on kind + host at the advert stage only — the coarse advert's `dir`
                    // is always `duplex`, so `dir` is applied against the descriptor's real ports
                    // after the fetch (mirrors the auto-mount coarse/plan split).
                    if let (Some(addr), Ok(advert)) =
                        (discovery::resolved_addr(&svc), discovery::advert_from_resolved(&svc))
                        && advert.matches(kind, None, host)
                    {
                        seen.insert(advert.id.clone(), (advert, addr, svc.get_port()));
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
        }
    }

    let mut out = Vec::new();
    for (advert, addr, port) in seen.into_values() {
        // Resolve the coarse advert's `descr` pointer into the full typed descriptor over the
        // peer's mesh control endpoint (docs/MESH-PROTOCOL.md). A peer that doesn't serve the
        // endpoint (or is momentarily down) still surfaces as the coarse advert.
        match capmesh_mesh::fetch_capability(addr, port, &advert.descr).await {
            Ok(cap) => {
                let ports = matching_ports(&cap, dir);
                if dir.is_some() && ports.is_empty() {
                    continue;
                }
                out.push(DiscoveredCapability {
                    advert,
                    addr,
                    port,
                    descriptor: Some(cap),
                    ports,
                });
            }
            Err(e) if dir.is_none() => {
                debug!(id = %advert.id, descr = %advert.descr, "descriptor unavailable: {e:#}");
                out.push(DiscoveredCapability {
                    advert,
                    addr,
                    port,
                    descriptor: None,
                    ports: Vec::new(),
                });
            }
            // Without the descriptor a `dir` match cannot be confirmed, so a dir-filtered browse
            // skips it.
            Err(e) => debug!(
                id = %advert.id,
                "skipping (descriptor unavailable, cannot confirm dir match): {e:#}"
            ),
        }
    }
    Ok(out)
}

async fn cmd_discover(
    timeout_secs: u64,
    kind: Option<&str>,
    dir: Option<&str>,
    host: Option<&str>,
) -> Result<()> {
    info!(timeout_secs, "discovering capmesh capabilities");
    let found = discover_capabilities(timeout_secs, kind, dir, host).await?;
    if found.is_empty() {
        info!("no capmesh capabilities discovered");
    }
    for d in &found {
        if d.descriptor.is_some() {
            info!(
                host = %d.advert.host,
                cap = %d.advert.cap,
                id = %d.advert.id,
                addr = %d.addr,
                port = d.port,
                ports = d.ports.len(),
                "capability"
            );
            for p in &d.ports {
                let codecs = p
                    .formats
                    .iter()
                    .map(|f| f.codec.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                info!(
                    id = %d.advert.id,
                    port_id = %p.port_id,
                    dir = p.dir.as_deref().unwrap_or("-"),
                    r#type = %p.type_,
                    name = %p.name,
                    codecs = %codecs,
                    "  port"
                );
            }
        } else {
            info!(
                host = %d.advert.host,
                cap = %d.advert.cap,
                dir = %d.advert.dir,
                id = %d.advert.id,
                addr = %d.addr,
                port = d.port,
                descr = %d.advert.descr,
                "capability (descriptor unavailable)"
            );
        }
    }
    Ok(())
}

/// Build the `cap=mcp` advert for capmesh's own control-tools MCP server (DESIGN §7.1), so peers and
/// the gateway discover it. The endpoint port is parsed from `serve_addr` (`host:port`); the
/// descriptor path is the MCP endpoint `/mcp`. Pure — the mDNS publish is the caller's job.
fn mcp_capability_advert(host_id: &str, serve_addr: &str) -> Result<CapabilityAdvert> {
    let addr: SocketAddr = serve_addr
        .parse()
        .with_context(|| format!("mcp-serve-addr `{serve_addr}` is not a host:port socket address"))?;
    let id = format!("{host_id}-mcp");
    Ok(CapabilityAdvert {
        cap: "mcp".to_string(),
        dir: "control".to_string(),
        id: id.clone(),
        host: host_id.to_string(),
        ep: addr.port(),
        descr: "/mcp".to_string(),
    })
}

/// Build the registry [`McpRoute`] for a discovered `cap=mcp` peer (DESIGN §7.2 auto-registration).
/// The gateway reaches it over `streamable-http` at `http://<addr>:<ep><descr>` — the advert's
/// resolved IP + control-endpoint port + descriptor pointer (which for a `cap=mcp` advert is the
/// server's `/mcp` path; see [`mcp_capability_advert`]). The route id is the advert's stable
/// capability id, so its tools land under a stable `<id>__` namespace on the gateway. The protocol
/// revision is unknown from the coarse advert (negotiated when the gateway connects) → `None`.
fn mcp_route_from_advert(advert: &CapabilityAdvert, addr: IpAddr) -> McpRoute {
    McpRoute {
        id: advert.id.clone(),
        url: format!("http://{addr}:{}{}", advert.ep, advert.descr),
        transport: McpTransport::StreamableHttp,
        protocol_rev: None,
    }
}

fn parse_role(s: &str) -> Result<MountRole> {
    match s {
        "mirror-source" => Ok(MountRole::MirrorSource),
        "mirror-sink" => Ok(MountRole::MirrorSink),
        "link" => Ok(MountRole::Link),
        other => anyhow::bail!("unknown role `{other}` (mirror-source | mirror-sink | link)"),
    }
}

/// Build a `MountSpec` (§3.1) from a config permanent-mount (§9).
fn permanent_to_spec(pm: &config::PermanentMount) -> Result<MountSpec> {
    let role = parse_role(&pm.role)?;
    let host = pm
        .remote
        .host
        .clone()
        .unwrap_or_else(|| pm.remote.addr.to_string());
    let mount_id = pm
        .mount_id
        .clone()
        .unwrap_or_else(|| format!("{host}-{}", pm.remote.port_id));
    Ok(MountSpec {
        mount_id,
        role,
        local: LocalEndpoint {
            is_virtual: !matches!(role, MountRole::Link),
            name: pm.local_name.clone(),
            // A `link` binds the configured local real port (§3.1 `local.port-id`); mirror roles
            // create a virtual endpoint and ignore it.
            port_id: match role {
                MountRole::Link => pm.local_port_id.clone(),
                MountRole::MirrorSource | MountRole::MirrorSink => None,
            },
        },
        remote: RemoteEndpoint {
            host,
            addr: pm.remote.addr,
            port: pm.remote.port,
            port_id: pm.remote.port_id.clone(),
        },
        format: Format {
            codec: pm.codec.clone(),
            params: Default::default(),
        },
    })
}

/// Build the MCP route registry (DESIGN §7.2) from the config's statically-declared `[[mcp-route]]`
/// entries. Each is validated through [`McpRouteRegistry::register`]; an invalid entry (e.g. a bad
/// id) is logged and skipped rather than failing daemon startup. The runtime register endpoint and
/// `cap=mcp` auto-discovery mutate this same registry, and the gateway is driven off it.
fn build_mcp_route_registry(cfg: &Config) -> McpRouteRegistry {
    let mut registry = McpRouteRegistry::new();
    for rc in &cfg.mcp_routes {
        match registry.register(rc.to_route()) {
            Ok(_) => info!(id = %rc.id, url = %rc.url, ?rc.transport, "configured mcp route"),
            Err(e) => warn!(id = %rc.id, "skipping invalid configured mcp route: {e}"),
        }
    }
    registry
}

/// The MCP route registry shared between the control endpoint and (later) the gateway-driving loop.
type SharedMcpRoutes = Arc<Mutex<McpRouteRegistry>>;

/// A driver that pushes route changes to the running MCP gateway's control channel.
#[derive(Clone)]
struct GatewayDriver {
    client: GatewayClient,
    base_url: String,
}

/// The MCP route control-endpoint state: the shared registry + an optional gateway driver (present
/// when `gateway-admin-url` is configured). On a route change, capmeshd drives the running gateway
/// to (de)federate — so a `register`/`unregister` (manual, or `cap=mcp` auto-discovery) takes effect
/// live.
#[derive(Clone)]
struct McpControlState {
    routes: SharedMcpRoutes,
    gateway: Option<GatewayDriver>,
}

/// The axum router for the MCP route control endpoint (DESIGN §7.2 `register`): `GET /mcp/routes`
/// lists the registry, `POST /mcp/routes` registers (upserts) a route from an [`McpRoute`] body,
/// `DELETE /mcp/routes/{id}` unregisters one. A change also drives the gateway (when configured).
fn mcp_control_router(state: McpControlState) -> Router {
    Router::new()
        .route("/mcp/routes", get(list_mcp_routes).post(register_mcp_route))
        .route("/mcp/routes/{id}", delete(unregister_mcp_route))
        .with_state(state)
}

/// Render a registry [`RouteResponse`] as an HTTP response.
fn render_route_response(resp: RouteResponse) -> impl IntoResponse {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(resp.body))
}

/// Drive the gateway with `cmd` (fire-and-forget): a down/unreachable gateway is logged, not fatal —
/// the registry stays authoritative and capmeshd re-drives on the next change. No-op when no gateway
/// is configured or the command is a [`GatewayCommand::Noop`].
fn drive_gateway(gateway: &Option<GatewayDriver>, cmd: GatewayCommand) {
    let Some(gw) = gateway else { return };
    if matches!(cmd, GatewayCommand::Noop) {
        return;
    }
    let client = gw.client.clone();
    let base_url = gw.base_url.clone();
    tokio::spawn(async move {
        if let Err(e) = gateway_driver::send_command(&client, &base_url, &cmd).await {
            warn!("driving gateway on route change failed: {e}");
        }
    });
}

/// Auto-register a discovered `cap=mcp` peer (DESIGN §7.2) into the shared route registry and drive
/// the running gateway to federate it — the discovery-driven analogue of the manual `register`
/// endpoint. Idempotent via the registry: a re-resolve of an already-registered peer with identical
/// coordinates is an `Unchanged` no-op (no gateway churn). An invalid advert id is logged, not fatal.
/// Unlike MIDI auto-mount, this does *not* skip our own advert: federating capmesh's own control
/// server through the gateway is exactly what §7.2 wants (agents reach capmesh's tools via the
/// gateway namespace).
fn auto_register_mcp(
    routes: &SharedMcpRoutes,
    gateway: &Option<GatewayDriver>,
    advert: &CapabilityAdvert,
    addr: IpAddr,
) {
    let route = mcp_route_from_advert(advert, addr);
    let outcome = routes.lock().unwrap().register(route.clone());
    match outcome {
        Ok(RegisterOutcome::Unchanged) => {
            debug!(id = %route.id, "cap=mcp re-resolved; route unchanged");
        }
        Ok(oc) => {
            info!(id = %route.id, url = %route.url, "cap=mcp discovered → registered; federating on gateway");
            drive_gateway(gateway, gateway_driver::on_register(&route, oc));
        }
        Err(e) => warn!(id = %route.id, "cap=mcp discovered but route rejected: {e}"),
    }
}

/// Auto-unregister a removed `cap=mcp` peer (DESIGN §7.2 teardown): drop it from the shared route
/// registry and drive the running gateway to defederate — the symmetric counterpart of
/// [`auto_register_mcp`], and the MCP analogue of a `while-advertised` auto-mount teardown (§6.1).
/// Only drives the gateway when the route was actually present (a peer removed after a manual
/// `unregister` is already gone → nothing to do).
fn auto_unregister_mcp(routes: &SharedMcpRoutes, gateway: &Option<GatewayDriver>, route_id: &str) {
    if routes.lock().unwrap().unregister(route_id).is_some() {
        info!(id = %route_id, "cap=mcp peer removed → unregistered; defederating on gateway");
        drive_gateway(gateway, gateway_driver::on_unregister(route_id));
    } else {
        debug!(id = %route_id, "cap=mcp peer removed but route already gone; nothing to defederate");
    }
}

async fn list_mcp_routes(State(state): State<McpControlState>) -> impl IntoResponse {
    let resp = state.routes.lock().unwrap().handle(RouteRequest::List);
    render_route_response(resp)
}

async fn register_mcp_route(
    State(state): State<McpControlState>,
    Json(route): Json<McpRoute>,
) -> impl IntoResponse {
    let route_for_gw = route.clone();
    let resp = state.routes.lock().unwrap().handle(RouteRequest::Register(route));
    // A registry change (Added/Updated — never Unchanged) federates the upstream on the gateway.
    if resp.status == 200 && resp.changed {
        drive_gateway(
            &state.gateway,
            gateway_driver::on_register(&route_for_gw, RegisterOutcome::Added),
        );
    }
    render_route_response(resp)
}

async fn unregister_mcp_route(
    State(state): State<McpControlState>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let resp = state
        .routes
        .lock()
        .unwrap()
        .handle(RouteRequest::Unregister(id.clone()));
    if resp.status == 200 && resp.changed {
        drive_gateway(&state.gateway, gateway_driver::on_unregister(&id));
    }
    render_route_response(resp)
}

/// Reconcile the config's permanent mounts (DESIGN §9) against the local MIDI data-plane
/// daemon named by `[dataplane.midi].socket`. Best-effort at startup: an unreachable daemon
/// or an invalid mount is logged, not fatal — the daemon keeps advertising/browsing.
async fn reconcile_permanent_mounts(cfg: &Config) {
    if cfg.permanent_mounts.is_empty() {
        return;
    }
    let Some(socket) = cfg.dataplane.get("midi").and_then(|d| d.socket.clone()) else {
        warn!("permanent-mounts configured but no [dataplane.midi] socket; skipping reconcile");
        return;
    };
    let specs: Vec<MountSpec> = match cfg.permanent_mounts.iter().map(permanent_to_spec).collect() {
        Ok(specs) => specs,
        Err(e) => {
            warn!("invalid permanent-mount: {e:#}");
            return;
        }
    };
    let reconciler = Reconciler::with_desired(specs);
    match connect_and_hello(&socket).await {
        Ok(mut client) => match reconciler.reconcile_once(&mut client).await {
            Ok(plan) if plan.is_empty() => info!("permanent-mounts: converged (no changes)"),
            Ok(plan) => info!(
                mounted = plan.to_mount.len(),
                unmounted = plan.to_unmount.len(),
                "permanent-mounts: reconciled"
            ),
            Err(e) => warn!("permanent-mount reconcile failed: {e:#}"),
        },
        Err(e) => warn!(
            socket = %socket.display(),
            "permanent-mount reconcile skipped (data-plane daemon unreachable): {e:#}"
        ),
    }
}

/// Connect + hello, then establish a mount (§3). This is the one-command path toward the
/// M0 demo: wire a remote port into a local virtual endpoint.
async fn cmd_mount(m: &MountArgs) -> Result<()> {
    let spec = m.to_spec()?;
    let mut client = connect_and_hello(&m.socket).await?;
    let res = client.mount(&spec).await.context("mount")?;
    info!(mount_id = %res.mount_id, state = ?res.state, "mount established");
    Ok(())
}

/// Codec names → `Format`s, defaulting to `[midi1]` when none are given.
fn codecs_to_formats(codecs: &[String]) -> Vec<Format> {
    let names = if codecs.is_empty() {
        &["midi1".to_string()][..]
    } else {
        codecs
    };
    names
        .iter()
        .map(|c| Format {
            codec: c.clone(),
            params: Default::default(),
        })
        .collect()
}

/// Connect a remote source to a local mount, negotiating the format first (§4 → §7). The
/// local side is the consumer, so its preference order ranks; on no common format the
/// connect is refused with both sides listed (§4.1 step 4).
#[allow(clippy::too_many_arguments)]
async fn cmd_connect(
    socket: &Path,
    remote_host: &str,
    remote_addr: IpAddr,
    remote_port: u16,
    remote_port_id: &str,
    role: &str,
    local_name: Option<String>,
    local_codecs: &[String],
    remote_codecs: &[String],
    mount_id: Option<String>,
) -> Result<()> {
    let role = parse_role(role)?;
    let local = codecs_to_formats(local_codecs);
    let remote = codecs_to_formats(remote_codecs);
    let format = negotiate::negotiate(&local, &remote).map_err(|nc| {
        let names = |fs: &[Format]| fs.iter().map(|f| f.codec.clone()).collect::<Vec<_>>();
        anyhow::anyhow!(
            "no common format (§4): local {:?}, remote {:?}",
            names(&nc.consumer),
            names(&nc.producer)
        )
    })?;
    info!(codec = %format.codec, "negotiated format");

    let mount_id = mount_id.unwrap_or_else(|| format!("{remote_host}-{remote_port_id}"));
    let spec = MountSpec {
        mount_id,
        role,
        local: LocalEndpoint {
            is_virtual: !matches!(role, MountRole::Link),
            name: local_name,
            // Discovery-driven connect does not yet name a local real port for `link` (follow-on).
            port_id: None,
        },
        remote: RemoteEndpoint {
            host: remote_host.to_string(),
            addr: remote_addr,
            port: remote_port,
            port_id: remote_port_id.to_string(),
        },
        format,
    };

    let mut client = connect_and_hello(socket).await?;
    let res = client.mount(&spec).await.context("mount")?;
    info!(mount_id = %res.mount_id, state = ?res.state, "connected");
    Ok(())
}

/// Browse the mesh for `timeout_secs`, collecting resolved capmesh adverts (deduped by id, with
/// their address + control endpoint) and the AppleMIDI control ports seen, correlated by IP (§5).
/// The discovery-driven connect uses this to learn a target's coordinates from mDNS alone.
async fn discover_targets(
    timeout_secs: u64,
) -> Result<(Vec<(CapabilityAdvert, IpAddr, u16)>, AppleMidiPeers)> {
    let events = discovery::browse().context("start capmesh mDNS browse")?;
    let apple_events = discovery::browse_apple_midi().context("start AppleMIDI browse")?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen: std::collections::BTreeMap<String, (CapabilityAdvert, IpAddr, u16)> =
        std::collections::BTreeMap::new();
    let mut apple = AppleMidiPeers::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = events.recv_async() => match ev {
                Ok(ServiceEvent::ServiceResolved(svc)) => {
                    if let (Some(addr), Ok(advert)) =
                        (discovery::resolved_addr(&svc), discovery::advert_from_resolved(&svc))
                    {
                        seen.insert(advert.id.clone(), (advert, addr, svc.get_port()));
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
            aev = apple_events.recv_async() => match aev {
                Ok(ServiceEvent::ServiceResolved(svc)) => {
                    if let Some(addr) = discovery::resolved_addr(&svc) {
                        apple.observe(addr, svc.get_port());
                    }
                }
                Ok(_) => {}
                Err(_) => {}
            },
        }
    }
    Ok((seen.into_values().collect(), apple))
}

/// Discover a capability by selector and connect it in one command (§6.1/§7). Browses the mesh,
/// [`select_one`](discovery::select_one)s the single matching capability (erroring on none or
/// ambiguity rather than guessing), fetches its descriptor, plans the mount against the local
/// codecs, correlates the AppleMIDI control port by IP, and issues the mount — the manual
/// `connect` with every remote coordinate learned from discovery.
#[allow(clippy::too_many_arguments)]
async fn cmd_connect_discover(
    socket: &Path,
    kind: Option<&str>,
    host: Option<&str>,
    id: Option<&str>,
    dir: Option<&str>,
    port: Option<&str>,
    role: &str,
    local_name: Option<String>,
    local_codecs: &[String],
    timeout_secs: u64,
    mount_id: Option<String>,
) -> Result<()> {
    info!(timeout_secs, ?kind, ?host, ?id, "discovering a capability to connect");
    let (targets, apple) = discover_targets(timeout_secs).await?;
    let adverts: Vec<CapabilityAdvert> = targets.iter().map(|(a, _, _)| a.clone()).collect();
    let chosen = discovery::select_one(&adverts, kind, None, host, id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (advert, addr, ep) = targets
        .iter()
        .find(|(a, _, _)| a.id == chosen.id)
        .expect("the chosen advert came from the discovered targets");
    info!(host = %advert.host, cap = %advert.cap, id = %advert.id, %addr, ep,
        "selected capability");

    let cap = capmesh_mesh::fetch_capability(*addr, *ep, &advert.descr)
        .await
        .with_context(|| format!("fetch descriptor for capability {}", advert.id))?;

    // Bind the port direction the role implies unless the caller pinned `--dir`, so a device that
    // exposes both a source and a sink port mounts the one the role means (§3.1).
    let dir = dir.or_else(|| automount::dir_for_role(role));
    let local = codecs_to_formats(local_codecs);
    let plan = automount::plan_mount(role, kind, dir, port, &cap.ports, &local)
        .map_err(|e| anyhow::anyhow!("cannot plan a mount for {}: {e}", advert.id))?;
    info!(port_id = %plan.port_id, codec = %plan.format.codec, "planned mount");

    let control_port = apple.control_port(addr).ok_or_else(|| {
        anyhow::anyhow!(
            "no _apple-midi._udp record seen for {addr} within {timeout_secs}s; \
             the peer's data-plane control port is unknown (try a longer --timeout-secs)"
        )
    })?;

    let mount_id = mount_id.unwrap_or_else(|| advert.id.clone());
    let spec = automount::build_mount_spec(
        plan,
        mount_id,
        advert.host.clone(),
        *addr,
        control_port,
        local_name,
        // connect-discover does not yet name a local real port for `link` (follow-on).
        None,
    );

    let mut client = connect_and_hello(socket).await?;
    let res = client.mount(&spec).await.context("mount")?;
    info!(mount_id = %res.mount_id, state = ?res.state, "connected (discovered)");
    Ok(())
}

/// Reconcile a single desired mount against a daemon (DESIGN §6). Modes: reconcile once and
/// exit (default); `--interval-secs N` re-reconciles every N seconds; `--watch` reacts to
/// daemon notifications (§5) and re-reconciles on drift, event-driven with no polling.
async fn cmd_reconcile(m: &MountArgs, interval_secs: u64, watch: bool) -> Result<()> {
    let reconciler = Reconciler::with_desired(vec![m.to_spec()?]);
    let mut client = connect_and_hello(&m.socket).await?;

    reconcile_pass(&reconciler, &mut client).await;

    if watch {
        info!("reconcile: watching daemon notifications (event-driven; ctrl-c to stop)");
        loop {
            match client.next_notification().await {
                Ok(n) if reconcile::wants_reconcile(&n) => {
                    info!(notification = ?n, "reconcile: drift — re-reconciling");
                    reconcile_pass(&reconciler, &mut client).await;
                }
                Ok(n) => info!(notification = ?n, "reconcile: notification (no action)"),
                Err(e) => {
                    warn!("notification stream ended: {e:#}");
                    break;
                }
            }
        }
    } else if interval_secs > 0 {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
            reconcile_pass(&reconciler, &mut client).await;
        }
    }
    Ok(())
}

/// One reconcile pass with logging; a failed pass is logged, not fatal (the loop retries).
async fn reconcile_pass(reconciler: &Reconciler, client: &mut CtlClient) {
    match reconciler.reconcile_once(client).await {
        Ok(plan) if plan.is_empty() => info!("reconcile: converged (no changes)"),
        Ok(plan) => info!(
            mounted = plan.to_mount.len(),
            unmounted = plan.to_unmount.len(),
            "reconcile: applied"
        ),
        Err(e) => warn!("reconcile pass failed: {e:#}"),
    }
}

/// Connect + hello, then tear a mount down (§3).
async fn cmd_unmount(socket: &Path, mount_id: &str) -> Result<()> {
    let mut client = connect_and_hello(socket).await?;
    client.unmount(mount_id).await.context("unmount")?;
    info!(%mount_id, "unmounted");
    Ok(())
}

/// Connect + hello, then print live mounts (§3). With `json`, emit the result as machine-readable
/// JSON on stdout (stable kebab-case wire names) for scripts/agents/the rehearsal harness;
/// otherwise emit human log lines.
async fn cmd_mount_status(socket: &Path, mount_id: Option<&str>, json: bool) -> Result<()> {
    let mut client = connect_and_hello(socket).await?;
    let res = client
        .mount_status(mount_id)
        .await
        .context("mount-status")?;
    if json {
        println!("{}", serde_json::to_string(&res).context("serialize mount-status")?);
        return Ok(());
    }
    if res.mounts.is_empty() {
        info!("no live mounts");
    }
    for m in &res.mounts {
        let (bytes_in, bytes_out, last_event) = match &m.stats {
            Some(s) => (s.bytes_in, s.bytes_out, s.last_event.clone()),
            None => (0, 0, None),
        };
        info!(
            mount_id = %m.mount_id,
            state = ?m.state,
            since = ?m.since,
            bytes_in,
            bytes_out,
            last_event = ?last_event,
            detail = ?m.detail,
            "mount"
        );
    }
    Ok(())
}

/// Open a `capmesh-ctl` connection and complete the mandatory `hello` handshake (§1.2).
async fn connect_and_hello(socket: &Path) -> Result<CtlClient> {
    let mut client = CtlClient::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    let hello = client.hello().await.context("hello handshake")?;
    info!(daemon = %hello.daemon, protocol = %hello.protocol, "hello ok");
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_advert_carries_the_serve_port_and_mcp_path() {
        let a = mcp_capability_advert("sc-host", "127.0.0.1:8092").unwrap();
        assert_eq!(a.cap, "mcp");
        assert_eq!(a.dir, "control");
        assert_eq!(a.id, "sc-host-mcp");
        assert_eq!(a.host, "sc-host");
        assert_eq!(a.ep, 8092);
        assert_eq!(a.descr, "/mcp");
    }

    #[test]
    fn mcp_advert_rejects_a_non_socket_addr() {
        assert!(mcp_capability_advert("h", "not-a-socket").is_err());
    }

    fn rule(kind: Option<&str>) -> Automount {
        Automount {
            selector: config::MatchSelector {
                kind: kind.map(String::from),
                port: None,
                dir: Some("source".into()),
            },
            action: "mirror-local".into(),
            lifetime: Some("while-advertised".into()),
        }
    }

    fn advert(host: &str, cap: &str) -> CapabilityAdvert {
        CapabilityAdvert {
            cap: cap.into(),
            dir: "duplex".into(),
            id: format!("{host}-{cap}"),
            host: host.into(),
            ep: 7420,
            descr: format!("/caps/{host}-{cap}"),
        }
    }

    #[test]
    fn matching_ports_applies_the_dir_selector_against_the_descriptor() {
        // A descriptor with one source port and one sink port. `dir` is matched here (the coarse
        // advert can't carry it), so the filter picks the requested direction; `None` keeps all.
        let cap: CapabilityDescriptor = serde_json::from_str(
            r#"{"id":"h-midi","host":"h","kind":"midi","dir":"duplex","ports":[
                {"port-id":"kbd-0","kind":"stream","dir":"source","type":"midi","name":"kbd"},
                {"port-id":"spk-0","kind":"stream","dir":"sink","type":"midi","name":"spk"}
            ]}"#,
        )
        .unwrap();

        let all = matching_ports(&cap, None);
        assert_eq!(all.len(), 2);

        let sources = matching_ports(&cap, Some("source"));
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].port_id, "kbd-0");

        let sinks = matching_ports(&cap, Some("sink"));
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].port_id, "spk-0");

        // A direction the descriptor doesn't expose yields nothing (→ discover drops it).
        assert!(matching_ports(&cap, Some("duplex")).is_empty());
    }

    #[tokio::test]
    async fn mcp_control_endpoint_register_list_unregister() {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt; // for `oneshot`

        let routes: SharedMcpRoutes = Arc::new(Mutex::new(McpRouteRegistry::new()));
        // No gateway configured in this test — the registry behavior is what's under test.
        let router = mcp_control_router(McpControlState { routes, gateway: None });

        // Run one request against a fresh clone of the (shared-state) router.
        async fn call(
            router: &Router,
            method: &str,
            path: &str,
            body: Option<serde_json::Value>,
        ) -> (u16, serde_json::Value) {
            let builder = Request::builder().method(method).uri(path);
            let req = match body {
                Some(b) => builder
                    .header("content-type", "application/json")
                    .body(Body::from(b.to_string()))
                    .unwrap(),
                None => builder.body(Body::empty()).unwrap(),
            };
            let resp = router.clone().oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }

        let board = serde_json::json!({
            "id": "board", "url": "http://h/board/mcp", "transport": "streamable-http"
        });

        // POST registers → 200 added.
        let (s, b) = call(&router, "POST", "/mcp/routes", Some(board.clone())).await;
        assert_eq!(s, 200);
        assert_eq!(b["outcome"], "added");

        // GET lists it.
        let (s, b) = call(&router, "GET", "/mcp/routes", None).await;
        assert_eq!(s, 200);
        assert_eq!(b["routes"][0]["id"], "board");

        // Idempotent re-POST → 200 unchanged.
        let (_, b) = call(&router, "POST", "/mcp/routes", Some(board)).await;
        assert_eq!(b["outcome"], "unchanged");

        // Invalid id → 400.
        let bad = serde_json::json!({"id":"bad_id","url":"http://h/mcp","transport":"stdio"});
        let (s, _) = call(&router, "POST", "/mcp/routes", Some(bad)).await;
        assert_eq!(s, 400);

        // DELETE existing → 200; DELETE again → 404.
        let (s, _) = call(&router, "DELETE", "/mcp/routes/board", None).await;
        assert_eq!(s, 200);
        let (s, _) = call(&router, "DELETE", "/mcp/routes/board", None).await;
        assert_eq!(s, 404);
    }

    #[test]
    fn permanent_to_spec_link_carries_the_local_port_id() {
        // A `link` permanent mount → a non-virtual MountSpec whose local end names the configured
        // local real port (§3.1 `local.port-id`); the daemon resolves direction from its own port.
        let cfg = config::Config::parse(
            r#"
host-id = "studio-host"

[[permanent-mount]]
role = "link"
local-port-id = "sink-supercollider"
remote = { addr = "192.168.1.23", port = 5004, port-id = "kbd-0" }
"#,
        )
        .expect("parse");
        let spec = permanent_to_spec(&cfg.permanent_mounts[0]).expect("spec");
        assert_eq!(spec.role, MountRole::Link);
        assert!(!spec.local.is_virtual);
        assert_eq!(spec.local.port_id.as_deref(), Some("sink-supercollider"));
    }

    #[test]
    fn permanent_to_spec_mirror_omits_the_local_port_id() {
        // A mirror mount never binds a local real port, even if `local-port-id` is (wrongly) set.
        let cfg = config::Config::parse(
            r#"
host-id = "music-host"

[[permanent-mount]]
role = "mirror-source"
local-port-id = "ignored"
remote = { addr = "192.168.1.23", port = 5004, port-id = "kbd-0" }
"#,
        )
        .expect("parse");
        let spec = permanent_to_spec(&cfg.permanent_mounts[0]).expect("spec");
        assert!(spec.local.is_virtual);
        assert_eq!(spec.local.port_id, None);
    }

    #[test]
    fn build_mcp_route_registry_seeds_valid_and_skips_invalid() {
        // Two well-formed routes and one with an invalid id (underscore → would break the gateway's
        // `<id>__<tool>` namespacing); the invalid one is skipped, not fatal.
        let cfg = config::Config::parse(
            r#"
host-id = "h"

[[mcp-route]]
id = "board"
url = "http://h/board/mcp"
transport = "streamable-http"

[[mcp-route]]
id = "bad_id"
url = "http://h/bad/mcp"
transport = "streamable-http"

[[mcp-route]]
id = "kb"
url = "http://h/kb/mcp"
transport = "streamable-http"
"#,
        )
        .expect("parse");
        let reg = build_mcp_route_registry(&cfg);
        // Only the two valid ids landed, in stable order.
        let ids: Vec<&str> = reg.routes().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["board", "kb"]);
        assert!(reg.get("bad_id").is_none());
    }

    #[test]
    fn mcp_route_from_advert_builds_a_streamable_http_url() {
        // A discovered `cap=mcp` advert → a streamable-http route the gateway reaches at
        // http://<resolved-ip>:<ep><descr>, keyed by the advert's stable capability id.
        let advert = CapabilityAdvert {
            cap: "mcp".to_string(),
            dir: "control".to_string(),
            id: "green-machine-mcp".to_string(),
            host: "green-machine".to_string(),
            ep: 8081,
            descr: "/mcp".to_string(),
        };
        let route = mcp_route_from_advert(&advert, "192.168.1.42".parse().unwrap());
        assert_eq!(route.id, "green-machine-mcp");
        assert_eq!(route.url, "http://192.168.1.42:8081/mcp");
        assert_eq!(route.transport, McpTransport::StreamableHttp);
        // The protocol rev is unknown from the coarse advert — negotiated when the gateway connects.
        assert_eq!(route.protocol_rev, None);
    }

    #[test]
    fn rules_for_advert_skips_our_own_advert() {
        let rules = vec![rule(Some("midi"))];
        // A node must not auto-mount the capability it itself advertises, even though the rule's
        // kind matches — it would discover its own `_capmesh._tcp` record over multicast.
        assert!(rules_for_advert(&rules, "sc-host", &advert("sc-host", "midi")).is_empty());
    }

    #[test]
    fn rules_for_advert_matches_a_foreign_peer_by_kind() {
        let rules = vec![rule(Some("midi"))];
        // A foreign peer whose kind matches → the rule applies (dir/port defer to the descriptor).
        assert_eq!(
            rules_for_advert(&rules, "sc-host", &advert("source-host", "midi")).len(),
            1
        );
        // Wrong kind → no rule applies, even for a foreign peer.
        assert!(rules_for_advert(&rules, "sc-host", &advert("source-host", "audio")).is_empty());
    }

    /// `caps_for_kind` builds a capability descriptor from a data-plane daemon's live
    /// `list-ports` (the mesh endpoint's per-request projection). Exercised against a fake
    /// `capmesh-ctl` server over a Unix socket — no real MIDI hardware/sequencer needed.
    #[tokio::test]
    async fn caps_for_kind_projects_list_ports() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "capmesh-mesh-provider-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();

            // hello
            let req = lines.next_line().await.unwrap().unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "hello");
            w.write_all(
                format!(
                    "{}\n",
                    serde_json::json!({"jsonrpc":"2.0","id":v["id"],
                        "result":{"protocol":"1","daemon":"nmidid/0.1","capabilities":["midi1"]}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();

            // list-ports
            let req = lines.next_line().await.unwrap().unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "list-ports");
            w.write_all(
                format!(
                    "{}\n",
                    serde_json::json!({"jsonrpc":"2.0","id":v["id"],"result":{"ports":[
                        {"port-id":"kbd-0","kind":"stream","dir":"source","type":"midi",
                         "name":"Keystation 49e","virtualizable":true,
                         "formats":[{"codec":"midi1"}]}]}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        });

        let cap = caps_for_kind("green-machine", "midi", &path).await.unwrap();
        assert_eq!(cap.id, "green-machine-midi");
        assert_eq!(cap.host, "green-machine");
        assert_eq!(cap.kind, "midi");
        assert_eq!(cap.dir, "duplex");
        assert_eq!(cap.ports.len(), 1);
        assert_eq!(cap.ports[0].port_id, "kbd-0");
        assert_eq!(cap.ports[0].formats[0].codec, "midi1");

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn caps_for_kind_errors_when_socket_is_absent() {
        // A down daemon → Err, which the provider turns into "skip this kind" (best-effort).
        let missing = std::env::temp_dir().join("capmesh-mesh-provider-does-not-exist.sock");
        let _ = std::fs::remove_file(&missing);
        assert!(caps_for_kind("h", "midi", &missing).await.is_err());
    }
}
