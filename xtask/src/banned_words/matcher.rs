//! The banned-word list and matching.
//!
//! After NFKC, a term made of ASCII letters and digits (one or more tokens
//! separated by spaces, `-` or `_`) is matched as whole words: it matches a
//! run of consecutive text tokens (see [`tokenize`]) whose concatenation
//! equals the term, ignoring case. Between two text tokens of the run, a
//! separator character is allowed only where the term has a token boundary,
//! so `quux corp` matches `QuuxCorp`, `quux_corp`, and `quuxcorp`, while
//! `zorblax` matches `newZorblaxPlayer` but not `zorblaxes`, `azorblax`, or
//! `zorb lax`. A term containing Han, Kana, or Hangul is matched as a
//! substring of the NFKC, lowercased text. Any other term is a list error.

use super::normalize::{is_cjk, lowercase_with_map, nfkc, tokenize};

#[derive(Debug)]
pub struct Term {
    /// Line in the list file (1-based).
    pub line: usize,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// Lowercase tokens joined without separators, and the byte offsets in
    /// it where a separator may appear in the text.
    Word {
        joined: String,
        boundaries: Vec<usize>,
    },
    /// NFKC, lowercased term.
    Substring(String),
}

/// A match: the index of the term in the list and the byte range in the
/// NFKC text.
#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    pub term: usize,
    pub start: usize,
    pub end: usize,
}

/// Parses the list. Errors name the list line but never the term.
pub fn parse_list(name: &str, text: &str) -> Result<Vec<Term>, String> {
    let mut terms: Vec<Term> = Vec::new();
    let mut errors = Vec::new();
    let mut seen: Vec<(String, usize)> = Vec::new();
    // Editors on Windows may save the list with a UTF-8 BOM.
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let normalized = nfkc(raw);
        let term = normalized.trim();
        if term.is_empty() || term.starts_with('#') {
            continue;
        }
        let (kind, key) = if is_word_term(term) {
            let tokens = tokenize(term);
            let mut joined = String::new();
            let mut boundaries = Vec::new();
            for t in &tokens {
                if !joined.is_empty() {
                    boundaries.push(joined.len());
                }
                joined.push_str(&t.text);
            }
            let key = format!("w:{joined}");
            (Kind::Word { joined, boundaries }, key)
        } else if term.chars().any(is_cjk) {
            let lower = term.to_lowercase();
            let key = format!("s:{lower}");
            (Kind::Substring(lower), key)
        } else {
            errors.push(format!(
                "{name}:{line}: a term must be ASCII letters and digits separated by spaces, \
                 '-' or '_', or contain Han, Kana, or Hangul characters"
            ));
            continue;
        };
        if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
            errors.push(format!(
                "{name}:{line}: duplicate of the term on line {first} after normalization"
            ));
            continue;
        }
        seen.push((key, line));
        terms.push(Term { line, kind });
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if terms.is_empty() {
        return Err(format!("{name}: list has no terms"));
    }
    Ok(terms)
}

/// One or more runs of ASCII letters and digits separated by spaces, `-` or
/// `_`.
fn is_word_term(term: &str) -> bool {
    term.split([' ', '-', '_'])
        .filter(|part| !part.is_empty())
        .all(|part| part.bytes().all(|b| b.is_ascii_alphanumeric()))
        && term.starts_with(|c: char| c.is_ascii_alphanumeric())
        && term.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// Every match of every term in `text` (NFKC contents or path).
pub fn find(terms: &[Term], text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let tokens = tokenize(text);
    let mut lower: Option<(String, Vec<(usize, usize)>)> = None;
    for (index, term) in terms.iter().enumerate() {
        match &term.kind {
            Kind::Word { joined, boundaries } => {
                for i in 0..tokens.len() {
                    let mut pos = 0;
                    for (j, tok) in tokens.iter().enumerate().skip(i) {
                        if j > i && tok.gap_before && !boundaries.contains(&pos) {
                            break;
                        }
                        if !joined[pos..].starts_with(tok.text.as_str()) {
                            break;
                        }
                        pos += tok.text.len();
                        if pos == joined.len() {
                            hits.push(Hit {
                                term: index,
                                start: tokens[i].start,
                                end: tok.end,
                            });
                            break;
                        }
                    }
                }
            }
            Kind::Substring(needle) => {
                let (lower, map) = lower.get_or_insert_with(|| lowercase_with_map(text));
                for (a, m) in lower.match_indices(needle.as_str()) {
                    let b = a + m.len();
                    let start = map
                        .iter()
                        .rev()
                        .find(|(lo, _)| *lo <= a)
                        .map_or(0, |&(_, src)| src);
                    let end = map
                        .iter()
                        .find(|(lo, _)| *lo >= b)
                        .map_or(text.len(), |&(_, src)| src);
                    hits.push(Hit {
                        term: index,
                        start,
                        end,
                    });
                }
            }
        }
    }
    hits.sort_by_key(|h| (h.start, h.term));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "# header\n\nzorblax\nquux corp\nテストメイ\n";

    fn terms() -> Vec<Term> {
        parse_list("list", LIST).unwrap()
    }

    /// Indices of the terms found in `text` after NFKC.
    fn found(text: &str) -> Vec<usize> {
        find(&terms(), &nfkc(text)).iter().map(|h| h.term).collect()
    }

    #[test]
    fn word_mode_positives() {
        for text in [
            "zorblax",
            "Zorblax",
            "ZORBLAX_X",
            "newZorblaxPlayer",
            "ｚｏｒｂｌａｘ",
            "ＺＯＲＢＬＡＸ",
            "zorblax2",
            "ZorBlax",
            "a zorblax.",
            "テストzorblaxのテスト",
        ] {
            assert_eq!(found(text), [0], "{text}");
        }
        for text in [
            "quux corp",
            "QuuxCorp",
            "quux_corp",
            "quuxcorp",
            "quux-corp",
        ] {
            assert_eq!(found(text), [1], "{text}");
        }
    }

    #[test]
    fn word_mode_negatives() {
        for text in [
            "zorblaxes",
            "azorblax",
            "zorb lax",
            "zorbla",
            "quux",
            "quuxcorpx",
            "quux corporation",
        ] {
            assert_eq!(found(text), Vec::<usize>::new(), "{text}");
        }
    }

    #[test]
    fn camel_case_term_allows_separator_at_its_boundary() {
        let terms = parse_list("list", "QuuxCorp\n").unwrap();
        assert_eq!(find(&terms, "quux corp").len(), 1);
        assert_eq!(find(&terms, "quuxcorp").len(), 1);
    }

    #[test]
    fn substring_mode() {
        for text in ["テストメイ", "あテストメイあ", "ﾃｽﾄﾒｲ", "xテストメイx"]
        {
            assert_eq!(found(text), [2], "{text}");
        }
        assert_eq!(found("てすとめい"), Vec::<usize>::new());
        assert_eq!(found("テスト メイ"), Vec::<usize>::new());
    }

    #[test]
    fn mixed_script_term_is_substring_and_case_insensitive() {
        let terms = parse_list("list", "テストZorb\n").unwrap();
        assert_eq!(find(&terms, &nfkc("xテストｚｏｒｂlax")).len(), 1);
    }

    #[test]
    fn hit_ranges_cover_the_match() {
        let text = nfkc("docs/newZorblaxPlayer/テストメイ.md");
        let hits = find(&terms(), &text);
        let parts: Vec<&str> = hits.iter().map(|h| &text[h.start..h.end]).collect();
        assert_eq!(parts, ["Zorblax", "テストメイ"]);
    }

    #[test]
    fn substring_range_survives_length_changing_lowercase() {
        let terms = parse_list("list", "テストメイ\n").unwrap();
        let text = "İİテストメイ";
        let hits = find(&terms, text);
        assert_eq!(&text[hits[0].start..hits[0].end], "テストメイ");
    }

    #[test]
    fn list_errors_never_echo_the_term() {
        let err = parse_list("list", "zorblax\nZORBLAX\nzorb!ax\n-zorblax\n").unwrap_err();
        assert!(
            err.contains("list:2: duplicate of the term on line 1"),
            "{err}"
        );
        assert!(err.contains("list:3: a term must be"), "{err}");
        assert!(err.contains("list:4: a term must be"), "{err}");
        assert!(!err.to_lowercase().contains("zorb"), "{err}");
    }

    #[test]
    fn duplicates_are_found_after_normalization() {
        assert!(parse_list("list", "quux corp\nQuux_Corp\n").is_err());
        assert!(parse_list("list", "ｚｏｒｂｌａｘ\nzorblax\n").is_err());
        assert!(parse_list("list", "ﾃｽﾄﾒｲ\nテストメイ\n").is_err());
    }

    #[test]
    fn empty_list_is_an_error() {
        let err = parse_list("list", "# only a header\n\n").unwrap_err();
        assert_eq!(err, "list: list has no terms");
        assert!(parse_list("list", "\u{3000}\n").is_err());
    }

    #[test]
    fn leading_bom_is_ignored() {
        let terms = parse_list("list", "\u{FEFF}zorblax\n").unwrap();
        assert_eq!(find(&terms, "Zorblax").len(), 1);
        let terms = parse_list("list", "\u{FEFF}# header\nzorblax\n").unwrap();
        assert_eq!(terms.len(), 1);
    }

    #[test]
    fn full_width_comment_and_space_are_normalized() {
        let terms = parse_list("list", "＃ comment\n\u{3000}zorblax\u{3000}\n").unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].line, 2);
    }
}
