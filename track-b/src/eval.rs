//! Evaluates BOOL and PHRASE queries against `Index`'s sorted
//! postings -- specs/b-index.md section 3. Every result set stays
//! sorted ascending throughout (merge-based set ops on already-sorted
//! inputs produce sorted output for free), so there's no separate
//! sort before formatting (section 4's smallest-10 and sum just read
//! straight off the front of the final `Vec<u32>`).

use crate::index::Index;
use crate::query_parse::{Op, Query};
use crate::tokenize::{for_each_token, MAX_TOKEN_LEN};

/// `R ∩ S(t)`: classic sorted-merge intersection, O(|a| + |b|).
fn and(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            i += 1;
        } else if a[i] > b[j] {
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out
}

/// `R ∪ S(t)`.
fn or(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] > b[j] {
            out.push(b[j]);
            j += 1;
        } else {
            out.push(a[i]);
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// `R \ S(t)`: documents of `R` that do NOT contain `t`.
fn not(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else if a[i] > b[j] {
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out
}

fn eval_bool(index: &Index, terms: &[u32], ops: &[Op]) -> Vec<u32> {
    let mut result = index.presence_of(terms[0]).to_vec();
    for (op, &term) in ops.iter().zip(&terms[1..]) {
        let postings = index.presence_of(term);
        result = match op {
            Op::And => and(&result, postings),
            Op::Or => or(&result, postings),
            Op::Not => not(&result, postings),
        };
    }
    result
}

/// Documents containing EVERY term in `terms` at `k` consecutive
/// positions, in that order (spec section 3's PHRASE rule). Never
/// stores a per-document token-position index (an earlier version
/// did -- it was 62% of peak heap, see PERF.md): candidates come from
/// intersecting each term's PRESENCE list (smallest first, to shrink
/// the working set fastest), then each candidate -- a tiny fraction
/// of all documents -- is verified by re-tokenizing its text directly
/// off the mmap (`index.doc_text`), the same exact-match semantics
/// the reference uses, just bounded to candidates instead of the
/// whole corpus.
fn eval_phrase(index: &Index, data: &[u8], terms: &[u32], term_bytes: &[Vec<u8>]) -> Vec<u32> {
    let mut term_order: Vec<usize> = (0..terms.len()).collect();
    term_order.sort_by_key(|&i| index.presence_of(terms[i]).len());

    let mut candidates = index.presence_of(terms[term_order[0]]).to_vec();
    for &i in &term_order[1..] {
        if candidates.is_empty() {
            return candidates;
        }
        candidates = and(&candidates, index.presence_of(terms[i]));
    }

    let k = terms.len();
    let mut buf = [0u8; MAX_TOKEN_LEN];
    // One allocation, reused (via `clear`, which keeps capacity)
    // across every candidate document -- never per token, never per
    // query. `position` is kept alongside each token's bytes because
    // a token longer than `MAX_TOKEN_LEN` is skipped by `for_each_token`
    // but still consumes a position: without the real position number,
    // two tokens either side of a skipped one would look falsely
    // adjacent.
    let mut doc_tokens: Vec<(u32, [u8; MAX_TOKEN_LEN], u8)> = Vec::new();
    let mut matches = Vec::new();

    for &doc_id in &candidates {
        let text = index.doc_text(data, doc_id);
        doc_tokens.clear();
        for_each_token(text, &mut buf, |position, token| {
            let mut stored = [0u8; MAX_TOKEN_LEN];
            stored[..token.len()].copy_from_slice(token);
            doc_tokens.push((position, stored, token.len() as u8));
        });

        'windows: for w in 0..doc_tokens.len() {
            let (start_pos, first_bytes, first_len) = doc_tokens[w];
            if first_bytes[..first_len as usize] != *term_bytes[terms[0] as usize] {
                continue;
            }
            if w + k > doc_tokens.len() {
                break;
            }
            for (t, term) in terms.iter().enumerate().skip(1) {
                let (pos, bytes, len) = doc_tokens[w + t];
                if pos != start_pos + t as u32 || bytes[..len as usize] != *term_bytes[*term as usize]
                {
                    continue 'windows;
                }
            }
            matches.push(doc_id);
            break;
        }
    }
    matches
}

pub fn eval(index: &Index, data: &[u8], term_bytes: &[Vec<u8>], query: &Query) -> Vec<u32> {
    match query {
        Query::Bool { terms, ops } => eval_bool(index, terms, ops),
        Query::Phrase { terms } => eval_phrase(index, data, terms, term_bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_intersects_sorted_slices() {
        assert_eq!(and(&[1, 2, 3, 5], &[2, 3, 4]), vec![2, 3]);
    }

    #[test]
    fn or_unions_sorted_slices_without_duplicates() {
        assert_eq!(or(&[1, 2, 5], &[2, 3, 5, 6]), vec![1, 2, 3, 5, 6]);
    }

    #[test]
    fn not_removes_the_second_slices_elements() {
        assert_eq!(not(&[1, 2, 3, 4], &[2, 4]), vec![1, 3]);
    }

    #[test]
    fn empty_inputs_are_handled_by_every_operator() {
        assert_eq!(and(&[], &[1, 2]), Vec::<u32>::new());
        assert_eq!(or(&[], &[1, 2]), vec![1, 2]);
        assert_eq!(not(&[1, 2], &[]), vec![1, 2]);
        assert_eq!(not(&[], &[1, 2]), Vec::<u32>::new());
    }

    fn build(doc_lines: &[&str], terms: &[&str]) -> (crate::index::Index, Vec<u8>, Vec<Vec<u8>>) {
        let data: Vec<u8> = doc_lines
            .iter()
            .flat_map(|l| l.bytes().chain(std::iter::once(b'\n')))
            .collect();
        let term_bytes: Vec<Vec<u8>> = terms.iter().map(|t| t.as_bytes().to_vec()).collect();
        let ranges = crate::chunked::line_aligned_ranges(&data, 12);
        let index = crate::index::build_index(&data, &ranges, &term_bytes);
        (index, data, term_bytes)
    }

    #[test]
    fn phrase_matches_consecutive_terms_in_order() {
        let (idx, data, term_bytes) = build(&["0\tthe surety posted bail bond today"], &["bail", "bond"]);
        assert_eq!(eval_phrase(&idx, &data, &[0, 1], &term_bytes), vec![0]);
    }

    #[test]
    fn phrase_requires_exact_adjacency_not_just_presence() {
        let (idx, data, term_bytes) = build(&["0\tbail posted bond"], &["bail", "bond"]);
        assert_eq!(eval_phrase(&idx, &data, &[0, 1], &term_bytes), Vec::<u32>::new());
    }

    #[test]
    fn phrase_handles_a_term_repeated_many_times() {
        // spec's "repeat" edge case: "w w w w" must still find "w w".
        let (idx, data, term_bytes) = build(&["0\tw w w w"], &["w"]);
        assert_eq!(eval_phrase(&idx, &data, &[0, 0], &term_bytes), vec![0]);
    }

    #[test]
    fn a_token_over_32_bytes_between_two_terms_breaks_adjacency() {
        // The oversized token is skipped by the tokenizer but still
        // consumes a position, so "bail" and "bond" are NOT adjacent
        // even though they're the only two tokens ever emitted.
        let long_token = "a".repeat(40);
        let doc = format!("0\tbail {long_token} bond");
        let (idx, data, term_bytes) = build(&[&doc], &["bail", "bond"]);
        assert_eq!(eval_phrase(&idx, &data, &[0, 1], &term_bytes), Vec::<u32>::new());
    }

    #[test]
    fn phrase_finds_a_match_that_starts_mid_document_after_a_false_start() {
        // "a a b" inside "a a a b": the window starting at the first
        // "a" fails (a,a,a != a,a,b) but the window starting at the
        // second "a" succeeds (a,a,b) -- a single-counter scan without
        // per-position window checks would miss this overlap.
        let (idx, data, term_bytes) = build(&["0\ta a a b"], &["a", "b"]);
        assert_eq!(eval_phrase(&idx, &data, &[0, 0, 1], &term_bytes), vec![0]);
    }

    #[test]
    fn phrase_candidates_are_limited_to_docs_containing_every_term() {
        let (idx, data, term_bytes) = build(
            &["0\tbail bond", "1\tbail only", "2\tbond only"],
            &["bail", "bond"],
        );
        assert_eq!(eval_phrase(&idx, &data, &[0, 1], &term_bytes), vec![0]);
    }
}
