//! Draw conditions (§4.7): numeric ops and boolean properties.
//!
//! A numeric op names a built-in boolean property or a declared option
//! item (`SkinObject.java:282-299` at `0.8.9`): the built-in wins, a
//! negative id negates it (`BooleanPropertyFactory.java:40-68`). Declared
//! items fold to constants (`Skin.java:214-226`). An id that is neither
//! is never drawn, for either sign.

use std::collections::BTreeSet;

/// The ids `BooleanPropertyFactory.getBooleanProperty(int)` knows, with
/// their names (`BooleanPropertyFactory.java:349-~720`, the `BooleanType`
/// entries), sorted by id. The practice-item patterns
/// ([`PRACTICE_ITEM`], [`PRACTICE_ITEM_SELECTED`]) are apart.
pub(crate) const KNOWN_BOOLEAN_IDS: &[(i32, &str)] = &[
    (1, "select_folderbar"),
    (2, "select_somgbar"),
    (3, "select_coursebar"),
    (5, "playablebar"),
    (21, "select_panel1"),
    (22, "select_panel2"),
    (23, "select_panel3"),
    (32, "autoplay_off"),
    (33, "autoplay_on"),
    (42, "gauge_groove"),
    (43, "gauge_hard"),
    (50, "ir_offline"),
    (51, "ir_online"),
    (60, "disable_save_score"),
    (61, "enable_save_score"),
    (62, "no_save_clear"),
    (80, "now_loading"),
    (81, "loaded"),
    (82, "replay_off"),
    (84, "replay_playing"),
    (90, "result_clear"),
    (91, "result_fail"),
    (100, "select_bar_not_played"),
    (101, "select_bar_failed"),
    (102, "select_bar_easy"),
    (103, "select_bar_normal"),
    (104, "select_bar_hard"),
    (105, "select_bar_fullcombo"),
    (118, "trophy_gauge_normal"),
    (119, "trophy_gauge_hard"),
    (121, "trophy_gauge_easy"),
    (125, "trophy_gauge_exhard"),
    (126, "trophy_option_normal"),
    (127, "trophy_option_mirror"),
    (128, "trophy_option_random"),
    (129, "trophy_option_srandom"),
    (130, "trophy_option_hrandom"),
    (131, "trophy_option_allscr"),
    (150, "chart_difficulty_0"),
    (151, "chart_difficulty_1"),
    (152, "chart_difficulty_2"),
    (153, "chart_difficulty_3"),
    (154, "chart_difficulty_4"),
    (155, "chart_difficulty_5"),
    (160, "chart_7key"),
    (161, "chart_5key"),
    (162, "chart_14key"),
    (163, "chart_10key"),
    (164, "chart_9key"),
    (170, "song_no_bga"),
    (171, "song_bga"),
    (172, "chart_no_ln"),
    (173, "chart_ln"),
    (174, "song_no_text"),
    (175, "song_text"),
    (176, "chart_no_bpmchange"),
    (177, "chart_bpmchange"),
    (178, "chart_no_randomsequence"),
    (179, "chart_randomsequence"),
    (180, "chart_judge_veryhard"),
    (181, "chart_judge_hard"),
    (182, "chart_judge_normal"),
    (183, "chart_judge_easy"),
    (184, "chart_judge_veryeasy"),
    (190, "no_stagefile"),
    (191, "stagefile"),
    (192, "no_banner"),
    (193, "banner"),
    (194, "no_backbmp"),
    (195, "backbmp"),
    (196, "replaydata_no_1"),
    (197, "replaydata_exist_1"),
    (198, "replaydata_saved_1"),
    (200, "rank_1p_aaa"),
    (201, "rank_1p_aa"),
    (202, "rank_1p_a"),
    (203, "rank_1p_b"),
    (204, "rank_1p_c"),
    (205, "rank_1p_d"),
    (206, "rank_1p_e"),
    (207, "rank_1p_f"),
    (220, "rank_aaa"),
    (221, "rank_aa"),
    (222, "rank_a"),
    (223, "rank_b"),
    (224, "rank_c"),
    (225, "rank_d"),
    (226, "rank_e"),
    (227, "rank_f"),
    (230, "gauge_1p_0_10"),
    (231, "gauge_1p_10_20"),
    (232, "gauge_1p_20_30"),
    (233, "gauge_1p_30_40"),
    (234, "gauge_1p_40_50"),
    (235, "gauge_1p_50_60"),
    (236, "gauge_1p_60_70"),
    (237, "gauge_1p_70_80"),
    (238, "gauge_1p_80_90"),
    (239, "gauge_1p_90_100"),
    (240, "gauge_1p_100"),
    (241, "judge_1p_perfect"),
    (261, "judge_2p_perfect"),
    (270, "lanecover1_changing"),
    (271, "lanecover1_on"),
    (272, "lift1_on"),
    (273, "hidden1_on"),
    (280, "course_stage1"),
    (281, "course_stage2"),
    (282, "course_stage3"),
    (283, "course_stage4"),
    (289, "course_stage_final"),
    (290, "mode_course"),
    (300, "rank_result_1p_aaa"),
    (301, "rank_result_1p_aa"),
    (302, "rank_result_1p_a"),
    (303, "rank_result_1p_b"),
    (304, "rank_result_1p_c"),
    (305, "rank_result_1p_d"),
    (306, "rank_result_1p_e"),
    (307, "rank_result_1p_f"),
    (320, "rank_best_1p_aaa"),
    (321, "rank_best_1p_aa"),
    (322, "rank_best_1p_a"),
    (323, "rank_best_1p_b"),
    (324, "rank_best_1p_c"),
    (325, "rank_best_1p_d"),
    (326, "rank_best_1p_e"),
    (327, "rank_best_1p_f"),
    (330, "update_score"),
    (331, "update_maxcombo"),
    (332, "update_misscount"),
    (335, "update_scorerank"),
    (336, "update_target"),
    (340, "rank_now_1p_aaa"),
    (341, "rank_now_1p_aa"),
    (342, "rank_now_1p_a"),
    (343, "rank_now_1p_b"),
    (344, "rank_now_1p_c"),
    (345, "rank_now_1p_d"),
    (346, "rank_now_1p_e"),
    (347, "rank_now_1p_f"),
    (352, "result_1pwin"),
    (353, "result_2pwin"),
    (354, "result_draw"),
    (361, "judge_3p_perfect"),
    (400, "constant"),
    (603, "ir_no_player"),
    (604, "ir_failed"),
    (606, "ir_waiting"),
    (608, "ir_busy"),
    (624, "not_compare_rival"),
    (625, "compare_rival"),
    (1002, "course_class"),
    (1003, "course_mirror"),
    (1004, "course_random"),
    (1005, "course_nospeed"),
    (1006, "course_nogood"),
    (1007, "course_nogreat"),
    (1008, "table_song"),
    (1010, "course_gauge_lr2"),
    (1011, "course_gauge_5keys"),
    (1012, "course_gauge_7keys"),
    (1013, "course_gauge_9keys"),
    (1014, "course_gauge_24keys"),
    (1015, "course_ln"),
    (1016, "course_cn"),
    (1017, "course_hcn"),
    (1030, "randomselectbar"),
    (1031, "randomcoursebar"),
    (1046, "gauge_ex"),
    (1080, "state_practice"),
    (1100, "select_bar_assist_easy"),
    (1101, "select_bar_light_assist_easy"),
    (1102, "select_bar_exhard"),
    (1103, "select_bar_perfect"),
    (1104, "select_bar_max"),
    (1128, "trophy_option_rrandom"),
    (1129, "trophy_option_spiral"),
    (1130, "trophy_option_exrandom"),
    (1131, "trophy_option_exsrandom"),
    (1160, "chart_24key"),
    (1161, "chart_48key"),
    (1177, "chart_bpmstop"),
    (1196, "replaydata_no_2"),
    (1197, "replaydata_exist_2"),
    (1198, "replaydata_saved_2"),
    (1199, "replaydata_no_3"),
    (1200, "replaydata_exist_3"),
    (1201, "replaydata_saved_3"),
    (1202, "replaydata_no_4"),
    (1203, "replaydata_exist_4"),
    (1204, "replaydata_saved_4"),
    (1205, "select_replaydata_1"),
    (1206, "select_replaydata_2"),
    (1207, "select_replaydata_3"),
    (1208, "select_replaydata_4"),
    (1240, "border_or_more_1p"),
    (1242, "judge_1p_early"),
    (1243, "judge_1p_late"),
    (1262, "judge_2p_early"),
    (1263, "judge_2p_late"),
    (1330, "draw_score"),
    (1331, "draw_maxcombo"),
    (1332, "draw_misscount"),
    (1335, "draw_scorerank"),
    (1336, "draw_target"),
    (1362, "judge_3p_early"),
    (1363, "judge_3p_late"),
    (2241, "judge_perfect_exist"),
    (2242, "judge_great_exist"),
    (2243, "judge_good_exist"),
    (2244, "judge_bad_exist"),
    (2245, "judge_poor_exist"),
    (2246, "judge_miss_exist"),
];

/// `practice_item1`..`16` (`BooleanPropertyFactory.java:114-160`).
pub(crate) const PRACTICE_ITEM: i32 = 3_000;
/// `practice_item1_selected`..`16`.
pub(crate) const PRACTICE_ITEM_SELECTED: i32 = 3_020;
const PRACTICE_ITEMS: i32 = 16;

/// The first of the eleven gauge-range conditions, `gauge_1p_0_10`; the
/// last, 240, is a full gauge (`BooleanPropertyFactory.java:300-321`).
const GAUGE_RANGE: i32 = 230;
/// `judge_1p_perfect`: the latest judgment was a PERFECT
/// (`BooleanPropertyFactory.java:234-247`).
const JUDGE_PERFECT: i32 = 241;

/// A condition as the importer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cond {
    /// A model expression (a `bool`).
    Expr(String),
    /// A built-in the model has no value for.
    Unsupported(i32),
    /// A declared option item, folded: whether it holds.
    Fold(bool),
    /// Neither built in nor declared: never drawn.
    Undefined(i32),
}

/// The model expression of supported built-in `id` (positive).
fn builtin(id: i32) -> Option<String> {
    match id {
        GAUGE_RANGE..=240 => {
            let k = id - GAUGE_RANGE;
            Some(format!("gauge >= {} && gauge < {}", 100 * k, 100 * (k + 1)))
        }
        JUDGE_PERFECT => Some("on(judge.perfect)".to_owned()),
        _ => None,
    }
}

/// Whether `id` (positive) is a built-in boolean property.
pub(crate) fn is_builtin(id: i32) -> bool {
    KNOWN_BOOLEAN_IDS
        .binary_search_by_key(&id, |&(i, _)| i)
        .is_ok()
        || (PRACTICE_ITEM..PRACTICE_ITEM + PRACTICE_ITEMS).contains(&id)
        || (PRACTICE_ITEM_SELECTED..PRACTICE_ITEM_SELECTED + PRACTICE_ITEMS).contains(&id)
}

/// A built-in, negated for `negative`.
fn built_in_cond(id: i32, negative: bool) -> Cond {
    match builtin(id) {
        Some(e) if negative => Cond::Expr(format!("!({e})")),
        Some(e) => Cond::Expr(e),
        None => Cond::Unsupported(if negative { -id } else { id }),
    }
}

/// Numeric op `op` (non-zero): built-ins first, then the declared items
/// `declared`, folded against the selected `enabled`.
pub(crate) fn numeric(op: i32, declared: &BTreeSet<i32>, enabled: &BTreeSet<i32>) -> Cond {
    let Ok(id) = i32::try_from(op.unsigned_abs()) else {
        return Cond::Undefined(op);
    };
    if is_builtin(id) {
        return built_in_cond(id, op < 0);
    }
    if declared.contains(&id) {
        return Cond::Fold(enabled.contains(&id) == (op > 0));
    }
    Cond::Undefined(op)
}

/// `s` as Java's `Integer.parseInt` reads it: a sign, then ASCII digits.
pub(crate) fn java_int(s: &str) -> Option<i32> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// The id of boolean property `name`, without `!` prefixes.
fn named_id(name: &str) -> Option<i32> {
    if let Some(&(id, _)) = KNOWN_BOOLEAN_IDS.iter().find(|(_, n)| *n == name) {
        return Some(id);
    }
    let rest = name.strip_prefix("practice_item")?;
    let (digits, first) = match rest.strip_suffix("_selected") {
        Some(d) => (d, PRACTICE_ITEM_SELECTED),
        None => (rest, PRACTICE_ITEM),
    };
    let n = java_int(digits)?;
    (1..=PRACTICE_ITEMS).contains(&n).then(|| first + n - 1)
}

/// A boolean property named `name` (`getBooleanProperty(String)`,
/// `BooleanPropertyFactory.java:69-85`): each leading `!` negates; a name
/// the factory does not know is a script, `None`.
pub(crate) fn named(name: &str) -> Option<Cond> {
    let mut negative = false;
    let mut rest = name;
    while let Some(r) = rest.strip_prefix('!') {
        negative = !negative;
        rest = r;
    }
    let id = named_id(rest)?;
    Some(built_in_cond(id, negative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_table_is_pinned() {
        assert_eq!(KNOWN_BOOLEAN_IDS.len(), 214);
        assert!(KNOWN_BOOLEAN_IDS.windows(2).all(|w| w[0].0 < w[1].0));
        for (id, name) in [
            (230, "gauge_1p_0_10"),
            (240, "gauge_1p_100"),
            (241, "judge_1p_perfect"),
        ] {
            assert!(KNOWN_BOOLEAN_IDS.contains(&(id, name)));
        }
        assert!(is_builtin(3_015) && is_builtin(3_035));
        assert!(!is_builtin(3_016) && !is_builtin(3_036));
    }

    #[test]
    fn names_negate_and_patterns_parse() {
        assert_eq!(
            named("!!judge_1p_perfect"),
            Some(Cond::Expr("on(judge.perfect)".into()))
        );
        assert_eq!(
            named("!practice_item+2_selected"),
            Some(Cond::Unsupported(-3_021))
        );
        assert_eq!(named("practice_item17"), None);
        assert_eq!(named("myscript"), None);
    }

    #[test]
    fn built_ins_win_over_declared_items() {
        let declared = BTreeSet::from([241, 900]);
        let on = BTreeSet::from([900]);
        assert_eq!(
            numeric(-241, &declared, &on),
            Cond::Expr("!(on(judge.perfect))".into())
        );
        assert_eq!(numeric(900, &declared, &on), Cond::Fold(true));
        assert_eq!(numeric(-900, &declared, &on), Cond::Fold(false));
        assert_eq!(numeric(901, &declared, &on), Cond::Undefined(901));
        assert_eq!(numeric(202, &declared, &on), Cond::Unsupported(202));
    }
}
