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
//! - `set_view` — focus which existing item the main view shows (or clear it).
//! - `remove_item` — prune one item from the inbox by id.
//! - `clear_surface` / `delete_surface` — empty, or delete, a surface.
//!
//! This module is transport-agnostic: [`dispatch`] maps one JSON-RPC message to
//! its response (or `None` for a notification). The HTTP glue + the optional
//! bearer-token gate live in [`crate::http`].

use serde_json::{Value, json};

use crate::inbox::{SurfaceStore, broadcast_view};
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
    {\"type\":\"pdf\",\"url\":\"…\",\"page\":N?} (optional 1-based `page` deep-links into the PDF), \
    {\"type\":\"text\",\"body\":\"…\"}, \
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
        },
        {
            "name": "clear_surface",
            "description": "Clear a surface's inbox — drop all items and reset the main view (the surface itself stays).",
            "inputSchema": {
                "type": "object",
                "properties": { "surface-id": { "type": "string" } },
                "required": ["surface-id"]
            }
        },
        {
            "name": "set_view",
            "description": "Focus which existing item the surface's main view shows, without pushing a new item. `item-id` is the id from list_items; omit or null to clear the main view. Errors if the surface or item is unknown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "surface-id": { "type": "string" },
                    "item-id": { "type": "string" }
                },
                "required": ["surface-id"]
            }
        },
        {
            "name": "remove_item",
            "description": "Remove one item from a surface's inbox by id (prune a stale item without clearing the whole surface). `item-id` is the id from list_items.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "surface-id": { "type": "string" },
                    "item-id": { "type": "string" }
                },
                "required": ["surface-id", "item-id"]
            }
        },
        {
            "name": "delete_surface",
            "description": "Delete a surface entirely (its inbox and on-disk log).",
            "inputSchema": {
                "type": "object",
                "properties": { "surface-id": { "type": "string" } },
                "required": ["surface-id"]
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
        "set_view" => set_view(store, &args),
        "remove_item" => remove_item(store, &args),
        "clear_surface" => match surface_id(&args) {
            Ok(id) => {
                store.clear(&id);
                tool_text(format!("cleared surface '{id}'"))
            }
            Err(e) => tool_error(e),
        },
        "delete_surface" => match surface_id(&args) {
            Ok(id) => {
                store.delete(&id);
                tool_text(format!("deleted surface '{id}'"))
            }
            Err(e) => tool_error(e),
        },
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

fn set_view(store: &SurfaceStore, args: &Value) -> Value {
    let id = match surface_id(args) {
        Ok(id) => id,
        Err(e) => return tool_error(e),
    };
    let item_id = args.get("item-id").and_then(Value::as_str);
    match store.set_view(&id, item_id) {
        Some(tx) => {
            // Fan the focus change out to attached tabs (same as the control path).
            broadcast_view(&tx, item_id.map(str::to_string));
            match item_id {
                Some(i) => tool_text(format!("main view set to item {i} on '{id}'")),
                None => tool_text(format!("cleared main view on '{id}'")),
            }
        }
        None => tool_error(format!("no such surface or item on '{id}'")),
    }
}

fn remove_item(store: &SurfaceStore, args: &Value) -> Value {
    let id = match surface_id(args) {
        Ok(id) => id,
        Err(e) => return tool_error(e),
    };
    let Some(item_id) = args.get("item-id").and_then(Value::as_str) else {
        return tool_error("remove_item requires 'item-id'".to_string());
    };
    if store.remove_item(&id, item_id) {
        tool_text(format!("removed item {item_id} from surface '{id}'"))
    } else {
        tool_error(format!("no such item '{item_id}' on surface '{id}'"))
    }
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
    fn tools_list_names_the_tools() {
        let store = SurfaceStore::in_memory();
        let out = dispatch(&store, &req(2, "tools/list", json!({}))).unwrap();
        let names: Vec<String> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "list_surfaces",
            "list_items",
            "send_item",
            "set_view",
            "remove_item",
            "clear_surface",
            "delete_surface",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
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
                "item":{"type":"pdf","url":"https://x/m.pdf","page":348}}}),
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
        // The pdf came through with its deep-link page (MCP carries the full item).
        let items_text = li["result"]["content"][0]["text"].as_str().unwrap();
        assert!(items_text.contains("pdf"));
        assert!(items_text.contains("\"page\": 348"));
    }

    #[test]
    fn set_view_tool_focuses_and_clears() {
        let store = SurfaceStore::in_memory();
        // Two items; b is promoted (the current view). Focus a via set_view.
        let a = store.push("phone", crate::item::DisplayItem::Text { body: "a".into() }, false);
        store.push("phone", crate::item::DisplayItem::Text { body: "b".into() }, true);
        let out = dispatch(
            &store,
            &req(2, "tools/call", json!({"name":"set_view",
                "arguments":{"surface-id":"phone","item-id":a.entry.id}})),
        )
        .unwrap();
        assert!(out["result"]["isError"].as_bool() != Some(true));
        assert_eq!(store.snapshot("phone").current_view.as_deref(), Some(a.entry.id.as_str()));

        // Clearing (no item-id) resets the view to none.
        dispatch(&store, &req(3, "tools/call", json!({"name":"set_view","arguments":{"surface-id":"phone"}}))).unwrap();
        assert_eq!(store.snapshot("phone").current_view, None);

        // An unknown item is a tool error.
        let bad = dispatch(
            &store,
            &req(4, "tools/call", json!({"name":"set_view","arguments":{"surface-id":"phone","item-id":"nope"}})),
        )
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
    }

    #[test]
    fn remove_item_tool_prunes_one() {
        let store = SurfaceStore::in_memory();
        let pushed = store.push("phone", crate::item::DisplayItem::Text { body: "a".into() }, true);
        let out = dispatch(
            &store,
            &req(2, "tools/call", json!({"name":"remove_item",
                "arguments":{"surface-id":"phone","item-id":pushed.entry.id}})),
        )
        .unwrap();
        assert!(out["result"]["isError"].as_bool() != Some(true));
        assert_eq!(store.snapshot("phone").items.len(), 0);
        // Removing an unknown item is a tool error.
        let bad = dispatch(
            &store,
            &req(3, "tools/call", json!({"name":"remove_item",
                "arguments":{"surface-id":"phone","item-id":"nope"}})),
        )
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
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
