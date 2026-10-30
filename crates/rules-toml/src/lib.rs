//! Rule presets as TOML files (design.md "Language and Crate Structure",
//! "Settings Files"). `rules` stays free of dependencies, so the file's shape
//! lives here as a separate set of types that convert to and from
//! [`RulePreset`] field by field.
//!
//! Times are integer µs and gauge values raw units of 1/100000 %, so a file
//! round-trips to the same canonical bytes and the same hash.
//!
//! Before the first release the file's shape may change incompatibly. A file
//! written by an older version is refused as unreadable, never converted.

use bms_model::TimeUs;
use rules::{
    Assignment, Denominator, EmptyPoorRule, GaugeDelta, GaugeKind, GaugeRule, GaugeValue,
    Implementation, JudgeWindows, LateHeadRule, LnEndWindows, LnMode, LnModeRule, LowDamage,
    MineRule, MissRule, PerJudgment, RankScale, RuleError, RulePreset, SameTimeOrder, ScoreWeights,
    TotalMode, Window,
};
use serde::{Deserialize, Serialize};

/// Why a preset file could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    /// The text is not TOML of the preset's shape.
    #[error(
        "the preset file cannot be read (a file from an older version is refused, not \
         converted; deleting it writes the current custom preset again)"
    )]
    Parse(#[source] toml::de::Error),
    /// The preset failed its validation.
    #[error("the preset is invalid")]
    Invalid(#[source] RuleError),
}

/// The largest preset file a reader accepts, in bytes. A file is untrusted,
/// so a reader reads one byte past this at most and refuses a longer file.
/// The custom preset takes about 2 KiB, and the largest presets the
/// canonical encoding's property tests generate (131 gauges, ids of 136
/// characters TOML escapes) about 160 KiB; a test keeps them below half
/// the limit.
pub const MAX_PRESET_FILE_BYTES: usize = 1 << 20;

/// Reads a preset from TOML and validates it.
///
/// # Errors
///
/// [`PresetError::Parse`] for text that is not a preset file, including an
/// unknown key; [`PresetError::Invalid`] for a preset that fails
/// [`RulePreset::validate`].
pub fn load_preset_toml(text: &str) -> Result<RulePreset, PresetError> {
    let file: PresetFile = toml::from_str(text).map_err(PresetError::Parse)?;
    let preset = RulePreset::from(file);
    preset.validate().map_err(PresetError::Invalid)?;
    Ok(preset)
}

/// Writes a preset as TOML that [`load_preset_toml`] reads back to an equal
/// preset.
///
/// # Panics
///
/// Never for a preset: every field has a TOML form.
#[must_use]
pub fn preset_to_toml(p: &RulePreset) -> String {
    toml::to_string(&PresetFile::from(p)).expect("every preset field has a TOML form")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetFile {
    id: String,
    version: u32,
    implementation: ImplementationFile,
    windows: WindowsFile,
    ln_end: LnEndFile,
    miss: MissFile,
    empty_poor: EmptyPoorFile,
    rank_scale: RankScaleFile,
    default_total_milli: i64,
    denominator: DenominatorFile,
    score: ScoreFile,
    same_time_order: SameTimeOrderFile,
    assignment: AssignmentFile,
    ln_mode: LnModeFile,
    total_mode: TotalModeFile,
    mine: MineFile,
    gauges: Vec<GaugeFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum ImplementationFile {
    Standard { version: u32 },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowFile {
    early_us: i64,
    late_us: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsFile {
    perfect: WindowFile,
    great: WindowFile,
    good: WindowFile,
    bad: WindowFile,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MissFile {
    PoorAfterLateBadOrOvertaken,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyPoorFile {
    ahead_us: i64,
    behind_us: i64,
    breaks_combo: bool,
    counts_bp: bool,
    counted: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LnEndFile {
    perfect_early_us: i64,
    great_early_us: i64,
    good_early_us: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RankScaleFile {
    rank_milli: [i64; 4],
    absent_milli: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DenominatorFile {
    TapsAndLnHeads,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerJudgmentFile<T> {
    perfect: T,
    great: T,
    good: T,
    bad: T,
    poor: T,
}

impl<T: Copy> PerJudgmentFile<T> {
    fn map<U>(self, f: impl Fn(T) -> U) -> PerJudgment<U> {
        PerJudgment {
            perfect: f(self.perfect),
            great: f(self.great),
            good: f(self.good),
            bad: f(self.bad),
            poor: f(self.poor),
        }
    }

    fn of<U: Copy>(p: &PerJudgment<U>, f: impl Fn(U) -> T) -> Self {
        Self {
            perfect: f(p.perfect),
            great: f(p.great),
            good: f(p.good),
            bad: f(p.bad),
            poor: f(p.poor),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoreFile {
    ex: PerJudgmentFile<u32>,
    breaks_combo: PerJudgmentFile<bool>,
    counts_bp: PerJudgmentFile<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SameTimeOrderFile {
    InputsFirst,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AssignmentFile {
    NearestTiesEarlier,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LnModeFile {
    default: LnKindFile,
    late_head: LateHeadFile,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LnKindFile {
    Ln,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LateHeadFile {
    CompletesAtPress,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TotalModeFile {
    ScalesIncrease,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum MineFile {
    WhileHeld { per_unit: i64 },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum DeltaFile {
    Fixed(i64),
    TotalShare { num: u32, den: u32 },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum GaugeKindFile {
    EndThreshold { clear: i64 },
    Survival,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LowDamageFile {
    below: i64,
    num: u32,
    den: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GaugeFile {
    id: String,
    kind: GaugeKindFile,
    start: i64,
    min: i64,
    max: i64,
    change: PerJudgmentFile<DeltaFile>,
    empty_poor: DeltaFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    low_damage: Option<LowDamageFile>,
}

const fn window(w: WindowFile) -> Window {
    Window {
        early: TimeUs(w.early_us),
        late: TimeUs(w.late_us),
    }
}

const fn window_file(w: Window) -> WindowFile {
    WindowFile {
        early_us: w.early.0,
        late_us: w.late.0,
    }
}

const fn delta(d: DeltaFile) -> GaugeDelta {
    match d {
        DeltaFile::Fixed(v) => GaugeDelta::Fixed(GaugeValue(v)),
        DeltaFile::TotalShare { num, den } => GaugeDelta::TotalShare { num, den },
    }
}

const fn delta_file(d: GaugeDelta) -> DeltaFile {
    match d {
        GaugeDelta::Fixed(v) => DeltaFile::Fixed(v.0),
        GaugeDelta::TotalShare { num, den } => DeltaFile::TotalShare { num, den },
    }
}

impl From<PresetFile> for RulePreset {
    fn from(f: PresetFile) -> Self {
        let PresetFile {
            id,
            version,
            implementation,
            windows,
            ln_end,
            miss,
            empty_poor,
            rank_scale,
            default_total_milli,
            denominator,
            score,
            same_time_order,
            assignment,
            ln_mode,
            total_mode,
            mine,
            gauges,
        } = f;
        Self {
            id,
            version,
            implementation: match implementation {
                ImplementationFile::Standard { version } => Implementation::Standard { version },
            },
            windows: JudgeWindows {
                perfect: window(windows.perfect),
                great: window(windows.great),
                good: window(windows.good),
                bad: window(windows.bad),
            },
            miss: match miss {
                MissFile::PoorAfterLateBadOrOvertaken => MissRule::PoorAfterLateBadOrOvertaken,
            },
            empty_poor: EmptyPoorRule {
                ahead: TimeUs(empty_poor.ahead_us),
                behind: TimeUs(empty_poor.behind_us),
                breaks_combo: empty_poor.breaks_combo,
                counts_bp: empty_poor.counts_bp,
                counted: empty_poor.counted,
            },
            ln_end: LnEndWindows {
                perfect: TimeUs(ln_end.perfect_early_us),
                great: TimeUs(ln_end.great_early_us),
                good: TimeUs(ln_end.good_early_us),
            },
            rank_scale: RankScale {
                rank_milli: rank_scale.rank_milli,
                absent_milli: rank_scale.absent_milli,
            },
            default_total_milli,
            denominator: match denominator {
                DenominatorFile::TapsAndLnHeads => Denominator::TapsAndLnHeads,
            },
            gauges: gauges
                .into_iter()
                .map(|g| GaugeRule {
                    id: g.id,
                    kind: match g.kind {
                        GaugeKindFile::EndThreshold { clear } => GaugeKind::EndThreshold {
                            clear: GaugeValue(clear),
                        },
                        GaugeKindFile::Survival => GaugeKind::Survival,
                    },
                    start: GaugeValue(g.start),
                    min: GaugeValue(g.min),
                    max: GaugeValue(g.max),
                    change: g.change.map(delta),
                    empty_poor: delta(g.empty_poor),
                    low_damage: g.low_damage.map(|l| LowDamage {
                        below: GaugeValue(l.below),
                        num: l.num,
                        den: l.den,
                    }),
                })
                .collect(),
            score: ScoreWeights {
                ex: score.ex.map(|v| v),
                breaks_combo: score.breaks_combo.map(|v| v),
                counts_bp: score.counts_bp.map(|v| v),
            },
            same_time_order: match same_time_order {
                SameTimeOrderFile::InputsFirst => SameTimeOrder::InputsFirst,
            },
            assignment: match assignment {
                AssignmentFile::NearestTiesEarlier => Assignment::NearestTiesEarlier,
            },
            ln_mode: LnModeRule {
                default: match ln_mode.default {
                    LnKindFile::Ln => LnMode::Ln,
                },
                late_head: match ln_mode.late_head {
                    LateHeadFile::CompletesAtPress => LateHeadRule::CompletesAtPress,
                },
            },
            total_mode: match total_mode {
                TotalModeFile::ScalesIncrease => TotalMode::ScalesIncrease,
            },
            mine: match mine {
                MineFile::WhileHeld { per_unit } => MineRule::WhileHeld {
                    per_unit: GaugeValue(per_unit),
                },
            },
        }
    }
}

impl From<&RulePreset> for PresetFile {
    fn from(p: &RulePreset) -> Self {
        // Destructured in full, so a new preset field fails to compile until
        // the file writes it.
        let RulePreset {
            id,
            version,
            implementation,
            windows,
            miss,
            empty_poor,
            ln_end,
            rank_scale,
            default_total_milli,
            denominator,
            gauges,
            score,
            same_time_order,
            assignment,
            ln_mode,
            total_mode,
            mine,
        } = p;
        Self {
            id: id.clone(),
            version: *version,
            implementation: match *implementation {
                Implementation::Standard { version } => ImplementationFile::Standard { version },
            },
            windows: WindowsFile {
                perfect: window_file(windows.perfect),
                great: window_file(windows.great),
                good: window_file(windows.good),
                bad: window_file(windows.bad),
            },
            miss: match miss {
                MissRule::PoorAfterLateBadOrOvertaken => MissFile::PoorAfterLateBadOrOvertaken,
            },
            empty_poor: {
                let EmptyPoorRule {
                    ahead,
                    behind,
                    breaks_combo,
                    counts_bp,
                    counted,
                } = *empty_poor;
                EmptyPoorFile {
                    ahead_us: ahead.0,
                    behind_us: behind.0,
                    breaks_combo,
                    counts_bp,
                    counted,
                }
            },
            ln_end: {
                let LnEndWindows {
                    perfect,
                    great,
                    good,
                } = *ln_end;
                LnEndFile {
                    perfect_early_us: perfect.0,
                    great_early_us: great.0,
                    good_early_us: good.0,
                }
            },
            rank_scale: RankScaleFile {
                rank_milli: rank_scale.rank_milli,
                absent_milli: rank_scale.absent_milli,
            },
            default_total_milli: *default_total_milli,
            denominator: match denominator {
                Denominator::TapsAndLnHeads => DenominatorFile::TapsAndLnHeads,
            },
            score: ScoreFile {
                ex: PerJudgmentFile::of(&score.ex, |v| v),
                breaks_combo: PerJudgmentFile::of(&score.breaks_combo, |v| v),
                counts_bp: PerJudgmentFile::of(&score.counts_bp, |v| v),
            },
            same_time_order: match same_time_order {
                SameTimeOrder::InputsFirst => SameTimeOrderFile::InputsFirst,
            },
            assignment: match assignment {
                Assignment::NearestTiesEarlier => AssignmentFile::NearestTiesEarlier,
            },
            ln_mode: LnModeFile {
                default: match ln_mode.default {
                    LnMode::Ln => LnKindFile::Ln,
                },
                late_head: match ln_mode.late_head {
                    LateHeadRule::CompletesAtPress => LateHeadFile::CompletesAtPress,
                },
            },
            total_mode: match total_mode {
                TotalMode::ScalesIncrease => TotalModeFile::ScalesIncrease,
            },
            mine: match *mine {
                MineRule::WhileHeld { per_unit } => MineFile::WhileHeld {
                    per_unit: per_unit.0,
                },
            },
            gauges: gauges
                .iter()
                .map(|g| GaugeFile {
                    id: g.id.clone(),
                    kind: match g.kind {
                        GaugeKind::EndThreshold { clear } => {
                            GaugeKindFile::EndThreshold { clear: clear.0 }
                        }
                        GaugeKind::Survival => GaugeKindFile::Survival,
                    },
                    start: g.start.0,
                    min: g.min.0,
                    max: g.max.0,
                    change: PerJudgmentFile::of(&g.change, delta_file),
                    empty_poor: delta_file(g.empty_poor),
                    low_damage: g.low_damage.map(|l| LowDamageFile {
                        below: l.below.0,
                        num: l.num,
                        den: l.den,
                    }),
                })
                .collect(),
        }
    }
}
