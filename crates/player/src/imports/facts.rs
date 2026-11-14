//! An attempt's import facts (design.md "beatoraja Lua Skins", import
//! facts): the snapshot a pending play skin's specialization runs against,
//! derived from the loaded chart, its play preset and the session's
//! autoplay. The skin importer takes the snapshot as data; deriving it,
//! the play level's parse included, is the player's.

use game::Loaded;
use skin_beatoraja::{Fact, ImportFacts, JudgeClass};
use skin_model::ScreenKind;

/// The facts of an attempt at `loaded`'s chart with autoplay as
/// `autoplay`: the play scene, autoplay known, the chart's judgment class
/// from its own rank, by the rule every screen uses
/// ([`JudgeClass::of_headers`], whatever the play preset judges with), and
/// the chart's play level parsed by [`level_of`].
pub fn attempt_facts(loaded: &Loaded, autoplay: bool) -> ImportFacts {
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay: Fact::Known(autoplay),
        judge_class: Fact::Known(JudgeClass::of_headers(
            &loaded.chart().meta.judgment_headers,
        )),
        play_level: Fact::Known(level_of(&loaded.labels().playlevel)),
        result: Fact::Unavailable,
    }
}

/// The difficulty `text` gives as `chart.difficulty` reads it:
/// `#DIFFICULTY` read as an integer token as the play level is
/// ([`level_of`]), from 1 to 5, and 0 for any other or none
/// (`BooleanPropertyFactory.java:442-447`, option 150 holding for
/// `d <= 0 || d > 5`). The loader keeps the last declaration that
/// reads, so a later one that does not keeps the earlier value.
pub fn difficulty_of(text: &str) -> u8 {
    u8::try_from(level_of(text))
        .ok()
        .filter(|d| (1..=5).contains(d))
        .unwrap_or(0)
}

/// The play level `text` gives: `#PLAYLEVEL` read as an integer token
/// ([`bms_parser::header_integer`]: an optional `+` or `-` and ASCII
/// digits, within `i32`); anything else, the empty text and other
/// scripts' digits included, is 0.
pub fn level_of(text: &str) -> i32 {
    bms_parser::header_integer(text).unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests;
