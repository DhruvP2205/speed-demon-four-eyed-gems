//! Parallel, bounded-memory chunked file reading: split a file into
//! line-aligned byte ranges for N worker threads, then stream each
//! range in bounded sub-chunks so no thread ever holds the whole file
//! (or even one whole range) in memory at once.

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
                                // Bounds peak memory at MAX_THREADS * SUB_CHUNK_SIZE for the read buffers
                                // (12 MB at 1 MiB; this track's lines average ~77 bytes, so 128 KiB is still
                                // over 1,600 lines per read -- plenty of batching, an order of magnitude less
                                // memory).
const SUB_CHUNK_SIZE: usize = 128 * 1024;

/// Find the first `\n` at or after `offset`, returning the offset just
/// past it (i.e. the start of the next line). Reads small probe buffers
/// rather than the whole file -- used only to realign the handful of
/// thread-boundary candidates, not per-line.
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
            return file_len; // hit EOF without finding '\n' (e.g. no trailing newline)
        }
    }
}

/// Split `[0, file_len)` into up to `n` line-aligned, non-overlapping
/// ranges covering the whole file with no line split across a boundary
/// and none double-counted.
pub fn line_aligned_ranges(file: &File, file_len: u64, n: usize) -> Vec<(u64, u64)> {
    if file_len == 0 || n == 0 {
        return Vec::new();
    }
    let n = n.min(file_len as usize).max(1);
    let base = file_len / n as u64;

    let mut boundaries = vec![0u64];
    for i in 1..n {
        let candidate = base * i as u64;
        boundaries.push(next_line_start(file, candidate, file_len));
    }
    boundaries.push(file_len);
    boundaries.dedup();

    let mut ranges = Vec::with_capacity(boundaries.len() - 1);
    for w in boundaries.windows(2) {
        if w[1] > w[0] {
            ranges.push((w[0], w[1]));
        }
    }
    ranges
}

/// Stream `[start, end)` of `file` in bounded-size sub-chunks, calling
/// `on_line` for each complete line (without its trailing `\n`). A line
/// split across two sub-chunk reads is reassembled via a small carry
/// buffer, so correctness doesn't depend on sub-chunk size -- only peak
/// memory does (bounded to roughly SUB_CHUNK_SIZE + one line).
pub fn for_each_line_in_range<F: FnMut(&[u8])>(file: &File, start: u64, end: u64, mut on_line: F) {
    let mut pos = start;
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; SUB_CHUNK_SIZE];

    while pos < end {
        let want = SUB_CHUNK_SIZE.min((end - pos) as usize);
        let n = pread_at(file, &mut buf[..want], pos).expect("chunk read");
        if n == 0 {
            break;
        }
        pos += n as u64;

        carry.extend_from_slice(&buf[..n]);
        if let Some(idx) = carry.iter().rposition(|&b| b == b'\n') {
            for line in carry[..idx].split(|&b| b == b'\n') {
                on_line(line);
            }
            carry.drain(..=idx);
        }
    }
    if !carry.is_empty() {
        for line in carry.split(|&b| b == b'\n') {
            if !line.is_empty() {
                on_line(line);
            }
        }
    }
}
