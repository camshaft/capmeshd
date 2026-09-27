use crate::{APPLEMIDI_SIGNATURE, error::ProtocolError};
use bytes::{BufMut, Bytes, BytesMut};

/// AppleMIDI protocol commands (2-byte codes). Some peers pad commands with two
/// trailing NULs; the parser tolerates that padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleMidiCommand {
    /// Invitation to connect ("IN")
    Invitation,
    /// Invitation accepted ("OK")
    InvitationAccepted,
    /// End session ("BY")
    End,
    /// Clock synchronization ("CK")
    Synchronization,
    /// Receiver feedback ("RS")
    ReceiverFeedback,
}

impl AppleMidiCommand {
    /// Wire bytes to serialize for this command. Apple endpoints and Wireshark
    /// expect the session control commands (IN/OK/BY) to be 4-byte padded, while
    /// Synchronization (CK) is typically sent as the compact 2-byte form. We keep
    /// CK/RS compact to avoid misaligned sync fields, and pad invitation-family
    /// commands for correct OK parsing.
    pub fn tx_bytes(&self) -> &'static [u8] {
        match self {
            Self::Invitation => b"IN\0\0",
            Self::InvitationAccepted => b"OK\0\0",
            Self::End => b"BY\0\0",
            Self::Synchronization => b"CK", // Updated to reflect 2-byte command
            Self::ReceiverFeedback => b"RS",
        }
    }

    /// Returns (command, bytes_consumed). If a trailing two-byte NUL padding is
    /// detected, `bytes_consumed` will be 4 so that callers can skip it.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), ProtocolError> {
        if bytes.len() < 2 {
            return Err(ProtocolError::InvalidCommand(
                "Command must be at least 2 bytes".to_string(),
            ));
        }

        let cmd = match bytes[0..2] {
            [b'I', b'N'] => Self::Invitation,
            [b'O', b'K'] => Self::InvitationAccepted,
            [b'B', b'Y'] => Self::End,
            [b'C', b'K'] => Self::Synchronization,
            [b'R', b'S'] => Self::ReceiverFeedback,
            _ => {
                return Err(ProtocolError::InvalidCommand(format!(
                    "Unknown command: {:?}",
                    &bytes[0..2]
                )));
            }
        };

        let consumed = if bytes.len() >= 4 && bytes[2] == 0 && bytes[3] == 0 {
            4
        } else {
            2
        };

        Ok((cmd, consumed))
    }
}

/// AppleMIDI protocol packet
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppleMidiPacket {
    /// Invitation packet
    Invitation {
        version: u16,
        token: u32,
        ssrc: u32,
        name: String,
    },
    /// Invitation accepted packet
    InvitationAccepted {
        version: u16,
        token: u32,
        ssrc: u32,
        name: String,
    },
    /// End session packet
    End { version: u16, token: u32, ssrc: u32 },
    /// Synchronization packet
    Synchronization {
        ssrc: u32,
        count: u8,
        timestamp1: u64,
        timestamp2: u64,
        timestamp3: u64,
    },
    /// Receiver feedback packet (RFC 6295 §5.3). We currently parse the
    /// sequence number and SSRC and ignore any optional recovery journal data.
    ReceiverFeedback { ssrc: u32, sequence: u16 },
}

impl AppleMidiPacket {
    /// Parse an AppleMIDI packet from bytes
    pub fn parse(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < 6 {
            return Err(ProtocolError::PacketTooShort {
                expected: 6,
                got: data.len(),
            });
        }

        if u16::from_be_bytes(data[0..2].try_into().unwrap()) != APPLEMIDI_SIGNATURE {
            return Err(ProtocolError::InvalidSignature {
                expected: APPLEMIDI_SIGNATURE,
                got: u16::from_be_bytes(data[0..2].try_into().unwrap()),
            });
        }

        // Command starts at offset 2. Allow up to 4 bytes to detect optional padding.
        let cmd_slice_len = (data.len() - 2).min(4);
        let (command, consumed) = AppleMidiCommand::from_bytes(&data[2..2 + cmd_slice_len])?;
        let mut offset = 2 + consumed;

        match command {
            AppleMidiCommand::Invitation | AppleMidiCommand::InvitationAccepted => {
                let header_len = 2 /*sig*/ + consumed + 2 /*version*/ + 4 /*token*/ + 4 /*ssrc*/;
                if data.len() < header_len {
                    return Err(ProtocolError::PacketTooShort {
                        expected: header_len,
                        got: data.len(),
                    });
                }

                let version = u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap());
                offset += 2;
                let token = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;
                let ssrc = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;

                let name_bytes = &data[offset..];
                let name_end = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                let name = String::from_utf8_lossy(&name_bytes[..name_end]).to_string();

                if command == AppleMidiCommand::Invitation {
                    Ok(AppleMidiPacket::Invitation {
                        version,
                        token,
                        ssrc,
                        name,
                    })
                } else {
                    Ok(AppleMidiPacket::InvitationAccepted {
                        version,
                        token,
                        ssrc,
                        name,
                    })
                }
            }
            AppleMidiCommand::End => {
                let header_len = 2 /*sig*/ + consumed + 2 /*version*/ + 4 /*token*/ + 4 /*ssrc*/;
                if data.len() < header_len {
                    return Err(ProtocolError::PacketTooShort {
                        expected: header_len,
                        got: data.len(),
                    });
                }

                let version = u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap());
                offset += 2;
                let token = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;
                let ssrc = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());

                Ok(AppleMidiPacket::End {
                    version,
                    token,
                    ssrc,
                })
            }
            AppleMidiCommand::Synchronization => {
                // signature (2) + command (2 or 4) + ssrc (4) + count (1) + 3 padding + 3*u64
                // 2-byte command -> 36 bytes, 4-byte padded command -> 38 bytes
                let min_len = if consumed == 4 { 38 } else { 36 };
                if data.len() < min_len {
                    return Err(ProtocolError::PacketTooShort {
                        expected: min_len,
                        got: data.len(),
                    });
                }

                let ssrc = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;
                let count = data[offset];
                offset += 1;

                // Padding (3 bytes)
                offset += 3;

                let timestamp1 = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
                offset += 8;
                let timestamp2 = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());
                offset += 8;
                let timestamp3 = u64::from_be_bytes(data[offset..offset + 8].try_into().unwrap());

                Ok(AppleMidiPacket::Synchronization {
                    ssrc,
                    count,
                    timestamp1,
                    timestamp2,
                    timestamp3,
                })
            }
            AppleMidiCommand::ReceiverFeedback => {
                // signature (2) + command (2 or 4) + sequence (2) + ssrc (4)
                let min_len = if consumed == 4 { 12 } else { 10 };
                if data.len() < min_len {
                    return Err(ProtocolError::PacketTooShort {
                        expected: min_len,
                        got: data.len(),
                    });
                }

                let sequence = u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap());
                offset += 2;
                let ssrc = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());

                Ok(AppleMidiPacket::ReceiverFeedback { ssrc, sequence })
            }
        }
    }

    /// Serialize the packet to bytes
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::new();

        buf.put_u16(APPLEMIDI_SIGNATURE);

        match self {
            AppleMidiPacket::Invitation {
                version,
                token,
                ssrc,
                name,
            } => {
                buf.put_slice(AppleMidiCommand::Invitation.tx_bytes());
                buf.put_u16(*version);
                buf.put_u32(*token);
                buf.put_u32(*ssrc);
                buf.put_slice(name.as_bytes());
                buf.put_u8(0); // null terminator
            }
            AppleMidiPacket::InvitationAccepted {
                version,
                token,
                ssrc,
                name,
            } => {
                buf.put_slice(AppleMidiCommand::InvitationAccepted.tx_bytes());
                buf.put_u16(*version);
                buf.put_u32(*token);
                buf.put_u32(*ssrc);
                buf.put_slice(name.as_bytes());
                buf.put_u8(0); // null terminator
            }
            AppleMidiPacket::End {
                version,
                token,
                ssrc,
            } => {
                buf.put_slice(AppleMidiCommand::End.tx_bytes());
                buf.put_u16(*version);
                buf.put_u32(*token);
                buf.put_u32(*ssrc);
            }
            AppleMidiPacket::Synchronization {
                ssrc,
                count,
                timestamp1,
                timestamp2,
                timestamp3,
            } => {
                buf.put_slice(AppleMidiCommand::Synchronization.tx_bytes()); // Updated to reflect 2-byte command
                buf.put_u32(*ssrc);
                buf.put_u8(*count);
                buf.put_u8(0); // padding
                buf.put_u8(0); // padding
                buf.put_u8(0); // padding
                buf.put_u64(*timestamp1);
                buf.put_u64(*timestamp2);
                buf.put_u64(*timestamp3);
            }
            AppleMidiPacket::ReceiverFeedback { ssrc, sequence } => {
                buf.put_slice(AppleMidiCommand::ReceiverFeedback.tx_bytes());
                buf.put_u16(*sequence);
                buf.put_u32(*ssrc);
            }
        }

        buf.freeze()
    }

    pub fn ssrc(&self) -> u32 {
        match self {
            AppleMidiPacket::Invitation { ssrc, .. }
            | AppleMidiPacket::InvitationAccepted { ssrc, .. }
            | AppleMidiPacket::End { ssrc, .. }
            | AppleMidiPacket::Synchronization { ssrc, .. }
            | AppleMidiPacket::ReceiverFeedback { ssrc, .. } => *ssrc,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::APPLEMIDI_VERSION;

    fn make_inv_bytes(cmd: &[u8]) -> Bytes {
        let mut buf = BytesMut::new();
        buf.put_u16(APPLEMIDI_SIGNATURE);
        buf.put_slice(cmd);
        buf.put_u16(APPLEMIDI_VERSION);
        buf.put_u32(12345);
        buf.put_u32(67890);
        buf.put_slice(b"TestDevice");
        buf.put_u8(0);
        buf.freeze()
    }

    fn make_rs_bytes(sequence: u16, ssrc: u32) -> Bytes {
        let mut buf = BytesMut::new();
        buf.put_u16(APPLEMIDI_SIGNATURE);
        buf.put_slice(b"RS");
        buf.put_u16(sequence);
        buf.put_u32(ssrc);
        // Extra padding to simulate larger packets some implementations send
        buf.put_u32(0);
        buf.freeze()
    }

    #[test]
    fn test_invitation_roundtrip() {
        let packet = AppleMidiPacket::Invitation {
            version: 2,
            token: 12345,
            ssrc: 67890,
            name: "TestDevice".to_string(),
        };

        let bytes = packet.to_bytes();
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        assert_eq!(packet, parsed);
    }

    #[test]
    fn test_invitation_accepted_roundtrip() {
        let packet = AppleMidiPacket::InvitationAccepted {
            version: 2,
            token: 12345,
            ssrc: 222,
            name: "TestDevice".to_string(),
        };

        let bytes = packet.to_bytes();
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        assert_eq!(packet, parsed);

        // Check wire layout: version at offset 6, token at 10, ssrc at 14, then name.
        assert_eq!(&bytes[0..2], &APPLEMIDI_SIGNATURE.to_be_bytes());
        assert_eq!(&bytes[2..6], b"OK\0\0");
        assert_eq!(u16::from_be_bytes(bytes[6..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 12345);
        assert_eq!(u32::from_be_bytes(bytes[12..16].try_into().unwrap()), 222);
    }

    #[test]
    fn test_parses_zero_padded_invitation() {
        let bytes = make_inv_bytes(b"IN\0\0");
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        match parsed {
            AppleMidiPacket::Invitation {
                version,
                token,
                ssrc,
                name,
            } => {
                assert_eq!(version, APPLEMIDI_VERSION);
                assert_eq!(token, 12345);
                assert_eq!(ssrc, 67890);
                assert_eq!(name, "TestDevice");
            }
            other => panic!("expected Invitation, got {:?}", other),
        }
    }

    #[test]
    fn test_parses_zero_padded_invitation_accepted() {
        let mut buf = BytesMut::new();
        buf.put_u16(APPLEMIDI_SIGNATURE);
        buf.put_slice(b"OK\0\0");
        buf.put_u16(APPLEMIDI_VERSION);
        buf.put_u32(12345);
        buf.put_u32(0xBB);
        buf.put_slice(b"TestDevice");
        buf.put_u8(0);
        let bytes = buf.freeze();

        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        match parsed {
            AppleMidiPacket::InvitationAccepted {
                version,
                token,
                ssrc,
                name,
            } => {
                assert_eq!(version, APPLEMIDI_VERSION);
                assert_eq!(token, 12345);
                assert_eq!(ssrc, 0xBB);
                assert_eq!(name, "TestDevice");
            }
            other => panic!("expected InvitationAccepted, got {:?}", other),
        }
    }

    #[test]
    fn test_parses_receiver_feedback() {
        let bytes = make_rs_bytes(42, 0x11223344);
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        match parsed {
            AppleMidiPacket::ReceiverFeedback { ssrc, sequence } => {
                assert_eq!(ssrc, 0x11223344);
                assert_eq!(sequence, 42);
            }
            other => panic!("expected ReceiverFeedback, got {:?}", other),
        }
    }

    #[test]
    fn test_end_roundtrip() {
        let packet = AppleMidiPacket::End {
            version: 2,
            token: 12345,
            ssrc: 67890,
        };

        let bytes = packet.to_bytes();
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        assert_eq!(packet, parsed);
    }

    #[test]
    fn test_sync_roundtrip() {
        let packet = AppleMidiPacket::Synchronization {
            ssrc: 67890,
            count: 0,
            timestamp1: 1000,
            timestamp2: 2000,
            timestamp3: 3000,
        };

        let bytes = packet.to_bytes();
        let parsed = AppleMidiPacket::parse(&bytes).unwrap();

        assert_eq!(packet, parsed);

        // CK should remain 36-byte payload (2-byte command) to keep sync fields aligned
        assert_eq!(
            bytes.len(),
            2 /*sig*/ + 2 /*cmd*/ + 4 /*ssrc*/ + 1 + 3 + 8*3
        );
    }

    #[test]
    fn test_sync_parses_short_padding_variant() {
        // Build a spec sync (4-byte padded command, three padding bytes)
        let mut buf = BytesMut::new();
        buf.put_u16(APPLEMIDI_SIGNATURE);
        buf.put_slice(b"CK");
        buf.put_u32(0xAABBCCDD); // ssrc
        buf.put_u8(1); // count
        buf.put_u8(0); // padding
        buf.put_u8(0); // padding
        buf.put_u8(0); // padding
        buf.put_u64(10);
        buf.put_u64(20);
        buf.put_u64(30);
        let bytes = buf.freeze();

        let parsed = AppleMidiPacket::parse(&bytes).unwrap();
        match parsed {
            AppleMidiPacket::Synchronization {
                ssrc,
                count,
                timestamp1,
                timestamp2,
                timestamp3,
            } => {
                assert_eq!(ssrc, 0xAABBCCDD);
                assert_eq!(count, 1);
                assert_eq!(timestamp1, 10);
                assert_eq!(timestamp2, 20);
                assert_eq!(timestamp3, 30);
            }
            other => panic!("expected Synchronization, got {other:?}"),
        }
    }

    #[test]
    fn test_sync_too_short() {
        // 35 bytes should still be rejected
        let mut buf = BytesMut::new();
        buf.put_u16(APPLEMIDI_SIGNATURE);
        buf.put_slice(b"CK");
        buf.put_u32(0xAABBCCDD); // ssrc
        buf.put_u8(0); // count
        buf.put_u8(0); // padding
        buf.put_u8(0); // padding
        buf.put_u8(0); // padding
        buf.put_u64(1);
        buf.put_u64(2);
        buf.put_u64(3);
        let mut bytes = buf.freeze().to_vec();
        bytes.truncate(35); // force too-short

        let err = AppleMidiPacket::parse(&bytes).unwrap_err();
        match err {
            ProtocolError::PacketTooShort { expected, got } => {
                assert_eq!(expected, 36);
                assert_eq!(got, 35);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
