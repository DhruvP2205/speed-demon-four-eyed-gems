//! Split an in-memory CSV buffer into N record-aligned byte ranges
//! for parallel processing. A newline inside a quoted field is NOT a
//! record boundary, so this tracks the same quote-nesting state as
//! `csv_parser::CsvReader` (field-start-gated quote open, `""`
//! escape, and "trailing content after a closing quote is literal,
//! not a re-opened quote") rather than naively splitting on `\n`.
//!
//! A plain single-threaded pass over the WHOLE buffer, tracking
//! quote-nesting state from byte 0 to find every split point, was
//! measured costing 470-700ms by itself on the 204 MB `full` input --
//! dwarfing the two genuinely-parallel aggregation phases combined
//! (confirmed via temporary phase instrumentation): pure serial setup
//! cost paid before any of the 12 worker threads can start. This is
//! gone now; `record_aligned_ranges` below (hash-matched end to end
//! before the old sequential version was removed -- see PERF.md)
//! replaces it: find each of the n-1
//! target split points' nearest RAW `\n` (plain byte search, not
//! quote-aware, independent per target, parallel), speculatively
//! assume each snapped position really is a record start (true for
//! the overwhelming majority of targets -- most lines are ordinary,
//! not inside a multi-line quoted field), scan the resulting n chunks
//! in parallel each under that assumption, then do ONE cheap
//! sequential pass (O(n_chunks), not O(file size)) verifying each
//! chunk's assumed entry state against the previous chunk's REAL exit
//! state and re-scanning only the (rare -- 0 times on this dataset)
//! chunk whose assumption was wrong.

/// Find the offset (relative to `data`) of the next `"` byte, or
/// `None` if there isn't one. Used while in a quoted field, where a
/// `"` is the ONLY byte that matters -- `,` and `\n` are plain
/// literal content inside an open quote, so long runs of ordinary
/// quoted text (the common case: names, notes) skip in 32-byte
/// strides instead of one branch per byte.
#[cfg(target_arch = "x86_64")]
fn find_next_quote(data: &[u8]) -> Option<usize> {
    if is_x86_feature_detected!("avx2") {
        // SAFETY: guarded by the runtime feature check above.
        unsafe { find_next_quote_avx2(data) }
    } else {
        data.iter().position(|&b| b == b'"')
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn find_next_quote(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == b'"')
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_next_quote_avx2(data: &[u8]) -> Option<usize> {
    use std::arch::x86_64::*;
    let quote = _mm256_set1_epi8(b'"' as i8);
    let mut i = 0usize;
    while i + 32 <= data.len() {
        let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
        let eq = _mm256_cmpeq_epi8(chunk, quote);
        let mask = _mm256_movemask_epi8(eq) as u32;
        if mask != 0 {
            return Some(i + mask.trailing_zeros() as usize);
        }
        i += 32;
    }
    data[i..].iter().position(|&b| b == b'"').map(|off| i + off)
}

/// Find the offset (relative to `data`) of the next `"`, `,`, or `\n`
/// byte, or `None` if there isn't one. Used while NOT in a quoted
/// field, where any of these three can change state -- everything
/// else is plain literal content that only flips `at_field_start` to
/// false, so runs of ordinary unquoted content also skip in bulk.
#[cfg(target_arch = "x86_64")]
fn find_next_special(data: &[u8]) -> Option<usize> {
    if is_x86_feature_detected!("avx2") {
        // SAFETY: guarded by the runtime feature check above.
        unsafe { find_next_special_avx2(data) }
    } else {
        data.iter().position(|&b| matches!(b, b'"' | b',' | b'\n'))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn find_next_special(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| matches!(b, b'"' | b',' | b'\n'))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_next_special_avx2(data: &[u8]) -> Option<usize> {
    use std::arch::x86_64::*;
    let quote = _mm256_set1_epi8(b'"' as i8);
    let comma = _mm256_set1_epi8(b',' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let mut i = 0usize;
    while i + 32 <= data.len() {
        let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
        let any = _mm256_or_si256(
            _mm256_or_si256(
                _mm256_cmpeq_epi8(chunk, quote),
                _mm256_cmpeq_epi8(chunk, comma),
            ),
            _mm256_cmpeq_epi8(chunk, newline),
        );
        let mask = _mm256_movemask_epi8(any) as u32;
        if mask != 0 {
            return Some(i + mask.trailing_zeros() as usize);
        }
        i += 32;
    }
    data[i..]
        .iter()
        .position(|&b| matches!(b, b'"' | b',' | b'\n'))
        .map(|off| i + off)
}

/// The entry state a chunk's scan can be handed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    FieldStart,
    MidField,
    InQuotes,
}

/// Result of scanning one chunk `[start, end)` under one `Entry`
/// hypothesis.
struct ChunkResult {
    exit_in_quotes: bool,
    exit_at_field_start: bool,
    /// Absolute position just past the first out-of-quote `\n` at or
    /// after `start`, if one was found within this chunk at all. Only
    /// meaningful -- and only read -- when this chunk's assumed entry
    /// state turns out to have been wrong (see
    /// `record_aligned_ranges`'s verify pass): the
    /// PRECEDING chunk's snapped boundary was actually inside a
    /// quoted field, so its real boundary is wherever THIS chunk's
    /// quote actually closes, not necessarily at this chunk's own
    /// snapped `end`.
    first_boundary: Option<usize>,
}

/// Scan one chunk `[start, end)` of `data` under a single `Entry`
/// hypothesis, tracking the same field-start-gated quote FSM as
/// `csv_parser::CsvReader`. Unlike a streamed/file-backed version,
/// this operates directly on an in-memory slice (the whole mmap, not
/// just this chunk) -- so resolving whether a `"` at or near `end` is
/// a closing quote or the first half of a `""` escape pair is just an
/// ordinary bounds-checked index into `data`, never a separate read:
/// there is no "ambiguous tail byte, defer to the next chunk" case to
/// handle at all, unlike a design that streams bounded blocks off a
/// file descriptor one range at a time.
fn scan_chunk(data: &[u8], start: usize, end: usize, entry: Entry) -> ChunkResult {
    let mut in_quotes = entry == Entry::InQuotes;
    let mut at_field_start = entry == Entry::FieldStart;
    let mut first_boundary: Option<usize> = None;
    if start >= end {
        return ChunkResult {
            exit_in_quotes: in_quotes,
            exit_at_field_start: at_field_start,
            first_boundary: None,
        };
    }

    let mut i = start;
    while i < end {
        if in_quotes {
            match find_next_quote(&data[i..end]) {
                Some(off) => i += off,
                None => break, // no closing quote within this chunk; carries into exit_in_quotes
            }
            // Always safe: `data` covers the whole mmap, not just
            // this chunk, so a lookahead past `end` is an ordinary
            // read of already-resident, read-only bytes.
            if i + 1 < data.len() && data[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_quotes = false;
            at_field_start = false;
            i += 1;
            continue;
        }
        match find_next_special(&data[i..end]) {
            Some(off) => {
                if off > 0 {
                    at_field_start = false;
                }
                i += off;
            }
            None => {
                at_field_start = false;
                break;
            }
        }
        let b = data[i];
        if b == b',' {
            at_field_start = true;
            i += 1;
            continue;
        }
        if b == b'\n' {
            if first_boundary.is_none() {
                first_boundary = Some(i + 1);
            }
            at_field_start = true;
            i += 1;
            continue;
        }
        // b == b'"' here (find_next_special only stops at '"', ',', or '\n')
        if at_field_start {
            in_quotes = true;
            at_field_start = false;
        }
        // else: stray '"' not at a field start, or trailing content
        // after a closing quote -- plain literal content, same as any
        // other byte.
        i += 1;
    }
    ChunkResult {
        exit_in_quotes: in_quotes,
        exit_at_field_start: at_field_start,
        first_boundary,
    }
}

/// Find the position right after the next RAW `\n` byte at or after
/// `offset` -- a plain literal byte search, deliberately NOT
/// quote-aware. Used only to optimistically guess that the nearest
/// newline to each target offset really is a record terminator,
/// before paying for any quote-tracking at all.
fn next_raw_newline(data: &[u8], offset: usize) -> usize {
    if offset >= data.len() {
        return data.len();
    }
    match data[offset..].iter().position(|&b| b == b'\n') {
        Some(idx) => offset + idx + 1,
        None => data.len(),
    }
}

/// The boundary-finding design used by `main.rs` -- see the module
/// doc above: turns what was an O(file size) single-threaded pass
/// into n independent, parallel per-chunk scans plus one O(n)
/// sequential verify pass.
///
/// First SNAP each of the n-1 target offsets forward to the position
/// right after the nearest RAW `\n` (`next_raw_newline`, no quote
/// tracking, independent per target). THAT position -- not the raw
/// offset -- is the single speculative guess: assume the nearest
/// newline really is a record terminator, true far more often than
/// "this arbitrary byte position starts a field," since most lines
/// are ordinary (non-quoted-multiline) records.
///
/// Each resulting chunk is scanned ONCE, in parallel, assuming
/// `Entry::FieldStart` at its (snapped) start. During the sequential
/// verify pass: when that assumption is TRUSTED (confirmed by the
/// previous chunk's real exit state), only the chunk's own `end`
/// matters -- not in a quote there means that specific snapped
/// newline really was a terminator. When NOT trusted, the chunk is
/// re-scanned for real under the correct entry state; `first_boundary`
/// (wherever the actual first out-of-quote `\n` falls, not necessarily
/// at this chunk's own `end`) resolves the boundary the PRECEDING
/// (wrongly-guessed) chunk should have produced, and `end` is still
/// checked independently afterward since it may ALSO now be valid.
pub fn record_aligned_ranges(data: &[u8], n: usize) -> Vec<(usize, usize)> {
    let len = data.len();
    if len == 0 || n == 0 {
        return Vec::new();
    }
    let n = n.min(len).max(1);
    let raw_targets: Vec<usize> = (1..n).map(|i| len * i / n).collect();

    // Search from `off - 1`, not `off`: a newline sitting at position
    // `off - 1` (so the next record starts exactly at `off`) already
    // satisfies target `off` -- searching from `off` itself would
    // skip past that newline and over-snap to a later one.
    let snapped: Vec<usize> = std::thread::scope(|scope| {
        let handles: Vec<_> = raw_targets
            .iter()
            .map(|&off| scope.spawn(move || next_raw_newline(data, off.saturating_sub(1))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("boundary snap worker panicked"))
            .collect()
    });

    let mut offsets = vec![0usize];
    offsets.extend(snapped);
    offsets.push(len);
    offsets.dedup();
    let n_chunks = offsets.len() - 1;

    let speculative: Vec<ChunkResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n_chunks)
            .map(|idx| {
                let (start, end) = (offsets[idx], offsets[idx + 1]);
                scope.spawn(move || scan_chunk(data, start, end, Entry::FieldStart))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("speculative scan worker panicked"))
            .collect()
    });

    let mut boundaries = vec![0usize];
    let mut entry = Entry::FieldStart; // byte 0 is always a genuine field start
    for idx in 0..n_chunks {
        let (start, end) = (offsets[idx], offsets[idx + 1]);
        let trusted = entry == Entry::FieldStart;
        let reparsed;
        let result: &ChunkResult = if trusted {
            &speculative[idx]
        } else {
            reparsed = scan_chunk(data, start, end, entry);
            &reparsed
        };
        if !trusted {
            // The PRECEDING chunk's snapped boundary was wrong -- its
            // real boundary, wherever it actually falls, is resolved
            // here via first_boundary.
            if let Some(real_boundary) = result.first_boundary {
                boundaries.push(real_boundary);
            }
        }
        // `end` is always right after a raw '\n' (by construction),
        // so not being in a quote there means that specific newline
        // really was an out-of-quote terminator.
        if !result.exit_in_quotes {
            boundaries.push(end);
        }
        entry = if result.exit_in_quotes {
            Entry::InQuotes
        } else if result.exit_at_field_start {
            Entry::FieldStart
        } else {
            Entry::MidField
        };
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
    use crate::csv_parser::CsvReader;

    fn reconstruct_record_count(
        f: impl Fn(&[u8], usize) -> Vec<(usize, usize)>,
        data: &[u8],
        n: usize,
    ) -> usize {
        f(data, n)
            .iter()
            .map(|&(s, e)| CsvReader::new(&data[s..e]).count())
            .sum()
    }

    /// Every record counted exactly once, for every chunk count tried
    /// -- the one invariant hash-matching ultimately depends on. Does
    /// NOT check that a particular target offset produces a
    /// particular boundary: the speculative design can legitimately
    /// keep or skip a given valid boundary depending on how targets
    /// happen to snap, so the only thing worth asserting is that the
    /// resulting partition is complete and correct, not that it's
    /// any ONE specific partition.
    fn assert_matches_baseline(data: &[u8], expected: usize) {
        for n in [1, 2, 3, 7, 12] {
            assert_eq!(
                reconstruct_record_count(record_aligned_ranges, data, n),
                expected,
                "n={n}"
            );
        }
    }

    #[test]
    fn splitting_never_changes_the_total_record_count() {
        let mut data = Vec::new();
        for i in 0..200 {
            if i % 7 == 0 {
                data.extend_from_slice(
                    format!("{i},\"a,b\",\"note\nwith\nlines\",{i},1.00\n").as_bytes(),
                );
            } else {
                data.extend_from_slice(format!("{i},plain,note,{i},1.00\n").as_bytes());
            }
        }
        let baseline = CsvReader::new(&data).count();
        assert_matches_baseline(&data, baseline);
    }

    #[test]
    fn a_newline_inside_a_quoted_field_is_never_treated_as_a_boundary() {
        // One giant quoted field spanning most of the buffer: no matter how many
        // ways we ask to split it, every split point must land outside the quote.
        let mut data = b"1,\"".to_vec();
        for i in 0..50 {
            data.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        data.extend_from_slice(b"\",1,1.00\n2,x,y,2,2.00\n");
        let baseline = CsvReader::new(&data).count();
        assert_matches_baseline(&data, baseline);
    }

    /// The case that actually exercises the speculative design's hard
    /// part: a single record with a large quoted field containing
    /// several embedded newlines, sized and positioned so that for at
    /// least one n, a target split point lands strictly inside the
    /// quoted field (on one of ITS embedded newlines) rather than
    /// between records -- the exact scenario the snap-then-verify
    /// logic exists to catch and correct.
    #[test]
    fn chunk_boundaries_inside_a_multiline_quoted_field_are_handled_correctly() {
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }
        let long_field_start = data.len();
        data.extend_from_slice(b"999,\"");
        for i in 0..200 {
            data.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        data.extend_from_slice(b"end\",999\n");
        let long_field_end = data.len();
        for i in 1000..1005 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }

        let total_len = data.len();
        let mut any_target_inside_quoted_field = false;
        for n in [1, 2, 3, 7, 12] {
            for i in 1..n {
                let target = total_len * i / n;
                if target > long_field_start && target < long_field_end {
                    any_target_inside_quoted_field = true;
                }
            }
        }
        assert!(
            any_target_inside_quoted_field,
            "fixture doesn't stress a split point inside the quoted field"
        );

        let baseline = CsvReader::new(&data).count();
        assert_eq!(baseline, 11);
        assert_matches_baseline(&data, baseline);
    }

    /// Adversarial: a quoted field whose content, after an embedded
    /// newline, looks exactly like a plausible CSV record. Rules out
    /// any accidental "does the text after this newline look record-
    /// shaped" heuristic -- only quote-nesting state should matter.
    #[test]
    fn fake_record_lookalike_inside_quoted_field_is_not_mistaken_for_a_boundary() {
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }
        let lookalike_start = data.len();
        data.extend_from_slice(b"999,\"before\n4,\"\"Smith\"\",\"\"x\"\",3,1.00\nafter\",999\n");
        let lookalike_end = data.len();
        for i in 1000..1005 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }

        let total_len = data.len();
        let mut any_target_inside = false;
        for n in [1, 2, 3, 7, 12] {
            for i in 1..n {
                let target = total_len * i / n;
                if target > lookalike_start && target < lookalike_end {
                    any_target_inside = true;
                }
            }
        }
        assert!(
            any_target_inside,
            "fixture doesn't stress a split point inside the lookalike text"
        );

        let baseline = CsvReader::new(&data).count();
        assert_eq!(baseline, 11);
        assert_matches_baseline(&data, baseline);
    }

    /// Adversarial: many `""` escape pairs interleaved with embedded
    /// newlines, so some target offset lands ON a byte of a `""` pair
    /// itself, with a real newline nearby too -- catches an off-by-one
    /// in the escape-skip logic.
    #[test]
    fn doubled_quotes_straddling_a_target_offset_are_handled_correctly() {
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }
        let quoted_start = data.len();
        data.extend_from_slice(b"999,\"");
        for _ in 0..500 {
            data.extend_from_slice(b"a\"\"\n");
        }
        data.extend_from_slice(b"end\",999\n");
        let quoted_end = data.len();
        for i in 1000..1005 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }

        let total_len = data.len();
        let mut any_target_on_a_quote_byte = false;
        for n in [1, 2, 3, 7, 12] {
            for i in 1..n {
                let target = total_len * i / n;
                if target > quoted_start && target < quoted_end && data[target] == b'"' {
                    any_target_on_a_quote_byte = true;
                }
            }
        }
        assert!(
            any_target_on_a_quote_byte,
            "fixture doesn't land a split point on a '\"' byte"
        );

        let baseline = CsvReader::new(&data).count();
        assert_eq!(baseline, 11);
        assert_matches_baseline(&data, baseline);
    }

    /// Adversarial: a `""` escape pair's two bytes straddle exactly
    /// where a chunk boundary would land (engineered for n=2), with
    /// embedded real newlines right after -- the case that would catch
    /// a cross-chunk lookahead bug (resolving the pair using the wrong
    /// byte, or failing to look past the chunk's own `end` at all).
    #[test]
    fn escaped_quote_pair_straddling_a_chunk_boundary_is_handled_correctly() {
        let k = 100usize;
        let mut data = Vec::new();
        data.extend_from_slice(b"1,\"");
        for _ in 0..k {
            data.push(b'x');
        }
        let pair_first_half = data.len();
        data.extend_from_slice(b"\"\"");
        let victim_start = data.len();
        data.extend_from_slice(b"line1\nline2\nline3");
        data.push(b'"');
        let victim_end = data.len();

        let target_total = 2 * (pair_first_half + 1);
        assert!(
            target_total >= data.len() + 2,
            "k too small for this padding scheme"
        );
        data.push(b',');
        for _ in 0..(target_total - data.len() - 1) {
            data.push(b'z');
        }
        data.push(b'\n');
        let total_len = data.len();
        assert_eq!(total_len, target_total);
        assert_eq!(data[pair_first_half], b'"');
        assert_eq!(data[pair_first_half + 1], b'"');
        assert_eq!(total_len / 2, pair_first_half + 1);

        let mut any_target_inside_victim = false;
        for n in [1, 2, 3, 7, 12] {
            for i in 1..n {
                let target = total_len * i / n;
                if target > victim_start && target < victim_end {
                    any_target_inside_victim = true;
                }
            }
        }
        assert!(
            any_target_inside_victim,
            "fixture doesn't land a split point inside the victim's quoted field"
        );

        let baseline = CsvReader::new(&data).count();
        assert_eq!(baseline, 1);
        assert_matches_baseline(&data, baseline);
    }

    /// Adversarial: an UNQUOTED field with a stray `"` not at the
    /// field's start (`a"b`), immediately followed by a genuinely
    /// quoted multi-line field. A finder that opens a quote on ANY
    /// `"` (not gated on field-start position) would wrongly treat the
    /// trigger as opening a quote, then misinterpret the victim's real
    /// opening `"` as a close -- splitting the victim's embedded
    /// newlines as if they were real boundaries.
    #[test]
    fn stray_quote_in_unquoted_field_does_not_open_a_phantom_quote() {
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }
        data.extend_from_slice(b"999,a\"b,999\n");
        let victim_start = data.len();
        data.extend_from_slice(b"1000,\"");
        for i in 0..200 {
            data.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        data.extend_from_slice(b"end\",1000\n");
        let victim_end = data.len();
        for i in 2000..2005 {
            data.extend_from_slice(format!("{i},short,{i}\n").as_bytes());
        }

        let total_len = data.len();
        let mut any_target_inside_victim = false;
        for n in [1, 2, 3, 7, 12] {
            for i in 1..n {
                let target = total_len * i / n;
                if target > victim_start && target < victim_end {
                    any_target_inside_victim = true;
                }
            }
        }
        assert!(
            any_target_inside_victim,
            "fixture doesn't land a split point inside the victim's quoted field"
        );

        let baseline = CsvReader::new(&data).count();
        assert_eq!(baseline, 12);
        assert_matches_baseline(&data, baseline);
    }

    #[test]
    fn empty_buffer_produces_no_ranges() {
        assert_eq!(record_aligned_ranges(b"", 12), Vec::new());
    }

    #[test]
    fn ranges_cover_the_whole_buffer_with_no_gap_or_overlap() {
        let data = b"1,2,3,4,5\n".repeat(500);
        for n in [1, 2, 3, 7, 12] {
            let ranges = record_aligned_ranges(&data, n);
            let mut expected_start = 0;
            for &(start, end) in &ranges {
                assert_eq!(start, expected_start, "n={n}");
                assert!(end > start, "n={n}");
                expected_start = end;
            }
            assert_eq!(expected_start, data.len(), "n={n}");
        }
    }
}
