use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("Invalid packet format: {0}")]
    InvalidFormat(String),

    #[error("Invalid signature: expected {expected:#X}, got {got:#X}")]
    InvalidSignature { expected: u16, got: u16 },

    #[error("Invalid command: {0}")]
    InvalidCommand(String),

    #[error("Packet too short: expected at least {expected} bytes, got {got}")]
    PacketTooShort { expected: usize, got: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
