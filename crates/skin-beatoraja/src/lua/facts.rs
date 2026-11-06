//! Import facts (design.md "beatoraja Lua Skins", import facts;
//! docs/stage3.md §3, Lua): the owned snapshot of the scene a skin is
//! imported for, the skin's own kind ([`ScreenKind`]), which the
//! configured pass answers `main_state.option` from, and the dependencies
//! an import records on it.
//!
//! The snapshot is plain data the caller fills, immutable for the pass,
//! so every read of one pass sees one scene. The facts are published by
//! id, as the source's boolean properties at beatoraja's `0.8.9` tag
//! answer them (`skin/property/BooleanPropertyFactory.java`):
//!
//! - 33 and 32, autoplay on and off (`:413-416`): the snapshot's
//!   [`ImportFacts::autoplay`] on [`ScreenKind::Play`], a constant `false` on
//!   every other scene, as the source's are off the play state;
//! - 180 to 184, the chart's judgment class (`:448-452`): the source's
//!   bands over [`ImportFacts::chart_rank`], the rank the play preset's
//!   header policy reads (which the evaluator judges with), where the
//!   source bands the chart's raw rank. A chart the policy reads no rank
//!   from is judged with the window policy's absent window, which is the
//!   `#RANK 2` window in every shipped preset, so its class is
//!   [`JudgeClass::Normal`], the class of `#RANK 2`;
//! - 290, course mode (`:523-524`): a constant `false`, as there are no
//!   courses;
//! - 1008, `table_song` (`:351-352`): a constant `false`, as there are no
//!   difficulty tables.
//!
//! `main_state.number` and `main_state.text` answer these ids, the rest
//! being play state:
//!
//! - number 96, the play level (`IntegerPropertyFactory.java:154`,
//!   `:580-584`): the snapshot's [`ImportFacts::play_level`], bound to the
//!   chart, as `SongData.getLevel()` gives it (`song/SongData.java:167`);
//! - texts 1001 to 1003, the difficulty table's name, level and both
//!   (`StringPropertyFactory.java:259-261`): the constant `""`, unrecorded,
//!   as there are no difficulty tables and outside a table the source's
//!   strings are empty (`PlayerResource.java:128-129`, `:531-535`).
//!
//! An import records each fact it read once, with the derived value it
//! read (the class, never the rank), in first-read order ([`FactDeps`]);
//! [`FactDeps::holds`] says whether a result still fits a snapshot.
//!
//! The boundary with the caller: this module gives the pure pieces. The
//! import is a function of the sources, the configuration and the
//! snapshot; its result names its scene and the facts it read; a base
//! import that reaches a [`Fact::Pending`] fact ends with a pending
//! result naming it ([`super::LuaPending`]); a [`Fact::Unavailable`] one
//! refuses. Building the snapshots from a session, the base import and
//! the attempt's specialization, the pending installation, generations,
//! invalidation and refusing a stale completion belong to the caller
//! (docs/stage3.md S10-07).

use skin_model::ScreenKind;

/// A fact as the snapshot has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact<T> {
    Known(T),
    /// Bound to the chart or the attempt, supplied by the attempt's
    /// specialization: an import that reads it ends pending.
    Pending,
    /// The scene cannot give it: an import that reads it refuses.
    Unavailable,
}

/// A rank the play preset's header policy reads, as `rules::JudgeRank`
/// has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartRank {
    /// `#RANK`, as written.
    Rank(i32),
    /// `#DEFEXRANK`, in thousandths of a percent.
    DefExRankMilli(i64),
}

/// A chart's judgment class: options 180 to 184, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeClass {
    VeryHard,
    Hard,
    Normal,
    Easy,
    VeryEasy,
}

/// The rank whose window the play uses when the header policy reads none:
/// every shipped preset's absent window is its `#RANK 2` window.
const ABSENT_RANK: ChartRank = ChartRank::Rank(2);

/// The option id of [`JudgeClass::VeryHard`]; the others follow it.
const JUDGE_CLASS_FIRST: i32 = 180;
const AUTOPLAY_OFF: i32 = 32;
const AUTOPLAY_ON: i32 = 33;
const COURSE_MODE: i32 = 290;

impl JudgeClass {
    const ALL: [Self; 5] = [
        Self::VeryHard,
        Self::Hard,
        Self::Normal,
        Self::Easy,
        Self::VeryEasy,
    ];

    /// The class of `rank`, the header policy's reading, by the source's
    /// bands: `j == 0 || 10 <= j < 35` very hard, `j == 1 || 35 <= j < 60`
    /// hard, `j == 2 || 60 <= j < 85` normal, `j == 3 || 85 <= j < 110`
    /// easy, `j == 4 || j >= 110` very easy. A `#RANK` is `j` itself; a
    /// `#DEFEXRANK` is its whole percent from 10 up, since the source's
    /// integer below 10 is a `#RANK` code. No rank read is the class of
    /// `#RANK 2`, whose window the play then uses. `None` for a `j` in no
    /// band.
    #[must_use]
    pub fn of(rank: Option<ChartRank>) -> Option<Self> {
        let j = match rank.unwrap_or(ABSENT_RANK) {
            ChartRank::Rank(r) => i64::from(r),
            ChartRank::DefExRankMilli(m) => Some(m.div_euclid(1_000)).filter(|&j| j >= 10)?,
        };
        Some(match j {
            0 | 10..35 => Self::VeryHard,
            1 | 35..60 => Self::Hard,
            2 | 60..85 => Self::Normal,
            3 | 85..110 => Self::Easy,
            4 | 110.. => Self::VeryEasy,
            _ => return None,
        })
    }

    /// The class option `id` (180 to 184) tests.
    fn of_option(id: i32) -> Option<Self> {
        let i = usize::try_from(id.checked_sub(JUDGE_CLASS_FIRST)?).ok()?;
        Self::ALL.get(i).copied()
    }
}

/// The facts of one scene, for one configured pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportFacts {
    pub scene: ScreenKind,
    /// Whether the session plays automatically; read on
    /// [`ScreenKind::Play`] only.
    pub autoplay: Fact<bool>,
    /// The rank the play preset's header policy reads from the drawn
    /// chart, `None` when it reads none (`rules::ReadHeaders::rank`);
    /// [`Fact::Unavailable`] when there is no chart.
    pub chart_rank: Fact<Option<ChartRank>>,
    /// The chart's play level as the source parses `#PLAYLEVEL` (Java's
    /// `Integer.parseInt`: an optional `+` or `-`, then one or more
    /// characters `Character.digit(c, 10)` accepts, over UTF-16 code
    /// units, within `i32`; anything else 0); [`Fact::Unavailable`] when
    /// there is no chart.
    pub play_level: Fact<i32>,
}

impl ImportFacts {
    /// Every fact unavailable.
    #[must_use]
    pub fn unavailable(scene: ScreenKind) -> Self {
        Self {
            scene,
            autoplay: Fact::Unavailable,
            chart_rank: Fact::Unavailable,
            play_level: Fact::Unavailable,
        }
    }

    /// The base snapshot: the session's facts known, the chart's pending
    /// (unavailable on [`ScreenKind::Selection`], which has no chart).
    #[must_use]
    pub fn base(scene: ScreenKind, autoplay: Fact<bool>) -> Self {
        let (chart_rank, play_level) = if scene == ScreenKind::Selection {
            (Fact::Unavailable, Fact::Unavailable)
        } else {
            (Fact::Pending, Fact::Pending)
        };
        Self {
            scene,
            autoplay,
            chart_rank,
            play_level,
        }
    }

    /// The fact `fact` as this snapshot gives it, derived.
    fn value(&self, fact: FactId) -> Fact<FactValue> {
        match fact {
            FactId::Autoplay => match self.autoplay {
                Fact::Known(b) => Fact::Known(FactValue::Bool(b)),
                Fact::Pending => Fact::Pending,
                Fact::Unavailable => Fact::Unavailable,
            },
            FactId::JudgeClass => match self.chart_rank {
                Fact::Known(r) => Fact::Known(FactValue::Class(JudgeClass::of(r))),
                Fact::Pending => Fact::Pending,
                Fact::Unavailable => Fact::Unavailable,
            },
            FactId::PlayLevel => match self.play_level {
                Fact::Known(l) => Fact::Known(FactValue::Level(l)),
                Fact::Pending => Fact::Pending,
                Fact::Unavailable => Fact::Unavailable,
            },
        }
    }
}

/// A published fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactId {
    /// Options 32 and 33.
    Autoplay,
    /// Options 180 to 184.
    JudgeClass,
    /// Number 96.
    PlayLevel,
}

/// A fact's derived value, as an import read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactValue {
    /// [`FactId::Autoplay`]: whether autoplay is on.
    Bool(bool),
    /// [`FactId::JudgeClass`]: the class, or none.
    Class(Option<JudgeClass>),
    /// [`FactId::PlayLevel`]: the level.
    Level(i32),
}

/// One fact an import read, with the value it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactRead {
    pub fact: FactId,
    pub value: FactValue,
}

/// What an import depends on: its scene, and each fact it read once, in
/// first-read order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactDeps {
    pub scene: ScreenKind,
    pub reads: Vec<FactRead>,
}

impl FactDeps {
    /// Whether a result with these dependencies fits `now`: the same
    /// scene, and each recorded fact known there at its recorded value.
    #[must_use]
    pub fn holds(&self, now: &ImportFacts) -> bool {
        now.scene == self.scene
            && self
                .reads
                .iter()
                .all(|r| now.value(r.fact) == Fact::Known(r.value))
    }
}

/// What `option(n)` is during the configured pass, for an `n` within
/// `i32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OptionRead {
    /// An id the source's factory does not know, or the scene's
    /// constant: the value, nothing recorded.
    Constant { value: bool, known: bool },
    /// A published fact: the answer, and the read to record.
    Fact { value: bool, read: FactRead },
    /// A published fact the snapshot has pending.
    Pending(FactId),
    /// A published fact the snapshot has unavailable.
    Unavailable,
    /// A built-in that is not a published fact.
    LiveState,
}

/// Answers `option(n)` from `facts`.
pub(crate) fn option(facts: &ImportFacts, n: i32) -> OptionRead {
    // `i32::MIN`'s magnitude is not an `i32`: not built in.
    let Some(id) = n
        .checked_abs()
        .filter(|&id| crate::import::ops::is_builtin(id))
    else {
        return OptionRead::Constant {
            value: false,
            known: false,
        };
    };
    let negate = |b: bool| b != (n < 0);
    let constant = |b: bool| OptionRead::Constant {
        value: negate(b),
        known: true,
    };
    let fact = match id {
        AUTOPLAY_OFF | AUTOPLAY_ON if facts.scene != ScreenKind::Play => return constant(false),
        COURSE_MODE | crate::import::ops::TABLE_SONG => return constant(false),
        AUTOPLAY_OFF | AUTOPLAY_ON => FactId::Autoplay,
        _ if JudgeClass::of_option(id).is_some() => FactId::JudgeClass,
        _ => return OptionRead::LiveState,
    };
    match facts.value(fact) {
        Fact::Known(value) => {
            let on = match value {
                FactValue::Bool(on) => on == (id == AUTOPLAY_ON),
                FactValue::Class(c) => c.is_some() && c == JudgeClass::of_option(id),
                FactValue::Level(_) => false,
            };
            OptionRead::Fact {
                value: negate(on),
                read: FactRead { fact, value },
            }
        }
        Fact::Pending => OptionRead::Pending(fact),
        Fact::Unavailable => OptionRead::Unavailable,
    }
}

/// The number id of the play level.
const PLAY_LEVEL: i32 = 96;

/// What `number(n)` is during the configured pass, for an `n` within
/// `i32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumberRead {
    /// A published fact: the answer, and the read to record.
    Fact { value: i32, read: FactRead },
    /// A published fact the snapshot has pending.
    Pending(FactId),
    /// A published fact the snapshot has unavailable.
    Unavailable,
    /// Not a published fact.
    LiveState,
}

/// Answers `number(n)` from `facts`.
pub(crate) fn number(facts: &ImportFacts, n: i32) -> NumberRead {
    if n != PLAY_LEVEL {
        return NumberRead::LiveState;
    }
    match facts.value(FactId::PlayLevel) {
        Fact::Known(value @ FactValue::Level(level)) => NumberRead::Fact {
            value: level,
            read: FactRead {
                fact: FactId::PlayLevel,
                value,
            },
        },
        Fact::Known(_) | Fact::Unavailable => NumberRead::Unavailable,
        Fact::Pending => NumberRead::Pending(FactId::PlayLevel),
    }
}

/// The text ids of the difficulty table's name, level and both.
const TABLE_TEXTS: std::ops::RangeInclusive<i32> = 1001..=1003;

/// What `text(n)` is during the configured pass, for an `n` within `i32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextRead {
    /// The scene's constant: the value, nothing recorded.
    Constant(&'static str),
    /// Not a published fact.
    LiveState,
}

/// What option `n` answers on every scene, when it is a scene constant:
/// course mode and `table_song`, false, a negative id negated.
pub(crate) fn scene_option(n: i32) -> Option<bool> {
    matches!(
        n.checked_abs()?,
        COURSE_MODE | crate::import::ops::TABLE_SONG
    )
    .then_some(n < 0)
}

/// Whether text `n` is the empty string on every scene: the table texts.
pub(crate) fn scene_text(n: i32) -> bool {
    TABLE_TEXTS.contains(&n)
}

/// Answers `text(n)` on `facts`' scene: the table texts are empty, as
/// there are no difficulty tables.
pub(crate) fn text(_facts: &ImportFacts, n: i32) -> TextRead {
    if scene_text(n) {
        TextRead::Constant("")
    } else {
        TextRead::LiveState
    }
}

#[cfg(test)]
mod tests {
    use skin_model::ScreenKind;

    use super::{Fact, ImportFacts, OptionRead, option};

    #[test]
    fn the_scene_s_constants_hold_off_play() {
        // Autoplay (32, 33) is false off the play scene whatever the
        // snapshot, and course mode (290) and `table_song` (1008) are false
        // on every scene.
        for scene in [
            ScreenKind::Selection,
            ScreenKind::Decision,
            ScreenKind::Result,
        ] {
            let mut f = ImportFacts::unavailable(scene);
            f.autoplay = Fact::Known(true);
            for (n, value) in [
                (32, false),
                (-32, true),
                (33, false),
                (-33, true),
                (290, false),
                (-290, true),
                (1_008, false),
                (-1_008, true),
            ] {
                assert_eq!(
                    option(&f, n),
                    OptionRead::Constant { value, known: true },
                    "{scene:?} {n}"
                );
            }
        }
    }
}
