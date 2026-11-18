//! A chart's difficulty as the library stores it (design.md "Loading and
//! Song Selection", Difficulty): its declared `#DIFFICULTY`, else one the
//! scan infers from its subtitle, its title and its note count.

use bms_model::ChartSummary;
use bms_parser::Header;

/// The keyword steps, in order: a text gives the difficulty of the first
/// keyword it contains, lowercased by Unicode's rules. The subtitle is
/// looked at first, then the title.
// Source: SF-021
const KEYWORDS: [(&str, u8); 6] = [
    ("beginner", 1),
    ("normal", 2),
    ("hyper", 3),
    ("another", 4),
    ("insane", 5),
    ("leggendaria", 5),
];

/// The note-count steps, in order: a chart of fewer notes than a count has
/// its difficulty, and one of at least the last count has 5.
// Source: SF-021
const NOTE_COUNTS: [(u64, u8); 4] = [(250, 1), (600, 2), (1_000, 3), (2_000, 4)];

/// The difficulty the library stores for a chart of `header` and seed-0
/// `summary`, 1 to 5, or 0 for none.
///
/// A `#DIFFICULTY` that reads as an integer other than 0
/// ([`bms_parser::header_integer`]) is kept: 1 to 5 as it is, any other
/// 0. Without one, or with 0, the difficulty is inferred: the first
/// keyword step that the subtitle, then the title, meets; else the
/// note-count step of the summary's taps and long notes, each long note
/// once; else, with no summary (a refused seed-0 branch), 0.
#[must_use]
pub fn of(header: &Header, summary: Option<&ChartSummary>) -> u8 {
    let declared = header
        .difficulty
        .as_deref()
        .and_then(bms_parser::header_integer)
        .unwrap_or(0);
    if declared != 0 {
        return u8::try_from(declared)
            .ok()
            .filter(|d| (1..=5).contains(d))
            .unwrap_or(0);
    }
    keyword(header.subtitle.as_deref())
        .or_else(|| keyword(header.title.as_deref()))
        .or_else(|| summary.map(by_notes))
        .unwrap_or(0)
}

/// The difficulty of the first keyword `text` contains.
// Source: SF-021
fn keyword(text: Option<&str>) -> Option<u8> {
    let text = text?.to_lowercase();
    KEYWORDS
        .iter()
        .find(|(word, _)| text.contains(word))
        .map(|&(_, difficulty)| difficulty)
}

/// The difficulty of `summary`'s note count.
// Source: SF-021
fn by_notes(summary: &ChartSummary) -> u8 {
    let notes = u64::from(summary.taps) + u64::from(summary.longs);
    NOTE_COUNTS
        .iter()
        .find(|&&(below, _)| notes < below)
        .map_or(5, |&(_, difficulty)| difficulty)
}
