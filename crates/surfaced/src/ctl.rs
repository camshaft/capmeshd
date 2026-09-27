//! The `surface-ctl` control socket (DESIGN §10.1, §4).
//!
//! `surfaced` is a first-party data-plane daemon: capmeshd stays stateless
//! plumbing and drives it over a **local Unix domain socket** carrying
//! newline-delimited JSON-RPC 2.0 (the `capmesh-ctl` framing, extended with the
//! surface methods). capmeshd's `surface` adapter is the client; this module is
//! the server. The framing + `hello` gate mirror the MIDI daemon (`nmidid`) so
//! the two first-party daemons speak the same shape.
//!
//! Methods (capmeshd → surfaced): `hello`, `create-surface`, `send-item`,
//! `set-view`, `list-items`. Every method but `hello` is refused until a
//! compatible `hello` completes. All surface state lives in the shared
//! [`SurfaceStore`], so a control-socket push and an HTTP push are identical and
//! both fan out to attached tabs.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::inbox::{SurfaceStore, broadcast_view};
use crate::item::DisplayItem;

/// The single-integer protocol major version this daemon speaks.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Daemon identity returned in the `hello` result.
pub const DAEMON_ID: &str = concat!("surfaced/", env!("CARGO_PKG_VERSION"));
/// Optional features advertised in the `hello` result.
pub const CAPABILITIES: &[&str] = &["surface", "durable-inbox", "attach-fanout"];

// JSON-RPC numeric codes.
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const DAEMON_DOMAIN: i64 = -32001;

/// A JSON-RPC error carrying a machine `data.code` (see DESIGN §6 error model).
struct CtlError {
    number: i64,
    code: &'static str,
    message: String,
}

impl CtlError {
    fn domain(code: &'static str, message: impl Into<String>) -> Self {
        CtlError {
            number: DAEMON_DOMAIN,
            code,
            message: message.into(),
        }
    }
    fn protocol(number: i64, code: &'static str, message: impl Into<String>) -> Self {
        CtlError {
            number,
            code,
            message: message.into(),
        }
    }
    fn object(&self) -> Value {
        serde_json::json!({
            "code": self.number,
            "message": self.message,
            "data": { "code": self.code },
        })
    }
}

fn success(id: Value, result: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}
fn error(id: Value, err: &CtlError) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": err.object()})
}

/// Per-connection dispatch state. A compatible `hello` must complete first.
pub struct Session {
    store: Arc<SurfaceStore>,
    hello_done: bool,
}

impl Session {
    pub fn new(store: Arc<SurfaceStore>) -> Self {
        Session {
            store,
            hello_done: false,
        }
    }

    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, CtlError> {
        if method != "hello" && !self.hello_done {
            return Err(CtlError::domain(
                "not-ready",
                "hello must be the first request on a connection",
            ));
        }
        match method {
            "hello" => self.handle_hello(params),
            "create-surface" => self.handle_create_surface(params),
            "send-item" => self.handle_send_item(params),
            "set-view" => self.handle_set_view(params),
            "list-items" => self.handle_list_items(params),
            "list-surfaces" => self.handle_list_surfaces(),
            "clear-items" => self.handle_clear(params),
            "remove-item" => self.handle_remove_item(params),
            "delete-surface" => self.handle_delete(params),
            "set-token" => self.handle_set_token(params),
            other => Err(CtlError::protocol(
                METHOD_NOT_FOUND,
                "method-not-found",
                format!("unknown method '{other}'"),
            )),
        }
    }

    fn handle_hello(&mut self, params: &Value) -> Result<Value, CtlError> {
        let major = params
            .get("protocol")
            .and_then(parse_major)
            .ok_or_else(|| {
                CtlError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "hello requires a 'protocol' major version",
                )
            })?;
        if major != PROTOCOL_MAJOR {
            return Err(CtlError::domain(
                "unsupported-protocol",
                format!("daemon speaks protocol {PROTOCOL_MAJOR}, client requested {major}"),
            ));
        }
        self.hello_done = true;
        Ok(serde_json::json!({
            "protocol": PROTOCOL_MAJOR.to_string(),
            "daemon": DAEMON_ID,
            "capabilities": CAPABILITIES,
        }))
    }

    fn handle_create_surface(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Attach token (DESIGN §10.1): only settable over the local-trust control
        // socket, never over HTTP. When set, HTTP attachment requires it.
        let attach_token = params
            .get("attach-token")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.store.ensure(&id, title, attach_token);
        Ok(serde_json::json!({ "id": id }))
    }

    fn handle_send_item(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        let item_val = params.get("item").ok_or_else(|| {
            CtlError::protocol(
                INVALID_PARAMS,
                "invalid-params",
                "send-item requires 'item'",
            )
        })?;
        let item: DisplayItem = serde_json::from_value(item_val.clone()).map_err(|e| {
            CtlError::protocol(
                INVALID_PARAMS,
                "invalid-params",
                format!("invalid item: {e}"),
            )
        })?;
        // Default: a push promotes to the main view (the latest is shown).
        let promote = params
            .get("promote")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let pushed = self.store.push(&id, item, promote);
        pushed.broadcast();
        Ok(serde_json::json!({ "id": pushed.entry.id, "ts": pushed.entry.ts }))
    }

    fn handle_set_view(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        let item_id = params.get("item-id").and_then(Value::as_str);
        match self.store.set_view(&id, item_id) {
            Some(tx) => {
                broadcast_view(&tx, item_id.map(str::to_string));
                Ok(Value::Object(Map::new()))
            }
            None => Err(CtlError::domain(
                "no-such-item",
                "unknown surface or item-id",
            )),
        }
    }

    fn handle_list_surfaces(&self) -> Result<Value, CtlError> {
        // Service discovery: which surfaces this daemon offers. A control-plane
        // op (capmeshd is control + discovery); content push stays data-plane.
        let surfaces = serde_json::to_value(self.store.list_surfaces())
            .unwrap_or_else(|_| Value::Array(Vec::new()));
        Ok(serde_json::json!({ "surfaces": surfaces }))
    }

    fn handle_clear(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        self.store.clear(&id); // idempotent: unknown surface is a no-op
        Ok(Value::Object(Map::new()))
    }

    fn handle_remove_item(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        let item_id = params.get("item-id").and_then(Value::as_str).ok_or_else(|| {
            CtlError::protocol(
                INVALID_PARAMS,
                "invalid-params",
                "remove-item requires 'item-id'",
            )
        })?;
        if self.store.remove_item(&id, item_id) {
            Ok(Value::Object(Map::new()))
        } else {
            Err(CtlError::domain(
                "no-such-item",
                "unknown surface or item-id",
            ))
        }
    }

    fn handle_delete(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        self.store.delete(&id); // idempotent: unknown surface is a no-op
        Ok(Value::Object(Map::new()))
    }

    fn handle_set_token(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        // `attach-token`: a string sets/rotates it; absent or null clears it
        // (reopening the surface). Local-trust socket only — never over HTTP.
        let token = params
            .get("attach-token")
            .and_then(Value::as_str)
            .map(str::to_string);
        if self.store.set_token(&id, token) {
            Ok(Value::Object(Map::new()))
        } else {
            Err(CtlError::domain(
                "no-such-surface",
                "unknown surface (create it first)",
            ))
        }
    }

    fn handle_list_items(&self, params: &Value) -> Result<Value, CtlError> {
        let id = surface_id(params)?;
        let view = self.store.snapshot(&id);
        serde_json::to_value(view)
            .map_err(|e| CtlError::protocol(DAEMON_DOMAIN, "internal", format!("serialize: {e}")))
    }

    fn respond(&mut self, req: Request) -> Option<Value> {
        let outcome = self.dispatch(&req.method, &req.params);
        match req.id {
            Some(id) => Some(match outcome {
                Ok(result) => success(id, result),
                Err(err) => error(id, &err),
            }),
            None => {
                if let Err(err) = outcome {
                    debug!("notification '{}' failed: {}", req.method, err.message);
                }
                None
            }
        }
    }
}

/// A `surface-id` param that must be a path-safe surface id.
fn surface_id(params: &Value) -> Result<String, CtlError> {
    let id = params
        .get("surface-id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CtlError::protocol(INVALID_PARAMS, "invalid-params", "requires 'surface-id'")
        })?;
    if !SurfaceStore::valid_id(id) {
        return Err(CtlError::domain("invalid-surface-id", "invalid surface id"));
    }
    Ok(id.to_string())
}

/// A single incoming JSON-RPC 2.0 request line.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

fn parse_major(v: &Value) -> Option<u32> {
    match v {
        Value::String(s) => s
            .split(|c: char| !c.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse().ok()),
        Value::Number(n) => n.as_u64().and_then(|u| u32::try_from(u).ok()),
        _ => None,
    }
}

/// Drive the NDJSON/JSON-RPC framing for one connection over any reader/writer.
pub async fn serve_connection<R, W>(
    mut reader: R,
    mut writer: W,
    store: Arc<SurfaceStore>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut session = Session::new(store);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading control line")?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => session.respond(req),
            Err(e) => Some(error(
                Value::Null,
                &CtlError::protocol(PARSE_ERROR, "parse-error", format!("invalid JSON: {e}")),
            )),
        };
        if let Some(value) = response {
            let mut buf = serde_json::to_vec(&value).context("serializing response")?;
            buf.push(b'\n');
            writer.write_all(&buf).await.context("writing response")?;
            writer.flush().await.context("flushing response")?;
        }
    }
    Ok(())
}

/// Bind the control socket at `path` and serve connections until cancelled.
///
/// A stale socket file is removed first; the socket is owner/group read-write
/// (`0o660`) — the local trust boundary (control-protocol §1.1).
pub async fn run(path: impl AsRef<Path>, store: Arc<SurfaceStore>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    info!("surfaced control socket listening on {}", path.display());
    loop {
        let (stream, _addr) = listener.accept().await.context("accepting connection")?;
        debug!("control connection accepted");
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            if let Err(e) = serve_connection(BufReader::new(read_half), write_half, store).await {
                warn!("control connection ended with error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn exchange(store: Arc<SurfaceStore>, requests: &[Value]) -> Vec<Value> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let handle =
            tokio::spawn(async move { serve_connection(BufReader::new(sr), sw, store).await.ok() });
        for req in requests {
            let mut line = serde_json::to_vec(req).unwrap();
            line.push(b'\n');
            client.write_all(&line).await.unwrap();
        }
        client.flush().await.unwrap();
        client.shutdown().await.unwrap();
        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        handle.await.unwrap();
        buf.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn hello() -> Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"hello",
            "params":{"protocol":"1","client":"capmeshd/0.1"}})
    }

    #[tokio::test]
    async fn hello_returns_identity_and_capabilities() {
        let out = exchange(Arc::new(SurfaceStore::in_memory()), &[hello()]).await;
        assert_eq!(out[0]["result"]["daemon"], DAEMON_ID);
        let caps: Vec<String> =
            serde_json::from_value(out[0]["result"]["capabilities"].clone()).unwrap();
        assert!(caps.contains(&"surface".to_string()));
    }

    #[tokio::test]
    async fn methods_before_hello_are_not_ready() {
        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"list-items",
            "params":{"surface-id":"phone"}});
        let out = exchange(Arc::new(SurfaceStore::in_memory()), &[req]).await;
        assert_eq!(out[0]["error"]["data"]["code"], "not-ready");
    }

    #[tokio::test]
    async fn send_item_then_list_items_reflects_it() {
        let store = Arc::new(SurfaceStore::in_memory());
        let create = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"create-surface",
            "params":{"surface-id":"phone","title":"My Phone"}});
        let send = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"send-item",
            "params":{"surface-id":"phone","item":{"type":"pdf","url":"https://x/m.pdf"}}});
        let list = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"list-items",
            "params":{"surface-id":"phone"}});
        let out = exchange(store, &[hello(), create, send, list]).await;
        assert_eq!(out[1]["result"]["id"], "phone");
        assert!(out[2]["result"]["id"].is_string());
        let view = &out[3]["result"];
        assert_eq!(view["title"], "My Phone");
        let items = view["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["item"]["type"], "pdf");
        assert_eq!(view["current-view"], items[0]["id"]);
    }

    #[tokio::test]
    async fn list_surfaces_reports_registered_surfaces() {
        let store = Arc::new(SurfaceStore::in_memory());
        let create = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"create-surface",
            "params":{"surface-id":"phone","title":"Phone"}});
        let list = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"list-surfaces","params":{}});
        let out = exchange(store, &[hello(), create, list]).await;
        let surfaces = out[2]["result"]["surfaces"].as_array().unwrap();
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0]["id"], "phone");
        assert_eq!(surfaces[0]["title"], "Phone");
        assert_eq!(surfaces[0]["item-count"], 0);
    }

    #[tokio::test]
    async fn clear_and_delete_surfaces() {
        let store = Arc::new(SurfaceStore::in_memory());
        let send = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"send-item",
            "params":{"surface-id":"s","item":{"type":"text","body":"hi"}}});
        let clear = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"clear-items","params":{"surface-id":"s"}});
        let list = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"list-items","params":{"surface-id":"s"}});
        let del = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"delete-surface","params":{"surface-id":"s"}});
        let all = serde_json::json!({"jsonrpc":"2.0","id":6,"method":"list-surfaces","params":{}});
        let out = exchange(store, &[hello(), send, clear, list, del, all]).await;
        // After clear, the surface exists but is empty.
        assert!(out[2]["result"].is_object());
        assert_eq!(out[3]["result"]["items"].as_array().unwrap().len(), 0);
        // After delete, the registry is empty.
        assert_eq!(out[5]["result"]["surfaces"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn remove_item_prunes_one() {
        let store = Arc::new(SurfaceStore::in_memory());
        let send = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"send-item",
            "params":{"surface-id":"s","item":{"type":"text","body":"a"}}});
        // Send, capture the item id, remove it, then list.
        let out1 = exchange(store.clone(), &[hello(), send]).await;
        let item_id = out1[1]["result"]["id"].as_str().unwrap().to_string();
        let remove = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"remove-item",
            "params":{"surface-id":"s","item-id":item_id}});
        let list = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"list-items","params":{"surface-id":"s"}});
        let bad = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"remove-item",
            "params":{"surface-id":"s","item-id":"nope"}});
        let out = exchange(store, &[hello(), remove, list, bad]).await;
        assert!(out[1]["result"].is_object());
        assert_eq!(out[2]["result"]["items"].as_array().unwrap().len(), 0);
        assert_eq!(out[3]["error"]["data"]["code"], "no-such-item");
    }

    #[tokio::test]
    async fn set_token_rotates_and_clears() {
        let store = Arc::new(SurfaceStore::in_memory());
        // Create tokened, then rotate the token.
        let create = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"create-surface",
            "params":{"surface-id":"s","attach-token":"t1"}});
        let rotate = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"set-token",
            "params":{"surface-id":"s","attach-token":"t2"}});
        let out = exchange(store.clone(), &[hello(), create, rotate]).await;
        assert!(out[2]["result"].is_object());
        // Old token now rejected, new one accepted, still not open.
        assert!(!store.authorize_attach("s", Some("t1")));
        assert!(store.authorize_attach("s", Some("t2")));
        assert!(!store.authorize_attach("s", None));

        // Clearing the token (no attach-token) reopens the surface.
        let clear = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"set-token",
            "params":{"surface-id":"s"}});
        let out2 = exchange(store.clone(), &[hello(), clear]).await;
        assert!(out2[1]["result"].is_object());
        assert!(store.authorize_attach("s", None));

        // Setting a token on an unknown surface is a domain error.
        let bad = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"set-token",
            "params":{"surface-id":"ghost","attach-token":"x"}});
        let out3 = exchange(store.clone(), &[hello(), bad]).await;
        assert_eq!(out3[1]["error"]["data"]["code"], "no-such-surface");
    }

    #[tokio::test]
    async fn set_view_unknown_item_errors() {
        let store = Arc::new(SurfaceStore::in_memory());
        let send = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"send-item",
            "params":{"surface-id":"s","item":{"type":"text","body":"hi"},"promote":false}});
        let bad = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"set-view",
            "params":{"surface-id":"s","item-id":"nope"}});
        let out = exchange(store, &[hello(), send, bad]).await;
        assert_eq!(out[2]["error"]["data"]["code"], "no-such-item");
    }

    #[tokio::test]
    async fn bad_surface_id_and_unknown_method() {
        let store = Arc::new(SurfaceStore::in_memory());
        let bad_id = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"list-items",
            "params":{"surface-id":"../etc"}});
        let bad_method =
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"frobnicate","params":{}});
        let out = exchange(store, &[hello(), bad_id, bad_method]).await;
        assert_eq!(out[1]["error"]["data"]["code"], "invalid-surface-id");
        assert_eq!(out[2]["error"]["data"]["code"], "method-not-found");
    }
}
