//! Output formatting -- the ONE place that should need to change once
//! the real spec ships (CLAUDE.md rule 4).
//!
//! SPEC-TBD: the official task spec's exact output format and no-path
//! convention are not released yet. Everything here matches reference/
//! ref_d.py and reference/oracle_d's OWN invented stand-in exactly
//! (verified by diffing on the edge-case and medium fixtures before
//! either oracle was trusted at full scale):
//!
//!     s,t,distance\n
//!
//! one line per query, IN INPUT ORDER (queries have no natural
//! grouping key to sort by), `distance` = -1 (literal) if `t` is
//! unreachable from `s`.

use std::io::{self, Write};

pub fn write_result_line<W: Write>(
    writer: &mut W,
    s: u32,
    t: u32,
    distance: i64,
) -> io::Result<()> {
    writeln!(writer, "{s},{t},{distance}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_reachable_distance() {
        let mut buf = Vec::new();
        write_result_line(&mut buf, 0, 8, 18).unwrap();
        assert_eq!(buf, b"0,8,18\n");
    }

    #[test]
    fn formats_unreachable_as_literal_negative_one() {
        let mut buf = Vec::new();
        write_result_line(&mut buf, 0, 9, -1).unwrap();
        assert_eq!(buf, b"0,9,-1\n");
    }

    #[test]
    fn formats_large_u64_scale_distances() {
        let mut buf = Vec::new();
        write_result_line(&mut buf, 376770, 747710, 383254659993).unwrap();
        assert_eq!(buf, b"376770,747710,383254659993\n");
    }
}
