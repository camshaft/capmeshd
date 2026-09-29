//! The gateway's inbound **Streamable-HTTP `/mcp` transport** (DESIGN §7.2): the single endpoint an
//! agent connects to. It is a thin shell around [`serve::handle_request`] — read one JSON-RPC
//! message, dispatch it against the shared [`Federation`], and either reply with the JSON-RPC
//! response or (for a notification) answer `202 Accepted` with no body.
//!
//! The outbound leg — actually calling a federated upstream on a `tools/call` — is injected as an
//! [`UpstreamForwarder`] in the router state, so this transport is exercised end-to-end in-process
//! (tower `oneshot`, no sockets) against a stub upstream. The daemon supplies the real forwarder
//! (an HTTP client built from [`crate::client`]) and the route-table that keeps the `Federation`
//! current.

use crate::{serve, server, Federation};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::Value;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

/// A boxed, `'static` future — a forward round-trip in flight.
pub type ForwardFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'static>>;

/// The gateway's outbound leg, injected into the transport: perform a `tools/call` against
/// `upstream_id`'s (original, de-namespaced) `tool` and return the upstream's raw JSON-RPC response
/// message, or `Err(reason)` on a transport failure. The daemon implements this with an HTTP client
/// ([`crate::client`]); tests implement it with a stub. Returns a `'static` future (the impl clones
/// what it needs) so the handler holds no borrow across the await.
pub trait UpstreamForwarder: Send + Sync + 'static {
    fn forward(&self, upstream_id: String, tool: String, arguments: Value) -> ForwardFuture;
}

/// Shared state behind the `/mcp` endpoint: the live federated tool surface (mutated as upstreams
/// (de)federate), the outbound forwarder, and a change notifier the `GET /mcp` SSE stream turns into
/// `tools/list_changed` events. Cheaply cloneable per axum's state contract.
#[derive(Clone)]
pub struct GatewayState {
    pub federation: Arc<RwLock<Federation>>,
    pub forwarder: Arc<dyn UpstreamForwarder>,
    /// Fires whenever the federated surface changes; each `GET /mcp` stream subscribes and pushes a
    /// `tools/list_changed`. The control channel ([`crate::admin`]) is the sender.
    pub notifier: broadcast::Sender<()>,
}

impl GatewayState {
    /// Build state from a federation snapshot, a forwarder, and a change notifier.
    pub fn new(
        federation: Federation,
        forwarder: Arc<dyn UpstreamForwarder>,
        notifier: broadcast::Sender<()>,
    ) -> Self {
        Self {
            federation: Arc::new(RwLock::new(federation)),
            forwarder,
            notifier,
        }
    }
}

/// The gateway's agent-facing router (Streamable-HTTP): `POST /mcp` handles each JSON-RPC message,
/// `GET /mcp` opens the server→agent SSE stream carrying `tools/list_changed` as upstreams
/// (de)federate mid-session.
pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/mcp", post(handle_mcp).get(mcp_stream))
        .with_state(state)
}

/// `GET /mcp`: the server→agent event stream. Subscribes to the change notifier and emits a
/// `notifications/tools/list_changed` SSE event whenever the federated surface changes (the agent
/// then re-issues `tools/list`). A broadcast lag also yields the notification — re-listing is safe.
async fn mcp_stream(State(state): State<GatewayState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let notification = serde_json::to_string(&server::tools_list_changed()).unwrap_or_default();
    let stream = BroadcastStream::new(state.notifier.subscribe())
        .map(move |_signal_or_lag| Ok(Event::default().data(notification.clone())));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Handle one POSTed JSON-RPC message. A malformed (non-JSON) body is rejected by the `Json`
/// extractor as `400` before reaching here.
async fn handle_mcp(State(state): State<GatewayState>, Json(req): Json<Value>) -> Response {
    // Hold the read guard across the forward: dispatch borrows the federation, and a tokio read
    // guard is `Send` (so the future stays `Send`); route updates take the write lock between calls.
    let federation = state.federation.read().await;
    let forwarder = state.forwarder.clone();
    let response = serve::handle_request(&federation, &req, move |upstream_id, tool, arguments| {
        forwarder.forward(upstream_id, tool, arguments)
    })
    .await;

    match response {
        // A JSON-RPC response (application/json is a valid Streamable-HTTP reply).
        Some(message) => (StatusCode::OK, Json(message)).into_response(),
        // A notification carried no id — nothing to return.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt; // for `oneshot`

    /// A stub upstream: records the last forwarded call and returns a canned result.
    struct StubForwarder {
        seen: std::sync::Mutex<Option<(String, String, Value)>>,
    }
    impl UpstreamForwarder for StubForwarder {
        fn forward(&self, upstream_id: String, tool: String, arguments: Value) -> ForwardFuture {
            *self.seen.lock().unwrap() = Some((upstream_id, tool, arguments));
            Box::pin(async move {
                Ok(json!({ "jsonrpc": "2.0", "id": 1,
                    "result": { "content": [{ "type": "text", "text": "ok" }] } }))
            })
        }
    }

    fn state_with(fed: Federation, stub: Arc<StubForwarder>) -> GatewayState {
        GatewayState {
            federation: Arc::new(RwLock::new(fed)),
            forwarder: stub,
            notifier: broadcast::channel(8).0,
        }
    }

    fn board_federation() -> Federation {
        let mut f = Federation::new();
        f.set_upstream_tools(
            "board",
            vec![json!({ "name": "create_task", "inputSchema": {"type":"object"} })],
        );
        f
    }

    async fn post(router: &Router, body: Value) -> (u16, Value) {
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
    async fn tools_list_is_served_over_http() {
        let stub = Arc::new(StubForwarder { seen: std::sync::Mutex::new(None) });
        let router = router(state_with(board_federation(), stub.clone()));

        let (status, body) = post(&router, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
        assert_eq!(status, 200);
        assert_eq!(body["result"]["tools"][0]["name"], "board__create_task");
        // A local reply never touches the upstream.
        assert!(stub.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_tools_call_forwards_over_http_and_returns_the_result() {
        let stub = Arc::new(StubForwarder { seen: std::sync::Mutex::new(None) });
        let router = router(state_with(board_federation(), stub.clone()));

        let (status, body) = post(&router, json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "board__create_task", "arguments": { "title": "hi" } }
        })).await;

        assert_eq!(status, 200);
        assert_eq!(body["id"], 7);
        assert_eq!(body["result"]["content"][0]["text"], "ok");
        // Forwarded to the owning upstream with the de-namespaced tool + verbatim args.
        assert_eq!(
            stub.seen.lock().unwrap().clone(),
            Some(("board".to_string(), "create_task".to_string(), json!({ "title": "hi" })))
        );
    }

    #[tokio::test]
    async fn a_notification_gets_202_and_no_body() {
        let stub = Arc::new(StubForwarder { seen: std::sync::Mutex::new(None) });
        let router = router(state_with(board_federation(), stub.clone()));

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.is_empty());
        assert!(stub.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unknown_tool_errors_without_forwarding() {
        let stub = Arc::new(StubForwarder { seen: std::sync::Mutex::new(None) });
        let router = router(state_with(board_federation(), stub.clone()));

        let (status, body) = post(&router, json!({
            "jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": { "name": "kb__search", "arguments": {} }
        })).await;
        assert_eq!(status, 200); // JSON-RPC errors ride a 200 HTTP response
        assert_eq!(body["error"]["code"], -32602);
        assert!(stub.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn get_mcp_opens_a_server_sent_event_stream() {
        let stub = Arc::new(StubForwarder { seen: std::sync::Mutex::new(None) });
        let router = router(state_with(board_federation(), stub));

        let req = Request::builder().method("GET").uri("/mcp").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        // The SSE stream is long-lived, so assert the response head only — don't drain the body.
        assert_eq!(resp.status().as_u16(), 200);
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/event-stream"), "content-type was {ct}");
    }
}
