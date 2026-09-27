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
use capmesh_ctl::{Format, MountRole, PortDescriptor};

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
}
