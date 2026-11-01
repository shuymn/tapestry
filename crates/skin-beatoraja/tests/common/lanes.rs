//! Self-made play objects: lanes over `a.png` parts, SUDDEN+, LIFT, a
//! judge and a gauge, as JSON members for the common skin bodies.

#![allow(dead_code)]

/// The part images, every one a cell of `a.png` (64×64): note, long-note
/// start, end, body, active body, mine, and spares.
pub const PARTS: &str = r#""image": [
    {"id": "nt", "src": "a", "x": 0, "y": 0, "w": 8, "h": 20},
    {"id": "ls", "src": "a", "x": 8, "y": 0, "w": 8, "h": 20},
    {"id": "le", "src": "a", "x": 16, "y": 0, "w": 8, "h": 20},
    {"id": "lb", "src": "a", "x": 24, "y": 0, "w": 8, "h": 20},
    {"id": "la", "src": "a", "x": 32, "y": 0, "w": 8, "h": 20},
    {"id": "mi", "src": "a", "x": 40, "y": 0, "w": 8, "h": 20},
    {"id": "bar", "src": "a", "x": 0, "y": 20, "w": 8, "h": 2},
    {"id": "cover", "src": "a", "x": 0, "y": 24, "w": 16, "h": 16},
    {"id": "lift", "src": "a", "x": 16, "y": 24, "w": 16, "h": 16},
    {"id": "deco", "src": "a", "x": 32, "y": 24, "w": 16, "h": 16},
    {"id": "g0", "src": "a", "x": 0, "y": 40, "w": 8, "h": 8},
    {"id": "g1", "src": "a", "x": 8, "y": 40, "w": 8, "h": 8},
    {"id": "g2", "src": "a", "x": 16, "y": 40, "w": 8, "h": 8},
    {"id": "g3", "src": "a", "x": 24, "y": 40, "w": 8, "h": 8}
    EXTRA]"#;

/// [`PARTS`] with more image records.
pub fn parts(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    PARTS.replace("EXTRA", &format!("{sep}{extra}"))
}

/// Eight lanes of the source: keys 50 wide from 100, the scratch (source
/// lane 7) 80 wide at 20, all from y 200, 600 tall.
pub fn lane_rects(y: i32, h: i32) -> String {
    let mut v: Vec<String> = (0..7)
        .map(|i| format!(r#"{{"x": {}, "y": {y}, "w": 50, "h": {h}}}"#, 100 + 50 * i))
        .collect();
    v.push(format!(r#"{{"x": 20, "y": {y}, "w": 80, "h": {h}}}"#));
    v.join(", ")
}

/// `name` eight times, as a JSON list.
pub fn eight(name: &str) -> String {
    let v: Vec<String> = (0..8).map(|_| format!("\"{name}\"")).collect();
    format!("[{}]", v.join(", "))
}

/// The `note` member with `extra` fields; `dst` is its lane rects.
pub fn note_with(dst: &str, extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#""note": {{"id": "notes", "note": {n}, "lnstart": {s}, "lnend": {e},
            "lnbody": {b}, "lnactive": {a}, "hcnstart": {s}, "hcnend": {e},
            "hcnbody": {b}, "hcnactive": {a}, "hcndamage": {b}, "hcnreactive": {a},
            "mine": {m}, "dst": [{dst}]{sep}{extra}}}"#,
        n = eight("nt"),
        s = eight("ls"),
        e = eight("le"),
        b = eight("lb"),
        a = eight("la"),
        m = eight("mi"),
    )
}

/// The `note` member over the default lanes with `extra` fields.
pub fn note(extra: &str) -> String {
    note_with(&lane_rects(200, 600), extra)
}

/// The note destination with `extra` fields, one key at 0.
pub const NOTES: &str = r#"{"id": "notes", "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;

/// A SUDDEN+ destination over the default lanes: x 20, w 430, its lower
/// edge on the lanes' top (800), 600 tall; `extra` fields.
pub fn cover(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#"{{"id": "cover", "offset": 4{sep}{extra},
            "dst": [{{"x": 20, "y": 800, "w": 430, "h": 600}}]}}"#
    )
}

/// The `liftCover` member of the default lanes: trimmed at y 200.
pub fn lift_object(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#""liftCover": [{{"id": "liftc", "src": "a", "x": 16, "y": 24, "w": 16, "h": 16,
            "disapearLine": 200{sep}{extra}}}]"#
    )
}

/// A LIFT destination under the default lanes, `extra` fields.
pub fn lift(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#"{{"id": "liftc"{sep}{extra},
            "dst": [{{"x": 20, "y": 0, "w": 430, "h": 200}}]}}"#
    )
}

/// The `hiddenCover` member: a 16×16 cell of `a.png` trimmed at y 200,
/// the default lanes' lower edge; `extra` fields.
pub fn hidden_object(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#""hiddenCover": [{{"id": "hid", "src": "a", "x": 32, "y": 24, "w": 16, "h": 16,
            "disapearLine": 200{sep}{extra}}}]"#
    )
}

/// A HIDDEN+ destination across the default lanes, 600 tall with its
/// upper edge on their lower edge (y 200); `extra` fields.
pub fn hidden(extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { ", " };
    format!(
        r#"{{"id": "hid"{sep}{extra},
            "dst": [{{"x": 20, "y": -400, "w": 430, "h": 600}}]}}"#
    )
}

/// A body of [`PARTS`] with `extra` images, `objects` members, and the
/// destinations `dsts`.
pub fn body(extra: &str, objects: &str, dsts: &[&str]) -> String {
    let sep = if objects.is_empty() { "" } else { ", " };
    format!(
        r#"{}{sep}{objects}, "destination": [{}]"#,
        parts(extra),
        dsts.join(", ")
    )
}
