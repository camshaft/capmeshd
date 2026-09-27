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

    async fn read_response(&mut self) -> Result<serde_json::Value, CtlError> {
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
            let value: serde_json::Value = serde_json::from_str(trimmed)?;
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
}
