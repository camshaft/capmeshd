//! `surfaced` — the browser **surface** data-plane daemon (DESIGN §10.1).
//!
//! A *surface* is a durable, addressable, arbitrarily-scriptable display sink on
//! the mesh: "aim a browser at it and it becomes a screen you can push to." The
//! logical surface lives here in `surfaced` and holds all state; a browser tab is
//! an ephemeral *attachment* pointed at `/s/{id}`, so a surface persists whether
//! or not a browser is open. A surface **is a persistent inbox** — every push
//! appends a display item to a durable, ordered, bounded log, and the tab renders
//! a main view (the head of the inbox) plus a visible feed (its history).
//!
//! `surfaced` is a first-party data-plane daemon like `nmidid`: capmeshd stays
//! stateless plumbing and drives it over a control socket, while `surfaced` owns
//! the HTTP/SSE serving, the inbox store, and attachment fan-out.
//!
//! This crate implements the HTTP/SSE server ([`http`]), the durable inbox store
//! with on-disk persistence ([`inbox`]), the display-item model ([`item`]), the
//! same-origin attachment page ([`page`]), the `surface-ctl` control socket
//! ([`ctl`]) that capmeshd drives over a local Unix socket, per-surface attach
//! tokens (in [`inbox`]/[`http`]), and an embedded MCP server ([`mcp`]) so an
//! agent can post items to surfaces and list them.

pub mod config;
pub mod ctl;
pub mod http;
pub mod inbox;
pub mod item;
pub mod mcp;
pub mod page;
