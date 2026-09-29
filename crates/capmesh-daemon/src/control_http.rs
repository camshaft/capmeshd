//! capmeshd's embedded **control-tools MCP server** HTTP transport (DESIGN §7.1): serves `POST /mcp`
//! with capmesh's own control tools (discover/describe/connect/disconnect/status), so the gateway
//! federates capmesh itself as a `cap=mcp` upstream.
//!
//! A thin shell around [`control_mcp::dispatch`](crate::control_mcp): read one JSON-RPC message,
//! answer a [`ControlAction::Reply`] locally (initialize / `tools/list` / ping / errors), drop a
//! notification, and for a [`ControlAction::Call`] run it through an injected [`ControlExecutor`] —
//! the only thing that touches the control plane — then wrap its result as the JSON-RPC response.
//! Injecting the executor keeps the transport unit-tested (tower `oneshot`, no sockets) against a
//! stub; the capmeshd binary supplies the real executor (discover/connect/… over `capmesh-ctl`).

use crate::control_mcp::{dispatch, ControlAction, ControlCall};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed, `'static` future — a control tool-call executing.
pub type ExecFuture = Pin<Box<dyn Future<Output = Value> + Send + 'static>>;

/// Executes a parsed [`ControlCall`] against capmesh's control plane and returns the MCP
/// `tools/call` **result** (the `{ content, … }` value). A tool-level failure is returned as an
/// error result (content + `isError: true`), not a JSON-RPC transport error — the control tools
/// always produce a result. The daemon supplies the real impl; tests supply a stub.
pub trait ControlExecutor: Send + Sync + 'static {
    fn execute(&self, call: ControlCall) -> ExecFuture;
}

/// Shared state behind the control server's `/mcp` endpoint.
#[derive(Clone)]
pub struct ControlState {
    pub executor: Arc<dyn ControlExecutor>,
}

impl ControlState {
    /// State wrapping the given executor.
    pub fn new(executor: Arc<dyn ControlExecutor>) -> Self {
        Self { executor }
    }
}

/// The control-tools MCP router: `POST /mcp` (Streamable-HTTP), mirroring the gateway's inbound
/// transport shape.
pub fn control_mcp_router(state: ControlState) -> Router {
    Router::new().route("/mcp", post(handle)).with_state(state)
}

async fn handle(State(state): State<ControlState>, Json(req): Json<Value>) -> Response {
    match dispatch(&req) {
        ControlAction::Reply(message) => (StatusCode::OK, Json(message)).into_response(),
        ControlAction::Ignore => StatusCode::ACCEPTED.into_response(),
        ControlAction::Call { request_id, call } => {
            let result = state.executor.execute(call).await;
            let response = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
            (StatusCode::OK, Json(response)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::Mutex;
    use tower::ServiceExt; // for `oneshot`

    /// A stub executor: records the last call and returns a canned result.
    struct StubExecutor {
        seen: Mutex<Option<ControlCall>>,
    }
    impl ControlExecutor for StubExecutor {
        fn execute(&self, call: ControlCall) -> ExecFuture {
            *self.seen.lock().unwrap() = Some(call);
            Box::pin(async move {
                json!({ "content": [{ "type": "text", "text": "done" }] })
            })
        }
    }

    fn router_with(stub: Arc<StubExecutor>) -> Router {
        control_mcp_router(ControlState::new(stub))
    }

    async fn post_msg(router: &Router, body: Value) -> (u16, Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn initialize_and_tools_list_are_answered_locally() {
        let stub = Arc::new(StubExecutor { seen: Mutex::new(None) });
        let router = router_with(stub.clone());

        let (s, init) = post_msg(&router, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await;
        assert_eq!(s, 200);
        assert_eq!(init["result"]["serverInfo"]["name"], "capmesh-control");

        let (s, list) = post_msg(&router, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert_eq!(s, 200);
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["discover", "describe", "connect", "disconnect", "status"]);
        // Local replies never touch the executor.
        assert!(stub.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_tools_call_runs_the_executor_and_wraps_the_result() {
        let stub = Arc::new(StubExecutor { seen: Mutex::new(None) });
        let router = router_with(stub.clone());

        let (s, body) = post_msg(&router, json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "discover", "arguments": { "kind": "midi" } }
        })).await;

        assert_eq!(s, 200);
        assert_eq!(body["id"], 7);
        assert_eq!(body["result"]["content"][0]["text"], "done");
        // The executor saw the parsed Discover call.
        assert_eq!(
            *stub.seen.lock().unwrap(),
            Some(ControlCall::Discover {
                kind: Some("midi".into()),
                dir: None,
                host: None,
                timeout_secs: None,
            })
        );
    }

    #[tokio::test]
    async fn a_notification_gets_202_and_an_unknown_tool_errors_locally() {
        let stub = Arc::new(StubExecutor { seen: Mutex::new(None) });
        let router = router_with(stub.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 202);

        let (s, body) = post_msg(&router, json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "frobnicate", "arguments": {} }
        })).await;
        assert_eq!(s, 200);
        assert_eq!(body["error"]["code"], -32602);
        // Neither path invoked the executor.
        assert!(stub.seen.lock().unwrap().is_none());
    }
}
