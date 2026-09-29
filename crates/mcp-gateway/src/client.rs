//! The gateway's outbound **MCP client** protocol layer (DESIGN §7.2): build the JSON-RPC 2.0
//! requests the gateway sends to a federated upstream, and parse the responses — including the
//! Streamable-HTTP `text/event-stream` framing an MCP server may reply with.
//!
//! This is transport-agnostic: it produces request bytes and consumes response bytes, with no
//! socket of its own. The daemon's HTTP transport POSTs [`initialize_request`] /
//! [`tools_list_request`] / [`tools_call_request`] to the upstream (carrying the negotiated
//! `Mcp-Session-Id`) and feeds each response body to [`parse_response_body`]. Keeping the wire
//! shaping here makes it unit-testable without a live upstream — the SSE framing especially.

use serde_json::{Value, json};

/// The MCP protocol revision the gateway proposes to upstreams by default (§7 — the revision whose
/// `tools/list_changed` a v2 client honors). A route may pin a different one.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2026-07-28";

/// The `initialize` request (MCP handshake). `client_name` identifies the gateway to the upstream.
pub fn initialize_request(id: i64, protocol_version: &str, client_name: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": protocol_version,
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") }
        }
    })
}

/// The `notifications/initialized` notification (no id) sent after a successful `initialize`.
pub fn initialized_notification() -> Value {
    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
}

/// A `tools/list` request.
pub fn tools_list_request(id: i64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} })
}

/// A `tools/call` request forwarding a call to the upstream's (original, un-namespaced) tool.
pub fn tools_call_request(id: i64, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

/// Parse an MCP response body into its JSON-RPC message object. Handles both a bare
/// `application/json` body and a Streamable-HTTP `text/event-stream` body (the JSON-RPC message
/// arrives in an SSE `data:` line). `content_type` is the response's declared type; the body is
/// also sniffed so a mislabeled stream still parses. Returns the `{jsonrpc, id, result|error}`
/// object.
pub fn parse_response_body(content_type: Option<&str>, body: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("non-utf8 response body: {e}"))?;
    let is_sse = content_type.is_some_and(|c| c.contains("text/event-stream"))
        || looks_like_sse(text);
    if is_sse {
        parse_sse_message(text).ok_or_else(|| "no JSON-RPC message in event stream".to_string())
    } else {
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid JSON response: {e}"))
    }
}

/// Whether a body looks like an SSE stream (starts, after any blank lines, with an SSE field).
fn looks_like_sse(text: &str) -> bool {
    text.lines()
        .map(str::trim_start)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.starts_with("data:") || l.starts_with("event:") || l.starts_with("id:"))
}

/// Extract the first SSE `data:` payload that parses as a JSON-RPC response (a JSON object with an
/// `id`). SSE payloads may span multiple `data:` lines within one event — those are joined.
fn parse_sse_message(text: &str) -> Option<Value> {
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            // SSE allows an optional single leading space after the colon.
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.trim().is_empty() {
            // Event boundary: try what we've accumulated, then reset for the next event.
            if let Some(v) = as_jsonrpc_response(&data) {
                return Some(v);
            }
            data.clear();
        }
    }
    as_jsonrpc_response(&data)
}

/// Parse `data` as a JSON object and keep it only if it's a JSON-RPC response (has an `id`).
fn as_jsonrpc_response(data: &str) -> Option<Value> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(trimmed).ok()?;
    v.get("id").is_some().then_some(v)
}

/// Extract the `result` from a JSON-RPC response, mapping a JSON-RPC `error` to `Err`.
pub fn rpc_result(message: &Value) -> Result<Value, String> {
    if let Some(err) = message.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(format!("upstream JSON-RPC error {code}: {msg}"));
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

/// The tool entries from a `tools/list` result (`result.tools`), or empty if absent.
pub fn tools_from_list_result(result: &Value) -> Vec<Value> {
    result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_builders_have_the_jsonrpc_shape() {
        let init = initialize_request(1, "2026-07-28", "capmesh-gateway");
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["protocolVersion"], "2026-07-28");
        assert_eq!(init["params"]["clientInfo"]["name"], "capmesh-gateway");

        assert_eq!(initialized_notification().get("id"), None); // a notification

        let list = tools_list_request(2);
        assert_eq!(list["method"], "tools/list");

        let call = tools_call_request(3, "create_task", json!({"title": "hi"}));
        assert_eq!(call["method"], "tools/call");
        assert_eq!(call["params"]["name"], "create_task");
        assert_eq!(call["params"]["arguments"]["title"], "hi");
    }

    #[test]
    fn parses_a_plain_json_response() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let msg = parse_response_body(Some("application/json"), body).unwrap();
        assert_eq!(rpc_result(&msg).unwrap()["ok"], true);
    }

    #[test]
    fn parses_a_streamable_http_event_stream_response() {
        // The shape a real MCP server (e.g. rmcp) returns: a keep-alive-ish first event, then the
        // JSON-RPC message in a later `data:` line.
        let body = b"data: \nid: 0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"create_task\"}]}}\n\n";
        let msg = parse_response_body(Some("text/event-stream"), body).unwrap();
        let result = rpc_result(&msg).unwrap();
        let tools = tools_from_list_result(&result);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "create_task");
    }

    #[test]
    fn sniffs_event_stream_even_when_content_type_is_missing() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n";
        let msg = parse_response_body(None, body).unwrap();
        assert_eq!(msg["id"], 7);
    }

    #[test]
    fn maps_a_jsonrpc_error_to_err() {
        let body = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let msg = parse_response_body(Some("application/json"), body).unwrap();
        let err = rpc_result(&msg).unwrap_err();
        assert!(err.contains("-32601"));
        assert!(err.contains("method not found"));
    }

    #[test]
    fn tools_from_list_result_is_empty_when_absent() {
        assert!(tools_from_list_result(&json!({})).is_empty());
        assert_eq!(tools_from_list_result(&json!({"tools":[{"name":"a"}]})).len(), 1);
    }
}
