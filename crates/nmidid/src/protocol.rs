//! Wire types for the `capmesh-ctl` control protocol (see
//! `capmeshd/docs/CONTROL-PROTOCOL.md`). Transport is newline-delimited JSON
//! (NDJSON) carrying JSON-RPC 2.0 messages over a local Unix domain socket.
//!
//! This slice (M0a increment 1) implements the framing plus the `hello`,
//! `list-ports` and `describe-port` methods. The remaining methods (`mount`,
//! `unmount`, `mount-status`) and the daemon→client notifications land in later
//! increments; their wire shapes are already fixed by the frozen spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The single-integer protocol major version this daemon speaks (§1.2).
pub const PROTOCOL_MAJOR: u32 = 1;

/// Daemon identity returned in the `hello` result.
pub const DAEMON_ID: &str = concat!("nmidid/", env!("CARGO_PKG_VERSION"));

/// Optional features advertised in the `hello` result (§5, §7).
///
/// `midi1` is the always-supported codec; `ump` is intentionally absent until
/// the converter lands. `virtual-endpoints`/`hotplug-events` name capabilities
/// later increments implement.
pub const CAPABILITIES: &[&str] = &["virtual-endpoints", "hotplug-events", "midi1"];

/// A single incoming JSON-RPC 2.0 request line.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Absent for a notification; present for a request expecting a reply.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A `Format` is `{"codec": "<name>", ...codec-specific params}` (§2). The extra
/// params are kept verbatim so audio/video daemons reuse the same shape without
/// a protocol change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Format {
    pub codec: String,
    #[serde(flatten)]
    pub params: Map<String, Value>,
}

impl Format {
    /// Classic MIDI 1.0 byte stream — the only codec this daemon offers today.
    pub fn midi1() -> Self {
        Format {
            codec: "midi1".to_string(),
            params: Map::new(),
        }
    }
}

/// A typed port the daemon owns (§2). `dir` is present only for stream ports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PortDescriptor {
    /// Stable within this daemon/host.
    pub port_id: String,
    /// `"stream"` | `"rpc"`.
    pub kind: String,
    /// `"source"` | `"sink"` — stream ports only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Capability type, e.g. `"midi"`.
    pub r#type: String,
    pub name: String,
    /// Daemon can materialize a local virtual mirror (§3.2).
    pub virtualizable: bool,
    /// Preference-ordered list of supported formats.
    pub formats: Vec<Format>,
}

/// A JSON-RPC error to return to the client. The machine code lives in
/// `data.code` (§6); the numeric `code` distinguishes protocol-level errors
/// (parse/invalid/method) from daemon-domain errors.
#[derive(Debug, Clone)]
pub struct DaemonError {
    pub number: i64,
    pub code: &'static str,
    pub message: String,
    pub data_extra: Map<String, Value>,
}

/// JSON-RPC parse error (malformed line).
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC method-not-found.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC invalid params.
pub const INVALID_PARAMS: i64 = -32602;
/// Daemon-domain error (the `data.code` string carries the specifics).
pub const DAEMON_DOMAIN: i64 = -32001;

impl DaemonError {
    /// A daemon-domain error (numeric `-32001`) with a `data.code` string (§6).
    pub fn domain(code: &'static str, message: impl Into<String>) -> Self {
        DaemonError {
            number: DAEMON_DOMAIN,
            code,
            message: message.into(),
            data_extra: Map::new(),
        }
    }

    /// A protocol-level error with an explicit numeric code.
    pub fn protocol(number: i64, code: &'static str, message: impl Into<String>) -> Self {
        DaemonError {
            number,
            code,
            message: message.into(),
            data_extra: Map::new(),
        }
    }

    /// Attach an extra field to the error's `data` object.
    pub fn with_data(mut self, key: &str, value: Value) -> Self {
        self.data_extra.insert(key.to_string(), value);
        self
    }

    /// Render the JSON-RPC `error` object.
    fn to_error_object(&self) -> Value {
        let mut data = Map::new();
        data.insert("code".to_string(), Value::String(self.code.to_string()));
        for (k, v) in &self.data_extra {
            data.insert(k.clone(), v.clone());
        }
        serde_json::json!({
            "code": self.number,
            "message": self.message,
            "data": Value::Object(data),
        })
    }
}

/// Build a JSON-RPC success response value for a given request id.
pub fn success_response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC error response value for a given request id.
pub fn error_response(id: Value, err: &DaemonError) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": err.to_error_object(),
    })
}
