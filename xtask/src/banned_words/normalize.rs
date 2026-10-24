//! Text normalization and word tokenization shared by terms and scanned text.
//!
//! Text is NFKC-normalized (full-width Latin becomes ASCII, half-width kana
//! becomes full-width) and lowercased. Hiragana and katakana are not folded.

use unicode_normalization::UnicodeNormalization;

/// NFKC form of `s`.
pub fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

/// Whether `c` is Han, Hiragana, Katakana, or Hangul. Terms containing such a
/// character are matched as substrings.
pub fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'     // Hangul Jamo
        | '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}'   // Katakana
        | '\u{3130}'..='\u{318F}'   // Hangul Compatibility Jamo
        | '\u{31F0}'..='\u{31FF}'   // Katakana Phonetic Extensions
        | '\u{3400}'..='\u{4DBF}'   // CJK Unified Ideographs Extension A
        | '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{A960}'..='\u{A97F}'   // Hangul Jamo Extended-A
        | '\u{AC00}'..='\u{D7FF}'   // Hangul Syllables, Jamo Extended-B
        | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
        | '\u{1B000}'..='\u{1B16F}' // Kana Supplement, Kana Extended-A, Small Kana
        | '\u{20000}'..='\u{3FFFF}' // CJK Unified Ideographs Extensions B and later
    )
}

/// Lowercases `s` character by character. Returns the lowercased text and,
/// for each character of `s`, `(byte offset in the result, byte offset in s)`,
/// so a range in the result maps back to `s`.
pub fn lowercase_with_map(s: &str) -> (String, Vec<(usize, usize)>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        map.push((out.len(), i));
        out.extend(c.to_lowercase());
    }
    (out, map)
}

/// A run of ASCII letters and digits split at case and letter/digit changes.
#[derive(Debug, PartialEq, Eq)]
pub struct Token {
    /// Byte range in the tokenized text.
    pub start: usize,
    pub end: usize,
    /// Lowercase token text.
    pub text: String,
    /// Whether a non-alphanumeric character separates this token from the
    /// previous one (as opposed to a case or letter/digit change).
    pub gap_before: bool,
}

/// Splits `s` (already NFKC) into tokens. Everything except ASCII letters and
/// digits separates tokens. Inside a run, a token ends at lower→Upper, at
/// letter↔digit, and before the last capital of an acronym followed by a
/// lowercase letter (`HTTPServer` → `HTTP`, `Server`).
pub fn tokenize(s: &str) -> Vec<Token> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    let mut gap = true;
    for (k, &(i, c)) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if let Some(s0) = start.take() {
                tokens.push(token(s, s0, i, gap));
            }
            gap = true;
            continue;
        }
        if let Some(s0) = start {
            let prev = chars[k - 1].1;
            let next = chars.get(k + 1).map(|&(_, n)| n);
            if is_split(prev, c, next) {
                tokens.push(token(s, s0, i, gap));
                gap = false;
                start = Some(i);
            }
        } else {
            start = Some(i);
        }
    }
    if let Some(s0) = start {
        tokens.push(token(s, s0, s.len(), gap));
    }
    tokens
}

fn token(s: &str, start: usize, end: usize, gap_before: bool) -> Token {
    Token {
        start,
        end,
        text: s[start..end].to_ascii_lowercase(),
        gap_before,
    }
}

fn is_split(prev: char, c: char, next: Option<char>) -> bool {
    (prev.is_ascii_lowercase() && c.is_ascii_uppercase())
        || (prev.is_ascii_alphabetic() && c.is_ascii_digit())
        || (prev.is_ascii_digit() && c.is_ascii_alphabetic())
        || (prev.is_ascii_uppercase()
            && c.is_ascii_uppercase()
            && next.is_some_and(|n| n.is_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        tokenize(&nfkc(s)).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn nfkc_folds_width() {
        assert_eq!(nfkc("ｚｏｒｂｌａｘ"), "zorblax");
        assert_eq!(nfkc("ＺＯＲＢＬＡＸ"), "ZORBLAX");
        assert_eq!(nfkc("ﾃｽﾄﾒｲ"), "テストメイ");
        assert_eq!(nfkc("ｶﾞ"), "ガ");
    }

    #[test]
    fn hiragana_is_not_folded_to_katakana() {
        assert_ne!(nfkc("てすとめい"), nfkc("テストメイ"));
    }

    #[test]
    fn tokenizes_camel_case() {
        assert_eq!(words("newZorblaxPlayer"), ["new", "zorblax", "player"]);
    }

    #[test]
    fn tokenizes_acronyms() {
        assert_eq!(words("HTTPServer"), ["http", "server"]);
        assert_eq!(words("ZORBLAX_X"), ["zorblax", "x"]);
        assert_eq!(words("parseURL"), ["parse", "url"]);
    }

    #[test]
    fn tokenizes_snake_kebab_and_paths() {
        assert_eq!(words("quux_corp-x"), ["quux", "corp", "x"]);
        assert_eq!(words("docs/zorblax.md"), ["docs", "zorblax", "md"]);
    }

    #[test]
    fn tokenizes_digits() {
        assert_eq!(words("zorblax2"), ["zorblax", "2"]);
        assert_eq!(words("v2Zorblax"), ["v", "2", "zorblax"]);
    }

    #[test]
    fn records_gaps_and_ranges() {
        let t = tokenize("a bC");
        assert_eq!(
            t,
            [
                Token {
                    start: 0,
                    end: 1,
                    text: "a".into(),
                    gap_before: true
                },
                Token {
                    start: 2,
                    end: 3,
                    text: "b".into(),
                    gap_before: true
                },
                Token {
                    start: 3,
                    end: 4,
                    text: "c".into(),
                    gap_before: false
                },
            ]
        );
    }

    #[test]
    fn non_ascii_separates_tokens() {
        assert_eq!(words("テストzorblaxのテスト"), ["zorblax"]);
    }

    #[test]
    fn lowercase_map_points_back() {
        let (lower, map) = lowercase_with_map("AÉb");
        assert_eq!(lower, "aéb");
        assert_eq!(map, [(0, 0), (1, 1), (3, 3)]);
    }
}
