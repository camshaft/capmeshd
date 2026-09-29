//! The `surfaced` daemon config.
//!
//! Per the fleet TOML-config mandate (operator seq-1377): the daemon takes its
//! configuration from a `--config` TOML file, **never** from environment
//! variables (no `RUST_LOG` / no `SURFACED_*` / no `env::var`). The NixOS module
//! renders this file from `services.surfaced.*` and passes `--config`; the file
//! is the single inspectable source of the daemon's settings.
//!
//! Mirrors capmeshd/nmidid's config shape (kebab-case keys, `deny_unknown_fields`,
//! a `log` env-filter directive). Every key is optional and defaults below.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The default config path the NixOS module renders to (and `--config`'s default).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/surfaced/surfaced.toml";

/// The default HTTP/SSE bind address.
pub const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8787";

fn default_http_addr() -> SocketAddr {
    DEFAULT_HTTP_ADDR
        .parse()
        .expect("DEFAULT_HTTP_ADDR is a valid SocketAddr")
}

fn default_base_path() -> String {
    String::new()
}

fn default_log() -> String {
    "info".to_string()
}

/// Parsed `/etc/surfaced/surfaced.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the HTTP/SSE server binds (attachment pages + push path).
    #[serde(rename = "http-addr", default = "default_http_addr")]
    pub http_addr: SocketAddr,

    /// Directory for durable per-surface inbox logs. `None` (omit the key) runs
    /// in-memory only — surfaces do not survive a restart.
    #[serde(rename = "state-dir", default)]
    pub state_dir: Option<String>,

    /// Mount the server under a URL prefix (e.g. `/surfaced`) for reverse-proxy
    /// deployment. Empty (default) serves at the root.
    #[serde(rename = "base-path", default = "default_base_path")]
    pub base_path: String,

    /// Path of the Unix control socket capmeshd drives (`surface-ctl`,
    /// docs/SURFACE-PROTOCOL.md). `None` (omit the key) runs HTTP-only, with no
    /// mesh control plane.
    #[serde(default)]
    pub socket: Option<String>,

    /// Bearer token required on the `/mcp` agent endpoint. `None` (omit the key)
    /// leaves `/mcp` open (trust the LAN or a reverse proxy).
    #[serde(rename = "mcp-token", default)]
    pub mcp_token: Option<String>,

    /// Log verbosity as a `tracing` env-filter directive (e.g. `info` or
    /// `surfaced=debug,info`). Configured here in the TOML — NOT via `RUST_LOG`
    /// or any env var (seq-1377). Defaults to `info`.
    #[serde(default = "default_log")]
    pub log: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            http_addr: default_http_addr(),
            state_dir: None,
            base_path: default_base_path(),
            socket: None,
            mcp_token: None,
            log: default_log(),
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
        toml::from_str(text).context("failed to parse surfaced config TOML")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = Config::parse("").expect("empty parses");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.http_addr.to_string(), "127.0.0.1:8787");
        assert_eq!(cfg.state_dir, None);
        assert_eq!(cfg.base_path, "");
        assert_eq!(cfg.socket, None);
        assert_eq!(cfg.mcp_token, None);
        assert_eq!(cfg.log, "info");
    }

    #[test]
    fn parses_a_full_config() {
        let cfg = Config::parse(
            r#"
http-addr = "0.0.0.0:8787"
state-dir = "/var/lib/surfaced"
base-path = "/surfaced"
socket = "/run/surfaced/surfaced.sock"
mcp-token = "s3cr3t"
log = "surfaced=debug,info"
"#,
        )
        .expect("parse");
        assert_eq!(cfg.http_addr.to_string(), "0.0.0.0:8787");
        assert_eq!(cfg.state_dir.as_deref(), Some("/var/lib/surfaced"));
        assert_eq!(cfg.base_path, "/surfaced");
        assert_eq!(cfg.socket.as_deref(), Some("/run/surfaced/surfaced.sock"));
        assert_eq!(cfg.mcp_token.as_deref(), Some("s3cr3t"));
        assert_eq!(cfg.log, "surfaced=debug,info");
    }

    #[test]
    fn log_defaults_to_info_and_parses_a_directive() {
        assert_eq!(Config::parse("").unwrap().log, "info");
        assert_eq!(Config::parse(r#"log = "warn""#).unwrap().log, "warn");
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields guards against a stale/misspelled module render key
        // silently becoming a no-op (e.g. the old `log-level`/`SURFACED_*` names).
        let err = Config::parse(r#"log-level = "debug""#).unwrap_err();
        assert!(
            err.to_string().contains("surfaced config TOML"),
            "unknown key should be a parse error, got: {err}"
        );
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let cfg = Config::parse(r#"log = "trace""#).expect("parse");
        assert_eq!(cfg.log, "trace");
        // Untouched keys keep their defaults.
        assert_eq!(cfg.http_addr.to_string(), "127.0.0.1:8787");
        assert_eq!(cfg.base_path, "");
        assert_eq!(cfg.state_dir, None);
    }

    #[test]
    fn rejects_a_malformed_http_addr() {
        let err = Config::parse(r#"http-addr = "not-an-addr""#).unwrap_err();
        assert!(
            err.to_string().contains("surfaced config TOML"),
            "a bad http-addr should be a parse error, got: {err}"
        );
    }
}
