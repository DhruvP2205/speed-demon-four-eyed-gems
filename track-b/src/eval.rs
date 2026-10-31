//! Evaluates BOOL and PHRASE queries against `Index`'s sorted
//! postings -- specs/b-index.md section 3. Every result set stays
//! sorted ascending throughout (merge-based set ops on already-sorted
//! inputs produce sorted output for free), so there's no separate
//! sort before formatting (section 4's smallest-10 and sum just read
//! straight off the front of the final `Vec<u32>`).

use crate::index::Index;
use crate::query_parse::{Op, Query};

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
/// re-tokenizes a document (unlike the reference): candidates come
/// from intersecting each term's PRESENCE list (smallest first, to
/// shrink the working set fastest), then each candidate is verified
/// directly against the already-built POSITION lists.
fn eval_phrase(index: &Index, terms: &[u32]) -> Vec<u32> {
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
    let mut matches = Vec::new();
    'doc: for &doc_id in &candidates {
        // Anchor on whichever term has the fewest occurrences IN THIS
        // DOCUMENT -- a document repeating one term heavily (spec's
        // "repeat" edge case, `w w w w`) must not turn every one of
        // its occurrences into a separate full scan of the others.
        let mut anchor_term_idx = 0usize;
        let mut anchor_positions = index.positions_in_doc(terms[0], doc_id);
        for t in 1..k {
            let p = index.positions_in_doc(terms[t], doc_id);
            if p.len() < anchor_positions.len() {
                anchor_positions = p;
                anchor_term_idx = t;
            }
        }
        for &(_, anchor_pos) in anchor_positions {
            let Some(phrase_start) = anchor_pos.checked_sub(anchor_term_idx as u32) else {
                continue;
            };
            let mut ok = true;
            for t in 0..k {
                if t == anchor_term_idx {
                    continue;
                }
                let want = phrase_start + t as u32;
                let positions = index.positions_in_doc(terms[t], doc_id);
                if positions.binary_search_by_key(&want, |&(_, p)| p).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                matches.push(doc_id);
                continue 'doc;
            }
        }
    }
    matches
}

pub fn eval(index: &Index, query: &Query) -> Vec<u32> {
    match query {
        Query::Bool { terms, ops } => eval_bool(index, terms, ops),
        Query::Phrase { terms } => eval_phrase(index, terms),
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
}
