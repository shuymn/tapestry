//! Effective number and text properties (§4.1): what the source's
//! property factories return for an id or a name, and which of those the
//! model shows.

use super::ops::java_int;

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
    /// The gauge's whole percent.
    GaugePercent,
    /// The gauge's tenths of a percent (D4 below 0.1 %).
    GaugeTenths,
    /// A judgment count: `count.perfect`..`count.poor`, then
    /// `count.empty_poor`.
    Count(u8),
}

impl Value {
    /// The model expression.
    pub(crate) fn expr(self) -> &'static str {
        match self {
            Self::ExScore => "ex_score",
            Self::MaxCombo => "max_combo",
            Self::Notes => "notes",
            Self::GaugePercent => "gauge / 10",
            Self::GaugeTenths => "gauge % 10",
            Self::Count(0) => "count.perfect",
            Self::Count(1) => "count.great",
            Self::Count(2) => "count.good",
            Self::Count(3) => "count.bad",
            Self::Count(4) => "count.poor",
            Self::Count(_) => "count.empty_poor",
        }
    }

    /// Whether the value differs from the source in an owner-decided way.
    pub(crate) fn deviates(self) -> bool {
        matches!(self, Self::Notes | Self::GaugeTenths)
    }

    /// The play-critical values (owner decision 5).
    pub(crate) fn critical(self) -> bool {
        matches!(self, Self::ExScore | Self::MaxCombo)
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
}

/// The model value of known integer property `id` (§4.9).
fn shown(id: i32) -> Option<Value> {
    Some(match id {
        71 | 101 => Value::ExScore,
        75 | 105 => Value::MaxCombo,
        74 | 106 => Value::Notes,
        107 => Value::GaugePercent,
        407 => Value::GaugeTenths,
        110..=114 => Value::Count(u8::try_from(id - 110).ok()?),
        420 => Value::Count(5),
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
    let n = java_int(name.strip_prefix(prefix)?.strip_suffix(suffix)?)?;
    (first..first + count).contains(&n).then_some(n)
}

/// `getIntegerProperty(String)` (`IntegerPropertyFactory.java:51-62`):
/// `None` for a name the factory does not know, a script.
pub(crate) fn int_by_name(name: &str) -> Option<IntProp> {
    if let Some(&(id, _)) = VALUE_TYPES.iter().find(|(_, n)| *n == name) {
        return Some(int_by_id(id));
    }
    let pattern = numbered(name, "ranking_exscore", "", 1, 10).is_some()
        || numbered(name, "ranking_index", "", 1, 10).is_some()
        || FOLDER_NAMES.contains(&name)
        || ["", "_rate", "_rate_afterdot"].iter().any(|suffix| {
            IR_CLEARS
                .iter()
                .any(|c| name == format!("ir_player_{c}{suffix}"))
        });
    pattern.then_some(IntProp::Unsupported(None))
}

/// A string property as the factory resolves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrProp {
    Null,
    /// A chart header field, by id 10–16.
    Field(i32),
    Unsupported(Option<i32>),
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
        assert_eq!(int_by_id(112), IntProp::Shown(Value::Count(2)));
        assert_eq!(int_by_id(9_999), IntProp::Null);
        assert_eq!(int_by_id(0), IntProp::Null);
        assert_eq!(int_by_id(150), IntProp::Unsupported(Some(150)));
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
}
