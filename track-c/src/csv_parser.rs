//! RFC4180-style CSV parsing for records.csv: quoted commas,
//! `""`-escaped quotes, embedded raw newlines (bare LF and CRLF)
//! inside quoted fields, mixed CRLF/LF between records. Mirrors
//! Python's own `csv` module (what `reference/c_join.py` relies on)
//! field-for-field, byte-for-byte.
//!
//! A BOM is stripped only where it marks a record start: byte 0, or
//! immediately after a terminating `\n`, never mid-field (spec section
//! 2.1). `CsvReader::next()` is only ever called with its cursor
//! positioned at exactly such a point, so it skips a BOM there
//! in-place, with no separate whole-buffer pre-pass and no copy --
//! the fields it hands back borrow directly from the input slice.
//!
//! A lone `\r` not followed by `\n` is treated as ordinary field
//! content, not a line ending -- the generator never emits one (the
//! only CR byte anywhere is the first byte of a CRLF terminator), so
//! this can't affect hash-matching against generated data.

use std::borrow::Cow;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// One parsed CSV record: borrowed slices for unquoted (or quote-free)
/// fields, owned bytes only for fields that needed `""`-unescaping.
pub type Record<'a> = Vec<Cow<'a, [u8]>>;

/// Iterator over records in a byte buffer, honoring embedded newlines
/// inside quoted fields -- this must NOT be implemented as "split
/// into lines, then parse each line", since a quoted field can
/// legitimately span several physical lines.
pub struct CsvReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> CsvReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        CsvReader { data, pos: 0 }
    }
}

impl<'a> Iterator for CsvReader<'a> {
    type Item = Record<'a>;

    fn next(&mut self) -> Option<Record<'a>> {
        if self.pos >= self.data.len() {
            return None;
        }
        let data = self.data;
        let mut pos = self.pos;

        // `pos` is always exactly a record start (byte 0, or right
        // after the previous record's `\n`) -- the only two places a
        // BOM can legally appear (spec 2.1) -- so this unconditionally
        // skips one here, in place, with no copy.
        if data[pos..].starts_with(&BOM) {
            pos += 3;
            if pos >= data.len() {
                self.pos = pos;
                return None;
            }
        }

        let mut fields: Record<'a> = Vec::with_capacity(5);

        loop {
            if pos < data.len() && data[pos] == b'"' {
                pos += 1;
                let start = pos;
                let mut needs_unescape = false;
                loop {
                    if pos >= data.len() {
                        break; // unterminated quote at EOF: lenient, take what's there
                    }
                    if data[pos] == b'"' {
                        if pos + 1 < data.len() && data[pos + 1] == b'"' {
                            needs_unescape = true;
                            pos += 2;
                            continue;
                        }
                        break;
                    }
                    pos += 1;
                }
                let raw = &data[start..pos];
                if pos < data.len() && data[pos] == b'"' {
                    pos += 1; // consume closing quote
                }
                let mut field: Cow<'a, [u8]> = if needs_unescape {
                    let mut buf = Vec::with_capacity(raw.len());
                    let mut j = 0;
                    while j < raw.len() {
                        if raw[j] == b'"' && j + 1 < raw.len() && raw[j + 1] == b'"' {
                            buf.push(b'"');
                            j += 2;
                        } else {
                            buf.push(raw[j]);
                            j += 1;
                        }
                    }
                    Cow::Owned(buf)
                } else {
                    Cow::Borrowed(raw)
                };
                // Python's lenient `csv` module: once the closing quote
                // is followed by anything other than a delimiter or
                // line terminator, that trailing text is appended onto
                // the SAME field as ordinary content -- it does not
                // reopen a quote or get unescaped.
                if pos < data.len() && data[pos] != b',' && data[pos] != b'\n' && data[pos] != b'\r'
                {
                    let trail_start = pos;
                    while pos < data.len() && data[pos] != b',' && data[pos] != b'\n' {
                        pos += 1;
                    }
                    let mut trail_end = pos;
                    if pos < data.len()
                        && data[pos] == b'\n'
                        && trail_end > trail_start
                        && data[trail_end - 1] == b'\r'
                    {
                        trail_end -= 1;
                    }
                    field
                        .to_mut()
                        .extend_from_slice(&data[trail_start..trail_end]);
                }
                fields.push(field);
            } else {
                // Only ',' and '\n' end an unquoted field -- NOT '\r'
                // (a lone '\r' is ordinary content, see module doc).
                let start = pos;
                while pos < data.len() && data[pos] != b',' && data[pos] != b'\n' {
                    pos += 1;
                }
                let mut end = pos;
                if pos < data.len() && data[pos] == b'\n' && end > start && data[end - 1] == b'\r' {
                    end -= 1;
                }
                fields.push(Cow::Borrowed(&data[start..end]));
            }

            if pos < data.len() && data[pos] == b',' {
                pos += 1;
                continue;
            }
            break;
        }

        // pos is now at '\n', EOF, or (only when the last field was
        // QUOTED and the record ends in CRLF) a raw '\r' the quoted
        // path never trimmed.
        if pos < data.len() && data[pos] == b'\r' && pos + 1 < data.len() && data[pos + 1] == b'\n'
        {
            pos += 2;
        } else if pos < data.len() && data[pos] == b'\n' {
            pos += 1;
        } else if pos < data.len() && data[pos] == b'\r' {
            pos += 1; // malformed; the generator never produces this, but guarantee forward progress
        }

        self.pos = pos;
        Some(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
        CsvReader::new(data)
            .map(|rec| rec.into_iter().map(|c| c.into_owned()).collect())
            .collect()
    }

    fn row_strs(data: &[u8]) -> Vec<Vec<String>> {
        rows(data)
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|f| String::from_utf8(f).unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn simple_unquoted_fields() {
        assert_eq!(row_strs(b"1,2,3\n"), vec![vec!["1", "2", "3"]]);
    }

    #[test]
    fn quoted_field_with_comma() {
        assert_eq!(row_strs(b"1,\"a,b\",3\n"), vec![vec!["1", "a,b", "3"]]);
    }

    #[test]
    fn quoted_field_with_escaped_quote() {
        assert_eq!(
            row_strs(b"1,\"say \"\"hi\"\"\",3\n"),
            vec![vec!["1", "say \"hi\"", "3"]]
        );
    }

    #[test]
    fn quoted_field_with_embedded_lf() {
        assert_eq!(row_strs(b"1,\"a\nb\",3\n"), vec![vec!["1", "a\nb", "3"]]);
    }

    #[test]
    fn quoted_field_with_embedded_crlf() {
        assert_eq!(
            row_strs(b"1,\"a\r\nb\",3\n"),
            vec![vec!["1", "a\r\nb", "3"]]
        );
    }

    #[test]
    fn mixed_crlf_and_lf_between_records() {
        assert_eq!(
            row_strs(b"1,2\r\n3,4\n5,6\r\n"),
            vec![vec!["1", "2"], vec!["3", "4"], vec!["5", "6"]]
        );
    }

    #[test]
    fn empty_quoted_and_unquoted_fields_are_the_same_empty_value() {
        assert_eq!(row_strs(b"1,\"\",3\n")[0][1], "");
        assert_eq!(row_strs(b"1,,3\n")[0][1], "");
    }

    #[test]
    fn no_trailing_newline_at_eof() {
        assert_eq!(row_strs(b"1,2,3"), vec![vec!["1", "2", "3"]]);
    }

    #[test]
    fn strips_bom_at_byte_zero_and_after_newline_but_not_mid_field() {
        let mut data = BOM.to_vec();
        data.extend_from_slice(b"1,2\n");
        data.extend_from_slice(&BOM);
        data.extend_from_slice(b"3,4\n");
        assert_eq!(row_strs(&data), vec![vec!["1", "2"], vec!["3", "4"]]);

        // A BOM mid-field is ordinary data, not a record start, so it
        // must never be special-cased or stripped.
        let mut mid = b"1,a".to_vec();
        mid.extend_from_slice(&BOM);
        mid.extend_from_slice(b"b,3\n");
        let expected_field: Vec<u8> = [b"a".as_slice(), &BOM, b"b".as_slice()].concat();
        assert_eq!(
            rows(&mid)[0][1],
            expected_field,
            "BOM mid-field must NOT be stripped"
        );
    }

    #[test]
    fn trailing_content_after_closing_quote_is_appended_not_requoted() {
        assert_eq!(row_strs(b"1,\"a\"b,3\n"), vec![vec!["1", "ab", "3"]]);
        assert_eq!(row_strs(b"1,\"a\"b\"c,3\n")[0][1], "ab\"c");
    }

    #[test]
    fn full_hostile_fixture_shape_matches_the_spec_worked_example() {
        let data = b"1,\"Kaphar, Titus\",\"said \"\"no\"\" twice\",7,1420.00\r\n\
                      2,\"Betts\",\"a note that\nspans two lines\",7,88.50\n\
                      3,,\"\",12,0.00\n\
                      4,\"trailing space \",\"comma, quote \"\" and newline\ntogether\",3,-5.25\n";
        let parsed = row_strs(data);
        assert_eq!(
            parsed[0],
            vec!["1", "Kaphar, Titus", "said \"no\" twice", "7", "1420.00"]
        );
        assert_eq!(
            parsed[1],
            vec!["2", "Betts", "a note that\nspans two lines", "7", "88.50"]
        );
        assert_eq!(parsed[2], vec!["3", "", "", "12", "0.00"]);
        assert_eq!(parsed[3][1], "trailing space ");
        assert_eq!(parsed[3][2], "comma, quote \" and newline\ntogether");
    }
}
