//! Robustness guard for the two `nmidi-core` wire parsers that consume untrusted
//! network bytes: [`AppleMidiPacket::parse`] (control channel) and
//! [`RtpPacket::parse`] (data channel). The `nmidid` pump feeds these whatever
//! arrives on the UDP sockets — truncated datagrams, random garbage, bit-flipped
//! packets from a hostile or buggy peer — so a parser that panics is a remote
//! crash of the mount task (a DoS). These tests assert the parsers only ever
//! return `Ok`/`Err`, never panic or hang, on arbitrary input.
//!
//! This is a deterministic stand-in for a fuzzer: a fixed-seed xorshift PRNG plus
//! exhaustive truncation/bit-flip sweeps over canonical packets, so it runs in
//! the ordinary (sandboxed) `-p nmidid` test suite and pins the no-panic
//! invariant against regressions in the parsers' bounds-checking.
//!
//! It lives in the `nmidid` crate (which depends on `nmidi-core`) rather than in
//! `nmidi-core` itself so it runs on the flake's verified `cargoTestFlags -p
//! nmidid` path.

use nmidi_core::{APPLEMIDI_SIGNATURE, APPLEMIDI_VERSION, AppleMidiPacket, RtpPacket};

/// Deterministic xorshift64* PRNG — no external dependency, reproducible failures.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }

    /// A random buffer whose length is in `[0, max_len]`.
    fn buf(&mut self, max_len: usize) -> Vec<u8> {
        let len = (self.next_u64() as usize) % (max_len + 1);
        (0..len).map(|_| self.byte()).collect()
    }
}

/// Parsing must never panic — return whether it produced a value (for sanity).
fn no_panic_applemidi(data: &[u8]) -> bool {
    AppleMidiPacket::parse(data).is_ok()
}

fn no_panic_rtp(data: &[u8]) -> bool {
    RtpPacket::parse(data).is_ok()
}

/// A canonical, well-formed sample of every AppleMIDI packet variant.
fn applemidi_samples() -> Vec<Vec<u8>> {
    let name = "some-device-name".to_string();
    vec![
        AppleMidiPacket::Invitation {
            version: APPLEMIDI_VERSION,
            token: 0x1122_3344,
            ssrc: 0x5566_7788,
            name: name.clone(),
        },
        AppleMidiPacket::InvitationAccepted {
            version: APPLEMIDI_VERSION,
            token: 0x1122_3344,
            ssrc: 0x5566_7788,
            name: name.clone(),
        },
        AppleMidiPacket::InvitationRejected {
            version: APPLEMIDI_VERSION,
            token: 0x1122_3344,
            ssrc: 0x5566_7788,
            name,
        },
        AppleMidiPacket::End {
            version: APPLEMIDI_VERSION,
            token: 0x1122_3344,
            ssrc: 0x5566_7788,
        },
        AppleMidiPacket::Synchronization {
            ssrc: 0xAABB_CCDD,
            count: 1,
            timestamp1: 0x1111_1111_1111_1111,
            timestamp2: 0x2222_2222_2222_2222,
            timestamp3: 0x3333_3333_3333_3333,
        },
        AppleMidiPacket::ReceiverFeedback {
            ssrc: 0x1122_3344,
            sequence: 4242,
        },
    ]
    .into_iter()
    .map(|p| p.to_bytes().to_vec())
    .collect()
}

fn rtp_samples() -> Vec<Vec<u8>> {
    let mut multi = RtpPacket::new(0xDEAD_BEEF, 42, 1000);
    multi.add_command(0, vec![0x90, 0x3C, 0x64]); // note on
    multi.add_command(10, vec![0x80, 0x3C, 0x00]); // note off
    multi.add_command(5, vec![0xB0, 0x07, 0x7F]); // control change

    let mut running = RtpPacket::new(0xCAFE_F00D, 7, 55);
    running.add_command(0, vec![0x90, 0x3C, 0x64]);
    running.add_command(0, vec![0x90, 0x3E, 0x64]);

    vec![
        RtpPacket::new(0x0000_0001, 0, 0), // empty command list
        multi,
        running,
    ]
    .into_iter()
    .map(|p| p.to_bytes().to_vec())
    .collect()
}

#[test]
fn applemidi_parse_survives_every_truncation() {
    for sample in applemidi_samples() {
        for len in 0..=sample.len() {
            // Must not panic for any prefix length.
            let _ = no_panic_applemidi(&sample[..len]);
        }
        // The full, well-formed packet must actually parse.
        assert!(
            no_panic_applemidi(&sample),
            "a canonical AppleMIDI packet failed to parse"
        );
    }
}

#[test]
fn rtp_parse_survives_every_truncation() {
    for sample in rtp_samples() {
        for len in 0..=sample.len() {
            let _ = no_panic_rtp(&sample[..len]);
        }
        assert!(
            no_panic_rtp(&sample),
            "a canonical RTP-MIDI packet failed to parse"
        );
    }
}

#[test]
fn applemidi_parse_survives_single_bit_flips() {
    for sample in applemidi_samples() {
        for byte_idx in 0..sample.len() {
            for bit in 0..8u32 {
                let mut corrupt = sample.clone();
                corrupt[byte_idx] ^= 1 << bit;
                let _ = no_panic_applemidi(&corrupt);
            }
        }
    }
}

#[test]
fn rtp_parse_survives_single_bit_flips() {
    for sample in rtp_samples() {
        for byte_idx in 0..sample.len() {
            for bit in 0..8u32 {
                let mut corrupt = sample.clone();
                corrupt[byte_idx] ^= 1 << bit;
                let _ = no_panic_rtp(&corrupt);
            }
        }
    }
}

#[test]
fn parsers_survive_random_garbage() {
    // Sweep a large number of random buffers through both parsers. Datagrams on
    // these sockets are bounded by the recv buffer, so 0..=2048 covers the range.
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..50_000 {
        let data = rng.buf(2048);
        let _ = no_panic_applemidi(&data);
        let _ = no_panic_rtp(&data);
    }
}

#[test]
fn parsers_survive_valid_prefix_plus_random_tail() {
    // A valid signature/header followed by hostile garbage is the most likely
    // real-world malformed packet (a peer that frames correctly but then sends
    // junk). Prefix each canonical sample with random-length random tails.
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for base in applemidi_samples() {
        for _ in 0..2000 {
            let mut data = base.clone();
            data.extend(rng.buf(256));
            let _ = no_panic_applemidi(&data);
        }
    }
    for base in rtp_samples() {
        for _ in 0..2000 {
            let mut data = base.clone();
            data.extend(rng.buf(256));
            let _ = no_panic_rtp(&data);
        }
    }

    // Also: a correct AppleMIDI signature with a random command/body — exercises
    // the command-dispatch and per-variant length checks with fuzzed fields.
    for _ in 0..20_000 {
        let mut data = APPLEMIDI_SIGNATURE.to_be_bytes().to_vec();
        data.extend(rng.buf(60));
        let _ = no_panic_applemidi(&data);
    }
}
