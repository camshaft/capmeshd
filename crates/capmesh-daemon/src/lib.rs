//! The capmesh control-plane core (DESIGN §6) — the host-independent logic capmeshd runs:
//! the desired-mount [`reconcile`]r that converges a daemon's actual mounts onto a desired
//! set (self-healing, event-driven off `capmesh-ctl` notifications) and the §4 format
//! [`negotiate`]r that picks the wire format for a mount. Both are pure of any concrete
//! transport beyond the `capmesh-ctl` client; the capmeshd binary supplies the CLI, config,
//! and discovery wiring around them.

pub mod automount;
pub mod control_http;
pub mod control_mcp;
pub mod control_result;
pub mod mcp_routes;
pub mod negotiate;
pub mod reconcile;
