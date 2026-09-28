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

use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{FromRef, FromRequestParts, Path, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::{
    inbox::{PushRequest, SurfaceEvent, SurfaceStore, ViewRequest, broadcast_view},
    mcp,
    page::{SURFACE_CSS, SURFACE_HTML, SURFACE_JS},
};

/// Router state: the shared store, the page HTML pre-rendered with the
/// `<base href>` for the configured mount prefix (so the page's relative asset
/// and API URLs resolve correctly whether served at `/` or behind a sub-path),
/// and an optional bearer token gating the MCP endpoint.
#[derive(Clone)]
struct AppState {
    store: Arc<SurfaceStore>,
    page_html: Arc<str>,
    mcp_token: Option<Arc<str>>,
}

// Handlers that need only the store extract it directly via `FromRef`.
impl FromRef<AppState> for Arc<SurfaceStore> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.store)
    }
}

/// Build the surface HTTP router over a shared [`SurfaceStore`], served at `/`.
pub fn router(store: Arc<SurfaceStore>) -> Router {
    router_with_base(store, "", None)
}

/// Build the router mounted under `base_path` (e.g. `/surfaced` when reverse-
/// proxied behind nginx). The prefix is normalized to `""` or `/segment...`;
/// all routes nest under it and the page's `<base href>` is set so its relative
/// URLs resolve under the prefix. nginx should proxy WITHOUT stripping the
/// prefix (`location /surfaced/ { proxy_pass http://127.0.0.1:8787; }`).
///
/// `mcp_token`, when set, is required as `Authorization: Bearer <token>` on the
/// `/mcp` endpoint (the agent API); `None` leaves `/mcp` open.
pub fn router_with_base(
    store: Arc<SurfaceStore>,
    base_path: &str,
    mcp_token: Option<String>,
) -> Router {
    let base = normalize_base(base_path);
    // Fingerprint the assets by content and reference them as `surface.js?v=<hash>`
    // in the (always-fresh) HTML. The asset URL then changes exactly when its
    // bytes change, so a redeploy is picked up immediately even though the assets
    // themselves are served with a long immutable cache (see `css`/`js`).
    let css_href = format!("surface.css?v={}", asset_hash(SURFACE_CSS));
    let js_src = format!("surface.js?v={}", asset_hash(SURFACE_JS));
    let page_html: Arc<str> = Arc::from(
        SURFACE_HTML
            .replace("__BASE_HREF__", &html_base_href(&base))
            .replace("__CSS_HREF__", &css_href)
            .replace("__JS_SRC__", &js_src),
    );
    let inner = Router::new()
        .route("/", get(health))
        .route("/surface.css", get(css))
        .route("/surface.js", get(js))
        .route("/s/{id}", get(page))
        .route("/s/{id}/events", get(events))
        .route("/s/{id}/items", get(list_items).post(push_item))
        .route("/s/{id}/items/{item_id}", delete(remove_item))
        .route("/s/{id}/view", post(set_view))
        .route("/s/{id}/clear", post(clear))
        .route("/mcp", post(mcp_post).get(mcp_get))
        .with_state(AppState {
            store,
            page_html,
            mcp_token: mcp_token.map(Arc::from),
        });
    if base.is_empty() {
        inner
    } else {
        Router::new().nest(&base, inner)
    }
}

/// The MCP endpoint (Streamable HTTP): the agent POSTs a JSON-RPC message; we
/// reply with a JSON response, or `202 Accepted` for a notification. Gated by
/// the optional bearer token.
async fn mcp_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(expected) = &state.mcp_token {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented != Some(expected.as_ref()) {
            return (StatusCode::UNAUTHORIZED, "mcp bearer token required\n").into_response();
        }
    }
    match mcp::dispatch(&state.store, &body) {
        Some(resp) => Json(resp).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// surfaced's MCP endpoint offers no server-initiated stream, so a `GET` (the
/// optional SSE channel) is Method Not Allowed — clients proceed request/response.
async fn mcp_get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
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

/// A content fingerprint for cache-busting an asset URL (`?v=<hash>`): FNV-1a
/// over the asset bytes. Dependency-free and fully deterministic (unlike
/// `DefaultHasher`, whose output is a std-internal detail), so the same content
/// always yields the same hash across builds and toolchains — the URL changes
/// exactly when the bytes change, letting the asset itself cache long-term.
fn asset_hash(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in content.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{hash:016x}")
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

/// Cache directive for the fingerprinted assets: the URL carries a content hash
/// (`?v=<hash>`), so the bytes at a given URL never change — cache them for a
/// year and skip revalidation. A content change yields a new URL, not a stale hit.
const ASSET_CACHE: &str = "public, max-age=31536000, immutable";

/// Serve the page stylesheet (linked from the HTML shell).
async fn css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, ASSET_CACHE),
        ],
        SURFACE_CSS,
    )
        .into_response()
}

/// Serve the page logic (linked from the HTML shell).
async fn js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, ASSET_CACHE),
        ],
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

/// Health + build probe. Returns 200 with the running version and the current
/// asset fingerprints, so a deploy can be verified without guessing: `curl …/`
/// and check `assets.js` matches the built frontend's hash (the fingerprint that
/// drives the page's `?v=` cache-busting). This is the answer to "did my redeploy
/// actually take?".
async fn health() -> Response {
    Json(json!({
        "service": "surfaced",
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "assets": {
            "js": asset_hash(SURFACE_JS),
            "css": asset_hash(SURFACE_CSS),
        },
    }))
    .into_response()
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

async fn clear(
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
    // Empties the inbox and broadcasts a fresh (empty) snapshot to attached tabs.
    store.clear(&id);
    StatusCode::NO_CONTENT.into_response()
}

/// Remove a single item from a surface's inbox by id (prune one stale item). The
/// removal broadcasts a fresh snapshot so attached tabs drop it live.
async fn remove_item(
    Path((id, item_id)): Path<(String, String)>,
    State(store): State<Arc<SurfaceStore>>,
    AttachToken(tok): AttachToken,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    if !store.authorize_attach(&id, tok.as_deref()) {
        return unauthorized();
    }
    if store.remove_item(&id, &item_id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no such surface or item\n").into_response()
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
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    // Tell a buffering reverse proxy (nginx and friends honor this) NOT to hold
    // the SSE bytes back: without it, live view/item events can be withheld until
    // the connection closes, so an attached tab appears to update only on reload.
    resp.headers_mut()
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    resp
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
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
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
    async fn events_stream_disables_proxy_buffering() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);
        // The response headers arrive before the (long-lived) SSE body streams,
        // so we can assert them without draining the stream.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/s/phone/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        // Without this a buffering reverse proxy holds live updates back until
        // the connection closes (the "have to refresh to update" symptom).
        assert_eq!(resp.headers().get("x-accel-buffering").unwrap(), "no");
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
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        // The reported js fingerprint matches the served asset (deploy-verifiable).
        assert_eq!(v["assets"]["js"], asset_hash(SURFACE_JS));
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
        // Asset refs are relative and content-fingerprinted (`?v=<hash>`); at the
        // root mount the base href is "/".
        assert!(html.contains(r#"href="surface.css?v="#));
        assert!(html.contains(r#"src="surface.js?v="#));
        assert!(html.contains(r#"<base href="/">"#));
        assert!(!html.contains("__BASE_HREF__"));
        assert!(!html.contains("__CSS_HREF__"));
        assert!(!html.contains("__JS_SRC__"));
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
        // Fingerprinted URL → cache long-term.
        assert_eq!(
            css.headers().get(header::CACHE_CONTROL).unwrap(),
            ASSET_CACHE
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
        assert_eq!(
            js.headers().get(header::CACHE_CONTROL).unwrap(),
            ASSET_CACHE
        );
    }

    #[test]
    fn asset_hash_is_deterministic_and_content_sensitive() {
        // Same bytes → same hash (stable URL); different bytes → different hash
        // (busts the cache). A fixed vector pins the algorithm so a future change
        // to it is a deliberate, visible edit.
        assert_eq!(asset_hash("body {}"), asset_hash("body {}"));
        assert_ne!(asset_hash("body {}"), asset_hash("body {} "));
        assert_eq!(asset_hash(""), "cbf29ce484222325");
    }

    #[tokio::test]
    async fn served_under_a_base_path_for_reverse_proxy() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router_with_base(store, "/surfaced/", None);

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

    #[tokio::test]
    async fn clear_endpoint_empties_the_surface() {
        let store = Arc::new(SurfaceStore::in_memory());
        store.push(
            "s",
            crate::item::DisplayItem::Text {
                body: "x".to_string(),
            },
            true,
        );
        let app = router(store.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/s/s/clear")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.snapshot("s").items.len(), 0);
    }

    #[tokio::test]
    async fn delete_item_endpoint_removes_one() {
        let store = Arc::new(SurfaceStore::in_memory());
        let a = store.push(
            "s",
            crate::item::DisplayItem::Text { body: "a".into() },
            true,
        );
        store.push(
            "s",
            crate::item::DisplayItem::Text { body: "b".into() },
            false,
        );
        let app = router(store.clone());
        // Remove item `a`.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/s/s/items/{}", a.entry.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.snapshot("s").items.len(), 1);
        // Removing an unknown item is 404.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/s/s/items/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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

    #[tokio::test]
    async fn mcp_endpoint_initializes_and_lists_tools() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router(store);
        let init =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
                .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(init))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["result"]["serverInfo"]["name"], "surfaced");
    }

    #[tokio::test]
    async fn mcp_endpoint_bearer_token_gates_access() {
        let store = Arc::new(SurfaceStore::in_memory());
        let app = router_with_base(store, "", Some("sek".to_string()));
        let body = || {
            Body::from(
                serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
                )
                .unwrap(),
            )
        };

        // No token → 401.
        let no = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(no.status(), StatusCode::UNAUTHORIZED);

        // Correct bearer → 200.
        let yes = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer sek")
                    .body(body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(yes.status(), StatusCode::OK);
    }
}
