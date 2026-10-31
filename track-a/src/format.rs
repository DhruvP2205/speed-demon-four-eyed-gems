//! Output format: specs/a-ingest.md section 4.
//!
//! One line per (hour, endpoint) group with >=1 request, space-separated:
//!     hour endpoint count p50 p95 p99 errors error_ppm\n
//!
//! Percentiles are nearest-rank in INTEGER arithmetic (section 3.4):
//! `(p * n + 99) / 100` is `ceil(p * n / 100)`, a 1-indexed rank into
//! the group's latencies sorted ascending -- computed by the caller
//! (`main.rs`, via `select_nth_unstable` rather than a full sort; see
//! `percentile_rank`, the shared 0-indexed-rank formula both that and
//! this module's own tests rely on) and passed in already resolved,
//! since this module only formats, never aggregates. error_ppm is
//! `floor(errors * 1_000_000 / count)` (section 3.6), also integer.
//! No binary floating point anywhere in this file, per
//! CONVENTIONS.md.

use std::io::Write;

/// 0-indexed nearest-rank position `(p * n + 99) / 100 - 1` into `n`
/// ascending-sorted values -- the same formula as the 1-indexed
/// `(p * n + 99) / 100` from spec section 3.4, shifted for direct use
/// as a slice index (e.g. with `select_nth_unstable`).
#[inline]
pub fn percentile_rank(n: u64, p: u64) -> usize {
    (((p * n + 99) / 100) - 1) as usize
}

pub struct GroupLine<'a> {
    pub hour: &'a [u8; 20],
    pub endpoint: &'a [u8],
    pub count: u32,
    pub errors: u32,
    pub p50: u32,
    pub p95: u32,
    pub p99: u32,
}

pub fn write_group_line<W: Write>(w: &mut W, line: &GroupLine) -> std::io::Result<()> {
    let error_ppm = (line.errors as u64 * 1_000_000 / line.count as u64) as u32;
    w.write_all(line.hour)?;
    w.write_all(b" ")?;
    w.write_all(line.endpoint)?;
    write!(
        w,
        " {} {} {} {} {} {}\n",
        line.count, line.p50, line.p95, line.p99, line.errors, error_ppm
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same values the old sorted-slice-indexing formula used, now via
    /// `percentile_rank` -- confirms the 0-indexed shift is right.
    fn percentile(sorted: &[u32], p: u64) -> u32 {
        sorted[percentile_rank(sorted.len() as u64, p)]
    }

    #[test]
    fn percentile_rank_matches_spec_examples() {
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
    fn percentile_rank_health_group_from_the_worked_example() {
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
        write_group_line(
            &mut buf,
            &GroupLine {
                hour,
                endpoint: b"/v2/checkout",
                count: 5,
                errors: 3,
                p50: 2500,
                p95: 204117,
                p99: 204117,
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
        write_group_line(
            &mut buf,
            &GroupLine {
                hour,
                endpoint: b"/v2/checkout",
                count: 1,
                errors: 1,
                p50: 7,
                p95: 7,
                p99: 7,
            },
        )
        .unwrap();
        assert_eq!(
            buf,
            b"2026-10-31T23:00:00Z /v2/checkout 1 7 7 7 1 1000000\n"
        );
    }
}
