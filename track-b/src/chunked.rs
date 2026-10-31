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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_count(data: &[u8]) -> usize {
        data.split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .count()
    }

    fn reconstruct_line_count(data: &[u8], n: usize) -> usize {
        line_aligned_ranges(data, n)
            .iter()
            .map(|&(s, e)| line_count(&data[s..e]))
            .sum()
    }

    #[test]
    fn splitting_never_changes_the_total_line_count() {
        let data = b"0\tthe quick fox\n".repeat(1000);
        let baseline = line_count(&data);
        for n in [1, 2, 3, 7, 12] {
            assert_eq!(reconstruct_line_count(&data, n), baseline, "n={n}");
        }
    }

    #[test]
    fn ranges_cover_the_whole_buffer_with_no_gap_or_overlap() {
        let data = b"0\tsome text\n".repeat(500);
        for n in [1, 2, 3, 7, 12] {
            let ranges = line_aligned_ranges(&data, n);
            let mut expected_start = 0;
            for &(start, end) in &ranges {
                assert_eq!(start, expected_start, "n={n}");
                assert!(end > start, "n={n}");
                expected_start = end;
            }
            assert_eq!(expected_start, data.len(), "n={n}");
        }
    }

    #[test]
    fn empty_buffer_produces_no_ranges() {
        assert_eq!(line_aligned_ranges(b"", 12), Vec::new());
    }
}
