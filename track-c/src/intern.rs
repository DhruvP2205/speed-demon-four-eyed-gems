//! Global, exact, sharded string interning for `name` fields.
//!
//! The per-thread `HashSet<Vec<u8>>` this replaces stores a FULL COPY
//! of every distinct name THAT THREAD sees. Since records are split
//! by byte range (not by content), the same name recurring across the
//! file is very likely to land in more than one thread's range, so
//! up to `MAX_THREADS` copies of it could be alive simultaneously
//! before the final merge frees the duplicates -- real peak-memory
//! cost when name reuse is common (confirmed: measured M was 646 MB
//! against a 204 MB input and a 367 MB reference).
//!
//! This stores each distinct name's bytes exactly ONCE, in one of
//! `NUM_SHARDS` independent arenas (sharded by hash, each behind its
//! own `Mutex`, so threads hashing to different shards don't
//! contend), and returns a small `u64` id in place of the name. Two
//! names are the SAME id only if their bytes compare equal -- the
//! hash is used only to narrow the search, never as the dedup key by
//! itself (a hash collision between two DIFFERENT names must not
//! merge them, or `names_distinct` would silently undercount and the
//! output would miss the exact-hash correctness gate).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

const NUM_SHARDS: usize = 64;

#[inline]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

struct Shard {
    arena: Vec<u8>,
    /// hash -> candidates with that hash: (offset into `arena`, length, local id).
    index: HashMap<u64, Vec<(u32, u32, u32)>>,
    next_local_id: u32,
}

pub struct InternTable {
    shards: Vec<Mutex<Shard>>,
}

impl InternTable {
    pub fn new() -> Self {
        InternTable {
            shards: (0..NUM_SHARDS)
                .map(|_| {
                    Mutex::new(Shard {
                        arena: Vec::new(),
                        index: HashMap::new(),
                        next_local_id: 0,
                    })
                })
                .collect(),
        }
    }

    /// Returns a globally unique id for `name`, interning its bytes
    /// (once, ever) if this is the first time they've been seen.
    pub fn intern(&self, name: &[u8]) -> u64 {
        let hash = hash_bytes(name);
        let shard_idx = (hash as usize) % self.shards.len();
        let mut shard = self.shards[shard_idx].lock().expect("intern shard lock");

        if let Some(candidates) = shard.index.get(&hash) {
            for &(offset, len, local_id) in candidates {
                let stored = &shard.arena[offset as usize..offset as usize + len as usize];
                if stored == name {
                    return pack(shard_idx, local_id);
                }
            }
        }

        let offset = shard.arena.len() as u32;
        shard.arena.extend_from_slice(name);
        let local_id = shard.next_local_id;
        shard.next_local_id += 1;
        shard
            .index
            .entry(hash)
            .or_default()
            .push((offset, name.len() as u32, local_id));
        pack(shard_idx, local_id)
    }
}

#[inline]
fn pack(shard_idx: usize, local_id: u32) -> u64 {
    ((shard_idx as u64) << 32) | local_id as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_name_interned_twice_returns_the_same_id() {
        let table = InternTable::new();
        let a = table.intern(b"Kaphar, Titus");
        let b = table.intern(b"Kaphar, Titus");
        assert_eq!(a, b);
    }

    #[test]
    fn different_names_get_different_ids() {
        let table = InternTable::new();
        let a = table.intern(b"Kaphar, Titus");
        let b = table.intern(b"Betts");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_name_interns_consistently() {
        let table = InternTable::new();
        let a = table.intern(b"");
        let b = table.intern(b"");
        assert_eq!(a, b);
    }

    #[test]
    fn many_distinct_names_all_round_trip_to_distinct_ids() {
        let table = InternTable::new();
        let names: Vec<String> = (0..2000).map(|i| format!("name-{i}")).collect();
        let ids: Vec<u64> = names.iter().map(|n| table.intern(n.as_bytes())).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "every distinct name must get a distinct id"
        );
        // Re-interning in reverse order must reproduce the exact same ids.
        for (i, name) in names.iter().enumerate().rev() {
            assert_eq!(table.intern(name.as_bytes()), ids[i]);
        }
    }

    #[test]
    fn concurrent_interning_from_multiple_threads_agrees_on_shared_names() {
        use std::sync::Arc;
        let table = Arc::new(InternTable::new());
        let shared_names = [b"Alice".as_slice(), b"Bob", b"Carol", b"Dave"];
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    let mut ids = Vec::new();
                    for _ in 0..50 {
                        for &name in &shared_names {
                            ids.push((name, table.intern(name)));
                        }
                    }
                    let _ = t;
                    ids
                })
            })
            .collect();
        let mut by_name: HashMap<&[u8], u64> = HashMap::new();
        for h in handles {
            for (name, id) in h.join().unwrap() {
                match by_name.get(name) {
                    Some(&existing) => {
                        assert_eq!(existing, id, "same name got different ids across threads")
                    }
                    None => {
                        by_name.insert(name, id);
                    }
                }
            }
        }
        assert_eq!(by_name.len(), shared_names.len());
    }
}
