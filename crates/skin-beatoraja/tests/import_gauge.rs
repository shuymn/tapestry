//! The gauge: its nodes, motion, parts and fields, through `build`.
//!
//! The gauge sits at (100, 500, 100, 10): drawn over [100, 570, 100, 10],
//! one segment per tenth with 10 parts.

mod common;

use common::play::round;
use common::{Loaded, d3_notes, has, import_body, import_sized};
use skin_beatoraja::{CriticalPart, DeviationId, ImportError, NoteKind, TimerWhy};
use skin_model::Filter;

/// Node images n0..n35, each an 8×8 cell of `a`, and `extra` records.
fn images(extra: &str) -> String {
    let v: Vec<String> = (0..36)
        .map(|k| {
            format!(
                r#"{{"id": "n{k}", "src": "a", "x": {}, "y": {}, "w": 8, "h": 8}}"#,
                8 * (k % 8),
                8 * (k / 8)
            )
        })
        .collect();
    format!(r#""image": [{}{extra}]"#, v.join(", "))
}

/// The node list n0..n(count−1).
fn nodes(count: usize) -> String {
    let v: Vec<String> = (0..count).map(|k| format!("\"n{k}\"")).collect();
    format!("[{}]", v.join(", "))
}

/// A gauge of `fields` drawn with `dst` fields, beside `extra` images.
fn gauge_with(fields: &str, dst: &str, extra: &str) -> Loaded {
    let body = format!(
        r#"{}, "gauge": {{"id": "g", {fields}}},
           "destination": [{{"id": "g"{dst}, "dst": [{{"x": 100, "y": 500, "w": 100, "h": 10}}]}}]"#,
        images(extra)
    );
    import_sized([1_920, 1_080], &body, &[])
}

/// Four nodes, 10 parts, and `fields`.
fn gauge(fields: &str) -> Loaded {
    let sep = if fields.is_empty() { "" } else { ", " };
    gauge_with(
        &format!(r#""nodes": {}, "parts": 10{sep}{fields}"#, nodes(4)),
        "",
        "",
    )
}

#[track_caller]
fn refused(l: Loaded, field: &'static str) {
    match l.err() {
        ImportError::Critical { part, why, .. } => assert_eq!(
            (part, why),
            (CriticalPart::Gauge, NoteKind::UnsupportedField { field })
        ),
        e => panic!("{e:?}"),
    }
}

#[test]
fn node_counts_the_source_indexes_import() {
    for count in [4, 8, 12, 36] {
        let l = gauge_with(
            &format!(r#""nodes": {}, "parts": 10"#, nodes(count)),
            "",
            "",
        );
        assert!(
            has(l.report(), NoteKind::Deviation(DeviationId::D10)),
            "{count}"
        );
        // At the start value (20 %), two front segments and eight back.
        assert_eq!(l.quads(1_000).len(), 10, "{count}");
    }
    for count in [3, 5, 35] {
        let l = gauge_with(
            &format!(r#""nodes": {}, "parts": 10"#, nodes(count)),
            "",
            "",
        );
        refused(l, "nodes");
    }
    refused(gauge_with(r#""parts": 10"#, "", ""), "nodes");
    refused(
        gauge_with(
            r#""nodes": ["n0", "n1", "n2", "missing"], "parts": 10"#,
            "",
            "",
        ),
        "nodes",
    );
    // An image of the empty id is a node; a `null` entry is none.
    let empty = r#", {"id": "", "src": "a", "x": 0, "y": 32, "w": 8, "h": 8}"#;
    gauge_with(r#""nodes": ["n0", "n1", "n2", ""], "parts": 10"#, "", empty).ok();
    refused(
        gauge_with(
            r#""nodes": ["n0", "n1", "n2", null], "parts": 10"#,
            "",
            empty,
        ),
        "nodes",
    );
    // A node of fewer frames than the last.
    let two = r#", {"id": "two", "src": "a", "w": 16, "h": 8, "divx": 2}"#;
    refused(
        gauge_with(
            r#""nodes": ["n0", "n1", "n2", "two"], "parts": 10"#,
            "",
            two,
        ),
        "nodes",
    );
    gauge_with(
        r#""nodes": ["two", "n1", "n2", "n3"], "parts": 10"#,
        "",
        two,
    )
    .ok();
}

#[test]
fn nodes_draw_their_first_frame_nearest() {
    // A 2×2 grid over 16×16 draws its first 8×8 cell, as a plain node.
    let grid =
        r#", {"id": "grid", "src": "a", "x": 0, "y": 0, "w": 16, "h": 16, "divx": 2, "divy": 2}"#;
    let uv = |l: Loaded| {
        let q = l.quads(1_000);
        let u = q[0].uv;
        (u[2] - u[0], u[3] - u[1])
    };
    let plain = uv(gauge(""));
    let gridded = uv(gauge_with(
        r#""nodes": ["grid", "n1", "n2", "n3"], "parts": 10"#,
        "",
        grid,
    ));
    assert_eq!(plain, gridded);
    // Colour and blend kept; the angle cleared; stretch and filter noted.
    let l = gauge_with(
        &format!(r#""nodes": {}, "parts": 10"#, nodes(4)),
        r#", "blend": 2, "stretch": 1, "filter": 1"#,
        "",
    );
    for field in ["stretch", "filter"] {
        assert!(has(l.report(), NoteKind::Ignored { field }), "{field}");
    }
    assert_eq!(l.quads(1_000)[0].filter, Filter::Nearest);
    // Two keys at one time differing only in the ignored angle.
    let body = format!(
        r#"{}, "gauge": {{"id": "g", "nodes": {}, "parts": 10}},
           "destination": [{{"id": "g", "dst": [
               {{"time": 0, "x": 100, "y": 500, "w": 100, "h": 10}},
               {{"time": 0, "x": 100, "y": 500, "w": 100, "h": 10, "angle": 90}}]}}]"#,
        images(""),
        nodes(4)
    );
    let l = import_sized([1_920, 1_080], &body, &[]);
    assert!(has(l.report(), NoteKind::Ignored { field: "angle" }));
    assert_eq!(round(l.quads(1_000)[0].rect), [100, 570, 10, 10]);
    let body = format!(
        r#"{}, "gauge": {{"id": "g", "nodes": {}, "parts": 10}},
           "destination": [{{"id": "g", "dst": [{{"x": 100, "y": 500, "w": 100, "h": 10, "r": 0, "angle": 90}}]}}]"#,
        images(""),
        nodes(4)
    );
    let l = import_sized([1_920, 1_080], &body, &[]);
    assert!(has(l.report(), NoteKind::Ignored { field: "angle" }));
    let q = l.quads(1_000);
    assert_eq!(round(q[0].rect), [100, 570, 10, 10]);
    assert_eq!(q[0].color, [0.0, 1.0, 1.0, 1.0]);
    assert_eq!(q[0].angle, 0.0);
}

#[test]
fn motions_follow_the_source() {
    for fields in [
        // Random, Rise, Fall, Flicker.
        r#""type": 0, "range": 3, "cycle": 33"#,
        r#""type": 1, "range": 3, "cycle": 33"#,
        r#""type": 2, "range": 3, "cycle": 33"#,
        r#""type": 3, "cycle": 100"#,
        // Still: range 0, also with no cycle.
        r#""type": 1, "range": 0, "cycle": 0"#,
        r#""type": 3, "range": 0, "cycle": 0"#,
    ] {
        gauge(fields).ok();
    }
    // A negative range is still for every kind, as range 0.
    for kind in 0..=3 {
        let still = |range: i32| {
            let mut d = gauge(&format!(r#""type": {kind}, "range": {range}, "cycle": 33"#)).drawn();
            [0, 17, 50, 1_000].map(|ms| d.frame(ms, &[]))
        };
        let zero = still(0);
        for range in [-1, -3] {
            assert_eq!(still(range), zero, "{kind} {range}");
        }
    }
    assert!(has(
        gauge(r#""type": 4"#).report(),
        NoteKind::Ignored { field: "type" }
    ));
    refused(gauge(r#""type": 0, "range": 11"#), "range");
    for fields in [
        r#""type": 3, "cycle": 0"#,
        r#""type": 1, "range": 2, "cycle": 0"#,
    ] {
        refused(gauge(fields), "cycle");
    }
}

/// Four nodes and `fields` in place of the parts.
fn parted(fields: &str) -> Loaded {
    gauge_with(&format!(r#""nodes": {}, {fields}"#, nodes(4)), "", "")
}

#[test]
fn parts_bound_the_segments() {
    assert!(has(
        parted(r#""parts": 0"#).report(),
        NoteKind::Ignored { field: "parts" }
    ));
    // Up to the widget budget's 16 384, each part a quad, one D10 note
    // and no unsupported `parts`.
    for p in [1_000, 1_001, 4_096, 16_384] {
        let l = parted(&format!(r#""parts": {p}"#));
        assert_eq!(l.report().usage.widget_quads, p, "{p}");
        let notes: Vec<NoteKind> = l.report().notes.iter().map(|n| n.kind).collect();
        assert_eq!(notes, [NoteKind::Deviation(DeviationId::D10)], "{p}");
        assert_eq!(l.quads(1_000).len(), p as usize, "{p}");
    }
    refused(parted(r#""parts": 16385"#), "parts");
    // Flicker adds the tip: 16 383 parts fill the budget, 16 384 pass it,
    // while a range of 0 makes the type-3 gauge still and it fits.
    assert_eq!(
        gauge(r#""type": 3, "cycle": 100"#)
            .report()
            .usage
            .widget_quads,
        11
    );
    let l = parted(r#""parts": 16383, "type": 3, "range": 1, "cycle": 100"#);
    assert_eq!(l.report().usage.widget_quads, 16_384);
    match parted(r#""parts": 16384, "type": 3, "range": 1, "cycle": 100"#).err() {
        ImportError::Limit {
            limit: skin_beatoraja::ImportLimit::WidgetQuads,
            at: Some(at),
        } => assert_eq!(at.line, 3, "{at:?}"),
        e => panic!("{e:?}"),
    }
    let l = parted(r#""parts": 16384, "type": 3, "range": 0, "cycle": 100"#);
    assert_eq!(l.report().usage.widget_quads, 16_384);
    assert_eq!(gauge("").report().usage.widget_quads, 10);
}

#[test]
fn gauge_scalars_are_read_exactly() {
    // Whole numbers in any spelling and decimal strings; defaults only
    // when absent.
    for fields in [
        r#""parts": 10.0"#,
        r#""parts": 1e1"#,
        r#""parts": "10""#,
        r#""parts": 10, "type": 2.0, "range": 1.0, "cycle": 1e2"#,
    ] {
        assert_eq!(parted(fields).quads(1_000).len(), 10, "{fields}");
    }
    // A fraction, a null or a string that is no integer is a type error,
    // past `i32` a range error, each at its field.
    for (fields, field) in [
        (r#""parts": 10.5"#, "parts"),
        (r#""parts": null"#, "parts"),
        (r#""parts": 10, "type": 0.5"#, "type"),
        (r#""parts": 10, "range": 2.5"#, "range"),
        (r#""parts": 10, "cycle": 33.3"#, "cycle"),
        (r#""parts": "ten""#, "parts"),
    ] {
        match parted(fields).err() {
            ImportError::Type { field: f, .. } => assert_eq!(f, field, "{fields}"),
            e => panic!("{fields}: {e:?}"),
        }
    }
    for (fields, field) in [
        (r#""parts": 3000000000"#, "parts"),
        (r#""parts": 10, "cycle": 3e9"#, "cycle"),
        (r#""parts": 1e-100"#, "parts"),
    ] {
        match parted(fields).err() {
            ImportError::Range { field: f, .. } => assert_eq!(f, field, "{fields}"),
            e => panic!("{fields}: {e:?}"),
        }
    }
    // A still gauge's cycle is unused, valid or not; a flicker's range
    // only enables it, whatever its size.
    assert_eq!(
        parted(r#""parts": 10, "type": 1, "range": 0, "cycle": -5"#)
            .quads(1_000)
            .len(),
        10
    );
    let l = parted(r#""parts": 10, "type": 3, "range": 2147483647, "cycle": 100"#);
    assert_eq!(l.report().usage.widget_quads, 11);
}

#[test]
fn node_arrays_are_counted_before_they_are_read() {
    // Five entries that are no ids: an unsupported count, refused before
    // any entry is read as a string.
    refused(
        gauge_with(r#""nodes": [{}, {}, {}, {}, {}], "parts": 10"#, "", ""),
        "nodes",
    );
    // A long array likewise.
    let long = vec!["{}"; 5_000].join(", ");
    refused(
        gauge_with(&format!(r#""nodes": [{long}], "parts": 10"#), "", ""),
        "nodes",
    );
    // A supported count still reads its entries.
    match gauge_with(r#""nodes": [{}, {}, {}, {}], "parts": 10"#, "", "").err() {
        ImportError::Type { field: "nodes", .. } => {}
        e => panic!("{e:?}"),
    }
}

/// `n` numbers of 11 cells and one of 5, spending the widget budget
/// before the gauge's destination `gauge` (none when empty), whose nodes
/// name an image the skin lacks.
fn spent(n: usize, gauge: &str) -> Loaded {
    let values = r#", "value": [
        {"id": "v11", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 11, "ref": 71},
        {"id": "vl", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 5, "ref": 71}]"#;
    let mut dsts: Vec<String> = (0..n)
        .map(|_| r#"{"id": "v11", "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}"#.to_owned())
        .collect();
    dsts.push(r#"{"id": "vl", "dst": [{"x": 0, "y": 0, "w": 8, "h": 8}]}"#.to_owned());
    if !gauge.is_empty() {
        dsts.push(gauge.to_owned());
    }
    let body = format!(
        "{}{values}, \"gauge\": {{\"id\": \"g\", \"nodes\": [\"n0\", \"n1\", \"n2\", \"gone\"], \"parts\": 10}},\n\"destination\": [{}]",
        images(""),
        dsts.join(",\n")
    );
    import_sized([1_920, 1_080], &body, &[])
}

#[test]
fn a_gauge_s_quads_are_admitted_before_its_nodes() {
    // 1 489 numbers of 11 cells and one of 5 spend the budget exactly.
    let l = spent(1_489, "");
    assert_eq!(l.report().usage.widget_quads, 16_384);
    // The gauge after them refuses at its own destination on the budget,
    // before its unresolved node is looked up.
    let g = r#"{"id": "g", "dst": [{"x": 100, "y": 500, "w": 100, "h": 10}]}"#;
    let l = spent(1_489, g);
    let text = std::fs::read_to_string(l.dir.path().join("s.json")).unwrap();
    let line = text
        .lines()
        .position(|t| t.starts_with(r#"{"id": "g""#))
        .unwrap()
        + 1;
    match l.err() {
        ImportError::Limit {
            limit: skin_beatoraja::ImportLimit::WidgetQuads,
            at: Some(at),
        } => assert_eq!(at.line as usize, line, "{at:?}"),
        e => panic!("{e:?}"),
    }
    // With room for its 10 quads, the unresolved node refuses it.
    refused(spent(1_488, g), "nodes");
    // A destination with no keys is never drawn: it imports and charges
    // nothing, the budget spent already.
    let l = spent(1_489, r#"{"id": "g", "dst": []}"#);
    assert_eq!(l.report().usage.widget_quads, 16_384);
    // The destination's own checks come first: a key outside `i32`.
    let bad = r#"{"id": "g", "dst": [{"x": 3000000000, "y": 500, "w": 100, "h": 10}]}"#;
    match spent(1_489, bad).err() {
        ImportError::Range { .. } => {}
        e => panic!("{e:?}"),
    }
}

/// A gauge without a timer that fades in and holds, and a second
/// destination of it that pulses additively under a condition, run on
/// `screen` (D3): only their alpha changes, each noted once.
#[test]
fn untimed_gauge_fades_run_on_screen() {
    let base = r#"{"id": "g", "loop": 700, "dst": [
        {"time": 500, "x": 100, "y": 500, "w": 100, "h": 10, "a": 0},
        {"time": 700, "x": 100, "y": 500, "w": 100, "h": 10, "a": 255}]}"#;
    let pulse = r#"{"id": "g", "loop": 700, "blend": 2, "draw": -241, "dst": [
        {"time": 700, "x": 100, "y": 500, "w": 100, "h": 10, "a": 0},
        {"time": 1200, "x": 100, "y": 500, "w": 100, "h": 10, "a": 80},
        {"time": 1500, "x": 100, "y": 500, "w": 100, "h": 10, "a": 0}]}"#;
    let skin = |dsts: &[&str]| {
        let body = format!(
            r#"{}, "gauge": {{"id": "g", "nodes": {}, "parts": 10}},
               "destination": [{}]"#,
            images(""),
            nodes(4),
            dsts.join(", ")
        );
        import_body(&body)
    };
    assert_eq!(d3_notes(&skin(&[base, pulse])), 2);
    // Absent before its first key, then its alpha at each `screen` time.
    let alphas = |dst: &str, ms: i64| -> Vec<f32> {
        skin(&[dst]).quads(ms).iter().map(|q| q.color[3]).collect()
    };
    assert_eq!(alphas(base, 499), Vec::<f32>::new());
    assert_eq!(alphas(base, 600)[0], 127.5 / 255.0);
    assert_eq!(alphas(base, 5_000), [1.0; 10]);
    assert_eq!(alphas(pulse, 699), Vec::<f32>::new());
    assert_eq!(alphas(pulse, 1_200), [80.0 / 255.0; 10]);
    // 800 ms on, the pulse repeats.
    assert_eq!(alphas(pulse, 2_000), [80.0 / 255.0; 10]);
    let mut d = skin(&[pulse]).drawn();
    d.frame(1_200, &[]);
    assert!(
        d.batch
            .runs()
            .iter()
            .all(|r| r.key.blend == skin_model::Blend::Add)
    );
    // Moving or tinting it untimed refuses.
    for last in [
        r#""x": 100, "y": 500, "w": 120, "h": 10"#,
        r#""x": 100, "y": 500, "w": 100, "h": 10, "g": 0"#,
    ] {
        let dst = format!(
            r#"{{"id": "g", "dst": [{{"time": 0, "x": 100, "y": 500, "w": 100, "h": 10}},
                                    {{"time": 100, {last}}}]}}"#
        );
        match skin(&[&dst]).err() {
            ImportError::Critical { part, why, .. } => assert_eq!(
                (part, why),
                (CriticalPart::Gauge, NoteKind::Timer(TimerWhy::Untimed))
            ),
            e => panic!("{e:?}"),
        }
    }
}
