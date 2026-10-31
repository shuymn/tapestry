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

/// A judge of no grades: it draws nothing (an ignored note) but gives
/// the skin its judge region.
const REGION: &str = r#""judge": [{"id": "j", "index": 0, "images": [], "numbers": []}]"#;

/// [`image`] after a [`REGION`] judge's destination, or before it with
/// `after`.
fn judged_image(fields: &str, after: bool) -> common::Loaded {
    let i = format!(r#"{{"id": "i", "timer": 41, {fields}, "dst": [{{"w": 10, "h": 10}}]}}"#);
    let j = r#"{"id": "j", "dst": [{"w": 1, "h": 1}]}"#;
    let (a, b) = if after {
        (i.as_str(), j)
    } else {
        (j, i.as_str())
    };
    import_body(&format!(
        r#"{HEADER}, {REGION}, "image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{a}, {b}]"#
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
/// selects 902. The source tree's conditions see every selection
/// (`JSONSkinLoader.java:254-260`); a destination's op sees the later
/// option's entry, which replaced the first's (`JSONSkinLoader.java:
/// 291-297`).
#[test]
fn a_later_option_overrides_a_shared_item_for_ops() {
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
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [900], {key}}}"#)), 0);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [-900], {key}}}"#)), 1);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [902], {key}}}"#)), 1);
    assert_eq!(drawn(&format!(r#"{{"id": "i", "op": [901], {key}}}"#)), 0);
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
    let loaded = judged_image(r#""op": [241]"#, false);
    assert_eq!(common::kinds(loaded.report()), [ignored("judge")]);
    let mut d = loaded.drawn();
    assert!(d.frame(0, &[]).is_empty());
    assert_eq!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).len(), 1);
    assert!(d.frame(20, &[judged(20, Judgment::Great, 2)]).is_empty());
}

/// 241 and timers 46 and 446 need a judge region, which a judge's
/// destination anywhere in the skin makes (`JsonPlaySkinObjectLoader.
/// java:283-287`, `JudgeManager.java:129, 677-690`); without one 241 is
/// false and the timers are never set, through every way of naming them.
#[test]
fn the_perfect_judgment_needs_a_judge_region() {
    let perfect = [judged(10, Judgment::Perfect, 1)];
    for after in [false, true] {
        let mut d = judged_image(r#""op": [241]"#, after).drawn();
        assert_eq!(d.frame(10, &perfect).len(), 1, "after {after}");
        let mut d = judged_image(r#""op": [-241]"#, after).drawn();
        assert!(d.frame(10, &perfect).is_empty(), "after {after}");
    }
    let fold_false = |fields: &str| {
        let loaded = image(fields);
        assert!(has(loaded.report(), ignored("op")), "{fields}");
        assert_eq!(loaded.report().usage.images, 0, "{fields}");
    };
    fold_false(r#""op": [241]"#);
    fold_false(r#""op": ["judge_1p_perfect"]"#);
    fold_false(r#""op": ["!!judge_1p_perfect"]"#);
    fold_false(r#""draw": "judge_1p_perfect""#);
    fold_false(r#""draw": 241"#);
    for fields in [
        r#""op": [-241]"#,
        r#""op": ["!judge_1p_perfect"]"#,
        r#""draw": "!judge_1p_perfect""#,
        r#""draw": -241"#,
    ] {
        let loaded = image(fields);
        assert!(loaded.report().notes.is_empty(), "{fields}");
        let mut d = loaded.drawn();
        assert_eq!(d.frame(10, &perfect).len(), 1, "{fields}");
    }
}

#[test]
fn known_unsupported_built_ins_drop() {
    let loaded = image(r#""op": [202]"#);
    let why = NoteKind::UnsupportedProperty {
        kind: PropertyKind::Boolean,
        id: Some(202),
    };
    assert!(has(loaded.report(), why));
    assert!(
        !loaded
            .report()
            .notes
            .iter()
            .any(|n| n.kind == ignored("op"))
    );
    let e = critical(r#""op": [202]"#).err();
    assert!(matches!(e, ImportError::Critical { why: w, .. } if w == why));
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

#[test]
fn names_use_the_same_table() {
    let mut d = judged_image(r#""op": ["!!judge_1p_perfect"]"#, false).drawn();
    assert_eq!(d.frame(10, &[judged(10, Judgment::Perfect, 1)]).len(), 1);
    let mut d = judged_image(r#""draw": "!judge_1p_perfect""#, false).drawn();
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
    let once = judged_image(r#""op": ["judge_1p_perfect"]"#, false);
    let repeated = judged_image(
        &format!(r#""op": [{many}], "draw": "judge_1p_perfect""#),
        false,
    );
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
    let loaded = judged_image(&format!(r#""op": [{}]"#, all.join(",")), false);
    assert!(loaded.ok().skin.ops().total() > 0);
}

fn gauge_name(k: u32) -> String {
    if k == 10 {
        "100".to_owned()
    } else {
        format!("{}_{}", 10 * k, 10 * (k + 1))
    }
}
