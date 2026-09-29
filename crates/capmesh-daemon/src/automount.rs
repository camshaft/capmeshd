//! Auto-mount planning (DESIGN §6.1) — the pure decision that turns a discovered capability
//! into a mount intent.
//!
//! Given an auto-mount rule's `action` + selector and a discovered capability's ports (from
//! the fetched descriptor, MESH-PROTOCOL.md §3), this picks the port to mount, maps the action
//! to a [`MountRole`], and negotiates the wire format (§4) against the local side's codecs.
//! It is deliberately transport-free: the result is everything a `MountSpec` needs *except* the
//! remote peer's address and control port, which the discovery layer fills in. That split
//! keeps the decision unit-testable and isolates the one piece still being pinned down (how a
//! peer's control port is discovered).

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
#[derive(Debug, Clone, thiserror::Error, PartialEq)]
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

/// The remote port direction a role binds (§3.1), when it is not given explicitly: a
/// `mirror-source` mirrors the remote's **source** locally, a `mirror-sink` forwards to the
/// remote's **sink**, and `link` binds an existing local port so it leaves the direction
/// unconstrained (`None`). Used to default the port selector for a discovery-driven connect so a
/// device that exposes *both* a source and a sink port binds the one the role means, not whichever
/// the descriptor lists first. An unknown action also yields `None` (rejected later by planning).
pub fn dir_for_role(action: &str) -> Option<&'static str> {
    match action {
        "mirror-local" | "mirror-source" => Some("source"),
        "mirror-sink" => Some("sink"),
        _ => None,
    }
}

/// Plan an auto-mount (§6.1): choose the first advertised port matching the selector
/// (`kind`↔port type, `dir`, `port` id — each `None` matches anything), map `action` to a
/// role, and negotiate the format from the local codecs (consumer) against that port's
/// advertised formats (producer, §4).
///
/// The returned `port_id` is always one that **exists in `ports`** (the peer's fetched
/// descriptor): the planner only ever names a port it selected here. This is where a remote
/// port-id is validated — a mirror-source data-plane daemon treats `remote.port-id` as an
/// opaque peer label and does not check it (nmidid #98), so a selector that pins a `port`
/// absent from the descriptor is caught here as [`PlanError::NoMatchingPort`], never issued.
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
/// coordinates. `control_port` is the remote's AppleMIDI **control** port — the SRV port of its
/// `_apple-midi._udp` record — and goes into `remote.port` **verbatim**: the data-plane daemon
/// derives the data port as `remote.port + 1` itself, so capmeshd MUST NOT add 1. The port is
/// resolved by the caller, keeping this kind-agnostic. Mirror roles create a local virtual
/// endpoint named `local_name`; `link` binds an existing real local port named by `local_port_id`
/// (the id from the data-plane daemon's `list-ports`; no virtual endpoint). `local_port_id` is
/// carried only for `link` — a mirror role ignores it (it creates a virtual endpoint, §3.1).
pub fn build_mount_spec(
    plan: MountPlan,
    mount_id: String,
    remote_host: String,
    remote_addr: IpAddr,
    control_port: u16,
    local_name: Option<String>,
    local_port_id: Option<String>,
) -> MountSpec {
    let is_virtual = matches!(plan.role, MountRole::MirrorSource | MountRole::MirrorSink);
    MountSpec {
        mount_id,
        role: plan.role,
        local: LocalEndpoint {
            is_virtual,
            name: local_name,
            // Only a `link` binds a named local real port; mirror roles never do.
            port_id: match plan.role {
                MountRole::Link => local_port_id,
                MountRole::MirrorSource | MountRole::MirrorSink => None,
            },
        },
        remote: RemoteEndpoint {
            host: remote_host,
            addr: remote_addr,
            // The control port verbatim; the daemon derives the data port as port + 1.
            port: control_port,
            port_id: plan.port_id,
        },
        format: plan.format,
    }
}

/// How long a derived auto-mount lives (DESIGN §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lifetime {
    /// Torn down when the source capability stops being advertised (the default). This is the
    /// mesh's ergonomic case: unplug the source and the local virtual device disappears.
    #[default]
    WhileAdvertised,
    /// Survives advert removal — kept until explicitly unmounted, like a permanent mount (§9).
    Permanent,
}

impl Lifetime {
    /// Classify a rule's optional `lifetime` string (§6.1). `None` and any unrecognized value
    /// fall back to the default [`Lifetime::WhileAdvertised`] (matching the daemon's prior
    /// stringly-typed handling); only `"permanent"` selects [`Lifetime::Permanent`].
    pub fn from_config(s: Option<&str>) -> Self {
        match s {
            Some("permanent") => Lifetime::Permanent,
            _ => Lifetime::WhileAdvertised,
        }
    }

    /// Whether a mount with this lifetime is torn down when its source's advert is removed (§6.1).
    pub fn tears_down_on_unadvertise(self) -> bool {
        matches!(self, Lifetime::WhileAdvertised)
    }
}

/// A live auto-mount the daemon has issued, tracked so it can honor the mount's [`Lifetime`]
/// when the source's advert is removed (§6.1). The daemon keys these by the source peer's mDNS
/// fullname — the identifier carried by both the resolve and the remove event.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAutoMount {
    /// The data-plane `mount-id` to unmount.
    pub mount_id: String,
    /// The capability kind, selecting which local data-plane socket to unmount against.
    pub kind: String,
    /// The mount's lifetime — decides whether advert removal tears it down.
    pub lifetime: Lifetime,
}

/// Partition a peer's active auto-mounts when its advert is removed (§6.1): the first returned
/// vec is the `while-advertised` mounts to tear down, the second the `permanent` ones to retain
/// (they outlive the advert). Pure, so the daemon's `ServiceRemoved` handler is one call plus
/// the unmount I/O.
pub fn teardown_on_unadvertise(
    mounts: Vec<ActiveAutoMount>,
) -> (Vec<ActiveAutoMount>, Vec<ActiveAutoMount>) {
    mounts
        .into_iter()
        .partition(|m| m.lifetime.tears_down_on_unadvertise())
}

/// Whether a just-issued auto-mount should be kept or immediately undone, decided against whether
/// the source advert is still live at the moment issuance completes (§6.1). A `ServiceRemoved`
/// can race an in-flight issuance: if the advert went away during the descriptor fetch/mount, a
/// `while-advertised` mount must be undone — it would otherwise be tracked against a peer fullname
/// that has already had its (empty) teardown run and will never see another removal, so it would
/// never be torn down. A `permanent` mount is kept regardless, since it outlives advert removal by
/// design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaceOutcome {
    /// Track and keep the mount.
    Track,
    /// The advert was removed mid-issuance; unmount what was just issued.
    CompensateUnmount,
}

/// Decide a just-issued auto-mount's fate against advert liveness + its [`Lifetime`] — see
/// [`RaceOutcome`]. Pure, so the daemon's issuance glue is one call under the registry lock.
pub fn resolve_issue_race(advert_still_live: bool, lifetime: Lifetime) -> RaceOutcome {
    if advert_still_live || !lifetime.tears_down_on_unadvertise() {
        RaceOutcome::Track
    } else {
        RaceOutcome::CompensateUnmount
    }
}

/// Record one just-issued auto-mount in a peer's tracking vec, keyed by `mount_id` (§6.1). A
/// `mount_id` is the data-plane idempotency key, so the same mount can be re-issued by concurrent
/// resolve tasks or by two rules matching one advert; this keeps the registry at one entry per
/// `mount_id` so `ServiceRemoved` teardown never fires a redundant unmount. Returns `true` if the
/// mount was newly tracked, `false` if an entry with that `mount_id` was already present.
pub fn track_active_mount(tracked: &mut Vec<ActiveAutoMount>, mount: ActiveAutoMount) -> bool {
    if tracked.iter().any(|m| m.mount_id == mount.mount_id) {
        return false;
    }
    tracked.push(mount);
    true
}

/// The resolved remote coordinates for an auto-mount, gathered by the discovery layer: the
/// idempotency `mount_id`, the peer's host-id and IP, its AppleMIDI **control** port (`None`
/// until the peer's `_apple-midi._udp` record has been seen), and the local virtual name.
#[derive(Debug, Clone)]
pub struct Remote {
    pub mount_id: String,
    pub host: String,
    pub addr: IpAddr,
    pub control_port: Option<u16>,
    pub local_name: Option<String>,
}

/// The outcome of evaluating one auto-mount rule against a discovered, descriptor-fetched peer.
#[derive(Debug, Clone, PartialEq)]
pub enum AutoMountOutcome {
    /// Issue this mount.
    Mount(MountSpec),
    /// A port matched and the format negotiated, but the peer's control port is not known yet
    /// (no `_apple-midi._udp` record seen). Skip for now; a later apple-midi resolve re-triggers.
    AwaitingControlPort,
    /// The rule did not apply (no matching port / unknown action / no common format).
    Skip(PlanError),
}

/// Evaluate one auto-mount rule (§6.1) against a peer whose descriptor has been fetched: plan
/// the mount, then require the peer's control port before committing. This composes
/// [`plan_mount`] and [`build_mount_spec`] and centralises the "matched but the data-plane port
/// isn't known yet" state, so the daemon's discovery glue is a single `match`.
pub fn evaluate(
    action: &str,
    selector_kind: Option<&str>,
    selector_dir: Option<&str>,
    selector_port: Option<&str>,
    ports: &[PortDescriptor],
    local_codecs: &[Format],
    remote: Remote,
) -> AutoMountOutcome {
    let plan = match plan_mount(
        action,
        selector_kind,
        selector_dir,
        selector_port,
        ports,
        local_codecs,
    ) {
        Ok(plan) => plan,
        Err(e) => return AutoMountOutcome::Skip(e),
    };
    let Some(control_port) = remote.control_port else {
        return AutoMountOutcome::AwaitingControlPort;
    };
    AutoMountOutcome::Mount(build_mount_spec(
        plan,
        remote.mount_id,
        remote.host,
        remote.addr,
        control_port,
        remote.local_name,
        // Discovery-driven auto-mount does not yet name a local real port for a `link` rule (the
        // rule selects a remote capability, not a local port); sourcing it is a follow-on. Manual
        // link mounts (connect / permanent-mount config) will pass it once those paths carry it.
        None,
    ))
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
    fn dir_for_role_maps_the_role_to_the_remote_port_direction() {
        assert_eq!(dir_for_role("mirror-source"), Some("source"));
        assert_eq!(dir_for_role("mirror-local"), Some("source")); // alias
        assert_eq!(dir_for_role("mirror-sink"), Some("sink"));
        assert_eq!(dir_for_role("link"), None); // link leaves direction unconstrained
        assert_eq!(dir_for_role("nonsense"), None);
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
    fn selector_port_absent_from_descriptor_is_no_match() {
        // capmeshd owns port-id validation (nmidid #98 treats remote.port-id as opaque): a
        // selector pinning a port-id that the fetched descriptor does not advertise must be
        // caught here, not issued and rejected by the daemon.
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        let err = plan_mount(
            "mirror-local",
            Some("midi"),
            None,
            Some("kbd-9"), // not in the descriptor
            &ports,
            &[fmt("midi1")],
        )
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
            5004, // the AppleMIDI CONTROL port (SRV port), verbatim; the daemon derives data = 5005
            Some("laptop: Keystation 49e".into()),
            // A local port-id passed for a mirror role is ignored — mirrors create a virtual endpoint.
            Some("ignored-for-mirror".into()),
        );
        assert_eq!(spec.mount_id, "laptop-kbd-0");
        assert_eq!(spec.role, MountRole::MirrorSource);
        assert!(spec.local.is_virtual);
        assert_eq!(spec.local.name.as_deref(), Some("laptop: Keystation 49e"));
        assert_eq!(spec.local.port_id, None); // mirror never binds a local real port
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
            // A `link` carries the local real-port selector through to `local.port-id`.
            Some("sink-supercollider".into()),
        );
        assert!(!spec.local.is_virtual);
        assert!(spec.local.name.is_none());
        assert_eq!(spec.local.port_id.as_deref(), Some("sink-supercollider"));
    }

    fn remote(control_port: Option<u16>) -> Remote {
        Remote {
            mount_id: "laptop-kbd-0".into(),
            host: "laptop".into(),
            addr: "192.168.1.23".parse().unwrap(),
            control_port,
            local_name: Some("laptop: Keystation 49e".into()),
        }
    }

    #[test]
    fn evaluate_issues_a_mount_when_the_control_port_is_known() {
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        let outcome = evaluate(
            "mirror-local",
            Some("midi"),
            Some("source"),
            None,
            &ports,
            &[fmt("midi1")],
            remote(Some(5004)),
        );
        match outcome {
            AutoMountOutcome::Mount(spec) => {
                assert_eq!(spec.mount_id, "laptop-kbd-0");
                assert_eq!(spec.remote.port, 5004); // control port verbatim
                assert_eq!(spec.remote.port_id, "kbd-0");
                assert_eq!(spec.role, MountRole::MirrorSource);
            }
            other => panic!("expected Mount, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_awaits_when_control_port_unknown() {
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        let outcome = evaluate(
            "mirror-local",
            Some("midi"),
            Some("source"),
            None,
            &ports,
            &[fmt("midi1")],
            remote(None), // no _apple-midi._udp record seen yet
        );
        assert_eq!(outcome, AutoMountOutcome::AwaitingControlPort);
    }

    #[test]
    fn lifetime_from_config_defaults_to_while_advertised() {
        assert_eq!(Lifetime::from_config(None), Lifetime::WhileAdvertised);
        assert_eq!(
            Lifetime::from_config(Some("while-advertised")),
            Lifetime::WhileAdvertised
        );
        assert_eq!(Lifetime::from_config(Some("permanent")), Lifetime::Permanent);
        // An unrecognized value falls back to the default rather than failing the mount.
        assert_eq!(Lifetime::from_config(Some("forever?")), Lifetime::WhileAdvertised);
    }

    #[test]
    fn only_while_advertised_tears_down_on_unadvertise() {
        assert!(Lifetime::WhileAdvertised.tears_down_on_unadvertise());
        assert!(!Lifetime::Permanent.tears_down_on_unadvertise());
    }

    #[test]
    fn teardown_partitions_while_advertised_from_permanent() {
        let m = |id: &str, lifetime| ActiveAutoMount {
            mount_id: id.into(),
            kind: "midi".into(),
            lifetime,
        };
        let mounts = vec![
            m("a", Lifetime::WhileAdvertised),
            m("b", Lifetime::Permanent),
            m("c", Lifetime::WhileAdvertised),
        ];
        let (to_unmount, to_retain) = teardown_on_unadvertise(mounts);
        assert_eq!(
            to_unmount.iter().map(|m| m.mount_id.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(
            to_retain.iter().map(|m| m.mount_id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[test]
    fn track_active_mount_dedups_on_mount_id() {
        let m = |id: &str| ActiveAutoMount {
            mount_id: id.into(),
            kind: "midi".into(),
            lifetime: Lifetime::WhileAdvertised,
        };
        let mut tracked = Vec::new();

        // First issuance of a mount-id is tracked.
        assert!(track_active_mount(&mut tracked, m("kbd-0")));
        // A distinct mount-id (a second capability on the same peer) is also tracked.
        assert!(track_active_mount(&mut tracked, m("kbd-1")));
        // Re-issuing the same mount-id — a concurrent resolve task or a second matching rule —
        // is a no-op, so `ServiceRemoved` teardown never fires a redundant unmount.
        assert!(!track_active_mount(&mut tracked, m("kbd-0")));

        assert_eq!(
            tracked.iter().map(|m| m.mount_id.as_str()).collect::<Vec<_>>(),
            ["kbd-0", "kbd-1"]
        );
    }

    #[test]
    fn resolve_issue_race_compensates_only_a_vanished_while_advertised_mount() {
        // Advert still live at issuance → always track, whatever the lifetime.
        assert_eq!(
            resolve_issue_race(true, Lifetime::WhileAdvertised),
            RaceOutcome::Track
        );
        assert_eq!(
            resolve_issue_race(true, Lifetime::Permanent),
            RaceOutcome::Track
        );
        // Advert removed during issuance: a while-advertised mount is undone (it would otherwise
        // leak, tracked against an already-torn-down fullname); a permanent mount persists.
        assert_eq!(
            resolve_issue_race(false, Lifetime::WhileAdvertised),
            RaceOutcome::CompensateUnmount
        );
        assert_eq!(
            resolve_issue_race(false, Lifetime::Permanent),
            RaceOutcome::Track
        );
    }

    #[test]
    fn evaluate_skips_when_the_rule_does_not_apply() {
        let ports = vec![port("kbd-0", "source", "midi", &["midi1"])];
        // Selector wants audio; the only port is midi → Skip(NoMatchingPort), even though a
        // control port is known (the rule simply doesn't apply here).
        let outcome = evaluate(
            "mirror-local",
            Some("audio"),
            None,
            None,
            &ports,
            &[fmt("midi1")],
            remote(Some(5004)),
        );
        assert_eq!(outcome, AutoMountOutcome::Skip(PlanError::NoMatchingPort));
    }
}
