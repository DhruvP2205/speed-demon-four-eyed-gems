//! Builds postings for exactly the terms `query_parse.rs` found in
//! `queries.txt` -- never a full inverted index over the corpus's
//! whole vocabulary (spec section 3: "indexing only the terms the
//! queries use is a legitimate strategy"). Every document still has
//! to be tokenized once (there's no way to know a token matches a
//! needed term without looking at it), but only matching tokens are
//! ever stored.
//!
//! Only one kind of posting is stored: PRESENCE (every needed term)
//! -- which documents contain it at least once, what BOOL queries
//! need directly and what PHRASE queries use to find candidate
//! documents (intersect each phrase term's presence list, rarest
//! first). PHRASE queries never get a stored per-document token
//! POSITION list: an earlier version kept one (every (doc, position)
//! occurrence of every phrase term), and massif showed it as 62% of
//! peak heap (positions Vec growth in `scan_docs` plus the final
//! scattered array -- see PERF.md) for a corpus where the actual
//! phrase-candidate set, after intersecting presence lists, is a
//! tiny fraction of all documents. Instead, `doc_offsets` records
//! only each document's absolute byte range in the mmap'd corpus, so
//! a phrase match can re-tokenize just the few candidates directly
//! from the mapping (`eval.rs`) -- bounded to candidates, never a
//! stored copy of any document's text.

use std::collections::HashMap;

use crate::fnv_hasher::FnvBuildHasher;
use crate::tokenize::for_each_token;

#[inline]
fn parse_doc_id(bytes: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = acc * 10 + (b - b'0') as u32;
    }
    acc
}

struct ThreadPostings {
    /// `(term_id, doc_id)`, deduped per doc WITHIN this thread (a term
    /// repeated many times in one document only needs one entry for
    /// presence) -- never duplicated ACROSS threads either, since
    /// every document belongs to exactly one thread's byte range.
    /// Pushed in increasing `doc_id` order (documents are scanned in
    /// file order within a thread's range) -- `build_index`'s scatter
    /// step relies on this to avoid a comparison sort entirely.
    presence: Vec<(u32, u32)>,
    /// Per-term counts, built alongside `presence` at no extra
    /// scanning cost -- lets `build_index` compute every term's
    /// global offset by summing counts, with no need to look at the
    /// (potentially tens of millions of) entries themselves until the
    /// O(n) scatter.
    presence_counts: Vec<u32>,
    /// `(doc_id, start, end)`: each document's absolute byte range
    /// `data[start..end]` for its tab-delimited text field (not
    /// including the `doc_id\t` prefix). Pushed in increasing
    /// `doc_id` order, same as `presence` -- `build_index` places
    /// each entry directly by `doc_id` into the final flat table, no
    /// comparison sort needed here either.
    doc_offsets: Vec<(u32, u32, u32)>,
}

/// Scans every document in `ranges` (one thread per range), looking
/// up each token against `term_lookup` (shared, read-only). Returns
/// one `ThreadPostings` per range, in the SAME order as `ranges`
/// (i.e. ascending doc id) -- `build_index` depends on this order.
fn scan_docs(
    data: &[u8],
    ranges: &[(usize, usize)],
    term_lookup: &HashMap<&[u8], u32, FnvBuildHasher>,
) -> Vec<ThreadPostings> {
    let num_terms = term_lookup.len();
    std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let mut presence = Vec::new();
                    let mut presence_counts = vec![0u32; num_terms];
                    let mut doc_offsets = Vec::new();
                    // Direct-indexed, not a HashMap: `term_id` is already
                    // a dense 0..num_terms integer, so tracking "the last
                    // doc_id this term was recorded for, in THIS thread"
                    // is a plain array lookup, no hashing.
                    let mut last_doc_for_term = vec![u32::MAX; num_terms];
                    let mut buf = [0u8; crate::tokenize::MAX_TOKEN_LEN];

                    let mut pos = start;
                    while pos < end {
                        let line_end = data[pos..end]
                            .iter()
                            .position(|&b| b == b'\n')
                            .map(|i| pos + i)
                            .unwrap_or(end);
                        let line = &data[pos..line_end];
                        if !line.is_empty() {
                            let tab = line
                                .iter()
                                .position(|&b| b == b'\t')
                                .expect("doc line missing TAB");
                            let doc_id = parse_doc_id(&line[..tab]);
                            let text_start = pos + tab + 1;
                            let text = &data[text_start..line_end];
                            doc_offsets.push((doc_id, text_start as u32, line_end as u32));

                            for_each_token(text, &mut buf, |_position, token| {
                                let Some(&term_id) = term_lookup.get(token) else {
                                    return; // not a term any query needs -- discard immediately
                                };
                                if last_doc_for_term[term_id as usize] != doc_id {
                                    presence.push((term_id, doc_id));
                                    presence_counts[term_id as usize] += 1;
                                    last_doc_for_term[term_id as usize] = doc_id;
                                }
                            });
                        }
                        pos = line_end + 1;
                    }
                    ThreadPostings {
                        presence,
                        presence_counts,
                        doc_offsets,
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("doc scan worker panicked"))
            .collect()
    })
}

/// The final, queryable index: for each `term_id`, a sorted-ascending
/// slice of the doc ids containing it (`presence`, ready for BOOL's
/// sorted-list AND/OR/NOT and for PHRASE's candidate intersection),
/// plus each document's absolute byte range in the mmap'd corpus
/// (`doc_offsets`, for PHRASE's re-tokenize-the-candidate check).
pub struct Index {
    presence: Vec<u8>,
    presence_offsets: Vec<(u32, u32)>,
    doc_offsets: Vec<(u32, u32)>,
}

impl Index {
    #[inline]
    pub fn presence_of(&self, term_id: u32) -> Vec<u32> {
        let (start, end) = self.presence_offsets[term_id as usize];
        let bytes = &self.presence[start as usize..end as usize];
        let mut out = Vec::new();
        let mut previous = 0u32;
        let mut value = 0u32;
        let mut shift = 0;
        for &byte in bytes {
            value |= ((byte & 0x7f) as u32) << shift;
            if byte & 0x80 == 0 {
                previous += value;
                out.push(previous);
                shift = 0;
                value = 0;
            } else {
                shift += 7;
            }
        }
        debug_assert_eq!(shift, 0);
        out
    }

    /// The raw text bytes (after the `doc_id\t` prefix) of `doc_id`,
    /// sliced directly from the mmap'd corpus `data` -- never copied.
    #[inline]
    pub fn doc_text<'a>(&self, data: &'a [u8], doc_id: u32) -> &'a [u8] {
        let (start, end) = self.doc_offsets[doc_id as usize];
        &data[start as usize..end as usize]
    }
}

/// Builds the queryable `Index` from `docs.txt`'s mmap'd bytes and the
/// term dictionary `query_parse.rs` already extracted.
///
/// No comparison sort anywhere: each thread already pushes its own
/// `presence`/`doc_offsets` entries in ascending `doc_id` order
/// (documents are scanned in file order within one thread's range),
/// and `ranges` itself is in ascending doc-id order across threads --
/// so concatenating thread 0's, then thread 1's, ... entries for a
/// given term (or for the doc-id-indexed table) is ALREADY fully
/// sorted. This turns the merge into a classic O(n) counting-sort
/// scatter for `presence`, and a plain direct-index placement for
/// `doc_offsets`: sum each thread's per-term counts into one global
/// histogram, prefix-sum it into offsets, then copy each thread's
/// entries (in thread order) into their term's slot, advancing a
/// per-term cursor. An earlier version concatenated everything into
/// one big `Vec` and called `sort_unstable` on ~47M entries at `full`
/// scale -- measured costing 1.81s of a 2.67s total build, by far the
/// dominant cost (see PERF.md); this scatter does the same job in a
/// single linear pass.
pub fn build_index(data: &[u8], ranges: &[(usize, usize)], term_bytes: &[Vec<u8>]) -> Index {
    let num_terms = term_bytes.len();
    let term_lookup: HashMap<&[u8], u32, FnvBuildHasher> = term_bytes
        .iter()
        .enumerate()
        .map(|(id, bytes)| (bytes.as_slice(), id as u32))
        .collect();

    let threads = scan_docs(data, ranges, &term_lookup);

    let mut presence_offsets = vec![0u32; num_terms + 1];
    for t in &threads {
        for term in 0..num_terms {
            presence_offsets[term + 1] += t.presence_counts[term];
        }
    }
    for term in 0..num_terms {
        presence_offsets[term + 1] += presence_offsets[term];
    }
    let total_presence = presence_offsets[num_terms] as usize;

    // Exact size: one slot per document, computed from the highest
    // doc_id actually seen (never assumes a literal dataset size).
    let num_docs = threads
        .iter()
        .filter_map(|t| t.doc_offsets.last())
        .map(|&(id, _, _)| id)
        .max()
        .map_or(0, |max_id| max_id as usize + 1);

    let mut presence_docs = vec![0u32; total_presence];
    let mut doc_offsets = vec![(0u32, 0u32); num_docs];
    let mut presence_cursor = presence_offsets.clone();

    // Consumes `threads` (by value, not `&threads`): each `ThreadPostings`
    // -- including its own, separate copy of up to several hundred MB of
    // `presence` entries at `full` scale -- is dropped as soon as ITS
    // contribution is scattered, instead of all 12 staying alive for the
    // whole loop alongside the final arrays they're being copied into.
    // Measured cutting real cgroup `memory.peak` by roughly a third (see
    // PERF.md).
    for t in threads {
        for (term_id, doc_id) in t.presence {
            let pos = presence_cursor[term_id as usize] as usize;
            presence_docs[pos] = doc_id;
            presence_cursor[term_id as usize] += 1;
        }
        for (doc_id, start, end) in t.doc_offsets {
            doc_offsets[doc_id as usize] = (start, end);
        }
    }

    let mut byte_offsets = vec![0u32; num_terms + 1];
    for term in 0..num_terms {
        let mut previous = 0u32;
        for &doc_id in
            &presence_docs[presence_offsets[term] as usize..presence_offsets[term + 1] as usize]
        {
            let mut delta = doc_id - previous;
            loop {
                byte_offsets[term + 1] += 1;
                if delta < 128 {
                    break;
                }
                delta >>= 7;
            }
            previous = doc_id;
        }
        byte_offsets[term + 1] += byte_offsets[term];
    }

    let mut compressed_presence = vec![0u8; byte_offsets[num_terms] as usize];
    for term in 0..num_terms {
        let mut previous = 0u32;
        let mut pos = byte_offsets[term] as usize;
        for &doc_id in
            &presence_docs[presence_offsets[term] as usize..presence_offsets[term + 1] as usize]
        {
            let mut delta = doc_id - previous;
            loop {
                let mut byte = (delta & 0x7f) as u8;
                delta >>= 7;
                if delta != 0 {
                    byte |= 0x80;
                }
                compressed_presence[pos] = byte;
                pos += 1;
                if delta == 0 {
                    break;
                }
            }
            previous = doc_id;
        }
    }

    Index {
        presence: compressed_presence,
        presence_offsets: byte_offsets.windows(2).map(|w| (w[0], w[1])).collect(),
        doc_offsets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::line_aligned_ranges;

    fn build(doc_lines: &[&str], terms: &[&str]) -> (Index, Vec<u8>) {
        let data: Vec<u8> = doc_lines
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect();
        let term_bytes: Vec<Vec<u8>> = terms.iter().map(|t| t.as_bytes().to_vec()).collect();
        let ranges = line_aligned_ranges(&data, 12);
        let index = build_index(&data, &ranges, &term_bytes);
        (index, data)
    }

    #[test]
    fn presence_lists_are_sorted_ascending_and_deduped_per_doc() {
        let docs = ["0\tbail bond bail", "1\tbail", "2\tbond"];
        let (idx, _data) = build(&docs, &["bail", "bond"]);
        assert_eq!(idx.presence_of(0), vec![0, 1]); // "bail": doc 0 once despite appearing twice
        assert_eq!(idx.presence_of(1), vec![0, 2]); // "bond"
    }

    #[test]
    fn an_absent_term_has_an_empty_presence_list() {
        let docs = ["0\tbail"];
        let (idx, _data) = build(&docs, &["bail", "nothere"]);
        assert_eq!(idx.presence_of(1), Vec::<u32>::new());
    }

    #[test]
    fn doc_text_slices_exactly_the_tab_delimited_field() {
        let docs = ["0\tbail bond bail", "1\tbail"];
        let (idx, data) = build(&docs, &["bail"]);
        assert_eq!(idx.doc_text(&data, 0), b"bail bond bail");
        assert_eq!(idx.doc_text(&data, 1), b"bail");
    }

    #[test]
    fn consistent_across_every_thread_count() {
        let docs: Vec<String> = (0..200u32)
            .map(|i| format!("{i}\tbail bond {i} writ corpus"))
            .collect();
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let term_bytes: Vec<Vec<u8>> = vec![b"bail".to_vec(), b"bond".to_vec(), b"writ".to_vec()];
        let data: Vec<u8> = doc_refs
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect();

        let baseline = build_index(&data, &line_aligned_ranges(&data, 1), &term_bytes);
        for n in [2, 3, 7, 12] {
            let idx = build_index(&data, &line_aligned_ranges(&data, n), &term_bytes);
            assert_eq!(idx.presence_of(0), baseline.presence_of(0), "n={n}");
            assert_eq!(idx.presence_of(1), baseline.presence_of(1), "n={n}");
            assert_eq!(idx.presence_of(2), baseline.presence_of(2), "n={n}");
            for doc in 0..200u32 {
                assert_eq!(
                    idx.doc_text(&data, doc),
                    baseline.doc_text(&data, doc),
                    "n={n} doc={doc}"
                );
            }
        }
    }
}
