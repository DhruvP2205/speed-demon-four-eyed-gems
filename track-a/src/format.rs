//! Output format: specs/a-ingest.md section 4.
//!
//! One line per (hour, endpoint) group with >=1 request, space-separated:
//!     hour endpoint count p50 p95 p99 errors error_ppm\n
//!
//! Percentiles are nearest-rank in INTEGER arithmetic (section 3.4):
//! `(p * n + 99) / 100` is `ceil(p * n / 100)`, a 1-indexed rank into
//! the group's latencies sorted ascending. error_ppm is `floor(errors
//! * 1_000_000 / count)` (section 3.6), also integer. No binary
//! floating point anywhere in this file, per CONVENTIONS.md.

use std::io::Write;

/// Nearest-rank percentile: 1-indexed rank `(p * n + 99) / 100`
/// (integer ceiling division), on latencies already sorted ascending.
#[inline]
pub fn percentile(sorted_latencies: &[u32], p: u64) -> u32 {
    let n = sorted_latencies.len() as u64;
    let rank = (p * n + 99) / 100;
    sorted_latencies[(rank - 1) as usize]
}

pub struct GroupLine<'a> {
    pub hour: &'a [u8; 20],
    pub endpoint: &'a [u8],
    pub count: u32,
    pub errors: u32,
    /// Latencies for this group, already sorted ascending.
    pub sorted_latencies: &'a [u32],
}

pub fn write_group_line<W: Write>(w: &mut W, line: &GroupLine) -> std::io::Result<()> {
    let p50 = percentile(line.sorted_latencies, 50);
    let p95 = percentile(line.sorted_latencies, 95);
    let p99 = percentile(line.sorted_latencies, 99);
    let error_ppm = (line.errors as u64 * 1_000_000 / line.count as u64) as u32;
    w.write_all(line.hour)?;
    w.write_all(b" ")?;
    w.write_all(line.endpoint)?;
    write!(
        w,
        " {} {} {} {} {} {}\n",
        line.count, p50, p95, p99, line.errors, error_ppm
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_matches_spec_examples() {
        // n=1: all three ranks equal the single value.
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);
        // n=101: ranks 51, 96, 100 (spec section 3.4's worked numbers).
        let v: Vec<u32> = (1..=101).collect();
        assert_eq!(percentile(&v, 50), 51);
        assert_eq!(percentile(&v, 95), 96);
        assert_eq!(percentile(&v, 99), 100);
    }

    #[test]
    fn percentile_health_group_from_the_worked_example() {
        // sorted [62, 62, 4294967295]: p50 is rank 2 (62), p95/p99 rank 3.
        let v = [62u32, 62, 4294967295];
        assert_eq!(percentile(&v, 50), 62);
        assert_eq!(percentile(&v, 95), 4294967295);
        assert_eq!(percentile(&v, 99), 4294967295);
    }

    #[test]
    fn write_group_line_matches_worked_example_checkout_row() {
        // hour 18, /v2/checkout: count=5, latencies sorted [0,1500,2500,30000,204117],
        // p50=2500 (rank3), p95=p99=204117 (rank5), errors=3 (503,500,599), ppm=600000.
        let mut buf = Vec::new();
        let hour = b"2026-10-30T18:00:00Z";
        let latencies = [0u32, 1500, 2500, 30000, 204117];
        write_group_line(
            &mut buf,
            &GroupLine {
                hour,
                endpoint: b"/v2/checkout",
                count: 5,
                errors: 3,
                sorted_latencies: &latencies,
            },
        )
        .unwrap();
        assert_eq!(
            buf,
            b"2026-10-30T18:00:00Z /v2/checkout 5 2500 204117 204117 3 600000\n"
        );
    }

    #[test]
    fn write_group_line_matches_worked_example_single_error_row() {
        // hour 2026-10-31T23:00, /v2/checkout: count=1, latency=7, errors=1 -> ppm=1000000.
        let mut buf = Vec::new();
        let hour = b"2026-10-31T23:00:00Z";
        let latencies = [7u32];
        write_group_line(
            &mut buf,
            &GroupLine {
                hour,
                endpoint: b"/v2/checkout",
                count: 1,
                errors: 1,
                sorted_latencies: &latencies,
            },
        )
        .unwrap();
        assert_eq!(
            buf,
            b"2026-10-31T23:00:00Z /v2/checkout 1 7 7 7 1 1000000\n"
        );
    }
}
