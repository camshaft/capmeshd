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
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::mounts::{Connector, MidiSink, now_rfc3339};
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
    ) -> Option<AbortHandle> {
        let handle = tokio::spawn(async move {
            if let Err(e) = run_pump(remote, sink, Arc::clone(&status)).await {
                warn!("mount pump failed: {e}");
                let mut s = status.lock().unwrap();
                s.state = MountState::Failed;
                s.detail = Some(format!("{e}"));
            }
        });
        Some(handle.abort_handle())
    }
}

fn set_state(status: &Arc<Mutex<MountStatus>>, state: MountState) {
    status.lock().unwrap().state = state;
}

/// Connect to the remote source and pump RTP-MIDI into `sink` until cancelled or
/// the remote ends the session.
async fn run_pump(
    remote: RemoteEndpoint,
    sink: Box<dyn MidiSink>,
    status: Arc<Mutex<MountStatus>>,
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
    set_state(&status, MountState::Active);

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
    set_state(&status, MountState::TornDown);
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

    struct RecordingSink(Mutex<Vec<Vec<u8>>>);
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
        let sink = RecordingSink(Mutex::new(Vec::new()));
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
        let sink = RecordingSink(Mutex::new(Vec::new()));
        let status = connecting_status();
        let pkt = RtpPacket::new(1, 1, 0); // no commands
        forward_rtp(&pkt, &sink, &status);
        assert!(sink.0.lock().unwrap().is_empty());
        let s = status.lock().unwrap();
        assert_eq!(s.state, MountState::Connecting);
        assert_eq!(s.stats.bytes_in, 0);
    }
}
