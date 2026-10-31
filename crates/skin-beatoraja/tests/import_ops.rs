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
