//! Driving the MCP gateway from route-registry changes (DESIGN §7.2). capmeshd owns the
//! [`McpRouteRegistry`](crate::mcp_routes); when a route is registered/unregistered (by the manual
//! `register` endpoint or `cap=mcp` auto-discovery), it must tell the running gateway to
//! (de)federate that upstream over the gateway's control channel (`/admin/upstreams`).
//!
//! This module is the pure mapping — registry change → the gateway admin request to send. The
//! binary owns the HTTP client that actually sends it, so the decision (which method/path/body, and
//! when to do nothing) is unit-tested here.

use crate::mcp_routes::RegisterOutcome;
use capmesh_ctl::{McpRoute, McpTransport};
use serde_json::{json, Value};

/// The gateway control-channel call a route change implies.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayCommand {
    /// `POST /admin/upstreams` with this body — federate (or re-federate) the upstream.
    Federate { body: Value },
    /// `DELETE /admin/upstreams/{id}` — defederate the upstream.
    Defederate { id: String },
    /// Nothing to drive: an unchanged re-register, or a transport the HTTP gateway can't federate.
    Noop,
}

impl GatewayCommand {
    /// The HTTP request to send to the gateway admin base URL: `(method, path, optional JSON body)`,
    /// or `None` for [`GatewayCommand::Noop`].
    pub fn request(&self) -> Option<(&'static str, String, Option<Value>)> {
        match self {
            GatewayCommand::Federate { body } => {
                Some(("POST", "/admin/upstreams".to_string(), Some(body.clone())))
            }
            GatewayCommand::Defederate { id } => {
                Some(("DELETE", format!("/admin/upstreams/{id}"), None))
            }
            GatewayCommand::Noop => None,
        }
    }
}

/// The command to drive the gateway with after a registry `register` returning `outcome`. Only a
/// real change to a `streamable-http` upstream federates; an unchanged re-register — or a `stdio`
/// route the HTTP gateway can't reach — is a no-op.
pub fn on_register(route: &McpRoute, outcome: RegisterOutcome) -> GatewayCommand {
    match outcome {
        RegisterOutcome::Unchanged => GatewayCommand::Noop,
        RegisterOutcome::Added | RegisterOutcome::Updated => match route.transport {
            McpTransport::StreamableHttp => GatewayCommand::Federate {
                body: federate_body(route),
            },
            McpTransport::Stdio => GatewayCommand::Noop,
        },
    }
}

/// The command to drive the gateway with after a registry `unregister` of `id`.
pub fn on_unregister(id: &str) -> GatewayCommand {
    GatewayCommand::Defederate { id: id.to_string() }
}

/// The `/admin/upstreams` federate body for a route — `protocol-rev` omitted when absent.
fn federate_body(route: &McpRoute) -> Value {
    let mut body = json!({
        "id": route.id,
        "url": route.url,
        "transport": "streamable-http",
    });
    if let Some(rev) = &route.protocol_rev {
        body["protocol-rev"] = json!(rev);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: &str, transport: McpTransport, rev: Option<&str>) -> McpRoute {
        McpRoute {
            id: id.to_string(),
            url: format!("http://h/{id}/mcp"),
            transport,
            protocol_rev: rev.map(str::to_string),
        }
    }

    #[test]
    fn a_new_streamable_http_route_federates() {
        let r = route("board", McpTransport::StreamableHttp, Some("2026-07-28"));
        let cmd = on_register(&r, RegisterOutcome::Added);
        let GatewayCommand::Federate { body } = &cmd else {
            panic!("expected Federate, got {cmd:?}")
        };
        assert_eq!(body["id"], "board");
        assert_eq!(body["url"], "http://h/board/mcp");
        assert_eq!(body["transport"], "streamable-http");
        assert_eq!(body["protocol-rev"], "2026-07-28");

        // Renders as a POST to the federate endpoint.
        let (method, path, sent) = cmd.request().unwrap();
        assert_eq!((method, path.as_str()), ("POST", "/admin/upstreams"));
        assert!(sent.is_some());
    }

    #[test]
    fn protocol_rev_is_omitted_when_absent() {
        let r = route("kb", McpTransport::StreamableHttp, None);
        let GatewayCommand::Federate { body } = on_register(&r, RegisterOutcome::Updated) else {
            panic!("expected Federate")
        };
        assert!(body.get("protocol-rev").is_none());
    }

    #[test]
    fn an_unchanged_register_is_a_noop() {
        let r = route("board", McpTransport::StreamableHttp, None);
        assert_eq!(on_register(&r, RegisterOutcome::Unchanged), GatewayCommand::Noop);
        assert!(on_register(&r, RegisterOutcome::Unchanged).request().is_none());
    }

    #[test]
    fn a_stdio_route_is_not_federated_by_the_http_gateway() {
        let r = route("local", McpTransport::Stdio, None);
        assert_eq!(on_register(&r, RegisterOutcome::Added), GatewayCommand::Noop);
    }

    #[test]
    fn unregister_defederates_by_id() {
        let cmd = on_unregister("board");
        assert_eq!(cmd, GatewayCommand::Defederate { id: "board".into() });
        let (method, path, sent) = cmd.request().unwrap();
        assert_eq!((method, path.as_str()), ("DELETE", "/admin/upstreams/board"));
        assert!(sent.is_none());
    }
}
