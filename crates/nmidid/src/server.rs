//! The control-socket server: NDJSON/JSON-RPC framing, the per-connection
//! handshake state machine, and method dispatch.
//!
//! Method handling is split from the transport so it can be unit-tested over an
//! in-memory pipe: [`serve_connection`] drives the framing over any
//! async reader/writer, and [`Session`] owns the per-connection dispatch.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::mounts::MountRegistry;
use crate::ports::PortProvider;
use crate::protocol::{
    self, CAPABILITIES, DAEMON_ID, DaemonError, INVALID_PARAMS, METHOD_NOT_FOUND, MountSpec,
    PARSE_ERROR, PROTOCOL_MAJOR, Request,
};

/// Socket peer-credential policy (CONTROL-PROTOCOL.md §1.1). The control socket
/// is local-trust-only; a daemon refuses connections whose peer credentials fall
/// outside the configured owner/group.
///
/// The daemon's own uid is always allowed (so an operator can't lock themselves
/// out). If no allow-rules are configured the policy is **non-enforcing**
/// (allows all, relying on the socket's `0o660` file permissions) — this keeps
/// the default non-breaking until a deployment configures the shared group.
#[derive(Debug, Clone)]
pub struct PeerPolicy {
    allow_uids: Vec<u32>,
    allow_gids: Vec<u32>,
    self_uid: u32,
}

impl PeerPolicy {
    /// Build from configured allowed uids/gids, allowing the daemon's own uid.
    pub fn new(allow_uids: Vec<u32>, allow_gids: Vec<u32>) -> Self {
        // SAFETY: geteuid is always successful and has no preconditions.
        let self_uid = unsafe { libc::geteuid() };
        PeerPolicy {
            allow_uids,
            allow_gids,
            self_uid,
        }
    }

    /// Test constructor with an explicit daemon uid (avoids depending on the
    /// runtime euid).
    #[cfg(test)]
    fn with_self_uid(allow_uids: Vec<u32>, allow_gids: Vec<u32>, self_uid: u32) -> Self {
        PeerPolicy {
            allow_uids,
            allow_gids,
            self_uid,
        }
    }

    /// Whether any allow-rule is configured (enforcement is active).
    pub fn enforcing(&self) -> bool {
        !self.allow_uids.is_empty() || !self.allow_gids.is_empty()
    }

    /// Whether a peer with the given uid and full group set may connect.
    ///
    /// `gids` is the peer's complete group membership (primary gid plus any
    /// supplementary groups), because the shared-group trust model keys on a
    /// group a client typically holds as a *supplementary* group — e.g. a
    /// systemd `DynamicUser` whose primary gid is transient but which joins the
    /// shared `capmesh` group via `SupplementaryGroups`. See `peer_gids`.
    pub fn allows(&self, uid: u32, gids: &[u32]) -> bool {
        if !self.enforcing() {
            return true;
        }
        if uid == self.self_uid || self.allow_uids.contains(&uid) {
            return true;
        }
        gids.iter().any(|g| self.allow_gids.contains(g))
    }
}

/// The full set of group ids to test a peer against: its primary gid plus any
/// supplementary groups. `SO_PEERCRED` carries only the peer's primary gid, so
/// on Linux we read its supplementary groups from `/proc/<pid>/status` (the pid
/// also comes from `SO_PEERCRED`). This is what lets a policy keyed on a shared
/// *supplementary* group match a client whose primary gid is something else.
fn peer_gids(cred: &tokio::net::unix::UCred) -> Vec<u32> {
    let mut gids = vec![cred.gid()];
    #[cfg(target_os = "linux")]
    if let Some(pid) = cred.pid()
        && let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status"))
    {
        for g in parse_status_groups(&status) {
            if !gids.contains(&g) {
                gids.push(g);
            }
        }
    }
    gids
}

/// Parse the `Groups:` line of `/proc/<pid>/status` into supplementary gids.
fn parse_status_groups(status: &str) -> Vec<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Groups:"))
        .map(|rest| rest.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}

/// Resolve a group name to its gid from `/etc/group`.
///
/// The shared-group trust model coordinates on a group *name* (default
/// `capmesh`), not a gid — an auto-allocated NixOS group has no gid known at
/// evaluation time, so the deployment authorizes the daemon by name and the
/// daemon resolves it here at startup. NixOS materializes every declared group
/// into `/etc/group`, so a name lookup there resolves the shared group without
/// pinning a magic gid anywhere.
pub fn resolve_group_gid(name: &str) -> Option<u32> {
    let contents = std::fs::read_to_string("/etc/group").ok()?;
    parse_group_gid(&contents, name)
}

/// Find `name`'s gid in `/etc/group`-formatted content (`name:passwd:gid:members`).
fn parse_group_gid(group_file: &str, name: &str) -> Option<u32> {
    group_file.lines().find_map(|line| {
        let mut fields = line.split(':');
        let gname = fields.next()?;
        let _passwd = fields.next()?;
        let gid = fields.next()?;
        (gname == name).then(|| gid.parse().ok())?
    })
}

/// Per-connection dispatch state. One `Session` exists per accepted connection;
/// the `mounts` registry is shared daemon-wide across every connection.
pub struct Session {
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
    /// A compatible `hello` must complete before any other method (§1.2).
    hello_done: bool,
}

impl Session {
    pub fn new(ports: Arc<dyn PortProvider>, mounts: Arc<MountRegistry>) -> Self {
        Session {
            ports,
            mounts,
            hello_done: false,
        }
    }

    /// Dispatch one parsed request, yielding the JSON-RPC `result` value or a
    /// [`DaemonError`].
    fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, DaemonError> {
        // The handshake gate: nothing but `hello` is served until `hello` succeeds.
        if method != "hello" && !self.hello_done {
            return Err(DaemonError::domain(
                "not-ready",
                "hello must be the first request on a connection",
            ));
        }

        match method {
            "hello" => self.handle_hello(params),
            "list-ports" => self.handle_list_ports(),
            "describe-port" => self.handle_describe_port(params),
            "mount" => self.handle_mount(params),
            "unmount" => self.handle_unmount(params),
            "mount-status" => self.handle_mount_status(params),
            other => Err(DaemonError::protocol(
                METHOD_NOT_FOUND,
                "method-not-found",
                format!("unknown method '{other}'"),
            )),
        }
    }

    fn handle_mount(&self, params: &Value) -> Result<Value, DaemonError> {
        let spec: MountSpec = serde_json::from_value(params.clone()).map_err(|e| {
            DaemonError::protocol(
                INVALID_PARAMS,
                "invalid-params",
                format!("invalid mount spec: {e}"),
            )
        })?;
        self.mounts.mount(spec)
    }

    fn handle_unmount(&self, params: &Value) -> Result<Value, DaemonError> {
        let mount_id = params
            .get("mount-id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "unmount requires a 'mount-id'",
                )
            })?;
        Ok(self.mounts.unmount(mount_id))
    }

    fn handle_mount_status(&self, params: &Value) -> Result<Value, DaemonError> {
        let mount_id = params.get("mount-id").and_then(Value::as_str);
        Ok(self.mounts.status(mount_id))
    }

    fn handle_hello(&mut self, params: &Value) -> Result<Value, DaemonError> {
        let major = params
            .get("protocol")
            .and_then(parse_major)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "hello requires a 'protocol' major version",
                )
            })?;

        if major != PROTOCOL_MAJOR {
            return Err(DaemonError::domain(
                "unsupported-protocol",
                format!("daemon speaks protocol {PROTOCOL_MAJOR}, client requested {major}"),
            )
            .with_data("supported", Value::from(PROTOCOL_MAJOR)));
        }

        self.hello_done = true;
        Ok(serde_json::json!({
            "protocol": PROTOCOL_MAJOR.to_string(),
            "daemon": DAEMON_ID,
            "capabilities": CAPABILITIES,
        }))
    }

    fn handle_list_ports(&self) -> Result<Value, DaemonError> {
        let ports = self.ports.list_ports().map_err(|e| {
            DaemonError::protocol(
                protocol::DAEMON_DOMAIN,
                "internal",
                format!("failed to enumerate ports: {e}"),
            )
        })?;
        Ok(serde_json::json!({ "ports": ports }))
    }

    fn handle_describe_port(&self, params: &Value) -> Result<Value, DaemonError> {
        let port_id = params
            .get("port-id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DaemonError::protocol(
                    INVALID_PARAMS,
                    "invalid-params",
                    "describe-port requires a 'port-id'",
                )
            })?;

        let ports = self.ports.list_ports().map_err(|e| {
            DaemonError::protocol(
                protocol::DAEMON_DOMAIN,
                "internal",
                format!("failed to enumerate ports: {e}"),
            )
        })?;

        ports
            .into_iter()
            .find(|p| p.port_id == port_id)
            .map(|p| serde_json::to_value(p).expect("PortDescriptor serializes"))
            .ok_or_else(|| {
                DaemonError::domain("no-such-port", format!("no local port '{port_id}'"))
            })
    }

    /// Handle one already-parsed request, producing the response value to send
    /// (or `None` for a JSON-RPC notification, which gets no reply).
    fn respond(&mut self, req: Request) -> Option<Value> {
        let outcome = self.dispatch(&req.method, &req.params);
        match req.id {
            // A request: always answer, matching the id.
            Some(id) => Some(match outcome {
                Ok(result) => protocol::success_response(id, result),
                Err(err) => protocol::error_response(id, &err),
            }),
            // A notification: no reply, even on error (§1). Log a failure.
            None => {
                if let Err(err) = outcome {
                    debug!("notification '{}' failed: {}", req.method, err.message);
                }
                None
            }
        }
    }
}

/// Parse a protocol major version from a JSON value that may be a stringified
/// integer (`"1"`) or a number (`1`).
fn parse_major(v: &Value) -> Option<u32> {
    match v {
        Value::String(s) => s
            .split(|c: char| !c.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse().ok()),
        Value::Number(n) => n.as_u64().and_then(|u| u32::try_from(u).ok()),
        _ => None,
    }
}

/// Drive the NDJSON/JSON-RPC framing for one connection over any reader/writer.
///
/// Requests are read one JSON value per line, dispatched, and answered; a
/// malformed line yields a JSON-RPC parse error with a null id and the
/// connection stays open. Concurrently, unsolicited `mount-state` notifications
/// (§5) from the daemon's broadcast are written to the same connection. Reading
/// and writing run as separate concurrent halves joined by a response channel,
/// so a pending `read_line` is never cancelled by a notification arriving.
pub async fn serve_connection<R, W>(
    mut reader: R,
    mut writer: W,
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut notif_rx = mounts.subscribe();
    let mut session = Session::new(ports, mounts);
    let (resp_tx, mut resp_rx) = mpsc::channel::<Value>(64);

    // Reader half: parse + dispatch requests, forward responses to the writer.
    // `move` so `resp_tx` is owned here and dropped when this half ends (EOF),
    // which is what signals the writer half to finish.
    let reader_half = async move {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .context("reading control line")?;
            if n == 0 {
                break; // EOF: peer closed the connection.
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Request>(trimmed) {
                Ok(req) => session.respond(req),
                Err(e) => Some(protocol::error_response(
                    Value::Null,
                    &DaemonError::protocol(
                        PARSE_ERROR,
                        "parse-error",
                        format!("invalid JSON: {e}"),
                    ),
                )),
            };
            if let Some(value) = response
                && resp_tx.send(value).await.is_err()
            {
                break; // writer gone
            }
        }
        Ok::<(), anyhow::Error>(())
        // `resp_tx` drops here, signalling the writer half to finish.
    };

    // Writer half: interleave responses and unsolicited notifications, one JSON
    // value per line. Both channel recvs are cancel-safe.
    let writer_half = async move {
        loop {
            let value = tokio::select! {
                biased;
                resp = resp_rx.recv() => match resp {
                    Some(v) => v,
                    None => break, // reader half ended
                },
                notif = notif_rx.recv() => match notif {
                    Ok(v) => v,
                    // Lagged: we dropped some notifications under load; the peer
                    // reconciles via mount-status. Closed: shouldn't happen while
                    // the connection holds the registry. Either way, keep serving.
                    Err(_) => continue,
                },
            };
            let mut buf = serde_json::to_vec(&value).context("serializing message")?;
            buf.push(b'\n');
            writer.write_all(&buf).await.context("writing message")?;
            writer.flush().await.context("flushing message")?;
        }
        Ok::<(), anyhow::Error>(())
    };

    let (reader_result, writer_result) = tokio::join!(reader_half, writer_half);
    reader_result?;
    writer_result?;
    Ok(())
}

/// Bind the control socket at `path` and serve connections until a termination
/// signal (SIGTERM/SIGINT) arrives, then tear every mount down gracefully.
///
/// Any stale socket file at `path` is removed first. The socket is set to
/// owner/group read-write (`0o660`) and each peer is checked against `peers` —
/// the local trust boundary (§1.1). `mounts` is the shared, daemon-wide mount
/// registry.
pub async fn run(
    path: impl AsRef<Path>,
    ports: Arc<dyn PortProvider>,
    mounts: Arc<MountRegistry>,
    peers: PeerPolicy,
) -> Result<()> {
    let path = path.as_ref();

    // Ensure the socket's parent directory exists. Under systemd `RuntimeDirectory`
    // creates it, but a manual or non-systemd run should not fail with ENOENT.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket directory {}", parent.display()))?;
    }

    // Remove a stale socket from a previous run so bind() doesn't fail with
    // "address already in use".
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }

    let listener =
        UnixListener::bind(path).with_context(|| format!("binding socket {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }

    info!(
        "nmidid control socket listening on {} (peer-cred enforcement: {})",
        path.display(),
        if peers.enforcing() { "on" } else { "off" }
    );

    let mut sigterm =
        signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

    loop {
        let (stream, _addr) = tokio::select! {
            accepted = listener.accept() => accepted.context("accepting connection")?,
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down");
                break;
            }
        };

        // Local-trust boundary (§1.1): refuse peers outside the configured
        // owner/group. Test the peer's full group set (primary + supplementary)
        // so a shared-group policy matches a DynamicUser client.
        match stream.peer_cred() {
            Ok(cred) => {
                let gids = peer_gids(&cred);
                if !peers.allows(cred.uid(), &gids) {
                    warn!(
                        "refused control connection from uid {} gids {:?} (not permitted)",
                        cred.uid(),
                        gids
                    );
                    continue;
                }
            }
            Err(e) => {
                // Fail closed while enforcing; otherwise proceed.
                if peers.enforcing() {
                    warn!("refused control connection: peer credentials unavailable: {e}");
                    continue;
                }
            }
        }
        debug!("control connection accepted");
        let ports = Arc::clone(&ports);
        let mounts = Arc::clone(&mounts);
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            let reader = BufReader::new(read_half);
            if let Err(e) = serve_connection(reader, write_half, ports, mounts).await {
                warn!("control connection ended with error: {e}");
            } else {
                debug!("control connection closed");
            }
        });
    }

    // Graceful shutdown: tear every mount down (each peer receives a BY) and give
    // the pump tasks a brief window to send it before the runtime stops, then
    // remove the socket file.
    let torn = mounts.shutdown_all();
    if torn > 0 {
        info!("shutting down: torn down {torn} active mount(s)");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::{NullConnector, NullMounter};
    use crate::protocol::{
        Format, LocalEndpoint, MountRole, MountSpec, PortDescriptor, RemoteEndpoint,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct StaticPorts(Vec<PortDescriptor>);

    impl PortProvider for StaticPorts {
        fn list_ports(&self) -> anyhow::Result<Vec<PortDescriptor>> {
            Ok(self.0.clone())
        }
    }

    fn sample_ports() -> Arc<dyn PortProvider> {
        Arc::new(StaticPorts(vec![
            PortDescriptor {
                port_id: "source-0".to_string(),
                kind: "stream".to_string(),
                dir: Some("source".to_string()),
                r#type: "midi".to_string(),
                name: "Keystation 49e".to_string(),
                virtualizable: true,
                formats: vec![Format::midi1()],
            },
            PortDescriptor {
                port_id: "sink-0".to_string(),
                kind: "stream".to_string(),
                dir: Some("sink".to_string()),
                r#type: "midi".to_string(),
                name: "SuperCollider".to_string(),
                virtualizable: true,
                formats: vec![Format::midi1()],
            },
        ]))
    }

    /// Drive a fixed set of request lines through a real `serve_connection`
    /// over an in-memory duplex pipe, returning each response line's parsed
    /// JSON in order.
    async fn exchange(ports: Arc<dyn PortProvider>, requests: &[Value]) -> Vec<Value> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::new(NullConnector),
        ));
        let handle = tokio::spawn(async move {
            serve_connection(BufReader::new(sr), sw, ports, mounts)
                .await
                .ok();
        });

        for req in requests {
            let mut line = serde_json::to_vec(req).unwrap();
            line.push(b'\n');
            client.write_all(&line).await.unwrap();
        }
        client.flush().await.unwrap();
        // Signal EOF so the server loop ends and flushes.
        client.shutdown().await.unwrap();

        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        handle.await.unwrap();

        buf.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn hello() -> Value {
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"hello",
            "params":{"protocol":"1","client":"capmeshd/0.1"}})
    }

    #[test]
    fn peer_policy_non_enforcing_allows_all() {
        let p = PeerPolicy::with_self_uid(vec![], vec![], 1000);
        assert!(!p.enforcing());
        assert!(p.allows(1234, &[5678]));
    }

    #[test]
    fn peer_policy_enforcing_allows_self_uid_and_listed() {
        let p = PeerPolicy::with_self_uid(vec![42], vec![7], 1000);
        assert!(p.enforcing());
        assert!(p.allows(1000, &[999])); // daemon's own uid always allowed
        assert!(p.allows(42, &[999])); // listed uid
        assert!(p.allows(0, &[7])); // listed gid as primary
        assert!(!p.allows(5, &[5])); // neither → refused
    }

    #[test]
    fn peer_policy_matches_a_listed_supplementary_group() {
        // The shared-group model: the client's primary gid is transient (e.g. a
        // DynamicUser) but it holds the allowed group as a *supplementary* gid.
        let p = PeerPolicy::with_self_uid(vec![], vec![7], 1000);
        assert!(p.allows(5, &[65534, 7])); // primary 65534 not listed, supp 7 is
        assert!(!p.allows(5, &[65534, 8])); // no listed gid anywhere → refused
    }

    #[test]
    fn parse_status_groups_reads_the_groups_line() {
        let status = "Name:\tcapmeshd\nUid:\t997\t997\t997\t997\nGid:\t65534\t65534\t65534\t65534\nGroups:\t7 24 \n";
        assert_eq!(parse_status_groups(status), vec![7, 24]);
        assert_eq!(parse_status_groups("Groups:\t\n"), Vec::<u32>::new());
        assert_eq!(parse_status_groups("no groups line here\n"), Vec::<u32>::new());
    }

    #[test]
    fn parse_group_gid_reads_etc_group() {
        let g = "root:x:0:\ncapmesh:x:989:capmeshd\naudio:x:29:alice,bob\n";
        assert_eq!(parse_group_gid(g, "capmesh"), Some(989));
        assert_eq!(parse_group_gid(g, "root"), Some(0));
        assert_eq!(parse_group_gid(g, "audio"), Some(29));
        assert_eq!(parse_group_gid(g, "nope"), None);
    }

    #[tokio::test]
    async fn hello_returns_daemon_identity_and_capabilities() {
        let out = exchange(sample_ports(), &[hello()]).await;
        assert_eq!(out.len(), 1);
        let r = &out[0]["result"];
        assert_eq!(r["protocol"], "1");
        assert_eq!(r["daemon"], DAEMON_ID);
        let caps: Vec<String> = serde_json::from_value(r["capabilities"].clone()).unwrap();
        assert!(caps.contains(&"midi1".to_string()));
        assert!(caps.contains(&"virtual-endpoints".to_string()));
    }

    #[tokio::test]
    async fn methods_before_hello_are_not_ready() {
        let list = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"list-ports","params":{}});
        let out = exchange(sample_ports(), &[list]).await;
        assert_eq!(out[0]["error"]["data"]["code"], "not-ready");
    }

    #[tokio::test]
    async fn list_ports_after_hello_returns_descriptors() {
        let list = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"list-ports","params":{}});
        let out = exchange(sample_ports(), &[hello(), list]).await;
        let ports = out[1]["result"]["ports"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0]["port-id"], "source-0");
        assert_eq!(ports[0]["dir"], "source");
        assert_eq!(ports[0]["type"], "midi");
        assert_eq!(ports[0]["formats"][0]["codec"], "midi1");
    }

    #[tokio::test]
    async fn describe_known_and_unknown_port() {
        let known = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"describe-port","params":{"port-id":"sink-0"}});
        let unknown = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"describe-port","params":{"port-id":"nope"}});
        let out = exchange(sample_ports(), &[hello(), known, unknown]).await;
        assert_eq!(out[1]["result"]["name"], "SuperCollider");
        assert_eq!(out[2]["error"]["data"]["code"], "no-such-port");
    }

    #[tokio::test]
    async fn mount_roundtrip_over_socket() {
        let mount = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"mount","params":{
            "mount-id":"m1","role":"mirror-source",
            "local":{"virtual":true,"name":"laptop: Keystation"},
            "remote":{"host":"laptop","addr":"192.168.1.23","port":5004,"port-id":"source-0"},
            "format":{"codec":"midi1"}}});
        let status =
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"mount-status","params":{}});
        let unmount = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"unmount","params":{"mount-id":"m1"}});
        let status2 =
            serde_json::json!({"jsonrpc":"2.0","id":5,"method":"mount-status","params":{}});
        let out = exchange(sample_ports(), &[hello(), mount, status, unmount, status2]).await;
        assert_eq!(out[1]["result"]["state"], "connecting");
        assert_eq!(out[1]["result"]["mount-id"], "m1");
        assert_eq!(out[2]["result"]["mounts"].as_array().unwrap().len(), 1);
        assert_eq!(out[2]["result"]["mounts"][0]["state"], "connecting");
        assert!(out[3]["result"].is_object()); // unmount → {}
        assert_eq!(out[4]["result"]["mounts"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn mount_state_notifications_are_pushed_to_the_connection() {
        let ports = sample_ports();
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::new(NullConnector),
        ));

        let (client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let mounts_srv = Arc::clone(&mounts);
        let handle = tokio::spawn(async move {
            serve_connection(BufReader::new(sr), sw, ports, mounts_srv)
                .await
                .ok()
        });

        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr);

        async fn read_line_ok(cr: &mut (impl AsyncBufReadExt + Unpin), what: &str) -> Value {
            let mut line = String::new();
            let n =
                tokio::time::timeout(std::time::Duration::from_secs(2), cr.read_line(&mut line))
                    .await
                    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                    .unwrap();
            assert!(n > 0, "unexpected EOF waiting for {what}");
            serde_json::from_str(&line).unwrap()
        }

        // hello — receiving the response proves the connection has subscribed to
        // notifications (subscribe happens before the reader loop).
        let mut hb = serde_json::to_vec(&hello()).unwrap();
        hb.push(b'\n');
        cw.write_all(&hb).await.unwrap();
        let hello_resp = read_line_ok(&mut cr, "hello response").await;
        assert_eq!(hello_resp["result"]["daemon"], DAEMON_ID);

        let spec = MountSpec {
            mount_id: "m1".to_string(),
            role: MountRole::MirrorSource,
            local: LocalEndpoint {
                r#virtual: true,
                name: None,
            },
            remote: RemoteEndpoint {
                host: None,
                addr: "192.168.1.23".to_string(),
                port: 5004,
                port_id: "source-0".to_string(),
            },
            format: Format::midi1(),
        };

        // A mount emits a `connecting` mount-state notification, pushed unsolicited.
        mounts.mount(spec).unwrap();
        let v = read_line_ok(&mut cr, "connecting notification").await;
        assert_eq!(v["method"], "mount-state");
        assert_eq!(v["params"]["mount-id"], "m1");
        assert_eq!(v["params"]["state"], "connecting");

        // Unmount emits a `torn-down` notification.
        mounts.unmount("m1");
        let v2 = read_line_ok(&mut cr, "torn-down notification").await;
        assert_eq!(v2["params"]["state"], "torn-down");

        drop(cw);
        handle.abort();
    }

    #[tokio::test]
    async fn mount_accepts_a_remote_port_id_unknown_to_this_host() {
        // `remote.port-id` names a port on the peer, not this host, so it is not
        // validated against local ports — the mount proceeds to `connecting`.
        let mount = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"mount","params":{
            "mount-id":"m1","role":"mirror-source",
            "local":{"virtual":true},
            "remote":{"addr":"192.168.1.23","port":5004,"port-id":"ghost-9"},
            "format":{"codec":"midi1"}}});
        let out = exchange(sample_ports(), &[hello(), mount]).await;
        assert_eq!(out[1]["result"]["state"], "connecting");
    }

    #[tokio::test]
    async fn unsupported_protocol_major_is_rejected() {
        let bad = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"hello",
            "params":{"protocol":"2","client":"capmeshd/0.1"}});
        let out = exchange(sample_ports(), &[bad]).await;
        assert_eq!(out[0]["error"]["data"]["code"], "unsupported-protocol");
    }

    #[tokio::test]
    async fn malformed_line_yields_parse_error_and_keeps_serving() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let ports = sample_ports();
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::new(NullConnector),
        ));
        let handle = tokio::spawn(async move {
            serve_connection(BufReader::new(sr), sw, ports, mounts)
                .await
                .ok()
        });

        client.write_all(b"{ this is not json\n").await.unwrap();
        let mut hello_line = serde_json::to_vec(&hello()).unwrap();
        hello_line.push(b'\n');
        client.write_all(&hello_line).await.unwrap();
        client.shutdown().await.unwrap();

        let mut buf = String::new();
        client.read_to_string(&mut buf).await.unwrap();
        handle.await.unwrap();

        let lines: Vec<Value> = buf
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["error"]["data"]["code"], "parse-error");
        assert_eq!(lines[0]["id"], Value::Null);
        // The connection kept serving: hello still got a result.
        assert_eq!(lines[1]["result"]["daemon"], DAEMON_ID);
    }

    /// Well-formed JSON-RPC whose method params are missing/invalid must yield
    /// `invalid-params` (not a crash or the wrong code), and the connection must
    /// keep serving afterwards.
    #[tokio::test]
    async fn malformed_params_yield_invalid_params_and_keep_serving() {
        // mount missing local/remote/format; unmount missing mount-id;
        // describe-port missing port-id.
        let bad_mount = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"mount",
            "params":{"mount-id":"m1","role":"mirror-source"}});
        let bad_unmount =
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"unmount","params":{}});
        let bad_describe =
            serde_json::json!({"jsonrpc":"2.0","id":4,"method":"describe-port","params":{}});
        let list = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"list-ports","params":{}});
        let out = exchange(
            sample_ports(),
            &[hello(), bad_mount, bad_unmount, bad_describe, list],
        )
        .await;
        assert_eq!(out[1]["error"]["data"]["code"], "invalid-params");
        assert_eq!(out[2]["error"]["data"]["code"], "invalid-params");
        assert_eq!(out[3]["error"]["data"]["code"], "invalid-params");
        // Every bad request was survived; the connection still serves.
        assert!(out[4]["result"]["ports"].is_array());
    }

    /// Exercise the real `run` accept path over an actual Unix socket: bind an
    /// *enforcing* policy that does not name us, then connect from this same
    /// process. The connection must still be served via the always-allowed
    /// self-uid bypass (§1.1), proving the wired `peer_cred()` gate lets a
    /// legitimate local peer through, and the socket is created `0o660`.
    #[tokio::test]
    async fn run_serves_a_permitted_connection_over_a_real_socket() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        use tokio::io::AsyncBufReadExt;
        use tokio::net::UnixStream;

        let path = std::env::temp_dir().join(format!(
            "nmidid-test-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let ports = sample_ports();
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::new(NullConnector),
        ));
        // Enforcing, but the allow-list does NOT name us; the connection is
        // permitted only by the self-uid bypass (client is this process).
        let peers = PeerPolicy::new(vec![999_999], vec![]);
        assert!(peers.enforcing());

        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run(&server_path, ports, mounts, peers).await.ok();
        });

        // Retry until the listener is bound (run() spawned above), bounded.
        let stream = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match UnixStream::connect(&path).await {
                    Ok(s) => break s,
                    Err(_) if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(e) => panic!("could not connect to nmidid socket: {e}"),
                }
            }
        };

        // §1.1: the control socket is owner/group-only.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "socket perms should be 0o660");

        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut line = serde_json::to_vec(&hello()).unwrap();
        line.push(b'\n');
        w.write_all(&line).await.unwrap();

        let mut resp = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut resp))
            .await
            .expect("hello response within timeout")
            .expect("read a response line");
        let v: Value = serde_json::from_str(resp.trim()).unwrap();
        assert_eq!(v["result"]["daemon"], DAEMON_ID);

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    /// `run` creates the socket's parent directory if it does not exist, so a
    /// manual or non-systemd launch does not fail with ENOENT.
    #[tokio::test]
    async fn run_creates_the_socket_parent_directory() {
        use std::time::{Duration, Instant};
        use tokio::net::UnixStream;

        let dir = std::env::temp_dir().join(format!(
            "nmidid-test-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!dir.exists(), "test precondition: parent dir absent");
        let path = dir.join("nmidid.sock");

        let ports = sample_ports();
        let mounts = Arc::new(MountRegistry::new(
            Arc::new(NullMounter),
            Arc::new(NullConnector),
        ));
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run(&server_path, ports, mounts, PeerPolicy::new(vec![], vec![]))
                .await
                .ok();
        });

        // If we can connect, the parent dir was created and the socket bound.
        let deadline = Instant::now() + Duration::from_secs(5);
        let connected = loop {
            match UnixStream::connect(&path).await {
                Ok(_) => break true,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(_) => break false,
            }
        };
        assert!(connected, "run did not bind a socket under a fresh parent dir");
        assert!(dir.exists(), "run should have created the socket parent dir");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
