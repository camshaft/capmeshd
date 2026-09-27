//! Mount lifecycle: the `mount` / `unmount` / `mount-status` methods (§3).
//!
//! [`MountRegistry`] tracks live mounts daemon-wide (shared across every control
//! connection). Two seams keep the registry, validation, and state machine
//! unit-testable without a real ALSA/CoreMIDI backend or network:
//! - [`Mounter`] materializes the local OS endpoint and hands back a [`MidiSink`]
//!   to push events into (the production [`MidirMounter`] creates the virtual
//!   `midir` port);
//! - [`Connector`] drives the mount's data path — the production connector (see
//!   `crate::pump`) spawns the AppleMIDI handshake + RTP-MIDI pump that moves the
//!   mount `connecting → active`; tests inject a no-op connector.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::{Notify, broadcast};

use crate::protocol::{
    DaemonError, MountRole, MountSpec, MountState, MountStats, MountStatus, RemoteEndpoint,
};

/// Depth of the per-daemon notification broadcast buffer. A slow control
/// connection that lags beyond this drops intermediate notifications (it still
/// gets the latest and can reconcile via `mount-status`).
const NOTIFY_CAPACITY: usize = 64;

/// Build the §5 `mount-state` notification (unsolicited, no `id`) for a status.
pub(crate) fn mount_state_notification(status: &MountStatus) -> Value {
    let mut params = serde_json::json!({
        "mount-id": status.mount_id,
        "state": status.state,
        "stats": status.stats,
    });
    if let Some(detail) = &status.detail {
        params["detail"] = Value::String(detail.clone());
    }
    serde_json::json!({"jsonrpc": "2.0", "method": "mount-state", "params": params})
}

/// Apply a state transition to a mount and emit a `mount-state` notification —
/// but only if the state actually changed (or a new `detail` is supplied), so a
/// steady stream of data does not spam identical notifications.
pub(crate) fn transition(
    status: &Arc<Mutex<MountStatus>>,
    notifier: &broadcast::Sender<Value>,
    new_state: MountState,
    detail: Option<String>,
) {
    let notification = {
        let mut s = status.lock().unwrap();
        if s.state == new_state && detail.is_none() {
            return;
        }
        s.state = new_state;
        if detail.is_some() {
            s.detail = detail;
        }
        mount_state_notification(&s)
    };
    // Err just means no control connection is currently subscribed.
    let _ = notifier.send(notification);
}

/// A place to push decoded MIDI bytes — the local end of a mount. Owned by the
/// mount's data-path task; dropping it releases the underlying endpoint (e.g.
/// removes the virtual MIDI port).
pub trait MidiSink: Send {
    /// Send one MIDI message (raw status+data bytes) to the local endpoint.
    fn send(&self, message: &[u8]) -> anyhow::Result<()>;
}

/// Materializes the local OS endpoint of a mount.
pub trait Mounter: Send + Sync {
    /// Whether this daemon/platform can create virtual endpoints.
    fn supports_virtual(&self) -> bool;

    /// Create a local virtual **source** (a port other apps read from, into
    /// which we push the remote source's events) named `display_name`, returning
    /// the sink to push into.
    fn create_virtual_source(&self, display_name: &str) -> anyhow::Result<Box<dyn MidiSink>>;
}

/// Drives a mount's data path: connects to the remote and pumps events into the
/// sink, updating `status`. The task runs until the remote ends the session or
/// `cancel` is notified (unmount), at which point it tears the session down
/// gracefully (sends `End`) and drops the sink → the local endpoint.
pub trait Connector: Send + Sync {
    fn start(
        &self,
        remote: RemoteEndpoint,
        sink: Box<dyn MidiSink>,
        status: Arc<Mutex<MountStatus>>,
        notifier: broadcast::Sender<Value>,
        cancel: Arc<Notify>,
    );
}

struct MountEntry {
    /// Shared with the data-path task, which updates state/stats live.
    status: Arc<Mutex<MountStatus>>,
    /// Signals the data-path task to tear down gracefully on unmount.
    cancel: Arc<Notify>,
}

/// Tracks live mounts and drives their creation/teardown.
pub struct MountRegistry {
    mounter: Arc<dyn Mounter>,
    connector: Arc<dyn Connector>,
    mounts: Mutex<HashMap<String, MountEntry>>,
    /// Daemon-wide `mount-state` notification fan-out; each control connection
    /// subscribes a receiver (§5).
    notifier: broadcast::Sender<Value>,
}

impl MountRegistry {
    pub fn new(mounter: Arc<dyn Mounter>, connector: Arc<dyn Connector>) -> Self {
        let (notifier, _) = broadcast::channel(NOTIFY_CAPACITY);
        MountRegistry {
            mounter,
            connector,
            mounts: Mutex::new(HashMap::new()),
            notifier,
        }
    }

    /// Subscribe to the daemon's notification stream (`mount-state`, `port-*`).
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifier.subscribe()
    }

    /// A sender handle onto the daemon's notification bus, for other producers
    /// (e.g. the hot-plug monitor) to emit §5 notifications on the same stream.
    pub fn notifier(&self) -> broadcast::Sender<Value> {
        self.notifier.clone()
    }

    /// Establish a mount (§3.1). Idempotent on `mount-id`: a repeat returns the
    /// existing mount's state rather than creating a second endpoint.
    pub fn mount(&self, spec: MountSpec) -> Result<serde_json::Value, DaemonError> {
        {
            let mounts = self.mounts.lock().unwrap();
            if let Some(entry) = mounts.get(&spec.mount_id) {
                let status = entry.status.lock().unwrap();
                return Ok(serde_json::json!({
                    "mount-id": status.mount_id,
                    "state": status.state,
                }));
            }
        }

        // Only mirror-source is implemented in M0a; other roles are declined
        // honestly rather than silently faked.
        if spec.role != MountRole::MirrorSource {
            return Err(DaemonError::domain(
                "role-unsupported",
                format!(
                    "role {:?} is not implemented yet (mirror-source only)",
                    spec.role
                ),
            ));
        }

        if spec.local.r#virtual && !self.mounter.supports_virtual() {
            return Err(DaemonError::domain(
                "virtual-unsupported",
                "this daemon cannot create virtual endpoints on this platform",
            ));
        }

        if spec.format.codec != "midi1" {
            return Err(DaemonError::domain(
                "format-unsupported",
                format!(
                    "codec '{}' is not supported (midi1 only)",
                    spec.format.codec
                ),
            ));
        }

        // `remote.port_id` identifies a port on the *remote* peer, which this
        // daemon cannot see — capmeshd validates it against the fetched remote
        // descriptor before issuing. Here it is an opaque label (the pump keys
        // the RTP session off `remote.addr`/`remote.port`), so we do not check it
        // against this host's local ports.
        let display_name = spec
            .local
            .name
            .clone()
            .unwrap_or_else(|| format!("nmidid: {}", spec.remote.port_id));

        let sink = self
            .mounter
            .create_virtual_source(&display_name)
            .map_err(|e| {
                DaemonError::domain("busy", format!("could not create virtual port: {e}"))
            })?;

        let status = Arc::new(Mutex::new(MountStatus {
            mount_id: spec.mount_id.clone(),
            state: MountState::Connecting,
            since: now_rfc3339(),
            stats: MountStats::default(),
            detail: None,
        }));
        let result = serde_json::json!({ "mount-id": spec.mount_id, "state": "connecting" });

        // Announce the initial `connecting` state (§5).
        let _ = self
            .notifier
            .send(mount_state_notification(&status.lock().unwrap()));

        // Hand the data path off to the connector (spawns the pump task), which
        // emits the subsequent active/failed/torn-down transitions. `cancel`
        // asks the task to tear the session down gracefully on unmount.
        let cancel = Arc::new(Notify::new());
        self.connector.start(
            spec.remote.clone(),
            sink,
            Arc::clone(&status),
            self.notifier.clone(),
            Arc::clone(&cancel),
        );

        self.mounts
            .lock()
            .unwrap()
            .insert(spec.mount_id.clone(), MountEntry { status, cancel });
        Ok(result)
    }

    /// Tear a mount down (§3). Idempotent: an unknown id is a clean no-op.
    pub fn unmount(&self, mount_id: &str) -> serde_json::Value {
        if let Some(entry) = self.mounts.lock().unwrap().remove(mount_id) {
            // Ask the data-path task to stop: it sends `End` (BY) to the remote
            // so the source does not hold a stale session, then exits — dropping
            // the sink, which removes the local virtual port.
            entry.cancel.notify_one();
            // Announce the teardown (§5) now; the task's own TornDown is a no-op
            // once the state has already changed.
            transition(&entry.status, &self.notifier, MountState::TornDown, None);
        }
        serde_json::json!({})
    }

    /// Tear down every live mount for daemon shutdown: each pump is signalled to
    /// stop gracefully (sends `End`/BY so no source holds a stale session) and a
    /// torn-down state is announced. Returns the number of mounts torn down.
    pub fn shutdown_all(&self) -> usize {
        let entries: Vec<MountEntry> = self
            .mounts
            .lock()
            .unwrap()
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in &entries {
            entry.cancel.notify_one();
            transition(&entry.status, &self.notifier, MountState::TornDown, None);
        }
        entries.len()
    }

    /// One or all live mounts (§3).
    pub fn status(&self, mount_id: Option<&str>) -> serde_json::Value {
        let mounts = self.mounts.lock().unwrap();
        let snapshot = |e: &MountEntry| e.status.lock().unwrap().clone();
        let list: Vec<MountStatus> = match mount_id {
            Some(id) => mounts.get(id).map(snapshot).into_iter().collect(),
            None => mounts.values().map(snapshot).collect(),
        };
        serde_json::json!({ "mounts": list })
    }
}

/// Production mounter: creates the virtual port via `midir`.
pub struct MidirMounter;

#[cfg(unix)]
struct MidirSink(Mutex<midir::MidiOutputConnection>);

#[cfg(unix)]
impl MidiSink for MidirSink {
    fn send(&self, message: &[u8]) -> anyhow::Result<()> {
        self.0
            .lock()
            .unwrap()
            .send(message)
            .map_err(|e| anyhow::anyhow!("midir send failed: {e}"))
    }
}

impl Mounter for MidirMounter {
    fn supports_virtual(&self) -> bool {
        // `midir` supports virtual ports on ALSA (Linux) and CoreMIDI (macOS).
        cfg!(unix)
    }

    fn create_virtual_source(&self, display_name: &str) -> anyhow::Result<Box<dyn MidiSink>> {
        #[cfg(unix)]
        {
            use midir::MidiOutput;
            use midir::os::unix::VirtualOutput;
            // A virtual OUTPUT we own appears to other local apps as a readable
            // MIDI source; we push the remote source's events into it.
            let out = MidiOutput::new("nmidid")?;
            let conn = out
                .create_virtual(display_name)
                .map_err(|e| anyhow::anyhow!("create_virtual failed: {e}"))?;
            Ok(Box::new(MidirSink(Mutex::new(conn))))
        }
        #[cfg(not(unix))]
        {
            let _ = display_name;
            anyhow::bail!("virtual MIDI ports are not supported on this platform")
        }
    }
}

/// A dependency-free RFC 3339 (UTC) timestamp for `MountStatus.since`.
pub(crate) fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Convert days-since-Unix-epoch to a civil (year, month, day). Howard Hinnant's
/// algorithm (public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A no-op mounter for tests in this and other modules (never touches ALSA).
#[cfg(test)]
pub struct NullMounter;

#[cfg(test)]
struct NullSink;

#[cfg(test)]
impl MidiSink for NullSink {
    fn send(&self, _message: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Mounter for NullMounter {
    fn supports_virtual(&self) -> bool {
        true
    }
    fn create_virtual_source(&self, _display_name: &str) -> anyhow::Result<Box<dyn MidiSink>> {
        Ok(Box::new(NullSink))
    }
}

/// A no-op connector for tests: never spawns a task, leaves the mount `connecting`.
#[cfg(test)]
pub struct NullConnector;

#[cfg(test)]
impl Connector for NullConnector {
    fn start(
        &self,
        _remote: RemoteEndpoint,
        _sink: Box<dyn MidiSink>,
        _status: Arc<Mutex<MountStatus>>,
        _notifier: broadcast::Sender<Value>,
        _cancel: Arc<Notify>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Format, LocalEndpoint};

    struct FakeMounter {
        supports_virtual: bool,
    }
    impl Mounter for FakeMounter {
        fn supports_virtual(&self) -> bool {
            self.supports_virtual
        }
        fn create_virtual_source(&self, _display_name: &str) -> anyhow::Result<Box<dyn MidiSink>> {
            Ok(Box::new(NullSink))
        }
    }

    fn registry(supports_virtual: bool) -> MountRegistry {
        MountRegistry::new(
            Arc::new(FakeMounter { supports_virtual }),
            Arc::new(NullConnector),
        )
    }

    fn spec(mount_id: &str, port_id: &str, codec: &str) -> MountSpec {
        MountSpec {
            mount_id: mount_id.to_string(),
            role: MountRole::MirrorSource,
            local: LocalEndpoint {
                r#virtual: true,
                name: Some("laptop: Keystation".to_string()),
            },
            remote: RemoteEndpoint {
                host: Some("laptop".to_string()),
                addr: "192.168.1.23".to_string(),
                port: 5004,
                port_id: port_id.to_string(),
            },
            format: Format {
                codec: codec.to_string(),
                params: serde_json::Map::new(),
            },
        }
    }

    #[test]
    fn mount_mirror_source_is_connecting_and_listed() {
        let reg = registry(true);
        let r = reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        assert_eq!(r["state"], "connecting");
        assert_eq!(r["mount-id"], "m1");
        let mounts = reg.status(None)["mounts"].as_array().unwrap().clone();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["mount-id"], "m1");
        assert_eq!(mounts[0]["state"], "connecting");
    }

    #[test]
    fn mount_is_idempotent_on_mount_id() {
        let reg = registry(true);
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        assert_eq!(reg.status(None)["mounts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn unmount_removes_and_is_idempotent() {
        let reg = registry(true);
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        reg.unmount("m1");
        assert!(reg.status(None)["mounts"].as_array().unwrap().is_empty());
        // Unknown id is a clean no-op.
        reg.unmount("nope");
    }

    #[test]
    fn shutdown_all_tears_down_every_mount() {
        let reg = registry(true);
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        reg.mount(spec("m2", "source-0", "midi1")).unwrap();
        assert_eq!(reg.status(None)["mounts"].as_array().unwrap().len(), 2);

        assert_eq!(reg.shutdown_all(), 2);
        assert!(reg.status(None)["mounts"].as_array().unwrap().is_empty());
        // Idempotent: nothing left to tear down.
        assert_eq!(reg.shutdown_all(), 0);
    }

    #[test]
    fn remote_port_id_is_opaque_not_validated_locally() {
        // `remote.port_id` names a port on the peer, which this daemon cannot
        // see, so a remote id unknown to this host is accepted (the mount
        // proceeds to `connecting`); capmeshd validates it against the fetched
        // remote descriptor before issuing.
        let reg = registry(true);
        let r = reg.mount(spec("m1", "a-remote-port-this-host-never-heard-of", "midi1"));
        assert_eq!(r.unwrap()["state"], "connecting");
    }

    #[test]
    fn non_midi1_codec_is_format_unsupported() {
        let reg = registry(true);
        let err = reg.mount(spec("m1", "source-0", "ump")).unwrap_err();
        assert_eq!(err.code, "format-unsupported");
    }

    #[test]
    fn virtual_on_platform_without_support_is_virtual_unsupported() {
        let reg = registry(false);
        let err = reg.mount(spec("m1", "source-0", "midi1")).unwrap_err();
        assert_eq!(err.code, "virtual-unsupported");
    }

    #[test]
    fn non_mirror_source_role_is_declined() {
        let reg = registry(true);
        let mut s = spec("m1", "source-0", "midi1");
        s.role = MountRole::Link;
        let err = reg.mount(s).unwrap_err();
        assert_eq!(err.code, "role-unsupported");
    }

    #[test]
    fn status_can_filter_by_mount_id() {
        let reg = registry(true);
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        reg.mount(spec("m2", "source-0", "midi1")).unwrap();
        assert_eq!(
            reg.status(Some("m1"))["mounts"].as_array().unwrap().len(),
            1
        );
        assert_eq!(reg.status(None)["mounts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn mount_and_unmount_emit_mount_state_notifications() {
        let reg = registry(true);
        let mut rx = reg.subscribe();
        reg.mount(spec("m1", "source-0", "midi1")).unwrap();
        // A `connecting` notification (§5) is emitted on mount.
        let n = rx.try_recv().expect("mount-state notification");
        assert_eq!(n["method"], "mount-state");
        assert_eq!(n["params"]["mount-id"], "m1");
        assert_eq!(n["params"]["state"], "connecting");
        assert!(n["params"]["stats"].is_object());

        reg.unmount("m1");
        let n = rx.try_recv().expect("torn-down notification");
        assert_eq!(n["params"]["state"], "torn-down");
        assert_eq!(n["params"]["mount-id"], "m1");
    }

    #[test]
    fn transition_only_emits_on_actual_change() {
        let (tx, mut rx) = broadcast::channel(8);
        let status = Arc::new(Mutex::new(MountStatus {
            mount_id: "m1".to_string(),
            state: MountState::Connecting,
            since: now_rfc3339(),
            stats: MountStats::default(),
            detail: None,
        }));
        transition(&status, &tx, MountState::Active, None);
        assert_eq!(rx.try_recv().unwrap()["params"]["state"], "active");
        // Same state again → no notification.
        transition(&status, &tx, MountState::Active, None);
        assert!(rx.try_recv().is_err());
    }
}
