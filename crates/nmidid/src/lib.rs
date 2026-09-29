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
//! - **Mirror mounts** ([`mounts`], [`pump`]): materialize a local virtual MIDI
//!   endpoint and drive the AppleMIDI handshake + RTP-MIDI pump — `connecting →
//!   active`, with clock-sync, dead-peer detection, and graceful teardown.
//!   `mirror-source` creates a local virtual source a remote keyboard plays;
//!   `mirror-sink` creates a local virtual sink whose MIDI is forwarded out to a
//!   remote sink. `link` (bind an existing real local port) is declined
//!   (`role-unsupported`) pending later work.
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
