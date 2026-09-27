//! The surface HTTP/SSE server — the same-origin page and the push path.
//!
//! `surfaced` serves *its own* page (DESIGN §10.1), so a display item can be as
//! rich as arbitrary same-origin HTML/JS. Routes:
//!
//! - `GET  /`               — health probe.
//! - `GET  /s/{id}`         — the attachment page (main view + inbox feed).
//! - `GET  /s/{id}/events`  — SSE: an initial snapshot then live updates.
//! - `GET  /s/{id}/items`   — the surface's current state as JSON.
//! - `POST /s/{id}/items`   — push a display item (the minimal push path; the
//!   control-socket `send-item` and the MCP `send` tool wrap this later).
//! - `POST /s/{id}/view`    — select which item the main view shows.
//!
//! A surface is created on first touch (attach or push), so a surface is
//! visibly real end-to-end without a prior `create-surface` call.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{FromRef, FromRequestParts, Path, State},
    http::{StatusCode, header, request::Parts},
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use serde_json::json;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::inbox::{PushRequest, SurfaceEvent, SurfaceStore, ViewRequest, broadcast_view};
use crate::page::{SURFACE_CSS, SURFACE_HTML, SURFACE_JS};

/// Router state: the shared store plus the page HTML pre-rendered with the
/// `<base href>` for the configured mount prefix (so the page's relative asset
/// and API URLs resolve correctly whether served at `/` or behind a sub-path).
#[derive(Clone)]
struct AppState {
    store: Arc<SurfaceStore>,
    page_html: Arc<str>,
}

// Handlers that need only the store extract it directly via `FromRef`.
impl FromRef<AppState> for Arc<SurfaceStore> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.store)
    }
}

/// Build the surface HTTP router over a shared [`SurfaceStore`], served at `/`.
pub fn router(store: Arc<SurfaceStore>) -> Router {
    router_with_base(store, "")
}

/// Build the router mounted under `base_path` (e.g. `/surfaced` when reverse-
/// proxied behind nginx). The prefix is normalized to `""` or `/segment...`;
/// all routes nest under it and the page's `<base href>` is set so its relative
/// URLs resolve under the prefix. nginx should proxy WITHOUT stripping the
/// prefix (`location /surfaced/ { proxy_pass http://127.0.0.1:8787; }`).
pub fn router_with_base(store: Arc<SurfaceStore>, base_path: &str) -> Router {
    let base = normalize_base(base_path);
    let page_html: Arc<str> =
        Arc::from(SURFACE_HTML.replace("__BASE_HREF__", &html_base_href(&base)));
    let inner = Router::new()
        .route("/", get(health))
        .route("/surface.css", get(css))
        .route("/surface.js", get(js))
        .route("/s/{id}", get(page))
        .route("/s/{id}/events", get(events))
        .route("/s/{id}/items", get(list_items).post(push_item))
        .route("/s/{id}/view", post(set_view))
        .with_state(AppState { store, page_html });
    if base.is_empty() {
        inner
    } else {
        Router::new().nest(&base, inner)
    }
}

/// Normalize a mount prefix to `""` (root) or `/seg[/seg...]` (no trailing slash).
fn normalize_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// The `<base href>` value for a normalized prefix — always ends in `/` so the
/// page's relative URLs resolve directly under it (`/` for the root mount).
fn html_base_href(base: &str) -> String {
    if base.is_empty() {
        "/".to_string()
    } else {
        format!("{base}/")
    }
}

/// Serve the page stylesheet (linked from the HTML shell).
async fn css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        SURFACE_CSS,
    )
        .into_response()
}

/// Serve the page logic (linked from the HTML shell).
async fn js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        SURFACE_JS,
    )
        .into_response()
}

fn bad_id() -> Response {
    (StatusCode::BAD_REQUEST, "invalid surface id\n").into_response()
}

/// The attach token a request presents, from (in order) the `X-Surface-Token`
/// header, an `Authorization: Bearer <t>` header, or a `?token=<t>` query param.
/// Use a URL-safe token — the query value is not percent-decoded.
struct AttachToken(Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for AttachToken {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(v) = parts
            .headers
            .get("x-surface-token")
            .and_then(|v| v.to_str().ok())
        {
            return Ok(AttachToken(Some(v.to_string())));
        }
        if let Some(t) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return Ok(AttachToken(Some(t.to_string())));
        }
        if let Some(q) = parts.uri.query() {
            for pair in q.split('&') {
                if let Some(t) = pair.strip_prefix("token=") {
                    return Ok(AttachToken(Some(t.to_string())));
                }
            }
        }
        Ok(AttachToken(None))
    }
}

/// The 401 for a token-protected surface accessed without a valid token.
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "attach token required\n").into_response()
}

async fn health() -> &'static str {
    "surfaced: ok\n"
}

async fn page(
    Path(id): Path<String>,
    State(state): State<AppState>,
    AttachToken(tok): AttachToken,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !state.store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    Html(state.page_html.to_string()).into_response()
}

async fn list_items(
    Path(id): Path<String>,
    State(store): State<Arc<SurfaceStore>>,
    AttachToken(tok): AttachToken,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    Json(store.snapshot(&id)).into_response()
}

async fn push_item(
    Path(id): Path<String>,
    State(store): State<Arc<SurfaceStore>>,
    AttachToken(tok): AttachToken,
    Json(req): Json<PushRequest>,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    let pushed = store.push(&id, req.item, req.promote);
    pushed.broadcast();
    (
        StatusCode::CREATED,
        Json(json!({"id": pushed.entry.id, "ts": pushed.entry.ts})),
    )
        .into_response()
}

async fn set_view(
    Path(id): Path<String>,
    State(store): State<Arc<SurfaceStore>>,
    AttachToken(tok): AttachToken,
    Json(req): Json<ViewRequest>,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    match store.set_view(&id, req.item_id.as_deref()) {
        Some(tx) => {
            broadcast_view(&tx, req.item_id.clone());
            StatusCode::NO_CONTENT.into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such surface or item\n").into_response(),
    }
}

async fn events(
    Path(id): Path<String>,
    State(store): State<Arc<SurfaceStore>>,
    AttachToken(tok): AttachToken,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    // Subscribe first, then snapshot (both under the store lock) so no push can
    // slip between the snapshot and the live stream.
    let (snapshot, rx) = store.subscribe(&id);
    let initial = SurfaceEvent::Snapshot { surface: snapshot };
    let live = BroadcastStream::new(rx)
        .filter_map(|r| r.ok())
        .map(to_event);
    let stream = tokio_stream::once(to_event(initial)).chain(live);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Render a surface event as an SSE `data:` frame. Our event types always
/// serialize; the fallback empty object only guards against an impossible error.
fn to_event(ev: SurfaceEvent) -> Result<Event, Infallible> {
    Ok(Event::default()
        .json_data(&ev)
        .unwrap_or_else(|_| Event::default().data("{}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    fn text_push(body: &str, promote: bool) -> Body {
        Body::from(
            serde_json::to_vec(&json!({"item":{"type":"text","body":body},"promote":promote}))
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn push_then_list_round_trips_through_http() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/s/phone/items")
                    .header("content-type", "application/json")
                    .body(text_push("hello phone", true))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/s/phone/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "phone");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["item"]["type"], "text");
        assert_eq!(items[0]["item"]["body"], "hello phone");
        // Promoted push is the main view.
        assert_eq!(v["current-view"], items[0]["id"]);
    }

    #[tokio::test]
    async fn unknown_item_type_is_rejected() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/s/phone/items")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"item":{"type":"bogus"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn health_ok() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn page_and_assets_serve_with_expected_content_types() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);

        // The HTML shell links the split-out asset routes.
        let page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/phone")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = to_bytes(page.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(html.to_vec()).unwrap();
        // Asset refs are relative; at the root mount the base href is "/".
        assert!(html.contains(r#"href="surface.css""#));
        assert!(html.contains(r#"src="surface.js""#));
        assert!(html.contains(r#"<base href="/">"#));
        assert!(!html.contains("__BASE_HREF__"));
        // Mobile layout: a hamburger toggle + the inbox feed as a drawer.
        assert!(html.contains(r#"id="menu""#));
        assert!(html.contains(r#"id="feed""#));

        let css = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/surface.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css.status(), StatusCode::OK);
        assert_eq!(
            css.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css; charset=utf-8"
        );

        let js = app
            .oneshot(
                Request::builder()
                    .uri("/surface.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(js.status(), StatusCode::OK);
        assert_eq!(
            js.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn served_under_a_base_path_for_reverse_proxy() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router_with_base(store, "/surfaced/");

        // Root paths are NOT served when mounted under a prefix.
        let at_root = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/phone")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(at_root.status(), StatusCode::NOT_FOUND);

        // The page is served under the prefix, with a matching <base href> so
        // its relative asset/API URLs resolve under the prefix.
        let page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/surfaced/s/phone")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = to_bytes(page.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(html.to_vec()).unwrap();
        assert!(html.contains(r#"<base href="/surfaced/">"#));

        // Assets and the push path also live under the prefix.
        let css = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/surfaced/surface.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css.status(), StatusCode::OK);

        let push = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/surfaced/s/phone/items")
                    .header("content-type", "application/json")
                    .body(text_push("via proxy", true))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(push.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn token_protected_surface_requires_a_valid_token() {
        let store = Arc::new(SurfaceStore::in_memory());
        store.ensure("secret", None, Some("s3cr3t".to_string()));
        let app = router(store);

        // No token → 401 on the page and the data endpoints.
        for uri in ["/s/secret", "/s/secret/events", "/s/secret/items"] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        // Wrong token → 401.
        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/secret?token=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        // Correct token via query → 200 page.
        let ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/secret?token=s3cr3t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);

        // Correct token via X-Surface-Token header → 200 items.
        let via_header = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/secret/items")
                    .header("x-surface-token", "s3cr3t")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_header.status(), StatusCode::OK);

        // A token-less surface stays open without a token.
        let open = app
            .oneshot(
                Request::builder()
                    .uri("/s/open/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(open.status(), StatusCode::OK);
    }

    #[test]
    fn base_path_normalization() {
        assert_eq!(normalize_base(""), "");
        assert_eq!(normalize_base("/"), "");
        assert_eq!(normalize_base("surfaced"), "/surfaced");
        assert_eq!(normalize_base("/surfaced/"), "/surfaced");
        assert_eq!(normalize_base("/a/b"), "/a/b");
        assert_eq!(html_base_href(""), "/");
        assert_eq!(html_base_href("/surfaced"), "/surfaced/");
    }
}
