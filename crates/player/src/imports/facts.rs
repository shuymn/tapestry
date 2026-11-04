//! An attempt's import facts (design.md "beatoraja Lua Skins", import
//! facts): the snapshot a pending play skin's specialization runs against,
//! derived from the loaded chart, its play preset and the session's
//! autoplay. The skin importer takes the snapshot as data; deriving it,
//! the play level's parse included, is the player's.

use game::Loaded;
use rules::JudgeRank;
use skin_beatoraja::{ChartRank, Fact, ImportFacts};
use skin_model::ScreenKind;

/// The facts of an attempt at `loaded`'s chart with autoplay as
/// `autoplay`: the play scene, autoplay known, the rank the play preset's
/// header policy reads from the chart (as the evaluator judges with it),
/// and the chart's play level parsed by [`level_of`].
pub fn attempt_facts(loaded: &Loaded, autoplay: bool) -> ImportFacts {
    let read = loaded
        .preset()
        .headers
        .read(&loaded.chart().meta.judgment_headers);
    let rank = read.rank.map(|r| match r {
        JudgeRank::Rank(r) => ChartRank::Rank(r),
        JudgeRank::DefExRankMilli(m) => ChartRank::DefExRankMilli(m),
    });
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay: Fact::Known(autoplay),
        chart_rank: Fact::Known(rank),
        play_level: Fact::Known(level_of(&loaded.labels().playlevel)),
    }
}

/// The first code point of each run of ten decimal digits, zero to nine,
/// in the Basic Multilingual Plane: the characters of general category Nd
/// there (Unicode 15.0), which `Character.digit(c, 10)` accepts.
const DIGIT_ZEROS: [u32; 37] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10,
];

/// `c`'s value as a decimal digit, as `Character.digit(c, 10)` gives it
/// for a UTF-16 code unit: a character outside the Basic Multilingual
/// Plane is two surrogates, neither a digit.
fn digit(c: char) -> Option<i64> {
    let c = u32::from(c);
    DIGIT_ZEROS
        .iter()
        .find(|&&zero| (zero..zero + 10).contains(&c))
        .map(|&zero| i64::from(c - zero))
}

/// The play level `text` gives, as the source's `Integer.parseInt` reads
/// `#PLAYLEVEL` (design.md "beatoraja Lua Skins", import facts): one
/// optional `+` or `-`, then one or more decimal digits ([`digit`]), the
/// value within `i32`; anything else, the empty text and surrounding
/// whitespace included, is 0.
pub fn level_of(text: &str) -> i32 {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.is_empty() {
        return 0;
    }
    let mut value: i64 = 0;
    for c in digits.chars() {
        let Some(d) = digit(c) else {
            return 0;
        };
        // Past `i32`'s magnitude the value is out of range either way.
        value = value * 10 + d;
        if value > i64::from(i32::MAX) + 1 {
            return 0;
        }
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).unwrap_or(0)
}

#[cfg(test)]
pub(super) mod tests;
