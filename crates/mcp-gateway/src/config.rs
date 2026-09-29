//! The gateway daemon's `--config` TOML (DESIGN §7.2; fleet mandate seq-1377: daemons take their
//! config from a file, never env vars). It declares where the single agent-facing `/mcp` binds and
//! the static set of upstreams to federate — the explicit floor, matching capmeshd's `[[mcp-route]]`
//! (capmeshd drives live route changes over the gateway's control socket in a later slice).

use serde::Deserialize;

/// The gateway daemon configuration, parsed from the `--config` TOML.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Where the agent-facing `POST /mcp` binds (e.g. `127.0.0.1:8091`).
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Log verbosity as a `tracing` env-filter directive (default `info`). Config, never `RUST_LOG`.
    #[serde(default = "default_log")]
    pub log: String,

    /// The upstreams to federate at startup (the explicit floor).
    #[serde(default)]
    pub upstream: Vec<UpstreamConfig>,
}

/// One federated upstream — its stable id (also its `tools/list` namespace prefix) and endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Stable upstream id (namespace prefix). Must not contain the `__` separator.
    pub id: String,
    /// The upstream's Streamable-HTTP endpoint URL.
    pub url: String,
    /// The transport the gateway speaks to it (only `streamable-http` is served today).
    #[serde(default = "default_transport")]
    pub transport: String,
    /// The MCP protocol revision the upstream negotiates, if known (e.g. `2026-07-28`).
    #[serde(rename = "protocol-rev", default)]
    pub protocol_rev: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:8091".to_string()
}

fn default_log() -> String {
    "info".to_string()
}

fn default_transport() -> String {
    "streamable-http".to_string()
}

impl GatewayConfig {
    /// Parse config from a TOML string.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("failed to parse gateway config TOML: {e}"))
    }

    /// Load config from a file path.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = GatewayConfig::parse("").unwrap();
        assert_eq!(cfg.bind, "127.0.0.1:8091");
        assert_eq!(cfg.log, "info");
        assert!(cfg.upstream.is_empty());
    }

    #[test]
    fn parses_bind_log_and_upstreams() {
        let cfg = GatewayConfig::parse(
            r#"
bind = "0.0.0.0:9000"
log = "mcp_gateway=debug,info"

[[upstream]]
id = "board"
url = "http://127.0.0.1:7001/mcp"

[[upstream]]
id = "kb"
url = "http://127.0.0.1:7002/mcp"
transport = "streamable-http"
protocol-rev = "2026-07-28"
"#,
        )
        .unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9000");
        assert_eq!(cfg.log, "mcp_gateway=debug,info");
        assert_eq!(cfg.upstream.len(), 2);
        assert_eq!(cfg.upstream[0].id, "board");
        assert_eq!(cfg.upstream[0].transport, "streamable-http"); // defaulted
        assert_eq!(cfg.upstream[1].protocol_rev.as_deref(), Some("2026-07-28"));
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        assert!(GatewayConfig::parse("nope = 1").is_err());
    }
}
