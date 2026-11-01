//! Output format: specs/b-index.md section 4.
//!
//! One line per query, in query order: `count sum id_1 ... id_m\n`,
//! `m = min(10, count)`, ids ascending. `result` is already sorted
//! ascending (every `eval.rs` set operation produces sorted output),
//! so this never sorts anything itself -- just reads off the front.

use std::io::Write;

pub fn write_result_line<W: Write>(w: &mut W, result: &[u32]) -> std::io::Result<()> {
    let count = result.len();
    let sum: u64 = result.iter().map(|&id| id as u64).sum();
    write!(w, "{count} {sum}")?;
    for &id in result.iter().take(10) {
        write!(w, " {id}")?;
    }
    writeln!(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(result: &[u32]) -> String {
        let mut buf = Vec::new();
        write_result_line(&mut buf, result).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn no_match_prints_zero_zero() {
        assert_eq!(line(&[]), "0 0\n");
    }

    #[test]
    fn worked_example_first_row() {
        // specs/b-index.md worked example: "BOOL bail AND bond" -> docs {0,1,3}.
        assert_eq!(line(&[0, 1, 3]), "3 4 0 1 3\n");
    }

    #[test]
    fn caps_the_id_list_at_ten_but_sum_and_count_cover_everything() {
        let result: Vec<u32> = (0..15).collect();
        assert_eq!(line(&result), "15 105 0 1 2 3 4 5 6 7 8 9\n");
    }
}
