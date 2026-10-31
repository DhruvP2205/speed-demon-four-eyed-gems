//! Split an in-memory CSV buffer into N record-aligned byte ranges
//! for parallel processing. A newline inside a quoted field is NOT a
//! record boundary, so this tracks the same quote-nesting state as
//! `csv_parser::CsvReader` (field-start-gated quote open, `""`
//! escape, and "trailing content after a closing quote is literal,
//! not a re-opened quote") rather than naively splitting on `\n`.

/// Split `data` into up to `n` non-overlapping, record-aligned ranges
/// covering `data` with no record split across a boundary.
pub fn record_aligned_ranges(data: &[u8], n: usize) -> Vec<(usize, usize)> {
    let len = data.len();
    if len == 0 || n == 0 {
        return Vec::new();
    }
    let n = n.min(len).max(1);
    let targets: Vec<usize> = (1..n).map(|i| len * i / n).collect();

    let mut boundaries = vec![0usize];
    let mut target_idx = 0;
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut i = 0;
    while i < len && target_idx < targets.len() {
        let b = data[i];
        if at_field_start && b == b'"' {
            in_quotes = true;
            at_field_start = false;
            i += 1;
            continue;
        }
        if in_quotes {
            if b == b'"' {
                if i + 1 < len && data[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_quotes = false; // closing quote; trailing content (if any) is literal, not re-quoted
                at_field_start = false;
                i += 1;
                continue;
            }
            i += 1;
            continue;
        }
        match b {
            b',' => {
                at_field_start = true;
                i += 1;
            }
            b'\n' => {
                i += 1;
                at_field_start = true;
                while target_idx < targets.len() && targets[target_idx] < i {
                    boundaries.push(i);
                    target_idx += 1;
                }
            }
            _ => {
                at_field_start = false;
                i += 1;
            }
        }
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

    fn reconstruct_record_count(data: &[u8], n: usize) -> usize {
        record_aligned_ranges(data, n)
            .iter()
            .map(|&(s, e)| CsvReader::new(&data[s..e]).count())
            .sum()
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
        for n in [1, 2, 3, 7, 12] {
            assert_eq!(reconstruct_record_count(&data, n), baseline, "n={n}");
        }
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
        for n in [1, 2, 3, 12] {
            assert_eq!(reconstruct_record_count(&data, n), baseline, "n={n}");
        }
    }
}
