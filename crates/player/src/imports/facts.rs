//! An attempt's import facts (design.md "beatoraja Lua Skins", import
//! facts): the snapshot a pending play skin's specialization runs against,
//! derived from the loaded chart, its play preset and the session's
//! autoplay. The skin importer takes the snapshot as data; deriving it,
//! the play level's parse included, is the player's.

use game::Loaded;
use rules::JudgeRank;
use skin_beatoraja::{ChartRank, Fact, ImportFacts, JudgeClass};
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
    ImportFacts {
        scene: ScreenKind::Play,
        autoplay: Fact::Known(autoplay),
        chart_rank: Fact::Known(read.rank.map(chart_rank)),
        play_level: Fact::Known(level_of(&loaded.labels().playlevel)),
    }
}

/// A rank the play preset's header policy reads, as the importer takes it.
const fn chart_rank(rank: JudgeRank) -> ChartRank {
    match rank {
        JudgeRank::Rank(r) => ChartRank::Rank(r),
        JudgeRank::DefExRankMilli(m) => ChartRank::DefExRankMilli(m),
    }
}

/// The judgment class of `rank`, the play preset's reading of the chart,
/// as `chart.judge_class` reads it: 0 the hardest to 4 the easiest, by the
/// bands the import facts use ([`JudgeClass::of`]), so options 180 to 184
/// read at a frame what they read when the skin was specialized; `None`
/// for a rank in no band.
pub fn class_of(rank: Option<JudgeRank>) -> Option<u8> {
    Some(match JudgeClass::of(rank.map(chart_rank))? {
        JudgeClass::VeryHard => 0,
        JudgeClass::Hard => 1,
        JudgeClass::Normal => 2,
        JudgeClass::Easy => 3,
        JudgeClass::VeryEasy => 4,
    })
}

/// The difficulty `text` gives as `chart.difficulty` reads it: the
/// source's integer, `#DIFFICULTY` read by `Integer.parseInt` as the play
/// level is ([`level_of`]; jbms-parser `CommandWord`, where an unreadable
/// line keeps the value before it, as the loader keeps the last readable
/// text, none leaving 0), from 1 to 5, and 0 for any other
/// (`BooleanPropertyFactory.java:442-447`, option 150 holding for
/// `d <= 0 || d > 5`).
pub fn difficulty_of(text: &str) -> u8 {
    u8::try_from(level_of(text))
        .ok()
        .filter(|d| (1..=5).contains(d))
        .unwrap_or(0)
}

/// The play level `text` gives, as the source's `Integer.parseInt` reads
/// `#PLAYLEVEL` (design.md "beatoraja Lua Skins", import facts;
/// [`bms_parser::java_int`]); anything it does not read, the empty text
/// and surrounding whitespace included, is 0.
pub fn level_of(text: &str) -> i32 {
    bms_parser::java_int(text).unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests;
