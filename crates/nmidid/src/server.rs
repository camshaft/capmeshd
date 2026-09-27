//! The control-socket server: NDJSON/JSON-RPC framing, the per-connection
//! handshake state machine, and method dispatch.
//!
//! Method handling is split from the transport so it can be unit-tested over an
//! in-memory pipe: [`serve_connection`] drives the framing over any
//! async reader/writer, and [`Session`] owns the per-connection dispatch.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

use crate::mounts::MountRegistry;
use crate::ports::PortProvider;
use crate::protocol::{
    self, CAPABILITIES, DAEMON_ID, DaemonError, INVALID_PARAMS, METHOD_NOT_FOUND, MountSpec,
    PARSE_ERROR, PROTOCOL_MAJOR, Request,
};

/// Per-connection dispatch state. One `Session` exists per accepted connection;
/// the `mounts` registry is shared daemon-wide across every connection.
pub struct Session {
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
    /// A compatible `hello` must complete before any other method (§1.2).
    hello_done: bool,
}

impl Session {
    pub fn new(ports: Arc<dyn PortProvider>, mounts: Arc<MountRegistry>) -> Self {
        Session {
            ports,
            mounts,
            hello_done: false,
        }
    }

    /// Dispatch one parsed request, yielding the JSON-RPC `result` value or a
    /// [`DaemonError`].
    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, DaemonError> {
        // The handshake gate: nothing but `hello` is served until `hello` succeeds.
        if method != "hello" && !self.hello_done {
            return Err(DaemonError::domain(
                "not-ready",
                "hello must be the first request on a connection",
            ));
        }

        match method {
            "hello" => self.handle_hello(params),
            "list-ports" => self.handle_list_ports(),
            "describe-port" => self.handle_describe_port(params),
            "mount" => self.handle_mount(params),
            "unmount" => self.handle_unmount(params),
            "mount-status" => self.handle_mount_status(params),
            other => Err(DaemonError::protocol(
                METHOD_NOT_FOUND,
                "method-not-found",
                format!("unknown method '{other}'"),
            )),
        }
    }

    fn handle_mount(&self, params: &Value) -> Result<Value, DaemonError> {
        let spec: MountSpec = serde_json::from_value(params.clone()).map_err(|e| {
            DaemonError::protocol(
                INVALID_PARAMS,
                "invalid-params",
                format!("invalid mount spec: {e}"),
            )
        })?;
        self.mounts.mount(spec)
    }

    fn handle_unmount(&self, params: &Value) -> Result<Value, DaemonError> {
        let mount_id = params
            .get("mount-id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "unmount requires a 'mount-id'",
                )
            })?;
        Ok(self.mounts.unmount(mount_id))
    }

    fn handle_mount_status(&self, params: &Value) -> Result<Value, DaemonError> {
        let mount_id = params.get("mount-id").and_then(Value::as_str);
        Ok(self.mounts.status(mount_id))
    }

    fn handle_hello(&mut self, params: &Value) -> Result<Value, DaemonError> {
        let major = params
            .get("protocol")
            .and_then(parse_major)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "hello requires a 'protocol' major version",
                )
            })?;

        if major != PROTOCOL_MAJOR {
            return Err(DaemonError::domain(
                "unsupported-protocol",
                format!("daemon speaks protocol {PROTOCOL_MAJOR}, client requested {major}"),
            )
            .with_data("supported", Value::from(PROTOCOL_MAJOR)));
        }

        self.hello_done = true;
        Ok(serde_json::json!({
            "protocol": PROTOCOL_MAJOR.to_string(),
            "daemon": DAEMON_ID,
            "capabilities": CAPABILITIES,
        }))
    }

    fn handle_list_ports(&self) -> Result<Value, DaemonError> {
        let ports = self.ports.list_ports().map_err(|e| {
            DaemonError::protocol(
                protocol::DAEMON_DOMAIN,
                "internal",
                format!("failed to enumerate ports: {e}"),
            )
        })?;
        Ok(serde_json::json!({ "ports": ports }))
    }

    fn handle_describe_port(&self, params: &Value) -> Result<Value, DaemonError> {
        let port_id = params
            .get("port-id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "describe-port requires a 'port-id'",
                )
            })?;

        let ports = self.ports.list_ports().map_err(|e| {
            DaemonError::protocol(
                protocol::DAEMON_DOMAIN,
                "internal",
                format!("failed to enumerate ports: {e}"),
            )
        })?;

        ports
            .into_iter()
            .find(|p| p.port_id == port_id)
            .map(|p| serde_json::to_value(p).expect("PortDescriptor serializes"))
            .ok_or_else(|| {
                DaemonError::domain("no-such-port", format!("no local port '{port_id}'"))
            })
    }

    /// Handle one already-parsed request, producing the response value to send
    /// (or `None` for a JSON-RPC notification, which gets no reply).
    fn respond(&mut self, req: Request) -> Option<Value> {
        let outcome = self.dispatch(&req.method, &req.params);
        match req.id {
            // A request: always answer, matching the id.
            Some(id) => Some(match outcome {
                Ok(result) => protocol::success_response(id, result),
                Err(err) => protocol::error_response(id, &err),
            }),
            // A notification: no reply, even on error (§1). Log a failure.
            None => {
                if let Err(err) = outcome {
                    debug!("notification '{}' failed: {}", req.method, err.message);
                }
                None
            }
        }
    }
}

/// Parse a protocol major version from a JSON value that may be a stringified
/// integer (`"1"`) or a number (`1`).
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
///
/// Reads one JSON value per line, dispatches it, and writes the response as a
/// single newline-terminated line. A malformed line yields a JSON-RPC parse
/// error with a null id; the connection stays open.
pub async fn serve_connection<R, W>(
    mut reader: R,
    mut writer: W,
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut session = Session::new(ports, mounts);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading control line")?;
        if n == 0 {
            // EOF: peer closed the connection.
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => session.respond(req),
            Err(e) => Some(protocol::error_response(
                Value::Null,
                &DaemonError::protocol(PARSE_ERROR, "parse-error", format!("invalid JSON: {e}")),
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
/// Any stale socket file at `path` is removed first. The socket is set to
/// owner/group read-write (`0o660`) — the local trust boundary (§1.1). Peer
/// credential enforcement is a later increment. `mounts` is the shared,
/// daemon-wide mount registry.
pub async fn run(
    path: impl AsRef<Path>,
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
) -> Result<()> {
    let path = path.as_ref();

    // Remove a stale socket from a previous run so bind() doesn't fail with
    // "address already in use".
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

    info!("nmidid control socket listening on {}", path.display());

    loop {
        let (stream, _addr) = listener.accept().await.context("accepting connection")?;
        debug!("control connection accepted");
        let ports = Arc::clone(&ports);
        let mounts = Arc::clone(&mounts);
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let reader = BufReader::new(read_half);
            if let Err(e) = serve_connection(reader, write_half, ports, mounts).await {
                warn!("control connection ended with error: {e}");
            } else {
                debug!("control connection closed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::NullMounter;
    use crate::protocol::{Format, PortDescriptor};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct StaticPorts(Vec<PortDescriptor>);

    impl PortProvider for StaticPorts {
        fn list_ports(&self) -> anyhow::Result<Vec<PortDescriptor>> {
            Ok(self.0.clone())
        }
    }

    fn sample_ports() -> Arc<dyn PortProvider> {
        Arc::new(StaticPorts(vec![
            PortDescriptor {
                port_id: "source-0".to_string(),
                kind: "stream".to_string(),
                dir: Some("source".to_string()),
                r#type: "midi".to_string(),
                name: "Keystation 49e".to_string(),
                virtualizable: true,
                formats: vec![Format::midi1()],
            },
            PortDescriptor {
                port_id: "sink-0".to_string(),
                kind: "stream".to_string(),
                dir: Some("sink".to_string()),
                r#type: "midi".to_string(),
                name: "SuperCollider".to_string(),
                virtualizable: true,
                formats: vec![Format::midi1()],
            },
        ]))
    }

    /// Drive a fixed set of request lines through a real `serve_connection`
    /// over an in-memory duplex pipe, returning each response line's parsed
    /// JSON in order.
    async fn exchange(ports: Arc<dyn PortProvider>, requests: &[Value]) -> Vec<Value> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::clone(&ports),
        ));
        let handle = tokio::spawn(async move {
            serve_connection(BufReader::new(sr), sw, ports, mounts)
                .await
                .ok();
        });

        for req in requests {
            let mut line = serde_json::to_vec(req).unwrap();
            line.push(b'\n');
            client.write_all(&line).await.unwrap();
        }
        client.flush().await.unwrap();
        // Signal EOF so the server loop ends and flushes.
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
    async fn hello_returns_daemon_identity_and_capabilities() {
        let out = exchange(sample_ports(), &[hello()]).await;
        assert_eq!(out.len(), 1);
        let r = &out[0]["result"];
        assert_eq!(r["protocol"], "1");
        assert_eq!(r["daemon"], DAEMON_ID);
        let caps: Vec<String> = serde_json::from_value(r["capabilities"].clone()).unwrap();
        assert!(caps.contains(&"midi1".to_string()));
        assert!(caps.contains(&"virtual-endpoints".to_string()));
    }

    #[tokio::test]
    async fn methods_before_hello_are_not_ready() {
        let list = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"list-ports","params":{}});
        let out = exchange(sample_ports(), &[list]).await;
        assert_eq!(out[0]["error"]["data"]["code"], "not-ready");
    }

    #[tokio::test]
    async fn list_ports_after_hello_returns_descriptors() {
        let list = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"list-ports","params":{}});
        let out = exchange(sample_ports(), &[hello(), list]).await;
        let ports = out[1]["result"]["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0]["port-id"], "source-0");
        assert_eq!(ports[0]["dir"], "source");
        assert_eq!(ports[0]["type"], "midi");
        assert_eq!(ports[0]["formats"][0]["codec"], "midi1");
    }

    #[tokio::test]
    async fn describe_known_and_unknown_port() {
        let known = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"describe-port","params":{"port-id":"sink-0"}});
        let unknown = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"describe-port","params":{"port-id":"nope"}});
        let out = exchange(sample_ports(), &[hello(), known, unknown]).await;
        assert_eq!(out[1]["result"]["name"], "SuperCollider");
        assert_eq!(out[2]["error"]["data"]["code"], "no-such-port");
    }

    #[tokio::test]
    async fn mount_roundtrip_over_socket() {
        let mount = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"mount","params":{
            "mount-id":"m1","role":"mirror-source",
            "local":{"virtual":true,"name":"laptop: Keystation"},
            "remote":{"host":"laptop","addr":"192.168.1.23","port":5004,"port-id":"source-0"},
            "format":{"codec":"midi1"}}});
        let status =
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"mount-status","params":{}});
        let unmount = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"unmount","params":{"mount-id":"m1"}});
        let status2 =
            serde_json::json!({"jsonrpc":"2.0","id":5,"method":"mount-status","params":{}});
        let out = exchange(sample_ports(), &[hello(), mount, status, unmount, status2]).await;
        assert_eq!(out[1]["result"]["state"], "connecting");
        assert_eq!(out[1]["result"]["mount-id"], "m1");
        assert_eq!(out[2]["result"]["mounts"].as_array().unwrap().len(), 1);
        assert_eq!(out[2]["result"]["mounts"][0]["state"], "connecting");
        assert!(out[3]["result"].is_object()); // unmount → {}
        assert_eq!(out[4]["result"]["mounts"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn mount_unknown_remote_port_is_no_such_port() {
        let mount = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"mount","params":{
            "mount-id":"m1","role":"mirror-source",
            "local":{"virtual":true},
            "remote":{"addr":"192.168.1.23","port":5004,"port-id":"ghost-9"},
            "format":{"codec":"midi1"}}});
        let out = exchange(sample_ports(), &[hello(), mount]).await;
        assert_eq!(out[1]["error"]["data"]["code"], "no-such-port");
    }

    #[tokio::test]
    async fn unsupported_protocol_major_is_rejected() {
        let bad = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"hello",
            "params":{"protocol":"2","client":"capmeshd/0.1"}});
        let out = exchange(sample_ports(), &[bad]).await;
        assert_eq!(out[0]["error"]["data"]["code"], "unsupported-protocol");
    }

    #[tokio::test]
    async fn malformed_line_yields_parse_error_and_keeps_serving() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let ports = sample_ports();
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::clone(&ports),
        ));
        let handle = tokio::spawn(async move {
            serve_connection(BufReader::new(sr), sw, ports, mounts)
                .await
                .ok()
        });

        client.write_all(b"{ this is not json\n").await.unwrap();
        let mut hello_line = serde_json::to_vec(&hello()).unwrap();
        hello_line.push(b'\n');
        client.write_all(&hello_line).await.unwrap();
        client.shutdown().await.unwrap();

        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        handle.await.unwrap();

        let lines: Vec<Value> = buf
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["error"]["data"]["code"], "parse-error");
        assert_eq!(lines[0]["id"], Value::Null);
        // The connection kept serving: hello still got a result.
        assert_eq!(lines[1]["result"]["daemon"], DAEMON_ID);
    }
}
