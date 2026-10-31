//! Hand-written integer parsing and hour-bucket byte-slicing. No
//! allocation, no checked arithmetic beyond what the spec's own value
//! ranges guarantee (status 100-599 fits comfortably in u32; latency
//! is explicitly uint32, 0 to 4294967295).

#[inline]
pub fn parse_u32(bytes: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = acc * 10 + (b - b'0') as u32;
    }
    acc
}

/// The 13-byte hour-bucket KEY (`YYYY-MM-DDTHH`, the fixed-width
/// prefix every grouping/interning step actually keys on) expanded
/// back to the full 20-byte output form `YYYY-MM-DDTHH:00:00Z`.
#[inline]
pub fn hour_output_bytes(hour_key: [u8; 13]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[..13].copy_from_slice(&hour_key);
    out[13..].copy_from_slice(b":00:00Z");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_unsigned_ints() {
        assert_eq!(parse_u32(b"0"), 0);
        assert_eq!(parse_u32(b"1234567890"), 1_234_567_890);
        assert_eq!(parse_u32(b"4294967295"), u32::MAX);
    }

    /// A 27-byte timestamp's first 13 bytes already ARE its hour-key
    /// -- the same byte-slice `main.rs`'s `process_line` takes
    /// directly off each line, exercised here via the full 20-byte
    /// expansion `hour_output_bytes` produces.
    fn hour_bucket_bytes(ts: &[u8]) -> [u8; 20] {
        debug_assert_eq!(ts.len(), 27);
        hour_output_bytes(ts[..13].try_into().expect("ts is at least 13 bytes"))
    }

    #[test]
    fn hour_bucket_truncates_correctly() {
        let bucket = hour_bucket_bytes(b"2026-10-30T18:42:17.114982Z");
        assert_eq!(&bucket, b"2026-10-30T18:00:00Z");
    }

    #[test]
    fn hour_bucket_handles_month_and_day_rollover() {
        assert_eq!(
            &hour_bucket_bytes(b"2026-11-01T00:00:00.000000Z"),
            b"2026-11-01T00:00:00Z"
        );
        assert_eq!(
            &hour_bucket_bytes(b"2026-10-31T23:59:59.999999Z"),
            b"2026-10-31T23:00:00Z"
        );
    }
}
