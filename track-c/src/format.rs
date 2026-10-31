//! Output format: specs/c-join.md sections 3-4.
//!
//! One line per group (a country code, or `_UNMATCHED`) with >=1
//! record, space-separated:
//!     country count id_sum amount_sum amount_min amount_max names_distinct name_bytes note_bytes
//!
//! Money fields are integer cents formatted as `-`(if negative) +
//! whole (no leading zeros) + `.` + exactly 2 fractional digits.
//! `-0.00` never appears, since `cents == 0` never gets a sign. No
//! binary floating point anywhere, per CONVENTIONS.md.

use std::io::Write;

/// `-525 -> "-5.25"`, `-5 -> "-0.05"`, `0 -> "0.00"`.
pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

pub struct GroupLine<'a> {
    pub key: &'a [u8],
    pub count: u64,
    pub id_sum: i64,
    pub amount_sum: i64,
    pub amount_min: i64,
    pub amount_max: i64,
    pub names_distinct: u64,
    pub name_bytes: u64,
    pub note_bytes: u64,
}

pub fn write_group_line<W: Write>(w: &mut W, line: &GroupLine) -> std::io::Result<()> {
    w.write_all(line.key)?;
    write!(
        w,
        " {} {} {} {} {} {} {} {}\n",
        line.count,
        line.id_sum,
        format_cents(line.amount_sum),
        format_cents(line.amount_min),
        format_cents(line.amount_max),
        line.names_distinct,
        line.name_bytes,
        line.note_bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_cents_matches_reference_examples() {
        assert_eq!(format_cents(-525), "-5.25");
        assert_eq!(format_cents(-5), "-0.05");
        assert_eq!(format_cents(0), "0.00");
        assert_eq!(format_cents(142000), "1420.00");
        assert_eq!(format_cents(-75), "-0.75");
    }

    #[test]
    fn write_group_line_matches_worked_example_aaa_row() {
        let mut buf = Vec::new();
        write_group_line(
            &mut buf,
            &GroupLine {
                key: b"AAA",
                count: 5,
                id_sum: 17,
                amount_sum: 151775,
                amount_min: -75,
                amount_max: 142000,
                names_distinct: 4,
                name_bytes: 24,
                note_bytes: 55,
            },
        )
        .unwrap();
        assert_eq!(buf, b"AAA 5 17 1517.75 -0.75 1420.00 4 24 55\n");
    }

    #[test]
    fn write_group_line_matches_worked_example_unmatched_row() {
        let mut buf = Vec::new();
        write_group_line(
            &mut buf,
            &GroupLine {
                key: b"_UNMATCHED",
                count: 2,
                id_sum: 15,
                amount_sum: 100,
                amount_min: -100,
                amount_max: 200,
                names_distinct: 2,
                name_bytes: 21,
                note_bytes: 1,
            },
        )
        .unwrap();
        assert_eq!(buf, b"_UNMATCHED 2 15 1.00 -1.00 2.00 2 21 1\n");
    }
}
