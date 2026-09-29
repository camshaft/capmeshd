//! The gateway's **outbound HTTP forwarder** (DESIGN §7.2): the concrete [`UpstreamForwarder`] the
//! daemon plugs into the inbound `/mcp` transport ([`crate::http`]). It performs the real
//! `tools/call` round-trip to a federated upstream over Streamable-HTTP and hands the upstream's raw
//! JSON-RPC response back to the serve seam.
//!
//! The wire shaping (request builders, `text/event-stream` parsing) lives in [`crate::client`]; this
//! module only owns the transport — one `POST` per call, carrying the upstream's negotiated
//! `Mcp-Session-Id` when present. Keeping the two apart is why `client` stays fully unit-tested and
//! this module is covered against an in-process HTTP upstream (real localhost round-trip).

use crate::client::{parse_response_body, tools_call_request};
use crate::http::{ForwardFuture, UpstreamForwarder};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// A hyper client speaking plain HTTP (Streamable-HTTP upstreams on the mesh are HTTP; no TLS leg).
pub type HttpClient = Client<HttpConnector, Full<Bytes>>;

/// Build the gateway's outbound HTTP client (shared by the forwarder and the startup connect).
pub fn http_client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Where the gateway reaches one federated upstream, plus any session it has negotiated.
#[derive(Debug, Clone, Default)]
pub struct UpstreamEndpoint {
    /// The upstream's Streamable-HTTP endpoint URL (its `POST` target).
    pub url: String,
    /// The `Mcp-Session-Id` from a prior `initialize`, sent on subsequent requests when present.
    pub session_id: Option<String>,
}

impl UpstreamEndpoint {
    /// An endpoint at `url` with no session yet.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            session_id: None,
        }
    }
}

/// The HTTP [`UpstreamForwarder`]: routes each `tools/call` to its owning upstream's endpoint.
#[derive(Clone)]
pub struct HttpForwarder {
    client: HttpClient,
    endpoints: Arc<HashMap<String, UpstreamEndpoint>>,
    next_id: Arc<AtomicI64>,
}

impl HttpForwarder {
    /// A forwarder over the given upstream endpoints (keyed by the same stable id the
    /// [`crate::Federation`] uses).
    pub fn new(endpoints: HashMap<String, UpstreamEndpoint>) -> Self {
        Self {
            client: http_client(),
            endpoints: Arc::new(endpoints),
            next_id: Arc::new(AtomicI64::new(1)),
        }
    }
}

impl UpstreamForwarder for HttpForwarder {
    fn forward(&self, upstream_id: String, tool: String, arguments: Value) -> ForwardFuture {
        let Some(endpoint) = self.endpoints.get(&upstream_id).cloned() else {
            let msg = format!("no endpoint for upstream '{upstream_id}'");
            return Box::pin(async move { Err(msg) });
        };
        let client = self.client.clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let request = tools_call_request(id, &tool, arguments);
            post_jsonrpc(&client, &endpoint.url, endpoint.session_id.as_deref(), &request).await
        })
    }
}

/// `POST` one JSON-RPC message to `url` (with the `Mcp-Session-Id` header when a session exists) and
/// parse the response — a plain `application/json` body or a Streamable-HTTP `text/event-stream`
/// (via [`parse_response_body`]). Returns the upstream's raw JSON-RPC message.
pub async fn post_jsonrpc(
    client: &HttpClient,
    url: &str,
    session_id: Option<&str>,
    message: &Value,
) -> Result<Value, String> {
    post_and_session(client, url, session_id, message)
        .await
        .map(|(message, _session)| message)
}

/// Like [`post_jsonrpc`], but also returns the `Mcp-Session-Id` the upstream assigned on the
/// response (present on an `initialize` reply). The startup connect uses this to carry the session
/// into subsequent requests.
pub async fn post_and_session(
    client: &HttpClient,
    url: &str,
    session_id: Option<&str>,
    message: &Value,
) -> Result<(Value, Option<String>), String> {
    let body = serde_json::to_vec(message).map_err(|e| format!("encode request: {e}"))?;
    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(url)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json, text/event-stream");
    if let Some(sid) = session_id {
        builder = builder.header("mcp-session-id", sid);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| format!("build request: {e}"))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| format!("http request to {url} failed: {e}"))?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header("content-type");
    let session = header("mcp-session-id");
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| format!("read response body: {e}"))?
        .to_bytes();
    let message = parse_response_body(content_type.as_deref(), &bytes)?;
    Ok((message, session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::json;

    /// Spawn an in-process HTTP upstream on an ephemeral loopback port; return its base URL.
    /// `handler` receives the POSTed JSON-RPC message and returns the response to send back.
    async fn spawn_upstream(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn forwards_a_call_and_returns_the_upstream_result() {
        // Upstream echoes back a canned tools/call result, asserting it saw the de-namespaced tool.
        let app = Router::new().route(
            "/mcp",
            post(|Json(req): Json<Value>| async move {
                assert_eq!(req["method"], "tools/call");
                assert_eq!(req["params"]["name"], "create_task");
                Json(json!({ "jsonrpc": "2.0", "id": req["id"],
                    "result": { "content": [{ "type": "text", "text": "created" }] } }))
            }),
        );
        let base = spawn_upstream(app).await;

        let mut endpoints = HashMap::new();
        endpoints.insert("board".to_string(), UpstreamEndpoint::new(format!("{base}/mcp")));
        let fwd = HttpForwarder::new(endpoints);

        let msg = fwd
            .forward("board".into(), "create_task".into(), json!({ "title": "hi" }))
            .await
            .unwrap();
        assert_eq!(msg["result"]["content"][0]["text"], "created");
    }

    #[tokio::test]
    async fn parses_a_streamable_http_event_stream_upstream() {
        // Upstream answers as SSE (text/event-stream), the shape a real rmcp server may use.
        let app = Router::new().route(
            "/mcp",
            post(|Json(_req): Json<Value>| async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
                )
            }),
        );
        let base = spawn_upstream(app).await;

        let mut endpoints = HashMap::new();
        endpoints.insert("kb".to_string(), UpstreamEndpoint::new(format!("{base}/mcp")));
        let fwd = HttpForwarder::new(endpoints);

        let msg = fwd.forward("kb".into(), "search".into(), json!({})).await.unwrap();
        assert_eq!(msg["result"]["ok"], true);
    }

    #[tokio::test]
    async fn an_unknown_upstream_is_a_transport_error() {
        let fwd = HttpForwarder::new(HashMap::new());
        let err = fwd.forward("nope".into(), "t".into(), json!({})).await.unwrap_err();
        assert!(err.contains("no endpoint for upstream 'nope'"));
    }

    #[tokio::test]
    async fn sends_the_session_header_when_present() {
        let app = Router::new().route(
            "/mcp",
            post(|headers: axum::http::HeaderMap, Json(_): Json<Value>| async move {
                let sid = headers
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                Json(json!({ "jsonrpc": "2.0", "id": 1, "result": { "sid": sid } }))
            }),
        );
        let base = spawn_upstream(app).await;

        let mut endpoints = HashMap::new();
        endpoints.insert(
            "board".to_string(),
            UpstreamEndpoint {
                url: format!("{base}/mcp"),
                session_id: Some("sess-123".to_string()),
            },
        );
        let fwd = HttpForwarder::new(endpoints);

        let msg = fwd.forward("board".into(), "create_task".into(), json!({})).await.unwrap();
        assert_eq!(msg["result"]["sid"], "sess-123");
    }
}
