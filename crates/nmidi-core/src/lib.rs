pub mod applemidi;
pub mod discovery;
pub mod error;
pub mod midi;
pub mod network;
pub mod rtp;
pub mod util;

pub use applemidi::{AppleMidiCommand, AppleMidiPacket};
pub use error::ProtocolError;
pub use rtp::{MidiCommand, RtpPacket};

pub const APPLEMIDI_SIGNATURE: u16 = 0xFFFF;
pub const APPLEMIDI_VERSION: u16 = 2;
