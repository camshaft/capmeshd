//! The desired-mount reconciler (DESIGN §6).
//!
//! capmeshd holds no persistent authoritative state; it holds a **desired-mount set**
//! (permanent mounts from config, temp mounts from agent/MCP commands) and continuously
//! reconciles actual vs desired by driving a data-plane daemon's `capmesh-ctl` socket
//! ([`capmesh_ctl`]). `connect`/`disconnect` are mutations of desired-state, not imperative
//! calls — this is what makes mounts idempotent and self-healing after a peer reboot.
//!
//! B2a is the poll-based core: [`reconcile`] is the pure decision (what to `mount` /
//! `unmount`), and [`Reconciler::reconcile_once`] applies it against a live daemon. When
//! `nmidid` ships `mount-state` notifications (§5), the reconciler will react to them
//! event-driven instead of polling `mount-status`.

use capmesh_ctl::{CtlClient, CtlError, MountSpec, MountState, MountStatus, Notification};
use std::collections::{HashMap, HashSet};
use tracing::info;

/// What a reconcile pass decided to do to converge actual → desired.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcilePlan {
    /// Desired mounts that are absent or dead in actual state — (re)issue `mount`.
    pub to_mount: Vec<MountSpec>,
    /// Actual mount-ids no longer desired — issue `unmount`.
    pub to_unmount: Vec<String>,
}

impl ReconcilePlan {
    /// True when actual already matches desired — nothing to do.
    pub fn is_empty(&self) -> bool {
        self.to_mount.is_empty() && self.to_unmount.is_empty()
    }
}

/// Compute the plan to converge `actual` toward `desired` (DESIGN §6). Pure: no I/O.
///
/// - A desired mount **absent** from actual, or present but `failed`/`torn-down`, is
///   (re)mounted — this is the self-heal after a peer reboot or a dropped link.
/// - A desired mount that is `pending`/`connecting`/`active`/`degraded` is left alone: it
///   is in-flight or live (degraded is impaired-but-live — surfaced, not churned).
/// - An actual mount **not** desired is torn down (unless already `torn-down`).
pub fn reconcile(desired: &[MountSpec], actual: &[MountStatus]) -> ReconcilePlan {
    let actual_by_id: HashMap<&str, MountState> = actual
        .iter()
        .map(|m| (m.mount_id.as_str(), m.state))
        .collect();
    let desired_ids: HashSet<&str> = desired.iter().map(|d| d.mount_id.as_str()).collect();

    let to_mount = desired
        .iter()
        .filter(|d| match actual_by_id.get(d.mount_id.as_str()) {
            None => true,
            Some(MountState::Failed | MountState::TornDown) => true,
            Some(_) => false,
        })
        .cloned()
        .collect();

    let to_unmount = actual
        .iter()
        .filter(|m| !desired_ids.contains(m.mount_id.as_str()))
        .filter(|m| m.state != MountState::TornDown)
        .map(|m| m.mount_id.clone())
        .collect();

    ReconcilePlan {
        to_mount,
        to_unmount,
    }
}

/// Whether a daemon notification (§5) means actual-state may have drifted from desired and
/// a reconcile pass should run. A mount dying (`failed`/`torn-down`) or a source port
/// disappearing warrants re-reconcile (self-heal / teardown); `connecting`/`active`/
/// `degraded` and a newly-arrived port are informational here — discovery-driven auto-mount
/// on `port-added` is a separate, selector-gated concern (M1, DESIGN §6.1).
pub fn wants_reconcile(n: &Notification) -> bool {
    match n {
        Notification::MountState { state, .. } => {
            matches!(state, MountState::Failed | MountState::TornDown)
        }
        Notification::PortRemoved { .. } => true,
        Notification::PortAdded(_) | Notification::Other { .. } => false,
    }
}

/// Owns the desired-mount set and applies [`reconcile`] against a daemon.
#[derive(Debug, Default)]
pub struct Reconciler {
    desired: Vec<MountSpec>,
}

impl Reconciler {
    /// A reconciler with the given desired-mount set.
    pub fn with_desired(desired: Vec<MountSpec>) -> Self {
        Self { desired }
    }

    /// One reconcile pass: fetch actual state via `mount-status`, compute the plan, and
    /// apply it (issue `mount`/`unmount`). Returns the plan that was applied.
    pub async fn reconcile_once(&self, client: &mut CtlClient) -> Result<ReconcilePlan, CtlError> {
        let actual = client.mount_status(None).await?.mounts;
        let plan = reconcile(&self.desired, &actual);
        for spec in &plan.to_mount {
            let res = client.mount(spec).await?;
            info!(mount_id = %res.mount_id, state = ?res.state, "reconcile: mounted");
        }
        for id in &plan.to_unmount {
            client.unmount(id).await?;
            info!(mount_id = %id, "reconcile: unmounted");
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capmesh_ctl::{Format, LocalEndpoint, MountRole, RemoteEndpoint};

    fn spec(mount_id: &str) -> MountSpec {
        MountSpec {
            mount_id: mount_id.to_string(),
            role: MountRole::MirrorSource,
            local: LocalEndpoint {
                is_virtual: true,
                name: None,
            },
            remote: RemoteEndpoint {
                host: "peer".into(),
                addr: "192.168.1.23".parse().unwrap(),
                port: 5004,
                port_id: "kbd-0".into(),
            },
            format: Format {
                codec: "midi1".into(),
                params: Default::default(),
            },
        }
    }

    fn status(mount_id: &str, state: MountState) -> MountStatus {
        MountStatus {
            mount_id: mount_id.to_string(),
            state,
            since: None,
            stats: None,
            detail: None,
        }
    }

    #[test]
    fn empty_desired_and_actual_is_a_noop() {
        assert!(reconcile(&[], &[]).is_empty());
    }

    #[test]
    fn desired_absent_from_actual_is_mounted() {
        let plan = reconcile(&[spec("m1")], &[]);
        assert_eq!(plan.to_mount.len(), 1);
        assert_eq!(plan.to_mount[0].mount_id, "m1");
        assert!(plan.to_unmount.is_empty());
    }

    #[test]
    fn actual_not_desired_is_unmounted() {
        let plan = reconcile(&[], &[status("stale", MountState::Active)]);
        assert_eq!(plan.to_unmount, vec!["stale".to_string()]);
        assert!(plan.to_mount.is_empty());
    }

    #[test]
    fn desired_and_active_is_left_alone() {
        let plan = reconcile(&[spec("m1")], &[status("m1", MountState::Active)]);
        assert!(plan.is_empty());
    }

    #[test]
    fn connecting_and_degraded_are_left_alone() {
        let plan = reconcile(
            &[spec("m1"), spec("m2")],
            &[
                status("m1", MountState::Connecting),
                status("m2", MountState::Degraded),
            ],
        );
        assert!(
            plan.is_empty(),
            "in-flight/impaired-but-live mounts must not churn"
        );
    }

    #[test]
    fn failed_desired_is_remounted() {
        let plan = reconcile(&[spec("m1")], &[status("m1", MountState::Failed)]);
        assert_eq!(plan.to_mount.len(), 1);
        assert!(plan.to_unmount.is_empty());
    }

    #[test]
    fn torn_down_desired_is_remounted_not_unmounted() {
        let plan = reconcile(&[spec("m1")], &[status("m1", MountState::TornDown)]);
        assert_eq!(
            plan.to_mount.len(),
            1,
            "a torn-down desired mount is re-mounted"
        );
        assert!(
            plan.to_unmount.is_empty(),
            "already torn-down: nothing to unmount"
        );
    }

    #[test]
    fn wants_reconcile_only_on_drift() {
        let ms = |state| Notification::MountState {
            mount_id: "m1".into(),
            state,
            detail: None,
            stats: None,
        };
        assert!(wants_reconcile(&ms(MountState::Failed)));
        assert!(wants_reconcile(&ms(MountState::TornDown)));
        assert!(!wants_reconcile(&ms(MountState::Active)));
        assert!(!wants_reconcile(&ms(MountState::Connecting)));
        assert!(!wants_reconcile(&ms(MountState::Degraded)));
        assert!(wants_reconcile(&Notification::PortRemoved {
            port_id: "kbd-0".into()
        }));
        assert!(!wants_reconcile(&Notification::Other {
            method: "port-added".into()
        }));
    }

    #[tokio::test]
    async fn reconcile_once_mounts_a_missing_desired_over_a_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let path = std::env::temp_dir().join(format!(
            "capmesh-reconcile-{}-{}.sock",
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
            let mut lines = BufReader::new(r).lines();
            // hello
            let _ = lines.next_line().await.unwrap().unwrap();
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol\":\"1\",\"daemon\":\"nmidid/0.1\",\"capabilities\":[]}}\n").await.unwrap();
            // mount-status → empty
            let req = lines.next_line().await.unwrap().unwrap();
            assert!(req.contains("mount-status"));
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"mounts\":[]}}\n")
                .await
                .unwrap();
            // mount → connecting
            let req = lines.next_line().await.unwrap().unwrap();
            assert!(req.contains("\"method\":\"mount\""));
            assert!(req.contains("\"mount-id\":\"m1\""));
            w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"mount-id\":\"m1\",\"state\":\"connecting\"}}\n").await.unwrap();
        });

        let mut client = CtlClient::connect(&path).await.unwrap();
        client.hello().await.unwrap();
        let r = Reconciler::with_desired(vec![spec("m1")]);
        let plan = r.reconcile_once(&mut client).await.unwrap();
        assert_eq!(plan.to_mount.len(), 1);
        assert_eq!(plan.to_mount[0].mount_id, "m1");

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
