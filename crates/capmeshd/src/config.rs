//! The data-plane config (DESIGN §4.1).
//!
//! capmeshd is config-driven: a TOML file declares which data-plane protocols this
//! host supports and how each in-process adapter reaches its local data-plane daemon.
//! The NixOS module (§9) renders this file from `services.capmesh.advertise.*`, but the
//! file is the single inspectable source of "what this host can plumb".
//!
//! M0b models the flat `[dataplane.<kind>]` form (the MIDI case). The nested
//! `[dataplane.printer.<instance>]` form (control-API kinds) is a later milestone (M5).

use anyhow::{Context, Result};
use capmesh_ctl::{McpRoute, McpTransport};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

/// The default control-endpoint port advertised over `_capmesh._tcp`.
pub const DEFAULT_PORT: u16 = 7420;

/// The default config path the NixOS module renders to.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/capmesh/capmesh.toml";

fn default_port() -> u16 {
    DEFAULT_PORT
}

/// Parsed `/etc/capmesh/capmesh.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// This host's stable id. Defaults to the system hostname when absent.
    #[serde(rename = "host-id", default)]
    pub host_id: Option<String>,

    /// Log verbosity as a `tracing` env-filter directive (e.g. `info` or `capmeshd=debug,info`).
    /// Configured here in the TOML — NOT via `RUST_LOG` or any env var (fleet mandate seq-1377:
    /// daemons take their config from `--config`, not the environment). Defaults to `info`.
    #[serde(default = "default_log")]
    pub log: String,

    /// Path to the cluster credential (DESIGN §8, trust boundary). Parsed now; the
    /// credential is enforced on actionable RPC in a later slice.
    #[serde(rename = "cluster-key-file", default)]
    #[allow(dead_code)] // consumed when the trust boundary (§8) is enforced
    pub cluster_key_file: Option<PathBuf>,

    /// The control endpoint port advertised over `_capmesh._tcp`.
    #[serde(rename = "advertise-port", default = "default_port")]
    pub advertise_port: u16,

    /// Data-plane adapters keyed by capability kind (`midi`, `audio`, …).
    #[serde(default)]
    pub dataplane: HashMap<String, Dataplane>,

    /// Auto-mount rules (DESIGN §6.1): each is consumed by the daemon's discovery-driven
    /// auto-mount pass, which evaluates every rule against each discovered, descriptor-fetched
    /// peer and issues the derived mount (see `main.rs` `try_auto_mount`).
    #[serde(default)]
    pub automount: Vec<Automount>,

    /// Permanent desired mounts (DESIGN §9) — reconciled at startup + on drift.
    #[serde(rename = "permanent-mount", default)]
    pub permanent_mounts: Vec<PermanentMount>,

    /// Statically-declared MCP routes (DESIGN §7.2) seeded into the route registry at startup —
    /// the "explicit floor" for known upstreams (kb / board / surfaced), complementing the runtime
    /// register endpoint and `cap=mcp` auto-discovery.
    #[serde(rename = "mcp-route", default)]
    pub mcp_routes: Vec<McpRouteConfig>,

    /// Local bind address for the MCP route control endpoint (DESIGN §7.2 `register`). When set,
    /// capmeshd serves `GET/POST/DELETE /mcp/routes` here so an operator (or `cap=mcp` discovery)
    /// can add/remove routes at runtime. Bind loopback — this mutates control-plane state and is
    /// not the peer-facing mesh endpoint. Absent → the endpoint is off (config routes only).
    #[serde(rename = "mcp-control-addr", default)]
    pub mcp_control_addr: Option<String>,

    /// Base URL of the MCP gateway's control channel (DESIGN §7.2), e.g. `http://127.0.0.1:8092`.
    /// When set, capmeshd drives the running gateway on every route registry change (register →
    /// federate, unregister → defederate) so a `cap=mcp` (de)registration takes effect live. Absent
    /// → the registry is maintained but the gateway is not driven (it uses its own static config).
    #[serde(rename = "gateway-admin-url", default)]
    pub gateway_admin_url: Option<String>,

    /// Local bind address for capmesh's own embedded control-tools MCP server (DESIGN §7.1). When
    /// set, capmeshd serves `POST /mcp` here exposing its control tools (discover/describe/connect/
    /// disconnect/status), so the gateway can federate capmesh as a `cap=mcp` upstream. Bind
    /// loopback (or the mesh interface once `cap=mcp` advertise lands). Absent → the server is off.
    #[serde(rename = "mcp-serve-addr", default)]
    pub mcp_serve_addr: Option<String>,
}

/// A statically-declared MCP route (DESIGN §7.2), the config form of an [`McpRoute`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpRouteConfig {
    /// Stable upstream id — also the gateway's `tools/list` namespace prefix.
    pub id: String,
    /// Where the gateway reaches it (endpoint URL for `streamable-http`, command for `stdio`).
    pub url: String,
    /// The transport the gateway speaks to this upstream.
    pub transport: McpTransport,
    /// The MCP protocol revision the upstream negotiates, if known (e.g. `2026-07-28`).
    #[serde(rename = "protocol-rev", default)]
    pub protocol_rev: Option<String>,
}

impl McpRouteConfig {
    /// The registry [`McpRoute`] this config entry declares.
    pub fn to_route(&self) -> McpRoute {
        McpRoute {
            id: self.id.clone(),
            url: self.url.clone(),
            transport: self.transport,
            protocol_rev: self.protocol_rev.clone(),
        }
    }
}

fn default_log() -> String {
    "info".to_string()
}

fn default_role() -> String {
    "mirror-source".to_string()
}

fn default_codec() -> String {
    "midi1".to_string()
}

/// A permanent desired mount (DESIGN §9), reconciled every boot for self-heal. M0b takes a
/// concrete remote endpoint; discovery-resolved capability names arrive with the connect API
/// + auto-mount (M1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermanentMount {
    /// Idempotency key (§3.1); defaults from the remote endpoint when absent.
    #[serde(rename = "mount-id", default)]
    pub mount_id: Option<String>,
    /// `mirror-source` | `mirror-sink` | `link` (§3.1).
    #[serde(default = "default_role")]
    pub role: String,
    /// Display name for the local virtual device (mirror roles).
    #[serde(rename = "local-name", default)]
    pub local_name: Option<String>,
    /// The local **real** port to bind for a `link` mount (§3.1 `local.port-id`) — the id from the
    /// data-plane daemon's own `list-ports` (e.g. `sink-supercollider`). Required in practice for a
    /// `link` mount (there is no other way to name which local port to bind); ignored for the mirror
    /// roles, which create a virtual endpoint rather than bind an existing one.
    #[serde(rename = "local-port-id", default)]
    pub local_port_id: Option<String>,
    /// Chosen wire-format codec.
    #[serde(default = "default_codec")]
    pub codec: String,
    pub remote: RemoteMount,
}

/// The remote endpoint of a permanent mount (§3.1) — connect by IP, never a `.local` name.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteMount {
    #[serde(default)]
    pub host: Option<String>,
    /// The peer IP (typed so a `.local`/`.lan` name will not deserialize; DESIGN §5).
    pub addr: IpAddr,
    pub port: u16,
    #[serde(rename = "port-id")]
    pub port_id: String,
}

/// One data-plane adapter binding (DESIGN §4.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataplane {
    /// Selects the in-process adapter: `nmidi-ctl` | `pipewire` | `moonraker-jsonrpc` | …
    pub protocol: String,
    /// Unix-socket path for socket protocols (e.g. `nmidi-ctl` → `/run/nmidid.sock`).
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// Endpoint URL for endpoint protocols (e.g. Moonraker WebSocket).
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// An auto-mount rule (DESIGN §6.1): the reconciler derives a desired mount for any
/// discovered capability matching `selector`, applying `action` for its `lifetime`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Automount {
    #[serde(rename = "match")]
    pub selector: MatchSelector,
    pub action: String,
    #[serde(default)]
    pub lifetime: Option<String>,
}

/// The selector over the typed model matched against advertised capabilities (§3.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSelector {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
}

impl MatchSelector {
    /// Coarse pre-fetch match (§6.1): whether this selector's `kind` admits a discovered
    /// capability, using only the field the host-level `_capmesh._tcp` advert carries reliably.
    ///
    /// The advert's `dir` is always coarse — a data-plane kind can expose both source and sink
    /// ports, so the host-level advert reports `duplex` — and `port` (a port-id) is not known
    /// until the descriptor is fetched. So `dir` and `port` are deliberately NOT matched here;
    /// they are applied against the descriptor's real ports when the mount is planned (see
    /// `capmesh_daemon::automount::plan_mount`). Matching them at this stage would reject a rule (e.g.
    /// `dir = "source"`) before the fetch that could satisfy it. A `None` `kind` admits anything.
    pub fn coarse_matches(&self, kind: &str) -> bool {
        self.kind.as_deref().is_none_or(|k| k == kind)
    }
}

impl Config {
    /// Load and parse the config at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        Self::parse(&text)
    }

    /// Parse config from a TOML string.
    pub fn parse(text: &str) -> Result<Self> {
        toml::from_str(text).context("failed to parse capmesh config TOML")
    }

    /// The resolved host id: the configured `host-id`, else the system hostname, else a
    /// stable fallback derived from the loopback so advertisement always has a value.
    pub fn resolved_host_id(&self) -> String {
        if let Some(id) = &self.host_id {
            return id.clone();
        }
        hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| format!("host-{}", Ipv4Addr::LOCALHOST))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_midi_example() {
        let cfg = Config::parse(
            r#"
host-id = "green-machine"
cluster-key-file = "/run/agenix/capmesh-cluster.key"

[dataplane.midi]
protocol = "nmidi-ctl"
socket   = "/run/nmidid.sock"
"#,
        )
        .expect("parse");
        assert_eq!(cfg.resolved_host_id(), "green-machine");
        assert_eq!(cfg.advertise_port, DEFAULT_PORT);
        let midi = cfg.dataplane.get("midi").expect("midi dataplane");
        assert_eq!(midi.protocol, "nmidi-ctl");
        assert_eq!(midi.socket.as_deref(), Some(Path::new("/run/nmidid.sock")));
        // Log verbosity is TOML config (seq-1377): absent → the `info` default.
        assert_eq!(cfg.log, "info");
    }

    #[test]
    fn log_defaults_to_info_and_parses_a_directive() {
        assert_eq!(Config::parse("").expect("empty parses").log, "info");
        let cfg = Config::parse(r#"log = "capmeshd=debug,info""#).expect("parse");
        assert_eq!(cfg.log, "capmeshd=debug,info");
    }

    #[test]
    fn parses_automount_selector() {
        let cfg = Config::parse(
            r#"
host-id = "desk"

[[automount]]
match  = { kind = "midi", port = "stream", dir = "source" }
action = "mirror-local"
lifetime = "while-advertised"
"#,
        )
        .expect("parse");
        assert_eq!(cfg.automount.len(), 1);
        let am = &cfg.automount[0];
        assert_eq!(am.selector.kind.as_deref(), Some("midi"));
        assert_eq!(am.action, "mirror-local");
        assert_eq!(am.lifetime.as_deref(), Some("while-advertised"));
    }

    #[test]
    fn coarse_matches_filters_on_kind_only() {
        let sel = MatchSelector {
            kind: Some("midi".into()),
            dir: Some("source".into()),
            port: None,
        };
        // Coarse (pre-fetch) match is kind-only: `dir`/`port` are applied later against the
        // fetched descriptor's real ports, so a dir-constrained rule must still pass the coarse
        // filter (the host advert's dir is always the coarse `duplex`).
        assert!(sel.coarse_matches("midi"));
        assert!(!sel.coarse_matches("audio")); // wrong kind
    }

    #[test]
    fn coarse_matches_empty_kind_admits_anything() {
        let sel = MatchSelector { kind: None, dir: None, port: None };
        assert!(sel.coarse_matches("midi"));
        assert!(sel.coarse_matches("audio"));
    }

    #[test]
    fn coarse_matches_ignores_a_port_constraint() {
        // A port-constrained rule must pass the coarse filter (port-id is unknown pre-fetch);
        // the port is matched against the descriptor when the mount is planned.
        let sel = MatchSelector {
            kind: Some("midi".into()),
            dir: None,
            port: Some("kbd-0".into()),
        };
        assert!(sel.coarse_matches("midi"));
        assert!(!sel.coarse_matches("audio"));
    }

    #[test]
    fn empty_config_falls_back_to_hostname() {
        let cfg = Config::parse("").expect("parse empty");
        assert!(!cfg.resolved_host_id().is_empty());
        assert!(cfg.dataplane.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(Config::parse("bogus-key = 1").is_err());
    }

    #[test]
    fn parses_mcp_routes() {
        let cfg = Config::parse(
            r#"
host-id = "green-machine"

[[mcp-route]]
id = "board"
url = "http://green-machine.lan:8880/board/mcp"
transport = "streamable-http"
protocol-rev = "2026-07-28"

[[mcp-route]]
id = "local-tool"
url = "run-local-mcp"
transport = "stdio"
"#,
        )
        .expect("parse");
        assert_eq!(cfg.mcp_routes.len(), 2);
        let board = &cfg.mcp_routes[0];
        assert_eq!(board.id, "board");
        assert_eq!(board.transport, McpTransport::StreamableHttp);
        assert_eq!(board.protocol_rev.as_deref(), Some("2026-07-28"));
        // to_route() carries the fields into the registry type.
        assert_eq!(board.to_route().url, "http://green-machine.lan:8880/board/mcp");
        // stdio + omitted protocol-rev.
        assert_eq!(cfg.mcp_routes[1].transport, McpTransport::Stdio);
        assert_eq!(cfg.mcp_routes[1].protocol_rev, None);
    }

    #[test]
    fn parses_a_permanent_mount() {
        let cfg = Config::parse(
            r#"
host-id = "music-host"

[dataplane.midi]
protocol = "nmidi-ctl"
socket   = "/run/nmidid.sock"

[[permanent-mount]]
local-name = "studio keyboard"
remote = { host = "studio", addr = "192.168.1.23", port = 5004, port-id = "kbd-0" }
"#,
        )
        .expect("parse");
        assert_eq!(cfg.permanent_mounts.len(), 1);
        let pm = &cfg.permanent_mounts[0];
        assert_eq!(pm.role, "mirror-source"); // default
        assert_eq!(pm.codec, "midi1"); // default
        assert_eq!(pm.local_name.as_deref(), Some("studio keyboard"));
        assert_eq!(pm.remote.addr, "192.168.1.23".parse::<IpAddr>().unwrap());
        assert_eq!(pm.remote.port, 5004);
        assert_eq!(pm.remote.port_id, "kbd-0");
        // A mirror mount omits the local real-port selector.
        assert_eq!(pm.local_port_id, None);
    }

    #[test]
    fn parses_a_link_permanent_mount_with_a_local_port_id() {
        // A `link` permanent mount names the local real port to bind via `local-port-id` (§3.1).
        let cfg = Config::parse(
            r#"
host-id = "studio-host"

[[permanent-mount]]
role = "link"
local-port-id = "sink-supercollider"
remote = { addr = "192.168.1.23", port = 5004, port-id = "kbd-0" }
"#,
        )
        .expect("parse");
        let pm = &cfg.permanent_mounts[0];
        assert_eq!(pm.role, "link");
        assert_eq!(pm.local_port_id.as_deref(), Some("sink-supercollider"));
    }

    #[test]
    fn parses_the_module_rendered_subtable_form() {
        // The exact shape the NixOS module emits (pkgs.formats.toml renders `remote` as a
        // [permanent-mount.remote] subtable, not an inline table) — pins renderer↔parser.
        let cfg = Config::parse(
            r#"
host-id = "check-host"

[[permanent-mount]]
codec = "midi1"
local-name = "studio keyboard"
role = "mirror-source"

[permanent-mount.remote]
addr = "192.168.1.23"
port = 5004
port-id = "kbd-0"
"#,
        )
        .expect("parse module-rendered form");
        let pm = &cfg.permanent_mounts[0];
        assert_eq!(pm.remote.port_id, "kbd-0");
        assert_eq!(pm.remote.addr, "192.168.1.23".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn parses_the_module_rendered_automount_subtable_form() {
        // The exact shape the NixOS module emits for an auto-mount rule (pkgs.formats.toml
        // renders `match` as an [automount.match] subtable, not an inline table) — pins
        // renderer↔parser for §6.1 the same way the permanent-mount test does for §9.
        let cfg = Config::parse(
            r#"
host-id = "check-host"

[[automount]]
action = "mirror-local"
lifetime = "while-advertised"

[automount.match]
dir = "source"
kind = "midi"
"#,
        )
        .expect("parse module-rendered automount form");
        assert_eq!(cfg.automount.len(), 1);
        let am = &cfg.automount[0];
        assert_eq!(am.selector.kind.as_deref(), Some("midi"));
        assert_eq!(am.selector.dir.as_deref(), Some("source"));
        assert_eq!(am.selector.port, None); // unset in the rule → matches any port
        assert_eq!(am.action, "mirror-local");
        assert_eq!(am.lifetime.as_deref(), Some("while-advertised"));
    }

    #[test]
    fn permanent_mount_rejects_a_non_ip_addr() {
        // Connect-by-IP (§5): a `.local` remote must not parse.
        let err = Config::parse(
            r#"
[[permanent-mount]]
remote = { addr = "studio.local", port = 5004, port-id = "kbd-0" }
"#,
        );
        assert!(err.is_err());
    }
}
