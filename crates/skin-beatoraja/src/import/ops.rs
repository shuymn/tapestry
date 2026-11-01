//! Draw conditions: numeric ops and boolean properties.
//!
//! A numeric op names a built-in boolean property or a declared option
//! item (`SkinObject.java:282-299` at `0.8.9`): the built-in wins, a
//! negative id negates it (`BooleanPropertyFactory.java:40-68`). Declared
//! items fold to constants (`Skin.java:214-226`). An id that is neither
//! is never drawn, for either sign.

use std::collections::BTreeMap;

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
    (40, "bgaoff"),
    (41, "bgaon"),
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
/// The shown gauge's families (`BooleanPropertyFactory.java:394-414,
/// 693-704`): `gauge_groove`, the source's gauge types up to 2;
/// `gauge_hard`, from 3; `gauge_ex`, types 0, 1, 4, 5, 7 and 8. The model
/// reads the shown gauge's look (D4): its six looks stand for the source's
/// types 0-5, and a preset's other gauges show as normal or hard.
const GAUGE_GROOVE: i32 = 42;
const GAUGE_HARD: i32 = 43;
const GAUGE_EX: i32 = 1_046;
/// The cover switches (`BooleanPropertyFactory.java:565-572`):
/// `lanecover1_changing`, here the cover-adjustment mode where the source
/// tests START or SELECT held (D4); `lanecover1_on`, `lift1_on` and
/// `hidden1_on`, the stored switches.
const COVER_CHANGING: i32 = 270;
const COVER_ON: i32 = 271;
const LIFT_ON: i32 = 272;
const HIDDEN_ON: i32 = 273;

/// A condition as the importer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cond {
    /// A model expression (a `bool`), and whether what it tests differs
    /// from the source's in an owner-decided way (D4).
    Expr {
        src: String,
        display_deviation: bool,
    },
    /// A built-in the model has no value for.
    Unsupported(i32),
    /// A declared option item, folded: whether it holds.
    Fold(bool),
    /// Neither built in nor declared: never drawn.
    Undefined(i32),
}

/// The shown gauge's kind is one of `kinds`, while it has one.
fn gauge_kind(kinds: &[&str]) -> String {
    let mut src = String::from("gauge.valid && (");
    for (i, k) in kinds.iter().enumerate() {
        if i > 0 {
            src.push_str(" || ");
        }
        src.push_str("gauge.kind == ");
        src.push_str(k);
    }
    src.push(')');
    src
}

/// The model expression of supported built-in `id` (positive), and
/// whether it is D4.
fn builtin(id: i32) -> Option<(String, bool)> {
    match id {
        GAUGE_RANGE..=240 => {
            let k = id - GAUGE_RANGE;
            let src = format!("gauge >= {} && gauge < {}", 100 * k, 100 * (k + 1));
            Some((src, false))
        }
        JUDGE_PERFECT => Some(("on(judge.perfect)".to_owned(), false)),
        GAUGE_GROOVE => Some((gauge_kind(&["assist_easy", "easy", "normal"]), true)),
        GAUGE_HARD => Some((gauge_kind(&["hard", "ex_hard", "hazard"]), true)),
        GAUGE_EX => Some((
            gauge_kind(&["assist_easy", "easy", "ex_hard", "hazard"]),
            true,
        )),
        COVER_CHANGING => Some(("cover.adjusting".to_owned(), true)),
        COVER_ON => Some(("cover.enabled".to_owned(), false)),
        LIFT_ON => Some(("lift.enabled".to_owned(), false)),
        HIDDEN_ON => Some(("hidden.enabled".to_owned(), false)),
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

/// A built-in, negated for `negative`. Without a judge region
/// (`judge_region`) the latest judgment is never a PERFECT
/// (`JudgeManager.java:129, 686`).
fn built_in_cond(id: i32, negative: bool, judge_region: bool) -> Cond {
    if id == JUDGE_PERFECT && !judge_region {
        return Cond::Fold(negative);
    }
    match builtin(id) {
        Some((e, display_deviation)) => Cond::Expr {
            src: if negative { format!("!({e})") } else { e },
            display_deviation,
        },
        None => Cond::Unsupported(if negative { -id } else { id }),
    }
}

/// Numeric op `op` (non-zero): built-ins first, then the declared items
/// `items`, each folded to whether it is selected.
pub(crate) fn numeric(op: i32, items: &BTreeMap<i32, bool>, judge_region: bool) -> Cond {
    let Ok(id) = i32::try_from(op.unsigned_abs()) else {
        return Cond::Undefined(op);
    };
    if is_builtin(id) {
        return built_in_cond(id, op < 0, judge_region);
    }
    if let Some(&on) = items.get(&id) {
        return Cond::Fold(on == (op > 0));
    }
    Cond::Undefined(op)
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
    let n = digits.parse::<i32>().ok()?;
    (1..=PRACTICE_ITEMS).contains(&n).then(|| first + n - 1)
}

/// A boolean property named `name` (`getBooleanProperty(String)`,
/// `BooleanPropertyFactory.java:69-85`): each leading `!` negates; a name
/// the factory does not know is a script, `None`.
pub(crate) fn named(name: &str, judge_region: bool) -> Option<Cond> {
    let mut negative = false;
    let mut rest = name;
    while let Some(r) = rest.strip_prefix('!') {
        negative = !negative;
        rest = r;
    }
    let id = named_id(rest)?;
    Some(built_in_cond(id, negative, judge_region))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id of the factory's `BooleanType` entries at `0.8.9`, the
    /// `SkinProperty` constants resolved, written out apart from the
    /// table so an omission fails.
    const UPSTREAM_IDS: [i32; 216] = [
        1, 2, 3, 5, 21, 22, 23, 32, 33, 40, 41, 42, 43, 50, 51, 60, 61, 62, 80, 81, 82, 84, 90, 91,
        100, 101, 102, 103, 104, 105, 118, 119, 121, 125, 126, 127, 128, 129, 130, 131, 150, 151,
        152, 153, 154, 155, 160, 161, 162, 163, 164, 170, 171, 172, 173, 174, 175, 176, 177, 178,
        179, 180, 181, 182, 183, 184, 190, 191, 192, 193, 194, 195, 196, 197, 198, 200, 201, 202,
        203, 204, 205, 206, 207, 220, 221, 222, 223, 224, 225, 226, 227, 230, 231, 232, 233, 234,
        235, 236, 237, 238, 239, 240, 241, 261, 270, 271, 272, 273, 280, 281, 282, 283, 289, 290,
        300, 301, 302, 303, 304, 305, 306, 307, 320, 321, 322, 323, 324, 325, 326, 327, 330, 331,
        332, 335, 336, 340, 341, 342, 343, 344, 345, 346, 347, 352, 353, 354, 361, 400, 603, 604,
        606, 608, 624, 625, 1002, 1003, 1004, 1005, 1006, 1007, 1008, 1010, 1011, 1012, 1013, 1014,
        1015, 1016, 1017, 1030, 1031, 1046, 1080, 1100, 1101, 1102, 1103, 1104, 1128, 1129, 1130,
        1131, 1160, 1161, 1177, 1196, 1197, 1198, 1199, 1200, 1201, 1202, 1203, 1204, 1205, 1206,
        1207, 1208, 1240, 1242, 1243, 1262, 1263, 1330, 1331, 1332, 1335, 1336, 1362, 1363, 2241,
        2242, 2243, 2244, 2245, 2246,
    ];

    #[test]
    fn the_names_table_is_pinned() {
        let ids: Vec<i32> = KNOWN_BOOLEAN_IDS.iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, UPSTREAM_IDS);
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
    fn every_supported_condition_fits_one_source() {
        // The built-ins with an expression, each sign: the closed set a
        // destination's conditions come from, deduplicated.
        let mut exprs = Vec::new();
        for &(id, _) in KNOWN_BOOLEAN_IDS {
            if [GAUGE_GROOVE, GAUGE_HARD, GAUGE_EX].contains(&id) {
                continue;
            }
            for op in [id, -id] {
                if let Cond::Expr { src, .. } = numeric(op, &BTreeMap::new(), true) {
                    exprs.push(src);
                }
            }
        }
        assert_eq!(exprs.len(), 32);
        let joined: usize =
            exprs.iter().map(|e| e.len() + 2).sum::<usize>() + 4 * (exprs.len() - 1);
        assert_eq!(joined, 974);
        // A number's availability joins them: the longest still fits.
        let guard = [
            "bpm.range_valid",
            "bpm.current_valid",
            "hispeed.valid",
            "duration.valid",
        ]
        .iter()
        .map(|g| g.len() + 2 + 4)
        .max()
        .unwrap_or(0);
        assert!(joined + guard <= skin_model::MAX_SOURCE_BYTES);
    }

    fn expr(src: &str, display_deviation: bool) -> Cond {
        Cond::Expr {
            src: src.to_owned(),
            display_deviation,
        }
    }

    #[test]
    fn gauge_families_test_the_shown_kind() {
        let groove = "gauge.valid && (gauge.kind == assist_easy || gauge.kind == easy \
                      || gauge.kind == normal)";
        let hard = "gauge.valid && (gauge.kind == hard || gauge.kind == ex_hard \
                    || gauge.kind == hazard)";
        let ex = "gauge.valid && (gauge.kind == assist_easy || gauge.kind == easy \
                  || gauge.kind == ex_hard || gauge.kind == hazard)";
        let none = BTreeMap::new();
        assert_eq!(numeric(42, &none, true), expr(groove, true));
        assert_eq!(
            numeric(-42, &none, false),
            expr(&format!("!({groove})"), true)
        );
        assert_eq!(numeric(43, &none, true), expr(hard, true));
        assert_eq!(
            named("!gauge_hard", true),
            Some(expr(&format!("!({hard})"), true))
        );
        assert_eq!(named("!!gauge_ex", true), Some(expr(ex, true)));
        assert_eq!(
            numeric(-1_046, &none, true),
            expr(&format!("!({ex})"), true)
        );
        // The built-ins win over items declared with their numbers.
        let items = BTreeMap::from([(42, false), (1_046, true)]);
        assert_eq!(numeric(42, &items, true), expr(groove, true));
        assert_eq!(
            numeric(-1_046, &items, true),
            expr(&format!("!({ex})"), true)
        );
        // Every one compiles against the play schema.
        for op in [42, -42, 43, -43, 1_046, -1_046] {
            let Cond::Expr { src, .. } = numeric(op, &none, true) else {
                panic!("{op}");
            };
            let scope = skin_model::Scope::new(&skin_model::PLAY);
            assert!(skin_model::compile(&src, &scope, skin_model::Ty::Bool).is_ok());
        }
        // The rank conditions stay unsupported.
        for id in [200, 207, 220, 300, 320, 340, 347] {
            assert_eq!(numeric(id, &none, true), Cond::Unsupported(id));
        }
    }

    /// Every supported condition, each sign, joined as one destination's
    /// visibility: past the compiler's source limit.
    #[test]
    fn the_whole_union_passes_the_source_limit() {
        let mut exprs = Vec::new();
        for &(id, _) in KNOWN_BOOLEAN_IDS {
            for op in [id, -id] {
                if let Cond::Expr { src, .. } = numeric(op, &BTreeMap::new(), true) {
                    exprs.push(src);
                }
            }
        }
        assert_eq!(exprs.len(), 38);
        let joined: usize =
            exprs.iter().map(|e| e.len() + 2).sum::<usize>() + 4 * (exprs.len() - 1);
        assert!(joined > skin_model::MAX_SOURCE_BYTES, "{joined}");
    }

    #[test]
    fn cover_switches_read_the_stored_settings() {
        let none = BTreeMap::new();
        for (id, name, src, d4) in [
            (270, "lanecover1_changing", "cover.adjusting", true),
            (271, "lanecover1_on", "cover.enabled", false),
            (272, "lift1_on", "lift.enabled", false),
            (273, "hidden1_on", "hidden.enabled", false),
        ] {
            assert_eq!(numeric(id, &none, true), expr(src, d4));
            assert_eq!(numeric(-id, &none, false), expr(&format!("!({src})"), d4));
            assert_eq!(named(name, true), Some(expr(src, d4)));
            assert_eq!(
                named(&format!("!{name}"), true),
                Some(expr(&format!("!({src})"), d4))
            );
            // The built-ins win over items declared with their numbers.
            let items = BTreeMap::from([(id, false)]);
            assert_eq!(numeric(id, &items, true), expr(src, d4));
        }
    }

    #[test]
    fn names_negate_and_patterns_parse() {
        assert_eq!(
            named("!!judge_1p_perfect", true),
            Some(expr("on(judge.perfect)", false))
        );
        assert_eq!(
            named("!practice_item+2_selected", true),
            Some(Cond::Unsupported(-3_021))
        );
        assert_eq!(named("practice_item17", true), None);
        assert_eq!(named("myscript", true), None);
        // Without a judge region, PERFECT is never the latest judgment.
        assert_eq!(named("judge_1p_perfect", false), Some(Cond::Fold(false)));
        assert_eq!(named("!judge_1p_perfect", false), Some(Cond::Fold(true)));
        assert_eq!(numeric(241, &BTreeMap::new(), false), Cond::Fold(false));
        assert_eq!(numeric(-241, &BTreeMap::new(), false), Cond::Fold(true));
    }

    #[test]
    fn built_ins_win_over_declared_items() {
        let items = BTreeMap::from([(241, false), (900, true)]);
        assert_eq!(
            numeric(-241, &items, true),
            expr("!(on(judge.perfect))", false)
        );
        assert_eq!(numeric(900, &items, true), Cond::Fold(true));
        assert_eq!(numeric(-900, &items, true), Cond::Fold(false));
        assert_eq!(numeric(901, &items, true), Cond::Undefined(901));
        assert_eq!(numeric(202, &items, true), Cond::Unsupported(202));
        // The BGA built-ins win over items declared with their numbers.
        let items = BTreeMap::from([(40, true), (41, true)]);
        assert_eq!(numeric(40, &items, true), Cond::Unsupported(40));
        assert_eq!(numeric(-41, &items, true), Cond::Unsupported(-41));
        assert_eq!(named("!bgaon", true), Some(Cond::Unsupported(-41)));
    }
}
