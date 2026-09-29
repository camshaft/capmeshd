//! The gateway's **control channel** (DESIGN §7.2): a small admin HTTP API that capmeshd drives to
//! (de)federate upstreams on the running gateway. capmeshd's `McpRouteRegistry`, on a
//! `register`/`unregister`, calls these routes; the gateway connects/drops the upstream via
//! [`daemon::federate`]/[`daemon::defederate`] against the *live* [`Federation`] + [`HttpForwarder`]
//! the `/mcp` server ([`crate::http`]) is serving — so the change takes effect mid-session.
//!
//! Mirrors capmeshd's own register-endpoint shape (axum + `deny_unknown_fields` body, in-process
//! `oneshot` tests). Loopback-only in deployment: this mutates control state, it is not the
//! agent-facing endpoint. The `changed` flag each call returns is what a later slice turns into a
//! `tools/list_changed` push to connected agents.

use crate::daemon::{defederate, federate};
use crate::forward::{HttpClient, HttpForwarder};
use crate::{client, Federation, NS_SEP};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Shared state behind the admin API: the outbound client + the same live federation and forwarder
/// the `/mcp` server uses (so a (de)federate is visible to agents immediately), plus the change
/// notifier the `GET /mcp` SSE streams push `tools/list_changed` from.
#[derive(Clone)]
pub struct AdminState {
    pub client: HttpClient,
    pub federation: Arc<RwLock<Federation>>,
    pub forwarder: Arc<HttpForwarder>,
    pub notifier: broadcast::Sender<()>,
}

impl AdminState {
    /// Signal connected `GET /mcp` streams that the federated surface changed. Best-effort: a send
    /// with no live subscribers is a no-op (agents re-list on their next connect anyway).
    fn notify_changed(&self) {
        let _ = self.notifier.send(());
    }
}

/// A `federate` request body — an upstream to connect + merge. `transport` is accepted for parity
/// with the route registry but only `streamable-http` is served today.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederateRequest {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(rename = "protocol-rev", default)]
    pub protocol_rev: Option<String>,
}

/// The gateway control API (loopback): `GET /admin/upstreams` lists federated ids, `POST` federates
/// one, `DELETE /admin/upstreams/{id}` defederates.
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/upstreams", get(list_upstreams).post(federate_upstream))
        .route("/admin/upstreams/{id}", axum::routing::delete(defederate_upstream))
        .with_state(state)
}

async fn list_upstreams(State(state): State<AdminState>) -> Response {
    let ids: Vec<String> = state
        .federation
        .read()
        .await
        .upstream_ids()
        .into_iter()
        .map(str::to_string)
        .collect();
    (StatusCode::OK, Json(json!({ "upstreams": ids }))).into_response()
}

async fn federate_upstream(State(state): State<AdminState>, Json(req): Json<FederateRequest>) -> Response {
    // A `__`-containing id is a client error (it would break tools/list namespacing).
    if req.id.contains(NS_SEP) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("id '{}' must not contain '{NS_SEP}'", req.id) })),
        )
            .into_response();
    }
    let protocol = req
        .protocol_rev
        .as_deref()
        .unwrap_or(client::DEFAULT_PROTOCOL_VERSION);
    match federate(&state.client, &state.federation, &state.forwarder, &req.id, &req.url, protocol).await {
        // A successful connect: on a real surface change, push tools/list_changed to live streams.
        Ok(changed) => {
            if changed {
                state.notify_changed();
            }
            (StatusCode::OK, Json(json!({ "id": req.id, "changed": changed }))).into_response()
        }
        // The upstream could not be reached / handshaked — a bad gateway.
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
    }
}

async fn defederate_upstream(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    let changed = defederate(&state.federation, &state.forwarder, &id).await;
    if changed {
        state.notify_changed();
    }
    (StatusCode::OK, Json(json!({ "id": id, "changed": changed }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::http_client;
    use axum::body::Body;
    use axum::http::Request;
    use axum::{routing::post as axpost, Json as AxJson};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::collections::HashMap;
    use tower::ServiceExt; // for `oneshot`

    /// An in-process MCP upstream answering initialize / tools/list / tools/call.
    async fn spawn_upstream(tool: &'static str) -> String {
        let app = Router::new().route(
            "/mcp",
            axpost(move |AxJson(req): AxJson<Value>| async move {
                let id = req.get("id").cloned();
                match req["method"].as_str().unwrap_or("") {
                    "initialize" => (
                        [("mcp-session-id", "sess-1")],
                        AxJson(json!({ "jsonrpc": "2.0", "id": id,
                            "result": { "protocolVersion": "2026-07-28", "capabilities": {} } })),
                    )
                        .into_response(),
                    "tools/list" => AxJson(json!({ "jsonrpc": "2.0", "id": id,
                        "result": { "tools": [{ "name": tool, "inputSchema": {"type":"object"} }] } }))
                    .into_response(),
                    _ => StatusCode::ACCEPTED.into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/mcp")
    }

    fn state() -> AdminState {
        AdminState {
            client: http_client(),
            federation: Arc::new(RwLock::new(Federation::new())),
            forwarder: Arc::new(HttpForwarder::new(HashMap::new())),
            notifier: broadcast::channel(8).0,
        }
    }

    async fn call(router: &Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let builder = Request::builder().method(method).uri(path);
        let req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn federate_list_and_defederate_over_the_admin_api() {
        let url = spawn_upstream("create_task").await;
        let st = state();
        let router = admin_router(st.clone());

        // Federate → 200 changed:true, and the live federation now carries it.
        let (s, b) = call(&router, "POST", "/admin/upstreams", Some(json!({ "id": "board", "url": url }))).await;
        assert_eq!(s, 200);
        assert_eq!(b["changed"], true);
        assert_eq!(
            st.federation.read().await.merged_tools()[0]["name"],
            "board__create_task"
        );

        // List → the federated id.
        let (s, b) = call(&router, "GET", "/admin/upstreams", None).await;
        assert_eq!(s, 200);
        assert_eq!(b["upstreams"], json!(["board"]));

        // Defederate → 200 changed:true, federation empties.
        let (s, b) = call(&router, "DELETE", "/admin/upstreams/board", None).await;
        assert_eq!(s, 200);
        assert_eq!(b["changed"], true);
        assert!(st.federation.read().await.upstream_ids().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_a_bad_gateway() {
        let router = admin_router(state());
        let (s, b) = call(
            &router,
            "POST",
            "/admin/upstreams",
            Some(json!({ "id": "dead", "url": "http://127.0.0.1:1/mcp" })),
        )
        .await;
        assert_eq!(s, 502);
        assert!(b["error"].as_str().unwrap().contains("http request"));
    }

    #[tokio::test]
    async fn a_surface_change_signals_the_notifier() {
        let url = spawn_upstream("create_task").await;
        let st = state();
        let mut rx = st.notifier.subscribe();
        let router = admin_router(st.clone());

        // A federate that changes the surface fires the notifier (→ GET /mcp pushes list_changed).
        let (s, _) = call(&router, "POST", "/admin/upstreams", Some(json!({ "id": "board", "url": url.clone() }))).await;
        assert_eq!(s, 200);
        assert!(rx.try_recv().is_ok(), "federate should have signalled a surface change");

        // Re-federating identical tools does not signal (no change).
        let (s, b) = call(&router, "POST", "/admin/upstreams", Some(json!({ "id": "board", "url": url }))).await;
        assert_eq!(s, 200);
        assert_eq!(b["changed"], false);
        assert!(rx.try_recv().is_err(), "an unchanged re-federate must not signal");

        // Defederate changes the surface → signals again.
        call(&router, "DELETE", "/admin/upstreams/board", None).await;
        assert!(rx.try_recv().is_ok(), "defederate should have signalled a surface change");
    }

    #[tokio::test]
    async fn a_namespaced_id_is_a_bad_request() {
        let router = admin_router(state());
        let (s, _b) = call(
            &router,
            "POST",
            "/admin/upstreams",
            Some(json!({ "id": "bad__id", "url": "http://127.0.0.1:9/mcp" })),
        )
        .await;
        assert_eq!(s, 400);
    }
}
