//! The gauge: its nodes, motion, parts and fields, through `build`.
//!
//! The gauge sits at (100, 500, 100, 10): drawn over [100, 570, 100, 10],
//! one segment per tenth with 10 parts.

mod common;

use common::play::round;
use common::{Loaded, has, import_sized};
use skin_beatoraja::{CriticalPart, DeviationId, ImportError, NoteKind};

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
        // Still: range 0, also with no cycle; −2 truncates to 0.
        r#""type": 1, "range": 0, "cycle": 0"#,
        r#""type": 0, "range": -1"#,
        r#""type": 0, "range": -2"#,
        r#""type": 1, "range": -2"#,
        r#""type": 2, "range": -2"#,
        // Flicker reads no range.
        r#""type": 3, "range": -5, "cycle": 100"#,
    ] {
        gauge(fields).ok();
    }
    assert!(has(
        gauge(r#""type": 4"#).report(),
        NoteKind::Ignored { field: "type" }
    ));
    for fields in [
        r#""type": 0, "range": -3"#,
        r#""type": 1, "range": -1"#,
        r#""type": 2, "range": -3"#,
        r#""type": 0, "range": 11"#,
    ] {
        refused(gauge(fields), "range");
    }
    for fields in [
        r#""type": 3, "cycle": 0"#,
        r#""type": 1, "range": 2, "cycle": 0"#,
    ] {
        refused(gauge(fields), "cycle");
    }
}

#[test]
fn parts_bound_the_segments() {
    assert!(has(
        gauge_with(&format!(r#""nodes": {}, "parts": 0"#, nodes(4)), "", "").report(),
        NoteKind::Ignored { field: "parts" }
    ));
    refused(
        gauge_with(&format!(r#""nodes": {}, "parts": 1001"#, nodes(4)), "", ""),
        "parts",
    );
    let l = gauge_with(&format!(r#""nodes": {}, "parts": 1000"#, nodes(4)), "", "");
    assert_eq!(l.report().usage.widget_quads, 1_000);
    // Flicker adds the tip.
    assert_eq!(
        gauge(r#""type": 3, "cycle": 100"#)
            .report()
            .usage
            .widget_quads,
        11
    );
    assert_eq!(gauge("").report().usage.widget_quads, 10);
}
