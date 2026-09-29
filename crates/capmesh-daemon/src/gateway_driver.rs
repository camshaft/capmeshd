//! Driving the MCP gateway from route-registry changes (DESIGN §7.2). capmeshd owns the
//! [`McpRouteRegistry`](crate::mcp_routes); when a route is registered/unregistered (by the manual
//! `register` endpoint or `cap=mcp` auto-discovery), it must tell the running gateway to
//! (de)federate that upstream over the gateway's control channel (`/admin/upstreams`).
//!
//! This module is the pure mapping — registry change → the gateway admin request to send. The
//! binary owns the HTTP client that actually sends it, so the decision (which method/path/body, and
//! when to do nothing) is unit-tested here.

use crate::mcp_routes::RegisterOutcome;
use bytes::Bytes;
use capmesh_ctl::{McpRoute, McpTransport};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};

/// The gateway control-channel call a route change implies.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayCommand {
    /// `POST /admin/upstreams` with this body — federate (or re-federate) the upstream.
    Federate { body: Value },
    /// `DELETE /admin/upstreams/{id}` — defederate the upstream.
    Defederate { id: String },
    /// Nothing to drive: an unchanged re-register, or a transport the HTTP gateway can't federate.
    Noop,
}

impl GatewayCommand {
    /// The HTTP request to send to the gateway admin base URL: `(method, path, optional JSON body)`,
    /// or `None` for [`GatewayCommand::Noop`].
    pub fn request(&self) -> Option<(&'static str, String, Option<Value>)> {
        match self {
            GatewayCommand::Federate { body } => {
                Some(("POST", "/admin/upstreams".to_string(), Some(body.clone())))
            }
            GatewayCommand::Defederate { id } => {
                Some(("DELETE", format!("/admin/upstreams/{id}"), None))
            }
            GatewayCommand::Noop => None,
        }
    }
}

/// The command to drive the gateway with after a registry `register` returning `outcome`. Only a
/// real change to a `streamable-http` upstream federates; an unchanged re-register — or a `stdio`
/// route the HTTP gateway can't reach — is a no-op.
pub fn on_register(route: &McpRoute, outcome: RegisterOutcome) -> GatewayCommand {
    match outcome {
        RegisterOutcome::Unchanged => GatewayCommand::Noop,
        RegisterOutcome::Added | RegisterOutcome::Updated => match route.transport {
            McpTransport::StreamableHttp => GatewayCommand::Federate {
                body: federate_body(route),
            },
            McpTransport::Stdio => GatewayCommand::Noop,
        },
    }
}

/// The command to drive the gateway with after a registry `unregister` of `id`.
pub fn on_unregister(id: &str) -> GatewayCommand {
    GatewayCommand::Defederate { id: id.to_string() }
}

/// A small HTTP client for the gateway admin channel (plain HTTP — loopback/LAN, no TLS).
pub type GatewayClient = Client<HttpConnector, Full<Bytes>>;

/// Build the gateway-admin HTTP client.
pub fn gateway_client() -> GatewayClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Send a [`GatewayCommand`] to the gateway admin at `base_url` (e.g. `http://127.0.0.1:8092`). A
/// [`GatewayCommand::Noop`] sends nothing. Best-effort from the caller's view: a transport failure
/// or a non-2xx status is returned as `Err(reason)` to log — a down gateway must not crash the
/// control plane (capmeshd re-drives on the next registry change / reconcile).
pub async fn send_command(
    client: &GatewayClient,
    base_url: &str,
    cmd: &GatewayCommand,
) -> Result<(), String> {
    let Some((method, path, body)) = cmd.request() else {
        return Ok(());
    };
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let body_bytes = match body {
        Some(v) => serde_json::to_vec(&v).map_err(|e| format!("encode body: {e}"))?,
        None => Vec::new(),
    };
    let mut builder = hyper::Request::builder().method(method).uri(&url);
    if !body_bytes.is_empty() {
        builder = builder.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(Full::new(Bytes::from(body_bytes)))
        .map_err(|e| format!("build request: {e}"))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| format!("gateway admin request to {url} failed: {e}"))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let bytes = response
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        Err(format!(
            "gateway admin {url} returned {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes)
        ))
    }
}

/// The `/admin/upstreams` federate body for a route — `protocol-rev` omitted when absent.
fn federate_body(route: &McpRoute) -> Value {
    let mut body = json!({
        "id": route.id,
        "url": route.url,
        "transport": "streamable-http",
    });
    if let Some(rev) = &route.protocol_rev {
        body["protocol-rev"] = json!(rev);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: &str, transport: McpTransport, rev: Option<&str>) -> McpRoute {
        McpRoute {
            id: id.to_string(),
            url: format!("http://h/{id}/mcp"),
            transport,
            protocol_rev: rev.map(str::to_string),
        }
    }

    #[test]
    fn a_new_streamable_http_route_federates() {
        let r = route("board", McpTransport::StreamableHttp, Some("2026-07-28"));
        let cmd = on_register(&r, RegisterOutcome::Added);
        let GatewayCommand::Federate { body } = &cmd else {
            panic!("expected Federate, got {cmd:?}")
        };
        assert_eq!(body["id"], "board");
        assert_eq!(body["url"], "http://h/board/mcp");
        assert_eq!(body["transport"], "streamable-http");
        assert_eq!(body["protocol-rev"], "2026-07-28");

        // Renders as a POST to the federate endpoint.
        let (method, path, sent) = cmd.request().unwrap();
        assert_eq!((method, path.as_str()), ("POST", "/admin/upstreams"));
        assert!(sent.is_some());
    }

    #[test]
    fn protocol_rev_is_omitted_when_absent() {
        let r = route("kb", McpTransport::StreamableHttp, None);
        let GatewayCommand::Federate { body } = on_register(&r, RegisterOutcome::Updated) else {
            panic!("expected Federate")
        };
        assert!(body.get("protocol-rev").is_none());
    }

    #[test]
    fn an_unchanged_register_is_a_noop() {
        let r = route("board", McpTransport::StreamableHttp, None);
        assert_eq!(on_register(&r, RegisterOutcome::Unchanged), GatewayCommand::Noop);
        assert!(on_register(&r, RegisterOutcome::Unchanged).request().is_none());
    }

    #[test]
    fn a_stdio_route_is_not_federated_by_the_http_gateway() {
        let r = route("local", McpTransport::Stdio, None);
        assert_eq!(on_register(&r, RegisterOutcome::Added), GatewayCommand::Noop);
    }

    #[test]
    fn unregister_defederates_by_id() {
        let cmd = on_unregister("board");
        assert_eq!(cmd, GatewayCommand::Defederate { id: "board".into() });
        let (method, path, sent) = cmd.request().unwrap();
        assert_eq!((method, path.as_str()), ("DELETE", "/admin/upstreams/board"));
        assert!(sent.is_none());
    }

    // --- send_command: against an in-process gateway-admin stub ---

    use axum::{extract::Path as AxPath, routing::post as axpost, Json as AxJson, Router};
    use std::sync::{Arc, Mutex};

    /// A stub gateway admin recording the requests it received; `status` is what it replies.
    #[derive(Default)]
    struct AdminLog {
        posted: Mutex<Vec<Value>>,
        deleted: Mutex<Vec<String>>,
    }

    async fn spawn_admin(log: Arc<AdminLog>, status: axum::http::StatusCode) -> String {
        let post_log = log.clone();
        let del_log = log.clone();
        let app = Router::new()
            .route(
                "/admin/upstreams",
                axpost(move |AxJson(body): AxJson<Value>| {
                    let log = post_log.clone();
                    async move {
                        log.posted.lock().unwrap().push(body);
                        status
                    }
                }),
            )
            .route(
                "/admin/upstreams/{id}",
                axum::routing::delete(move |AxPath(id): AxPath<String>| {
                    let log = del_log.clone();
                    async move {
                        log.deleted.lock().unwrap().push(id);
                        status
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn send_command_posts_a_federate_and_deletes_a_defederate() {
        let log = Arc::new(AdminLog::default());
        let base = spawn_admin(log.clone(), axum::http::StatusCode::OK).await;
        let client = gateway_client();

        // Federate → POST /admin/upstreams with the route body.
        let r = route("board", McpTransport::StreamableHttp, Some("2026-07-28"));
        send_command(&client, &base, &on_register(&r, RegisterOutcome::Added))
            .await
            .unwrap();
        // Defederate → DELETE /admin/upstreams/board.
        send_command(&client, &base, &on_unregister("board")).await.unwrap();

        let posted = log.posted.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0]["id"], "board");
        assert_eq!(posted[0]["transport"], "streamable-http");
        assert_eq!(*log.deleted.lock().unwrap(), vec!["board".to_string()]);
    }

    #[tokio::test]
    async fn send_command_noop_sends_nothing() {
        let log = Arc::new(AdminLog::default());
        let base = spawn_admin(log.clone(), axum::http::StatusCode::OK).await;
        let client = gateway_client();
        send_command(&client, &base, &GatewayCommand::Noop).await.unwrap();
        assert!(log.posted.lock().unwrap().is_empty());
        assert!(log.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_non_2xx_gateway_reply_is_an_error() {
        let log = Arc::new(AdminLog::default());
        let base = spawn_admin(log, axum::http::StatusCode::BAD_GATEWAY).await;
        let client = gateway_client();
        let r = route("board", McpTransport::StreamableHttp, None);
        let err = send_command(&client, &base, &on_register(&r, RegisterOutcome::Added))
            .await
            .unwrap_err();
        assert!(err.contains("returned 502"));
    }
}
