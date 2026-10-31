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
//! Whole file loaded into memory once (204 MB at `full`, well inside
//! the 8 GB budget), split into up to MAX_THREADS record-aligned
//! ranges (see src/chunked.rs -- quote-aware, since an embedded `\n`
//! is not a record boundary), aggregated per thread into a small
//! (~241-entry) map, then merged and sorted by country code as
//! unsigned bytes (`_UNMATCHED`'s leading `_`, 0x5F, sorts after any
//! 3-letter uppercase code, so no special-casing is needed).
//!
//! Each record's `name` is interned through a shared, sharded
//! `InternTable` (`src/intern.rs`) before a country group's distinct-
//! name set ever sees it: the set stores compact 8-byte ids, not name
//! bytes, and the bytes themselves live exactly once, globally, not
//! once per thread that happens to see a repeated name.

mod chunked;
mod csv_parser;
mod format;
mod intern;

use std::collections::{HashMap, HashSet};
use std::fs;

use chunked::record_aligned_ranges;
use csv_parser::{strip_record_start_boms, CsvReader};
use format::{write_group_line, GroupLine};
use intern::InternTable;

const MAX_THREADS: usize = 12;
const RECORDS_FILE: &str = "records.csv";
const REGIONS_FILE: &str = "regions.csv";
const OUTPUT_FILE: &str = "aggregates.txt";
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

fn load_regions(path: &str) -> HashMap<u32, [u8; 3]> {
    let data = fs::read(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut regions = HashMap::new();
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
        let mut code = [0u8; 3];
        code.copy_from_slice(&country[..3]);
        regions.insert(region_id, code);
    }
    regions
}

#[derive(Default)]
struct CountryAgg {
    count: u64,
    id_sum: i64,
    amount_sum: i64,
    amount_min: Option<i64>,
    amount_max: Option<i64>,
    /// Intern-table ids (see `src/intern.rs`), not name bytes: dedup is
    /// still exact (two names collide here only if their actual bytes
    /// compared equal when interned), but a group's distinct-name set
    /// now costs 8 bytes/entry instead of a heap-allocated byte-string
    /// copy, and the bytes themselves are stored once globally instead
    /// of once per thread that happens to see that name.
    names: HashSet<u64>,
    name_bytes: u64,
    note_bytes: u64,
}

impl CountryAgg {
    fn add(&mut self, id: u64, name_id: u64, name_len: usize, note_len: usize, cents: i64) {
        self.count += 1;
        self.id_sum += id as i64;
        self.amount_sum += cents;
        self.amount_min = Some(self.amount_min.map_or(cents, |m| m.min(cents)));
        self.amount_max = Some(self.amount_max.map_or(cents, |m| m.max(cents)));
        self.name_bytes += name_len as u64;
        self.note_bytes += note_len as u64;
        self.names.insert(name_id);
    }

    fn merge(&mut self, other: CountryAgg) {
        self.count += other.count;
        self.id_sum += other.id_sum;
        self.amount_sum += other.amount_sum;
        self.amount_min = Some(match (self.amount_min, other.amount_min) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return,
        });
        self.amount_max = Some(match (self.amount_max, other.amount_max) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => 0,
        });
        self.name_bytes += other.name_bytes;
        self.note_bytes += other.note_bytes;
        self.names.extend(other.names);
    }
}

type LocalMap = HashMap<Vec<u8>, CountryAgg>;

fn process_chunk(
    raw: &[u8],
    is_first_chunk: bool,
    regions: &HashMap<u32, [u8; 3]>,
    names: &InternTable,
) -> LocalMap {
    let cleaned = strip_record_start_boms(raw);
    let mut local: LocalMap = HashMap::new();
    for (i, fields) in CsvReader::new(&cleaned).enumerate() {
        if is_first_chunk && i == 0 {
            continue; // the header row: "id,name,note,region_id,amount"
        }
        let id = parse_u64(&fields[0]);
        let name: &[u8] = &fields[1];
        let note: &[u8] = &fields[2];
        let region_id = parse_u64(&fields[3]) as u32;
        let cents = parse_cents(&fields[4]);
        let key: Vec<u8> = regions
            .get(&region_id)
            .map(|c| c.to_vec())
            .unwrap_or_else(|| UNMATCHED.to_vec());
        let name_id = names.intern(name);
        local
            .entry(key)
            .or_default()
            .add(id, name_id, name.len(), note.len(), cents);
    }
    local
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map(String::as_str).unwrap_or("/data");
    let out_dir = args.get(2).map(String::as_str).unwrap_or("/out");

    let regions = load_regions(&format!("{data_dir}/{REGIONS_FILE}"));
    let records_path = format!("{data_dir}/{RECORDS_FILE}");
    let data = fs::read(&records_path).unwrap_or_else(|e| panic!("open {records_path}: {e}"));

    let ranges = record_aligned_ranges(&data, MAX_THREADS);
    let names = InternTable::new();
    let partials: Vec<LocalMap> = std::thread::scope(|scope| {
        let regions_ref = &regions;
        let data_ref = &data;
        let names_ref = &names;
        let handles: Vec<_> = ranges
            .iter()
            .enumerate()
            .map(|(idx, &(start, end))| {
                scope.spawn(move || {
                    process_chunk(&data_ref[start..end], idx == 0, regions_ref, names_ref)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });

    let mut global: LocalMap = HashMap::new();
    for partial in partials {
        for (key, agg) in partial {
            global.entry(key).or_default().merge(agg);
        }
    }

    let mut keys: Vec<&Vec<u8>> = global.keys().collect();
    keys.sort();

    std::fs::create_dir_all(out_dir).expect("create out dir");
    let out_file = fs::File::create(format!("{out_dir}/{OUTPUT_FILE}")).expect("create output");
    let mut writer = std::io::BufWriter::new(out_file);

    for key in keys {
        let agg = global.get(key).unwrap();
        write_group_line(
            &mut writer,
            &GroupLine {
                key,
                count: agg.count,
                id_sum: agg.id_sum,
                amount_sum: agg.amount_sum,
                amount_min: agg.amount_min.unwrap_or(0),
                amount_max: agg.amount_max.unwrap_or(0),
                names_distinct: agg.names.len() as u64,
                name_bytes: agg.name_bytes,
                note_bytes: agg.note_bytes,
            },
        )
        .expect("write output line");
    }
}
