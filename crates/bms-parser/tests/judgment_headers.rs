//! Judgment-header declarations (`#RANK`, `#DEFEXRANK`, `#TOTAL`): the
//! loader keeps every active one in file order and canonical form
//! (design.md "Chart Model").

use std::fs;

use bms_model::HeaderValue::{self, Decimal, Integer, NotANumber};
use bms_model::JudgmentHeader;
use bms_model::JudgmentHeaderName::{self, DefExRank, Rank, Total};
use bms_parser::{Limits, LoadError, LoadOptions, Loaded, Warning, WarningKind, load_bms};
use proptest::prelude::*;
use tempfile::TempDir;

const ONE: i128 = HeaderValue::ONE;

/// One key-1 note, so the chart has something to play.
const NOTE: &str = "#BPM 120\n#WAV01 click.wav\n#00111:01\n";

fn load_with(text: &str, limits: Limits) -> Result<Loaded, LoadError> {
    let dir = TempDir::new().unwrap();
    // The loader only resolves the path; the bytes need not be audio.
    fs::write(dir.path().join("click.wav"), b"RIFF").unwrap();
    load_bms(
        text.as_bytes(),
        dir.path(),
        &LoadOptions {
            limits,
            ..LoadOptions::default()
        },
    )
}

fn load(text: &str) -> Loaded {
    load_with(text, Limits::default()).unwrap()
}

fn headers(text: &str) -> Vec<JudgmentHeader> {
    load(text).chart.meta.judgment_headers
}

fn h(name: JudgmentHeaderName, value: HeaderValue) -> JudgmentHeader {
    JudgmentHeader { name, value }
}

fn hash(text: &str) -> [u8; 32] {
    load(text).chart.identity.resolved_hash
}

#[test]
fn rejected_defexrank_keeps_the_earlier_rank() {
    // The loader keeps both, in order, with their classes; which one counts
    // is the preset's (crates/eval/tests/style_beatoraja.rs has the same
    // test for beatoraja-style's reading).
    let loaded = load(&format!("#RANK 3\n#DEFEXRANK 120.5\n{NOTE}"));
    assert_eq!(
        loaded.chart.meta.judgment_headers,
        [
            h(Rank, Integer(3 * ONE)),
            h(DefExRank, Decimal(120 * ONE + ONE / 2))
        ]
    );
    assert_eq!(loaded.warnings, []);
}

#[test]
fn integer_and_decimal_tokens_stay_distinct() {
    let integer = format!("#DEFEXRANK 120\n{NOTE}");
    let decimal = format!("#DEFEXRANK 120.0\n{NOTE}");
    assert_eq!(headers(&integer), [h(DefExRank, Integer(120 * ONE))]);
    assert_eq!(headers(&decimal), [h(DefExRank, Decimal(120 * ONE))]);
    assert_ne!(hash(&integer), hash(&decimal));
    // So do a decimal's values, and the header that declares one.
    assert_ne!(hash(&decimal), hash(&format!("#DEFEXRANK 120.5\n{NOTE}")));
    assert_ne!(hash(&decimal), hash(&format!("#TOTAL 120.0\n{NOTE}")));
    // And the order of two declarations.
    assert_ne!(
        hash(&format!("#RANK 3\n#TOTAL 200\n{NOTE}")),
        hash(&format!("#TOTAL 200\n#RANK 3\n{NOTE}"))
    );
}

#[test]
fn inactive_branch_declarations_are_dropped() {
    let text = format!(
        "#RANK 2\n\
         #SETRANDOM 2\n\
         #IF 1\n#RANK 0\n#TOTAL 100\n\
         #ELSE\n#DEFEXRANK 80\n\
         #SETRANDOM 1\n#IF 1\n#TOTAL 300\n#ENDIF\n#IF 2\n#TOTAL 400\n#ENDIF\n#ENDRANDOM\n\
         #ENDIF\n#ENDRANDOM\n\
         #IF 1\n#TOTAL 500\n#ENDIF\n\
         {NOTE}"
    );
    // The stray #IF after the blocks has no value to test and pushes no
    // flag, so its #TOTAL is read.
    assert_eq!(
        headers(&text),
        [
            h(Rank, Integer(2 * ONE)),
            h(DefExRank, Integer(80 * ONE)),
            h(Total, Integer(300 * ONE)),
            h(Total, Integer(500 * ONE)),
        ]
    );
}

#[test]
fn header_canonical_form_ignores_text() {
    let base = hash(&format!("#RANK 3\n#DEFEXRANK 120.5\n#TOTAL 250.5\n{NOTE}"));
    for text in [
        // Whitespace, around and inside the line.
        format!("  #RANK\t3  \n#DEFEXRANK   120.5\r\n#TOTAL 250.5 \n{NOTE}"),
        // The header's case.
        format!("#rank 3\n#DefExRank 120.5\n#total 250.5\n{NOTE}"),
        // Line positions: blank lines, comments and other headers between.
        format!("\n; a comment\n#RANK 3\n\n#TITLE \n#DEFEXRANK 120.5\n\n\n#TOTAL 250.5\n{NOTE}"),
        // Another spelling of the same value in the same class.
        format!("#RANK +003\n#DEFEXRANK 120.50\n#TOTAL 2.505E2\n{NOTE}"),
    ] {
        assert_eq!(hash(&text), base, "{text:?}");
    }
}

#[test]
fn unreadable_header_is_kept_and_warned() {
    let loaded = load(&format!("#RANK\n#DEFEXRANK easy\n#TOTAL 1,5\n{NOTE}"));
    assert_eq!(
        loaded.chart.meta.judgment_headers,
        [
            h(Rank, NotANumber),
            h(DefExRank, NotANumber),
            h(Total, NotANumber)
        ]
    );
    assert_eq!(
        loaded.warnings,
        ["#RANK", "#DEFEXRANK", "#TOTAL"]
            .into_iter()
            .zip(1..)
            .map(|(what, line)| Warning {
                line: Some(line),
                kind: WarningKind::NotANumber(what),
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(loaded.chart.lanes[1].len(), 1, "the chart still loads");
    // Values that are numbers are not warned, whatever a preset makes of
    // them.
    assert_eq!(
        load(&format!("#RANK 9\n#DEFEXRANK -2\n#TOTAL 0\n{NOTE}")).warnings,
        []
    );
}

/// What beatoraja-style's header policy reads from `text`.
fn read_by_beatoraja_style(text: &str) -> rules::ReadHeaders {
    rules::beatoraja_style()
        .headers
        .read(&headers(&format!("{text}\n{NOTE}")))
}

/// A known difference (crates/rules/src/beatoraja_style.rs): a positive
/// value below 10^-18 is cut off to 0 here, so it is not above 0 and the
/// earlier `#TOTAL` stays; `Double.parseDouble` keeps it positive, so
/// jbms-parser takes it (BMSDecoder.java:730-743).
#[test]
fn a_total_below_the_kept_digits_keeps_the_earlier_one() {
    assert_eq!(
        headers(&format!("#TOTAL 200\n#TOTAL 1e-19\n{NOTE}")),
        [h(Total, Integer(200 * ONE)), h(Total, Decimal(0))]
    );
    assert_eq!(
        read_by_beatoraja_style("#TOTAL 200\n#TOTAL 1e-19").total_milli,
        Some(200_000)
    );
}

/// A known difference (crates/rules/src/beatoraja_style.rs): non-ASCII
/// whitespace around a value (U+00A0, U+3000) is trimmed here, so the
/// value is read; Java's `trim` keeps it and `Integer.parseInt` throws, so
/// jbms-parser keeps the earlier `#RANK` (BMSDecoder.java:367-368, :702-713).
#[test]
fn non_ascii_whitespace_around_a_value_is_trimmed() {
    for space in ['\u{a0}', '\u{3000}'] {
        let text = format!("#TITLE 判定\n#RANK 3\n#RANK {space}2{space}");
        assert_eq!(
            headers(&format!("{text}\n{NOTE}")),
            [h(Rank, Integer(3 * ONE)), h(Rank, Integer(2 * ONE))],
            "{space:?}"
        );
        assert_eq!(
            read_by_beatoraja_style(&text).rank,
            Some(rules::JudgeRank::Rank(2)),
            "{space:?}"
        );
    }
}

#[test]
fn the_fixture_keeps_its_active_declarations() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/charts/judgment_headers.bms");
    let text = String::from_utf8(fs::read(path).unwrap()).unwrap();
    for seed in 0..8 {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("click.wav"), b"RIFF").unwrap();
        let opts = LoadOptions {
            seed,
            ..LoadOptions::default()
        };
        let chart = load_bms(text.as_bytes(), dir.path(), &opts).unwrap().chart;
        let branch = match chart.identity.random_results[..] {
            [1] => h(Rank, Integer(ONE)),
            [2] => h(DefExRank, Integer(120 * ONE)),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(
            chart.meta.judgment_headers,
            [
                h(Rank, Integer(3 * ONE)),
                h(DefExRank, Decimal(120 * ONE + ONE / 2)),
                h(Total, Integer(200 * ONE)),
                h(Total, Integer(0)),
                branch,
                h(Total, NotANumber),
            ],
            "seed {seed}"
        );
    }
}

/// A small file of nothing but declarations, at the line limit: each is
/// one fixed-size entry backed by its own line, and one line more is
/// refused before it is read.
#[test]
fn declarations_are_bounded_by_the_line_limit() {
    let limits = Limits::default();
    let lines = limits.max_lines;
    let text = "#RANK 1\n".repeat(lines);
    // About 4 MB, a quarter of the file limit.
    assert!(text.len() < limits.max_file_bytes / 4);
    let loaded = load_with(&text, limits).unwrap();
    let kept = &loaded.chart.meta.judgment_headers;
    assert_eq!(kept.len(), lines);
    assert!(kept.iter().all(|&d| d == h(Rank, Integer(ONE))));
    // One 48-byte entry per 8-byte line (24 MB at the line limit), held at
    // its exact size.
    assert_eq!(size_of::<JudgmentHeader>(), 48);
    assert_eq!(kept.capacity(), lines);
    let more = format!("{text}#RANK 1\n");
    assert_eq!(
        load_with(&more, limits),
        Err(LoadError::TooManyLines { line: lines + 1 })
    );
}

/// A declaration's text and the value the loader keeps for it.
fn declaration() -> impl Strategy<Value = (String, HeaderValue)> {
    let sign = prop_oneof![Just(""), Just("+"), Just("-")];
    let integer =
        (sign.clone(), 0_usize..3, 0_i64..1_000_000_000_000).prop_map(|(sign, zeros, n)| {
            let v = i128::from(n) * ONE;
            let v = if sign == "-" { -v } else { v };
            (format!("{sign}{}{n}", "0".repeat(zeros)), Integer(v))
        });
    // `int.frac` with 1-18 fractional digits, or `int.` and `int.0`.
    let decimal = (sign, 0_i64..1_000_000_000_000, 0_usize..=18, any::<u64>()).prop_map(
        |(sign, int, digits, frac)| {
            let digits_u32 = u32::try_from(digits).unwrap();
            let frac = if digits == 0 {
                0
            } else {
                frac % 10_u64.pow(digits_u32)
            };
            let text = if digits == 0 {
                format!("{sign}{int}.")
            } else {
                format!("{sign}{int}.{frac:0width$}", width = digits)
            };
            let v = i128::from(int) * ONE + i128::from(frac) * 10_i128.pow(18 - digits_u32);
            (text, Decimal(if sign == "-" { -v } else { v }))
        },
    );
    let garbage = prop_oneof![
        Just(""),
        Just("easy"),
        Just("1,5"),
        Just("NaN"),
        Just("0x10"),
        Just("1e19"),
        Just("1000000000000"),
    ]
    .prop_map(|t| (t.to_owned(), NotANumber));
    prop_oneof![integer, decimal, garbage]
}

/// A declaration: its header and name as written, its value's text and
/// canonical form, and its layout `(filler lines, lowercase, padding)`.
type Decl = (
    (JudgmentHeaderName, &'static str),
    (String, HeaderValue),
    (u8, bool, u8),
);

fn header_name() -> impl Strategy<Value = (JudgmentHeaderName, &'static str)> {
    prop_oneof![
        Just((Rank, "RANK")),
        Just((DefExRank, "DEFEXRANK")),
        Just((Total, "TOTAL")),
    ]
}

/// The declarations, each with filler lines before it, a case for its
/// header, and padding around its value.
fn declarations() -> impl Strategy<Value = Vec<Decl>> {
    prop::collection::vec(
        (
            header_name(),
            declaration(),
            (0_u8..3, any::<bool>(), 0_u8..3),
        ),
        0..12,
    )
}

/// The chart text, each declaration laid out by its `(filler lines,
/// lowercase, padding)`, or plainly when `plain`.
fn chart_text(decls: &[Decl], plain: bool) -> String {
    let mut text = String::new();
    for ((_, name), (value, _), (filler, lower, pad)) in decls {
        let (filler, lower, pad) = if plain {
            (0, false, 0)
        } else {
            (*filler, *lower, *pad)
        };
        for i in 0..filler {
            text.push_str(if i % 2 == 0 { "\n" } else { "; filler\n" });
        }
        let name = if lower {
            name.to_ascii_lowercase()
        } else {
            (*name).to_owned()
        };
        let pad = " ".repeat(usize::from(pad));
        text.push_str(&format!("{pad}#{name} {pad}{value}{pad}\n"));
    }
    text.push_str(NOTE);
    text
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every declaration is kept in order with its class and exact value;
    /// only the not-a-number ones warn; and the hash ignores the layout.
    #[test]
    fn judgment_headers_keep_order_and_class(decls in declarations()) {
        let loaded = load(&chart_text(&decls, false));
        let expected: Vec<_> = decls
            .iter()
            .map(|((name, _), (_, value), _)| h(*name, *value))
            .collect();
        prop_assert_eq!(&loaded.chart.meta.judgment_headers, &expected);
        let warned = loaded
            .warnings
            .iter()
            .filter(|w| matches!(w.kind, WarningKind::NotANumber(_)))
            .count();
        prop_assert_eq!(warned, expected.iter().filter(|d| d.value == NotANumber).count());
        prop_assert_eq!(
            loaded.chart.identity.resolved_hash,
            hash(&chart_text(&decls, true))
        );
    }
}
