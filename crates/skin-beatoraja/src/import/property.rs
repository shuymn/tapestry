//! Effective number and text properties: what the source's
//! property factories return for an id or a name, and which of those the
//! model shows, on each kind of screen.

use skin_model::{ScreenKind, SelectionText};

use super::result::{self, profile::ResValue};
use super::selection::profile::{Class, SelText, SelValue};
use super::selection::{self};

/// The ids of `IntegerPropertyFactory.ValueType` with their names
/// (`IntegerPropertyFactory.java:64-~490` at `0.8.9`), sorted by id.
pub(crate) const VALUE_TYPES: &[(i32, &str)] = &[
    (10, "hispeed_lr2"),
    (12, "notesdisplaytiming"),
    (14, "lanecover1"),
    (17, "playtime_total_hour"),
    (18, "playtime_total_minute"),
    (19, "playtime_totla_saecond"),
    (20, "current_fps"),
    (21, "currenttime_year"),
    (22, "currenttime_month"),
    (23, "currenttime_day"),
    (24, "currenttime_hour"),
    (25, "currenttime_minute"),
    (26, "currenttime_saecond"),
    (27, "boottime_hour"),
    (28, "boottime_minute"),
    (29, "boottime_second"),
    (30, "player_playcount"),
    (31, "player_clearcount"),
    (32, "player_failcount"),
    (33, "player_perfect"),
    (34, "player_great"),
    (35, "player_good"),
    (36, "player_bad"),
    (37, "player_poor"),
    (45, "folder_level_beginner"),
    (46, "folder_level_normal"),
    (47, "folder_level_hyper"),
    (48, "folder_level_another"),
    (49, "folder_level_insane"),
    (57, "volume_system"),
    (58, "volume_key"),
    (59, "volume_background"),
    (71, "score"),
    (72, "maxscore"),
    (74, "totalnotes"),
    (75, "maxcombo"),
    (76, "misscount"),
    (77, "playcount"),
    (78, "clearcount"),
    (79, "failcount"),
    (90, "maxbpm"),
    (91, "minbpm"),
    (92, "mainbpm"),
    (96, "playlevel"),
    (100, "point"),
    (101, "score2"),
    (102, "score_rate"),
    (103, "score_rate_afterdot"),
    (105, "maxcombo2"),
    (106, "totalnotes2"),
    (107, "groovegauge"),
    (108, "diff_exscore"),
    (115, "total_rate"),
    (116, "total_rate_afterdot"),
    (121, "target_score"),
    (122, "target_score_rate"),
    (123, "target_score_rate_afterdot"),
    (128, "diff_exscore2"),
    (135, "target_total_rate"),
    (136, "target_total_rate_afterdot"),
    (150, "highscore"),
    (151, "target_score2"),
    (152, "diff_highscore"),
    (153, "diff_targetscore"),
    (154, "diff_nextrank"),
    (155, "score_rate2"),
    (156, "score_rate_afterdot2"),
    (157, "target_score_rate2"),
    (158, "target_score_rate_afterdot2"),
    (160, "nowbpm"),
    (161, "playtime_minute"),
    (162, "playtime_second"),
    (163, "timeleft_minute"),
    (164, "timeleft_second"),
    (165, "loading_progress"),
    (170, "highscore2"),
    (171, "score3"),
    (172, "diff_highscore2"),
    (173, "target_maxcombo"),
    (174, "maxcombo3"),
    (175, "diff_maxcombo"),
    (176, "target_misscount"),
    (177, "misscount2"),
    (178, "diff_misscount"),
    (179, "ir_rank"),
    (180, "ir_totalplayer"),
    (182, "ir_prevrank"),
    (183, "best_rate"),
    (184, "best_rate_afterdot"),
    (200, "ir_totalplayer2"),
    (220, "ir_update_waiting"),
    (226, "ir_totalclear"),
    (227, "ir_totalclearrate"),
    (228, "ir_totalfullcombo"),
    (229, "ir_totalfullcomborate"),
    (241, "ir_totalclearrate_afterdot"),
    (242, "ir_totalfullcomborate_afterdot"),
    (243, "lastplay_timestamp"),
    (244, "lastplay_year"),
    (245, "lastplay_month"),
    (246, "lastplay_day"),
    (247, "lastplay_hour"),
    (248, "lastplay_minute"),
    (249, "lastplay_second"),
    (271, "rival_score"),
    (300, "folder_totalsongs"),
    (310, "hispeed"),
    (311, "hispeed_afterdot"),
    (312, "duration"),
    (313, "duration_green"),
    (314, "lift1"),
    (315, "hidden1"),
    (316, "lanecover2"),
    (333, "player_notes"),
    (350, "chart_totalnote_n"),
    (351, "chart_totalnote_ln"),
    (352, "chart_totalnote_s"),
    (353, "chart_totalnote_ls"),
    (360, "chart_peakdensity"),
    (361, "chart_peakdensity_afterdot"),
    (362, "chart_enddensity"),
    (363, "chart_enddensity_peak"),
    (364, "chart_averagedensity"),
    (365, "chart_averagedensity_afterdot"),
    (368, "chart_totalgauge"),
    (370, "clear"),
    (371, "target_clear"),
    (372, "duration_average"),
    (373, "duration_average_afterdot"),
    (374, "timing_average"),
    (375, "timing_average_afterdot"),
    (376, "timing_stddev"),
    (377, "timing_atddev_afterdot"),
    (400, "judgerank"),
    (407, "groovegauge_afterdot"),
    (420, "miss"),
    (421, "early_miss"),
    (422, "late_miss"),
    (423, "totalearly"),
    (424, "totallate"),
    (425, "combobreak"),
    (426, "poor_plus_miss"),
    (427, "bad_plus_poor_plus_miss"),
    (525, "judge_duration1"),
    (526, "judge_duration2"),
    (527, "judge_duration3"),
    (1163, "chartlength_minute"),
    (1164, "chartlength_second"),
];

/// The id ranges `ValueType.getProperty` builds
/// (`IntegerPropertyFactory.java:496-532`): durations, best and rival
/// counts and rates, judgment counts and their early/late splits.
const VALUE_RANGES: &[(i32, i32)] = &[(80, 89), (110, 114), (280, 289), (410, 419), (1_312, 1_327)];

/// The numbered patterns (`IntegerPropertyFactory.java:984-1000`): ids,
/// and names as a prefix with a 1-based number.
const VALUE_PATTERN_IDS: &[i32] = &[
    // ranking ex score and index, 380-399; folder clear counts, 320-330
    // (named below); the IR clear counts and rates.
    202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 222,
    223, 224, 225, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240,
];

const FOLDER_NAMES: [&str; 11] = [
    "folder_noplay",
    "folder_failed",
    "folder_assist",
    "folder_lightassist",
    "folder_easy",
    "folder_normal",
    "folder_hard",
    "folder_exhard",
    "folder_fullcombo",
    "folder_prefect",
    "folder_max",
];

const IR_CLEARS: [&str; 11] = [
    "noplay",
    "failed",
    "assist",
    "lightassist",
    "easy",
    "normal",
    "hard",
    "exhard",
    "fullcombo",
    "perfect",
    "max",
];

/// The string property ids and names of `StringPropertyFactory.StringType`
/// (`StringPropertyFactory.java:~176-~400`), sorted by id.
pub(crate) const STRING_TYPES: &[(i32, &str)] = &[
    (1, "rival"),
    (2, "player"),
    (3, "target"),
    (10, "title"),
    (11, "subtitle"),
    (12, "fulltitle"),
    (13, "genre"),
    (14, "artist"),
    (15, "subartist"),
    (16, "fullartist"),
    (30, "searchword"),
    (50, "skinname"),
    (51, "skinauthor"),
    (60, "mode"),
    (61, "sort"),
    (62, "difficulty"),
    (86, "chartreplication"),
    (1000, "directory"),
    (1001, "tablename"),
    (1002, "tablelevel"),
    (1003, "tablefull"),
    (1010, "version"),
    (1020, "irname"),
    (1021, "irUserName"),
    (1030, "songhashmd5"),
    (1031, "songhashsha256"),
];

/// The numbered string patterns (`StringPropertyFactory.java:79-99`):
/// first id, count, name prefix and suffix, and the first name number
/// (the index mappers, `StringPropertyFactory.java:81, 87, 150-153`).
const STRING_PATTERNS: &[(i32, i32, &str, &str, i32)] = &[
    (40, 10, "key", "", 1),
    (240, 44, "key", "", 11),
    (100, 10, "skincategory", "", 1),
    (110, 10, "skinitem", "", 1),
    (120, 10, "rankingname", "", 1),
    (150, 10, "coursetitle", "", 1),
    (200, 10, "targetnamep", "", 1),
    (210, 10, "targetnamen", "", 1),
    (1_040, 16, "practice_item", "", 1),
    (1_060, 16, "practice_item", "_label", 1),
    (1_060, 16, "practice_item_label", "", 1),
    (1_080, 16, "practice_item", "_value", 1),
    (1_080, 16, "practice_item_value", "", 1),
];

/// What an integer property shows in the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    ExScore,
    MaxCombo,
    /// The target count (D4 during section practice).
    Notes,
    /// The source's 7-key point (`point`, 100; `ScoreDataProperty.java:
    /// 76-79`), over the attempt's targets: a section's during section
    /// practice, as the source's practice counts only its section's notes
    /// (`pattern/PracticeModifier.java:30-40`, `play/JudgeManager.java:135`).
    Point,
    /// The gauge's whole percent.
    GaugePercent,
    /// The gauge's tenths of a percent (D4 below 0.1 %).
    GaugeTenths,
    /// A judgment count: `count.perfect`..`count.poor`, then
    /// `count.empty_poor`.
    Count(u8),
    /// The scored POORs, early and late: empty POORs never among them,
    /// whatever the preset's `empty_poor.counted`, as the source keeps its
    /// judge index 4 apart from 5 (`ScoreData.java:277-278`). `Count(4)`
    /// includes them under such a preset.
    ScoredPoor,
    /// `combobreak` (425, `IntegerPropertyFactory.java:461-462`): BAD and
    /// the scored POORs.
    ComboBreak,
    /// `bad_plus_poor_plus_miss` (427, `:465-467`): those and the empty
    /// POORs.
    BadPoorMiss,
    /// `totalearly` and `totallate` (423, 424): `count.fast` and
    /// `count.slow`, the early and the late judgments GREAT to POOR and
    /// empty POORs, as the source sums judges 1 to 5
    /// (`IntegerPropertyFactory.java`, `totalearly`, `totallate`).
    Fast,
    Slow,
    /// A judgment's early and late counts, `count.early.perfect`..
    /// `count.early.poor`, then `count.early.empty_poor`, and the same
    /// late: 410 to 419 in early and late pairs PERFECT to POOR, 421
    /// (`early_miss`) and 422 (`late_miss`) for the empty POOR. Early is
    /// pressed at or before the note, as the source's `note − press ≥ 0`;
    /// a miss is late; a long note takes the side of whichever of its
    /// head and its release lies farther from its time, the release on a
    /// tie (`play/JudgeManager.java:511-520`, in `update`, which compares
    /// the two distances; `updateMicro`, which counts).
    Early(u8),
    Late(u8),
    /// The rate over the targets judged so far, whole percent and
    /// hundredths (D4): the source's `getNowRate*`
    /// (`IntegerPropertyFactory.java:176-180`).
    CurrentRatePercent,
    CurrentRateFraction,
    /// The rate over the attempt's targets, whole percent and hundredths
    /// (D4): the source's `getRate*`
    /// (`IntegerPropertyFactory.java:187-188, 200-201, 690-699`).
    WholeRatePercent,
    WholeRateFraction,
    /// The chart's highest and lowest BPM, whole (`maxbpm`, `minbpm`;
    /// `IntegerPropertyFactory.java:161-166`), while the range is known.
    BpmMax,
    BpmMin,
    /// The tempo at the display head, whole (`nowbpm`, `:204`), while it
    /// is known.
    BpmNow,
    /// The elapsed play time's minutes and seconds (`playtime_minute`,
    /// `playtime_second`, `:205-206`).
    ElapsedMinutes,
    ElapsedSeconds,
    /// The remaining time's minutes and seconds (`timeleft_minute`,
    /// `timeleft_second`, `:207-222`), rounded up without the source's
    /// added second and margin.
    RemainingMinutes,
    RemainingSeconds,
    /// The configured lane cover, lift and hidden cover in ‰, and the lane
    /// cover over the whole lane (`lanecover1`, `lift1`, `hidden1`,
    /// `lanecover2`, `:68-89`), whatever their switches.
    CoverPermille,
    LiftPermille,
    HiddenPermille,
    CoverAbsolutePermille,
    /// The multiplicative hi-speed in hundredths, whole and its
    /// hundredths (`hispeed_lr2`, `hispeed`, `hispeed_afterdot`,
    /// `:586-602`), while the scroll policy has one.
    HispeedX100,
    HispeedWhole,
    HispeedFraction,
    /// The travel duration in ms and its green number (`duration`,
    /// `duration_green`, `:292-314`), while it is known.
    Duration,
    DurationGreen,
    /// The chart's play level (`playlevel`, `:154`, `:580-584`), while it
    /// is known.
    PlayLevel,
    /// One of the eight duration variants' ms or green number, 1312 to
    /// 1327 as `id − 1312` (`:532-556`): the tempo `n / 4` (current,
    /// MAIN, minimum, maximum), with the configured lane cover or none
    /// (`n / 2 % 2`), the green number for an odd `n`; each rounded half
    /// up from the exact ratio (D4), while its variant is known.
    DurationVariant(u8),
    /// `judge_duration1` (525, `:469`, `:909-916`): the latest judgment's
    /// timing, note time minus press in ms (`play/JudgeManager.java:457`,
    /// `:690`), as `judge.recent_ms` takes it, read whichever widget draws
    /// judgments, where the source sets it only for a judge region its
    /// judge widget makes (`play/JudgeManager.java:129-133`, `:678`).
    RecentTiming,
    /// `chart_totalgauge` (368, `:389-394`): the song information's gauge
    /// total, `(int) getTotal()`: the chart's `#TOTAL` read as the
    /// reference decoder reads it, else the default its rule sets for an
    /// absent one (`play/BMSPlayerRule.java:68-80`), whatever the play
    /// preset, while the chart is known. The source has the song
    /// information on a direct launch too (`PlayerResource.java:175`,
    /// `song/SongData.java:130-139`, `:219`).
    SongTotal,
    /// A selection number, over the selection schema
    /// ([`super::selection::profile::number`]).
    Selection(SelValue),
    /// A result number, over the result schema
    /// ([`super::result::profile::number`]).
    Result(ResValue),
}

/// The fields of [`Value::DurationVariant`] by `id − 1312`.
const DURATION_VARIANTS: [&str; 16] = [
    "duration.current.covered.ms",
    "duration.current.covered.green",
    "duration.current.uncovered.ms",
    "duration.current.uncovered.green",
    "duration.main.covered.ms",
    "duration.main.covered.green",
    "duration.main.uncovered.ms",
    "duration.main.uncovered.green",
    "duration.min.covered.ms",
    "duration.min.covered.green",
    "duration.min.uncovered.ms",
    "duration.min.uncovered.green",
    "duration.max.covered.ms",
    "duration.max.covered.green",
    "duration.max.uncovered.ms",
    "duration.max.uncovered.green",
];

/// The validity of [`Value::DurationVariant`] by `(id − 1312) / 2`.
const DURATION_VARIANTS_VALID: [&str; 8] = [
    "duration.current.covered.valid",
    "duration.current.uncovered.valid",
    "duration.main.covered.valid",
    "duration.main.uncovered.valid",
    "duration.min.covered.valid",
    "duration.min.uncovered.valid",
    "duration.max.covered.valid",
    "duration.max.uncovered.valid",
];

impl Value {
    /// The model expression.
    pub(crate) fn expr(self) -> &'static str {
        match self {
            Self::ExScore => "ex_score",
            Self::MaxCombo => "max_combo",
            Self::Notes => "notes",
            Self::Point => "point",
            Self::GaugePercent => "gauge / 10",
            Self::GaugeTenths => "gauge % 10",
            Self::Count(0) => "count.perfect",
            Self::Count(1) => "count.great",
            Self::Count(2) => "count.good",
            Self::Count(3) => "count.bad",
            Self::Count(4) => "count.poor",
            Self::Count(_) => "count.empty_poor",
            Self::ScoredPoor => "count.early.poor + count.late.poor",
            Self::ComboBreak => "count.bad + count.early.poor + count.late.poor",
            Self::BadPoorMiss => {
                "count.bad + count.early.poor + count.late.poor + count.empty_poor"
            }
            Self::Fast => "count.fast",
            Self::Slow => "count.slow",
            Self::Early(0) => "count.early.perfect",
            Self::Early(1) => "count.early.great",
            Self::Early(2) => "count.early.good",
            Self::Early(3) => "count.early.bad",
            Self::Early(4) => "count.early.poor",
            Self::Early(_) => "count.early.empty_poor",
            Self::Late(0) => "count.late.perfect",
            Self::Late(1) => "count.late.great",
            Self::Late(2) => "count.late.good",
            Self::Late(3) => "count.late.bad",
            Self::Late(4) => "count.late.poor",
            Self::Late(_) => "count.late.empty_poor",
            Self::CurrentRatePercent => "rate.current_bp / 100",
            Self::CurrentRateFraction => "rate.current_bp % 100",
            Self::WholeRatePercent => "rate.whole_bp / 100",
            Self::WholeRateFraction => "rate.whole_bp % 100",
            Self::BpmMax => "bpm.max_x100 / 100",
            Self::BpmMin => "bpm.min_x100 / 100",
            Self::BpmNow => "bpm.current_x100 / 100",
            Self::ElapsedMinutes => "time.elapsed_s / 60",
            Self::ElapsedSeconds => "time.elapsed_s % 60",
            Self::RemainingMinutes => "time.remaining_s / 60",
            Self::RemainingSeconds => "time.remaining_s % 60",
            Self::CoverPermille => "cover.permille",
            Self::LiftPermille => "lift.permille",
            Self::HiddenPermille => "hidden.permille",
            Self::CoverAbsolutePermille => "cover.absolute_permille",
            Self::HispeedX100 => "hispeed.x100",
            Self::HispeedWhole => "hispeed.x100 / 100",
            Self::HispeedFraction => "hispeed.x100 % 100",
            Self::Duration => "duration.ms",
            Self::DurationGreen => "duration.green",
            Self::PlayLevel => "chart.level",
            Self::RecentTiming => "judge.recent_ms",
            Self::SongTotal => "chart.total",
            Self::Selection(v) => v.expr(),
            Self::Result(v) => v.expr,
            Self::DurationVariant(n) => DURATION_VARIANTS
                .get(usize::from(n))
                .copied()
                .unwrap_or("duration.max.uncovered.green"),
        }
    }

    /// The condition the value is drawn under, when it can be unknown:
    /// an unknown value draws nothing rather than a 0.
    pub(crate) fn availability(self) -> Option<&'static str> {
        match self {
            Self::BpmMax | Self::BpmMin => Some("bpm.range_valid"),
            Self::BpmNow => Some("bpm.current_valid"),
            Self::HispeedX100 | Self::HispeedWhole | Self::HispeedFraction => Some("hispeed.valid"),
            Self::Duration | Self::DurationGreen => Some("duration.valid"),
            Self::PlayLevel => Some("chart.level_valid"),
            Self::SongTotal => Some("chart.total_valid"),
            Self::DurationVariant(n) => DURATION_VARIANTS_VALID.get(usize::from(n / 2)).copied(),
            Self::Selection(v) => Some(v.availability()),
            Self::Result(v) => Some(v.guard),
            _ => None,
        }
    }

    /// Whether the value differs from the source in an owner-decided way.
    pub(crate) fn deviates(self) -> bool {
        !matches!(
            self,
            Self::ExScore
                | Self::MaxCombo
                | Self::Point
                | Self::GaugePercent
                | Self::Count(_)
                | Self::ScoredPoor
                | Self::ComboBreak
                | Self::BadPoorMiss
                | Self::Fast
                | Self::Slow
                | Self::Early(_)
                | Self::Late(_)
                | Self::PlayLevel
                | Self::RecentTiming
                | Self::SongTotal
                | Self::Selection(_)
                | Self::Result(_)
        )
    }

    /// The essential part a result value shows (owner decision O4).
    pub(crate) fn essential(self) -> Option<super::policy::EssentialPart> {
        match self {
            Self::Result(v) => v.part,
            _ => None,
        }
    }

    /// The role of an element showing the value: its critical or
    /// essential part's, else decorative.
    pub(crate) fn role(self) -> super::policy::Role {
        use super::policy::Role;
        self.part()
            .map(Role::Critical)
            .or(self.essential().map(Role::Essential))
            .unwrap_or(Role::Decorative)
    }

    /// The critical part a play-critical value shows (owner decision 5).
    pub(crate) fn part(self) -> Option<super::policy::CriticalPart> {
        match self {
            Self::ExScore => Some(super::policy::CriticalPart::ExScore),
            Self::MaxCombo => Some(super::policy::CriticalPart::MaxCombo),
            _ => None,
        }
    }

    /// The least and greatest value [`Self::expr`] gives: its field's
    /// declared range, divided or reduced as the expression is.
    pub(crate) fn range(self) -> (i64, i64) {
        let sum = |terms: i64| (0, terms * i64::from(skin_model::MAX_COUNT));
        match self {
            Self::ScoredPoor => return sum(2),
            Self::ComboBreak => return sum(3),
            Self::BadPoorMiss => return sum(4),
            Self::Result(v) => return v.range,
            _ => {}
        }
        let e = self.expr();
        let (name, op) = e.split_once(' ').map_or((e, None), |(n, r)| (n, Some(r)));
        let schema = match self {
            Self::Selection(_) => &skin_model::SELECTION,
            _ => &skin_model::PLAY,
        };
        let (lo, hi) = match schema.field(name).map(|(_, f)| f.ty) {
            Some(skin_model::FieldTy::Int { lo, hi }) => (i64::from(lo), i64::from(hi)),
            // Never met (a test reads every value); the widest range is
            // the safe one.
            _ => (i64::from(i32::MIN), i64::from(i32::MAX)),
        };
        let by = |r: &str| {
            r.get(2..)
                .and_then(|k| k.parse::<i64>().ok())
                .filter(|&k| k > 0)
        };
        match op {
            Some(r) if r.starts_with('/') => by(r).map_or((lo, hi), |k| (lo / k, hi / k)),
            Some(r) if r.starts_with('%') => by(r).map_or((lo, hi), |k| (0, hi.min(k - 1))),
            _ => (lo, hi),
        }
    }
}

/// An integer property as the factory resolves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntProp {
    /// The factory returns null: nothing is drawn.
    Null,
    /// A property the model shows.
    Shown(Value),
    /// A property the model has no value for, by id when it has one.
    Unsupported(Option<i32>),
    /// A selection property bridged only with the native settings (class
    /// E of the selection profile): it draws nothing until then.
    Unavailable(i32),
}

/// A `value` field as the value-property factory resolves it on a `kind`
/// screen (`JsonSkinSerializer.java:103-104`): an id or a name; a name
/// the factory does not know is a script, which the model does not run.
pub(crate) fn value_prop(kind: ScreenKind, value: &super::Prop) -> IntProp {
    match value {
        super::Prop::Id(n) => int_in(kind, *n),
        super::Prop::Name(s) => name_id(s).map_or_else(
            || int_by_name(s).unwrap_or(IntProp::Unsupported(None)),
            |id| int_in(kind, id),
        ),
        super::Prop::Null | super::Prop::Callable(_) => IntProp::Null,
    }
}

/// The id of integer property `name`, when the factory names one.
fn name_id(name: &str) -> Option<i32> {
    VALUE_TYPES
        .iter()
        .find(|(_, n)| *n == name)
        .map(|&(id, _)| id)
}

/// Integer property `id` on a `kind` screen: play's mapping, or the
/// selection or result profile's ([`selection::profile::number`],
/// [`super::result::profile::number`]), an id outside
/// it as the factory knows it ([`int_by_id`]) but shown by nothing.
pub(crate) fn int_in(kind: ScreenKind, id: i32) -> IntProp {
    let class = match kind {
        ScreenKind::Selection => selection::profile::number(id).map(|c| c.map(Value::Selection)),
        ScreenKind::Result => result::profile::number(id).map(|c| c.map(Value::Result)),
        ScreenKind::Play | ScreenKind::Decision => return int_by_id(id),
    };
    match class {
        Some(Class::Bridge(v)) => IntProp::Shown(v),
        Some(Class::Unavailable) => IntProp::Unavailable(id),
        Some(Class::Omit) => IntProp::Unsupported(Some(id)),
        None => match int_by_id(id) {
            IntProp::Shown(_) => IntProp::Unsupported(Some(id)),
            p => p,
        },
    }
}

/// The model value of known integer property `id`.
fn shown(id: i32) -> Option<Value> {
    Some(match id {
        71 | 101 => Value::ExScore,
        75 | 105 => Value::MaxCombo,
        74 | 106 => Value::Notes,
        100 => Value::Point,
        107 => Value::GaugePercent,
        407 => Value::GaugeTenths,
        102 => Value::CurrentRatePercent,
        103 => Value::CurrentRateFraction,
        115 | 155 => Value::WholeRatePercent,
        116 | 156 => Value::WholeRateFraction,
        110..=114 => Value::Count(u8::try_from(id - 110).ok()?),
        420 => Value::Count(5),
        425 => Value::ComboBreak,
        427 => Value::BadPoorMiss,
        423 => Value::Fast,
        424 => Value::Slow,
        410..=419 => {
            let grade = u8::try_from((id - 410) / 2).ok()?;
            if id % 2 == 0 {
                Value::Early(grade)
            } else {
                Value::Late(grade)
            }
        }
        421 => Value::Early(5),
        422 => Value::Late(5),
        90 => Value::BpmMax,
        91 => Value::BpmMin,
        96 => Value::PlayLevel,
        525 => Value::RecentTiming,
        368 => Value::SongTotal,
        160 => Value::BpmNow,
        161 => Value::ElapsedMinutes,
        162 => Value::ElapsedSeconds,
        163 => Value::RemainingMinutes,
        164 => Value::RemainingSeconds,
        14 => Value::CoverPermille,
        314 => Value::LiftPermille,
        315 => Value::HiddenPermille,
        316 => Value::CoverAbsolutePermille,
        10 => Value::HispeedX100,
        310 => Value::HispeedWhole,
        311 => Value::HispeedFraction,
        312 => Value::Duration,
        313 => Value::DurationGreen,
        1_312..=1_327 => Value::DurationVariant(u8::try_from(id - 1_312).ok()?),
        _ => return None,
    })
}

/// What `main_state.judge(n)` reads (`MainStatePropertyLuaApiExporter.java:
/// 87-93`): the count of judge index `n`, 0 PERFECT to 5 the empty POOR
/// (`ScoreData.java:266`), `None` for any other index, which the source
/// reads as 0 (`:279`).
pub(crate) fn judge_count(n: i64) -> Option<Value> {
    Some(match n {
        0..=3 => Value::Count(u8::try_from(n).ok()?),
        4 => Value::ScoredPoor,
        5 => Value::Count(5),
        _ => return None,
    })
}

/// `getIntegerProperty(int)` (`IntegerPropertyFactory.java:36-49`).
pub(crate) fn int_by_id(id: i32) -> IntProp {
    let known = VALUE_TYPES.binary_search_by_key(&id, |&(i, _)| i).is_ok()
        || VALUE_RANGES.iter().any(|&(a, b)| (a..=b).contains(&id))
        || (380..=399).contains(&id)
        || (320..=330).contains(&id)
        || VALUE_PATTERN_IDS.contains(&id);
    if !known {
        return IntProp::Null;
    }
    shown(id).map_or(IntProp::Unsupported(Some(id)), IntProp::Shown)
}

/// Whether `name` is `prefix`, a number in `first..first + count`, and
/// `suffix`.
fn numbered(name: &str, prefix: &str, suffix: &str, first: i32, count: i32) -> Option<i32> {
    let n = name
        .strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse::<i32>()
        .ok()?;
    (first..first + count).contains(&n).then_some(n)
}

/// The pattern names' numbers 1 to 10, as `numberedNames` writes them
/// (`IntegerPropertyFactory.java:1072-1078`).
const RANKS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"];

/// `getIntegerProperty(String)` (`IntegerPropertyFactory.java:51-62`):
/// `None` for a name the factory does not know, a script. Every pattern
/// name is matched by equality, as the factory's are
/// (`IntegerPropertyFactory.java:1029-1037`).
pub(crate) fn int_by_name(name: &str) -> Option<IntProp> {
    if let Some(&(id, _)) = VALUE_TYPES.iter().find(|(_, n)| *n == name) {
        return Some(int_by_id(id));
    }
    let ranked = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|n| RANKS.contains(&n))
    };
    let pattern = ranked("ranking_exscore")
        || ranked("ranking_index")
        || FOLDER_NAMES.contains(&name)
        || name.strip_prefix("ir_player_").is_some_and(|rest| {
            IR_CLEARS.iter().any(|c| {
                rest.strip_prefix(c)
                    .is_some_and(|suffix| ["", "_rate", "_rate_afterdot"].contains(&suffix))
            })
        });
    pattern.then_some(IntProp::Unsupported(None))
}

/// An image-index property (`getImageIndexProperty(int)`,
/// `IntegerPropertyFactory.java:920-961`), the namespace of an image
/// group's `ref` and an image set's `ref`: apart from the value
/// properties, where 40 resolves to nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexProp {
    /// The factory returns null: the source selects entry 0
    /// (`SkinImage.java:123-125`).
    Null,
    /// `gaugetype_1p` (40, `IntegerPropertyFactory.java:1128-1135`): the
    /// shown gauge's type, 0-5 in ordinary play, read from `gauge.kind`
    /// while `gauge.valid`.
    GaugeKind,
    /// `lanecover`, `lift`, `hidden` (330-332, `:1261-1295`): whether
    /// each cover is switched on, read from the play's current switches.
    /// The source reads the configuration's flags while its lanes draw
    /// from a copy (`LaneRenderer.java:91`), so a switch toggled in play
    /// (`ControlInputProcessor.java:162-166`) changes ours, not its.
    CoverEnabled,
    LiftEnabled,
    HiddenEnabled,
    /// `option_1p` (42, `IntegerPropertyFactory.java:1136-1143`): the lane
    /// option, 0 (OFF), as tapestry applies none.
    RandomOption,
    /// The 1P lanes' visual states (500-507, `:929-935`,
    /// `play/JudgeManager.java:805-811`), by the source's skin offset: 0 the
    /// scratch, 1-7 the keys, as [`bms_model::Lane::index`] numbers them
    /// (`play/LaneProperty.java:50`). Read from `judge.lane.N`.
    LaneJudge(u8),
    /// A selection image index's selector over the selection schema
    /// ([`selection::profile::image_index`]).
    Selection(&'static str),
    /// A selection setting or action button, bridged only with the native
    /// settings: it draws nothing until then.
    Unavailable(i32),
    /// A property the model has no value for.
    Unsupported(i32),
}

/// The fields of [`IndexProp::LaneJudge`], by lane.
const LANE_JUDGES: [&str; 8] = [
    "judge.lane.0",
    "judge.lane.1",
    "judge.lane.2",
    "judge.lane.3",
    "judge.lane.4",
    "judge.lane.5",
    "judge.lane.6",
    "judge.lane.7",
];

impl IndexProp {
    /// The integer selector in the model: `None` for an unsupported one.
    /// The gauge's type compares `gauge.kind` with every variant, never
    /// casting it, and hides while the gauge is invalid.
    pub(crate) fn selector(self) -> Option<&'static str> {
        Some(match self {
            Self::Null => "0",
            Self::GaugeKind => {
                "if gauge.valid then (if gauge.kind == assist_easy then 0 \
                 else if gauge.kind == easy then 1 else if gauge.kind == normal then 2 \
                 else if gauge.kind == hard then 3 else if gauge.kind == ex_hard then 4 \
                 else if gauge.kind == hazard then 5 else -1) else -1"
            }
            Self::CoverEnabled => "if cover.enabled then 1 else 0",
            Self::LiftEnabled => "if lift.enabled then 1 else 0",
            Self::HiddenEnabled => "if hidden.enabled then 1 else 0",
            Self::RandomOption => "0",
            Self::LaneJudge(n) => LANE_JUDGES.get(usize::from(n))?,
            Self::Selection(src) => src,
            Self::Unsupported(_) | Self::Unavailable(_) => return None,
        })
    }

    /// Whether the selector differs from the source in an owner-decided
    /// way (D4): the gauge's type projected onto the six looks, and the
    /// switches read from the play rather than the configuration.
    pub(crate) fn deviates(self) -> bool {
        !matches!(
            self,
            Self::Null
                | Self::RandomOption
                | Self::LaneJudge(_)
                | Self::Unsupported(_)
                | Self::Selection(_)
                | Self::Unavailable(_)
        )
    }
}

/// Image-index property `id` on a `kind` screen: play's mapping, or the
/// selection or result profile's ([`selection::profile::image_index`],
/// [`super::result::profile::image_index`]), an id
/// outside it unsupported where the factory knows it and selecting entry
/// 0 where it does not.
pub(crate) fn index_in(kind: ScreenKind, id: i32) -> IndexProp {
    let class = match kind {
        ScreenKind::Selection => selection::profile::image_index(id),
        ScreenKind::Result => result::profile::image_index(id),
        ScreenKind::Play | ScreenKind::Decision => return index_by_id(id),
    };
    match class {
        Some(Class::Bridge(src)) => IndexProp::Selection(src),
        Some(Class::Unavailable) => IndexProp::Unavailable(id),
        Some(Class::Omit) => IndexProp::Unsupported(id),
        None => match index_by_id(id) {
            IndexProp::Null => IndexProp::Null,
            _ => IndexProp::Unsupported(id),
        },
    }
}

/// The image-index ids the factory knows besides the supported ones
/// (`IntegerPropertyFactory.java:920-961, 1088-1383`): the `IndexType`
/// entries, the ranking patterns 380-399, the skin-selection ids
/// (`SkinPropertyMapper.java:101-104`) and the judge readings 508-519 and
/// 1510-1699 (`SkinProperty.java:1054-1067`).
const INDEX_KNOWN: &[(i32, i32)] = &[
    (10, 12),
    (43, 43),
    (54, 55),
    (61, 63),
    (72, 72),
    (75, 75),
    (78, 78),
    (89, 90),
    (170, 185),
    (301, 308),
    (321, 324),
    (340, 343),
    (350, 353),
    (360, 361),
    (370, 371),
    (380, 399),
    (400, 400),
    (450, 466),
    (469, 469),
    (508, 519),
    (1_510, 1_699),
];

/// The image-index property `id`.
pub(crate) fn index_by_id(id: i32) -> IndexProp {
    match id {
        40 => IndexProp::GaugeKind,
        42 => IndexProp::RandomOption,
        330 => IndexProp::CoverEnabled,
        331 => IndexProp::LiftEnabled,
        332 => IndexProp::HiddenEnabled,
        500..=507 => IndexProp::LaneJudge(u8::try_from(id - 500).unwrap_or(0)),
        _ if INDEX_KNOWN.iter().any(|&(a, b)| (a..=b).contains(&id)) => IndexProp::Unsupported(id),
        _ => IndexProp::Null,
    }
}

/// A string property as the factory resolves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrProp {
    Null,
    /// A chart header field, by id 10–16.
    Field(i32),
    /// A selection screen's own string.
    Screen(SelectionText),
    Unsupported(Option<i32>),
}

/// String property `id` on a `kind` screen: play's mapping, or the
/// selection or result profile's ([`selection::profile::text`],
/// [`super::result::profile::text`]), an id outside it
/// as the factory knows it ([`str_by_id`]).
pub(crate) fn str_in(kind: ScreenKind, id: i32) -> StrProp {
    if kind == ScreenKind::Result {
        return match result::profile::text(id) {
            Some(Class::Bridge(())) => str_by_id(id),
            Some(Class::Unavailable | Class::Omit) => StrProp::Unsupported(Some(id)),
            None => match str_by_id(id) {
                StrProp::Field(_) => StrProp::Unsupported(Some(id)),
                p => p,
            },
        };
    }
    if kind != ScreenKind::Selection {
        return str_by_id(id);
    }
    match selection::profile::text(id) {
        Some(Class::Bridge(SelText::Field(_))) => str_by_id(id),
        Some(Class::Bridge(SelText::Screen(s))) => StrProp::Screen(s),
        Some(Class::Unavailable | Class::Omit) => StrProp::Unsupported(Some(id)),
        None => str_by_id(id),
    }
}

/// String property `name` on a `kind` screen ([`str_by_name`] through
/// [`str_in`]).
pub(crate) fn str_by_name_in(kind: ScreenKind, name: &str) -> Option<StrProp> {
    match STRING_TYPES.iter().find(|(_, n)| *n == name) {
        Some(&(id, _)) => Some(str_in(kind, id)),
        None => str_by_name(name),
    }
}

/// `getStringProperty(int)`.
pub(crate) fn str_by_id(id: i32) -> StrProp {
    if (10..=16).contains(&id) {
        return StrProp::Field(id);
    }
    let known = STRING_TYPES.binary_search_by_key(&id, |&(i, _)| i).is_ok()
        || STRING_PATTERNS
            .iter()
            .any(|&(id0, count, ..)| (id0..id0 + count).contains(&id));
    if known {
        StrProp::Unsupported(Some(id))
    } else {
        StrProp::Null
    }
}

/// `getStringProperty(String)`: `None` for an unknown name, a script.
pub(crate) fn str_by_name(name: &str) -> Option<StrProp> {
    if let Some(&(id, _)) = STRING_TYPES.iter().find(|(_, n)| *n == name) {
        return Some(str_by_id(id));
    }
    STRING_PATTERNS
        .iter()
        .any(|&(_, count, prefix, suffix, first)| {
            numbered(name, prefix, suffix, first, count).is_some()
        })
        .then_some(StrProp::Unsupported(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_pinned() {
        assert_eq!(VALUE_TYPES.len(), 148);
        assert_eq!(STRING_TYPES.len(), 26);
        assert!(VALUE_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(STRING_TYPES.windows(2).all(|w| w[0].0 < w[1].0));
        for (id, name) in [
            (71, "score"),
            (75, "maxcombo"),
            (101, "score2"),
            (420, "miss"),
        ] {
            assert!(VALUE_TYPES.contains(&(id, name)));
        }
        assert!(STRING_TYPES.contains(&(12, "fulltitle")));
    }

    #[test]
    fn ids_and_names_resolve() {
        assert_eq!(int_by_id(71), IntProp::Shown(Value::ExScore));
        assert_eq!(int_by_id(100), IntProp::Shown(Value::Point));
        assert_eq!(int_by_name("point"), Some(IntProp::Shown(Value::Point)));
        assert_eq!(
            Value::Point.range(),
            (i64::from(i32::MIN), i64::from(i32::MAX))
        );
        assert!(!Value::Point.deviates() && Value::Point.part().is_none());
        assert_eq!(Value::Point.availability(), None);
        assert_eq!(int_by_id(112), IntProp::Shown(Value::Count(2)));
        assert_eq!(int_by_id(9_999), IntProp::Null);
        assert_eq!(int_by_id(0), IntProp::Null);
        assert_eq!(int_by_id(150), IntProp::Unsupported(Some(150)));
        for (id, name, value) in [
            (102, "score_rate", Value::CurrentRatePercent),
            (103, "score_rate_afterdot", Value::CurrentRateFraction),
            (115, "total_rate", Value::WholeRatePercent),
            (116, "total_rate_afterdot", Value::WholeRateFraction),
            (155, "score_rate2", Value::WholeRatePercent),
            (156, "score_rate_afterdot2", Value::WholeRateFraction),
        ] {
            assert_eq!(int_by_id(id), IntProp::Shown(value), "{id}");
            assert_eq!(int_by_name(name), Some(IntProp::Shown(value)), "{name}");
            assert!(value.deviates() && value.part().is_none());
        }
        // The readouts, each guarded while it can be unknown.
        for (id, name, value, guard) in [
            (90, "maxbpm", Value::BpmMax, Some("bpm.range_valid")),
            (91, "minbpm", Value::BpmMin, Some("bpm.range_valid")),
            (160, "nowbpm", Value::BpmNow, Some("bpm.current_valid")),
            (161, "playtime_minute", Value::ElapsedMinutes, None),
            (162, "playtime_second", Value::ElapsedSeconds, None),
            (163, "timeleft_minute", Value::RemainingMinutes, None),
            (164, "timeleft_second", Value::RemainingSeconds, None),
            (14, "lanecover1", Value::CoverPermille, None),
            (314, "lift1", Value::LiftPermille, None),
            (315, "hidden1", Value::HiddenPermille, None),
            (316, "lanecover2", Value::CoverAbsolutePermille, None),
            (10, "hispeed_lr2", Value::HispeedX100, Some("hispeed.valid")),
            (310, "hispeed", Value::HispeedWhole, Some("hispeed.valid")),
            (
                311,
                "hispeed_afterdot",
                Value::HispeedFraction,
                Some("hispeed.valid"),
            ),
            (312, "duration", Value::Duration, Some("duration.valid")),
            (
                313,
                "duration_green",
                Value::DurationGreen,
                Some("duration.valid"),
            ),
        ] {
            assert_eq!(int_by_id(id), IntProp::Shown(value), "{id}");
            assert_eq!(int_by_name(name), Some(IntProp::Shown(value)), "{name}");
            assert_eq!(value.availability(), guard, "{name}");
            assert!(value.deviates() && value.part().is_none());
            let scope = skin_model::Scope::new(&skin_model::PLAY);
            assert!(skin_model::compile(value.expr(), &scope, skin_model::Ty::Int).is_ok());
            if let Some(g) = guard {
                assert!(skin_model::compile(g, &scope, skin_model::Ty::Bool).is_ok());
            }
        }
        // The early and late counts and their totals, counted as the
        // source counts them: no D4 note.
        let early_late = [
            (410, None, Value::Early(0), "count.early.perfect"),
            (411, None, Value::Late(0), "count.late.perfect"),
            (412, None, Value::Early(1), "count.early.great"),
            (413, None, Value::Late(1), "count.late.great"),
            (414, None, Value::Early(2), "count.early.good"),
            (415, None, Value::Late(2), "count.late.good"),
            (416, None, Value::Early(3), "count.early.bad"),
            (417, None, Value::Late(3), "count.late.bad"),
            (418, None, Value::Early(4), "count.early.poor"),
            (419, None, Value::Late(4), "count.late.poor"),
            (420, Some("miss"), Value::Count(5), "count.empty_poor"),
            (
                421,
                Some("early_miss"),
                Value::Early(5),
                "count.early.empty_poor",
            ),
            (
                422,
                Some("late_miss"),
                Value::Late(5),
                "count.late.empty_poor",
            ),
            (423, Some("totalearly"), Value::Fast, "count.fast"),
            (424, Some("totallate"), Value::Slow, "count.slow"),
        ];
        let scope = skin_model::Scope::new(&skin_model::PLAY);
        for (id, name, value, expr) in early_late {
            assert_eq!(int_by_id(id), IntProp::Shown(value), "{id}");
            if let Some(name) = name {
                assert_eq!(int_by_name(name), Some(IntProp::Shown(value)), "{name}");
            }
            assert_eq!((value.expr(), value.availability()), (expr, None));
            assert!(!value.deviates() && value.part().is_none(), "{id}");
            assert!(skin_model::compile(expr, &scope, skin_model::Ty::Int).is_ok());
        }
        // The recent timing, 1P only: the other players' stay unsupported.
        let recent = Value::RecentTiming;
        assert_eq!(int_by_id(525), IntProp::Shown(recent));
        assert_eq!(int_by_name("judge_duration1"), Some(IntProp::Shown(recent)));
        assert_eq!(
            (recent.expr(), recent.availability()),
            ("judge.recent_ms", None)
        );
        assert!(!recent.deviates() && recent.part().is_none());
        assert_eq!(
            recent.range(),
            (-i64::from(i32::MAX - 1), i64::from(i32::MAX - 1))
        );
        for id in [526, 527] {
            assert_eq!(int_by_id(id), IntProp::Unsupported(Some(id)), "{id}");
        }
        // Neither is a value property; others keep needing their own data.
        assert_eq!(int_by_id(15), IntProp::Null);
        assert_eq!(int_by_id(131), IntProp::Null);
        for id in [92, 1_163, 1_164, 372, 373] {
            assert_eq!(int_by_id(id), IntProp::Unsupported(Some(id)), "{id}");
        }
        // 1312 to 1327: each variant's ms and green, under its validity,
        // by numeric id only.
        for (n, (t, k)) in ["current", "main", "min", "max"]
            .into_iter()
            .flat_map(|t| ["covered", "uncovered"].map(|k| (t, k)))
            .enumerate()
        {
            for (part, v) in ["ms", "green"].into_iter().enumerate() {
                let id = 1_312 + 2 * i32::try_from(n).unwrap() + i32::try_from(part).unwrap();
                let value = Value::DurationVariant(u8::try_from(id - 1_312).unwrap());
                assert_eq!(int_by_id(id), IntProp::Shown(value), "{id}");
                let (expr, valid) = (
                    format!("duration.{t}.{k}.{v}"),
                    format!("duration.{t}.{k}.valid"),
                );
                assert_eq!(value.expr(), expr, "{id}");
                assert_eq!(value.availability(), Some(valid.as_str()), "{id}");
                assert!(value.deviates() && value.part().is_none(), "{id}");
                assert!(skin_model::compile(&expr, &scope, skin_model::Ty::Int).is_ok());
                assert!(skin_model::compile(&valid, &scope, skin_model::Ty::Bool).is_ok());
            }
        }
        assert_eq!(int_by_id(1_311), IntProp::Null);
        assert_eq!(int_by_id(1_328), IntProp::Null);

        // The earlier values are never guarded.
        for v in [
            Value::ExScore,
            Value::Notes,
            Value::GaugePercent,
            Value::Count(0),
        ] {
            assert_eq!(v.availability(), None);
        }
        assert!(!Value::GaugePercent.deviates() && !Value::Count(5).deviates());
        // 40 is no value property; as an image index it is the gauge type.
        assert_eq!(int_by_id(40), IntProp::Null);
        assert_eq!(int_by_name("gaugetype_1p"), None);
        assert_eq!(index_by_id(40), IndexProp::GaugeKind);
        assert_eq!(index_by_id(42), IndexProp::RandomOption);
        assert_eq!(index_by_id(43), IndexProp::Unsupported(43));
        // The 1P lanes' visual states, 500 the scratch to 507 key 7, read
        // from each lane's field and keeping the source; 508, 509 and the
        // other players' stay unsupported, and none is a value property.
        let scope = skin_model::Scope::new(&skin_model::PLAY);
        for n in 0..8_u8 {
            let id = 500 + i32::from(n);
            let p = index_by_id(id);
            assert_eq!(p, IndexProp::LaneJudge(n));
            let src = p.selector().unwrap();
            assert_eq!(src, format!("judge.lane.{n}"));
            assert!(!p.deviates());
            assert!(skin_model::compile(src, &scope, skin_model::Ty::Int).is_ok());
            assert_eq!(int_by_id(id), IntProp::Null);
        }
        for id in [508, 509, 510, 517, 519] {
            assert_eq!(index_by_id(id), IndexProp::Unsupported(id), "{id}");
        }
        assert_eq!(
            int_by_name("maxcombo2"),
            Some(IntProp::Shown(Value::MaxCombo))
        );
        assert_eq!(
            int_by_name("ranking_index10"),
            Some(IntProp::Unsupported(None))
        );
        assert_eq!(int_by_name("nope"), None);
        assert_eq!(str_by_id(13), StrProp::Field(13));
        assert_eq!(str_by_id(1_000), StrProp::Unsupported(Some(1_000)));
        assert_eq!(str_by_id(17), StrProp::Null);
        assert_eq!(str_by_name("title"), Some(StrProp::Field(10)));
        assert_eq!(str_by_name("key54"), Some(StrProp::Unsupported(None)));
        assert_eq!(str_by_name("key55"), None);
        assert_eq!(str_by_name("key0"), None);
        assert_eq!(
            str_by_name("targetnamep10"),
            Some(StrProp::Unsupported(None))
        );
    }

    #[test]
    fn names_resolve_exactly_as_the_source_s_factories_resolve_them() {
        // The integer patterns' names by equality
        // (`IntegerPropertyFactory.java:1029-1037`, `:1072-1078`): any
        // other spelling of a rank is a script.
        for name in [
            "ranking_exscore1",
            "ranking_exscore10",
            "ranking_index1",
            "ranking_index10",
            "ir_player_lightassist_rate_afterdot",
            "folder_prefect",
        ] {
            assert_eq!(
                int_by_name(name),
                Some(IntProp::Unsupported(None)),
                "{name}"
            );
        }
        for name in [
            "ranking_exscore+1",
            "ranking_exscore01",
            "ranking_exscore0",
            "ranking_exscore11",
            "ranking_index010",
            "ranking_index-1",
            "ir_player_assist_rated",
        ] {
            assert_eq!(int_by_name(name), None, "{name}");
        }
        // The string patterns parse their number as `Integer.parseInt`
        // does (`StringPropertyFactory.java:57-63`), apart from digits
        // other than ASCII, which are left to scripts.
        assert_eq!(str_by_name("key+1"), Some(StrProp::Unsupported(None)));
        assert_eq!(str_by_name("key\u{ff11}"), None);
    }

    #[test]
    fn image_indices_resolve() {
        // Each known range's ends are known, one past them null.
        for &(a, b) in INDEX_KNOWN {
            assert_eq!(index_by_id(a), IndexProp::Unsupported(a));
            assert_eq!(index_by_id(b), IndexProp::Unsupported(b));
        }
        for id in [
            0, 1, 9, 13, 41, 44, 186, 300, 309, 329, 333, 467, 468, 520, 1_700,
        ] {
            assert_eq!(index_by_id(id), IndexProp::Null, "{id}");
        }
        assert!(INDEX_KNOWN.windows(2).all(|w| w[0].1 < w[1].0));
        // The whole of 450-466 is known (`IntegerPropertyFactory.java:1369`
        // among them).
        for id in 450..=466 {
            assert_eq!(index_by_id(id), IndexProp::Unsupported(id), "{id}");
        }
        let scope = skin_model::Scope::new(&skin_model::PLAY);
        let ops = |p: IndexProp| {
            let src = p.selector().unwrap();
            skin_model::compile(src, &scope, skin_model::Ty::Int)
                .unwrap()
                .op_count()
        };
        for (id, p, n) in [
            (40, IndexProp::GaugeKind, 34),
            (330, IndexProp::CoverEnabled, 4),
            (331, IndexProp::LiftEnabled, 4),
            (332, IndexProp::HiddenEnabled, 4),
        ] {
            assert_eq!(index_by_id(id), p);
            assert!(p.deviates());
            assert_eq!(ops(p), n, "{id}");
        }
        assert_eq!(ops(IndexProp::Null), 1);
        assert!(!IndexProp::Null.deviates());
        assert_eq!(IndexProp::Unsupported(42).selector(), None);
    }
}

#[cfg(test)]
mod range_tests {
    use super::{IntProp, Value, int_by_id};

    #[test]
    fn every_shown_value_reads_a_declared_field() {
        for id in 0..2_000 {
            if let IntProp::Shown(v) = int_by_id(id) {
                let field = v.expr().split(' ').next().unwrap_or("");
                assert!(
                    skin_model::PLAY.field(field).is_some(),
                    "{id}: {}",
                    v.expr()
                );
                let (lo, hi) = v.range();
                // The play level is signed, as `Integer.parseInt` is, the
                // recent timing, as a note time minus a press is, and the
                // point, whose wrapped terms reach all of `i32`.
                let least = match v {
                    Value::PlayLevel | Value::RecentTiming => -i64::from(i32::MAX),
                    Value::Point => i64::from(i32::MIN),
                    _ => 0,
                };
                assert!(lo >= least && lo <= hi && hi <= i64::from(i32::MAX), "{id}");
            }
        }
    }
}
