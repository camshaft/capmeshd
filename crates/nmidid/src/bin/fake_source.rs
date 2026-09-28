//! `nmidi-fake-source` — a synthetic RTP-MIDI *source* for headless rehearsal.
//!
//! It plays the AppleMIDI *invitee* (server) role a real MIDI source device would:
//! it binds a control + data UDP socket pair, accepts an incoming invitation,
//! answers clock-sync probes, and streams periodic RTP-MIDI notes to the peer.
//!
//! This lets the full `nmidid` mount data path (`connecting → active → bytes-in`)
//! be exercised in CI without any physical MIDI hardware: point an `nmidid`
//! mount at this source and the note stream drives the mount to `active`. The
//! companion capmeshd nixosTest loads `snd-virmidi` in the guest so `nmidid` can
//! create its local virtual mirror.
//!
//! Single-peer: the most recent inviter is the active peer.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use nmidi_core::discovery::ServiceAdvertiser;
use nmidi_core::util::generate_ssrc;
use nmidi_core::{APPLEMIDI_VERSION, AppleMidiPacket, RtpPacket};
use tokio::net::UdpSocket;
use tracing::{Level, info, warn};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(name = "nmidi-fake-source")]
#[command(about = "Synthetic RTP-MIDI source for headless rehearsal/CI")]
struct Args {
    /// Address to bind the control + data sockets on.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Control port to bind; the data port is this + 1 (AppleMIDI convention).
    #[arg(long, default_value = "5008")]
    port: u16,

    /// Interval between emitted MIDI events, in milliseconds.
    #[arg(long, default_value = "500")]
    note_interval_ms: u64,

    /// Accept the session and answer clock-sync, but emit no MIDI. Lets a
    /// rehearsal assert that a live-but-silent source keeps the mount `active`
    /// via clock-sync alone (rather than being torn down as unresponsive).
    #[arg(long)]
    no_notes: bool,

    /// Reject every invitation with `NO` instead of accepting. Lets a rehearsal
    /// assert the mount fails fast with a `rejected` detail (CONTROL-PROTOCOL).
    #[arg(long)]
    reject: bool,

    /// Session name advertised in the invitation reply and the mDNS service.
    #[arg(long, default_value = "nmidi-fake-source")]
    name: String,

    /// Do not advertise the `_apple-midi._udp` mDNS service (advertised by default
    /// so a browsing host can discover the data-plane control port).
    #[arg(long)]
    no_advertise: bool,

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, default_value = "info")]
    log_level: String,
}

/// The control-plane reply this source sends for an inbound AppleMIDI packet, if
/// any. Pure (no I/O), so the accept / reject / clock-sync behavior is testable:
/// an invitation is accepted (`OK`) or, in `reject` mode, declined (`NO`); a `CK0`
/// clock-sync probe is answered with `CK1` carrying our timestamp; anything else
/// (e.g. `End`) has no reply.
fn control_reply(
    packet: &AppleMidiPacket,
    reject: bool,
    ssrc: u32,
    name: &str,
    ts: u64,
) -> Option<AppleMidiPacket> {
    match packet {
        AppleMidiPacket::Invitation { token, .. } if reject => {
            Some(AppleMidiPacket::InvitationRejected {
                version: APPLEMIDI_VERSION,
                token: *token,
                ssrc,
                name: name.to_string(),
            })
        }
        AppleMidiPacket::Invitation { token, .. } => Some(AppleMidiPacket::InvitationAccepted {
            version: APPLEMIDI_VERSION,
            token: *token,
            ssrc,
            name: name.to_string(),
        }),
        AppleMidiPacket::Synchronization {
            count: 0,
            timestamp1,
            ..
        } => Some(AppleMidiPacket::Synchronization {
            ssrc,
            count: 1,
            timestamp1: *timestamp1,
            timestamp2: ts,
            timestamp3: 0,
        }),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let level = match args.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder().with_max_level(level).finish(),
    )?;

    let control = UdpSocket::bind((args.bind.as_str(), args.port))
        .await
        .with_context(|| format!("binding control {}:{}", args.bind, args.port))?;
    let data = UdpSocket::bind((args.bind.as_str(), args.port + 1))
        .await
        .with_context(|| format!("binding data {}:{}", args.bind, args.port + 1))?;
    let ssrc = generate_ssrc();
    info!(
        "nmidi-fake-source listening on {}:{} (control) / {} (data), ssrc {:#x}",
        args.bind,
        args.port,
        args.port + 1,
        ssrc
    );

    // Advertise the data-plane endpoint over `_apple-midi._udp` (the RTP-MIDI
    // standard discovery record) so a browsing host learns the control port
    // without it being hand-supplied. Held for the process lifetime; the handle
    // unregisters the service on drop.
    let _service = if args.no_advertise {
        None
    } else {
        match ServiceAdvertiser::new()
            .and_then(|a| a.advertise_service(&args.name, &args.name, args.port, HashMap::new()))
        {
            Ok(service) => {
                info!(
                    "advertising _apple-midi._udp service {:?} on control port {}",
                    args.name, args.port
                );
                Some(service)
            }
            Err(e) => {
                // Advertising is best-effort; the source still works with a
                // hand-supplied remote port if mDNS is unavailable.
                warn!("mDNS advertise failed (continuing without it): {e}");
                None
            }
        }
    };

    let started = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_millis(args.note_interval_ms));
    let mut buf = [0u8; 2048];
    // The peer's DATA address (its control port + 1), set once a session is accepted.
    let mut peer_data: Option<SocketAddr> = None;
    let mut seq: u16 = 0;
    // A short major-scale phrase, alternating note-on / note-off.
    const NOTES: [u8; 4] = [60, 64, 67, 72];
    let mut step: usize = 0;

    loop {
        tokio::select! {
            recv = control.recv_from(&mut buf) => {
                let (n, from) = recv.context("control recv")?;
                let Ok(packet) = AppleMidiPacket::parse(&buf[..n]) else { continue };
                let ts = (started.elapsed().as_micros() / 100) as u64;
                if let Some(reply) = control_reply(&packet, args.reject, ssrc, &args.name, ts) {
                    control.send_to(&reply.to_bytes(), from).await.context("send control reply")?;
                }
                // Session-state side effects (kept out of the pure reply logic).
                match packet {
                    AppleMidiPacket::Invitation { .. } if args.reject => {
                        info!("rejected session from {from}");
                    }
                    AppleMidiPacket::Invitation { .. } => {
                        peer_data = Some(SocketAddr::new(from.ip(), from.port() + 1));
                        info!("accepted session from {from}; streaming MIDI");
                    }
                    AppleMidiPacket::End { .. } => {
                        info!("peer ended the session");
                        peer_data = None;
                    }
                    _ => {}
                }
            }
            _ = ticker.tick() => {
                if let Some(dest) = peer_data.filter(|_| !args.no_notes) {
                    let note = NOTES[(step / 2) % NOTES.len()];
                    let on = step.is_multiple_of(2);
                    let midi = if on {
                        vec![0x90, note, 0x64] // note on
                    } else {
                        vec![0x80, note, 0x00] // note off
                    };
                    step = step.wrapping_add(1);
                    let mut rtp = RtpPacket::new(ssrc, seq, seq as u32 * 100);
                    seq = seq.wrapping_add(1);
                    rtp.add_command(0, midi);
                    if let Err(e) = data.send_to(&rtp.to_bytes(), dest).await {
                        info!("data send to {dest} failed: {e}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSRC: u32 = 0xDEAD_BEEF;
    const NAME: &str = "test-source";
    const TS: u64 = 1234;

    fn invitation(token: u32) -> AppleMidiPacket {
        AppleMidiPacket::Invitation {
            version: APPLEMIDI_VERSION,
            token,
            ssrc: 0x0102_0304,
            name: "inviter".to_string(),
        }
    }

    #[test]
    fn an_invitation_is_accepted_by_default() {
        let reply = control_reply(&invitation(42), false, SSRC, NAME, TS);
        match reply {
            Some(AppleMidiPacket::InvitationAccepted {
                token, ssrc, name, ..
            }) => {
                assert_eq!(token, 42, "reply echoes the inviter's token");
                assert_eq!(ssrc, SSRC, "reply carries our ssrc");
                assert_eq!(name, NAME, "reply carries our name");
            }
            other => panic!("expected InvitationAccepted, got {other:?}"),
        }
    }

    #[test]
    fn an_invitation_is_rejected_in_reject_mode() {
        let reply = control_reply(&invitation(42), true, SSRC, NAME, TS);
        match reply {
            Some(AppleMidiPacket::InvitationRejected { token, ssrc, .. }) => {
                assert_eq!(token, 42, "rejection echoes the inviter's token");
                assert_eq!(ssrc, SSRC);
            }
            other => panic!("expected InvitationRejected, got {other:?}"),
        }
    }

    #[test]
    fn a_ck0_probe_is_answered_with_ck1() {
        let ck0 = AppleMidiPacket::Synchronization {
            ssrc: 0x0102_0304,
            count: 0,
            timestamp1: 999,
            timestamp2: 0,
            timestamp3: 0,
        };
        let reply = control_reply(&ck0, false, SSRC, NAME, TS);
        match reply {
            Some(AppleMidiPacket::Synchronization {
                ssrc,
                count,
                timestamp1,
                timestamp2,
                ..
            }) => {
                assert_eq!(ssrc, SSRC, "CK1 carries our ssrc");
                assert_eq!(count, 1, "CK0 is answered with count 1 (CK1)");
                assert_eq!(timestamp1, 999, "CK1 echoes the peer's timestamp1");
                assert_eq!(timestamp2, TS, "CK1 stamps our receive time in timestamp2");
            }
            other => panic!("expected Synchronization CK1, got {other:?}"),
        }
    }

    #[test]
    fn a_non_zero_sync_count_has_no_reply() {
        // Only CK0 opens a probe we answer; a CK1/CK2 in flight is not re-answered.
        let ck2 = AppleMidiPacket::Synchronization {
            ssrc: 0x0102_0304,
            count: 2,
            timestamp1: 1,
            timestamp2: 2,
            timestamp3: 3,
        };
        assert!(control_reply(&ck2, false, SSRC, NAME, TS).is_none());
    }

    #[test]
    fn an_end_packet_has_no_reply() {
        let end = AppleMidiPacket::End {
            version: APPLEMIDI_VERSION,
            token: 42,
            ssrc: 0x0102_0304,
        };
        assert!(control_reply(&end, false, SSRC, NAME, TS).is_none());
    }
}
