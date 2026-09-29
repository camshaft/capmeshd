//! The MCP gateway's **federation core** (DESIGN §7.2): the pure logic that merges several upstream
//! MCP servers into one namespaced tool surface and routes a call back to its owning upstream.
//!
//! The gateway federates each upstream under its stable id as a `tools/list` namespace prefix —
//! upstream `board`'s tool `create_task` is advertised to the agent as `board__create_task`.
//! Because an upstream id never contains the `__` separator (enforced by `McpRoute::valid_id` on
//! the capmeshd side), a namespaced name splits unambiguously on its FIRST `__` back into
//! `(upstream_id, original_tool)` — even when the tool name itself contains `__`.
//!
//! This module is transport-agnostic (no MCP client, no server, no network), so it is fully
//! unit-tested; the outbound client, the inbound `/mcp` server, and the route-table wiring that
//! capmeshd drives live in the gateway daemon around it.

pub mod client;

use serde_json::Value;
use std::collections::BTreeMap;

/// The namespace separator between an upstream id and a tool name (`<id>__<tool>`).
pub const NS_SEP: &str = "__";

/// The gateway-facing name for an upstream's tool: `<upstream_id>__<tool>`.
pub fn namespaced_name(upstream_id: &str, tool: &str) -> String {
    format!("{upstream_id}{NS_SEP}{tool}")
}

/// Split a namespaced tool name back into `(upstream_id, tool)` on the FIRST `__`. `None` if there
/// is no separator. The upstream id never contains `__`, so the first `__` is always the boundary,
/// even when the tool name itself contains `__`.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once(NS_SEP)
}

/// The federated view of several upstream MCP servers (DESIGN §7.2): each upstream's `tools/list`
/// kept under its id, merged into one namespaced surface for the agent and routed back on a call.
#[derive(Debug, Clone, Default)]
pub struct Federation {
    upstreams: BTreeMap<String, Vec<Value>>,
}

impl Federation {
    /// An empty federation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or replace) an upstream's advertised tools — its raw `tools/list` entries.
    pub fn set_upstream_tools(&mut self, upstream_id: impl Into<String>, tools: Vec<Value>) {
        self.upstreams.insert(upstream_id.into(), tools);
    }

    /// Drop an upstream (it defederated / went away). Returns whether it had been federated.
    pub fn remove_upstream(&mut self, upstream_id: &str) -> bool {
        self.upstreams.remove(upstream_id).is_some()
    }

    /// The merged `tools/list` the gateway advertises: every upstream's tools with their `name`
    /// rewritten to `<id>__<name>`, ordered by upstream id then the upstream's own order. A tool
    /// entry without a string `name` is skipped — it could not be addressed on a `tools/call`.
    pub fn merged_tools(&self) -> Vec<Value> {
        let mut merged = Vec::new();
        for (id, tools) in &self.upstreams {
            for tool in tools {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let mut rewritten = tool.clone();
                if let Some(obj) = rewritten.as_object_mut() {
                    obj.insert("name".to_string(), Value::String(namespaced_name(id, name)));
                }
                merged.push(rewritten);
            }
        }
        merged
    }

    /// Route a namespaced tool name (from a `tools/call`) to its owning upstream id + original tool
    /// name. `None` if it isn't namespaced or names an upstream this federation doesn't hold.
    pub fn route<'a>(&self, namespaced: &'a str) -> Option<(&'a str, &'a str)> {
        let (id, tool) = split_name(namespaced)?;
        self.upstreams.contains_key(id).then_some((id, tool))
    }

    /// The federated upstream ids, ordered.
    pub fn upstream_ids(&self) -> Vec<&str> {
        self.upstreams.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Value {
        json!({ "name": name, "description": format!("does {name}"), "inputSchema": {"type":"object"} })
    }

    #[test]
    fn namespacing_round_trips_including_tool_names_with_underscores() {
        assert_eq!(namespaced_name("board", "create_task"), "board__create_task");
        assert_eq!(split_name("board__create_task"), Some(("board", "create_task")));
        // The tool name itself may contain `__`; only the FIRST `__` is the boundary.
        assert_eq!(split_name("kb__search__v2"), Some(("kb", "search__v2")));
        // Not namespaced.
        assert_eq!(split_name("bare"), None);
    }

    #[test]
    fn merged_tools_namespaces_and_orders_by_upstream_id() {
        let mut fed = Federation::new();
        fed.set_upstream_tools("surfaced", vec![tool("send_item")]);
        fed.set_upstream_tools("board", vec![tool("create_task"), tool("list_tasks")]);

        let merged = fed.merged_tools();
        let names: Vec<&str> = merged.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Ordered by upstream id (board before surfaced), each namespaced, upstream order preserved.
        assert_eq!(
            names,
            ["board__create_task", "board__list_tasks", "surfaced__send_item"]
        );
        // Non-name fields are carried through untouched.
        assert_eq!(merged[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn route_resolves_to_upstream_and_original_tool() {
        let mut fed = Federation::new();
        fed.set_upstream_tools("board", vec![tool("create_task")]);

        assert_eq!(fed.route("board__create_task"), Some(("board", "create_task")));
        // Unknown upstream → None even though it parses.
        assert_eq!(fed.route("kb__search"), None);
        // Not namespaced → None.
        assert_eq!(fed.route("bare"), None);
    }

    #[test]
    fn remove_upstream_drops_its_tools() {
        let mut fed = Federation::new();
        fed.set_upstream_tools("board", vec![tool("create_task")]);
        assert!(fed.remove_upstream("board"));
        assert!(!fed.remove_upstream("board")); // already gone
        assert!(fed.merged_tools().is_empty());
        assert_eq!(fed.route("board__create_task"), None);
    }

    #[test]
    fn tool_without_a_string_name_is_skipped() {
        let mut fed = Federation::new();
        fed.set_upstream_tools("x", vec![json!({"description": "no name"}), tool("ok")]);
        let merged = fed.merged_tools();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["name"], "x__ok");
    }
}
