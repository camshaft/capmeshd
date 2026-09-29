//! The capmeshd binary's real [`ControlOps`] (DESIGN §7.1): back the embedded control-tools MCP
//! server with actual mesh discovery + a `capmesh-ctl` client to the data-plane daemon.
//!
//! This is the network glue under the tested control-tools stack (`control_mcp` dispatch →
//! `control_http` transport → `control_exec` mapping → `control_result` shaping). The pure decision
//! logic lives in `capmesh-daemon`; here we only perform the ops: discover/describe browse the mesh,
//! connect/disconnect/status drive the configured data-plane control socket. The one non-network
//! bit — turning a [`ConnectArgs`] into a [`MountSpec`] — is unit-tested.

use crate::{discover_capabilities, DiscoveredCapability};
use capmesh_ctl::{
    CtlClient, Format, LocalEndpoint, MountRole, MountSpec, MountStatus, RemoteEndpoint,
};
use capmesh_daemon::control_exec::{ControlOps, OpFuture};
use capmesh_daemon::control_mcp::ConnectArgs;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The default discover browse window (seconds) when a call doesn't specify one.
const DEFAULT_DISCOVER_SECS: u64 = 3;

/// Real control operations: mesh discovery + a `capmesh-ctl` client to the data-plane socket.
pub struct CapmeshControlOps {
    /// The data-plane control socket that connect/disconnect/status drive. `None` when no data-plane
    /// is configured — those ops then fail cleanly (discover/describe still work).
    socket: Option<PathBuf>,
}

impl CapmeshControlOps {
    /// Ops driving the given data-plane socket (or none, leaving connect/disconnect/status to fail
    /// with a clear message).
    pub fn new(socket: Option<PathBuf>) -> Self {
        Self { socket }
    }

    fn require_socket(&self) -> Result<PathBuf, String> {
        self.socket
            .clone()
            .ok_or_else(|| "no data-plane socket configured for connect/disconnect/status".to_string())
    }
}

impl ControlOps for CapmeshControlOps {
    fn discover(
        &self,
        kind: Option<String>,
        dir: Option<String>,
        host: Option<String>,
        timeout_secs: Option<u64>,
    ) -> OpFuture<Value> {
        Box::pin(async move {
            let secs = timeout_secs.unwrap_or(DEFAULT_DISCOVER_SECS);
            let caps = discover_capabilities(secs, kind.as_deref(), dir.as_deref(), host.as_deref())
                .await
                .map_err(|e| format!("discover failed: {e:#}"))?;
            Ok(Value::Array(caps.iter().map(cap_to_json).collect()))
        })
    }

    fn describe(&self, host: String, id: String) -> OpFuture<Value> {
        Box::pin(async move {
            let caps = discover_capabilities(DEFAULT_DISCOVER_SECS, None, None, Some(&host))
                .await
                .map_err(|e| format!("discover failed: {e:#}"))?;
            let cap = caps
                .iter()
                .find(|c| c.advert.id == id)
                .ok_or_else(|| format!("no capability '{id}' on host '{host}'"))?;
            Ok(cap_to_json(cap))
        })
    }

    fn connect(&self, args: ConnectArgs) -> OpFuture<MountStatus> {
        let socket = self.require_socket();
        Box::pin(async move {
            let socket = socket?;
            let spec = connect_args_to_spec(&args)?;
            let mut client = open_ctl(&socket).await?;
            let res = client
                .mount(&spec)
                .await
                .map_err(|e| format!("mount failed: {e}"))?;
            Ok(MountStatus {
                mount_id: res.mount_id,
                state: res.state,
                since: None,
                stats: None,
                detail: None,
            })
        })
    }

    fn disconnect(&self, mount_id: String) -> OpFuture<()> {
        let socket = self.require_socket();
        Box::pin(async move {
            let socket = socket?;
            let mut client = open_ctl(&socket).await?;
            client
                .unmount(&mount_id)
                .await
                .map_err(|e| format!("unmount failed: {e}"))?;
            Ok(())
        })
    }

    fn status(&self, mount_id: Option<String>) -> OpFuture<Vec<MountStatus>> {
        let socket = self.require_socket();
        Box::pin(async move {
            let socket = socket?;
            let mut client = open_ctl(&socket).await?;
            let res = client
                .mount_status(mount_id.as_deref())
                .await
                .map_err(|e| format!("mount-status failed: {e}"))?;
            Ok(res.mounts)
        })
    }
}

/// Open + handshake a `capmesh-ctl` client to the data-plane socket.
async fn open_ctl(socket: &Path) -> Result<CtlClient, String> {
    let mut client = CtlClient::connect(socket)
        .await
        .map_err(|e| format!("connect {}: {e}", socket.display()))?;
    client.hello().await.map_err(|e| format!("hello: {e}"))?;
    Ok(client)
}

/// Serialize a discovered capability into the structured JSON the `discover`/`describe` tools return.
fn cap_to_json(cap: &DiscoveredCapability) -> Value {
    json!({
        "host": cap.advert.host,
        "cap": cap.advert.cap,
        "dir": cap.advert.dir,
        "id": cap.advert.id,
        "addr": cap.addr.to_string(),
        "port": cap.port,
        "descriptor": cap.descriptor,
        "ports": cap.ports,
    })
}

/// Turn a `connect` tool-call's [`ConnectArgs`] into a [`MountSpec`] (§3.1) — the wire form of the
/// binary's `MountArgs::to_spec`. Pure (no network), so it's unit-tested.
fn connect_args_to_spec(args: &ConnectArgs) -> Result<MountSpec, String> {
    let role = crate::parse_role(&args.role).map_err(|e| e.to_string())?;
    let addr = args
        .remote_addr
        .parse()
        .map_err(|e| format!("invalid remote_addr '{}': {e}", args.remote_addr))?;
    let mount_id = args
        .mount_id
        .clone()
        .unwrap_or_else(|| format!("{}-{}", args.remote_host, args.remote_port_id));
    Ok(MountSpec {
        mount_id,
        role,
        local: LocalEndpoint {
            // Mirror roles materialize a local virtual endpoint; `link` binds a real port.
            is_virtual: !matches!(role, MountRole::Link),
            name: args.local_name.clone(),
            // The connect tool does not yet carry a local real-port selector for `link` (follow-on);
            // a link mount established here has no named local port until then.
            port_id: None,
        },
        remote: RemoteEndpoint {
            host: args.remote_host.clone(),
            addr,
            port: args.remote_port,
            port_id: args.remote_port_id.clone(),
        },
        format: Format {
            codec: args.codec.clone(),
            params: Default::default(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ConnectArgs {
        ConnectArgs {
            remote_host: "studio".into(),
            remote_addr: "192.168.1.23".into(),
            remote_port: 5004,
            remote_port_id: "kbd-0".into(),
            role: "mirror-source".into(),
            local_name: Some("studio keyboard".into()),
            codec: "midi1".into(),
            mount_id: None,
        }
    }

    #[test]
    fn connect_args_to_spec_maps_fields_and_defaults_the_mount_id() {
        let spec = connect_args_to_spec(&args()).unwrap();
        assert_eq!(spec.mount_id, "studio-kbd-0"); // <host>-<port-id>
        assert!(matches!(spec.role, MountRole::MirrorSource));
        assert!(spec.local.is_virtual); // mirror role → virtual local endpoint
        assert_eq!(spec.remote.addr.to_string(), "192.168.1.23");
        assert_eq!(spec.remote.port, 5004);
        assert_eq!(spec.format.codec, "midi1");
    }

    #[test]
    fn link_role_binds_a_real_local_port() {
        let mut a = args();
        a.role = "link".into();
        let spec = connect_args_to_spec(&a).unwrap();
        assert!(matches!(spec.role, MountRole::Link));
        assert!(!spec.local.is_virtual);
    }

    #[test]
    fn a_bad_role_or_addr_is_a_clean_error() {
        let mut a = args();
        a.role = "bogus".into();
        assert!(connect_args_to_spec(&a).is_err());

        let mut a = args();
        a.remote_addr = "not-an-ip".into();
        let err = connect_args_to_spec(&a).unwrap_err();
        assert!(err.contains("invalid remote_addr"));
    }
}
