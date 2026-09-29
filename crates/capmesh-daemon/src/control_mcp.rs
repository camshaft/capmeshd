//! capmeshd's **own** control tools exposed as a small embedded MCP server (DESIGN §7.1).
//!
//! This is the §7.1 half of capmeshd-as-MCP: the control plane advertises itself on the mesh as a
//! `cap=mcp` server whose tools are capmesh's control operations — `discover` / `describe` /
//! `connect` / `disconnect` / `status` — so an agent drives the mesh through the same federated
//! `/mcp` surface it uses for everything else. This is distinct from the route-registry / gateway
//! half (§7.2, [`crate::mcp_routes`]): here capmesh is an *upstream*, not the federator.
//!
//! Like the gateway's inbound server ([`mcp-gateway`'s `server::dispatch`]), this module is the
//! **transport-agnostic dispatch core**: it maps one JSON-RPC message to either a complete reply
//! (initialize / `tools/list` / ping / errors) or a typed [`ControlCall`] the daemon must execute
//! against its control plane, wrapping the result as the response to `request_id`. The async
//! execution and the Streamable-HTTP transport are the daemon's job, so the protocol dispatch is
//! pure and fully unit-tested here.

use serde_json::{Value, json};

/// The MCP protocol revision capmesh's control server speaks (§7), matching the gateway.
pub const CONTROL_PROTOCOL_VERSION: &str = "2026-07-28";

/// The five control tools capmesh advertises (their `tools/list` names). The gateway federates them
/// namespaced as `capmesh__discover`, etc.
pub const TOOL_DISCOVER: &str = "discover";
pub const TOOL_DESCRIBE: &str = "describe";
pub const TOOL_CONNECT: &str = "connect";
pub const TOOL_DISCONNECT: &str = "disconnect";
pub const TOOL_STATUS: &str = "status";

/// A parsed control tool-call the daemon executes against its control plane. Mirrors the capmeshd
/// binary's own subcommands so an MCP call and a CLI invocation drive the identical control path.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlCall {
    /// Browse the mesh for capabilities matching an optional selector (§7 discover).
    Discover {
        kind: Option<String>,
        dir: Option<String>,
        host: Option<String>,
        timeout_secs: Option<u64>,
    },
    /// Fetch one discovered capability's full typed descriptor.
    Describe { host: String, id: String },
    /// Mount a remote capability as a local virtual device (§6.1/§7 connect).
    Connect(ConnectArgs),
    /// Tear down a mount by id.
    Disconnect { mount_id: String },
    /// Report mount status — all mounts, or one by id.
    Status { mount_id: Option<String> },
}

/// The `connect` tool's arguments — the wire form of the binary's `MountArgs` (§3.1). `remote_addr`
/// stays a string here; the daemon parses/validates it into an `IpAddr` when it builds the
/// `MountSpec`, so this dispatch layer carries no `std::net` coupling.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectArgs {
    pub remote_host: String,
    pub remote_addr: String,
    pub remote_port: u16,
    pub remote_port_id: String,
    /// `mirror-source` | `mirror-sink` | `link`; defaults to `mirror-source`.
    pub role: String,
    pub local_name: Option<String>,
    /// Wire codec; defaults to `midi1`.
    pub codec: String,
    /// Idempotency key; the daemon defaults it to `<remote-host>-<remote-port-id>` when absent.
    pub mount_id: Option<String>,
}

/// What the transport should do with one dispatched message.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlAction {
    /// Send this JSON-RPC response back to the agent.
    Reply(Value),
    /// Execute `call` against the control plane, then wrap its result as the response to
    /// `request_id`.
    Call { request_id: Value, call: ControlCall },
    /// A notification (no `id`) — nothing to send back.
    Ignore,
}

/// Dispatch one JSON-RPC 2.0 message from an agent against capmesh's control-tool surface.
pub fn dispatch(req: &Value) -> ControlAction {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications carry no `id` and get no reply.
    let Some(id) = req.get("id").cloned() else {
        return ControlAction::Ignore;
    };
    match method {
        "initialize" => ControlAction::Reply(ok(id, initialize_result())),
        "tools/list" => ControlAction::Reply(ok(id, json!({ "tools": tool_catalog() }))),
        "ping" => ControlAction::Reply(ok(id, json!({}))),
        "tools/call" => dispatch_tools_call(id, req),
        other => ControlAction::Reply(err(id, -32601, &format!("method not found: {other}"))),
    }
}

fn dispatch_tools_call(id: Value, req: &Value) -> ControlAction {
    let params = req.get("params");
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let empty = json!({});
    let args = params.and_then(|p| p.get("arguments")).unwrap_or(&empty);
    match parse_call(name, args) {
        Ok(call) => ControlAction::Call {
            request_id: id,
            call,
        },
        Err(ParseError::UnknownTool) => {
            ControlAction::Reply(err(id, -32602, &format!("unknown tool '{name}'")))
        }
        Err(ParseError::BadArgs(msg)) => ControlAction::Reply(err(id, -32602, &msg)),
    }
}

/// Why a `tools/call` could not be turned into a [`ControlCall`].
enum ParseError {
    UnknownTool,
    BadArgs(String),
}

fn parse_call(name: &str, args: &Value) -> Result<ControlCall, ParseError> {
    match name {
        TOOL_DISCOVER => Ok(ControlCall::Discover {
            kind: opt_str(args, "kind")?,
            dir: opt_str(args, "dir")?,
            host: opt_str(args, "host")?,
            timeout_secs: opt_u64(args, "timeout_secs")?,
        }),
        TOOL_DESCRIBE => Ok(ControlCall::Describe {
            host: req_str(args, "host")?,
            id: req_str(args, "id")?,
        }),
        TOOL_CONNECT => Ok(ControlCall::Connect(ConnectArgs {
            remote_host: req_str(args, "remote_host")?,
            remote_addr: req_str(args, "remote_addr")?,
            remote_port: req_u16(args, "remote_port")?,
            remote_port_id: req_str(args, "remote_port_id")?,
            role: opt_str(args, "role")?.unwrap_or_else(|| "mirror-source".to_string()),
            local_name: opt_str(args, "local_name")?,
            codec: opt_str(args, "codec")?.unwrap_or_else(|| "midi1".to_string()),
            mount_id: opt_str(args, "mount_id")?,
        })),
        TOOL_DISCONNECT => Ok(ControlCall::Disconnect {
            mount_id: req_str(args, "mount_id")?,
        }),
        TOOL_STATUS => Ok(ControlCall::Status {
            mount_id: opt_str(args, "mount_id")?,
        }),
        _ => Err(ParseError::UnknownTool),
    }
}

// --- argument extractors: present-and-right-type or a precise -32602 message ---

fn req_str(args: &Value, key: &str) -> Result<String, ParseError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(bad(key, "a string")),
        None => Err(missing(key)),
    }
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, ParseError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad(key, "a string")),
    }
}

fn req_u16(args: &Value, key: &str) -> Result<u16, ParseError> {
    match args.get(key) {
        Some(v) if v.is_u64() => u16::try_from(v.as_u64().unwrap())
            .map_err(|_| bad(key, "a port in 0..=65535")),
        Some(_) => Err(bad(key, "a port in 0..=65535")),
        None => Err(missing(key)),
    }
}

fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ParseError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) if v.is_u64() => Ok(Some(v.as_u64().unwrap())),
        Some(_) => Err(bad(key, "a non-negative integer")),
    }
}

fn missing(key: &str) -> ParseError {
    ParseError::BadArgs(format!("missing required argument '{key}'"))
}

fn bad(key: &str, want: &str) -> ParseError {
    ParseError::BadArgs(format!("argument '{key}' must be {want}"))
}

/// The static `tools/list` capmesh advertises. Descriptions read as agent-facing tool docs; the
/// schemas mirror the binary's subcommand arguments.
pub fn tool_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": TOOL_DISCOVER,
            "description": "Browse the LAN mesh for capmesh capabilities, optionally filtered by kind/dir/host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "Capability kind, e.g. \"midi\"." },
                    "dir": { "type": "string", "description": "Port direction, e.g. \"source\" or \"sink\"." },
                    "host": { "type": "string", "description": "Restrict to one peer host-id." },
                    "timeout_secs": { "type": "integer", "minimum": 0, "description": "How long to browse before returning." }
                }
            }
        }),
        json!({
            "name": TOOL_DESCRIBE,
            "description": "Fetch one discovered capability's full typed descriptor (its ports and codecs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "host": { "type": "string", "description": "Peer host-id the capability is on." },
                    "id": { "type": "string", "description": "Capability id from discover." }
                },
                "required": ["host", "id"]
            }
        }),
        json!({
            "name": TOOL_CONNECT,
            "description": "Mount a remote capability as a local virtual device (the desired-mount is reconciled + self-healed).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "remote_host": { "type": "string", "description": "Remote peer host-id." },
                    "remote_addr": { "type": "string", "description": "Remote peer IP (from the mDNS record)." },
                    "remote_port": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "Remote data-plane port." },
                    "remote_port_id": { "type": "string", "description": "Remote port-id to mount." },
                    "role": { "type": "string", "enum": ["mirror-source", "mirror-sink", "link"], "default": "mirror-source" },
                    "local_name": { "type": "string", "description": "Display name for the local virtual device (mirror roles)." },
                    "codec": { "type": "string", "default": "midi1", "description": "Chosen wire-format codec." },
                    "mount_id": { "type": "string", "description": "Idempotency key; defaults to <remote-host>-<remote-port-id>." }
                },
                "required": ["remote_host", "remote_addr", "remote_port", "remote_port_id"]
            }
        }),
        json!({
            "name": TOOL_DISCONNECT,
            "description": "Tear down a mount by id (drops it from the desired-mount set).",
            "inputSchema": {
                "type": "object",
                "properties": { "mount_id": { "type": "string", "description": "The mount to tear down." } },
                "required": ["mount_id"]
            }
        }),
        json!({
            "name": TOOL_STATUS,
            "description": "Report mount status — all mounts, or one by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "mount_id": { "type": "string", "description": "Restrict to one mount id." } }
            }
        }),
    ]
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": CONTROL_PROTOCOL_VERSION,
        // The control-tool set is static, so no `listChanged`.
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "capmesh-control", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn call(id: i64, name: &str, arguments: Value) -> ControlAction {
        dispatch(&req(id, "tools/call", json!({ "name": name, "arguments": arguments })))
    }

    #[test]
    fn initialize_advertises_the_control_proto() {
        let ControlAction::Reply(v) = dispatch(&req(1, "initialize", json!({}))) else {
            panic!("expected reply")
        };
        assert_eq!(v["result"]["protocolVersion"], "2026-07-28");
        assert_eq!(v["result"]["serverInfo"]["name"], "capmesh-control");
    }

    #[test]
    fn tools_list_advertises_all_five_control_tools() {
        let ControlAction::Reply(v) = dispatch(&req(2, "tools/list", json!({}))) else {
            panic!("expected reply")
        };
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["discover", "describe", "connect", "disconnect", "status"]
        );
        // Every advertised tool must actually be routable — no drift between catalog and parser.
        for name in names {
            let out = call(9, name, minimal_args(name));
            assert!(
                matches!(out, ControlAction::Call { .. }),
                "catalog tool '{name}' did not route to a ControlCall"
            );
        }
    }

    /// The minimal valid argument object for each catalog tool.
    fn minimal_args(name: &str) -> Value {
        match name {
            "discover" | "status" => json!({}),
            "describe" => json!({ "host": "h", "id": "cap-0" }),
            "connect" => json!({
                "remote_host": "h", "remote_addr": "192.168.1.23",
                "remote_port": 5004, "remote_port_id": "kbd-0"
            }),
            "disconnect" => json!({ "mount_id": "m1" }),
            other => panic!("no minimal args for {other}"),
        }
    }

    #[test]
    fn discover_parses_its_optional_selector() {
        let out = call(3, "discover", json!({ "kind": "midi", "dir": "source", "timeout_secs": 3 }));
        assert_eq!(
            out,
            ControlAction::Call {
                request_id: json!(3),
                call: ControlCall::Discover {
                    kind: Some("midi".into()),
                    dir: Some("source".into()),
                    host: None,
                    timeout_secs: Some(3),
                },
            }
        );
    }

    #[test]
    fn connect_applies_role_and_codec_defaults() {
        let out = call(4, "connect", json!({
            "remote_host": "studio", "remote_addr": "192.168.1.23",
            "remote_port": 5004, "remote_port_id": "kbd-0"
        }));
        let ControlAction::Call { call: ControlCall::Connect(a), .. } = out else {
            panic!("expected a connect call")
        };
        assert_eq!(a.role, "mirror-source");
        assert_eq!(a.codec, "midi1");
        assert_eq!(a.remote_port, 5004);
        assert_eq!(a.mount_id, None);
    }

    #[test]
    fn a_missing_required_argument_is_a_precise_error_reply() {
        let ControlAction::Reply(v) = call(5, "describe", json!({ "host": "h" })) else {
            panic!("expected error reply")
        };
        assert_eq!(v["error"]["code"], -32602);
        assert!(v["error"]["message"].as_str().unwrap().contains("'id'"));
    }

    #[test]
    fn a_wrong_typed_argument_is_rejected() {
        // remote_port as a string, not an integer.
        let ControlAction::Reply(v) = call(6, "connect", json!({
            "remote_host": "h", "remote_addr": "1.2.3.4",
            "remote_port": "5004", "remote_port_id": "p"
        })) else {
            panic!("expected error reply")
        };
        assert_eq!(v["error"]["code"], -32602);
        assert!(v["error"]["message"].as_str().unwrap().contains("remote_port"));

        // A port past u16 is rejected too.
        let ControlAction::Reply(v) = call(7, "connect", json!({
            "remote_host": "h", "remote_addr": "1.2.3.4",
            "remote_port": 70000, "remote_port_id": "p"
        })) else {
            panic!("expected error reply")
        };
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn unknown_tool_and_unknown_method_and_notifications() {
        let ControlAction::Reply(v) = call(8, "frobnicate", json!({})) else {
            panic!("expected reply")
        };
        assert_eq!(v["error"]["code"], -32602);

        let ControlAction::Reply(v) = dispatch(&req(8, "no_such_method", json!({}))) else {
            panic!("expected reply")
        };
        assert_eq!(v["error"]["code"], -32601);

        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(dispatch(&note), ControlAction::Ignore);
    }

    #[test]
    fn ping_replies_empty() {
        let ControlAction::Reply(v) = dispatch(&req(10, "ping", json!({}))) else {
            panic!("expected reply")
        };
        assert_eq!(v["result"], json!({}));
    }
}
