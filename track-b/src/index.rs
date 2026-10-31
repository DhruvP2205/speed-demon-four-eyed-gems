//! Builds postings for exactly the terms `query_parse.rs` found in
//! `queries.txt` -- never a full inverted index over the corpus's
//! whole vocabulary (spec section 3: "indexing only the terms the
//! queries use is a legitimate strategy"). Every document still has
//! to be tokenized once (there's no way to know a token matches a
//! needed term without looking at it), but only matching tokens are
//! ever stored.
//!
//! Two kinds of postings, built in the same pass:
//! - PRESENCE (every needed term): which documents contain it at
//!   least once -- what BOOL queries need.
//! - POSITION (only terms used in >=1 PHRASE query): every (doc,
//!   position) occurrence -- what PHRASE queries need to verify
//!   consecutive terms without ever re-tokenizing a document.

use std::collections::HashMap;

use crate::fnv_hasher::FnvBuildHasher;
use crate::tokenize::{for_each_token, MAX_TOKEN_LEN};

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
    /// `(term_id, doc_id, position)`, one entry per occurrence, only
    /// for terms `needs_position` flags -- deliberately NOT deduped,
    /// phrase matching needs every occurrence. Also pushed in
    /// increasing `(doc_id, position)` order within this thread, for
    /// the same reason.
    positions: Vec<(u32, u32, u32)>,
    /// Per-term counts, built alongside `presence`/`positions` at no
    /// extra scanning cost -- lets `build_index` compute every term's
    /// global offset by summing counts, with no need to look at the
    /// (potentially tens of millions of) entries themselves until the
    /// O(n) scatter.
    presence_counts: Vec<u32>,
    position_counts: Vec<u32>,
}

/// Scans every document in `ranges` (one thread per range), looking
/// up each token against `term_lookup` (shared, read-only). Returns
/// one `ThreadPostings` per range, in the SAME order as `ranges`
/// (i.e. ascending doc id) -- `build_index` depends on this order.
fn scan_docs(
    data: &[u8],
    ranges: &[(usize, usize)],
    term_lookup: &HashMap<&[u8], u32, FnvBuildHasher>,
    needs_position: &[bool],
) -> Vec<ThreadPostings> {
    let num_terms = needs_position.len();
    std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                scope.spawn(move || {
                    let mut presence = Vec::new();
                    let mut positions = Vec::new();
                    let mut presence_counts = vec![0u32; num_terms];
                    let mut position_counts = vec![0u32; num_terms];
                    // Direct-indexed, not a HashMap: `term_id` is already
                    // a dense 0..num_terms integer, so tracking "the last
                    // doc_id this term was recorded for, in THIS thread"
                    // is a plain array lookup, no hashing.
                    let mut last_doc_for_term = vec![u32::MAX; num_terms];
                    let mut buf = [0u8; MAX_TOKEN_LEN];

                    for line in data[start..end].split(|&b| b == b'\n') {
                        if line.is_empty() {
                            continue;
                        }
                        let tab = line
                            .iter()
                            .position(|&b| b == b'\t')
                            .expect("doc line missing TAB");
                        let doc_id = parse_doc_id(&line[..tab]);
                        let text = &line[tab + 1..];

                        for_each_token(text, &mut buf, |position, token| {
                            let Some(&term_id) = term_lookup.get(token) else {
                                return; // not a term any query needs -- discard immediately
                            };
                            if needs_position[term_id as usize] {
                                positions.push((term_id, doc_id, position));
                                position_counts[term_id as usize] += 1;
                            }
                            if last_doc_for_term[term_id as usize] != doc_id {
                                presence.push((term_id, doc_id));
                                presence_counts[term_id as usize] += 1;
                                last_doc_for_term[term_id as usize] = doc_id;
                            }
                        });
                    }
                    ThreadPostings {
                        presence,
                        positions,
                        presence_counts,
                        position_counts,
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
/// sorted-list AND/OR/NOT), and a slice of `(doc_id, position)` pairs
/// sorted by `(doc_id, position)` (`positions`, ready for PHRASE's
/// per-doc position lookups) for terms that need it.
pub struct Index {
    presence: Vec<u32>,
    presence_offsets: Vec<(u32, u32)>,
    positions: Vec<(u32, u32)>,
    position_offsets: Vec<(u32, u32)>,
}

impl Index {
    #[inline]
    pub fn presence_of(&self, term_id: u32) -> &[u32] {
        let (start, end) = self.presence_offsets[term_id as usize];
        &self.presence[start as usize..end as usize]
    }

    /// All positions of `term_id` within `doc_id`, ascending -- empty
    /// if the term never needed positions or never occurs there.
    #[inline]
    pub fn positions_in_doc(&self, term_id: u32, doc_id: u32) -> &[(u32, u32)] {
        let (start, end) = self.position_offsets[term_id as usize];
        let slice = &self.positions[start as usize..end as usize];
        let lo = slice.partition_point(|&(d, _)| d < doc_id);
        let hi = lo + slice[lo..].partition_point(|&(d, _)| d == doc_id);
        &slice[lo..hi]
    }
}

/// Builds the queryable `Index` from `docs.txt`'s mmap'd bytes and the
/// term dictionary `query_parse.rs` already extracted.
///
/// No comparison sort anywhere: each thread already pushes its own
/// `presence`/`positions` entries in ascending `(doc_id)` /
/// `(doc_id, position)` order (documents are scanned in file order
/// within one thread's range), and `ranges` itself is in ascending
/// doc-id order across threads -- so concatenating thread 0's, then
/// thread 1's, ... entries for a given term is ALREADY fully sorted.
/// This turns the merge into a classic O(n) counting-sort scatter:
/// sum each thread's per-term counts into one global histogram,
/// prefix-sum it into offsets, then copy each thread's entries (in
/// thread order) into their term's slot, advancing a per-term cursor.
/// An earlier version concatenated everything into one big `Vec` and
/// called `sort_unstable` on ~47M entries at `full` scale -- measured
/// costing 1.81s of a 2.67s total build, by far the dominant cost
/// (see PERF.md); this scatter does the same job in a single linear
/// pass.
pub fn build_index(
    data: &[u8],
    ranges: &[(usize, usize)],
    term_bytes: &[Vec<u8>],
    needs_position: &[bool],
) -> Index {
    let num_terms = term_bytes.len();
    let term_lookup: HashMap<&[u8], u32, FnvBuildHasher> = term_bytes
        .iter()
        .enumerate()
        .map(|(id, bytes)| (bytes.as_slice(), id as u32))
        .collect();

    let threads = scan_docs(data, ranges, &term_lookup, needs_position);

    let mut presence_offsets = vec![0u32; num_terms + 1];
    let mut position_offsets = vec![0u32; num_terms + 1];
    for t in &threads {
        for term in 0..num_terms {
            presence_offsets[term + 1] += t.presence_counts[term];
            position_offsets[term + 1] += t.position_counts[term];
        }
    }
    for term in 0..num_terms {
        presence_offsets[term + 1] += presence_offsets[term];
        position_offsets[term + 1] += position_offsets[term];
    }
    let total_presence = presence_offsets[num_terms] as usize;
    let total_positions = position_offsets[num_terms] as usize;

    let mut presence_docs = vec![0u32; total_presence];
    let mut position_pairs = vec![(0u32, 0u32); total_positions];
    let mut presence_cursor = presence_offsets.clone();
    let mut position_cursor = position_offsets.clone();

    // Consumes `threads` (by value, not `&threads`): each `ThreadPostings`
    // -- including its own, separate copy of up to several hundred MB of
    // `presence`/`positions` entries at `full` scale -- is dropped as
    // soon as ITS contribution is scattered, instead of all 12 staying
    // alive for the whole loop alongside the final arrays they're being
    // copied into. Measured cutting real cgroup `memory.peak` by roughly
    // a third (see PERF.md).
    for t in threads {
        for (term_id, doc_id) in t.presence {
            let pos = presence_cursor[term_id as usize] as usize;
            presence_docs[pos] = doc_id;
            presence_cursor[term_id as usize] += 1;
        }
        for (term_id, doc_id, token_pos) in t.positions {
            let pos = position_cursor[term_id as usize] as usize;
            position_pairs[pos] = (doc_id, token_pos);
            position_cursor[term_id as usize] += 1;
        }
    }

    let presence_offset_pairs: Vec<(u32, u32)> =
        presence_offsets.windows(2).map(|w| (w[0], w[1])).collect();
    let position_offset_pairs: Vec<(u32, u32)> =
        position_offsets.windows(2).map(|w| (w[0], w[1])).collect();

    Index {
        presence: presence_docs,
        presence_offsets: presence_offset_pairs,
        positions: position_pairs,
        position_offsets: position_offset_pairs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::line_aligned_ranges;

    fn build(doc_lines: &[&str], terms: &[&str], needs_position: &[bool]) -> Index {
        let data: Vec<u8> = doc_lines
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect();
        let term_bytes: Vec<Vec<u8>> = terms.iter().map(|t| t.as_bytes().to_vec()).collect();
        let ranges = line_aligned_ranges(&data, 12);
        build_index(&data, &ranges, &term_bytes, needs_position)
    }

    #[test]
    fn presence_lists_are_sorted_ascending_and_deduped_per_doc() {
        let docs = ["0\tbail bond bail", "1\tbail", "2\tbond"];
        let idx = build(&docs, &["bail", "bond"], &[false, false]);
        assert_eq!(idx.presence_of(0), &[0, 1]); // "bail": doc 0 once despite appearing twice
        assert_eq!(idx.presence_of(1), &[0, 2]); // "bond"
    }

    #[test]
    fn an_absent_term_has_an_empty_presence_list() {
        let docs = ["0\tbail"];
        let idx = build(&docs, &["bail", "nothere"], &[false, false]);
        assert_eq!(idx.presence_of(1), &[] as &[u32]);
    }

    #[test]
    fn positions_are_recorded_for_every_occurrence_when_needed() {
        let docs = ["0\tbail bond bail"];
        let idx = build(&docs, &["bail"], &[true]);
        assert_eq!(idx.positions_in_doc(0, 0), &[(0, 0), (0, 2)]);
    }

    #[test]
    fn positions_are_per_document_not_mixed_across_documents() {
        let docs = ["0\tbail bond", "1\tbond bail"];
        let idx = build(&docs, &["bail"], &[true]);
        assert_eq!(idx.positions_in_doc(0, 0), &[(0, 0)]);
        assert_eq!(idx.positions_in_doc(0, 1), &[(1, 1)]);
    }

    #[test]
    fn a_term_not_needing_positions_has_an_empty_position_list_even_if_present() {
        let docs = ["0\tbail"];
        let idx = build(&docs, &["bail"], &[false]);
        assert_eq!(idx.positions_in_doc(0, 0), &[] as &[(u32, u32)]);
    }

    #[test]
    fn consistent_across_every_thread_count() {
        let docs: Vec<String> = (0..200u32)
            .map(|i| format!("{i}\tbail bond {i} writ corpus"))
            .collect();
        let doc_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let term_bytes: Vec<Vec<u8>> = vec![b"bail".to_vec(), b"bond".to_vec(), b"writ".to_vec()];
        let needs_position = vec![true, false, true];
        let data: Vec<u8> = doc_refs
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect();

        let baseline = build_index(
            &data,
            &line_aligned_ranges(&data, 1),
            &term_bytes,
            &needs_position,
        );
        for n in [2, 3, 7, 12] {
            let idx = build_index(
                &data,
                &line_aligned_ranges(&data, n),
                &term_bytes,
                &needs_position,
            );
            assert_eq!(idx.presence_of(0), baseline.presence_of(0), "n={n}");
            assert_eq!(idx.presence_of(1), baseline.presence_of(1), "n={n}");
            assert_eq!(idx.presence_of(2), baseline.presence_of(2), "n={n}");
            for doc in 0..200u32 {
                assert_eq!(
                    idx.positions_in_doc(0, doc),
                    baseline.positions_in_doc(0, doc),
                    "n={n} doc={doc}"
                );
            }
        }
    }
}
