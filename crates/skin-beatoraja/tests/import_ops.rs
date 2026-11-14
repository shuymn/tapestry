//! Ops and draw conditions: built-ins first, declared options
//! folded, undefined ids never drawn, names through the same table.

mod common;

use common::play::judged;
use common::{has, import_body};
use rules::{GaugeValue, Judgment};
use skin_beatoraja::{CriticalPart, ImportError, NoteKind, PropertyKind};

/// Options 900 (selected by default) and 901, and option 241.
const HEADER: &str = r#""property": [
    {"name": "p", "item": [{"name": "x", "op": 900}, {"name": "y", "op": 901}], "def": "x"},
    {"name": "q", "item": [{"name": "z", "op": 241}]}
]"#;

fn image(fields: &str) -> common::Loaded {
    import_body(&format!(
        r#"{HEADER}, "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{{"id": "i", "timer": 41, {fields}, "dst": [{{"w": 10, "h": 10}}]}}]"#
    ))
}

/// A critical EX score with `fields`.
fn critical(fields: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      "ref": 71}}],
           "destination": [{{"id": "v", "timer": 41, {fields}, "dst": [{{"w": 12, "h": 20}}]}}]"#
    ))
}

fn ignored(field: &'static str) -> NoteKind {
    NoteKind::Ignored { field }
}

/// Two options sharing item 900: the first selects it, the later one
/// selects 902. A destination's op sees every selection, as `if` does.
#[test]
fn ops_see_every_selected_item() {
    let shared = r#""property": [
        {"name": "a", "item": [{"name": "x", "op": 900}, {"name": "y", "op": 901}], "def": "x"},
        {"name": "b", "item": [{"name": "s", "op": 900}, {"name": "z", "op": 902}], "def": "z"}
    ]"#;
    let drawn = |dst: &str| {
        import_body(&format!(
            r#"{shared}, "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
               "destination": [{dst}]"#
        ))
        .quads(0)
        .len()
    };
    let key = r#""dst": [{"w": 10, "h": 10}]"#;
    assert_eq!(
        drawn(&format!(r#"{{"if": 900, "value": {{"id": "i", {key}}}}}"#)),
        1
    );
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [900], {key}}}"#)), 1);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [-900], {key}}}"#)), 0);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [902], {key}}}"#)), 1);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [901], {key}}}"#)), 0);
}

/// Two options that both declare and select 241: the op is still the
/// built-in perfect, which follows the latest judgment.
#[test]
fn a_built_in_wins_over_options_sharing_it() {
    let loaded = import_body(
        r#""property": [
            {"name": "a", "item": [{"name": "x", "op": 241}], "def": "x"},
            {"name": "b", "item": [{"name": "y", "op": 241}], "def": "y"}
        ], "image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
        "destination": [{"id": "i", "timer": 41, "op": [241], "dst": [{"w": 10, "h": 10}]}]"#,
    );
    let mut d = loaded.drawn();
    assert!(d.frame(0, &[]).is_empty());
    assert_eq!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).len(), 1);
    assert!(d.frame(20, &[judged(20, Judgment::Great, 2)]).is_empty());
}

#[test]
fn op_0_is_no_condition() {
    let loaded = image(r#""op": [0, 0]"#);
    assert!(loaded.report().notes.is_empty());
    assert_eq!(loaded.quads(0).len(), 1);
}

#[test]
fn undefined_ids_are_never_drawn() {
    for op in [7_777, -7_777] {
        let loaded = image(&format!(r#""op": [{op}]"#));
        assert!(has(loaded.report(), NoteKind::UndefinedOp(op)));
        assert_eq!(loaded.report().usage.images, 0);
        assert!(loaded.quads(0).is_empty());
        let e = critical(&format!(r#""op": [{op}]"#)).err();
        assert!(
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::ExScore,
                    why: NoteKind::UndefinedOp(o),
                    ..
                } if o == op
            ),
            "{e:?}"
        );
    }
}

#[test]
fn gauge_ranges_and_the_perfect_judgment() {
    // 20 % shows 232 (200 ≤ g < 300) and not its neighbours.
    for (op, shown) in [(231, false), (232, true), (233, false), (-232, false)] {
        let mut d = image(&format!(r#""op": [{op}]"#)).drawn();
        assert_eq!(d.frame(0, &[]).len(), usize::from(shown), "{op}");
    }
    let mut d = image(r#""op": [233]"#).drawn();
    d.gauges[0].value = GaugeValue(30 * GaugeValue::PERCENT - 1);
    assert!(d.frame(0, &[]).is_empty());
    d.gauges[0].value = GaugeValue(30 * GaugeValue::PERCENT);
    assert_eq!(d.frame(1, &[]).len(), 1);
    let mut d = image(r#""op": [240]"#).drawn();
    d.gauges[0].value = GaugeValue::FULL;
    assert_eq!(d.frame(0, &[]).len(), 1);
    let mut d = image(r#""op": [230]"#).drawn();
    d.gauges[0].value = GaugeValue(0);
    assert_eq!(d.frame(0, &[]).len(), 1);
    // 241 is the last judgment's perfect, a built-in although declared.
    let loaded = image(r#""op": [241]"#);
    assert!(loaded.report().notes.is_empty());
    let mut d = loaded.drawn();
    assert!(d.frame(0, &[]).is_empty());
    assert_eq!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).len(), 1);
    assert!(d.frame(20, &[judged(20, Judgment::Great, 2)]).is_empty());
}

/// 241 reads the latest judgment whether or not the skin draws a judge,
/// through every way of naming it and its negation.
#[test]
fn the_perfect_judgment_needs_no_judge() {
    let perfect = [judged(10, Judgment::Perfect, 1)];
    let great = [judged(20, Judgment::Great, 2)];
    for (fields, on_perfect) in [
        (r#""op": [241]"#, true),
        (r#""op": ["judge_1p_perfect"]"#, true),
        (r#""op": ["!!judge_1p_perfect"]"#, true),
        (r#""draw": "judge_1p_perfect""#, true),
        (r#""draw": 241"#, true),
        (r#""op": [-241]"#, false),
        (r#""op": ["!judge_1p_perfect"]"#, false),
        (r#""draw": "!judge_1p_perfect""#, false),
        (r#""draw": -241"#, false),
    ] {
        let loaded = image(fields);
        assert!(loaded.report().notes.is_empty(), "{fields}");
        assert_eq!(loaded.report().usage.images, 1, "{fields}");
        let mut d = loaded.drawn();
        assert_eq!(
            d.frame(10, &perfect).len(),
            usize::from(on_perfect),
            "{fields}"
        );
        assert_eq!(
            d.frame(20, &great).len(),
            usize::from(!on_perfect),
            "{fields}"
        );
    }
}

#[test]
fn known_unsupported_built_ins_drop() {
    let loaded = image(r#""op": [300]"#);
    let why = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Boolean,
        id: Some(300),
    };
    assert!(has(loaded.report(), why));
    assert!(
        !loaded
            .report()
            .notes
            .iter()
            .any(|n| n.kind == ignored("op"))
    );
    let e = critical(r#""op": [300]"#).err();
    assert!(matches!(e, ImportError::Critical { why: w, .. } if w == why));
    // The chart conditions beside the BPM-change pair stay unsupported.
    for id in [172, 1_177] {
        let why = NoteKind::UnsupportedProperty {
            kind: PropertyKind::Boolean,
            id: Some(id),
        };
        assert!(
            has(image(&format!(r#""op": [{id}]"#)).report(), why),
            "{id}"
        );
    }
}

#[test]
fn declared_options_fold() {
    for (op, shown) in [(900, true), (-900, false), (901, false), (-901, true)] {
        let loaded = image(&format!(r#""op": [{op}]"#));
        assert_eq!(
            has(loaded.report(), ignored("op")),
            !shown,
            "{op}: {:?}",
            loaded.report()
        );
        assert_eq!(loaded.quads(0).len(), usize::from(shown), "{op}");
    }
    // The same signed id counts once; both signs never draw.
    assert_eq!(image(r#""op": [900, 900]"#).quads(0).len(), 1);
    assert!(image(r#""op": [900, -900]"#).quads(0).is_empty());
}

/// `table_song` is false: the group under its negation draws, the group
/// under it never does, by number and by name, with nothing dropped.
#[test]
fn the_table_song_group_under_its_negation_draws() {
    for (op, name, drawn) in [(-1_008, "!table_song", 1), (1_008, "table_song", 0)] {
        for fields in [format!(r#""op": [{op}]"#), format!(r#""op": ["{name}"]"#)] {
            let loaded = image(&fields);
            let notes = &loaded.report().notes;
            assert!(
                !notes
                    .iter()
                    .any(|n| matches!(n.kind, NoteKind::UnsupportedProperty { .. })),
                "{fields}: {notes:?}"
            );
            assert_eq!(loaded.quads(0).len(), drawn, "{fields}");
        }
    }
}

#[test]
fn names_use_the_same_table() {
    let mut d = image(r#""op": ["!!judge_1p_perfect"]"#).drawn();
    assert_eq!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).len(), 1);
    let mut d = image(r#""draw": "!judge_1p_perfect""#).drawn();
    assert_eq!(d.frame(0, &[]).len(), 1);
    assert!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).is_empty());
    let script = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Boolean,
        id: None,
    };
    assert!(has(image(r#""op": ["a_script()"]"#).report(), script));
    assert!(has(image(r#""draw": "nothing""#).report(), script));
    // A numeric `draw` that is not a built-in has no effect.
    let loaded = image(r#""draw": 900"#);
    assert!(has(loaded.report(), ignored("draw")));
    assert_eq!(loaded.quads(0).len(), 1);
}

#[test]
fn repeated_conditions_add_nothing() {
    // 500 000 repeats of one name build one condition, not a source past
    // the compiler's limit.
    let many = vec![r#""judge_1p_perfect""#; 500_000].join(",");
    let once = image(r#""op": ["judge_1p_perfect"]"#);
    let repeated = image(&format!(r#""op": [{many}], "draw": "judge_1p_perfect""#));
    assert_eq!(
        repeated.ok().skin.ops().total(),
        once.ok().skin.ops().total()
    );
    let mut d = repeated.drawn();
    assert!(d.frame(10, &[]).is_empty());
    assert_eq!(d.frame(20, &[judged(20, Judgment::Perfect, 1)]).len(), 1);
    // Every supported condition and its negation together fit.
    let all: Vec<String> = (0..=10)
        .map(|k| format!(r#""gauge_1p_{}""#, gauge_name(k)))
        .chain(std::iter::once(r#""judge_1p_perfect""#.to_owned()))
        .flat_map(|n| [n.clone(), n.replacen('"', "\"!", 1)])
        .collect();
    let loaded = image(&format!(r#""op": [{}]"#, all.join(",")));
    assert!(loaded.ok().skin.ops().total() > 0);
}

const D4: NoteKind = NoteKind::Deviation(skin_beatoraja::DeviationId::D4);

#[test]
fn gauge_families_read_the_shown_kind() {
    // The drawn play shows one normal gauge: groove, neither hard nor ex.
    for (op, shown) in [
        ("42", true),
        ("-42", false),
        ("43", false),
        (r#""!gauge_hard""#, true),
        (r#""gauge_ex""#, false),
        ("-1046", true),
    ] {
        let loaded = image(&format!(r#""op": [{op}]"#));
        assert_eq!(common::kinds(loaded.report()), [D4], "{op}");
        let mut d = loaded.drawn();
        assert_eq!(d.frame(0, &[]).len(), usize::from(shown), "{op}");
        // With no shown gauge every family is false and every negation
        // true.
        d.gauges.clear();
        let negated = op.starts_with('-') || op.contains('!');
        assert_eq!(d.frame(16, &[]).len(), usize::from(negated), "{op}");
    }
    // Through `draw` too, and repeated: one condition, one note.
    let once = image(r#""op": [42]"#);
    let repeated = image(r#""op": [42, "gauge_groove", 42], "draw": "gauge_groove""#);
    assert_eq!(common::kinds(repeated.report()), [D4]);
    assert_eq!(
        repeated.ok().skin.ops().total(),
        once.ok().skin.ops().total()
    );
    // A built-in past the ranks stays unsupported.
    let loaded = image(r#""op": [300]"#);
    let why = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Boolean,
        id: Some(300),
    };
    assert!(has(loaded.report(), why));
}

#[test]
fn a_rate_number_under_a_kind_condition_has_one_deviation() {
    let loaded = import_body(
        r#""value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3,
                      "ref": 102}],
           "destination": [{"id": "v", "op": [43], "dst": [{"w": 12, "h": 20}]}]"#,
    );
    assert_eq!(common::kinds(loaded.report()), [D4]);
    // A critical EX score under one is kept, with its note.
    let loaded = critical(r#""op": [-43]"#);
    assert_eq!(common::kinds(loaded.report()), [D4]);
    assert_eq!(loaded.drawn().frame(0, &[]).len(), 1);
}

#[test]
fn every_condition_together_passes_the_source_limit() {
    let all: Vec<String> = (0..=10)
        .map(|k| format!(r#""gauge_1p_{}""#, gauge_name(k)))
        .chain(
            ["judge_1p_perfect", "gauge_groove", "gauge_hard", "gauge_ex"]
                .map(|n| format!(r#""{n}""#)),
        )
        .flat_map(|n| [n.clone(), n.replacen('"', "\"!", 1)])
        .collect();
    let e = image(&format!(r#""op": [{}]"#, all.join(","))).err();
    let ImportError::Expr { at, source } = &e else {
        panic!("{e:?}");
    };
    assert!(
        matches!(source.kind, skin_model::ErrorKind::TooLong { .. }),
        "{e:?}"
    );
    assert!(at.line > 0, "{e:?}");

    // One sign of every supported built-in, the longer negated one, on a
    // number with an availability guard: refused at the destination too.
    let ops = (230..=241)
        .chain([
            42, 43, 1_046, 270, 271, 272, 273, 176, 177, 32, 33, 80, 81, 1_242, 1_243,
        ])
        .chain(2_242..=2_245)
        .map(|id| (-id).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let loaded = import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 3,
                      "ref": 160}}],
           "destination": [{{"id": "v", "op": [{ops}], "dst": [{{"w": 12, "h": 20}}]}}]"#
    ));
    let e = loaded.result.as_ref().err();
    let Some(ImportError::Expr { at, source }) = e else {
        panic!("{e:?}");
    };
    assert!(
        matches!(source.kind, skin_model::ErrorKind::TooLong { .. }),
        "{e:?}"
    );
    assert!(at.line > 0, "{e:?}");
}

fn gauge_name(k: u32) -> String {
    if k == 10 {
        "100".to_owned()
    } else {
        format!("{}_{}", 10 * k, 10 * (k + 1))
    }
}

#[test]
fn cover_switches_by_number_and_name_build_one_condition() {
    let once = image(r#""op": [271]"#);
    let repeated = image(r#""op": [271, "lanecover1_on", "!!lanecover1_on"], "draw": 271"#);
    assert!(common::kinds(repeated.report()).is_empty());
    assert_eq!(
        repeated.ok().skin.ops().total(),
        once.ok().skin.ops().total()
    );
    // 270 reads the adjustments' snapshot as the source reads its
    // controls: no deviation, however often it is listed.
    let changing = image(r#""op": [270, "lanecover1_changing"]"#);
    assert!(common::kinds(changing.report()).is_empty());
    // The built-in wins over an item declared with its number.
    let declared = import_body(
        r#""property": [{"name": "p", "item": [{"name": "x", "op": 272}], "def": "x"}],
           "image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
           "destination": [{"id": "i", "op": [-272], "dst": [{"w": 10, "h": 10}]}]"#,
    );
    let mut d = declared.drawn();
    // The drawn play's covers are on: the negated switch hides it.
    assert!(d.frame(0, &[]).is_empty());
}

/// Both signs of one BPM-change condition fit one source and never draw,
/// whatever the chart's fact.
#[test]
fn both_signs_of_a_bpm_change_condition_never_draw() {
    let loaded = image(r#""op": [176, -176]"#);
    assert_eq!(common::kinds(loaded.report()), [D4]);
    let chart = bms_model::ChartBuilder::new(150_000).build().unwrap();
    for fact in [Some(false), Some(true), None] {
        let skin = loaded.ok().skin.clone();
        let mut d = common::play::Drawn::over_with(skin, &chart, None, fact);
        assert!(d.frame(0, &[]).is_empty(), "{fact:?}");
    }
}

/// s10-10-design.md C28, through a JSON skin: option 201 holds §8's band,
/// timer 351 waits for the best and graph 113 is the best's whole rate,
/// in a play of 7 notes whose best is 11 and target 13.
#[test]
fn json_score_figures_read_the_play() {
    let loaded = import_body(
        r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
           "graph": [{"id": "g", "src": "a", "w": 64, "h": 4, "type": 113, "angle": 0}],
           "destination": [
             {"id": "g", "dst": [{"x": 0, "y": 0, "w": 140, "h": 4}]},
             {"id": "i", "op": [201], "dst": [{"x": 0, "y": 100, "w": 11, "h": 10}]},
             {"id": "i", "timer": 351, "dst": [{"x": 0, "y": 200, "w": 12, "h": 10}]}
           ]"#,
    );
    let mut d = loaded.drawn_over(7);
    let ctx = skin_model::PlayContext {
        score: skin_model::PlayScore {
            basis: skin_model::ScoreBasis {
                best: 11,
                best_known: true,
                target: 13,
            },
            ..skin_model::PlayScore::NONE
        },
        ..common::play::context()
    };
    d.play = d.skin.prepare(&d.chart, &ctx).unwrap();
    let mut t = common::play::tally(5, 3);
    t.counts.perfect = 2;
    t.counts.great = 1;
    d.tally = t;
    let widths: Vec<f32> = d.frame(1_000, &[]).iter().map(|q| q.rect[2]).collect();
    // The bar at 11/14 of 140 and the band's image; the timer's waits.
    assert_eq!(widths.len(), 2, "{widths:?}");
    assert!((widths[0] - 110.0).abs() < 1e-3, "{widths:?}");
    assert!((widths[1] - 11.0).abs() < 1e-3, "{widths:?}");
    let mut t = common::play::tally(12, 6);
    t.counts.perfect = 6;
    d.tally = t;
    // Rank 201 off (AAA now), the timer's image on.
    let widths: Vec<f32> = d.frame(1_016, &[]).iter().map(|q| q.rect[2]).collect();
    assert_eq!(widths.len(), 2, "{widths:?}");
    assert!((widths[1] - 12.0).abs() < 1e-3, "{widths:?}");
}
