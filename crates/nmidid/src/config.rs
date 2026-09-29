//! The `nmidid` daemon config.
//!
//! Per the fleet TOML-config mandate (operator seq-1377): the daemon takes its
//! configuration from a `--config` TOML file, **never** from environment
//! variables (no `RUST_LOG` / no `env::var`). The NixOS module renders this file
//! from `services.nmidid.*` and passes `--config`; the file is the single
//! inspectable source of the daemon's settings.
//!
//! Mirrors capmeshd's config shape (kebab-case keys, `deny_unknown_fields`, a
//! `log` env-filter directive). Every key is optional and defaults below.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// The default config path the NixOS module renders to (and `--config`'s default).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/nmidid/nmidid.toml";

/// The default control socket path.
pub const DEFAULT_SOCKET: &str = "/run/nmidid.sock";

fn default_socket() -> String {
    DEFAULT_SOCKET.to_string()
}

fn default_monitor_interval() -> u64 {
    5
}

fn default_log() -> String {
    "info".to_string()
}

/// Parsed `/etc/nmidid/nmidid.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Unix control socket to bind (CONTROL-PROTOCOL §1).
    #[serde(default = "default_socket")]
    pub socket: String,

    /// How often to poll local MIDI ports for hot-plug, in seconds (§5).
    #[serde(rename = "monitor-interval", default = "default_monitor_interval")]
    pub monitor_interval: u64,

    /// Log verbosity as a `tracing` env-filter directive (e.g. `info` or
    /// `nmidid=debug,info`). Configured here in the TOML — NOT via `RUST_LOG` or
    /// any env var (seq-1377). Defaults to `info`.
    #[serde(default = "default_log")]
    pub log: String,

    /// Permit control connections from these uids (§1.1). The daemon's own uid is
    /// always allowed; empty (with no gids/groups) means enforcement is off.
    #[serde(rename = "allow-uids", default)]
    pub allow_uids: Vec<u32>,

    /// Permit control connections from these gids (§1.1).
    #[serde(rename = "allow-gids", default)]
    pub allow_gids: Vec<u32>,

    /// Permit control connections from these group *names* (§1.1), each resolved
    /// to a gid from `/etc/group` at startup. Preferred over a raw gid, since an
    /// auto-allocated NixOS group has no gid known at evaluation time.
    #[serde(rename = "allow-groups", default)]
    pub allow_groups: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            socket: default_socket(),
            monitor_interval: default_monitor_interval(),
            log: default_log(),
            allow_uids: Vec::new(),
            allow_gids: Vec::new(),
            allow_groups: Vec::new(),
        }
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
        toml::from_str(text).context("failed to parse nmidid config TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = Config::parse("").expect("empty parses");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.socket, "/run/nmidid.sock");
        assert_eq!(cfg.monitor_interval, 5);
        assert_eq!(cfg.log, "info");
        assert!(cfg.allow_uids.is_empty());
        assert!(cfg.allow_gids.is_empty());
        assert!(cfg.allow_groups.is_empty());
    }

    #[test]
    fn parses_a_full_config() {
        let cfg = Config::parse(
            r#"
socket = "/run/nmidid/nmidid.sock"
monitor-interval = 10
log = "nmidid=debug,info"
allow-uids = [1000, 1001]
allow-gids = [29]
allow-groups = ["capmesh", "audio"]
"#,
        )
        .expect("parse");
        assert_eq!(cfg.socket, "/run/nmidid/nmidid.sock");
        assert_eq!(cfg.monitor_interval, 10);
        assert_eq!(cfg.log, "nmidid=debug,info");
        assert_eq!(cfg.allow_uids, vec![1000, 1001]);
        assert_eq!(cfg.allow_gids, vec![29]);
        assert_eq!(cfg.allow_groups, vec!["capmesh".to_string(), "audio".to_string()]);
    }

    #[test]
    fn log_defaults_to_info_and_parses_a_directive() {
        assert_eq!(Config::parse("").unwrap().log, "info");
        assert_eq!(
            Config::parse(r#"log = "warn""#).unwrap().log,
            "warn"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields guards against a stale/misspelled module render key
        // silently becoming a no-op.
        let err = Config::parse(r#"loglevel = "debug""#).unwrap_err();
        assert!(
            err.to_string().contains("nmidid config TOML"),
            "unknown key should be a parse error, got: {err}"
        );
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let cfg = Config::parse(r#"log = "trace""#).expect("parse");
        assert_eq!(cfg.log, "trace");
        // Untouched keys keep their defaults.
        assert_eq!(cfg.socket, "/run/nmidid.sock");
        assert_eq!(cfg.monitor_interval, 5);
    }
}
