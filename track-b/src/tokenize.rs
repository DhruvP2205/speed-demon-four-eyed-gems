//! Byte-based tokenizer, specs/b-index.md section 2.2. This rule is
//! the WHOLE definition of a token, deliberately not Unicode-aware:
//! ASCII-lowercase the bytes (0x41-0x5A -> 0x61-0x7A, nothing else
//! changes), then a token is a maximal run of bytes in 0x61-0x7A
//! (`a`-`z`) or 0x30-0x39 (`0`-`9`); every other byte, INCLUDING every
//! byte of 0x80 or above (so every continuation/lead byte of a
//! multi-byte UTF-8 sequence), is a separator. This means the
//! tokenizer never needs to decode UTF-8 at all -- a non-ASCII
//! character's bytes are, byte for byte, just more separator bytes,
//! which is exactly what "Zurich" from `Zürich` (-> `z`, `rich`) and
//! `stanbul` from `İSTANBUL` (-> the leading İ's two bytes are both
//! separators, `STANBUL` lowers to one token) require.

/// No token in a generated corpus is longer than 32 bytes (spec
/// section 2.2), and no query term can be either (grammar, section
/// 2.3: `term = 1*32(...)`) -- so a token longer than this can never
/// equal any term a query could ask about, and is skipped entirely
/// rather than truncated (truncating could make two genuinely
/// different long tokens compare equal).
pub const MAX_TOKEN_LEN: usize = 32;

#[inline]
fn to_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

#[inline]
fn is_token_byte(b: u8) -> bool {
    let lower = to_lower(b);
    lower.is_ascii_lowercase() || lower.is_ascii_digit()
}

/// Calls `f(position, lowered_token)` for every token in `text`, in
/// order, `position` 0-based and counting EVERY token (including ones
/// over `MAX_TOKEN_LEN`, skipped from `f` but still consuming a
/// position) -- phrase matching needs position numbers to stay
/// consistent with the real token stream regardless of which specific
/// tokens happen to be short enough to ever match a query term.
/// `buf` is a caller-owned scratch buffer, reused across calls: no
/// allocation per token, only a `MAX_TOKEN_LEN`-byte stack copy for
/// tokens short enough to possibly matter.
pub fn for_each_token(text: &[u8], buf: &mut [u8; MAX_TOKEN_LEN], mut f: impl FnMut(u32, &[u8])) {
    let mut position: u32 = 0;
    let n = text.len();
    let mut i = 0;
    while i < n {
        if is_token_byte(text[i]) {
            let start = i;
            while i < n && is_token_byte(text[i]) {
                i += 1;
            }
            let len = i - start;
            if len <= MAX_TOKEN_LEN {
                for (j, &b) in text[start..i].iter().enumerate() {
                    buf[j] = to_lower(b);
                }
                f(position, &buf[..len]);
            }
            position += 1;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &[u8]) -> Vec<Vec<u8>> {
        let mut buf = [0u8; MAX_TOKEN_LEN];
        let mut out = Vec::new();
        for_each_token(text, &mut buf, |_pos, tok| out.push(tok.to_vec()));
        out
    }

    fn positions(text: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut buf = [0u8; MAX_TOKEN_LEN];
        let mut out = Vec::new();
        for_each_token(text, &mut buf, |pos, tok| out.push((pos, tok.to_vec())));
        out
    }

    #[test]
    fn simple_ascii_words() {
        assert_eq!(
            tokens(b"The surety posted bail"),
            vec![
                b"the".to_vec(),
                b"surety".to_vec(),
                b"posted".to_vec(),
                b"bail".to_vec()
            ]
        );
    }

    #[test]
    fn digits_and_letters_are_both_token_bytes_but_case_only_folds_letters() {
        assert_eq!(tokens(b"007 ABC"), vec![b"007".to_vec(), b"abc".to_vec()]);
    }

    #[test]
    fn zurich_splits_on_the_non_ascii_u_with_diaeresis() {
        // Z-u-with-diaeresis-rich: the 2-byte u-umlaut is pure separator bytes.
        assert_eq!(
            tokens("Zürich".as_bytes()),
            vec![b"z".to_vec(), b"rich".to_vec()]
        );
    }

    #[test]
    fn naive_splits_on_the_non_ascii_i_with_diaeresis() {
        assert_eq!(
            tokens("naïve".as_bytes()),
            vec![b"na".to_vec(), b"ve".to_vec()]
        );
    }

    #[test]
    fn kelvin_sign_is_a_separator_not_the_letter_k() {
        // "273K" with the real KELVIN SIGN (U+212A, 3 UTF-8 bytes) is just "273" --
        // the Kelvin sign's bytes are all >= 0x80, so they produce no token at all.
        let mut text = b"273".to_vec();
        text.extend_from_slice("\u{212A}".as_bytes());
        assert_eq!(tokens(&text), vec![b"273".to_vec()]);
    }

    #[test]
    fn turkish_capital_i_with_dot_above_leaves_only_stanbul() {
        // U+0130 (2 UTF-8 bytes) then ASCII "STANBUL" lowers to "stanbul".
        let mut text = "\u{0130}".as_bytes().to_vec();
        text.extend_from_slice(b"STANBUL");
        assert_eq!(tokens(&text), vec![b"stanbul".to_vec()]);
    }

    #[test]
    fn punctuation_and_hyphens_separate_tokens() {
        assert_eq!(
            tokens(b"habeas-corpus, court's U.S.C."),
            vec![
                b"habeas".to_vec(),
                b"corpus".to_vec(),
                b"court".to_vec(),
                b"s".to_vec(),
                b"u".to_vec(),
                b"s".to_vec(),
                b"c".to_vec()
            ]
        );
    }

    #[test]
    fn empty_text_has_no_tokens() {
        assert_eq!(tokens(b""), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn positions_are_assigned_in_order_starting_at_zero() {
        assert_eq!(
            positions(b"a b c"),
            vec![(0, b"a".to_vec()), (1, b"b".to_vec()), (2, b"c".to_vec())]
        );
    }

    #[test]
    fn a_token_longer_than_32_bytes_is_skipped_but_still_consumes_a_position() {
        let long_token = "a".repeat(33);
        let text = format!("{long_token} b");
        // The 33-byte token is skipped (never passed to `f`), but `b`
        // still gets position 1, not 0 -- proving the skipped token's
        // position was still counted.
        assert_eq!(positions(text.as_bytes()), vec![(1, b"b".to_vec())]);
    }

    #[test]
    fn a_token_exactly_32_bytes_is_kept() {
        let token = "a".repeat(32);
        assert_eq!(tokens(token.as_bytes()), vec![token.into_bytes()]);
    }
}
