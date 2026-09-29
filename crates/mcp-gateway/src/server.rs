//! The gateway's inbound **MCP server** dispatch (DESIGN §7.2): map one JSON-RPC message from an
//! agent to either a complete reply (initialize / `tools/list` from the [`Federation`] / ping /
//! errors) or a decision to forward a `tools/call` to the owning upstream.
//!
//! Pure and transport-agnostic — the async forward and the Streamable-HTTP `/mcp` transport are the
//! daemon's job — so the protocol dispatch is unit-tested here. The daemon serves the single `/mcp`
//! endpoint, feeds each message to [`dispatch`], sends back a [`ServerAction::Reply`], and for a
//! [`ServerAction::Forward`] uses the outbound [`crate::client`] to call the upstream and wrap the
//! result as the response to `request_id`.

use crate::Federation;
use serde_json::{Value, json};

/// The MCP protocol revision the gateway serves to agents (§7): `2026-07-28`, whose
/// `tools/list_changed` a v2 client honors mid-session.
pub const SERVER_PROTOCOL_VERSION: &str = "2026-07-28";

/// What the transport should do with one dispatched message.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerAction {
    /// Send this JSON-RPC response back to the agent.
    Reply(Value),
    /// Forward a `tools/call` to `upstream_id`'s original `tool`, then wrap the upstream's result as
    /// the JSON-RPC response to `request_id`.
    Forward {
        request_id: Value,
        upstream_id: String,
        tool: String,
        arguments: Value,
    },
    /// A notification (no `id`) — nothing to send back.
    Ignore,
}

/// Dispatch one JSON-RPC 2.0 message from an agent against the current [`Federation`].
pub fn dispatch(federation: &Federation, req: &Value) -> ServerAction {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications carry no `id` and get no reply.
    let Some(id) = req.get("id").cloned() else {
        return ServerAction::Ignore;
    };
    match method {
        "initialize" => ServerAction::Reply(ok(id, initialize_result())),
        "tools/list" => {
            ServerAction::Reply(ok(id, json!({ "tools": federation.merged_tools() })))
        }
        "ping" => ServerAction::Reply(ok(id, json!({}))),
        "tools/call" => dispatch_tools_call(federation, id, req),
        other => ServerAction::Reply(err(id, -32601, &format!("method not found: {other}"))),
    }
}

fn dispatch_tools_call(federation: &Federation, id: Value, req: &Value) -> ServerAction {
    let params = req.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    match federation.route(name) {
        Some((upstream_id, tool)) => ServerAction::Forward {
            request_id: id,
            upstream_id: upstream_id.to_string(),
            tool: tool.to_string(),
            arguments,
        },
        None => ServerAction::Reply(err(id, -32602, &format!("unknown tool '{name}'"))),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": SERVER_PROTOCOL_VERSION,
        // `listChanged: true` — the gateway emits `tools/list_changed` as upstreams (de)federate.
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": { "name": "capmesh-mcp-gateway", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({ "name": name, "description": "", "inputSchema": {"type":"object"} })
    }

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn federation() -> Federation {
        let mut f = Federation::new();
        f.set_upstream_tools("board", vec![tool("create_task")]);
        f
    }

    #[test]
    fn initialize_advertises_proto_and_list_changed() {
        let out = dispatch(&federation(), &req(1, "initialize", json!({})));
        let ServerAction::Reply(v) = out else {
            panic!("expected reply")
        };
        assert_eq!(v["result"]["protocolVersion"], "2026-07-28");
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(v["result"]["serverInfo"]["name"], "capmesh-mcp-gateway");
    }

    #[test]
    fn tools_list_returns_the_namespaced_federation() {
        let out = dispatch(&federation(), &req(2, "tools/list", json!({})));
        let ServerAction::Reply(v) = out else {
            panic!("expected reply")
        };
        assert_eq!(v["result"]["tools"][0]["name"], "board__create_task");
    }

    #[test]
    fn tools_call_forwards_a_known_tool_to_its_upstream() {
        let out = dispatch(
            &federation(),
            &req(
                3,
                "tools/call",
                json!({"name": "board__create_task", "arguments": {"title": "hi"}}),
            ),
        );
        assert_eq!(
            out,
            ServerAction::Forward {
                request_id: json!(3),
                upstream_id: "board".into(),
                tool: "create_task".into(),
                arguments: json!({"title": "hi"}),
            }
        );
    }

    #[test]
    fn tools_call_unknown_tool_is_a_reply_error() {
        let out = dispatch(
            &federation(),
            &req(4, "tools/call", json!({"name": "kb__search"})),
        );
        let ServerAction::Reply(v) = out else {
            panic!("expected reply")
        };
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn notification_is_ignored_and_unknown_method_errors() {
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert_eq!(dispatch(&federation(), &note), ServerAction::Ignore);

        let out = dispatch(&federation(), &req(5, "frobnicate", json!({})));
        let ServerAction::Reply(v) = out else {
            panic!("expected reply")
        };
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn ping_replies_empty() {
        let out = dispatch(&federation(), &req(6, "ping", json!({})));
        assert_eq!(out, ServerAction::Reply(ok(json!(6), json!({}))));
    }
}
