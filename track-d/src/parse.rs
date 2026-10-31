//! Parallel parsing of `graph.txt`/`queries.txt` into compact `u32`
//! arrays. Every line is a fixed, small number of whitespace-separated
//! uint32 fields with no quoting and no possibility of an embedded
//! newline, so the next `\n` is always a safe split point.

use std::fs::File;
use std::io;

#[cfg(unix)]
pub fn pread_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
pub fn pread_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

const PROBE_SIZE: usize = 4096; // comfortably larger than any one generated line

/// First `\n` at or after `offset`, returned as the start of the next line.
fn next_line_start(file: &File, offset: u64, file_len: u64) -> u64 {
    if offset >= file_len {
        return file_len;
    }
    let mut probe = vec![0u8; PROBE_SIZE];
    let mut pos = offset;
    loop {
        let want = PROBE_SIZE.min((file_len - pos) as usize);
        if want == 0 {
            return file_len;
        }
        let n = pread_at(file, &mut probe[..want], pos).expect("probe read");
        if let Some(idx) = probe[..n].iter().position(|&b| b == b'\n') {
            return pos + idx as u64 + 1;
        }
        pos += n as u64;
        if n < want {
            return file_len; // EOF without a '\n' (no trailing newline)
        }
    }
}

/// Split `[start, file_len)` into up to `n` line-aligned, non-overlapping
/// ranges covering that span with no line split across a boundary.
pub fn line_aligned_ranges(file: &File, start: u64, file_len: u64, n: usize) -> Vec<(u64, u64)> {
    if file_len <= start || n == 0 {
        return Vec::new();
    }
    let span = file_len - start;
    let n = n.min(span as usize).max(1);
    let base = span / n as u64;

    let mut boundaries = vec![start];
    for i in 1..n {
        let candidate = start + base * i as u64;
        boundaries.push(next_line_start(file, candidate, file_len));
    }
    boundaries.push(file_len);
    boundaries.dedup();

    let mut ranges = Vec::with_capacity(boundaries.len().saturating_sub(1));
    for w in boundaries.windows(2) {
        if w[1] > w[0] {
            ranges.push((w[0], w[1]));
        }
    }
    ranges
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

/// Parse every `u v w` triple in `buf` into three parallel arrays,
/// pre-sized from a newline count so none of the three `Vec`s grows
/// and reallocates mid-parse.
pub fn parse_edge_triples(buf: &[u8]) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let count_hint = buf.iter().filter(|&&b| b == b'\n').count() + 1;
    let mut us = Vec::with_capacity(count_hint);
    let mut vs = Vec::with_capacity(count_hint);
    let mut ws = Vec::with_capacity(count_hint);

    let n = buf.len();
    let mut i = 0;
    while i < n {
        i = skip_non_digits(buf, i);
        if i >= n {
            break;
        }
        let (u, ni) = parse_uint(buf, i);
        i = skip_non_digits(buf, ni);
        let (v, ni) = parse_uint(buf, i);
        i = skip_non_digits(buf, ni);
        let (w, ni) = parse_uint(buf, i);
        i = ni;
        us.push(u);
        vs.push(v);
        ws.push(w);
    }
    (us, vs, ws)
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn with_temp_file<R>(data: &[u8], f: impl FnOnce(&File, u64) -> R) -> R {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "track_d_parse_test_{}_{unique_id}.txt",
            std::process::id()
        ));
        {
            let mut f = File::create(&path).expect("create temp file");
            f.write_all(data).expect("write temp file");
        }
        let file = File::open(&path).expect("open temp file");
        let len = data.len() as u64;
        let result = f(&file, len);
        drop(file);
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn parses_simple_edge_triples() {
        let (us, vs, ws) = parse_edge_triples(b"0 1 5\n1 2 3\n2 0 7\n");
        assert_eq!(us, vec![0, 1, 2]);
        assert_eq!(vs, vec![1, 2, 0]);
        assert_eq!(ws, vec![5, 3, 7]);
    }

    #[test]
    fn parses_edge_triples_without_trailing_newline() {
        let (us, vs, ws) = parse_edge_triples(b"0 1 5\n1 2 3");
        assert_eq!(us, vec![0, 1]);
        assert_eq!(vs, vec![1, 2]);
        assert_eq!(ws, vec![5, 3]);
    }

    #[test]
    fn parses_large_weight_values() {
        let (us, vs, ws) = parse_edge_triples(b"0 1 4294967295\n");
        assert_eq!(us, vec![0]);
        assert_eq!(vs, vec![1]);
        assert_eq!(ws, vec![4_294_967_295u32]);
    }

    #[test]
    fn parses_query_pairs_in_order() {
        let pairs = parse_query_pairs(b"0 8\n1 2\n0 9\n9 3\n");
        assert_eq!(pairs, vec![(0, 8), (1, 2), (0, 9), (9, 3)]);
    }

    #[test]
    fn line_aligned_ranges_cover_whole_span_with_no_gap_or_overlap() {
        let data = b"5 6\n0 1 1\n1 2 2\n2 3 3\n3 4 4\n4 0 5\n0 2 6\n";
        with_temp_file(data, |file, len| {
            let header_end = 4u64; // "5 6\n" is 4 bytes
            for n in [1, 2, 3, 7, 12] {
                let ranges = line_aligned_ranges(file, header_end, len, n);
                assert_eq!(ranges.first().map(|&(s, _)| s), Some(header_end), "n={n}");
                assert_eq!(ranges.last().map(|&(_, e)| e), Some(len), "n={n}");
                for w in ranges.windows(2) {
                    assert_eq!(w[0].1, w[1].0, "n={n}: gap or overlap between ranges");
                }
            }
        });
    }

    #[test]
    fn parsing_across_line_aligned_ranges_reconstructs_every_edge() {
        let mut data = String::from("1000 10\n");
        for i in 0..10u32 {
            data.push_str(&format!("{} {} {}\n", i, (i + 1) % 10, i * 7 + 1));
        }
        let data = data.into_bytes();
        with_temp_file(&data, |file, len| {
            let header_end = data.iter().position(|&b| b == b'\n').unwrap() as u64 + 1;
            for n in [1, 2, 3, 7, 12] {
                let ranges = line_aligned_ranges(file, header_end, len, n);
                let mut total_edges = 0usize;
                for &(start, end) in &ranges {
                    let mut buf = vec![0u8; (end - start) as usize];
                    pread_at(file, &mut buf, start).expect("read range");
                    let (us, _, _) = parse_edge_triples(&buf);
                    total_edges += us.len();
                }
                assert_eq!(total_edges, 10, "n={n}: edge count diverged across ranges");
            }
        });
    }
}
