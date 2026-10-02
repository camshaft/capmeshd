//! The gateway daemon's **startup federation build** (DESIGN §7.2): connect each configured upstream
//! and assemble the [`Federation`] the `/mcp` transport serves, plus the [`UpstreamEndpoint`] map the
//! [`HttpForwarder`] routes `tools/call`s through.
//!
//! Connecting an upstream is the MCP client handshake over [`crate::forward`]: `initialize` (capture
//! the assigned `Mcp-Session-Id`), the `notifications/initialized` note, then `tools/list`. It is
//! **best-effort per upstream** — one unreachable or misbehaving server is logged and skipped so the
//! gateway still serves the rest (and can pick the straggler up later when capmeshd re-drives the
//! route). The `main` binary parses [`GatewayConfig`], calls [`connect_upstreams`], builds
//! [`GatewayState`](crate::http::GatewayState), and serves [`crate::http::router`].

use crate::client::{
    DEFAULT_PROTOCOL_VERSION, initialize_request, initialized_notification, rpc_result,
    tools_from_list_result, tools_list_request,
};
use crate::config::UpstreamConfig;
use crate::forward::{HttpClient, HttpForwarder, UpstreamEndpoint, post_and_session, post_jsonrpc};
use crate::{Federation, NS_SEP};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tracing::{info, warn};

/// The client name the gateway presents to upstreams on `initialize`.
const GATEWAY_CLIENT_NAME: &str = "capmesh-gateway";

/// Default interval between startup-retry sweeps of the still-pending upstreams.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Default number of startup-retry sweeps before the gateway gives up on a straggler upstream
/// (`RETRY_INTERVAL` × this ≈ the window an upstream has to come up after the gateway).
pub const RETRY_MAX_ATTEMPTS: usize = 60;

/// Connect every configured upstream into a [`Federation`] and an endpoint map (best-effort). An
/// upstream that fails to connect — or whose id contains the `__` namespace separator — is logged
/// and skipped.
pub async fn connect_upstreams(
    client: &HttpClient,
    upstreams: &[UpstreamConfig],
) -> (Federation, HashMap<String, UpstreamEndpoint>) {
    let mut federation = Federation::new();
    let mut endpoints = HashMap::new();
    for up in upstreams {
        if up.id.contains(NS_SEP) {
            warn!(id = %up.id, "skipping upstream: id must not contain '__' (namespace separator)");
            continue;
        }
        let protocol = up
            .protocol_rev
            .as_deref()
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        match connect_one(client, &up.url, protocol).await {
            Ok((tools, session_id)) => {
                info!(id = %up.id, url = %up.url, tools = tools.len(), "federated upstream");
                federation.set_upstream_tools(&up.id, tools);
                endpoints.insert(
                    up.id.clone(),
                    UpstreamEndpoint {
                        url: up.url.clone(),
                        session_id,
                    },
                );
            }
            Err(e) => warn!(id = %up.id, url = %up.url, "skipping upstream: {e}"),
        }
    }
    (federation, endpoints)
}

/// The MCP client handshake against one upstream: `initialize` (capturing its `Mcp-Session-Id`), the
/// `notifications/initialized` note (best-effort), then `tools/list`. Returns the upstream's tools
/// and the negotiated session.
async fn connect_one(
    client: &HttpClient,
    url: &str,
    protocol: &str,
) -> Result<(Vec<Value>, Option<String>), String> {
    let (init_msg, session_id) = post_and_session(
        client,
        url,
        None,
        &initialize_request(1, protocol, GATEWAY_CLIENT_NAME),
    )
    .await?;
    // A JSON-RPC error on initialize means the handshake failed.
    rpc_result(&init_msg)?;

    // Per MCP the client sends `notifications/initialized` after initialize. It carries no id, so the
    // server replies with no body — best-effort, we don't parse or require the response.
    let _ = post_jsonrpc(
        client,
        url,
        session_id.as_deref(),
        &initialized_notification(),
    )
    .await;

    let (list_msg, _) =
        post_and_session(client, url, session_id.as_deref(), &tools_list_request(2)).await?;
    let result = rpc_result(&list_msg)?;
    Ok((tools_from_list_result(&result), session_id))
}

/// Federate (or re-federate) one upstream on the *running* gateway: connect it, point the forwarder
/// at its endpoint, and merge its tools into the live [`Federation`]. Returns whether the federated
/// surface changed (a new upstream, or different tools) — the caller emits `tools/list_changed`
/// when it did. This is the op capmeshd drives over the gateway control socket on a `register`.
pub async fn federate(
    client: &HttpClient,
    federation: &RwLock<Federation>,
    forwarder: &HttpForwarder,
    id: &str,
    url: &str,
    protocol: &str,
) -> Result<bool, String> {
    if id.contains(NS_SEP) {
        return Err(format!("upstream id '{id}' must not contain '{NS_SEP}'"));
    }
    let (tools, session_id) = connect_one(client, url, protocol).await?;
    // Point the forwarder first, so a concurrent tools/call can route the moment the tools appear.
    forwarder.set_endpoint(
        id,
        UpstreamEndpoint {
            url: url.to_string(),
            session_id,
        },
    );
    let changed = federation.write().await.set_upstream_tools(id, tools);
    info!(%id, %url, changed, "federated upstream (live)");
    Ok(changed)
}

/// Defederate one upstream from the running gateway: drop it from the forwarder and the live
/// [`Federation`]. Returns whether it had been federated (→ emit `tools/list_changed`).
pub async fn defederate(
    federation: &RwLock<Federation>,
    forwarder: &HttpForwarder,
    id: &str,
) -> bool {
    forwarder.remove_endpoint(id);
    let changed = federation.write().await.remove_upstream(id);
    info!(%id, changed, "defederated upstream (live)");
    changed
}

/// Retry the upstreams that failed to connect at startup, federating each live the moment it answers.
///
/// [`connect_upstreams`] is best-effort: an upstream still booting — or ordered *after* the gateway —
/// is skipped. On a host where capmeshd drives the control channel, that straggler gets re-registered;
/// but on the static-config floor (no capmeshd) nothing re-drives it, so it would stay dark until the
/// gateway restarts, forcing a strict "every upstream before the gateway" start-ordering. This
/// background loop removes that constraint: it re-attempts each still-pending upstream every
/// `interval` and [`federate`]s it live when it comes up (firing `tools/list_changed` to connected
/// agents via `notifier`), giving up on a given upstream only after `max_attempts` sweeps.
///
/// `pending` must already exclude `__`-invalid ids (those never federate); it is consumed as the loop
/// drops each upstream that successfully federates.
#[allow(clippy::too_many_arguments)]
pub async fn retry_pending_upstreams(
    client: HttpClient,
    federation: Arc<RwLock<Federation>>,
    forwarder: Arc<HttpForwarder>,
    notifier: broadcast::Sender<()>,
    mut pending: Vec<UpstreamConfig>,
    interval: Duration,
    max_attempts: usize,
) {
    for attempt in 1..=max_attempts {
        if pending.is_empty() {
            return;
        }
        tokio::time::sleep(interval).await;
        let mut still_pending = Vec::with_capacity(pending.len());
        for up in pending {
            let protocol = up
                .protocol_rev
                .as_deref()
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            match federate(&client, &federation, &forwarder, &up.id, &up.url, protocol).await {
                Ok(changed) => {
                    info!(id = %up.id, attempt, "late-federated pending upstream");
                    // A real surface change pushes tools/list_changed to connected agents.
                    if changed {
                        let _ = notifier.send(());
                    }
                }
                Err(e) => {
                    warn!(id = %up.id, attempt, "pending upstream still unreachable: {e}");
                    still_pending.push(up);
                }
            }
        }
        pending = still_pending;
    }
    if !pending.is_empty() {
        let ids: Vec<&str> = pending.iter().map(|u| u.id.as_str()).collect();
        warn!(
            ?ids,
            max_attempts, "giving up retrying pending upstreams after startup window"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::http_client;
    use axum::response::IntoResponse;
    use axum::{Json, Router, routing::post};
    use serde_json::json;

    /// An in-process MCP upstream: answers initialize (assigning a session), the initialized note,
    /// and tools/list. `tool` is the single tool it advertises.
    async fn spawn_upstream(tool: &'static str) -> String {
        let app = Router::new().route(
            "/mcp",
            post(move |Json(req): Json<Value>| async move {
                let method = req["method"].as_str().unwrap_or("");
                let id = req.get("id").cloned();
                match method {
                    "initialize" => (
                        [("mcp-session-id", "sess-xyz")],
                        Json(json!({ "jsonrpc": "2.0", "id": id,
                            "result": { "protocolVersion": "2026-07-28", "capabilities": {} } })),
                    )
                        .into_response(),
                    "tools/list" => Json(json!({ "jsonrpc": "2.0", "id": id,
                        "result": { "tools": [{ "name": tool, "inputSchema": {"type":"object"} }] } }))
                    .into_response(),
                    "tools/call" => Json(json!({ "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": "created" }] } }))
                    .into_response(),
                    // notifications/initialized — no id, no body.
                    _ => axum::http::StatusCode::ACCEPTED.into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/mcp")
    }

    fn upstream(id: &str, url: String) -> UpstreamConfig {
        UpstreamConfig {
            id: id.to_string(),
            url,
            transport: "streamable-http".to_string(),
            protocol_rev: None,
        }
    }

    #[tokio::test]
    async fn connects_upstreams_into_a_namespaced_federation_with_sessions() {
        let board_url = spawn_upstream("create_task").await;
        let kb_url = spawn_upstream("search").await;
        let client = http_client();

        let (federation, endpoints) = connect_upstreams(
            &client,
            &[upstream("board", board_url), upstream("kb", kb_url)],
        )
        .await;

        // Both upstreams federated, tools namespaced by id.
        let names: Vec<String> = federation
            .merged_tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["board__create_task", "kb__search"]);
        // Sessions captured from the initialize response header.
        assert_eq!(endpoints["board"].session_id.as_deref(), Some("sess-xyz"));
        assert_eq!(endpoints.len(), 2);
    }

    #[tokio::test]
    async fn an_unreachable_upstream_is_skipped_not_fatal() {
        let good = spawn_upstream("create_task").await;
        let client = http_client();

        // The second upstream points at a dead port — connect_upstreams must still federate the good one.
        let (federation, endpoints) = connect_upstreams(
            &client,
            &[
                upstream("board", good),
                upstream("dead", "http://127.0.0.1:1/mcp".to_string()),
            ],
        )
        .await;

        assert_eq!(federation.upstream_ids(), ["board"]);
        assert!(!endpoints.contains_key("dead"));
    }

    #[tokio::test]
    async fn an_id_with_the_namespace_separator_is_skipped() {
        let url = spawn_upstream("t").await;
        let client = http_client();
        let (federation, _) = connect_upstreams(&client, &[upstream("bad__id", url)]).await;
        assert!(federation.upstream_ids().is_empty());
    }

    #[tokio::test]
    async fn federate_then_defederate_updates_the_live_federation_and_forwarder() {
        use crate::forward::HttpForwarder;
        use crate::http::UpstreamForwarder;
        use std::collections::HashMap;

        let url = spawn_upstream("create_task").await;
        let client = http_client();
        let federation = RwLock::new(Federation::new());
        let forwarder = HttpForwarder::new(HashMap::new());

        // Federate on the running gateway: new upstream → surface changed.
        let changed = federate(
            &client,
            &federation,
            &forwarder,
            "board",
            &url,
            "2026-07-28",
        )
        .await
        .unwrap();
        assert!(changed);
        assert_eq!(
            federation.read().await.merged_tools()[0]["name"],
            "board__create_task"
        );
        // The forwarder now routes to the freshly-federated upstream.
        let msg = forwarder
            .forward("board".into(), "create_task".into(), json!({}))
            .await
            .unwrap();
        assert_eq!(msg["result"]["content"][0]["text"], "created");

        // Re-federate with identical tools → no surface change (idempotent).
        let changed = federate(
            &client,
            &federation,
            &forwarder,
            "board",
            &url,
            "2026-07-28",
        )
        .await
        .unwrap();
        assert!(!changed);

        // Defederate → removed from both; the forwarder no longer routes it.
        assert!(defederate(&federation, &forwarder, "board").await);
        assert!(federation.read().await.upstream_ids().is_empty());
        let err = forwarder
            .forward("board".into(), "create_task".into(), json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("no endpoint for upstream 'board'"));
        // Defederating an absent upstream reports no change.
        assert!(!defederate(&federation, &forwarder, "board").await);
    }

    #[tokio::test]
    async fn retry_federates_an_upstream_that_comes_up_after_startup() {
        use crate::forward::HttpForwarder;
        use std::collections::HashMap;
        use std::time::Duration;

        // Bind the listener now (so we know its port) but DON'T serve yet — the upstream is "down"
        // at startup, mirroring an upstream ordered after the gateway.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/mcp");

        let client = http_client();
        let federation = Arc::new(RwLock::new(Federation::new()));
        let forwarder = Arc::new(HttpForwarder::new(HashMap::new()));
        let (notifier, mut rx) = broadcast::channel(8);

        // The upstream wasn't connectable at startup, so it is pending.
        let pending = vec![UpstreamConfig {
            id: "board".to_string(),
            url: url.clone(),
            transport: "streamable-http".to_string(),
            protocol_rev: None,
        }];
        let retry = tokio::spawn(retry_pending_upstreams(
            client,
            federation.clone(),
            forwarder.clone(),
            notifier,
            pending,
            Duration::from_millis(20),
            50,
        ));

        // Bring the upstream up shortly after; a retry sweep should then federate it live.
        let app = Router::new().route(
            "/mcp",
            post(move |Json(req): Json<Value>| async move {
                let id = req.get("id").cloned();
                match req["method"].as_str().unwrap_or("") {
                    "initialize" => (
                        [("mcp-session-id", "sess-late")],
                        Json(json!({ "jsonrpc": "2.0", "id": id,
                            "result": { "protocolVersion": "2026-07-28", "capabilities": {} } })),
                    )
                        .into_response(),
                    "tools/list" => Json(json!({ "jsonrpc": "2.0", "id": id,
                        "result": { "tools": [{ "name": "create_task", "inputSchema": {"type":"object"} }] } }))
                    .into_response(),
                    _ => axum::http::StatusCode::ACCEPTED.into_response(),
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The retry loop drains to empty and returns once the upstream is federated.
        tokio::time::timeout(Duration::from_secs(5), retry)
            .await
            .expect("retry loop should finish once the upstream comes up")
            .unwrap();

        assert_eq!(
            federation.read().await.merged_tools()[0]["name"],
            "board__create_task"
        );
        // A late federation that changed the surface signalled connected streams.
        assert!(
            rx.try_recv().is_ok(),
            "late federation should fire tools/list_changed"
        );
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts_on_a_dead_upstream() {
        use crate::forward::HttpForwarder;
        use std::collections::HashMap;
        use std::time::Duration;

        let client = http_client();
        let federation = Arc::new(RwLock::new(Federation::new()));
        let forwarder = Arc::new(HttpForwarder::new(HashMap::new()));
        let (notifier, _rx) = broadcast::channel(8);

        let pending = vec![UpstreamConfig {
            id: "dead".to_string(),
            url: "http://127.0.0.1:1/mcp".to_string(),
            transport: "streamable-http".to_string(),
            protocol_rev: None,
        }];

        // Bounded: 3 sweeps of 10ms must return (not hang) and leave the federation empty.
        tokio::time::timeout(
            Duration::from_secs(5),
            retry_pending_upstreams(
                client,
                federation.clone(),
                forwarder,
                notifier,
                pending,
                Duration::from_millis(10),
                3,
            ),
        )
        .await
        .expect("retry loop must give up after max_attempts, not hang");

        assert!(federation.read().await.upstream_ids().is_empty());
    }

    #[tokio::test]
    async fn federate_rejects_a_namespaced_id() {
        let url = spawn_upstream("t").await;
        let client = http_client();
        let federation = RwLock::new(Federation::new());
        let forwarder = crate::forward::HttpForwarder::new(std::collections::HashMap::new());
        let err = federate(
            &client,
            &federation,
            &forwarder,
            "bad__id",
            &url,
            "2026-07-28",
        )
        .await
        .unwrap_err();
        assert!(err.contains("must not contain"));
    }
}
