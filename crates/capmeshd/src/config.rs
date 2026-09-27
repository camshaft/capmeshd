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

    /// Auto-mount selectors (DESIGN §6.1). Parsed now; the reconciler consumes them at M1.
    #[serde(default)]
    #[allow(dead_code)] // consumed by the reconciler's auto-mount pass (M1)
    pub automount: Vec<Automount>,

    /// Permanent desired mounts (DESIGN §9) — reconciled at startup + on drift.
    #[serde(rename = "permanent-mount", default)]
    pub permanent_mounts: Vec<PermanentMount>,
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

/// An auto-mount selector (DESIGN §6.1): the reconciler derives desired mounts from
/// live discovery events matching this selector.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // fields consumed by the reconciler's auto-mount pass (M1)
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
#[allow(dead_code)] // fields consumed by the reconciler's auto-mount pass (M1)
pub struct MatchSelector {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub port: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
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
