//! The capmesh typed model — the `capmesh-ctl` value types shared across the mesh
//! (docs/CONTROL-PROTOCOL.md): the port descriptor, the mount request/status shapes, and
//! the daemon→client notification enum. These are pure serde data types with no transport;
//! the `capmesh-ctl` client (capmeshd) and any other tool depend on them rather than
//! re-declaring the wire shapes.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

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
    pub fn from_method(
        method: &str,
        params: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
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

/// A capability's rich descriptor as served by the peer↔peer mesh control endpoint
/// (docs/MESH-PROTOCOL.md §3): the response body of `GET /caps/<id>`. It resolves the coarse
/// `_capmesh._tcp` advert's `descr` pointer into the full typed shape a mount needs — the
/// remote `port-id`, data-plane `port`, and preference-ordered `formats` — carried by its
/// [`PortDescriptor`]s. Serialized by the serving host, deserialized by the browsing host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    /// The stable capability id (matches the advert `id` / the `descr` path).
    pub id: String,
    /// The advertising host's id.
    pub host: String,
    /// Capability kind: `midi | audio | screen | control-api | ...` (matches the advert `cap`).
    pub kind: String,
    /// Direction: `source | sink | duplex | control` (matches the advert `dir`).
    pub dir: String,
    /// The typed ports this capability exposes (§3) — the same shape `capmesh-ctl` `list-ports`
    /// returns, reused verbatim so the model has a single definition.
    #[serde(default)]
    pub ports: Vec<PortDescriptor>,
}

/// The response body of the mesh endpoint's `GET /caps` (docs/MESH-PROTOCOL.md §3): every
/// capability the serving host currently exposes, for a one-round-trip browse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    #[serde(default)]
    pub caps: Vec<CapabilityDescriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

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
    fn format_round_trips_with_params() {
        let f = Format {
            codec: "ump".into(),
            params: serde_json::Map::from_iter([("group".to_string(), serde_json::json!(3))]),
        };
        let back: Format = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(f, back);
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

    #[test]
    fn parses_the_mesh_capability_descriptor_example() {
        // The MESH-PROTOCOL.md §3 `GET /caps/<id>` example.
        let json = r#"{
            "id": "b1f0-uuid", "host": "green-machine", "kind": "midi", "dir": "source",
            "ports": [
              { "port-id": "kbd-0", "kind": "stream", "dir": "source", "type": "midi",
                "name": "Keystation 49e", "virtualizable": true,
                "formats": [ {"codec":"ump","group":0}, {"codec":"midi1"} ] }
            ]
        }"#;
        let cap: CapabilityDescriptor = serde_json::from_str(json).unwrap();
        assert_eq!(cap.id, "b1f0-uuid");
        assert_eq!(cap.host, "green-machine");
        assert_eq!(cap.kind, "midi");
        assert_eq!(cap.dir, "source");
        assert_eq!(cap.ports.len(), 1);
        assert_eq!(cap.ports[0].port_id, "kbd-0");
        assert_eq!(cap.ports[0].formats.len(), 2);

        // Round-trips through the on-wire shape (nested PortDescriptor keys preserved).
        let v = serde_json::to_value(&cap).unwrap();
        assert_eq!(v["id"], "b1f0-uuid");
        assert_eq!(v["ports"][0]["port-id"], "kbd-0");
        assert_eq!(v["ports"][0]["type"], "midi");
        let back: CapabilityDescriptor = serde_json::from_value(v).unwrap();
        assert_eq!(back, cap);
    }

    #[test]
    fn caps_list_response_round_trips() {
        // The `GET /caps` envelope; empty list decodes from a missing/empty `caps`.
        let empty: CapabilitiesResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.caps.is_empty());

        let resp = CapabilitiesResponse {
            caps: vec![CapabilityDescriptor {
                id: "a".into(),
                host: "h".into(),
                kind: "midi".into(),
                dir: "sink".into(),
                ports: vec![],
            }],
        };
        let back: CapabilitiesResponse =
            serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert_eq!(back, resp);
    }
}
