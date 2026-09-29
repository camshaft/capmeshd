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
    initialized_notification, initialize_request, rpc_result, tools_from_list_result,
    tools_list_request, DEFAULT_PROTOCOL_VERSION,
};
use crate::config::UpstreamConfig;
use crate::forward::{post_and_session, post_jsonrpc, HttpClient, UpstreamEndpoint};
use crate::{Federation, NS_SEP};
use serde_json::Value;
use std::collections::HashMap;
use tracing::{info, warn};

/// The client name the gateway presents to upstreams on `initialize`.
const GATEWAY_CLIENT_NAME: &str = "capmesh-gateway";

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
        let protocol = up.protocol_rev.as_deref().unwrap_or(DEFAULT_PROTOCOL_VERSION);
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
    let _ = post_jsonrpc(client, url, session_id.as_deref(), &initialized_notification()).await;

    let (list_msg, _) =
        post_and_session(client, url, session_id.as_deref(), &tools_list_request(2)).await?;
    let result = rpc_result(&list_msg)?;
    Ok((tools_from_list_result(&result), session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forward::http_client;
    use axum::response::IntoResponse;
    use axum::{routing::post, Json, Router};
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
}
