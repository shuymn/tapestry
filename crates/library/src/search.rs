//! Search and sort (design.md "Loading and Song Selection"): the normalized
//! text the scan stores with each chart, and the one statement a query runs.
//!
//! **Normalization.** [`normalize`] applies NFKC, lowercases, folds
//! katakana U+30A1-U+30F6 to hiragana and drops C0 control characters, the
//! same way when indexing and when querying. Neither side holds NUL, at
//! which SQLite's `LIKE` and string parsing would stop.
//!
//! **Matching.** A query matches a chart when one of its title, subtitle,
//! artist, subartist or genre, normalized, contains the normalized query as
//! a substring. The fields are stored in `norm_text` joined by
//! [`SEPARATOR`], a C0 character no normalized query holds, so no match
//! spans two fields. A query of three or more characters (Unicode scalar
//! values after normalization, the unit of the trigram tokenizer) is one
//! FTS5 phrase, `"` doubled and the whole quoted, which finds the
//! candidates; `instr` then keeps those that contain the query exactly,
//! since the tokenizer maps U+FFFE and U+FFFF to U+FFFD. A shorter query is
//! a `LIKE` pattern with `\`, `%` and `_` escaped. A query that normalizes
//! to nothing matches every chart. SQLite's query syntax is never exposed.
//!
//! **Snapshot.** A query is one statement on the reader connection, so in
//! WAL mode it reads the last committed batch of a running scan and neither
//! waits for the writer nor holds it up.

use std::fmt::Write as _;

use bms_parser::Header;
use rusqlite::{Connection, ToSql};
use unicode_normalization::UnicodeNormalization as _;

use crate::ChartEntry;
use crate::db;

/// Joins the fields of `norm_text`: a C0 control character, which
/// [`normalize`] drops from the fields and from every query.
pub const SEPARATOR: char = '\u{1F}';

/// Normalized characters from which a query is an FTS5 phrase: the trigram
/// tokenizer matches nothing shorter.
const PHRASE_CHARS: usize = 3;

/// How results are ordered. Every order ends with the path, which is unique,
/// so it is total and the same on every query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    /// By normalized title, in code point order; charts with no title last.
    #[default]
    Title,
    /// By normalized artist, then as [`Sort::Title`]; no artist last.
    Artist,
    /// By `#PLAYLEVEL`: levels of ASCII digits only, by their value at any
    /// length, then any other level by its text, then no level; equal
    /// values by their text (`07` before `7`), then as [`Sort::Title`].
    Level,
    /// By file modification time, newest first.
    Modified,
}

/// A search of the library.
#[derive(Clone, Copy, Debug)]
pub struct Search<'a> {
    /// What to look for, as typed. Empty matches every chart.
    pub text: &'a str,
    /// The order.
    pub sort: Sort,
    /// Groups the charts by folder: folders by path, each in `sort` order.
    pub by_folder: bool,
    /// The most charts returned. Nothing is allocated for results that do
    /// not exist, so any value is safe.
    pub limit: usize,
}

/// `text` normalized for searching: NFKC, lowercase, katakana U+30A1-
/// U+30F6 folded to hiragana, and C0 control characters dropped.
pub fn normalize(text: &str) -> String {
    text.nfkc()
        .flat_map(char::to_lowercase)
        .map(fold_kana)
        .filter(|c| !matches!(c, '\0'..='\u{1F}'))
        .collect()
}

/// Katakana U+30A1-U+30F6 to the hiragana 0x60 below it.
fn fold_kana(c: char) -> char {
    if ('\u{30A1}'..='\u{30F6}').contains(&c) {
        char::from_u32(u32::from(c) - 0x60).unwrap_or(c)
    } else {
        c
    }
}

/// The normalized values the scan stores beside a chart's header.
#[derive(Debug)]
pub struct Keys {
    /// The searched fields, joined by [`SEPARATOR`].
    pub text: String,
    /// The title's sort key.
    pub title: Option<String>,
    /// The artist's sort key.
    pub artist: Option<String>,
}

impl Keys {
    pub fn of(header: &Header) -> Self {
        let norm = |field: &Option<String>| field.as_deref().map(normalize);
        let title = norm(&header.title);
        let artist = norm(&header.artist);
        let (subtitle, subartist, genre) = (
            norm(&header.subtitle),
            norm(&header.subartist),
            norm(&header.genre),
        );
        let mut text = String::new();
        for (i, field) in [&title, &subtitle, &artist, &subartist, &genre]
            .into_iter()
            .enumerate()
        {
            if i > 0 {
                text.push(SEPARATOR);
            }
            text.push_str(field.as_deref().unwrap_or_default());
        }
        Self {
            text,
            title,
            artist,
        }
    }
}

/// How the rows are chosen.
#[derive(Debug, PartialEq, Eq)]
enum Filter {
    /// Every chart.
    All,
    /// `charts_fts MATCH ?1` with `phrase`, and `instr(norm_text, ?2)` with
    /// the normalized query `needle`.
    Phrase { phrase: String, needle: String },
    /// `norm_text LIKE ?1 ESCAPE '\'`, with this pattern.
    Like(String),
}

impl Filter {
    fn of(text: &str) -> Self {
        let norm = normalize(text);
        if norm.is_empty() {
            Self::All
        } else if norm.chars().count() >= PHRASE_CHARS {
            Self::Phrase {
                phrase: format!("\"{}\"", norm.replace('"', "\"\"")),
                needle: norm,
            }
        } else {
            let mut pattern = String::from("%");
            for c in norm.chars() {
                if matches!(c, '\\' | '%' | '_') {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
            pattern.push('%');
            Self::Like(pattern)
        }
    }
}

/// A level of ASCII digits only.
const NUMERIC_LEVEL: &str = "c.playlevel <> '' AND c.playlevel NOT GLOB '*[^0-9]*'";

/// The significant digits of a numeric level. Ordered by their count, then
/// as text, they order the values at any length, where a cast to an integer
/// would saturate.
const LEVEL_DIGITS: &str = "ltrim(c.playlevel, '0')";

/// The `ORDER BY` terms of `sort`, after the folder when grouping.
fn order(sort: Sort, by_folder: bool) -> String {
    let mut terms = String::new();
    if by_folder {
        terms.push_str("c.folder, ");
    }
    // The terms of the `charts_title` index, so the unfiltered title order
    // reads it rather than sorting every row.
    let by_title = "c.title_key IS NULL, c.title_key, c.path";
    match sort {
        Sort::Title => terms.push_str(by_title),
        Sort::Artist => {
            let _ = write!(terms, "c.artist_key NULLS LAST, {by_title}");
        }
        Sort::Level => {
            let _ = write!(
                terms,
                "CASE WHEN c.playlevel IS NULL THEN 2 WHEN {NUMERIC_LEVEL} THEN 0 ELSE 1 END, \
                 CASE WHEN {NUMERIC_LEVEL} THEN length({LEVEL_DIGITS}) END, \
                 CASE WHEN {NUMERIC_LEVEL} THEN {LEVEL_DIGITS} END, \
                 c.playlevel, {by_title}"
            );
        }
        Sort::Modified => terms.push_str("c.mtime_ns DESC, c.path"),
    }
    terms
}

/// The charts `search` finds, in its order, at most its limit.
pub fn run(conn: &Connection, search: &Search<'_>) -> rusqlite::Result<Vec<ChartEntry>> {
    let limit = i64::try_from(search.limit).unwrap_or(i64::MAX);
    let filter = Filter::of(search.text);
    let (from, mut args): (&str, Vec<&dyn ToSql>) = match &filter {
        Filter::All => ("charts c", Vec::new()),
        Filter::Phrase { phrase, needle } => (
            "charts_fts JOIN charts c ON c.id = charts_fts.rowid
             WHERE charts_fts MATCH ?1 AND instr(c.norm_text, ?2) > 0",
            vec![phrase, needle],
        ),
        Filter::Like(pattern) => (
            r"charts c WHERE c.norm_text LIKE ?1 ESCAPE '\'",
            vec![pattern],
        ),
    };
    // The limit is the last parameter, in the candidates' query, so it
    // counts only rows that match.
    args.push(&limit);
    let order = order(search.sort, search.by_folder);
    let sql = format!(
        "SELECT {} FROM charts c WHERE c.id IN (
             SELECT c.id FROM {from} ORDER BY {order} LIMIT ?{}
         ) ORDER BY {order}",
        db::ENTRY_COLUMNS,
        args.len(),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query(args.as_slice())?;
    rows.mapped(db::entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_width_case_and_kana() {
        assert_eq!(normalize("ＡＢＣ"), "abc");
        assert_eq!(normalize("ｶﾞｷﾞ"), "がぎ");
        assert_eq!(normalize("カタカナ ヴ ヶ"), "かたかな ゔ ゖ");
        // Outside U+30A1-U+30F6: the long sound mark and U+30F7.
        assert_eq!(normalize("ーヷ"), "ーヷ");
        assert_eq!(normalize("Ⅻ"), "xii");
    }

    #[test]
    fn short_queries_escape_like_wildcards() {
        assert_eq!(Filter::of("%"), Filter::Like(r"%\%%".to_owned()));
        assert_eq!(Filter::of("a_"), Filter::Like(r"%a\_%".to_owned()));
        assert_eq!(Filter::of(r"\"), Filter::Like(r"%\\%".to_owned()));
    }

    #[test]
    fn long_queries_are_one_quoted_phrase() {
        assert_eq!(
            Filter::of(r#"say "hi""#),
            Filter::Phrase {
                phrase: r#""say ""hi""""#.to_owned(),
                needle: r#"say "hi""#.to_owned(),
            }
        );
        // Counted after normalization: one character of NFKC "ｶﾞ".
        assert_eq!(Filter::of("ｶﾞa"), Filter::Like("%がa%".to_owned()));
    }

    #[test]
    fn the_title_order_reads_its_index() {
        let conn = Connection::open_in_memory().unwrap();
        db::create(&conn).unwrap();
        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("; ")
        };
        let title = plan(&format!(
            "SELECT c.id FROM charts c ORDER BY {} LIMIT 200",
            order(Sort::Title, false)
        ));
        assert!(title.contains("charts_title"), "{title}");
        assert!(!title.contains("TEMP B-TREE"), "{title}");
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(normalize("a\0b\u{1F}c\td\u{7F}"), "abcd\u{7F}");
        assert_eq!(Filter::of(""), Filter::All);
        assert_eq!(Filter::of("\0\n\u{1F}"), Filter::All);
        assert_eq!(Filter::of("a\0b"), Filter::Like("%ab%".to_owned()));
    }
}
