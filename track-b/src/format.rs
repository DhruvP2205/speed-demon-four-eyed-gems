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
