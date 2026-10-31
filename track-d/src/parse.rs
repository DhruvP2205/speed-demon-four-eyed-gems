//! Parsing helpers for `graph.txt`/`queries.txt`'s plain `u32` fields.
//! Every line is a fixed, small number of whitespace-separated uint32
//! fields with no quoting and no possibility of an embedded newline,
//! so the next `\n` is always a safe split point.

/// Split `[start, len)` of `data` into up to `n` line-aligned,
/// non-overlapping ranges covering that span with no line split
/// across a boundary. Operates directly on the mmap'd slice -- no
/// file I/O, no probe reads -- since the whole file is already
/// resident; finding each boundary is a plain, independent, parallel
/// search for the nearest `\n`.
pub fn line_aligned_ranges(data: &[u8], start: usize, n: usize) -> Vec<(usize, usize)> {
    let len = data.len();
    if len <= start || n == 0 {
        return Vec::new();
    }
    let span = len - start;
    let n = n.min(span).max(1);

    let mut boundaries = vec![start];
    for i in 1..n {
        let target = start + span * i / n;
        let b = match data[target..].iter().position(|&b| b == b'\n') {
            Some(idx) => target + idx + 1,
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

/// Parse a single unsigned integer starting at `buf[i]` (an ASCII
/// digit), returning the value and the position right after its last
/// digit. No overflow checking: every field in this track's format
/// fits comfortably in a `u32`.
#[inline]
pub fn parse_uint(buf: &[u8], mut i: usize) -> (u32, usize) {
    let mut val: u32 = 0;
    while i < buf.len() && buf[i].is_ascii_digit() {
        val = val * 10 + (buf[i] - b'0') as u32;
        i += 1;
    }
    (val, i)
}

#[inline]
pub fn skip_non_digits(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && !buf[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// Parse every `s t` pair in `buf` into a `Vec<(u32, u32)>`, preserving
/// input order (output must answer query line `i` on output line `i`).
pub fn parse_query_pairs(buf: &[u8]) -> Vec<(u32, u32)> {
    let count_hint = buf.iter().filter(|&&b| b == b'\n').count() + 1;
    let mut pairs = Vec::with_capacity(count_hint);

    let n = buf.len();
    let mut i = 0;
    while i < n {
        i = skip_non_digits(buf, i);
        if i >= n {
            break;
        }
        let (s, ni) = parse_uint(buf, i);
        i = skip_non_digits(buf, ni);
        if i >= n {
            break;
        }
        let (t, ni) = parse_uint(buf, i);
        i = ni;
        pairs.push((s, t));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query_pairs_in_order() {
        let pairs = parse_query_pairs(b"0 8\n1 2\n0 9\n9 3\n");
        assert_eq!(pairs, vec![(0, 8), (1, 2), (0, 9), (9, 3)]);
    }

    #[test]
    fn line_aligned_ranges_cover_whole_span_with_no_gap_or_overlap() {
        let data = b"5 6\n0 1 1\n1 2 2\n2 3 3\n3 4 4\n4 0 5\n0 2 6\n";
        let header_end = 4usize; // "5 6\n" is 4 bytes
        for n in [1, 2, 3, 7, 12] {
            let ranges = line_aligned_ranges(data, header_end, n);
            assert_eq!(ranges.first().map(|&(s, _)| s), Some(header_end), "n={n}");
            assert_eq!(ranges.last().map(|&(_, e)| e), Some(data.len()), "n={n}");
            for w in ranges.windows(2) {
                assert_eq!(w[0].1, w[1].0, "n={n}: gap or overlap between ranges");
            }
        }
    }

    #[test]
    fn line_aligned_ranges_never_splits_a_line() {
        let mut data = String::from("1000 10\n");
        for i in 0..10u32 {
            data.push_str(&format!("{} {} {}\n", i, (i + 1) % 10, i * 7 + 1));
        }
        let data = data.into_bytes();
        let header_end = data.iter().position(|&b| b == b'\n').unwrap() + 1;
        for n in [1, 2, 3, 7, 12] {
            let ranges = line_aligned_ranges(&data, header_end, n);
            let mut total_edges = 0usize;
            for &(start, end) in &ranges {
                let mut i = start;
                while i < end {
                    i = skip_non_digits(&data, i);
                    if i >= end {
                        break;
                    }
                    let (_, ni) = parse_uint(&data, i);
                    i = skip_non_digits(&data, ni);
                    let (_, ni) = parse_uint(&data, i);
                    i = skip_non_digits(&data, ni);
                    let (_, ni) = parse_uint(&data, i);
                    i = ni;
                    total_edges += 1;
                }
            }
            assert_eq!(total_edges, 10, "n={n}: edge count diverged across ranges");
        }
    }
}
