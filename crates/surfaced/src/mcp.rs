//! An embedded **MCP server** so an agent can drive surfaces (DESIGN §7 `send`).
//!
//! surfaced exposes a Model Context Protocol endpoint at `/mcp` (Streamable HTTP:
//! the agent POSTs JSON-RPC 2.0 messages and gets a JSON response). It shares the
//! live [`SurfaceStore`], so an agent's `send_item` lands on the same surface a
//! browser is attached to and fans out immediately.
//!
//! Tools:
//! - `list_surfaces` — every registered surface (id, title, item count, view).
//! - `list_items` — the display items on one surface.
//! - `send_item` — post a display item to a surface (creating it if new).
//!
//! This module is transport-agnostic: [`dispatch`] maps one JSON-RPC message to
//! its response (or `None` for a notification). The HTTP glue + the optional
//! bearer-token gate live in [`crate::http`].

use serde_json::{Value, json};

use crate::inbox::SurfaceStore;
use crate::item::DisplayItem;

/// The MCP protocol version this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Handle one JSON-RPC 2.0 message. Returns `Some(response)` for a request (has
/// an `id`) and `None` for a notification (e.g. `notifications/initialized`).
pub fn dispatch(store: &SurfaceStore, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    // Notifications carry no id and get no reply.
    let id = id?;

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize_result()),
        "tools/list" => Ok(tools_list()),
        "tools/call" => Ok(tools_call(store, &params)),
        "ping" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "surfaced", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn tools_list() -> Value {
    json!({ "tools": [
        {
            "name": "list_surfaces",
            "description": "List all registered surfaces on this daemon (id, title, item count, current view).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_items",
            "description": "List the display items currently on a surface's inbox.",
            "inputSchema": {
                "type": "object",
                "properties": { "surface-id": { "type": "string" } },
                "required": ["surface-id"]
            }
        },
        {
            "name": "send_item",
            "description": "Post a display item to a surface (created if new). `item` is one of: \
    {\"type\":\"pdf\",\"url\":\"…\"}, {\"type\":\"text\",\"body\":\"…\"}, \
    {\"type\":\"link\",\"url\":\"…\",\"title\":\"…\"}, {\"type\":\"navigate\",\"url\":\"…\"}, \
    {\"type\":\"html\",\"markup\":\"…\"}, or {\"type\":\"script\",\"code\":\"…\"}. \
    `promote` (default true) also shows it in the main view.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "surface-id": { "type": "string" },
                    "item": { "type": "object" },
                    "promote": { "type": "boolean" }
                },
                "required": ["surface-id", "item"]
            }
        }
    ]})
}

fn tools_call(store: &SurfaceStore, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "list_surfaces" => ok_json(&store.list_surfaces()),
        "list_items" => match surface_id(&args) {
            Ok(id) => ok_json(&store.snapshot(&id)),
            Err(e) => tool_error(e),
        },
        "send_item" => send_item(store, &args),
        other => tool_error(format!("unknown tool '{other}'")),
    }
}

fn send_item(store: &SurfaceStore, args: &Value) -> Value {
    let id = match surface_id(args) {
        Ok(id) => id,
        Err(e) => return tool_error(e),
    };
    let Some(item_val) = args.get("item") else {
        return tool_error("send_item requires 'item'".to_string());
    };
    let item: DisplayItem = match serde_json::from_value(item_val.clone()) {
        Ok(i) => i,
        Err(e) => return tool_error(format!("invalid item: {e}")),
    };
    let promote = args.get("promote").and_then(Value::as_bool).unwrap_or(true);
    let pushed = store.push(&id, item, promote);
    pushed.broadcast();
    tool_text(format!(
        "posted item {} to surface '{}'",
        pushed.entry.id, id
    ))
}

/// Extract and validate a `surface-id` argument.
fn surface_id(args: &Value) -> Result<String, String> {
    let id = args
        .get("surface-id")
        .and_then(Value::as_str)
        .ok_or_else(|| "requires 'surface-id'".to_string())?;
    if !SurfaceStore::valid_id(id) {
        return Err(format!("invalid surface-id '{id}'"));
    }
    Ok(id.to_string())
}

/// A tool result whose text content is the pretty-printed JSON of `value`.
fn ok_json<T: serde::Serialize>(value: &T) -> Value {
    let text =
        serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("serialize error: {e}"));
    tool_text(text)
}

fn tool_text(text: String) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ] })
}

fn tool_error(message: String) -> Value {
    json!({ "content": [ { "type": "text", "text": message } ], "isError": true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let store = SurfaceStore::in_memory();
        let out = dispatch(&store, &req(1, "initialize", json!({}))).unwrap();
        assert_eq!(out["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(out["result"]["serverInfo"]["name"], "surfaced");
        assert!(out["result"]["capabilities"].get("tools").is_some());
    }

    #[test]
    fn notifications_get_no_response() {
        let store = SurfaceStore::in_memory();
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(dispatch(&store, &note).is_none());
    }

    #[test]
    fn tools_list_names_the_three_tools() {
        let store = SurfaceStore::in_memory();
        let out = dispatch(&store, &req(2, "tools/list", json!({}))).unwrap();
        let names: Vec<String> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"list_surfaces".to_string()));
        assert!(names.contains(&"list_items".to_string()));
        assert!(names.contains(&"send_item".to_string()));
    }

    #[test]
    fn send_item_then_list_surfaces_and_items() {
        let store = Arc::new(SurfaceStore::in_memory());
        // send_item posts to a (new) surface.
        let call = req(
            3,
            "tools/call",
            json!({"name":"send_item","arguments":{
                "surface-id":"phone",
                "item":{"type":"pdf","url":"https://x/m.pdf"}}}),
        );
        let out = dispatch(&store, &call).unwrap();
        assert!(out["result"]["isError"].as_bool() != Some(true));
        assert!(
            out["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("posted item")
        );

        // list_surfaces sees it.
        let ls = dispatch(
            &store,
            &req(
                4,
                "tools/call",
                json!({"name":"list_surfaces","arguments":{}}),
            ),
        )
        .unwrap();
        let text = ls["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("phone"));
        assert!(text.contains("\"item-count\": 1"));

        // list_items returns the pdf.
        let li = dispatch(
            &store,
            &req(
                5,
                "tools/call",
                json!({"name":"list_items","arguments":{"surface-id":"phone"}}),
            ),
        )
        .unwrap();
        assert!(
            li["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("pdf")
        );
    }

    #[test]
    fn tool_errors_are_reported_as_tool_results() {
        let store = SurfaceStore::in_memory();
        // Bad surface id.
        let bad = dispatch(
            &store,
            &req(6, "tools/call", json!({"name":"send_item","arguments":{"surface-id":"../x","item":{"type":"text","body":"hi"}}})),
        )
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
        // Unknown tool.
        let unk = dispatch(
            &store,
            &req(7, "tools/call", json!({"name":"nope","arguments":{}})),
        )
        .unwrap();
        assert_eq!(unk["result"]["isError"], true);
        // Unknown method → JSON-RPC error.
        let m = dispatch(&store, &req(8, "frobnicate", json!({}))).unwrap();
        assert_eq!(m["error"]["code"], -32601);
    }
}
