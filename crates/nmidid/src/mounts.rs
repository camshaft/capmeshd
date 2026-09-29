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
use tokio::sync::{Notify, broadcast, mpsc};

use crate::protocol::{
    DaemonError, INVALID_PARAMS, MountRole, MountSpec, MountState, MountStats, MountStatus,
    RemoteEndpoint,
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

/// A place to push decoded MIDI bytes — the local end of a `mirror-source` mount
/// (a virtual source other apps read from). Owned by the mount's data-path task;
/// dropping it releases the underlying endpoint (e.g. removes the virtual MIDI
/// port).
pub trait MidiSink: Send {
    /// Send one MIDI message (raw status+data bytes) to the local endpoint.
    fn send(&self, message: &[u8]) -> anyhow::Result<()>;
}

/// The local end of a `mirror-sink` mount: a stream of MIDI messages produced by
/// local apps writing into the virtual sink, which the pump forwards out to the
/// remote sink over RTP-MIDI. Owned by the mount's data-path task; dropping it
/// releases the underlying endpoint (removes the virtual MIDI port).
pub trait MidiSource: Send {
    /// The channel carrying MIDI messages received from the local virtual sink.
    /// The pump awaits it; a `None` from `recv` means the endpoint is gone.
    fn receiver(&mut self) -> &mut mpsc::Receiver<Vec<u8>>;
}

/// Which local endpoint a mount materializes, handed to the [`Connector`] so the
/// one pump implementation can drive either data direction: `mirror-source` pushes
/// remote MIDI into a local [`MidiSink`]; `mirror-sink` pumps local MIDI out from a
/// [`MidiSource`] to the remote. A `link` mount reuses the same variants — it just
/// binds an *existing real* port instead of creating a virtual one (a real source
/// → [`LocalEnd::Sink`] forwarding out; a real sink → [`LocalEnd::Source`]
/// forwarding in).
pub enum LocalEnd {
    /// A local source (virtual or real) fed by the remote source: push remote
    /// MIDI into it via the [`MidiSink`].
    Source(Box<dyn MidiSink>),
    /// A local sink (virtual or real) whose MIDI is forwarded to the remote: read
    /// it via the [`MidiSource`].
    Sink(Box<dyn MidiSource>),
}

/// The outcome of binding a `link` mount to an existing real local port.
pub enum RealPort {
    /// The named real port was found and connected, bound to the data direction
    /// implied by the port itself (real source → forward out; real sink →
    /// forward in).
    Connected(LocalEnd),
    /// No local real port has the requested id (→ `no-such-port`).
    NotFound,
}

/// Materializes the local OS endpoint of a mount.
pub trait Mounter: Send + Sync {
    /// Whether this daemon/platform can create virtual endpoints.
    fn supports_virtual(&self) -> bool;

    /// Create a local virtual **source** (a port other apps read from, into
    /// which we push the remote source's events) named `display_name`, returning
    /// the sink to push into.
    fn create_virtual_source(&self, display_name: &str) -> anyhow::Result<Box<dyn MidiSink>>;

    /// Create a local virtual **sink** (a port other apps write to) named
    /// `display_name`, returning the source that yields the MIDI they send — the
    /// pump forwards it out to the remote sink.
    fn create_virtual_sink(&self, display_name: &str) -> anyhow::Result<Box<dyn MidiSource>>;

    /// Bind an **existing real** local port named by `port_id` (a `link` mount,
    /// §3.1) and return the [`LocalEnd`] wired to the direction the port implies:
    /// a real *source* (MIDI input, e.g. a hardware keyboard) → [`LocalEnd::Sink`]
    /// (read it, forward out); a real *sink* (MIDI output, e.g. a hardware synth)
    /// → [`LocalEnd::Source`] (receive from the remote, forward in). Returns
    /// [`RealPort::NotFound`] if no local port has that id; `Err` if the port is
    /// found but the OS connection fails.
    fn connect_real(&self, port_id: &str) -> anyhow::Result<RealPort>;
}

/// Drives a mount's data path: connects to the remote and pumps events between it
/// and the local endpoint, updating `status`. The task runs until the remote ends
/// the session or `cancel` is notified (unmount), at which point it tears the
/// session down gracefully (sends `End`) and drops the local endpoint.
pub trait Connector: Send + Sync {
    fn start(
        &self,
        remote: RemoteEndpoint,
        local: LocalEnd,
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

    /// Establish a mount (§3.1). Idempotent on `mount-id`: a repeat against a
    /// still-live mount returns the existing state rather than creating a second
    /// endpoint. A repeat against a *terminal* mount (`failed`/`torn-down`, whose
    /// pump task has exited) starts a fresh attempt — this is how capmeshd's
    /// reconciler retries a `failed` mount (§3.2 "on failed it retries").
    pub fn mount(&self, spec: MountSpec) -> Result<serde_json::Value, DaemonError> {
        {
            let mut mounts = self.mounts.lock().unwrap();
            let existing = mounts.get(&spec.mount_id).map(|e| {
                let s = e.status.lock().unwrap();
                (s.mount_id.clone(), s.state)
            });
            if let Some((mount_id, state)) = existing {
                // A mount in a terminal (dead) state — its pump task has exited
                // and its virtual port has been dropped — is stale. capmeshd's
                // reconciler retries a `failed` mount by re-issuing `mount` on the
                // same id (the idempotency key; CONTROL-PROTOCOL §3.2 "on failed it
                // retries"), so a re-issue here must start a *fresh* attempt rather
                // than echo the dead state forever. A still-live mount
                // (pending/connecting/active/degraded) stays idempotent.
                if matches!(state, MountState::Failed | MountState::TornDown) {
                    if let Some(dead) = mounts.remove(&spec.mount_id) {
                        // The task has already exited terminally; this is only a
                        // guard in case it is mid-teardown.
                        dead.cancel.notify_one();
                    }
                } else {
                    return Ok(serde_json::json!({
                        "mount-id": mount_id,
                        "state": state,
                    }));
                }
            }
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

        // Materialize the local endpoint matching the role's data direction:
        // mirror-source → a virtual source we push into; mirror-sink → a virtual
        // sink we read from and forward out; link → bind an existing real local
        // port (direction resolved from the daemon's own list-ports for that id).
        let local = match spec.role {
            MountRole::MirrorSource => LocalEnd::Source(
                self.mounter
                    .create_virtual_source(&display_name)
                    .map_err(|e| {
                        DaemonError::domain("busy", format!("could not create virtual port: {e}"))
                    })?,
            ),
            MountRole::MirrorSink => {
                LocalEnd::Sink(self.mounter.create_virtual_sink(&display_name).map_err(|e| {
                    DaemonError::domain("busy", format!("could not create virtual port: {e}"))
                })?)
            }
            MountRole::Link => {
                // A `link` binds an existing real local port named by
                // `local.port-id` (§3.1) — required for this role, and it must be
                // a port this daemon's list-ports advertises.
                let port_id = spec.local.port_id.as_deref().ok_or_else(|| {
                    DaemonError::protocol(
                        INVALID_PARAMS,
                        "invalid-params",
                        "a link mount requires local.port-id (the local real port to bind)",
                    )
                })?;
                match self.mounter.connect_real(port_id).map_err(|e| {
                    DaemonError::domain("busy", format!("could not bind local port: {e}"))
                })? {
                    RealPort::Connected(end) => end,
                    RealPort::NotFound => {
                        return Err(DaemonError::domain(
                            "no-such-port",
                            format!("no local port '{port_id}'"),
                        ));
                    }
                }
            }
        };

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
            local,
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

/// The `mirror-sink` local end: owns the `midir` virtual INPUT connection (whose
/// callback feeds `rx`) and hands the pump the receiving end. Dropping this drops
/// the connection, removing the virtual port.
#[cfg(unix)]
struct MidirSource {
    // Held only to keep the virtual input port alive; its callback feeds `rx`.
    _conn: midir::MidiInputConnection<()>,
    rx: mpsc::Receiver<Vec<u8>>,
}

#[cfg(unix)]
impl MidiSource for MidirSource {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Vec<u8>> {
        &mut self.rx
    }
}

/// Bound on the callback→pump channel for a virtual sink. MIDI is realtime, so a
/// full channel drops rather than blocks the OS callback (see `create_virtual_sink`).
#[cfg(unix)]
const SINK_CHANNEL_DEPTH: usize = 1024;

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

    fn create_virtual_sink(&self, display_name: &str) -> anyhow::Result<Box<dyn MidiSource>> {
        #[cfg(unix)]
        {
            use midir::MidiInput;
            use midir::os::unix::VirtualInput;
            // A virtual INPUT we own appears to other local apps as a writable
            // MIDI sink; the callback forwards each message they send into the
            // channel the pump reads. `try_send` never blocks the realtime MIDI
            // callback — under sustained overload the oldest-unsent message is
            // dropped rather than stalling the OS thread.
            let inp = MidiInput::new("nmidid")?;
            let (tx, rx) = mpsc::channel::<Vec<u8>>(SINK_CHANNEL_DEPTH);
            let conn = inp
                .create_virtual(
                    display_name,
                    move |_timestamp, message, _| {
                        let _ = tx.try_send(message.to_vec());
                    },
                    (),
                )
                .map_err(|e| anyhow::anyhow!("create_virtual (input) failed: {e}"))?;
            Ok(Box::new(MidirSource { _conn: conn, rx }))
        }
        #[cfg(not(unix))]
        {
            let _ = display_name;
            anyhow::bail!("virtual MIDI ports are not supported on this platform")
        }
    }

    fn connect_real(&self, port_id: &str) -> anyhow::Result<RealPort> {
        #[cfg(unix)]
        {
            use midir::{MidiInput, MidiOutput};
            use nmidi_core::midi::MidiPortType;

            // Resolve `port_id` against the same enumeration list-ports exposes, so
            // it names the port the caller saw. Unknown id → NotFound (no-such-port).
            let ports = nmidi_core::midi::detect_ports()?;
            let Some(info) = crate::ports::resolve_port(&ports, port_id) else {
                return Ok(RealPort::NotFound);
            };

            match info.port_type {
                // A real MIDI *input* (a source, e.g. a keyboard): read from it and
                // forward its events OUT to the remote sink — same as mirror-sink.
                MidiPortType::Input => {
                    let inp = MidiInput::new("nmidid")?;
                    let midir_ports = inp.ports();
                    let port = midir_ports.get(info.index).ok_or_else(|| {
                        anyhow::anyhow!("real MIDI input '{}' vanished before connect", info.name)
                    })?;
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(SINK_CHANNEL_DEPTH);
                    let conn = inp
                        .connect(
                            port,
                            "nmidid-link",
                            move |_timestamp, message, _| {
                                let _ = tx.try_send(message.to_vec());
                            },
                            (),
                        )
                        .map_err(|e| anyhow::anyhow!("connect real input failed: {e}"))?;
                    Ok(RealPort::Connected(LocalEnd::Sink(Box::new(MidirSource {
                        _conn: conn,
                        rx,
                    }))))
                }
                // A real MIDI *output* (a sink, e.g. a synth): receive from the
                // remote and forward INTO it — same as mirror-source.
                MidiPortType::Output => {
                    let out = MidiOutput::new("nmidid")?;
                    let midir_ports = out.ports();
                    let port = midir_ports.get(info.index).ok_or_else(|| {
                        anyhow::anyhow!("real MIDI output '{}' vanished before connect", info.name)
                    })?;
                    let conn = out
                        .connect(port, "nmidid-link")
                        .map_err(|e| anyhow::anyhow!("connect real output failed: {e}"))?;
                    Ok(RealPort::Connected(LocalEnd::Source(Box::new(MidirSink(
                        Mutex::new(conn),
                    )))))
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = port_id;
            anyhow::bail!("real MIDI ports are not supported on this platform")
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

/// A no-op `MidiSource` for tests: never yields a message (its sender is retained
/// so the channel stays open). Enough to satisfy the mirror-sink endpoint seam
/// where no pump actually polls it.
#[cfg(test)]
pub(crate) struct NullSource {
    _tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
}

#[cfg(test)]
impl NullSource {
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel(1);
        NullSource { _tx: tx, rx }
    }
}

#[cfg(test)]
impl MidiSource for NullSource {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Vec<u8>> {
        &mut self.rx
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
    fn create_virtual_sink(&self, _display_name: &str) -> anyhow::Result<Box<dyn MidiSource>> {
        Ok(Box::new(NullSource::new()))
    }
    fn connect_real(&self, _port_id: &str) -> anyhow::Result<RealPort> {
        Ok(RealPort::Connected(LocalEnd::Sink(Box::new(NullSource::new()))))
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
        _local: LocalEnd,
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
        fn create_virtual_sink(&self, _display_name: &str) -> anyhow::Result<Box<dyn MidiSource>> {
            Ok(Box::new(NullSource::new()))
        }
        fn connect_real(&self, port_id: &str) -> anyhow::Result<RealPort> {
            // Simulate list-ports resolution: a real source id → forward-out
            // (LocalEnd::Sink), a real sink id → forward-in (LocalEnd::Source),
            // anything else → NotFound.
            match port_id {
                "source-real" => {
                    Ok(RealPort::Connected(LocalEnd::Sink(Box::new(NullSource::new()))))
                }
                "sink-real" => Ok(RealPort::Connected(LocalEnd::Source(Box::new(NullSink)))),
                _ => Ok(RealPort::NotFound),
            }
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
                port_id: None,
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
    fn mount_mirror_sink_is_connecting_and_listed() {
        // mirror-sink creates a local virtual sink and drives the outbound pump;
        // it must be accepted (not declined) and reach `connecting`.
        let reg = registry(true);
        let mut s = spec("m1", "sink-0", "midi1");
        s.role = MountRole::MirrorSink;
        let r = reg.mount(s).unwrap();
        assert_eq!(r["state"], "connecting");
        assert_eq!(r["mount-id"], "m1");
        let mounts = reg.status(None)["mounts"].as_array().unwrap().clone();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["state"], "connecting");
    }

    #[test]
    fn mirror_sink_on_platform_without_virtual_support_is_declined() {
        let reg = registry(false);
        let mut s = spec("m1", "sink-0", "midi1");
        s.role = MountRole::MirrorSink;
        let err = reg.mount(s).unwrap_err();
        assert_eq!(err.code, "virtual-unsupported");
    }

    /// Build a `link` mount spec binding local real port `local_port_id`
    /// (`virtual: false`, no virtual endpoint).
    fn link_spec(mount_id: &str, local_port_id: Option<&str>) -> MountSpec {
        let mut s = spec(mount_id, "remote-port", "midi1");
        s.role = MountRole::Link;
        s.local.r#virtual = false;
        s.local.name = None;
        s.local.port_id = local_port_id.map(str::to_string);
        s
    }

    #[test]
    fn link_to_a_real_source_port_is_connecting() {
        // A real source (MIDI input) link binds and reaches connecting (forwards
        // its MIDI out to the remote).
        let reg = registry(true);
        let r = reg.mount(link_spec("m1", Some("source-real"))).unwrap();
        assert_eq!(r["state"], "connecting");
        assert_eq!(r["mount-id"], "m1");
        assert_eq!(reg.status(None)["mounts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn link_to_a_real_sink_port_is_connecting() {
        // A real sink (MIDI output) link binds and reaches connecting (receives
        // from the remote, forwards into it).
        let reg = registry(true);
        let r = reg.mount(link_spec("m1", Some("sink-real"))).unwrap();
        assert_eq!(r["state"], "connecting");
    }

    #[test]
    fn link_without_a_port_id_is_invalid_params() {
        let reg = registry(true);
        let err = reg.mount(link_spec("m1", None)).unwrap_err();
        assert_eq!(err.code, "invalid-params");
    }

    #[test]
    fn link_to_an_unknown_local_port_is_no_such_port() {
        let reg = registry(true);
        let err = reg.mount(link_spec("m1", Some("source-ghost"))).unwrap_err();
        assert_eq!(err.code, "no-such-port");
    }

    #[test]
    fn link_is_not_gated_by_virtual_support() {
        // A `link` binds an existing real port and creates no virtual endpoint, so
        // it must NOT be refused as virtual-unsupported even on a platform that
        // cannot create virtual ports.
        let reg = registry(false);
        let r = reg.mount(link_spec("m1", Some("source-real"))).unwrap();
        assert_eq!(r["state"], "connecting");
    }

    #[test]
    fn remount_after_terminal_state_reattempts_fresh() {
        // CONTROL-PROTOCOL §3.2: capmeshd's reconciler retries a `failed` mount by
        // re-issuing `mount` on the same id. A stale terminal entry (whose pump
        // task has exited and virtual port dropped) must NOT wedge that id — the
        // re-issue starts a fresh attempt.
        for terminal in [MountState::Failed, MountState::TornDown] {
            let reg = registry(true);
            reg.mount(spec("m1", "source-0", "midi1")).unwrap();
            // Simulate the pump reaching a terminal state and its task exiting.
            {
                let mounts = reg.mounts.lock().unwrap();
                mounts.get("m1").unwrap().status.lock().unwrap().state = terminal;
            }
            // Re-issue: must re-attempt (fresh `connecting`), not echo the dead state.
            let r = reg.mount(spec("m1", "source-0", "midi1")).unwrap();
            assert_eq!(r["state"], "connecting", "re-mount after {terminal:?} must re-attempt");
            let mounts = reg.status(None)["mounts"].as_array().unwrap().clone();
            assert_eq!(mounts.len(), 1, "still exactly one mount for the id");
            assert_eq!(mounts[0]["state"], "connecting");
        }
    }

    #[test]
    fn remount_while_live_stays_idempotent() {
        // A still-live mount (connecting/active/degraded) is NOT re-attempted: the
        // re-issue echoes the current state and does not create a second endpoint.
        for live in [MountState::Connecting, MountState::Active, MountState::Degraded] {
            let reg = registry(true);
            reg.mount(spec("m1", "source-0", "midi1")).unwrap();
            {
                let mounts = reg.mounts.lock().unwrap();
                mounts.get("m1").unwrap().status.lock().unwrap().state = live;
            }
            let r = reg.mount(spec("m1", "source-0", "midi1")).unwrap();
            let want = serde_json::to_value(live).unwrap();
            assert_eq!(r["state"], want, "re-mount while {live:?} echoes the live state");
            assert_eq!(reg.status(None)["mounts"].as_array().unwrap().len(), 1);
        }
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
