//! `nmidid` — the MIDI data-plane daemon.
//!
//! `nmidid` owns the RTP-MIDI data plane behind the `capmesh-ctl` control
//! socket (see `capmeshd/docs/CONTROL-PROTOCOL.md`). capmeshd is the control
//! plane; it drives this daemon over a local Unix domain socket carrying
//! newline-delimited JSON-RPC 2.0.
//!
//! This crate currently implements the control-socket framing and the
//! `hello` / `list-ports` / `describe-port` methods (M0a increment 1). The
//! `mount` / `unmount` / `mount-status` methods and the hot-plug notifications
//! land in later increments.

pub mod ports;
pub mod protocol;
pub mod server;
