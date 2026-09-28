//! The RTP-MIDI data-path pump for a `mirror-source` mount.
//!
//! [`RtpConnector`] spawns a task that acts as the AppleMIDI *inviter* and makes
//! a remote keyboard play a local instrument. Its lifecycle:
//!
//! 1. **Handshake.** Invite the remote's control channel (`IN`) and await `OK`,
//!    retrying on timeout; an explicit `NO` fails the mount immediately. Only
//!    once the control channel is accepted do we invite the data channel (the
//!    order AppleMIDI expects). The mount is `connecting` until this completes,
//!    then `active`.
//! 2. **Forwarding.** Each received RTP-MIDI message is forwarded into the local
//!    virtual sink, accumulating `bytes-in`. Packets are gated by RTP sequence
//!    number ([`SeqGate`]) so a reordered or duplicated datagram never replays
//!    already-played MIDI. (MIDI running status is expanded during parsing in
//!    `nmidi-core`.)
//! 3. **Clock-sync.** As the session initiator we *drive* clock-sync: we send our
//!    own `CK0` periodically and complete the exchange on the peer's `CK1` with
//!    `CK2`, and we also answer a peer-initiated `CK0` with `CK1`. This is the
//!    keep-alive a live session relies on. (Timestamp-accurate *scheduling* —
//!    offset-based playout — is a deliberate non-goal; we forward as packets
//!    arrive, which is correct for live play.)
//! 4. **Liveness.** If the remote goes silent past the session timeout (no data
//!    and no clock-sync response), the mount fails so capmeshd re-reconciles.
//! 5. **Teardown.** On unmount/daemon-shutdown (cancel) or a remote `End`, we send
//!    `End` (BY) to the peer and drop the sink (releasing the virtual port),
//!    transitioning the mount `torn-down`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmidi_core::network::NetworkSockets;
use nmidi_core::util::{generate_ssrc, generate_token, get_hostname};
use nmidi_core::{APPLEMIDI_VERSION, AppleMidiPacket, RtpPacket};
use serde_json::Value;
use tokio::sync::{Notify, broadcast};
use tracing::{debug, info, warn};

use crate::mounts::{Connector, MidiSink, now_rfc3339, transition};
use crate::protocol::{MountState, MountStatus, RemoteEndpoint};

/// How long to wait for `InvitationAccepted` before retrying, and how many times.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_ATTEMPTS: u32 = 3;

/// How often the session initiator drives a clock-sync exchange (CK0). Real
/// AppleMIDI peers expect the inviter to keep clock-sync alive; without it an
/// idle or strict source may time the session out.
const CK_SYNC_INTERVAL: Duration = Duration::from_secs(10);

/// How long the remote may be silent (no data and no clock-sync response to our
/// periodic CK0) before the session is declared dead — three missed CK cycles.
/// A dead session fails the mount so capmeshd re-reconciles, rather than lingering
/// "active" forever behind a source that vanished.
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Drops stale or duplicate RTP packets by sequence number, so a reordered or
/// retransmitted datagram never replays MIDI that has already been played (a
/// replayed note-on/off would double-trigger the instrument).
#[derive(Default)]
struct SeqGate {
    last: Option<u16>,
}

impl SeqGate {
    /// Admit `seq` if it is the first packet of the session, or newer than the
    /// last admitted one under 16-bit wraparound (RFC 1982 serial arithmetic:
    /// the shorter direction around the ring is "newer").
    fn admit(&mut self, seq: u16) -> bool {
        let newer = self
            .last
            .is_none_or(|l| seq != l && seq.wrapping_sub(l) < 0x8000);
        if newer {
            self.last = Some(seq);
        }
        newer
    }
}

/// Tracks whether the remote is still responsive. Any packet received from the
/// peer (data or control) is activity; if nothing arrives within `timeout` — our
/// periodic CK0 having gone unanswered — the session is stale and the mount
/// should fail rather than linger "active" behind a vanished source.
struct SessionLiveness {
    last_activity: Instant,
    timeout: Duration,
}

impl SessionLiveness {
    fn new(timeout: Duration) -> Self {
        SessionLiveness {
            last_activity: Instant::now(),
            timeout,
        }
    }

    fn record_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    fn is_stale(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) > self.timeout
    }
}

/// Production connector: spawns the AppleMIDI handshake + RTP-MIDI pump.
pub struct RtpConnector;

impl Connector for RtpConnector {
    fn start(
        &self,
        remote: RemoteEndpoint,
        sink: Box<dyn MidiSink>,
        status: Arc<Mutex<MountStatus>>,
        notifier: broadcast::Sender<Value>,
        cancel: Arc<Notify>,
    ) {
        tokio::spawn(async move {
            if let Err(e) =
                run_pump(remote, sink, Arc::clone(&status), notifier.clone(), cancel).await
            {
                warn!("mount pump failed: {e}");
                transition(&status, &notifier, MountState::Failed, Some(format!("{e}")));
            }
        });
    }
}

/// Connect to the remote source and pump RTP-MIDI into `sink` until the remote
/// ends the session or `cancel` fires (unmount). On either graceful stop we send
/// `End` (BY) to the remote so it does not hold a stale session until timeout.
async fn run_pump(
    remote: RemoteEndpoint,
    sink: Box<dyn MidiSink>,
    status: Arc<Mutex<MountStatus>>,
    notifier: broadcast::Sender<Value>,
    cancel: Arc<Notify>,
) -> anyhow::Result<()> {
    // Connect by the IP (never a .local/.lan name) — capmeshd fills remote.addr
    // from the mDNS record.
    let control_addr: SocketAddr = format!("{}:{}", remote.addr, remote.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("bad remote addr {}:{}: {e}", remote.addr, remote.port))?;
    let data_addr = SocketAddr::new(control_addr.ip(), control_addr.port().wrapping_add(1));

    let sockets = NetworkSockets::bind_consecutive("0.0.0.0").await?;
    let ssrc = generate_ssrc();
    let token = generate_token();
    let name = get_hostname();

    let invitation = AppleMidiPacket::Invitation {
        version: APPLEMIDI_VERSION,
        token,
        ssrc,
        name: name.clone(),
    };
    // AppleMIDI orders the handshake: invite the control channel first; the data
    // channel is invited only after the control invitation is accepted (below).
    // A strict peer may ignore a data invitation that arrives before the control
    // session exists.
    sockets.send_control(&invitation, &control_addr).await?;
    debug!("sent AppleMIDI control invitation to {control_addr}");

    // Await InvitationAccepted (or a graceful cancel), resending on timeout.
    let mut attempts = 0;
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => { cancelled = true; break; }
            res = tokio::time::timeout(HANDSHAKE_TIMEOUT, sockets.recv_control()) => match res {
                Ok(Ok((AppleMidiPacket::InvitationAccepted { .. }, _))) => break,
                Ok(Ok((AppleMidiPacket::InvitationRejected { .. }, _))) => {
                    // The remote explicitly declined; retrying would only spin to
                    // timeout, so fail fast with a clear cause.
                    anyhow::bail!("remote rejected the invitation");
                }
                Ok(Ok(_)) => continue, // other control packet before accept; keep waiting
                Ok(Err(e)) => debug!("control recv during handshake: {e}"),
                Err(_) => {
                    attempts += 1;
                    if attempts >= HANDSHAKE_ATTEMPTS {
                        anyhow::bail!("no InvitationAccepted after {HANDSHAKE_ATTEMPTS} attempts");
                    }
                    sockets.send_control(&invitation, &control_addr).await?;
                }
            }
        }
    }

    if !cancelled {
        // Control channel accepted — now invite the data channel (correct
        // AppleMIDI order). Best-effort: we don't block on the data OK; MIDI
        // arrives once the peer establishes its side.
        let _ = sockets.send_control_on_data(&invitation, &data_addr).await;
        debug!("sent AppleMIDI data invitation to {data_addr}");

        info!(
            "mount active: mirroring {} into local virtual port",
            control_addr
        );
        transition(&status, &notifier, MountState::Active, None);

        let started = Instant::now();
        let mut gate = SeqGate::default();
        // Timestamps are in 100µs units of our own clock.
        let now_ts = |started: &Instant| (started.elapsed().as_micros() / 100) as u64;
        // As the session initiator, drive clock-sync periodically. The first
        // tick fires immediately, sending the initial CK0 as soon as we go active.
        let mut ck_timer = tokio::time::interval(CK_SYNC_INTERVAL);
        let mut live = SessionLiveness::new(SESSION_TIMEOUT);
        loop {
            tokio::select! {
                biased;
                _ = cancel.notified() => {
                    info!("mount unmounted; tearing down session");
                    break;
                }
                _ = ck_timer.tick() => {
                    // If the remote has gone silent through our clock-sync cycles,
                    // treat the session as dead so the mount fails and re-reconciles.
                    if live.is_stale(Instant::now()) {
                        anyhow::bail!("remote unresponsive for {SESSION_TIMEOUT:?}");
                    }
                    let ck0 = AppleMidiPacket::Synchronization {
                        ssrc,
                        count: 0,
                        timestamp1: now_ts(&started),
                        timestamp2: 0,
                        timestamp3: 0,
                    };
                    let _ = sockets.send_control(&ck0, &control_addr).await;
                }
                data = sockets.recv_data() => match data {
                    Ok((packet, _)) => {
                        live.record_activity();
                        if gate.admit(packet.header.sequence) {
                            forward_rtp(&packet, sink.as_ref(), &status);
                        } else {
                            debug!("dropping stale/duplicate RTP packet seq {}", packet.header.sequence);
                        }
                    }
                    Err(e) => debug!("data recv error: {e}"),
                },
                control = sockets.recv_control() => match control {
                    Ok((AppleMidiPacket::End { .. }, _)) => {
                        info!("remote ended the session");
                        break;
                    }
                    Ok((AppleMidiPacket::Synchronization { count, timestamp1, timestamp2, .. }, from)) => {
                        live.record_activity();
                        // AppleMIDI clock-sync (100µs units). Answer a peer's CK0
                        // with CK1, and complete our own exchange by answering the
                        // peer's CK1 (a response to our CK0) with CK2.
                        match count {
                            0 => {
                                let reply = AppleMidiPacket::Synchronization {
                                    ssrc,
                                    count: 1,
                                    timestamp1,
                                    timestamp2: now_ts(&started),
                                    timestamp3: 0,
                                };
                                let _ = sockets.send_control(&reply, &from).await;
                            }
                            1 => {
                                let reply = AppleMidiPacket::Synchronization {
                                    ssrc,
                                    count: 2,
                                    timestamp1,
                                    timestamp2,
                                    timestamp3: now_ts(&started),
                                };
                                let _ = sockets.send_control(&reply, &from).await;
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => live.record_activity(),
                    Err(e) => debug!("control recv error: {e}"),
                },
            }
        }
    }

    let end = AppleMidiPacket::End {
        version: APPLEMIDI_VERSION,
        token,
        ssrc,
    };
    let _ = sockets.send_control(&end, &control_addr).await;
    transition(&status, &notifier, MountState::TornDown, None);
    Ok(())
}

/// Forward every MIDI message in an RTP-MIDI packet into the local sink, and
/// update the mount's stats/state. Pure w.r.t. the network, so it is unit-tested.
pub(crate) fn forward_rtp(
    packet: &RtpPacket,
    sink: &dyn MidiSink,
    status: &Arc<Mutex<MountStatus>>,
) {
    let mut forwarded = 0u64;
    for cmd in &packet.commands {
        if cmd.data.is_empty() {
            continue;
        }
        match sink.send(&cmd.data) {
            Ok(()) => forwarded += cmd.data.len() as u64,
            Err(e) => warn!("dropping MIDI message, sink send failed: {e}"),
        }
    }
    if forwarded > 0 {
        let mut s = status.lock().unwrap();
        s.stats.bytes_in += forwarded;
        s.stats.last_event = Some(now_rfc3339());
        s.state = MountState::Active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MountStats, MountStatus};

    /// A sink whose recorded messages are observable via a shared handle, so a
    /// test can inspect what `run_pump` (which owns the sink) forwarded.
    struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);
    impl RecordingSink {
        fn new() -> Self {
            RecordingSink(Arc::new(Mutex::new(Vec::new())))
        }
        fn handle(&self) -> Arc<Mutex<Vec<Vec<u8>>>> {
            Arc::clone(&self.0)
        }
    }
    impl MidiSink for RecordingSink {
        fn send(&self, message: &[u8]) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(message.to_vec());
            Ok(())
        }
    }

    fn connecting_status() -> Arc<Mutex<MountStatus>> {
        Arc::new(Mutex::new(MountStatus {
            mount_id: "m1".to_string(),
            state: MountState::Connecting,
            since: now_rfc3339(),
            stats: MountStats::default(),
            detail: None,
        }))
    }

    #[test]
    fn forward_rtp_pushes_messages_and_marks_active() {
        let sink = RecordingSink::new();
        let status = connecting_status();

        let mut pkt = RtpPacket::new(1, 1, 0);
        pkt.add_command(0, vec![0x90, 0x40, 0x7f]); // note on
        pkt.add_command(10, vec![0x80, 0x40, 0x00]); // note off
        forward_rtp(&pkt, &sink, &status);

        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![vec![0x90, 0x40, 0x7f], vec![0x80, 0x40, 0x00]]
        );
        let s = status.lock().unwrap();
        assert_eq!(s.state, MountState::Active);
        assert_eq!(s.stats.bytes_in, 6);
        assert!(s.stats.last_event.is_some());
    }

    #[test]
    fn seq_gate_admits_first_and_newer_drops_stale_and_dupes() {
        let mut g = SeqGate::default();
        assert!(g.admit(100)); // first packet always admitted
        assert!(g.admit(101)); // strictly newer
        assert!(!g.admit(101)); // duplicate dropped
        assert!(!g.admit(50)); // stale (out-of-order older) dropped
        assert!(g.admit(102)); // resume forward progress
    }

    #[test]
    fn session_liveness_flags_a_silent_peer_and_resets_on_activity() {
        let start = Instant::now();
        let mut live = SessionLiveness {
            last_activity: start,
            timeout: Duration::from_secs(30),
        };
        assert!(!live.is_stale(start + Duration::from_secs(29)));
        assert!(live.is_stale(start + Duration::from_secs(31)));
        // Fresh activity resets the staleness clock.
        live.last_activity = start + Duration::from_secs(31);
        assert!(!live.is_stale(start + Duration::from_secs(40)));
        assert!(live.is_stale(start + Duration::from_secs(62)));
    }

    #[test]
    fn seq_gate_handles_16bit_wraparound() {
        let mut g = SeqGate::default();
        assert!(g.admit(65535));
        assert!(g.admit(0)); // 0 is one past 65535 → newer across the wrap
        assert!(g.admit(1));
        assert!(!g.admit(65535)); // now behind by 2 → stale
    }

    #[test]
    fn forward_rtp_empty_packet_leaves_state_untouched() {
        let sink = RecordingSink::new();
        let status = connecting_status();
        let pkt = RtpPacket::new(1, 1, 0); // no commands
        forward_rtp(&pkt, &sink, &status);
        assert!(sink.0.lock().unwrap().is_empty());
        let s = status.lock().unwrap();
        assert_eq!(s.state, MountState::Connecting);
        assert_eq!(s.stats.bytes_in, 0);
    }

    /// End-to-end active-path test: a fake remote AppleMIDI peer accepts the
    /// invitation and sends an RTP-MIDI note; the real `run_pump` must complete
    /// the handshake, forward the note into the sink, and drive the mount to
    /// `active` with `bytes-in` — all without any real MIDI hardware.
    #[tokio::test]
    async fn pump_completes_handshake_and_forwards_rtp() {
        use tokio::net::UdpSocket;

        let fake_ctl = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_ctl_port = fake_ctl.local_addr().unwrap().port();

        // Fake peer: accept the invitation, then send one RTP note to the
        // inviter's data port (its control port + 1, per bind_consecutive).
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = fake_ctl.recv_from(&mut buf).await.unwrap();
            if let Ok(AppleMidiPacket::Invitation { token, ssrc, .. }) =
                AppleMidiPacket::parse(&buf[..n])
            {
                let accept = AppleMidiPacket::InvitationAccepted {
                    version: APPLEMIDI_VERSION,
                    token,
                    ssrc,
                    name: "fake-peer".to_string(),
                };
                fake_ctl.send_to(&accept.to_bytes(), from).await.unwrap();

                let inviter_data = SocketAddr::new(from.ip(), from.port() + 1);
                let mut rtp = RtpPacket::new(0xABCD, 1, 0);
                rtp.add_command(0, vec![0x90, 0x40, 0x7f]); // note on
                fake_data
                    .send_to(&rtp.to_bytes(), inviter_data)
                    .await
                    .unwrap();
            }
        });

        let sink = RecordingSink::new();
        let recorded = sink.handle();
        let status = connecting_status();
        let (notifier, _rx) = broadcast::channel(8);
        let remote = RemoteEndpoint {
            host: None,
            addr: "127.0.0.1".to_string(),
            port: fake_ctl_port,
            port_id: "source-0".to_string(),
        };
        let pump = tokio::spawn(run_pump(
            remote,
            Box::new(sink),
            Arc::clone(&status),
            notifier,
            Arc::new(Notify::new()),
        ));

        // Wait for the note to be forwarded (bounded so a failure can't hang).
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !recorded.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(got.is_ok(), "pump did not forward RTP within timeout");

        assert_eq!(*recorded.lock().unwrap(), vec![vec![0x90, 0x40, 0x7f]]);
        let s = status.lock().unwrap();
        assert_eq!(s.state, MountState::Active);
        assert!(s.stats.bytes_in >= 3);
        drop(s);
        pump.abort();
    }

    /// The pump answers the remote's clock-sync probe: a `CK0` must be met with
    /// a `CK1` that echoes `timestamp1` — the keep-alive a live AppleMIDI
    /// session relies on to stay up.
    #[tokio::test]
    async fn pump_answers_clock_sync_ck0_with_ck1() {
        use tokio::net::UdpSocket;

        let fake_ctl = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_ctl_port = fake_ctl.local_addr().unwrap().port();

        let ck1 = Arc::new(Mutex::new(None::<AppleMidiPacket>));
        let ck1_w = Arc::clone(&ck1);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            // Accept the invitation.
            let (n, from) = fake_ctl.recv_from(&mut buf).await.unwrap();
            let AppleMidiPacket::Invitation { token, ssrc, .. } =
                AppleMidiPacket::parse(&buf[..n]).unwrap()
            else {
                return;
            };
            let accept = AppleMidiPacket::InvitationAccepted {
                version: APPLEMIDI_VERSION,
                token,
                ssrc,
                name: "fake-peer".to_string(),
            };
            fake_ctl.send_to(&accept.to_bytes(), from).await.unwrap();

            // Probe with CK0 and capture the CK1 reply.
            let ck0 = AppleMidiPacket::Synchronization {
                ssrc,
                count: 0,
                timestamp1: 0xDEAD_BEEF,
                timestamp2: 0,
                timestamp3: 0,
            };
            fake_ctl.send_to(&ck0.to_bytes(), from).await.unwrap();
            // The pump also drives its own CK0 (count 0) as initiator; wait for
            // the CK1 (count 1) that answers *our* probe.
            loop {
                let (n, _) = fake_ctl.recv_from(&mut buf).await.unwrap();
                if let Ok(p @ AppleMidiPacket::Synchronization { count: 1, .. }) =
                    AppleMidiPacket::parse(&buf[..n])
                {
                    *ck1_w.lock().unwrap() = Some(p);
                    break;
                }
            }
        });

        let status = connecting_status();
        let (notifier, _rx) = broadcast::channel(8);
        let remote = RemoteEndpoint {
            host: None,
            addr: "127.0.0.1".to_string(),
            port: fake_ctl_port,
            port_id: "source-0".to_string(),
        };
        let pump = tokio::spawn(run_pump(
            remote,
            Box::new(RecordingSink::new()),
            Arc::clone(&status),
            notifier,
            Arc::new(Notify::new()),
        ));

        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(p) = ck1.lock().unwrap().clone() {
                    break p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("received a CK1 reply within timeout");

        match reply {
            AppleMidiPacket::Synchronization {
                count, timestamp1, ..
            } => {
                assert_eq!(count, 1, "reply must be CK1");
                assert_eq!(timestamp1, 0xDEAD_BEEF, "CK1 echoes the CK0 timestamp1");
            }
            other => panic!("expected Synchronization, got {other:?}"),
        }
        pump.abort();
    }

    /// As the session initiator the pump must *drive* clock-sync: it sends its
    /// own CK0 and, on the peer's CK1 response, completes the exchange with CK2.
    #[tokio::test]
    async fn pump_initiates_clock_sync_and_completes_with_ck2() {
        use tokio::net::UdpSocket;

        let fake_ctl = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_ctl_port = fake_ctl.local_addr().unwrap().port();

        let ck2 = Arc::new(Mutex::new(None::<AppleMidiPacket>));
        let ck2_w = Arc::clone(&ck2);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = fake_ctl.recv_from(&mut buf).await.unwrap();
            let AppleMidiPacket::Invitation { token, ssrc, .. } =
                AppleMidiPacket::parse(&buf[..n]).unwrap()
            else {
                return;
            };
            let accept = AppleMidiPacket::InvitationAccepted {
                version: APPLEMIDI_VERSION,
                token,
                ssrc,
                name: "fake-peer".to_string(),
            };
            fake_ctl.send_to(&accept.to_bytes(), from).await.unwrap();

            // Wait for the pump's own CK0 (count 0), respond with CK1, then
            // capture the CK2 (count 2) that completes the exchange.
            loop {
                let (n, _) = fake_ctl.recv_from(&mut buf).await.unwrap();
                match AppleMidiPacket::parse(&buf[..n]) {
                    Ok(AppleMidiPacket::Synchronization {
                        count: 0,
                        timestamp1,
                        ..
                    }) => {
                        let ck1 = AppleMidiPacket::Synchronization {
                            ssrc,
                            count: 1,
                            timestamp1,
                            timestamp2: 0x1234,
                            timestamp3: 0,
                        };
                        fake_ctl.send_to(&ck1.to_bytes(), from).await.unwrap();
                    }
                    Ok(p @ AppleMidiPacket::Synchronization { count: 2, .. }) => {
                        *ck2_w.lock().unwrap() = Some(p);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let status = connecting_status();
        let (notifier, _rx) = broadcast::channel(8);
        let remote = RemoteEndpoint {
            host: None,
            addr: "127.0.0.1".to_string(),
            port: fake_ctl_port,
            port_id: "source-0".to_string(),
        };
        let pump = tokio::spawn(run_pump(
            remote,
            Box::new(RecordingSink::new()),
            Arc::clone(&status),
            notifier,
            Arc::new(Notify::new()),
        ));

        let reply = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(p) = ck2.lock().unwrap().clone() {
                    break p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("received a CK2 within timeout");

        match reply {
            AppleMidiPacket::Synchronization {
                count, timestamp2, ..
            } => {
                assert_eq!(count, 2, "initiator completes the exchange with CK2");
                assert_eq!(timestamp2, 0x1234, "CK2 echoes the peer's CK1 timestamp2");
            }
            other => panic!("expected Synchronization, got {other:?}"),
        }
        pump.abort();
    }

    /// Unmount must tear the session down *gracefully*: on cancel the pump sends
    /// `End` (BY) to the remote so it does not hold a stale session, and the
    /// mount transitions to `torn-down`.
    #[tokio::test]
    async fn pump_cancel_sends_end_to_peer_and_tears_down() {
        use tokio::net::UdpSocket;

        let fake_ctl = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_ctl_port = fake_ctl.local_addr().unwrap().port();

        let got_end = Arc::new(Mutex::new(false));
        let got_end_w = Arc::clone(&got_end);
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = fake_ctl.recv_from(&mut buf).await.unwrap();
            let AppleMidiPacket::Invitation { token, ssrc, .. } =
                AppleMidiPacket::parse(&buf[..n]).unwrap()
            else {
                return;
            };
            let accept = AppleMidiPacket::InvitationAccepted {
                version: APPLEMIDI_VERSION,
                token,
                ssrc,
                name: "fake-peer".to_string(),
            };
            fake_ctl.send_to(&accept.to_bytes(), from).await.unwrap();
            // Wait for the graceful End (BY) that unmount should send.
            loop {
                let (n, _) = fake_ctl.recv_from(&mut buf).await.unwrap();
                if let Ok(AppleMidiPacket::End { .. }) = AppleMidiPacket::parse(&buf[..n]) {
                    *got_end_w.lock().unwrap() = true;
                    break;
                }
            }
        });

        let status = connecting_status();
        let (notifier, _rx) = broadcast::channel(8);
        let cancel = Arc::new(Notify::new());
        let remote = RemoteEndpoint {
            host: None,
            addr: "127.0.0.1".to_string(),
            port: fake_ctl_port,
            port_id: "source-0".to_string(),
        };
        let pump = tokio::spawn(run_pump(
            remote,
            Box::new(RecordingSink::new()),
            Arc::clone(&status),
            notifier,
            Arc::clone(&cancel),
        ));

        // Once active, unmount (cancel) and expect a graceful BY at the peer.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if status.lock().unwrap().state == MountState::Active {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("mount reached active");
        cancel.notify_one();

        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if *got_end.lock().unwrap() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "peer did not receive End (BY) on unmount");
        let _ = pump.await;
        assert_eq!(status.lock().unwrap().state, MountState::TornDown);
    }

    /// A remote that explicitly rejects the invitation must fail the mount
    /// *fast* — within the handshake, not by spinning to the multi-attempt
    /// timeout — and carry a rejection detail.
    #[tokio::test]
    async fn pump_rejection_fails_fast_with_detail() {
        use tokio::net::UdpSocket;

        let fake_ctl = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fake_ctl_port = fake_ctl.local_addr().unwrap().port();

        // Fake peer: reply to the invitation with an explicit "NO".
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, from) = fake_ctl.recv_from(&mut buf).await.unwrap();
            if let Ok(AppleMidiPacket::Invitation { token, ssrc, .. }) =
                AppleMidiPacket::parse(&buf[..n])
            {
                let reject = AppleMidiPacket::InvitationRejected {
                    version: APPLEMIDI_VERSION,
                    token,
                    ssrc,
                    name: "fake-peer".to_string(),
                };
                fake_ctl.send_to(&reject.to_bytes(), from).await.unwrap();
            }
        });

        let (notifier, mut rx) = broadcast::channel(8);
        let status = connecting_status();
        let remote = RemoteEndpoint {
            host: None,
            addr: "127.0.0.1".to_string(),
            port: fake_ctl_port,
            port_id: "source-0".to_string(),
        };

        RtpConnector.start(
            remote,
            Box::new(RecordingSink::new()),
            Arc::clone(&status),
            notifier,
            Arc::new(Notify::new()),
        );

        // A 2s bound is well under the 3×5s handshake timeout, so passing proves
        // the rejection short-circuits the retry loop.
        let notification = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("failed notification within timeout (should not spin to handshake timeout)")
            .expect("notification received");
        assert_eq!(notification["params"]["state"], "failed");
        let detail = notification["params"]["detail"].as_str().unwrap_or("");
        assert!(
            detail.contains("rejected"),
            "detail should mention rejection, got: {detail}"
        );
        assert_eq!(status.lock().unwrap().state, MountState::Failed);
    }

    /// When the pump can't establish the session, the mount must transition to
    /// `failed` (with a detail) and emit a `mount-state` failed notification —
    /// the signal capmeshd's reconciler keys re-reconcile off. Driven fast via
    /// an unparseable remote address so run_pump returns immediately.
    #[tokio::test]
    async fn pump_failure_transitions_to_failed_and_notifies() {
        let (notifier, mut rx) = broadcast::channel(8);
        let status = connecting_status();
        let remote = RemoteEndpoint {
            host: None,
            addr: "definitely-not-an-ip".to_string(),
            port: 5008,
            port_id: "source-0".to_string(),
        };

        RtpConnector.start(
            remote,
            Box::new(RecordingSink::new()),
            Arc::clone(&status),
            notifier,
            Arc::new(Notify::new()),
        );

        let notification = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("failed notification within timeout")
            .expect("notification received");
        assert_eq!(notification["method"], "mount-state");
        assert_eq!(notification["params"]["state"], "failed");
        assert!(
            notification["params"]["detail"].is_string(),
            "failed notification carries a detail"
        );
        assert_eq!(status.lock().unwrap().state, MountState::Failed);
    }
}
