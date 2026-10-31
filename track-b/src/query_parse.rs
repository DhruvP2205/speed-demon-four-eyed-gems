//! Parses `queries.txt` (specs/b-index.md section 2.3) into a term
//! dictionary plus one `Query` per line, in input order (output must
//! answer query `i` on line `i`).
//!
//! Interning every distinct query term up front, before the corpus is
//! ever touched, is what lets the corpus scan (src/index.rs) build
//! postings only for the terms these 50,000 queries actually use,
//! instead of a full inverted index over the corpus's 600,000-word
//! vocabulary (explicitly a legitimate strategy, spec section 3 and
//! section 8 point 4).

use std::collections::HashMap;

pub enum Op {
    And,
    Or,
    Not,
}

pub enum Query {
    /// `terms.len() == ops.len() + 1`: `terms[0]` then
    /// `(ops[i], terms[i+1])` applied left to right.
    Bool {
        terms: Vec<u32>,
        ops: Vec<Op>,
    },
    Phrase {
        terms: Vec<u32>,
    },
}

pub struct QueryIndex {
    pub queries: Vec<Query>,
    /// `term_id -> bytes` (always lowercase already, grammar section
    /// 2.3: `term = 1*32(%x61-7A / %x30-39)`).
    pub term_bytes: Vec<Vec<u8>>,
}

fn intern(term: &[u8], term_ids: &mut HashMap<Vec<u8>, u32>, term_bytes: &mut Vec<Vec<u8>>) -> u32 {
    if let Some(&id) = term_ids.get(term) {
        return id;
    }
    let id = term_bytes.len() as u32;
    term_bytes.push(term.to_vec());
    term_ids.insert(term.to_vec(), id);
    id
}

pub fn parse_queries(data: &[u8]) -> QueryIndex {
    let mut term_ids: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut term_bytes: Vec<Vec<u8>> = Vec::new();
    let mut queries: Vec<Query> = Vec::new();

    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"BOOL ") {
            let mut parts = rest.split(|&b| b == b' ');
            let first = parts.next().expect("BOOL query missing first term");
            let mut terms = vec![intern(first, &mut term_ids, &mut term_bytes)];
            let mut ops = Vec::new();
            loop {
                let op_part = match parts.next() {
                    Some(p) => p,
                    None => break,
                };
                let term_part = parts
                    .next()
                    .expect("BOOL query operator with no following term");
                let op = match op_part {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => panic!(
                        "unknown BOOL operator {:?}",
                        String::from_utf8_lossy(op_part)
                    ),
                };
                ops.push(op);
                terms.push(intern(term_part, &mut term_ids, &mut term_bytes));
            }
            queries.push(Query::Bool { terms, ops });
        } else if let Some(rest) = line.strip_prefix(b"PHRASE \"") {
            let inner = rest
                .strip_suffix(b"\"")
                .expect("PHRASE query missing closing quote");
            let terms: Vec<u32> = inner
                .split(|&b| b == b' ')
                .map(|t| intern(t, &mut term_ids, &mut term_bytes))
                .collect();
            queries.push(Query::Phrase { terms });
        } else {
            panic!(
                "unrecognized query line: {:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    QueryIndex {
        queries,
        term_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_bool_query() {
        let idx = parse_queries(b"BOOL bail AND bond\n");
        assert_eq!(idx.queries.len(), 1);
        match &idx.queries[0] {
            Query::Bool { terms, ops } => {
                assert_eq!(terms.len(), 2);
                assert_eq!(ops.len(), 1);
                assert!(matches!(ops[0], Op::And));
                assert_eq!(idx.term_bytes[terms[0] as usize], b"bail");
                assert_eq!(idx.term_bytes[terms[1] as usize], b"bond");
            }
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn parses_a_single_term_bool_query_with_no_operator() {
        let idx = parse_queries(b"BOOL bail\n");
        match &idx.queries[0] {
            Query::Bool { terms, ops } => {
                assert_eq!(terms.len(), 1);
                assert!(ops.is_empty());
            }
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn parses_a_phrase_query() {
        let idx = parse_queries(b"PHRASE \"bail bond\"\n");
        match &idx.queries[0] {
            Query::Phrase { terms } => {
                assert_eq!(terms.len(), 2);
                assert_eq!(idx.term_bytes[terms[0] as usize], b"bail");
                assert_eq!(idx.term_bytes[terms[1] as usize], b"bond");
            }
            _ => panic!("expected Phrase"),
        }
    }

    #[test]
    fn the_same_term_used_in_bool_and_phrase_queries_shares_one_id() {
        let idx = parse_queries(b"BOOL bail AND bond\nPHRASE \"writ of bail\"\n");
        let bail_in_bool = match &idx.queries[0] {
            Query::Bool { terms, .. } => terms[0],
            _ => panic!(),
        };
        let bail_in_phrase = match &idx.queries[1] {
            Query::Phrase { terms } => terms[2],
            _ => panic!(),
        };
        assert_eq!(
            bail_in_bool, bail_in_phrase,
            "same term bytes must intern to the same id"
        );
    }

    #[test]
    fn literal_and_or_not_words_in_a_term_position_are_ordinary_terms() {
        // Grammar note: terms are always lowercase, operators always
        // uppercase, so lowercase "and"/"or"/"not" as a TERM is fine --
        // only uppercase tokens in operator position are operators.
        let idx = parse_queries(b"BOOL and AND not\n");
        match &idx.queries[0] {
            Query::Bool { terms, ops } => {
                assert_eq!(idx.term_bytes[terms[0] as usize], b"and");
                assert_eq!(idx.term_bytes[terms[1] as usize], b"not");
                assert!(matches!(ops[0], Op::And));
            }
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn preserves_query_order() {
        let idx = parse_queries(b"BOOL a\nPHRASE \"b c\"\nBOOL d OR e\n");
        assert_eq!(idx.queries.len(), 3);
        assert!(matches!(idx.queries[0], Query::Bool { .. }));
        assert!(matches!(idx.queries[1], Query::Phrase { .. }));
        assert!(matches!(idx.queries[2], Query::Bool { .. }));
    }
}
