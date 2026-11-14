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
//! - 180 to 184, the chart's judgment class (`:448-452`): the snapshot's
//!   [`ImportFacts::judge_class`], the class of the chart's own rank
//!   ([`JudgeClass::of_headers`]), whatever the play preset judges with,
//!   as `chart.judge_class` gives it at frame time;
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
//!   on [`ScreenKind::Result`] the result's `chart.level` under its
//!   validity instead, through the result profile as a frame reads it;
//! - texts 1001 to 1003, the difficulty table's name, level and both
//!   (`StringPropertyFactory.java:259-261`): the constant `""`, unrecorded,
//!   as there are no difficulty tables and outside a table the source's
//!   strings are empty (`PlayerResource.java:128-129`, `:531-535`).
//!
//! An import records each fact it read once, with the derived value it
//! read, in first-read order ([`FactDeps`]);
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

use bms_model::JudgmentHeader;
use rules::{HeaderPolicy, JudgeRank};
use skin_model::{ResultFields, ScreenKind};

use crate::import::result::profile::{self, Read, ResOption};
use crate::import::selection::profile::Class;

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

/// A chart's judgment class: options 180 to 184, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JudgeClass {
    VeryHard,
    Hard,
    Normal,
    Easy,
    VeryEasy,
}

/// The rank the reference's 7-key rule gives `#RANK 0` to `#RANK 4`
/// (`play/JudgeProperty.java:210`, `JudgeWindowRule.NORMAL`'s
/// `judgerank[r][1]`); `#RANK 2`'s is also the `#DEFEXRANK` multiplier
/// and the rank of a chart with neither header.
const RANK_RATES: [i64; 5] = [25, 50, 75, 100, 125];
const RANK_2_RATE: i64 = 75;

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

    /// The class a skin shows for a chart whose judgment headers are
    /// `headers` (in file order), from the chart alone, whatever the play
    /// preset: [`of_rank`](Self::of_rank) of the rank the beatoraja-style
    /// header policy ([`HeaderPolicy::DefExRankFirst`]) reads, the policy
    /// the library stores each chart's rank with, so a chart's play and
    /// result show the class its library entry shows.
    #[must_use]
    pub fn of_headers(headers: &[JudgmentHeader]) -> Option<Self> {
        Self::of_rank(HeaderPolicy::DefExRankFirst.read(headers).rank)
    }

    /// The class of a chart of rank `rank` (the beatoraja-style header
    /// policy's reading, `None` for no rank header), on every screen: the
    /// song data's judge rank (`BooleanPropertyFactory.java:448-452`)
    /// after one 7-key validation.
    ///
    /// - The validation (`play/BMSPlayerRule.java:59-66`) makes the rank a
    ///   percentage `j`: `#RANK r` is 25, 50, 75, 100 or 125, none is
    ///   `#RANK 2`, and `#DEFEXRANK n` (its whole part) is `n * 75 / 100`
    ///   in plain integer arithmetic, truncated toward 0.
    /// - The bands: `j == 0 || 10 <= j < 35` very hard, `j == 1 || 35 <= j
    ///   < 60` hard, `j == 2 || 60 <= j < 85` normal, `j == 3 || 85 <= j <
    ///   110` easy, `j == 4 || j >= 110` very easy; `None` for a `j` in no
    ///   band (5 to 9, from `#DEFEXRANK 7` to `13`).
    ///
    /// So `#DEFEXRANK 120` is easy (90), `#DEFEXRANK 2` hard (1) and
    /// `#DEFEXRANK 1` very hard (0). The source validates its chart twice on
    /// its play and result screens, and multiplies in `i32`; neither is
    /// followed (design.md "Rendering and Skins", Differences from the
    /// source, readouts).
    #[must_use]
    pub fn of_rank(rank: Option<JudgeRank>) -> Option<Self> {
        Self::band(Self::percent(rank))
    }

    /// The validation of `rank` into a percentage `j`
    /// (`play/BMSPlayerRule.java:59-66`): `#RANK r` is 25, 50, 75, 100 or
    /// 125, none is `#RANK 2`, and `#DEFEXRANK n` (its whole part) is
    /// `n * 75 / 100`, exact: the policy's thousandths fit `i64`, so the
    /// whole part times 75 does too.
    fn percent(rank: Option<JudgeRank>) -> i64 {
        match rank {
            None => RANK_2_RATE,
            Some(JudgeRank::Rank(r)) => usize::try_from(r)
                .ok()
                .and_then(|i| RANK_RATES.get(i))
                .map_or(RANK_2_RATE, |&rate| rate),
            Some(JudgeRank::DefExRankMilli(m)) => {
                m.div_euclid(1_000).saturating_mul(RANK_2_RATE) / 100
            }
        }
    }

    /// The class of percentage `j` (`BooleanPropertyFactory.java:448-452`).
    const fn band(j: i64) -> Option<Self> {
        Some(match j {
            0 | 10..35 => Self::VeryHard,
            1 | 35..60 => Self::Hard,
            2 | 60..85 => Self::Normal,
            3 | 85..110 => Self::Easy,
            4 | 110.. => Self::VeryEasy,
            _ => return None,
        })
    }

    /// Its index, 0 the hardest to 4 the easiest, as `chart.judge_class`
    /// holds it.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Self::VeryHard => 0,
            Self::Hard => 1,
            Self::Normal => 2,
            Self::Easy => 3,
            Self::VeryEasy => 4,
        }
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
    /// The drawn chart's judgment class ([`JudgeClass::of_headers`]),
    /// `None` when it is in no class; [`Fact::Unavailable`] when there is
    /// no chart.
    pub judge_class: Fact<Option<JudgeClass>>,
    /// The chart's play level, `#PLAYLEVEL` read as an integer token (an
    /// optional `+` or `-` and ASCII digits, within `i32`; anything else
    /// 0); [`Fact::Unavailable`] when there is no chart.
    pub play_level: Fact<i32>,
    /// The viewed result's fields, as its frames read them
    /// ([`ResultFields::of`] the view); read on [`ScreenKind::Result`]
    /// only, through the result profile.
    pub result: Fact<ResultFields>,
}

impl ImportFacts {
    /// Every fact unavailable.
    #[must_use]
    pub fn unavailable(scene: ScreenKind) -> Self {
        Self {
            scene,
            autoplay: Fact::Unavailable,
            judge_class: Fact::Unavailable,
            play_level: Fact::Unavailable,
            result: Fact::Unavailable,
        }
    }

    /// The base snapshot: the session's facts known, the chart's pending
    /// (unavailable on [`ScreenKind::Selection`], which has no chart), and
    /// the result's pending on [`ScreenKind::Result`].
    #[must_use]
    pub fn base(scene: ScreenKind, autoplay: Fact<bool>) -> Self {
        let (judge_class, play_level) = if scene == ScreenKind::Selection {
            (Fact::Unavailable, Fact::Unavailable)
        } else {
            (Fact::Pending, Fact::Pending)
        };
        let result = if scene == ScreenKind::Result {
            Fact::Pending
        } else {
            Fact::Unavailable
        };
        Self {
            scene,
            autoplay,
            judge_class,
            play_level,
            result,
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
            FactId::JudgeClass => match self.judge_class {
                Fact::Known(c) => Fact::Known(FactValue::Class(c)),
                Fact::Pending => Fact::Pending,
                Fact::Unavailable => Fact::Unavailable,
            },
            FactId::PlayLevel => match self.play_level {
                Fact::Known(l) => Fact::Known(FactValue::Level(l)),
                Fact::Pending => Fact::Pending,
                Fact::Unavailable => Fact::Unavailable,
            },
            FactId::ResultNumber(n) => self.read_result(|fields| {
                let Some(Class::Bridge(v)) = profile::number(n) else {
                    return Read::Unavailable;
                };
                profile::read_number(&v, fields)
            }),
            FactId::ResultOption(n) => self.read_result(|fields| profile::read_option(n, fields)),
        }
    }

    /// A result fact read by `read` from the snapshot's fields: an int
    /// for a number, 0 or 1 for an option.
    fn read_result(&self, read: impl FnOnce(&ResultFields) -> Read) -> Fact<FactValue> {
        match self.result {
            Fact::Known(fields) => match read(&fields) {
                Read::Value(v) => Fact::Known(FactValue::Int(v)),
                Read::Unavailable => Fact::Unavailable,
            },
            Fact::Pending => Fact::Pending,
            Fact::Unavailable => Fact::Unavailable,
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
    /// A result number by its id, read through the result profile on
    /// [`ScreenKind::Result`].
    ResultNumber(i32),
    /// A result option by its id (positive), read likewise.
    ResultOption(i32),
}

impl FactId {
    /// Whether the fact depends on the viewed result beyond the chart's
    /// own facts: a result number or option whose expression or validity
    /// reads anything but the chart's fields (its judged notes, which a
    /// view replaces with its own, count as the view's), or one the
    /// profile does not bridge. The session's and the chart's facts, the
    /// chart's fields and a constant option do not.
    #[must_use]
    pub fn reads_view(self) -> bool {
        match self {
            Self::Autoplay | Self::JudgeClass | Self::PlayLevel => false,
            Self::ResultNumber(n) => match profile::number(n) {
                Some(Class::Bridge(v)) => !(chart_only(v.expr) && chart_only(v.guard)),
                _ => true,
            },
            Self::ResultOption(n) => match profile::option(n) {
                Some(Class::Bridge(ResOption::Expr(src))) => {
                    !(chart_only(src) && chart_only(profile::option_guard(n)))
                }
                Some(Class::Bridge(ResOption::Constant(_))) => false,
                _ => true,
            },
        }
    }
}

/// Whether the result schema expression `src` reads only the chart's
/// fields other than its judged notes.
fn chart_only(src: &str) -> bool {
    src.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .filter(|t| !t.is_empty() && !t.starts_with(|c: char| c.is_ascii_digit()))
        .all(|t| {
            matches!(
                t,
                "if" | "then" | "else" | "true" | "false" | "abs" | "clamp"
            ) || (t.starts_with("chart.") && !t.starts_with("chart.notes"))
        })
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
    /// [`FactId::ResultNumber`]: the value, `i32::MIN` when hidden;
    /// [`FactId::ResultOption`]: 1 when it holds, else 0.
    Int(i32),
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
        // Play's other constants: no replay, course stage or constant mode.
        _ if facts.scene == ScreenKind::Play && crate::import::ops::play_constant_false(id) => {
            return constant(false);
        }
        AUTOPLAY_OFF | AUTOPLAY_ON => FactId::Autoplay,
        _ if JudgeClass::of_option(id).is_some() => FactId::JudgeClass,
        // The result profile's: a constant, or a result fact.
        _ if facts.scene == ScreenKind::Result => match profile::option(id) {
            Some(Class::Bridge(ResOption::Constant(b))) => return constant(b),
            Some(Class::Bridge(ResOption::Expr(_))) => FactId::ResultOption(id),
            _ => return OptionRead::LiveState,
        },
        // The decision profile's constants.
        _ if facts.scene == ScreenKind::Decision => {
            match crate::import::decision::profile::option(id) {
                Some(Class::Bridge(ResOption::Constant(b))) => return constant(b),
                _ => return OptionRead::LiveState,
            }
        }
        _ => return OptionRead::LiveState,
    };
    match facts.value(fact) {
        Fact::Known(value) => {
            let on = match value {
                FactValue::Bool(on) => on == (id == AUTOPLAY_ON),
                FactValue::Class(c) => c.is_some() && c == JudgeClass::of_option(id),
                FactValue::Level(_) => false,
                FactValue::Int(v) => v != 0,
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
    // A result scene reads every profile number, the play level among
    // them, from the result's fields, as its frames do (D12); the
    // application's readouts, which the frame fills and no view holds, are
    // live state, as on selection.
    let fact = if facts.scene == ScreenKind::Result
        && !profile::APP.contains(&n)
        && matches!(profile::number(n), Some(Class::Bridge(_)))
    {
        FactId::ResultNumber(n)
    } else if n == PLAY_LEVEL {
        FactId::PlayLevel
    } else {
        return NumberRead::LiveState;
    };
    match facts.value(fact) {
        Fact::Known(value @ (FactValue::Level(v) | FactValue::Int(v))) => NumberRead::Fact {
            value: v,
            read: FactRead { fact, value },
        },
        Fact::Known(_) | Fact::Unavailable => NumberRead::Unavailable,
        Fact::Pending => NumberRead::Pending(fact),
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

#[cfg(test)]
mod rank_tests {
    use bms_model::{HeaderValue, JudgmentHeader, JudgmentHeaderName};
    use rules::JudgeRank::{self, DefExRankMilli, Rank};

    use super::JudgeClass::{self, Easy, Hard, Normal, VeryEasy, VeryHard};

    /// A chart's class takes one validation in plain arithmetic, the same
    /// from a stored rank (a song list's) and from the chart's headers (an
    /// attempt's): `#DEFEXRANK 1` is very hard (0), and a `#DEFEXRANK` whose
    /// product passes `i32` keeps its value (30 000 000 is 22 500 000,
    /// 57 266 231 is 42 949 673, 57 266 232 is 42 949 674: all very easy).
    #[test]
    fn a_class_takes_one_validation_in_plain_arithmetic() {
        let header = |rank: Option<JudgeRank>| {
            rank.map(|r| match r {
                Rank(r) => JudgmentHeader {
                    name: JudgmentHeaderName::Rank,
                    value: HeaderValue::integer(i64::from(r)),
                },
                DefExRankMilli(m) => JudgmentHeader {
                    name: JudgmentHeaderName::DefExRank,
                    value: HeaderValue::integer(m / 1_000),
                },
            })
        };
        for (rank, class) in [
            (None, Some(Normal)),
            (Some(Rank(0)), Some(VeryHard)),
            (Some(Rank(1)), Some(Hard)),
            (Some(Rank(3)), Some(Easy)),
            (Some(Rank(4)), Some(VeryEasy)),
            (Some(DefExRankMilli(1_000)), Some(VeryHard)),
            (Some(DefExRankMilli(2_000)), Some(Hard)),
            (Some(DefExRankMilli(10_000)), None),
            (Some(DefExRankMilli(120_000)), Some(Easy)),
            (Some(DefExRankMilli(30_000_000_000)), Some(VeryEasy)),
            (Some(DefExRankMilli(57_266_231_000)), Some(VeryEasy)),
            (Some(DefExRankMilli(57_266_232_000)), Some(VeryEasy)),
            (Some(DefExRankMilli(3_000_000_000_000)), Some(VeryEasy)),
        ] {
            assert_eq!(JudgeClass::of_rank(rank), class, "{rank:?}");
            let headers: Vec<JudgmentHeader> = header(rank).into_iter().collect();
            assert_eq!(JudgeClass::of_headers(&headers), class, "{rank:?}");
        }
    }
}
