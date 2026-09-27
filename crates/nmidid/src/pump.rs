//! The RTP-MIDI data-path pump for a `mirror-source` mount.
//!
//! [`RtpConnector`] spawns a task that acts as the AppleMIDI *inviter*: it
//! completes the `IN`/`OK` handshake to the remote source, then receives
//! RTP-MIDI and forwards each MIDI message into the mount's local virtual sink,
//! driving the mount `connecting → active` and accumulating `bytes-in`. This is
//! the piece that makes a remote keyboard actually play a local instrument.
//!
//! Timestamp-accurate scheduling (full AppleMIDI clock sync) is a refinement:
//! today we respond to the remote's clock-sync probes minimally and forward
//! MIDI as it arrives, which is correct for live play.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmidi_core::network::NetworkSockets;
use nmidi_core::util::{generate_ssrc, generate_token, get_hostname};
use nmidi_core::{APPLEMIDI_VERSION, AppleMidiPacket, RtpPacket};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::mounts::{Connector, MidiSink, now_rfc3339, transition};
use crate::protocol::{MountState, MountStatus, RemoteEndpoint};

/// How long to wait for `InvitationAccepted` before retrying, and how many times.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_ATTEMPTS: u32 = 3;

/// Production connector: spawns the AppleMIDI handshake + RTP-MIDI pump.
pub struct RtpConnector;

impl Connector for RtpConnector {
    fn start(
        &self,
        remote: RemoteEndpoint,
        sink: Box<dyn MidiSink>,
        status: Arc<Mutex<MountStatus>>,
        notifier: broadcast::Sender<Value>,
    ) -> Option<AbortHandle> {
        let handle = tokio::spawn(async move {
            if let Err(e) = run_pump(remote, sink, Arc::clone(&status), notifier.clone()).await {
                warn!("mount pump failed: {e}");
                transition(&status, &notifier, MountState::Failed, Some(format!("{e}")));
            }
        });
        Some(handle.abort_handle())
    }
}

/// Connect to the remote source and pump RTP-MIDI into `sink` until cancelled or
/// the remote ends the session.
async fn run_pump(
    remote: RemoteEndpoint,
    sink: Box<dyn MidiSink>,
    status: Arc<Mutex<MountStatus>>,
    notifier: broadcast::Sender<Value>,
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
    sockets.send_control(&invitation, &control_addr).await?;
    sockets
        .send_control_on_data(&invitation, &data_addr)
        .await?;
    debug!("sent AppleMIDI invitation to {control_addr} / {data_addr}");

    // Await InvitationAccepted, resending on timeout.
    let mut attempts = 0;
    loop {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, sockets.recv_control()).await {
            Ok(Ok((AppleMidiPacket::InvitationAccepted { .. }, _))) => break,
            Ok(Ok(_)) => continue, // other control packet before accept; keep waiting
            Ok(Err(e)) => debug!("control recv during handshake: {e}"),
            Err(_) => {
                attempts += 1;
                if attempts >= HANDSHAKE_ATTEMPTS {
                    anyhow::bail!("no InvitationAccepted after {HANDSHAKE_ATTEMPTS} attempts");
                }
                sockets.send_control(&invitation, &control_addr).await?;
                sockets
                    .send_control_on_data(&invitation, &data_addr)
                    .await?;
            }
        }
    }

    info!(
        "mount active: mirroring {} into local virtual port",
        control_addr
    );
    transition(&status, &notifier, MountState::Active, None);

    let started = Instant::now();
    loop {
        tokio::select! {
            data = sockets.recv_data() => match data {
                Ok((packet, _)) => forward_rtp(&packet, sink.as_ref(), &status),
                Err(e) => debug!("data recv error: {e}"),
            },
            control = sockets.recv_control() => match control {
                Ok((AppleMidiPacket::End { .. }, _)) => {
                    info!("remote ended the session");
                    break;
                }
                Ok((AppleMidiPacket::Synchronization { count, timestamp1, .. }, from)) => {
                    // Minimal clock-sync keep-alive: answer CK0 with CK1 carrying
                    // our current timestamp (100µs units).
                    if count == 0 {
                        let ts = (started.elapsed().as_micros() / 100) as u64;
                        let reply = AppleMidiPacket::Synchronization {
                            ssrc,
                            count: 1,
                            timestamp1,
                            timestamp2: ts,
                            timestamp3: 0,
                        };
                        let _ = sockets.send_control(&reply, &from).await;
                    }
                }
                Ok(_) => {}
                Err(e) => debug!("control recv error: {e}"),
            },
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
