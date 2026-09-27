//! `nmidid` — the MIDI data-plane daemon.
//!
//! `nmidid` owns the RTP-MIDI data plane behind the `capmesh-ctl` control
//! socket (see `capmeshd/docs/CONTROL-PROTOCOL.md`). capmeshd is the control
//! plane; it drives this daemon over a local Unix domain socket carrying
//! newline-delimited JSON-RPC 2.0.
//!
//! This crate implements the control-socket framing, the
//! `hello` / `list-ports` / `describe-port` methods, and `mount` / `unmount` /
//! `mount-status` for the `mirror-source` role (local virtual endpoint). The
//! remote AppleMIDI handshake + RTP-MIDI pump (mount → `active`) and the
//! hot-plug notifications land in the following increments.

pub mod mounts;
pub mod ports;
pub mod protocol;
pub mod pump;
pub mod server;
