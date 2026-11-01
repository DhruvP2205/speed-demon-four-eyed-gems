//! Split an in-memory `docs.txt` buffer into N line-aligned byte
//! ranges for parallel processing. A document's text is defined to
//! contain no TAB, LF or CR byte (spec section 2.1), so a line is
//! never split across a thread boundary by splitting on raw `\n` --
//! no quote-tracking needed, unlike Track C's CSV.

/// Split `data` into up to `n` non-overlapping, line-aligned ranges
/// covering `data` with no line split across a boundary.
pub fn line_aligned_ranges(data: &[u8], n: usize) -> Vec<(usize, usize)> {
    let len = data.len();
    if len == 0 || n == 0 {
        return Vec::new();
    }
    let n = n.min(len).max(1);
    let targets: Vec<usize> = (1..n).map(|i| len * i / n).collect();

    let mut boundaries = vec![0usize];
    for &t in &targets {
        let b = match data[t..].iter().position(|&b| b == b'\n') {
            Some(idx) => t + idx + 1,
            None => len,
        };
        boundaries.push(b);
    }
    boundaries.push(len);
    boundaries.dedup();

    boundaries
        .windows(2)
        .filter(|w| w[1] > w[0])
        .map(|w| (w[0], w[1]))
        .collect()
}
