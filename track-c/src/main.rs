//! Speed Demon 2026, Track C (Join). specs/c-join.md is the contract.
//!
//! Joins /data/records.csv (hostile CSV: quoted fields, `""` escapes,
//! embedded newlines, mixed CRLF/LF, a UTF-8 BOM at file start and at
//! some mid-file record boundaries) against /data/regions.csv on
//! `region_id`, and writes /out/aggregates.txt: per country (or
//! `_UNMATCHED` for an unknown region_id), the record count, id sum,
//! amount sum/min/max in integer cents, distinct name count, and the
//! summed UTF-8 byte length of unescaped names/notes.
//!
//! Both input files are mapped read-only via `mmap` (`src/mmap_file.rs`)
//! instead of read into an owned buffer: for a 204 MB input, `fs::read`
//! is a real kernel-to-heap memcpy (confirmed by this track's own
//! `valgrind --massif` profiling as a literal 200+ MB entry in the
//! peak-heap breakdown); `mmap` maps the page cache directly into this
//! process's address space, no copy.
//!
//! Of the nine output fields, eight are simple streaming aggregates --
//! `count`, `id_sum`, `amount_sum`/`min`/`max`, `name_bytes`,
//! `note_bytes` -- that need no retained per-record data, just an O(1)
//! update into a flat array direct-indexed by a dense country id (see
//! `pack_country`: `[A-Z]{3}` packs injectively and order-preservingly
//! into `0..17576`, so there is no hashing anywhere in the region
//! join). `names_distinct` is the one field that needs real
//! deduplication, and is the only reason this file has a "phase 2" at
//! all:
//!
//!   Phase 1 (12 threads, disjoint byte ranges, zero shared state): each
//!   thread parses its range directly off the mmap (its fields borrow
//!   straight from the mapped bytes -- `Cow::Owned` only for a name
//!   that needed `""`-unescaping), updates its OWN flat aggregate array
//!   by direct index, and for each record's name, computes
//!   `shard = hash(name) % 12` and pushes `(country, name)` into its
//!   OWN per-shard buffer. No hashmap, no lock, no shared mutable state
//!   -- this replaces the old mutex-sharded `InternTable`, which was
//!   the hot-loop lock-contention source the board's T gap pointed at.
//!
//!   Phase 2 (12 threads, one shard each, exclusively): since a name's
//!   shard depends only on its own bytes, every occurrence of a given
//!   name across all 12 phase-1 threads lands in the SAME shard index
//!   -- so each phase-2 thread owns its shard outright and needs no
//!   lock either. No hashing at all: sorts its shard's `(country,
//!   name)` pairs (still an exact byte comparison, never hash-only --
//!   a collision must never silently merge two different names) and
//!   counts a new distinct name wherever consecutive sorted entries
//!   differ, then emits a per-country distinct count for its shard.
//!   Summing the 12 shards' counts per country gives the exact global
//!   `names_distinct`. An earlier version used a `HashMap<u16,
//!   HashSet<&[u8]>>` instead; a measured A/B (see PERF.md) found the
//!   sort consistently faster once this phase became the largest
//!   remaining cost in the program.

mod chunked;
mod csv_parser;
mod format;
mod mmap_file;

use std::borrow::Cow;

use chunked::record_aligned_ranges;
use csv_parser::CsvReader;
use format::{write_group_line, GroupLine};
use mmap_file::MappedFile;

const MAX_THREADS: usize = 12;
const RECORDS_FILE: &str = "records.csv";
const REGIONS_FILE: &str = "regions.csv";
const OUTPUT_FILE: &str = "aggregates.txt";

/// Every possible `[A-Z]{3}` country code packs injectively into
/// `0..NUM_COUNTRY_CODES`; `UNMATCHED_IDX` is one extra slot past the
/// end for `_UNMATCHED`.
const NUM_COUNTRY_CODES: usize = 26 * 26 * 26; // 17,576
const UNMATCHED_IDX: usize = NUM_COUNTRY_CODES;
const NUM_SLOTS: usize = NUM_COUNTRY_CODES + 1;
const UNMATCHED: &[u8] = b"_UNMATCHED";

#[inline]
fn parse_u64(bytes: &[u8]) -> u64 {
    let mut acc: u64 = 0;
    for &b in bytes {
        acc = acc * 10 + (b - b'0') as u64;
    }
    acc
}

/// `"-5.25" -> -525`. `amount` grammar (spec 2.1): `["-"] 1*DIGIT "." 2DIGIT`.
#[inline]
fn parse_cents(bytes: &[u8]) -> i64 {
    let (neg, rest) = if bytes.first() == Some(&b'-') {
        (true, &bytes[1..])
    } else {
        (false, bytes)
    };
    let dot = rest
        .iter()
        .position(|&b| b == b'.')
        .expect("amount missing '.'");
    let whole = parse_u64(&rest[..dot]) as i64;
    let frac = parse_u64(&rest[dot + 1..]) as i64;
    let cents = whole * 100 + frac;
    if neg {
        -cents
    } else {
        cents
    }
}

/// Packs a `[A-Z]{3}` country code into a dense `0..NUM_COUNTRY_CODES`
/// index. Injective AND order-preserving: each letter contributes a
/// digit in `0..26` and the weights (676, 26, 1) exceed that range, so
/// comparing two packed indices numerically gives exactly the same
/// order as comparing the three letters lexicographically (byte
/// order). That means iterating indices `0..NUM_SLOTS` ascending IS
/// the fully sorted output order already (`_UNMATCHED` at
/// `UNMATCHED_IDX`, the largest index, sorts last) -- see `main`,
/// which emits output in this order directly with no separate sort.
#[inline]
fn pack_country(code: &[u8]) -> usize {
    (code[0] - b'A') as usize * 676 + (code[1] - b'A') as usize * 26 + (code[2] - b'A') as usize
}

#[inline]
fn unpack_country(idx: usize) -> [u8; 3] {
    let c0 = idx / 676;
    let rem = idx % 676;
    let c1 = rem / 26;
    let c2 = rem % 26;
    [b'A' + c0 as u8, b'A' + c1 as u8, b'A' + c2 as u8]
}

/// `region_to_country[region_id]` is the packed country index for that
/// region, direct-indexed (region_id is a plain positive integer, up
/// to ~1,000,000 at full scale, per spec section 2.5) -- no hashing.
/// An out-of-range or never-assigned `region_id` reads back
/// `UNMATCHED_IDX` by construction: the vector is filled with it
/// before any real region overwrites an entry, and `country_of`'s
/// bounds-checked `.get` falls back to it for anything beyond the end.
fn load_regions(path: &str) -> Vec<u16> {
    let mapped = MappedFile::open(path, false).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let data = mapped.as_slice();
    let mut max_region_id: u32 = 0;
    let mut entries: Vec<(u32, u16)> = Vec::new();
    let mut lines = data.split(|&b| b == b'\n');
    lines.next(); // header: "region_id,country"
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let comma = line
            .iter()
            .position(|&b| b == b',')
            .expect("regions.csv row missing comma");
        let region_id = parse_u64(&line[..comma]) as u32;
        let country = &line[comma + 1..];
        let idx = pack_country(&country[..3]) as u16;
        max_region_id = max_region_id.max(region_id);
        entries.push((region_id, idx));
    }
    let mut region_to_country = vec![UNMATCHED_IDX as u16; max_region_id as usize + 1];
    for (region_id, idx) in entries {
        region_to_country[region_id as usize] = idx;
    }
    region_to_country
}

#[inline]
fn country_of(region_to_country: &[u16], region_id: u32) -> usize {
    region_to_country
        .get(region_id as usize)
        .copied()
        .unwrap_or(UNMATCHED_IDX as u16) as usize
}

#[derive(Clone, Copy)]
struct CountryAgg {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

impl Default for CountryAgg {
    fn default() -> Self {
        CountryAgg {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
        }
    }
}

impl CountryAgg {
    #[inline]
    fn add(&mut self, id: u64, name_len: usize, note_len: usize, cents: i64) {
        self.count += 1;
        self.id_sum += id as i64;
        self.amount_sum += cents;
        if cents < self.amount_min {
            self.amount_min = cents;
        }
        if cents > self.amount_max {
            self.amount_max = cents;
        }
        self.name_bytes += name_len as u64;
        self.note_bytes += note_len as u64;
    }

    #[inline]
    fn merge(&mut self, other: &CountryAgg) {
        self.count += other.count;
        self.id_sum += other.id_sum;
        self.amount_sum += other.amount_sum;
        self.amount_min = self.amount_min.min(other.amount_min);
        self.amount_max = self.amount_max.max(other.amount_max);
        self.name_bytes += other.name_bytes;
        self.note_bytes += other.note_bytes;
    }
}

/// FNV-1a: fast and deterministic, which is all shard assignment needs
/// -- it only has to be a stable function of the bytes, never a
/// security property.
#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// One phase-1 thread's output: its own flat aggregate array (direct
/// country index, no hashing) plus its names bucketed into 12 shards
/// for phase 2.
struct ThreadResult<'a> {
    aggs: Vec<CountryAgg>,
    shard_bufs: Vec<Vec<(u16, Cow<'a, [u8]>)>>,
}

/// Phase 1: parse one thread's disjoint byte range, straight off the
/// mmap. No hashmap, no lock, no shared mutable state: every write is
/// either into this thread's own `aggs` (direct index) or its own
/// shard buffers (push, no lookup).
fn process_chunk<'a>(
    raw: &'a [u8],
    is_first_chunk: bool,
    region_to_country: &[u16],
) -> ThreadResult<'a> {
    let mut aggs = vec![CountryAgg::default(); NUM_SLOTS];
    let mut shard_bufs: Vec<Vec<(u16, Cow<'a, [u8]>)>> =
        (0..MAX_THREADS).map(|_| Vec::new()).collect();

    for (i, mut fields) in CsvReader::new(raw).enumerate() {
        if is_first_chunk && i == 0 {
            continue; // header row: "id,name,note,region_id,amount"
        }
        let id = parse_u64(&fields[0]);
        let note_len = fields[2].len();
        let region_id = parse_u64(&fields[3]) as u32;
        let cents = parse_cents(&fields[4]);
        let country_idx = country_of(region_to_country, region_id);
        let name = fields.swap_remove(1);

        aggs[country_idx].add(id, name.len(), note_len, cents);

        let shard = (fnv1a(&name) % MAX_THREADS as u64) as usize;
        shard_bufs[shard].push((country_idx as u16, name));
    }

    ThreadResult { aggs, shard_bufs }
}

/// Phase 2: exact dedup for exactly one shard, gathered from every
/// phase-1 thread's contribution to it. A name's shard depends only on
/// its own bytes, so every occurrence of a given name across all 12
/// phase-1 threads lands in this same shard -- this thread owns it
/// exclusively, so no lock is needed here either.
///
/// No hashing anywhere: collects every `(country, name)` pair assigned
/// to this shard into one flat `Vec`, sorts it (lexicographic on
/// `name`, so equal `(country, name)` pairs become adjacent -- still
/// an EXACT byte comparison, never hash-only), then counts a new
/// distinct name each time consecutive entries differ. Replaced an
/// earlier `HashMap<u16, HashSet<&[u8]>>` version (first with std's
/// default SipHash, then with a hand-rolled FNV-1a hasher once
/// profiling showed phase 2 was the single largest remaining phase):
/// an interleaved, 5-round A/B against that FNV-hashed version showed
/// this sort-based design consistently faster with non-overlapping
/// timing ranges (see PERF.md) -- the two nested hash structures' own
/// overhead (table growth, probing) turned out to cost more than the
/// hashing itself did.
fn dedup_shard<'a>(results: &'a [ThreadResult<'a>], shard: usize) -> Vec<u64> {
    let total: usize = results.iter().map(|r| r.shard_bufs[shard].len()).sum();
    let mut pairs: Vec<(u16, &'a [u8])> = Vec::with_capacity(total);
    for result in results {
        for (country_idx, name) in &result.shard_bufs[shard] {
            pairs.push((*country_idx, name.as_ref()));
        }
    }
    pairs.sort_unstable();

    let mut counts = vec![0u64; NUM_SLOTS];
    let mut prev: Option<(u16, &[u8])> = None;
    for &(country_idx, name) in &pairs {
        if prev != Some((country_idx, name)) {
            counts[country_idx as usize] += 1;
            prev = Some((country_idx, name));
        }
    }
    counts
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let region_to_country = load_regions(&format!("{data_dir}/{REGIONS_FILE}"));
    let records_path = format!("{data_dir}/{RECORDS_FILE}");
    let mapped = MappedFile::open(&records_path, true)
        .unwrap_or_else(|e| panic!("open {records_path}: {e}"));
    let data = mapped.as_slice();

    let ranges = record_aligned_ranges(data, MAX_THREADS);
    let region_to_country_ref = &region_to_country;

    // Phase 1: parse, direct-index aggregate, shard-bucket names.
    let phase1: Vec<ThreadResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(idx, &(start, end))| {
                scope.spawn(move || {
                    process_chunk(&data[start..end], idx == 0, region_to_country_ref)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("phase-1 worker panicked"))
            .collect()
    });

    // Phase 2: exact dedup, one thread per shard, each shard owned exclusively.
    let phase1_ref = &phase1;
    let shard_counts: Vec<Vec<u64>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..MAX_THREADS)
            .map(|shard| scope.spawn(move || dedup_shard(phase1_ref, shard)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("phase-2 worker panicked"))
            .collect()
    });

    let mut final_aggs = vec![CountryAgg::default(); NUM_SLOTS];
    for result in &phase1 {
        for i in 0..NUM_SLOTS {
            final_aggs[i].merge(&result.aggs[i]);
        }
    }
    let mut names_distinct = vec![0u64; NUM_SLOTS];
    for counts in &shard_counts {
        for i in 0..NUM_SLOTS {
            names_distinct[i] += counts[i];
        }
    }

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file =
        std::fs::File::create(format!("{out_dir}/{OUTPUT_FILE}")).expect("create output");
    let mut writer = std::io::BufWriter::new(out_file);

    // Ascending index order already matches bytewise-sorted output
    // order exactly (see `pack_country`), so no sort step is needed.
    for idx in 0..NUM_SLOTS {
        let agg = &final_aggs[idx];
        if agg.count == 0 {
            continue;
        }
        let code = if idx == UNMATCHED_IDX {
            None
        } else {
            Some(unpack_country(idx))
        };
        let key: &[u8] = match &code {
            Some(c) => c.as_slice(),
            None => UNMATCHED,
        };
        write_group_line(
            &mut writer,
            &GroupLine {
                key,
                count: agg.count,
                id_sum: agg.id_sum,
                amount_sum: agg.amount_sum,
                amount_min: agg.amount_min,
                amount_max: agg.amount_max,
                names_distinct: names_distinct[idx],
                name_bytes: agg.name_bytes,
                note_bytes: agg.note_bytes,
            },
        )
        .expect("write output line");
    }
}
