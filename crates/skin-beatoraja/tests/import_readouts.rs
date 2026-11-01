//! Play readouts: the BPM, time, cover, hi-speed and duration numbers by
//! id and name, guarded while unknown, the cover conditions, and the ids
//! that stay unsupported or null.

mod common;

use bms_model::{ChartBuilder, Lane, TimeUs};
use common::play::{Drawn, MS};
use common::{has, import_body};
use skin_beatoraja::{DeviationId, NoteKind, PropertyKind};
use skin_model::{BpmRange, CoverSettings, DurationRatio, FrameReadouts};

const D4: NoteKind = NoteKind::Deviation(DeviationId::D4);

/// The readout numbers, one row each, each id or name once:
/// `(id, property, cells, zero padding)`.
const ROWS: [(&str, &str, i32, bool); 16] = [
    ("max", r#""ref": 90"#, 3, false),
    ("min", r#""value": "minbpm""#, 3, false),
    ("now", r#""value": 160"#, 3, false),
    ("pm", r#""value": "playtime_minute""#, 2, false),
    ("ps", r#""ref": 162"#, 2, true),
    ("lm", r#""value": 163"#, 2, false),
    ("ls", r#""value": "timeleft_second""#, 2, true),
    ("cov", r#""value": "lanecover1""#, 4, false),
    ("lift", r#""ref": 314"#, 4, false),
    ("hid", r#""value": "hidden1""#, 4, false),
    ("abs", r#""value": 316"#, 4, false),
    ("hs2", r#""value": "hispeed_lr2""#, 4, false),
    ("hs", r#""ref": 310"#, 2, false),
    ("hsd", r#""value": "hispeed_afterdot""#, 2, true),
    ("dur", r#""value": 312"#, 4, false),
    ("grn", r#""value": "duration_green""#, 4, false),
];

/// An EX score of one cell at y = 0, which tells the glyphs apart, then
/// [`ROWS`] from y = 50.
fn readouts() -> common::Loaded {
    let mut values = vec![
        r#"{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}"#
            .to_owned(),
    ];
    let mut dsts = vec![r#"{"id": "ex", "dst": [{"x": 0, "y": 0, "w": 12, "h": 20}]}"#.to_owned()];
    for (i, (id, prop, digit, zero)) in ROWS.iter().enumerate() {
        let padding = i32::from(*zero);
        values.push(format!(
            r#"{{"id": "{id}", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": {digit},
                 "padding": {padding}, {prop}}}"#
        ));
        let y = 50 * (i + 1);
        dsts.push(format!(
            r#"{{"id": "{id}", "dst": [{{"x": 0, "y": {y}, "w": 12, "h": 20}}]}}"#
        ));
    }
    import_body(&format!(
        r#""value": [{}], "destination": [{}]"#,
        values.join(","),
        dsts.join(",")
    ))
}

/// Each row's digits as drawn at `ms`, read through the EX score's
/// glyphs; a blank cell or an undrawn row is empty.
fn rows(d: &mut Drawn, ms: i64) -> Vec<String> {
    let glyph = d.ex_glyphs(ms);
    let q = d.frame(ms, &[]);
    // Rows by their top, from the EX score's (the lowest on the canvas).
    let mut rows = vec![String::new(); ROWS.len()];
    let mut cells: Vec<_> = q.iter().map(|q| (q.rect[1], q.rect[0], q.uv)).collect();
    cells.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.total_cmp(&b.1)));
    let ex_top = cells.first().unwrap().0;
    for (y, uv) in cells.iter().map(|c| (c.0, c.2)) {
        if y == ex_top {
            continue;
        }
        // 50 px a row, upwards.
        let row = ((ex_top - y) / 50.0).round() as usize - 1;
        rows[row].push(glyph[&uv.map(f32::to_bits)]);
    }
    rows
}

/// The chart: a tap at 2:05.
fn chart() -> bms_model::Chart {
    ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(125_000 * MS))
        .build()
        .unwrap()
}

const KNOWN: FrameReadouts = FrameReadouts {
    bpm_current_x100: Some(15_075),
    hispeed_x100: Some(105),
    duration: Some(DurationRatio {
        numerator: 6_675,
        denominator: 10,
    }),
    // Both off, their amounts stored; the lanes draw none of them.
    covers: CoverSettings {
        cover: 500,
        lift: 250,
        hidden: 120,
        cover_enabled: false,
        lift_enabled: false,
        hidden_enabled: false,
        adjusting: false,
    },
};

#[test]
fn readouts_show_their_fields() {
    let loaded = readouts();
    // One D4 note at each readout number, nothing else.
    let notes = &loaded.report().notes;
    assert_eq!(notes.len(), ROWS.len(), "{notes:?}");
    assert!(notes.iter().all(|n| n.kind == D4), "{notes:?}");
    let range = BpmRange {
        min_x100: 7_599,
        max_x100: 30_000,
    };
    let mut d = Drawn::over_with(loaded.ok().skin.clone(), &chart(), Some(range));
    d.readouts = Some(KNOWN);
    #[rustfmt::skip]
    let at_start = [
        "300", "75", "150", "0", "00", "2", "05",
        "500", "250", "120", "375", "105", "1", "05", "668", "400",
    ];
    assert_eq!(rows(&mut d, 0), at_start);
    // At 61.5 s: 61 s played, 63.5 s left, shown as 64.
    let later = rows(&mut d, 61_500);
    assert_eq!(&later[3..7], ["1", "01", "1", "04"]);
    // Unknown values draw nothing, the clocks and covers still do.
    let mut d = Drawn::over(d.skin, &chart());
    d.readouts = Some(FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        ..KNOWN
    });
    #[rustfmt::skip]
    let unknown = [
        "", "", "", "0", "00", "2", "05",
        "500", "250", "120", "375", "", "", "", "", "",
    ];
    assert_eq!(rows(&mut d, 0), unknown);
    // A duration without a hi-speed: the duration draws, the hi-speed not.
    d.readouts = Some(FrameReadouts {
        hispeed_x100: None,
        ..KNOWN
    });
    let r = rows(&mut d, 0);
    assert_eq!(&r[11..], ["", "", "", "668", "400"]);
    // A present 0 is drawn.
    d.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(0),
        hispeed_x100: Some(0),
        ..KNOWN
    });
    let r = rows(&mut d, 0);
    assert_eq!(r[2], "0");
    assert_eq!(&r[11..14], ["0", "0", "00"]);
}

#[test]
fn readout_numbers_are_decorative() {
    // An undefined op drops each with a note; nothing is refused.
    for (_, prop, ..) in ROWS {
        let loaded = import_body(&format!(
            r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                          {prop}}}],
               "destination": [{{"id": "v", "op": [7777], "dst": [{{"w": 12, "h": 20}}]}}]"#
        ));
        assert!(has(loaded.report(), NoteKind::UndefinedOp(7_777)), "{prop}");
    }
}

/// One number of `props`.
fn number(props: &str) -> common::Loaded {
    import_body(&format!(
        r#""value": [{{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 4,
                      {props}}}],
           "destination": [{{"id": "v", "dst": [{{"w": 12, "h": 20}}]}}]"#
    ))
}

#[test]
fn ids_without_these_contracts_stay_unsupported() {
    for id in [92, 1_163, 1_164, 1_312, 1_327, 372, 373] {
        let loaded = number(&format!(r#""ref": {id}"#));
        let why = NoteKind::UnsupportedProperty {
            kind: PropertyKind::Integer,
            id: Some(id),
        };
        assert!(has(loaded.report(), why), "{id}");
    }
    // 15 and 131 are no value properties: a value of either falls back to
    // the `ref`, and a `ref` of either draws nothing.
    for id in [15, 131] {
        let loaded = number(&format!(r#""value": {id}, "ref": 71"#));
        assert!(loaded.report().notes.is_empty(), "{id}");
        assert_eq!(loaded.drawn().frame(0, &[]).len(), 1, "{id}");
        let loaded = number(&format!(r#""ref": {id}"#));
        assert!(
            has(loaded.report(), NoteKind::Ignored { field: "value" }),
            "{id}"
        );
    }
}

/// An image under `op`.
fn image(op: &str) -> common::Loaded {
    import_body(&format!(
        r#""image": [{{"id": "i", "src": "a", "w": 64, "h": 64}}],
           "destination": [{{"id": "i", "op": [{op}], "dst": [{{"w": 10, "h": 10}}]}}]"#
    ))
}

#[test]
fn cover_conditions_read_the_stored_switches() {
    let covers = |cover_enabled, lift_enabled, hidden_enabled, adjusting| FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        covers: CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled,
            lift_enabled,
            hidden_enabled,
            adjusting,
        },
    };
    for (op, d4) in [
        ("270", true),
        (r#""lanecover1_changing""#, true),
        ("271", false),
        (r#""!!lanecover1_on""#, false),
        ("272", false),
        (r#""lift1_on""#, false),
        ("273", false),
        (r#""hidden1_on""#, false),
    ] {
        let loaded = image(op);
        let want: &[NoteKind] = if d4 { &[D4] } else { &[] };
        assert_eq!(common::kinds(loaded.report()), want, "{op}");
        let mut d = loaded.drawn();
        let id = match op.trim_matches('"').trim_start_matches('!') {
            "lanecover1_changing" => "270",
            "lanecover1_on" => "271",
            "lift1_on" => "272",
            "hidden1_on" => "273",
            n => n,
        };
        // On at 0 amount is on; each switch alone turns its condition on.
        for (frame, r) in [
            (0, covers(false, false, false, false)),
            (1, covers(true, false, false, false)),
            (2, covers(false, true, false, false)),
            (3, covers(false, false, true, false)),
            (4, covers(false, false, false, true)),
        ] {
            d.readouts = Some(r);
            let shown = match id {
                "270" => frame == 4,
                "271" => frame == 1,
                "272" => frame == 2,
                _ => frame == 3,
            };
            assert_eq!(d.frame(16 * frame, &[]).len(), usize::from(shown), "{op}");
        }
        // Negated by sign or `!`.
        let negated = op
            .strip_prefix('"')
            .map_or_else(|| format!("-{op}"), |rest| format!(r#""!{rest}"#));
        let mut d = image(&negated).drawn();
        d.readouts = Some(covers(true, true, true, true));
        assert!(d.frame(0, &[]).is_empty(), "{negated}");
        d.readouts = Some(covers(false, false, false, false));
        assert_eq!(d.frame(16, &[]).len(), 1, "{negated}");
    }
}
