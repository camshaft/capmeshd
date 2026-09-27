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
    extract::{Path, State},
    http::StatusCode,
    response::{
        Html, IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use serde_json::json;
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::inbox::{PushRequest, SurfaceEvent, SurfaceStore, ViewRequest, broadcast_view};
use crate::page::SURFACE_PAGE;

/// Build the surface HTTP router over a shared [`SurfaceStore`].
pub fn router(store: Arc<SurfaceStore>) -> Router {
    Router::new()
        .route("/", get(health))
        .route("/s/{id}", get(page))
        .route("/s/{id}/events", get(events))
        .route("/s/{id}/items", get(list_items).post(push_item))
        .route("/s/{id}/view", post(set_view))
        .with_state(store)
}

fn bad_id() -> Response {
    (StatusCode::BAD_REQUEST, "invalid surface id\n").into_response()
}

async fn health() -> &'static str {
    "surfaced: ok\n"
}

async fn page(Path(id): Path<String>) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    Html(SURFACE_PAGE).into_response()
}

async fn list_items(Path(id): Path<String>, State(store): State<Arc<SurfaceStore>>) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    Json(store.snapshot(&id)).into_response()
}

async fn push_item(
    Path(id): Path<String>,
    State(store): State<Arc<SurfaceStore>>,
    Json(req): Json<PushRequest>,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
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
    Json(req): Json<ViewRequest>,
) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
    }
    match store.set_view(&id, req.item_id.as_deref()) {
        Some(tx) => {
            broadcast_view(&tx, req.item_id.clone());
            StatusCode::NO_CONTENT.into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such surface or item\n").into_response(),
    }
}

async fn events(Path(id): Path<String>, State(store): State<Arc<SurfaceStore>>) -> Response {
    if !SurfaceStore::valid_id(&id) {
        return bad_id();
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
}
