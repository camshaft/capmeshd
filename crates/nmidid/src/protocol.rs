//! Wire types for the `capmesh-ctl` control protocol (see
//! `capmeshd/docs/CONTROL-PROTOCOL.md`). Transport is newline-delimited JSON
//! (NDJSON) carrying JSON-RPC 2.0 messages over a local Unix domain socket.
//!
//! This module defines the framing plus the wire types for every control
//! method the daemon serves — `hello`, `list-ports`, `describe-port`, `mount`,
//! `unmount`, `mount-status` — and the unsolicited daemon→client notifications
//! (`mount-state`, `port-added`/`port-removed`). All are implemented (dispatch
//! in [`crate::server`], mounts in [`crate::mounts`], hot-plug in
//! [`crate::hotplug`]); the wire shapes follow the frozen spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The single-integer protocol major version this daemon speaks (§1.2).
pub const PROTOCOL_MAJOR: u32 = 1;

/// Daemon identity returned in the `hello` result.
pub const DAEMON_ID: &str = concat!("nmidid/", env!("CARGO_PKG_VERSION"));

/// Optional features advertised in the `hello` result (§5, §7).
///
/// `midi1` is the always-supported codec; `ump` is intentionally absent until
/// the converter lands. `virtual-endpoints` (the daemon materializes local
/// virtual MIDI ports, see [`crate::mounts`]) and `hotplug-events` (unsolicited
/// `port-added`/`port-removed`, see [`crate::hotplug`]) are both implemented.
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

// ---------------------------------------------------------------------------
// Mount wire types (§3) — the `mount` / `unmount` / `mount-status` methods.
// ---------------------------------------------------------------------------

/// Which end of a mount the daemon materializes (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountRole {
    /// Create a local virtual **source** fed by the remote source (the
    /// keyboard-shows-up case). The only role implemented in M0a.
    MirrorSource,
    /// Create a local virtual **sink** that forwards to the remote sink.
    MirrorSink,
    /// Connect an existing local **real** port to the remote (no virtual endpoint).
    Link,
}

/// The local endpoint the daemon owns/creates (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LocalEndpoint {
    /// Create a virtual endpoint (requires the port's `virtualizable`).
    #[serde(default)]
    pub r#virtual: bool,
    /// Display name for the virtual device.
    #[serde(default)]
    pub name: Option<String>,
}

/// The remote peer this host connects to — DIRECT, p2p (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RemoteEndpoint {
    #[serde(default)]
    pub host: Option<String>,
    /// ALWAYS the IP from the mDNS record, never a `.local`/`.lan` name.
    pub addr: String,
    /// The AppleMIDI **control** port (e.g. the SRV port from `_apple-midi._udp`).
    /// The daemon derives the data-plane port as `port + 1` (AppleMIDI
    /// convention); callers pass the control port here, not `port + 1`.
    pub port: u16,
    pub port_id: String,
}

/// A mount request carrying the negotiated format (§3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MountSpec {
    /// capmeshd-assigned id; the reconciler's idempotency key.
    pub mount_id: String,
    pub role: MountRole,
    pub local: LocalEndpoint,
    pub remote: RemoteEndpoint,
    /// The CHOSEN format — result of negotiation (§4), not a list.
    pub format: Format,
}

/// The lifecycle state of a mount (§3.2).
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

/// Per-mount counters (§3.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MountStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<String>,
}

/// A live mount's status (§3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MountStatus {
    pub mount_id: String,
    pub state: MountState,
    pub since: String,
    pub stats: MountStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `PortDescriptor` nmidid emits over `capmesh-ctl` is also the payload a
    /// peer fetches over the mesh (docs/MESH-PROTOCOL.md), and `capmesh-model`
    /// holds the canonical definition capmeshd's client deserializes. These are
    /// two independent structs, so guard their wire compatibility: a round-trip
    /// through `capmesh_model::PortDescriptor` must preserve every field. This
    /// fails CI if either side renames/adds/drops a field and the wire drifts.
    #[test]
    fn port_descriptor_wire_matches_capmesh_model() {
        let ours = PortDescriptor {
            port_id: "source-keystation-49e".to_string(),
            kind: "stream".to_string(),
            dir: Some("source".to_string()),
            r#type: "midi".to_string(),
            name: "Keystation 49e".to_string(),
            virtualizable: true,
            formats: vec![Format {
                codec: "midi1".to_string(),
                params: {
                    let mut m = Map::new();
                    m.insert("group".to_string(), Value::from(0));
                    m
                },
            }],
        };

        // ours → JSON → capmesh-model must parse and preserve the JSON verbatim.
        let ours_json = serde_json::to_value(&ours).unwrap();
        let theirs: capmesh_model::PortDescriptor =
            serde_json::from_value(ours_json.clone()).unwrap();
        let theirs_json = serde_json::to_value(&theirs).unwrap();
        assert_eq!(
            ours_json, theirs_json,
            "PortDescriptor wire shape drifted between nmidid and capmesh-model"
        );

        // Spot-check the renamed/keyword fields survive the round-trip.
        assert_eq!(theirs.port_id, ours.port_id);
        assert_eq!(theirs.type_, ours.r#type);
        assert_eq!(theirs.dir, ours.dir);
        assert_eq!(theirs.formats[0].codec, "midi1");
        assert_eq!(theirs.formats[0].params["group"], Value::from(0));
    }

    /// The `mount-status` result and the §5 `mount-state` notification nmidid emits
    /// are read by capmeshd's client, which deserializes them into the canonical
    /// `capmesh-model` mount types — and that is exactly how the M0 rehearsal harness
    /// observes the data plane (mount `state` reaching `active`, `stats.bytes-in`
    /// growing, and the failure `detail`). These are two independent structs, so
    /// guard the wire the same way [`port_descriptor_wire_matches_capmesh_model`]
    /// does: the fields the reconciler and the harness read must survive a round-trip
    /// into `capmesh_model`. This fails CI if either side renames a mount-status field
    /// (e.g. `bytes-in` ↔ `bytes_in`) and the wire silently drifts.
    #[test]
    fn mount_status_wire_matches_capmesh_model() {
        // (1) The `mount-status` result shape `{"mounts": [MountStatus, ...]}` (§3.2),
        //     which a root-run `capmeshd mount-status` reads on the SC host.
        let active = MountStatus {
            mount_id: "m1".to_string(),
            state: MountState::Active,
            since: "2026-09-27T18:07:52Z".to_string(),
            stats: MountStats {
                bytes_in: 10432,
                bytes_out: 0,
                last_event: None,
            },
            detail: None,
        };
        let result_json = serde_json::json!({ "mounts": [serde_json::to_value(&active).unwrap()] });
        let theirs: capmesh_model::MountStatusResult =
            serde_json::from_value(result_json).expect("mount-status result parses as capmesh-model");
        assert_eq!(theirs.mounts.len(), 1);
        assert_eq!(theirs.mounts[0].state, capmesh_model::MountState::Active);
        assert_eq!(
            theirs.mounts[0]
                .stats
                .as_ref()
                .expect("stats present")
                .bytes_in,
            10432,
            "mount-status stats.bytes-in must survive into capmesh-model"
        );

        // (2) The unsolicited §5 `mount-state` notification, which capmeshd subscribes
        //     to — including the failure `detail` the reject/unresponsive scenarios read.
        let failed = MountStatus {
            mount_id: "m1".to_string(),
            state: MountState::Failed,
            since: "2026-09-27T18:07:52Z".to_string(),
            stats: MountStats::default(),
            detail: Some("remote rejected the invitation".to_string()),
        };
        let notif = crate::mounts::mount_state_notification(&failed);
        let parsed = capmesh_model::Notification::from_method(
            notif["method"].as_str().unwrap(),
            notif["params"].clone(),
        )
        .expect("mount-state notification parses as capmesh-model");
        match parsed {
            capmesh_model::Notification::MountState { state, detail, .. } => {
                assert_eq!(state, capmesh_model::MountState::Failed);
                assert_eq!(detail.as_deref(), Some("remote rejected the invitation"));
            }
            other => panic!("expected a MountState notification, got {other:?}"),
        }
    }
}
