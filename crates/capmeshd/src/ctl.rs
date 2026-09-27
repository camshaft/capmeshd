//! The `capmesh-ctl` client — capmeshd's side of the data-plane control socket
//! (docs/CONTROL-PROTOCOL.md). capmeshd is the CLIENT: it issues JSON-RPC 2.0 requests
//! over a newline-delimited-JSON (NDJSON) Unix-domain-socket connection to a first-party
//! data-plane daemon (the first being `nmidid`), which replies with a matching `result`
//! or `error` and may emit unsolicited notifications.
//!
//! This is the `midi` adapter's transport to `nmidid`. B1a implements the read path of the
//! handshake + enumeration — `hello` (which MUST be first, §1.2), `list-ports`, and
//! `describe-port` (§3). `mount`/`unmount`/`mount-status` and the daemon→client
//! notifications land in following slices as `nmidid` grows them.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// A supported wire format for a port (CONTROL-PROTOCOL §2): a `codec` plus
/// codec-specific params (e.g. MIDI `ump` carries an optional `group`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Format {
    pub codec: String,
    /// Codec-specific params, flattened onto the same object (e.g. `{"group": 0}`).
    #[serde(flatten, default)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// A typed port a daemon node exposes (CONTROL-PROTOCOL §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortDescriptor {
    /// Stable within this daemon/host.
    #[serde(rename = "port-id")]
    pub port_id: String,
    /// `"stream"` | `"rpc"`.
    pub kind: String,
    /// Stream ports only: `"source"` | `"sink"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Capability type: `midi | audio | video | ...`.
    #[serde(rename = "type")]
    pub type_: String,
    pub name: String,
    /// The daemon can materialize a local virtual mirror (DESIGN §3.2).
    #[serde(default)]
    pub virtualizable: bool,
    /// Preference-ordered supported formats (§4 negotiation preserves this order).
    #[serde(default)]
    pub formats: Vec<Format>,
}

/// `hello` result (§1.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HelloResult {
    pub protocol: String,
    pub daemon: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// `list-ports` result (§3).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ListPortsResult {
    pub ports: Vec<PortDescriptor>,
}

/// Which end of a mount the daemon materializes (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountRole {
    /// Create a local virtual **source** fed by the remote source (keyboard-shows-up case).
    MirrorSource,
    /// Create a local virtual **sink** that forwards to the remote sink.
    MirrorSink,
    /// Connect an existing local **real** port to the remote (no virtual endpoint).
    Link,
}

/// The local endpoint the daemon owns/creates for a mount (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalEndpoint {
    /// Create a virtual endpoint (requires `port.virtualizable`).
    #[serde(rename = "virtual", default)]
    pub is_virtual: bool,
    /// Display name for the virtual device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The remote peer a mount connects to, DIRECT peer-to-peer (§3.1). `addr` is typed as an
/// [`IpAddr`] to enforce the connect-by-IP rule — a `.local`/`.lan` name won't deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteEndpoint {
    pub host: String,
    /// ALWAYS the IP from the mDNS record (DESIGN §5), never a `.local`/`.lan` name.
    pub addr: IpAddr,
    pub port: u16,
    #[serde(rename = "port-id")]
    pub port_id: String,
}

/// A mount request carrying the negotiated format (§3.1). `mount-id` is the reconciler's
/// idempotency key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountSpec {
    #[serde(rename = "mount-id")]
    pub mount_id: String,
    pub role: MountRole,
    pub local: LocalEndpoint,
    pub remote: RemoteEndpoint,
    /// The CHOSEN format — the result of negotiation (§4), a single format not a list.
    pub format: Format,
}

/// A mount's lifecycle state (§3.2). The reconciler self-heals off these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountState {
    Pending,
    Connecting,
    Active,
    Degraded,
    Failed,
    TornDown,
}

/// Throughput/liveness counters for a live mount (§3.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MountStats {
    #[serde(rename = "bytes-in", default)]
    pub bytes_in: u64,
    #[serde(rename = "bytes-out", default)]
    pub bytes_out: u64,
    #[serde(rename = "last-event", default)]
    pub last_event: Option<String>,
}

/// The status of one mount (§3.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MountStatus {
    #[serde(rename = "mount-id")]
    pub mount_id: String,
    pub state: MountState,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub stats: Option<MountStats>,
    #[serde(default)]
    pub detail: Option<String>,
}

/// `mount` result (§3): the established mount's id + current state.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MountResult {
    #[serde(rename = "mount-id")]
    pub mount_id: String,
    pub state: MountState,
}

/// `mount-status` result (§3): one or all live mounts.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MountStatusResult {
    pub mounts: Vec<MountStatus>,
}

/// An unsolicited daemon→client notification (§5). capmeshd re-advertises / re-reconciles
/// on these — this is what powers self-heal and auto-mount without polling.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    /// A device appeared (hot-plug) — gated behind the `hotplug-events` capability.
    PortAdded(PortDescriptor),
    /// A device went away → dependent mounts should be torn down.
    PortRemoved { port_id: String },
    /// A mount changed state (§3.2) — the reconciler self-heals off this.
    MountState {
        mount_id: String,
        state: MountState,
        detail: Option<String>,
        stats: Option<MountStats>,
    },
    /// A notification method this client version does not model (forward-compat).
    Other { method: String },
}

impl Notification {
    /// Parse a notification from its `method` + `params` (§5).
    fn from_method(method: &str, params: serde_json::Value) -> Result<Self, serde_json::Error> {
        Ok(match method {
            "port-added" => {
                #[derive(Deserialize)]
                struct P {
                    port: PortDescriptor,
                }
                Notification::PortAdded(serde_json::from_value::<P>(params)?.port)
            }
            "port-removed" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(rename = "port-id")]
                    port_id: String,
                }
                Notification::PortRemoved {
                    port_id: serde_json::from_value::<P>(params)?.port_id,
                }
            }
            "mount-state" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(rename = "mount-id")]
                    mount_id: String,
                    state: MountState,
                    #[serde(default)]
                    detail: Option<String>,
                    #[serde(default)]
                    stats: Option<MountStats>,
                }
                let p: P = serde_json::from_value(params)?;
                Notification::MountState {
                    mount_id: p.mount_id,
                    state: p.state,
                    detail: p.detail,
                    stats: p.stats,
                }
            }
            other => Notification::Other {
                method: other.to_string(),
            },
        })
    }
}

/// The `data` object of a JSON-RPC error — carries the machine `code` (§6).
#[derive(Debug, Clone, Deserialize)]
struct RpcErrorData {
    #[serde(default)]
    code: Option<String>,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Deserialize)]
struct RpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<RpcErrorData>,
}

/// A JSON-RPC response frame.
#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct Request<'a, P: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: P,
}

#[derive(Debug, Serialize)]
struct HelloParams {
    protocol: &'static str,
    client: &'static str,
}

/// The protocol major version capmeshd speaks (§1.2).
pub const PROTOCOL_VERSION: &str = "1";
/// The `client` identifier sent in `hello`.
pub const CLIENT_ID: &str = "capmeshd/0.1";

/// Errors from driving a `capmesh-ctl` socket.
#[derive(Debug, thiserror::Error)]
pub enum CtlError {
    #[error("capmesh-ctl socket io: {0}")]
    Io(#[from] std::io::Error),
    #[error("capmesh-ctl json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daemon closed the capmesh-ctl connection")]
    Closed,
    /// A JSON-RPC `error` reply. `code` is the machine code from `data.code` (§6) when
    /// present, else the stringified numeric code.
    #[error("daemon declined [{code}]: {message}")]
    Rpc { code: String, message: String },
}

/// A connection to a data-plane daemon's `capmesh-ctl` socket. One connection per
/// capmeshd↔daemon pair; capmeshd issues requests, the daemon replies.
pub struct CtlClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    line: String,
}

impl CtlClient {
    /// Connect to the daemon's Unix control socket. The caller MUST call [`hello`] before
    /// any other method (§1.2); the daemon rejects everything else with `not-ready`.
    ///
    /// [`hello`]: CtlClient::hello
    pub async fn connect(socket: &Path) -> Result<Self, CtlError> {
        let stream = UnixStream::connect(socket).await?;
        let (read, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read),
            writer,
            next_id: 1,
            line: String::new(),
        })
    }

    /// Handshake (§1.2). MUST be the first call on the connection.
    pub async fn hello(&mut self) -> Result<HelloResult, CtlError> {
        let value = self
            .call(
                "hello",
                HelloParams {
                    protocol: PROTOCOL_VERSION,
                    client: CLIENT_ID,
                },
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Enumerate the daemon's current local ports (§3) — feeds advertisement + auto-mount.
    pub async fn list_ports(&mut self) -> Result<ListPortsResult, CtlError> {
        let value = self.call("list-ports", serde_json::json!({})).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Full descriptor for one port (§3). `no-such-port` if the id is unknown.
    pub async fn describe_port(&mut self, port_id: &str) -> Result<PortDescriptor, CtlError> {
        let value = self
            .call("describe-port", serde_json::json!({ "port-id": port_id }))
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Establish a mount (§3). Idempotent on `spec.mount_id` — re-sending the same spec is
    /// safe, which is what lets the reconciler converge desired-state.
    pub async fn mount(&mut self, spec: &MountSpec) -> Result<MountResult, CtlError> {
        let value = self.call("mount", spec).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Tear a mount down (§3). Idempotent — an unknown `mount_id` is a successful no-op.
    pub async fn unmount(&mut self, mount_id: &str) -> Result<(), CtlError> {
        self.call("unmount", serde_json::json!({ "mount-id": mount_id }))
            .await?;
        Ok(())
    }

    /// One mount's status, or all live mounts when `mount_id` is `None` (§3).
    pub async fn mount_status(
        &mut self,
        mount_id: Option<&str>,
    ) -> Result<MountStatusResult, CtlError> {
        let params = match mount_id {
            Some(id) => serde_json::json!({ "mount-id": id }),
            None => serde_json::json!({}),
        };
        let value = self.call("mount-status", params).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Issue one request and read its matching reply, returning `result` or mapping the
    /// JSON-RPC `error` to [`CtlError::Rpc`]. Unsolicited daemon notifications (no `id`)
    /// that arrive first are skipped — they are handled by the reconciler's event path in
    /// a later slice, not here.
    async fn call<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<serde_json::Value, CtlError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut buf = serde_json::to_vec(&req)?;
        buf.push(b'\n');
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        self.read_response().await
    }

    /// Read the next non-empty JSON frame (one NDJSON line) from the socket.
    async fn read_line_value(&mut self) -> Result<serde_json::Value, CtlError> {
        loop {
            self.line.clear();
            let n = self.reader.read_line(&mut self.line).await?;
            if n == 0 {
                return Err(CtlError::Closed);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return Ok(serde_json::from_str(trimmed)?);
        }
    }

    async fn read_response(&mut self) -> Result<serde_json::Value, CtlError> {
        loop {
            let value = self.read_line_value().await?;
            // A daemon→client notification (a `method`, no `id`) — skip; not our reply.
            if value.get("method").is_some() && value.get("id").is_none() {
                continue;
            }
            let resp: Response = serde_json::from_value(value)?;
            if let Some(err) = resp.error {
                let code = err
                    .data
                    .and_then(|d| d.code)
                    .unwrap_or_else(|| err.code.to_string());
                return Err(CtlError::Rpc {
                    code,
                    message: err.message,
                });
            }
            return Ok(resp.result.unwrap_or(serde_json::Value::Null));
        }
    }

    /// Await the next unsolicited daemon→client notification (§5). Skips response frames
    /// (no request is outstanding in the listen phase). This is what the reconciler reacts
    /// to instead of polling `mount-status` once the daemon emits mount-state / hotplug.
    pub async fn next_notification(&mut self) -> Result<Notification, CtlError> {
        loop {
            let value = self.read_line_value().await?;
            let method = match value.get("method").and_then(|m| m.as_str()) {
                Some(m) => m.to_string(),
                None => continue, // a response with no outstanding request — ignore
            };
            let params = value
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            return Ok(Notification::from_method(&method, params)?);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_request_serializes_to_the_wire_shape() {
        let req = Request {
            jsonrpc: "2.0",
            id: 1,
            method: "hello",
            params: HelloParams {
                protocol: PROTOCOL_VERSION,
                client: CLIENT_ID,
            },
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "hello");
        assert_eq!(v["params"]["protocol"], "1");
        assert_eq!(v["params"]["client"], "capmeshd/0.1");
    }

    #[test]
    fn parses_the_port_descriptor_example() {
        // The §2 PortDescriptor example, preference-ordered formats with a ump group param.
        let json = r#"{
            "port-id": "kbd-0", "kind": "stream", "dir": "source", "type": "midi",
            "name": "Keystation 49e", "virtualizable": true,
            "formats": [{"codec":"ump","group":0},{"codec":"midi1"}]
        }"#;
        let pd: PortDescriptor = serde_json::from_str(json).unwrap();
        assert_eq!(pd.port_id, "kbd-0");
        assert_eq!(pd.kind, "stream");
        assert_eq!(pd.dir.as_deref(), Some("source"));
        assert_eq!(pd.type_, "midi");
        assert!(pd.virtualizable);
        assert_eq!(pd.formats.len(), 2);
        assert_eq!(pd.formats[0].codec, "ump");
        assert_eq!(
            pd.formats[0].params.get("group"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(pd.formats[1].codec, "midi1");
    }

    #[test]
    fn parses_the_hello_result_example() {
        let json = r#"{"protocol":"1","daemon":"nmidid/0.1",
            "capabilities":["virtual-endpoints","hotplug-events","midi1","ump"]}"#;
        let hr: HelloResult = serde_json::from_str(json).unwrap();
        assert_eq!(hr.protocol, "1");
        assert_eq!(hr.daemon, "nmidid/0.1");
        assert!(hr.capabilities.contains(&"midi1".to_string()));
    }

    #[test]
    fn error_reply_maps_to_the_machine_code() {
        // The §6 no-common-format example.
        let json = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32001,
            "message":"no common format","data":{"code":"no-common-format",
            "local":[{"codec":"midi1"}],"remote":[{"codec":"ump"}]}}}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        let err = resp.error.expect("error present");
        let code = err.data.and_then(|d| d.code).unwrap();
        assert_eq!(code, "no-common-format");
        assert_eq!(err.message, "no common format");
    }

    #[test]
    fn format_round_trips_with_params() {
        let f = Format {
            codec: "ump".into(),
            params: serde_json::Map::from_iter([("group".to_string(), serde_json::json!(3))]),
        };
        let back: Format = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
    }

    /// End-to-end over a real Unix socket: a fake daemon answers hello + list-ports and
    /// the client drives the NDJSON handshake + enumeration. Pins the framing + call loop.
    #[tokio::test]
    async fn client_round_trips_hello_and_list_ports_over_a_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "capmesh-ctl-test-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = TokioBufReader::new(r).lines();

            let req = lines.next_line().await.unwrap().unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "hello");
            let resp = serde_json::json!({"jsonrpc":"2.0","id": v["id"],
                "result":{"protocol":"1","daemon":"nmidid/0.1",
                          "capabilities":["virtual-endpoints","midi1"]}});
            w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();

            let req = lines.next_line().await.unwrap().unwrap();
            let v: serde_json::Value = serde_json::from_str(&req).unwrap();
            assert_eq!(v["method"], "list-ports");
            let resp = serde_json::json!({"jsonrpc":"2.0","id": v["id"],"result":{"ports":[
                {"port-id":"kbd-0","kind":"stream","dir":"source","type":"midi",
                 "name":"Keystation 49e","virtualizable":true,"formats":[{"codec":"midi1"}]}]}});
            w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        });

        let mut client = CtlClient::connect(&path).await.unwrap();
        let hello = client.hello().await.unwrap();
        assert_eq!(hello.daemon, "nmidid/0.1");
        let ports = client.list_ports().await.unwrap();
        assert_eq!(ports.ports.len(), 1);
        assert_eq!(ports.ports[0].port_id, "kbd-0");
        assert_eq!(ports.ports[0].dir.as_deref(), Some("source"));

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_and_reserializes_the_mount_spec_example() {
        // The §3.1 MountSpec example.
        let json = r#"{"mount-id":"b1f0","role":"mirror-source",
            "local":{"virtual":true,"name":"laptop: Keystation 49e"},
            "remote":{"host":"laptop","addr":"192.168.1.23","port":5004,"port-id":"kbd-0"},
            "format":{"codec":"midi1"}}"#;
        let spec: MountSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.mount_id, "b1f0");
        assert_eq!(spec.role, MountRole::MirrorSource);
        assert!(spec.local.is_virtual);
        assert_eq!(spec.local.name.as_deref(), Some("laptop: Keystation 49e"));
        assert_eq!(spec.remote.addr, "192.168.1.23".parse::<IpAddr>().unwrap());
        assert_eq!(spec.remote.port, 5004);
        assert_eq!(spec.remote.port_id, "kbd-0");
        assert_eq!(spec.format.codec, "midi1");

        // Re-serialization uses the on-wire hyphenated/renamed keys.
        let v = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["mount-id"], "b1f0");
        assert_eq!(v["role"], "mirror-source");
        assert_eq!(v["local"]["virtual"], true);
        assert_eq!(v["remote"]["port-id"], "kbd-0");
        assert_eq!(v["remote"]["addr"], "192.168.1.23");
    }

    #[test]
    fn parses_the_mount_status_example() {
        // The §3.2 MountStatus example.
        let json = r#"{"mount-id":"b1f0","state":"active","since":"2026-09-27T18:04:11Z",
            "stats":{"bytes-in":10432,"bytes-out":0,"last-event":"2026-09-27T18:07:52Z"},
            "detail":null}"#;
        let st: MountStatus = serde_json::from_str(json).unwrap();
        assert_eq!(st.mount_id, "b1f0");
        assert_eq!(st.state, MountState::Active);
        assert_eq!(st.since.as_deref(), Some("2026-09-27T18:04:11Z"));
        let stats = st.stats.expect("stats present");
        assert_eq!(stats.bytes_in, 10432);
        assert_eq!(stats.bytes_out, 0);
        assert_eq!(stats.last_event.as_deref(), Some("2026-09-27T18:07:52Z"));
        assert!(st.detail.is_none());
    }

    #[test]
    fn mount_state_kebab_round_trips() {
        assert_eq!(
            serde_json::to_value(MountState::TornDown).unwrap(),
            "torn-down"
        );
        let s: MountState = serde_json::from_str("\"connecting\"").unwrap();
        assert_eq!(s, MountState::Connecting);
    }

    #[test]
    fn remote_endpoint_rejects_a_non_ip_addr() {
        // Connect-by-IP (DESIGN §5): a `.local`/`.lan` name must not deserialize.
        let json = r#"{"host":"laptop","addr":"keyboard.local","port":5004,"port-id":"kbd-0"}"#;
        assert!(serde_json::from_str::<RemoteEndpoint>(json).is_err());
    }

    #[test]
    fn parses_notification_variants() {
        let ms = Notification::from_method(
            "mount-state",
            serde_json::json!({"mount-id":"m1","state":"active",
                "stats":{"bytes-in":42,"bytes-out":0},"detail":null}),
        )
        .unwrap();
        match ms {
            Notification::MountState {
                mount_id,
                state,
                stats,
                ..
            } => {
                assert_eq!(mount_id, "m1");
                assert_eq!(state, MountState::Active);
                assert_eq!(stats.unwrap().bytes_in, 42);
            }
            other => panic!("expected MountState, got {other:?}"),
        }

        let pr = Notification::from_method("port-removed", serde_json::json!({"port-id":"kbd-0"}))
            .unwrap();
        assert_eq!(
            pr,
            Notification::PortRemoved {
                port_id: "kbd-0".into()
            }
        );

        let unknown = Notification::from_method("something-new", serde_json::json!({})).unwrap();
        assert_eq!(
            unknown,
            Notification::Other {
                method: "something-new".into()
            }
        );
    }

    #[tokio::test]
    async fn next_notification_reads_a_mount_state_over_a_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
        use tokio::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "capmesh-notif-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = TokioBufReader::new(r).lines();
            // hello
            let _ = lines.next_line().await.unwrap().unwrap();
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol\":\"1\",\"daemon\":\"nmidid/0.1\",\"capabilities\":[]}}\n").await.unwrap();
            // then an unsolicited mount-state notification (no id)
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"mount-state\",\"params\":{\"mount-id\":\"m1\",\"state\":\"active\"}}\n").await.unwrap();
        });

        let mut client = CtlClient::connect(&path).await.unwrap();
        client.hello().await.unwrap();
        let n = client.next_notification().await.unwrap();
        assert_eq!(
            n,
            Notification::MountState {
                mount_id: "m1".into(),
                state: MountState::Active,
                detail: None,
                stats: None,
            }
        );

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
