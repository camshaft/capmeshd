//! Auto-mount planning (DESIGN §6.1) — the pure decision that turns a discovered capability
//! into a mount intent.
//!
//! Given an auto-mount rule's `action` + selector and a discovered capability's ports (from
//! the fetched descriptor, MESH-PROTOCOL.md §3), this picks the port to mount, maps the action
//! to a [`MountRole`], and negotiates the wire format (§4) against the local side's codecs.
//! It is deliberately transport-free: the result is everything a `MountSpec` needs *except* the
//! remote peer's address and data-plane port, which the discovery layer fills in. That split
//! keeps the decision unit-testable and isolates the one piece still being pinned down (how a
//! peer's data-plane port is conveyed).

use crate::negotiate::{self, NoCommonFormat};
use capmesh_ctl::{Format, LocalEndpoint, MountRole, MountSpec, PortDescriptor, RemoteEndpoint};
use std::net::IpAddr;

/// The outcome of planning an auto-mount: which remote port, the role the local daemon
/// materializes, and the negotiated format.
#[derive(Debug, Clone, PartialEq)]
pub struct MountPlan {
    pub port_id: String,
    pub role: MountRole,
    pub format: Format,
}

/// Why an auto-mount could not be planned.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PlanError {
    #[error("no advertised port matches the auto-mount selector")]
    NoMatchingPort,
    #[error("unknown auto-mount action `{0}`")]
    UnknownAction(String),
    #[error(transparent)]
    NoCommonFormat(#[from] NoCommonFormat),
}

/// Map an auto-mount rule's `action` to the mount role the local daemon materializes (§3.1).
/// `mirror-local` is the ergonomic alias for `mirror-source` (the keyboard-shows-up case).
pub fn role_for_action(action: &str) -> Option<MountRole> {
    match action {
        "mirror-local" | "mirror-source" => Some(MountRole::MirrorSource),
        "mirror-sink" => Some(MountRole::MirrorSink),
        "link" => Some(MountRole::Link),
        _ => None,
    }
}

/// Plan an auto-mount (§6.1): choose the first advertised port matching the selector
/// (`kind`↔port type, `dir`, `port` id — each `None` matches anything), map `action` to a
/// role, and negotiate the format from the local codecs (consumer) against that port's
/// advertised formats (producer, §4).
pub fn plan_mount(
    action: &str,
    selector_kind: Option<&str>,
    selector_dir: Option<&str>,
    selector_port: Option<&str>,
    ports: &[PortDescriptor],
    local_codecs: &[Format],
) -> Result<MountPlan, PlanError> {
    let role = role_for_action(action).ok_or_else(|| PlanError::UnknownAction(action.to_string()))?;
    let port = ports
        .iter()
        .find(|p| {
            selector_kind.is_none_or(|k| p.type_ == k)
                && selector_dir.is_none_or(|d| p.dir.as_deref() == Some(d))
                && selector_port.is_none_or(|pid| p.port_id == pid)
        })
        .ok_or(PlanError::NoMatchingPort)?;
    let format = negotiate::negotiate(local_codecs, &port.formats)?;
    Ok(MountPlan {
        port_id: port.port_id.clone(),
        role,
        format,
    })
}

/// Assemble the full [`MountSpec`] (§3.1) from a [`MountPlan`] and the resolved remote
/// coordinates. The remote data-plane `port` is resolved by the caller — kept out of the
/// planner so this stays kind-agnostic (for MIDI/AppleMIDI the caller reads it from the peer's
/// `_apple-midi._udp` record; DESIGN §5). Mirror roles create a local virtual endpoint named
/// `local_name`; `link` binds an existing real local port, so no virtual endpoint.
pub fn build_mount_spec(
    plan: MountPlan,
    mount_id: String,
    remote_host: String,
    remote_addr: IpAddr,
    data_port: u16,
    local_name: Option<String>,
) -> MountSpec {
    let is_virtual = matches!(plan.role, MountRole::MirrorSource | MountRole::MirrorSink);
    MountSpec {
        mount_id,
        role: plan.role,
        local: LocalEndpoint {
            is_virtual,
            name: local_name,
        },
        remote: RemoteEndpoint {
            host: remote_host,
            addr: remote_addr,
            port: data_port,
            port_id: plan.port_id,
        },
        format: plan.format,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(codec: &str) -> Format {
        Format {
            codec: codec.into(),
            params: serde_json::Map::new(),
        }
    }

    fn port(id: &str, dir: &str, ty: &str, codecs: &[&str]) -> PortDescriptor {
        PortDescriptor {
            port_id: id.into(),
            kind: "stream".into(),
            dir: Some(dir.into()),
            type_: ty.into(),
            name: id.into(),
            virtualizable: true,
            formats: codecs.iter().map(|c| fmt(c)).collect(),
        }
    }

    #[test]
    fn role_mapping() {
        assert_eq!(role_for_action("mirror-local"), Some(MountRole::MirrorSource));
        assert_eq!(role_for_action("mirror-source"), Some(MountRole::MirrorSource));
        assert_eq!(role_for_action("mirror-sink"), Some(MountRole::MirrorSink));
        assert_eq!(role_for_action("link"), Some(MountRole::Link));
        assert_eq!(role_for_action("nonsense"), None);
    }

    #[test]
    fn plans_the_matching_source_port_and_negotiates_format() {
        let ports = vec![
            port("spk-0", "sink", "midi", &["midi1"]),
            port("kbd-0", "source", "midi", &["ump", "midi1"]),
        ];
        let plan = plan_mount(
            "mirror-local",
            Some("midi"),
            Some("source"),
            None,
            &ports,
            &[fmt("midi1")],
        )
        .unwrap();
        assert_eq!(plan.port_id, "kbd-0");
        assert_eq!(plan.role, MountRole::MirrorSource);
        assert_eq!(plan.format.codec, "midi1"); // the one codec common to both sides
    }

    #[test]
    fn port_selector_pins_a_specific_port_id() {
        let ports = vec![
            port("kbd-0", "source", "midi", &["midi1"]),
            port("kbd-1", "source", "midi", &["midi1"]),
        ];
        let plan = plan_mount(
            "mirror-source",
            None,
            None,
            Some("kbd-1"),
            &ports,
            &[fmt("midi1")],
        )
        .unwrap();
        assert_eq!(plan.port_id, "kbd-1");
    }

    #[test]
    fn no_matching_port_is_an_error() {
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        let err = plan_mount("mirror-local", Some("audio"), None, None, &ports, &[fmt("midi1")])
            .unwrap_err();
        assert_eq!(err, PlanError::NoMatchingPort);
    }

    #[test]
    fn unknown_action_is_an_error() {
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        let err = plan_mount("teleport", None, None, None, &ports, &[fmt("midi1")]).unwrap_err();
        assert_eq!(err, PlanError::UnknownAction("teleport".into()));
    }

    #[test]
    fn no_common_format_is_an_error() {
        let ports = vec![port("kbd-0", "source", "midi", &["ump"])];
        let err = plan_mount("mirror-local", None, None, None, &ports, &[fmt("midi1")])
            .unwrap_err();
        assert!(matches!(err, PlanError::NoCommonFormat(_)));
    }

    #[test]
    fn build_mount_spec_mirror_source_is_virtual() {
        let plan = MountPlan {
            port_id: "kbd-0".into(),
            role: MountRole::MirrorSource,
            format: fmt("midi1"),
        };
        let spec = build_mount_spec(
            plan,
            "laptop-kbd-0".into(),
            "laptop".into(),
            "192.168.1.23".parse().unwrap(),
            5004, // data-plane port the caller resolved (e.g. AppleMIDI control 5003 + 1)
            Some("laptop: Keystation 49e".into()),
        );
        assert_eq!(spec.mount_id, "laptop-kbd-0");
        assert_eq!(spec.role, MountRole::MirrorSource);
        assert!(spec.local.is_virtual);
        assert_eq!(spec.local.name.as_deref(), Some("laptop: Keystation 49e"));
        assert_eq!(spec.remote.host, "laptop");
        assert_eq!(
            spec.remote.addr,
            "192.168.1.23".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(spec.remote.port, 5004);
        assert_eq!(spec.remote.port_id, "kbd-0");
        assert_eq!(spec.format.codec, "midi1");
    }

    #[test]
    fn build_mount_spec_link_is_not_virtual() {
        let plan = MountPlan {
            port_id: "p".into(),
            role: MountRole::Link,
            format: fmt("midi1"),
        };
        let spec = build_mount_spec(
            plan,
            "m".into(),
            "h".into(),
            "10.0.0.1".parse().unwrap(),
            6000,
            None,
        );
        assert!(!spec.local.is_virtual);
        assert!(spec.local.name.is_none());
    }
}
