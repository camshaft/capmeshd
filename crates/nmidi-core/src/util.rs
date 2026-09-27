use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a unique SSRC based on current time
pub fn generate_ssrc() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (now & 0xFFFFFFFF) as u32
}

/// Generate a unique token based on current time
pub fn generate_token() -> u32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    ((now >> 32) & 0xFFFFFFFF) as u32
}

/// Get current timestamp in microseconds
pub fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// Convert microseconds to RTP MIDI timestamp (10kHz clock)
///
/// RFC 6295 specifies that RTP MIDI uses a 10kHz clock rate,
/// meaning each tick is 100 microseconds (0.1 milliseconds).
/// This function converts microseconds to ticks and wraps to u32.
pub fn micros_to_rtp_timestamp(micros: u64) -> u32 {
    // Divide by 100 to convert from microseconds to 100-microsecond ticks (10kHz)
    let ticks = micros / 100;
    // RTP timestamp is 32-bit and wraps around
    ticks as u32
}

/// Get the system hostname, falling back to "localhost" if unavailable
pub fn get_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_micros_to_rtp_timestamp() {
        // Test basic conversion: 100 microseconds = 1 tick
        assert_eq!(micros_to_rtp_timestamp(100), 1);

        // Test 1 millisecond = 10 ticks (1000 micros / 100)
        assert_eq!(micros_to_rtp_timestamp(1000), 10);

        // Test 1 second = 10,000 ticks (1,000,000 micros / 100)
        assert_eq!(micros_to_rtp_timestamp(1_000_000), 10_000);

        // Test wrapping: u32::MAX ticks in microseconds should wrap around
        let max_ticks_micros = (u32::MAX as u64) * 100;
        assert_eq!(micros_to_rtp_timestamp(max_ticks_micros), u32::MAX);

        // Test wrapping: one more tick should wrap to 0
        assert_eq!(micros_to_rtp_timestamp(max_ticks_micros + 100), 0);

        // Test wrapping: two more ticks should wrap to 1
        assert_eq!(micros_to_rtp_timestamp(max_ticks_micros + 200), 1);
    }
}
