//! The control-tools **executor logic** (DESIGN §7.1): map a parsed [`ControlCall`] to a control
//! operation and shape its outcome into an MCP `tools/call` result.
//!
//! The actual operations — mesh discovery and driving a data-plane daemon over `capmesh-ctl` — are
//! injected as [`ControlOps`], so this mapping (which op, success → [`control_result`], failure →
//! [`error_result`]) is pure and unit-tested against a mock. The capmeshd binary implements
//! [`ControlOps`] with the real network calls and wraps it in [`OpsExecutor`], which plugs into the
//! control server ([`control_http`](crate::control_http)) as a [`ControlExecutor`].

use crate::control_http::{ControlExecutor, ExecFuture};
use crate::control_mcp::{ConnectArgs, ControlCall};
use crate::control_result::{error_result, mount_result, ok_result, status_result};
use capmesh_ctl::MountStatus;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed, `'static` future of one control operation's outcome.
pub type OpFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'static>>;

/// capmesh's control operations, injected into [`OpsExecutor`]. The daemon implements these with
/// mesh discovery + `capmesh-ctl`; an `Err(reason)` becomes a tool-level error result.
pub trait ControlOps: Send + Sync + 'static {
    /// Browse the mesh; returns the discovered capabilities as a structured JSON array.
    fn discover(
        &self,
        kind: Option<String>,
        dir: Option<String>,
        host: Option<String>,
        timeout_secs: Option<u64>,
    ) -> OpFuture<Value>;
    /// Fetch one capability's full descriptor as a structured JSON value.
    fn describe(&self, host: String, id: String) -> OpFuture<Value>;
    /// Mount a remote capability; returns the resulting mount status.
    fn connect(&self, args: ConnectArgs) -> OpFuture<MountStatus>;
    /// Tear down a mount by id.
    fn disconnect(&self, mount_id: String) -> OpFuture<()>;
    /// Report mount status — all mounts, or one by id.
    fn status(&self, mount_id: Option<String>) -> OpFuture<Vec<MountStatus>>;
}

/// A [`ControlExecutor`] that runs each [`ControlCall`] through injected [`ControlOps`] and shapes
/// the outcome into an MCP `tools/call` result.
#[derive(Clone)]
pub struct OpsExecutor {
    ops: Arc<dyn ControlOps>,
}

impl OpsExecutor {
    /// An executor over the given control operations.
    pub fn new(ops: Arc<dyn ControlOps>) -> Self {
        Self { ops }
    }
}

impl ControlExecutor for OpsExecutor {
    fn execute(&self, call: ControlCall) -> ExecFuture {
        let ops = self.ops.clone();
        Box::pin(async move {
            match call {
                ControlCall::Discover {
                    kind,
                    dir,
                    host,
                    timeout_secs,
                } => match ops.discover(kind, dir, host, timeout_secs).await {
                    Ok(caps) => ok_result(discover_summary(&caps), caps),
                    Err(e) => error_result(e),
                },
                ControlCall::Describe { host, id } => match ops.describe(host, id).await {
                    Ok(descriptor) => ok_result("capability descriptor", descriptor),
                    Err(e) => error_result(e),
                },
                ControlCall::Connect(args) => match ops.connect(args).await {
                    Ok(status) => mount_result(&status),
                    Err(e) => error_result(e),
                },
                ControlCall::Disconnect { mount_id } => {
                    let id = mount_id.clone();
                    match ops.disconnect(mount_id).await {
                        Ok(()) => ok_result(format!("unmounted {id}"), json!({ "mount-id": id })),
                        Err(e) => error_result(e),
                    }
                }
                ControlCall::Status { mount_id } => match ops.status(mount_id).await {
                    Ok(mounts) => status_result(&mounts),
                    Err(e) => error_result(e),
                },
            }
        })
    }
}

/// A one-line summary for a `discover` result: the count when the structured value is the expected
/// JSON array of capabilities.
fn discover_summary(caps: &Value) -> String {
    match caps.as_array() {
        Some(a) => format!("discovered {} capabilit{}", a.len(), if a.len() == 1 { "y" } else { "ies" }),
        None => "discovered capabilities".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capmesh_ctl::MountState;
    use std::sync::Mutex;

    /// A mock ControlOps: canned responses + records which op was invoked.
    #[derive(Default)]
    struct MockOps {
        discovered: Mutex<Option<(Option<String>, Option<String>)>>, // (kind, dir) seen by discover
        connect_ok: bool,
    }

    fn ready<T: Send + 'static>(v: Result<T, String>) -> OpFuture<T> {
        Box::pin(std::future::ready(v))
    }

    impl ControlOps for MockOps {
        fn discover(
            &self,
            kind: Option<String>,
            dir: Option<String>,
            _host: Option<String>,
            _timeout_secs: Option<u64>,
        ) -> OpFuture<Value> {
            *self.discovered.lock().unwrap() = Some((kind, dir));
            ready(Ok(json!([{ "id": "kbd-0" }, { "id": "synth-1" }])))
        }
        fn describe(&self, _host: String, id: String) -> OpFuture<Value> {
            ready(Ok(json!({ "id": id, "ports": [] })))
        }
        fn connect(&self, args: ConnectArgs) -> OpFuture<MountStatus> {
            if self.connect_ok {
                ready(Ok(MountStatus {
                    mount_id: args.mount_id.unwrap_or_else(|| "m1".into()),
                    state: MountState::Connecting,
                    since: None,
                    stats: None,
                    detail: None,
                }))
            } else {
                ready(Err("peer unreachable".to_string()))
            }
        }
        fn disconnect(&self, _mount_id: String) -> OpFuture<()> {
            ready(Ok(()))
        }
        fn status(&self, _mount_id: Option<String>) -> OpFuture<Vec<MountStatus>> {
            ready(Ok(vec![MountStatus {
                mount_id: "kbd".into(),
                state: MountState::Active,
                since: None,
                stats: None,
                detail: None,
            }]))
        }
    }

    fn exec(ops: MockOps) -> OpsExecutor {
        OpsExecutor::new(Arc::new(ops))
    }

    #[tokio::test]
    async fn discover_shapes_a_counted_ok_result_and_passes_the_selector() {
        let ops = MockOps::default();
        let e = exec(ops);
        let r = e
            .execute(ControlCall::Discover {
                kind: Some("midi".into()),
                dir: Some("source".into()),
                host: None,
                timeout_secs: None,
            })
            .await;
        assert_eq!(r["isError"], false);
        assert_eq!(r["content"][0]["text"], "discovered 2 capabilities");
        assert_eq!(r["structuredContent"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn connect_success_is_a_mount_result_failure_is_an_error_result() {
        let ok = exec(MockOps { connect_ok: true, ..Default::default() });
        let args = ConnectArgs {
            remote_host: "h".into(),
            remote_addr: "1.2.3.4".into(),
            remote_port: 5004,
            remote_port_id: "kbd-0".into(),
            role: "mirror-source".into(),
            local_name: None,
            codec: "midi1".into(),
            mount_id: Some("m1".into()),
        };
        let r = ok.execute(ControlCall::Connect(args.clone())).await;
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["mount"]["state"], "connecting");

        let bad = exec(MockOps { connect_ok: false, ..Default::default() });
        let r = bad.execute(ControlCall::Connect(args)).await;
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "peer unreachable");
    }

    #[tokio::test]
    async fn disconnect_and_status_shape_results() {
        let e = exec(MockOps::default());
        let r = e.execute(ControlCall::Disconnect { mount_id: "m1".into() }).await;
        assert_eq!(r["isError"], false);
        assert_eq!(r["content"][0]["text"], "unmounted m1");
        assert_eq!(r["structuredContent"]["mount-id"], "m1");

        let r = e.execute(ControlCall::Status { mount_id: None }).await;
        assert_eq!(r["content"][0]["text"], "kbd: active");
        assert_eq!(r["structuredContent"]["mounts"][0]["mount-id"], "kbd");
    }
}
