//! The gateway's **serve seam** (DESIGN §7.2): tie the pure inbound [`dispatch`](crate::server)
//! and an injected async upstream forward into the single JSON-RPC response the gateway returns to
//! the agent.
//!
//! This is where the two pure cores meet the network without embedding it: [`handle_request`] runs
//! [`dispatch`], answers a [`ServerAction::Reply`] itself, drops a [`ServerAction::Ignore`], and for
//! a [`ServerAction::Forward`] awaits the caller-supplied forwarder — the only thing that touches a
//! socket (the daemon's outbound HTTP client, built from [`crate::client`]). Keeping the forward
//! injected lets the whole gateway pipeline be unit-tested against stub upstreams, so the daemon's
//! HTTP transport is a thin wrapper: read a request body, `handle_request`, write the response.

use crate::server::{ServerAction, dispatch};
use crate::Federation;
use serde_json::{Value, json};
use std::future::Future;

/// Handle one inbound JSON-RPC message against `federation`, forwarding a `tools/call` through
/// `forward`. Returns the JSON-RPC response to send back, or `None` for a notification (no reply).
///
/// `forward(upstream_id, tool, arguments)` performs the actual `tools/call` round-trip to the owning
/// upstream and returns its **raw JSON-RPC response message** (`{jsonrpc, id, result|error}`), or an
/// `Err(reason)` if the transport itself failed. The upstream's `result` (including a tool-level
/// `isError` payload) or its JSON-RPC `error` is passed back to the agent verbatim — only the `id`
/// is rewritten to the agent's original request id. A transport failure becomes an internal error
/// (`-32603`) so the agent always gets a well-formed response to its id.
pub async fn handle_request<F, Fut>(federation: &Federation, req: &Value, forward: F) -> Option<Value>
where
    F: FnOnce(String, String, Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    match dispatch(federation, req) {
        ServerAction::Reply(v) => Some(v),
        ServerAction::Ignore => None,
        ServerAction::Forward {
            request_id,
            upstream_id,
            tool,
            arguments,
        } => Some(match forward(upstream_id, tool, arguments).await {
            Ok(upstream_msg) => reframe(request_id, &upstream_msg),
            Err(reason) => internal_error(request_id, &format!("upstream call failed: {reason}")),
        }),
    }
}

/// Rebuild the upstream's response under the agent's original `id`: pass its `error` through if
/// present, else its `result` (defaulting to `null`). This preserves a tool-level error result
/// (`result.isError`) as well as a protocol-level JSON-RPC `error` — the gateway is a conduit, not
/// an interpreter, of the upstream's answer.
fn reframe(id: Value, upstream: &Value) -> Value {
    if let Some(error) = upstream.get("error") {
        json!({ "jsonrpc": "2.0", "id": id, "error": error })
    } else {
        let result = upstream.get("result").cloned().unwrap_or(Value::Null);
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }
}

fn internal_error(id: Value, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn federation() -> Federation {
        let mut f = Federation::new();
        f.set_upstream_tools(
            "board",
            vec![json!({ "name": "create_task", "inputSchema": {"type":"object"} })],
        );
        f
    }

    /// A forwarder that must never be called (a Reply/Ignore path). Panics if invoked.
    fn no_forward(_: String, _: String, _: Value) -> std::future::Ready<Result<Value, String>> {
        panic!("forward must not be called for a locally-answered message");
    }

    #[tokio::test]
    async fn a_reply_is_answered_locally_without_forwarding() {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let out = handle_request(&federation(), &req, no_forward).await.unwrap();
        assert_eq!(out["result"]["tools"][0]["name"], "board__create_task");
    }

    #[tokio::test]
    async fn a_notification_yields_no_response_and_no_forward() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle_request(&federation(), &note, no_forward).await.is_none());
    }

    #[tokio::test]
    async fn an_unknown_tool_errors_locally_without_forwarding() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "kb__search", "arguments": {} } });
        let out = handle_request(&federation(), &req, no_forward).await.unwrap();
        assert_eq!(out["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn a_known_call_forwards_the_original_tool_and_reframes_the_result() {
        let seen = Cell::new(None);
        let req = json!({ "jsonrpc": "2.0", "id": 42, "method": "tools/call",
            "params": { "name": "board__create_task", "arguments": { "title": "hi" } } });

        let out = handle_request(&federation(), &req, |up, tool, args| {
            seen.set(Some((up, tool, args)));
            // The upstream answers with its own id (7) — the gateway must rewrite it to 42.
            std::future::ready(Ok(json!({
                "jsonrpc": "2.0", "id": 7,
                "result": { "content": [{ "type": "text", "text": "created" }] }
            })))
        })
        .await
        .unwrap();

        // Forwarded to the owning upstream with the DE-namespaced tool + verbatim args.
        assert_eq!(
            seen.into_inner(),
            Some(("board".to_string(), "create_task".to_string(), json!({ "title": "hi" })))
        );
        // Result passed through, re-ided to the agent's request id.
        assert_eq!(out["id"], 42);
        assert_eq!(out["result"]["content"][0]["text"], "created");
    }

    #[tokio::test]
    async fn an_upstream_error_is_passed_through_under_the_original_id() {
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "board__create_task", "arguments": {} } });
        let out = handle_request(&federation(), &req, |_, _, _| {
            std::future::ready(Ok(json!({
                "jsonrpc": "2.0", "id": 1,
                "error": { "code": -32602, "message": "missing title" }
            })))
        })
        .await
        .unwrap();
        assert_eq!(out["id"], 5);
        assert_eq!(out["error"]["code"], -32602);
        assert_eq!(out["error"]["message"], "missing title");
    }

    #[tokio::test]
    async fn a_transport_failure_becomes_an_internal_error_to_the_original_id() {
        let req = json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "board__create_task", "arguments": {} } });
        let out = handle_request(&federation(), &req, |_, _, _| {
            std::future::ready(Err("connection refused".to_string()))
        })
        .await
        .unwrap();
        assert_eq!(out["id"], 9);
        assert_eq!(out["error"]["code"], -32603);
        assert!(out["error"]["message"].as_str().unwrap().contains("connection refused"));
    }
}
