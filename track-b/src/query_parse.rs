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
