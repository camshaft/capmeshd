//! capmeshd — the LAN capability mesh control-plane daemon (DESIGN.md).
//!
//! M0b slice: stateless control plane, MIDI-first. The default invocation runs the
//! daemon: it loads the §4.1 TOML config, advertises this host's configured capability
//! kinds over `_capmesh._tcp`, and browses the mesh — logging discovered peers by the IP
//! from their mDNS record (§5). The `probe-ctl` subcommand drives a data-plane daemon's
//! `capmesh-ctl` socket ([`ctl`]) directly (hello + list-ports), the first integration
//! point of the `midi` adapter against a live `nmidid`. The desired-mount reconciler and
//! the control API land in following slices.

mod config;

use anyhow::{Context, Result};
use capmesh_ctl::{CtlClient, CtlError, Format, LocalEndpoint, MountRole, MountSpec, RemoteEndpoint};
use capmesh_daemon::negotiate;
use capmesh_daemon::reconcile::{self, Reconciler};
use capmesh_discovery as discovery;
use capmesh_mesh::server::CapabilityProvider;
use capmesh_model::CapabilityDescriptor;
use clap::{Parser, Subcommand};
use config::Config;
use discovery::{CapabilityAdvert, ServiceAdvertiser};
use mdns_sd::ServiceEvent;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

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
        Some(Cmd::Mount(m)) => return cmd_mount(m).await,
        Some(Cmd::Unmount { socket, mount_id }) => return cmd_unmount(socket, mount_id).await,
        Some(Cmd::MountStatus { socket, mount_id }) => {
            return cmd_mount_status(socket, mount_id.as_deref()).await;
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
    if adverts.is_empty() {
        info!("no data-plane kinds configured; browsing only");
    }

    // Reconcile permanent mounts (DESIGN §9) against the local MIDI data-plane daemon at
    // startup. Best-effort: if the daemon socket is down (e.g. nmidid not up yet), log and
    // carry on — a later tick's reconcile / the daemon coming up converges it.
    reconcile_permanent_mounts(&cfg).await;

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
                                        note_automount_candidates(&cfg, &advert, addr);
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

/// Match a freshly-resolved peer advert against the config's auto-mount selectors (§6.1) and
/// log each candidate. This is the discovery-driven half of auto-mount: the coarse advert
/// carries `kind`/`dir` (matched here), but not the per-port descriptor. Issuing the derived
/// mount needs the remote port's data-plane port, port-id, and formats — the mesh descriptor
/// fetch that lands in a later slice — so a matched candidate is surfaced, not yet mounted.
/// A selector that constrains `port` is intentionally skipped at this stage (no port-id yet).
fn note_automount_candidates(cfg: &Config, advert: &CapabilityAdvert, addr: IpAddr) {
    for am in &cfg.automount {
        if am.selector.matches(&advert.cap, &advert.dir, None) {
            info!(
                cap = %advert.cap,
                dir = %advert.dir,
                host = %advert.host,
                %addr,
                action = %am.action,
                lifetime = am.lifetime.as_deref().unwrap_or("while-advertised"),
                "auto-mount candidate matched (§6.1); awaiting descriptor fetch to issue the mount"
            );
        }
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
async fn cmd_discover(
    timeout_secs: u64,
    kind: Option<&str>,
    dir: Option<&str>,
    host: Option<&str>,
) -> Result<()> {
    let events = discovery::browse().context("start mDNS browse")?;
    info!(timeout_secs, "discovering capmesh capabilities");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut seen: std::collections::BTreeMap<String, (CapabilityAdvert, std::net::IpAddr, u16)> =
        std::collections::BTreeMap::new();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = events.recv_async() => match ev {
                Ok(ServiceEvent::ServiceResolved(svc)) => {
                    if let (Some(addr), Ok(advert)) =
                        (discovery::resolved_addr(&svc), discovery::advert_from_resolved(&svc))
                        && advert.matches(kind, dir, host)
                    {
                        seen.insert(advert.id.clone(), (advert, addr, svc.get_port()));
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            },
        }
    }

    if seen.is_empty() {
        info!("no capmesh capabilities discovered");
    }
    for (advert, addr, port) in seen.values() {
        // Resolve the coarse advert's `descr` pointer into the full typed descriptor over the
        // peer's mesh control endpoint (docs/MESH-PROTOCOL.md). A peer that doesn't serve the
        // endpoint (or is momentarily down) still shows as the coarse advert.
        match capmesh_mesh::fetch_capability(*addr, *port, &advert.descr).await {
            Ok(cap) => {
                info!(
                    host = %advert.host,
                    cap = %advert.cap,
                    id = %advert.id,
                    %addr,
                    port,
                    ports = cap.ports.len(),
                    "capability"
                );
                for p in &cap.ports {
                    let codecs = p
                        .formats
                        .iter()
                        .map(|f| f.codec.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    info!(
                        id = %advert.id,
                        port_id = %p.port_id,
                        dir = p.dir.as_deref().unwrap_or("-"),
                        r#type = %p.type_,
                        name = %p.name,
                        codecs = %codecs,
                        "  port"
                    );
                }
            }
            Err(e) => info!(
                host = %advert.host,
                cap = %advert.cap,
                dir = %advert.dir,
                id = %advert.id,
                %addr,
                port,
                descr = %advert.descr,
                "capability (descriptor unavailable: {e})"
            ),
        }
    }
    Ok(())
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

/// Connect + hello, then print live mounts (§3).
async fn cmd_mount_status(socket: &Path, mount_id: Option<&str>) -> Result<()> {
    let mut client = connect_and_hello(socket).await?;
    let res = client
        .mount_status(mount_id)
        .await
        .context("mount-status")?;
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
