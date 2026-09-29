//! `nmidid` — the MIDI data-plane daemon.
//!
//! `nmidid` owns the RTP-MIDI data plane behind the `capmesh-ctl` control
//! socket (see `capmeshd/docs/CONTROL-PROTOCOL.md`). capmeshd is the control
//! plane; it drives this daemon over a local Unix domain socket carrying
//! newline-delimited JSON-RPC 2.0.
//!
//! # What it does
//!
//! - **Control socket** ([`server`]): NDJSON / JSON-RPC 2.0 framing, the
//!   `hello`-first handshake gate, and method dispatch. The socket is
//!   local-trust: a file-permission gate (mode `0o660` + owning group) and then
//!   a peer-credential gate ([`server::PeerPolicy`], CONTROL-PROTOCOL §1.1).
//! - **Methods**: `hello` / `list-ports` / `describe-port` and
//!   `mount` / `unmount` / `mount-status`, plus the unsolicited §5 notifications
//!   `mount-state` and `port-added` / `port-removed` (hot-plug, [`hotplug`]).
//! - **Mounts** ([`mounts`], [`pump`]): materialize the local endpoint and drive
//!   the AppleMIDI handshake + RTP-MIDI pump — `connecting → active`, with
//!   clock-sync, dead-peer detection, and graceful teardown. All three roles are
//!   supported: `mirror-source` creates a local virtual source a remote keyboard
//!   plays; `mirror-sink` creates a local virtual sink whose MIDI is forwarded
//!   out to a remote sink; `link` binds an existing *real* local port
//!   (`local.port-id`) instead of a virtual one — a real source forwards out, a
//!   real sink receives in, with the direction resolved from the daemon's own
//!   `list-ports` descriptor for that id.
//!
//! # Seams
//!
//! The data path is expressed through traits so the daemon is testable without
//! hardware or a network: [`ports::PortProvider`] (MIDI enumeration),
//! [`mounts::Mounter`] / [`mounts::MidiSink`] (virtual endpoints), and
//! [`mounts::Connector`] (the pump). Production impls wrap `midir` and real
//! sockets; tests substitute in-memory fakes.

pub mod config;
pub mod hotplug;
pub mod mounts;
pub mod ports;
pub mod protocol;
pub mod pump;
pub mod server;
