//! The MCP **route registry** (DESIGN §7.2) — capmeshd's control-plane record of which MCP servers
//! the gateway federates, keyed by each upstream's stable id (also its `tools/list` namespace
//! prefix).
//!
//! capmeshd owns this registry because it is the control plane; a route enters it either
//! explicitly (the `register` endpoint) or automatically (a `cap=mcp` discovery calling that same
//! path — §6.1 auto-mount for MCP). capmeshd drives the gateway data-plane daemon off the registry
//! and never serves MCP traffic itself. The wire type [`McpRoute`] lives in `capmesh-model`; this
//! module is the registry behavior. The register endpoint and gateway-driving glue live in the
//! capmeshd binary.

use capmesh_ctl::McpRoute;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The outcome of a [`McpRouteRegistry::register`] — lets the caller decide whether to re-drive the
/// gateway / emit `tools/list_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// A new upstream id — the federated set grew (gateway gains a route → `tools/list_changed`).
    Added,
    /// An existing id whose route details changed — the upstream must be re-federated.
    Updated,
    /// An existing id re-registered with identical details — a no-op (register is idempotent).
    Unchanged,
}

/// capmeshd's registry of federated MCP upstreams, keyed by id (DESIGN §7.2). Ordered by id, so
/// listings and the gateway config derived from it are stable/deterministic.
#[derive(Debug, Clone, Default)]
pub struct McpRouteRegistry {
    routes: BTreeMap<String, McpRoute>,
}

impl McpRouteRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (upsert) an upstream by id. Returns whether the federated set actually changed, so
    /// the caller re-drives the gateway only when needed; rejects an invalid id (see
    /// [`McpRoute::valid_id`]).
    pub fn register(&mut self, route: McpRoute) -> Result<RegisterOutcome, String> {
        if !McpRoute::valid_id(&route.id) {
            return Err(format!("invalid mcp route id '{}'", route.id));
        }
        let outcome = match self.routes.get(&route.id) {
            Some(existing) if *existing == route => RegisterOutcome::Unchanged,
            Some(_) => RegisterOutcome::Updated,
            None => RegisterOutcome::Added,
        };
        self.routes.insert(route.id.clone(), route);
        Ok(outcome)
    }

    /// Remove an upstream by id, returning the removed route (`None` if it wasn't registered).
    pub fn unregister(&mut self, id: &str) -> Option<McpRoute> {
        self.routes.remove(id)
    }

    /// The route for `id`, if registered.
    pub fn get(&self, id: &str) -> Option<&McpRoute> {
        self.routes.get(id)
    }

    /// All routes, ordered by id (stable).
    pub fn routes(&self) -> Vec<&McpRoute> {
        self.routes.values().collect()
    }

    /// How many upstreams are registered.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether no upstreams are registered.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Apply one control-API request (DESIGN §7.2, capmeshd's `register` endpoint), returning the
    /// response the transport should render. `changed` is set when the federated set actually
    /// changed, so the caller re-drives the gateway / emits `tools/list_changed` only then.
    pub fn handle(&mut self, req: RouteRequest) -> RouteResponse {
        match req {
            RouteRequest::List => RouteResponse {
                status: 200,
                body: json!({ "routes": self.routes() }),
                changed: false,
            },
            RouteRequest::Register(route) => match self.register(route) {
                Ok(outcome) => {
                    let outcome_str = match outcome {
                        RegisterOutcome::Added => "added",
                        RegisterOutcome::Updated => "updated",
                        RegisterOutcome::Unchanged => "unchanged",
                    };
                    RouteResponse {
                        status: 200,
                        body: json!({ "outcome": outcome_str }),
                        changed: !matches!(outcome, RegisterOutcome::Unchanged),
                    }
                }
                Err(e) => RouteResponse {
                    status: 400,
                    body: json!({ "error": e }),
                    changed: false,
                },
            },
            RouteRequest::Unregister(id) => match self.unregister(&id) {
                Some(route) => RouteResponse {
                    status: 200,
                    body: json!({ "unregistered": route }),
                    changed: true,
                },
                None => RouteResponse {
                    status: 404,
                    body: json!({ "error": format!("no such mcp route '{id}'") }),
                    changed: false,
                },
            },
        }
    }
}

/// A control-API request against the route registry (DESIGN §7.2). Transport-agnostic: the capmeshd
/// binary parses its HTTP surface (`GET`/`POST`/`DELETE /mcp/routes`) into these and renders the
/// [`RouteResponse`] back. Keeping the contract here makes it unit-testable without a live socket.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteRequest {
    /// List the current routes (`GET /mcp/routes`).
    List,
    /// Register — upsert — a route (`POST /mcp/routes` with an [`McpRoute`] body).
    Register(McpRoute),
    /// Unregister a route by id (`DELETE /mcp/routes/<id>`).
    Unregister(String),
}

/// The outcome of [`McpRouteRegistry::handle`]: an HTTP status, a JSON body, and whether the
/// federated set changed (so the caller re-drives the gateway / emits `tools/list_changed`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResponse {
    pub status: u16,
    pub body: Value,
    pub changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use capmesh_ctl::McpTransport;

    fn route(id: &str, url: &str) -> McpRoute {
        McpRoute {
            id: id.into(),
            url: url.into(),
            transport: McpTransport::StreamableHttp,
            protocol_rev: Some("2026-07-28".into()),
        }
    }

    #[test]
    fn register_reports_added_updated_and_unchanged() {
        let mut reg = McpRouteRegistry::new();
        assert_eq!(
            reg.register(route("board", "http://h/board/mcp")).unwrap(),
            RegisterOutcome::Added
        );
        // Same id + identical details → idempotent no-op.
        assert_eq!(
            reg.register(route("board", "http://h/board/mcp")).unwrap(),
            RegisterOutcome::Unchanged
        );
        // Same id, different endpoint → the upstream moved, must re-federate.
        assert_eq!(
            reg.register(route("board", "http://h2/board/mcp")).unwrap(),
            RegisterOutcome::Updated
        );
        assert_eq!(reg.get("board").unwrap().url, "http://h2/board/mcp");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn register_rejects_an_invalid_id() {
        let mut reg = McpRouteRegistry::new();
        assert!(
            reg.register(route("has_underscore", "http://h/mcp"))
                .is_err()
        );
        assert!(reg.is_empty());
    }

    #[test]
    fn unregister_returns_the_removed_route() {
        let mut reg = McpRouteRegistry::new();
        reg.register(route("kb", "http://h/kb/mcp")).unwrap();
        let removed = reg.unregister("kb").expect("was registered");
        assert_eq!(removed.id, "kb");
        assert!(reg.is_empty());
        // Removing an unknown id is None, not an error.
        assert!(reg.unregister("kb").is_none());
    }

    #[test]
    fn routes_are_listed_in_stable_id_order() {
        let mut reg = McpRouteRegistry::new();
        reg.register(route("surfaced", "http://h/surfaced/mcp"))
            .unwrap();
        reg.register(route("board", "http://h/board/mcp")).unwrap();
        reg.register(route("kb", "http://h/kb/mcp")).unwrap();
        let ids: Vec<&str> = reg.routes().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["board", "kb", "surfaced"]);
    }

    #[test]
    fn handle_register_reports_status_and_change() {
        let mut reg = McpRouteRegistry::new();

        // New route: 200, changed, outcome "added".
        let r = reg.handle(RouteRequest::Register(route("board", "http://h/board/mcp")));
        assert_eq!(r.status, 200);
        assert!(r.changed);
        assert_eq!(r.body["outcome"], "added");

        // Identical re-register: 200, NOT changed (idempotent) — gateway must not be re-driven.
        let r = reg.handle(RouteRequest::Register(route("board", "http://h/board/mcp")));
        assert_eq!(r.status, 200);
        assert!(!r.changed);
        assert_eq!(r.body["outcome"], "unchanged");

        // Invalid id: 400, not changed.
        let r = reg.handle(RouteRequest::Register(route("bad_id", "http://h/mcp")));
        assert_eq!(r.status, 400);
        assert!(!r.changed);
        assert!(r.body["error"].is_string());
    }

    #[test]
    fn handle_list_and_unregister() {
        let mut reg = McpRouteRegistry::new();
        reg.handle(RouteRequest::Register(route("kb", "http://h/kb/mcp")));

        let listed = reg.handle(RouteRequest::List);
        assert_eq!(listed.status, 200);
        assert!(!listed.changed);
        assert_eq!(listed.body["routes"].as_array().unwrap().len(), 1);
        assert_eq!(listed.body["routes"][0]["id"], "kb");

        // Unregister existing: 200 + changed + the removed route echoed.
        let removed = reg.handle(RouteRequest::Unregister("kb".into()));
        assert_eq!(removed.status, 200);
        assert!(removed.changed);
        assert_eq!(removed.body["unregistered"]["id"], "kb");

        // Unregister missing: 404, not changed.
        let missing = reg.handle(RouteRequest::Unregister("kb".into()));
        assert_eq!(missing.status, 404);
        assert!(!missing.changed);
    }
}
