//! MCP `tools/call` **result shaping** for capmesh's control tools (DESIGN §7.1). The control
//! server's executor ([`control_http::ControlExecutor`](crate::control_http)) turns each control
//! operation's outcome into one of these result values — a human-readable `content` text block plus
//! machine-readable `structuredContent` — which [`control_http`](crate::control_http) wraps as the
//! JSON-RPC response.
//!
//! A *tool-level* failure (e.g. an unreachable data-plane socket) is an [`error_result`]
//! (`isError: true`), NOT a JSON-RPC transport error: the tool ran and reported a problem, which MCP
//! surfaces to the agent as a failed call. Pure and unit-tested; the binary supplies the op calls.

use capmesh_ctl::MountStatus;
use serde_json::{json, Value};

/// A successful `tools/call` result: a human-readable `content` text summary plus machine-readable
/// `structuredContent`.
pub fn ok_result(summary: impl Into<String>, structured: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": summary.into() }],
        "structuredContent": structured,
        "isError": false
    })
}

/// A tool-level error result (`isError: true`) — the tool ran but failed. Distinct from a JSON-RPC
/// transport error; MCP surfaces this to the agent as a failed tool call.
pub fn error_result(message: impl Into<String>) -> Value {
    json!({
        "content": [{ "type": "text", "text": message.into() }],
        "isError": true
    })
}

/// Result for `status`: one line per mount (`<id>: <state>`) plus the mounts as structured content.
pub fn status_result(mounts: &[MountStatus]) -> Value {
    let summary = if mounts.is_empty() {
        "no mounts".to_string()
    } else {
        mounts
            .iter()
            .map(|m| format!("{}: {}", m.mount_id, state_str(m)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    ok_result(summary, json!({ "mounts": mounts }))
}

/// Result for `connect` / `disconnect` reporting a single mount's resulting state.
pub fn mount_result(status: &MountStatus) -> Value {
    ok_result(
        format!("mount {}: {}", status.mount_id, state_str(status)),
        json!({ "mount": status }),
    )
}

/// The mount's state rendered as its wire string (matching the serde representation), for the
/// human summary line.
fn state_str(status: &MountStatus) -> String {
    serde_json::to_value(status.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use capmesh_ctl::MountState;

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
    fn ok_result_has_content_and_structured_and_is_not_error() {
        let r = ok_result("hi", json!({ "n": 1 }));
        assert_eq!(r["content"][0]["type"], "text");
        assert_eq!(r["content"][0]["text"], "hi");
        assert_eq!(r["structuredContent"]["n"], 1);
        assert_eq!(r["isError"], false);
    }

    #[test]
    fn error_result_is_marked_is_error() {
        let r = error_result("nope");
        assert_eq!(r["content"][0]["text"], "nope");
        assert_eq!(r["isError"], true);
        // A tool-level error carries no structuredContent.
        assert!(r.get("structuredContent").is_none());
    }

    #[test]
    fn status_result_empty_and_populated() {
        let empty = status_result(&[]);
        assert_eq!(empty["content"][0]["text"], "no mounts");
        assert_eq!(empty["structuredContent"]["mounts"].as_array().unwrap().len(), 0);

        let r = status_result(&[
            status("kbd", MountState::Active),
            status("synth", MountState::Failed),
        ]);
        // Summary is one line per mount, using the wire state string.
        assert_eq!(r["content"][0]["text"], "kbd: active\nsynth: failed");
        let mounts = r["structuredContent"]["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0]["mount-id"], "kbd");
        assert_eq!(mounts[0]["state"], "active");
    }

    #[test]
    fn mount_result_reports_one_mounts_state() {
        let r = mount_result(&status("kbd", MountState::Connecting));
        assert_eq!(r["content"][0]["text"], "mount kbd: connecting");
        assert_eq!(r["structuredContent"]["mount"]["state"], "connecting");
        assert_eq!(r["isError"], false);
    }
}
