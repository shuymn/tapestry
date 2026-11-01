//! Rule presets as TOML files (design.md "Language and Crate Structure",
//! "Settings Files"). `rules` stays free of dependencies, so the file's shape
//! lives here as a separate set of types that convert to and from
//! [`RulePreset`] field by field.
//!
//! Times are integer µs and gauge values raw units of 1/100000 %, so a file
//! round-trips to the same canonical bytes and the same hash.
//!
//! A file holds a user preset (design.md "User Presets"): its name, which is
//! also its preset ID, is the file's stem, and the file names nothing
//! itself. A name is 1 to 64 characters of `a-z`, `0-9`, `-` and `_`, and
//! never a built-in's ([`rules::BUILT_IN_PRESETS`]); every reader of a user
//! preset checks it here.
//!
//! Before the first release the file's shape may change incompatibly. A file
//! of an older shape is refused as unreadable, never converted.

use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bms_model::TimeUs;
use rules::{
    Assignment, DefaultTotal, Denominator, EmptyPoorRange, EmptyPoorRule, GaugeDelta, GaugeKind,
    GaugeRule, GaugeValue, HeaderPolicy, Implementation, JudgeWindows, LaneTable, LateHeadRule,
    LnEndWindows, LnMode, LnModeRule, LowDamage, MineRule, MissRule, PerJudgment, PerLaneClass,
    RankScale, RuleError, RulePreset, SameTimeOrder, ScaledRank, ScoreWeights, TotalCorrection,
    TotalMode, Window, WindowPolicy,
};
use serde::{Deserialize, Serialize};

/// Why a preset's text could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    /// The name is not a user preset's.
    #[error(transparent)]
    Name(NameError),
    /// The text is not TOML of the preset's shape.
    #[error(
        "the preset file cannot be read (a file from an older version, one with an `id` \
         among them, is refused, not converted; `eval-cli preset export <built-in> <path>` \
         writes a built-in preset to a new file to edit)"
    )]
    Parse(#[source] toml::de::Error),
    /// The preset failed its validation.
    #[error("the preset is invalid")]
    Invalid(#[source] RuleError),
}

/// Why a name cannot be a user preset's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Not 1 to 64 characters of `a-z`, `0-9`, `-` and `_`.
    #[error(
        "`{0}` is not a preset name: a name is 1 to {MAX_PRESET_NAME} characters of a-z, 0-9, \
         `-` and `_`"
    )]
    NotAName(String),
    /// A built-in preset's name, which no file may take.
    #[error("`{0}` is the name of a built-in preset")]
    BuiltIn(String),
}

/// Why a preset file could not be used: the file, and the reason.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the preset file {} cannot be used", .path.display())]
pub struct PresetFileError {
    /// The file.
    pub path: PathBuf,
    /// Why.
    #[source]
    pub cause: PresetFileCause,
}

impl PresetFileError {
    /// The file system refused `path` with `kind`.
    #[must_use]
    pub const fn io(path: PathBuf, kind: io::ErrorKind) -> Self {
        Self {
            path,
            cause: PresetFileCause::Io(kind),
        }
    }
}

/// Why a preset file could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresetFileCause {
    /// The file could not be read.
    #[error("{0}")]
    Io(io::ErrorKind),
    /// The file is larger than [`MAX_PRESET_FILE_BYTES`].
    #[error(
        "the file is larger than {MAX_PRESET_FILE_BYTES} bytes, the largest preset file accepted"
    )]
    TooLarge,
    /// The path is a symbolic link, not a file.
    #[error("the path is a symbolic link; a preset file must be a file itself")]
    Link,
    /// The file is not UTF-8 text.
    #[error("the file is not UTF-8 text")]
    NotUtf8,
    /// The stem or the text is not a user preset's.
    #[error(transparent)]
    Preset(PresetError),
}

/// The longest preset name, in characters.
pub const MAX_PRESET_NAME: usize = 64;

/// Checks that `name` can name a user preset: 1 to [`MAX_PRESET_NAME`]
/// characters of `a-z`, `0-9`, `-` and `_`, and not a built-in's name.
///
/// # Errors
///
/// [`NameError::NotAName`] or [`NameError::BuiltIn`].
pub fn check_user_preset_name(name: &str) -> Result<(), NameError> {
    let valid = (1..=MAX_PRESET_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        return Err(NameError::NotAName(name.to_owned()));
    }
    if rules::BUILT_IN_PRESETS.contains(&name) {
        return Err(NameError::BuiltIn(name.to_owned()));
    }
    Ok(())
}

/// The user preset name a file at `path` takes: its stem, checked.
///
/// # Errors
///
/// The stem's [`NameError`]; a stem that is not Unicode is not a name.
pub fn user_preset_name(path: &Path) -> Result<&str, NameError> {
    let stem = path.file_stem().unwrap_or_default();
    let name = stem
        .to_str()
        .ok_or_else(|| NameError::NotAName(stem.to_string_lossy().into_owned()))?;
    check_user_preset_name(name)?;
    Ok(name)
}

/// Reads the user preset file at `path`, named by its stem, with its
/// modification time. The name is checked before the file is opened; the
/// file is read one byte past [`MAX_PRESET_FILE_BYTES`] at most, and its
/// content and time come from one open file, so a save that replaces the
/// file in between cannot pair old content with the new time.
///
/// # Errors
///
/// The file and why it cannot be used: its stem is not a user preset's
/// name, the path is a symbolic link, it cannot be read, it is too large or
/// not UTF-8, or its text is not a valid preset.
pub fn read_preset_file(path: &Path) -> Result<(RulePreset, Option<SystemTime>), PresetFileError> {
    let error = |cause| PresetFileError {
        path: path.to_owned(),
        cause,
    };
    let name =
        user_preset_name(path).map_err(|e| error(PresetFileCause::Preset(PresetError::Name(e))))?;
    let io = |e: io::Error| error(PresetFileCause::Io(e.kind()));
    // A link could read a file outside the folder, and its target's changes
    // would not show in the link's own modification time: the path itself
    // must be a file, before the open and still after it.
    let not_link = || match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(error(PresetFileCause::Link)),
        Ok(_) => Ok(()),
        Err(e) => Err(io(e)),
    };
    not_link()?;
    let file = fs::File::open(path).map_err(io)?;
    not_link()?;
    let modified = file.metadata().and_then(|m| m.modified()).ok();
    let limit = u64::try_from(MAX_PRESET_FILE_BYTES).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > MAX_PRESET_FILE_BYTES {
        return Err(error(PresetFileCause::TooLarge));
    }
    let text = String::from_utf8(bytes).map_err(|_| error(PresetFileCause::NotUtf8))?;
    let preset = load_preset_toml(name, &text).map_err(|e| error(PresetFileCause::Preset(e)))?;
    Ok((preset, modified))
}

/// The largest preset file a reader accepts, in bytes. A file is untrusted,
/// so a reader reads one byte past this at most and refuses a longer file.
/// The tapestry preset takes about 2 KiB, and the largest presets the
/// canonical encoding's property tests generate (131 gauges, ids of 136
/// characters TOML escapes) about 160 KiB; a test keeps them below half
/// the limit.
pub const MAX_PRESET_FILE_BYTES: usize = 1 << 20;

/// Reads the user preset `name` from TOML and validates it; `name` is its
/// preset ID.
///
/// # Errors
///
/// [`PresetError::Name`] for a name no user preset may take
/// ([`check_user_preset_name`]); [`PresetError::Parse`] for text that is not
/// a preset file, including an unknown key such as an older file's `id`;
/// [`PresetError::Invalid`] for a preset that fails
/// [`RulePreset::validate`].
pub fn load_preset_toml(name: &str, text: &str) -> Result<RulePreset, PresetError> {
    check_user_preset_name(name).map_err(PresetError::Name)?;
    let file: PresetFile = toml::from_str(text).map_err(PresetError::Parse)?;
    let preset = file.into_preset(name.to_owned());
    preset.validate().map_err(PresetError::Invalid)?;
    Ok(preset)
}

/// Writes a preset as TOML that [`load_preset_toml`] reads back, under any
/// name, to an equal preset with that ID. The ID is not written.
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
    version: u32,
    implementation: ImplementationFile,
    tables: TablesFile,
    miss: MissFile,
    empty_poor: EmptyPoorFile,
    headers: HeadersFile,
    window_policy: WindowPolicyFile,
    default_total: DefaultTotalFile,
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
enum HeadersFile {
    Decimals,
    DefExRankFirst,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MissFile {
    PoorAfterLateBadOrOvertaken,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TablesFile {
    keys: TableFile,
    scratch: TableFile,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableFile {
    windows: WindowsFile,
    ln_end: LnEndFile,
    empty_poor: EmptyPoorRangeFile,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyPoorRangeFile {
    ahead_us: i64,
    behind_us: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyPoorFile {
    breaks_combo: bool,
    counts_bp: bool,
    counted: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum WindowPolicyFile {
    Thousandths {
        rank_milli: [i64; 4],
        absent_milli: i64,
    },
    Scaled {
        rank_percent: [u32; 5],
        absent_percent: u32,
        defexrank_percent: u32,
    },
    Interpolated {
        rank_percent: [u32; 5],
        absent_percent: u32,
        defexrank_percent: u32,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LnEndFile {
    perfect_early_us: i64,
    great_early_us: i64,
    good_early_us: i64,
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
    ComboPriority,
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

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum DefaultTotalFile {
    Fixed { milli: i64 },
    NotesCurve,
    NotesLinear,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum GaugeKindFile {
    EndThreshold { clear: i64 },
    Survival { fails_below: i64 },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CorrectionFile {
    None,
    LimitIncrease,
    ScaleDamage,
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
    lamp: u16,
    kind: GaugeKindFile,
    start: i64,
    min: i64,
    max: i64,
    change: PerJudgmentFile<DeltaFile>,
    empty_poor: DeltaFile,
    correction: CorrectionFile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    low_damage: Vec<LowDamageFile>,
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

fn table(t: TableFile) -> LaneTable {
    let TableFile {
        windows,
        ln_end,
        empty_poor,
    } = t;
    LaneTable {
        windows: JudgeWindows {
            perfect: window(windows.perfect),
            great: window(windows.great),
            good: window(windows.good),
            bad: window(windows.bad),
        },
        ln_end: LnEndWindows {
            perfect: TimeUs(ln_end.perfect_early_us),
            great: TimeUs(ln_end.great_early_us),
            good: TimeUs(ln_end.good_early_us),
        },
        empty_poor: EmptyPoorRange {
            ahead: TimeUs(empty_poor.ahead_us),
            behind: TimeUs(empty_poor.behind_us),
        },
    }
}

fn table_file(t: &LaneTable) -> TableFile {
    let LaneTable {
        windows,
        ln_end,
        empty_poor,
    } = t;
    let LnEndWindows {
        perfect,
        great,
        good,
    } = *ln_end;
    let EmptyPoorRange { ahead, behind } = *empty_poor;
    TableFile {
        windows: WindowsFile {
            perfect: window_file(windows.perfect),
            great: window_file(windows.great),
            good: window_file(windows.good),
            bad: window_file(windows.bad),
        },
        ln_end: LnEndFile {
            perfect_early_us: perfect.0,
            great_early_us: great.0,
            good_early_us: good.0,
        },
        empty_poor: EmptyPoorRangeFile {
            ahead_us: ahead.0,
            behind_us: behind.0,
        },
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

impl PresetFile {
    /// The preset the file holds, named `id`: the file itself names nothing.
    fn into_preset(self, id: String) -> RulePreset {
        let Self {
            version,
            implementation,
            tables,
            miss,
            empty_poor,
            headers,
            window_policy,
            default_total,
            denominator,
            score,
            same_time_order,
            assignment,
            ln_mode,
            total_mode,
            mine,
            gauges,
        } = self;
        RulePreset {
            id,
            version,
            implementation: match implementation {
                ImplementationFile::Standard { version } => Implementation::Standard { version },
            },
            tables: PerLaneClass {
                keys: table(tables.keys),
                scratch: table(tables.scratch),
            },
            miss: match miss {
                MissFile::PoorAfterLateBadOrOvertaken => MissRule::PoorAfterLateBadOrOvertaken,
            },
            empty_poor: EmptyPoorRule {
                breaks_combo: empty_poor.breaks_combo,
                counts_bp: empty_poor.counts_bp,
                counted: empty_poor.counted,
            },
            headers: match headers {
                HeadersFile::Decimals => HeaderPolicy::Decimals,
                HeadersFile::DefExRankFirst => HeaderPolicy::DefExRankFirst,
            },
            window_policy: match window_policy {
                WindowPolicyFile::Thousandths {
                    rank_milli,
                    absent_milli,
                } => WindowPolicy::Thousandths(RankScale {
                    rank_milli,
                    absent_milli,
                }),
                WindowPolicyFile::Scaled {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                } => WindowPolicy::Scaled(ScaledRank {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                }),
                WindowPolicyFile::Interpolated {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                } => WindowPolicy::Interpolated(ScaledRank {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                }),
            },
            default_total: match default_total {
                DefaultTotalFile::Fixed { milli } => DefaultTotal::Fixed { milli },
                DefaultTotalFile::NotesCurve => DefaultTotal::NotesCurve,
                DefaultTotalFile::NotesLinear => DefaultTotal::NotesLinear,
            },
            denominator: match denominator {
                DenominatorFile::TapsAndLnHeads => Denominator::TapsAndLnHeads,
            },
            gauges: gauges
                .into_iter()
                .map(|g| GaugeRule {
                    id: g.id,
                    lamp: g.lamp,
                    kind: match g.kind {
                        GaugeKindFile::EndThreshold { clear } => GaugeKind::EndThreshold {
                            clear: GaugeValue(clear),
                        },
                        GaugeKindFile::Survival { fails_below } => GaugeKind::Survival {
                            fails_below: GaugeValue(fails_below),
                        },
                    },
                    start: GaugeValue(g.start),
                    min: GaugeValue(g.min),
                    max: GaugeValue(g.max),
                    change: g.change.map(delta),
                    empty_poor: delta(g.empty_poor),
                    correction: match g.correction {
                        CorrectionFile::None => TotalCorrection::None,
                        CorrectionFile::LimitIncrease => TotalCorrection::LimitIncrease,
                        CorrectionFile::ScaleDamage => TotalCorrection::ScaleDamage,
                    },
                    low_damage: g
                        .low_damage
                        .into_iter()
                        .map(|l| LowDamage {
                            below: GaugeValue(l.below),
                            num: l.num,
                            den: l.den,
                        })
                        .collect(),
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
                AssignmentFile::ComboPriority => Assignment::ComboPriority,
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
        // the file writes it. The ID is the file's stem, never written.
        let RulePreset {
            id: _,
            version,
            implementation,
            tables,
            miss,
            empty_poor,
            headers,
            window_policy,
            default_total,
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
            version: *version,
            implementation: match *implementation {
                Implementation::Standard { version } => ImplementationFile::Standard { version },
            },
            tables: {
                let PerLaneClass { keys, scratch } = tables;
                TablesFile {
                    keys: table_file(keys),
                    scratch: table_file(scratch),
                }
            },
            miss: match miss {
                MissRule::PoorAfterLateBadOrOvertaken => MissFile::PoorAfterLateBadOrOvertaken,
            },
            empty_poor: {
                let EmptyPoorRule {
                    breaks_combo,
                    counts_bp,
                    counted,
                } = *empty_poor;
                EmptyPoorFile {
                    breaks_combo,
                    counts_bp,
                    counted,
                }
            },
            headers: match headers {
                HeaderPolicy::Decimals => HeadersFile::Decimals,
                HeaderPolicy::DefExRankFirst => HeadersFile::DefExRankFirst,
            },
            window_policy: match *window_policy {
                WindowPolicy::Thousandths(RankScale {
                    rank_milli,
                    absent_milli,
                }) => WindowPolicyFile::Thousandths {
                    rank_milli,
                    absent_milli,
                },
                WindowPolicy::Scaled(ScaledRank {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                }) => WindowPolicyFile::Scaled {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                },
                WindowPolicy::Interpolated(ScaledRank {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                }) => WindowPolicyFile::Interpolated {
                    rank_percent,
                    absent_percent,
                    defexrank_percent,
                },
            },
            default_total: match *default_total {
                DefaultTotal::Fixed { milli } => DefaultTotalFile::Fixed { milli },
                DefaultTotal::NotesCurve => DefaultTotalFile::NotesCurve,
                DefaultTotal::NotesLinear => DefaultTotalFile::NotesLinear,
            },
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
                Assignment::ComboPriority => AssignmentFile::ComboPriority,
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
                    lamp: g.lamp,
                    kind: match g.kind {
                        GaugeKind::EndThreshold { clear } => {
                            GaugeKindFile::EndThreshold { clear: clear.0 }
                        }
                        GaugeKind::Survival { fails_below } => GaugeKindFile::Survival {
                            fails_below: fails_below.0,
                        },
                    },
                    start: g.start.0,
                    min: g.min.0,
                    max: g.max.0,
                    change: PerJudgmentFile::of(&g.change, delta_file),
                    empty_poor: delta_file(g.empty_poor),
                    correction: match g.correction {
                        TotalCorrection::None => CorrectionFile::None,
                        TotalCorrection::LimitIncrease => CorrectionFile::LimitIncrease,
                        TotalCorrection::ScaleDamage => CorrectionFile::ScaleDamage,
                    },
                    low_damage: g
                        .low_damage
                        .iter()
                        .map(|l| LowDamageFile {
                            below: l.below.0,
                            num: l.num,
                            den: l.den,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}
