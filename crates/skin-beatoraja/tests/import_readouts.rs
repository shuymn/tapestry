//! Play readouts: the BPM, time, cover, hi-speed and duration numbers by
//! id and name, guarded while unknown, the cover conditions, and the ids
//! that stay unsupported or null; the duration variants 1312 to 1327.

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
    numbers_of(&ROWS)
}

/// An EX score of one cell at y = 0, then `rows` from y = 50.
fn numbers_of(rows: &[(&str, &str, i32, bool)]) -> common::Loaded {
    let mut values = vec![
        r#"{"id": "ex", "src": "n", "w": 120, "h": 20, "divx": 10, "digit": 1, "ref": 71}"#
            .to_owned(),
    ];
    let mut dsts = vec![r#"{"id": "ex", "dst": [{"x": 0, "y": 0, "w": 12, "h": 20}]}"#.to_owned()];
    for (i, (id, prop, digit, zero)) in rows.iter().enumerate() {
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

/// Each of [`ROWS`]' digits as drawn at `ms`, read through the EX score's
/// glyphs; a blank cell or an undrawn row is empty.
fn rows(d: &mut Drawn, ms: i64) -> Vec<String> {
    rows_of(d, ms, ROWS.len())
}

/// Each of `n` rows' digits as drawn at `ms`, as [`rows`].
fn rows_of(d: &mut Drawn, ms: i64, n: usize) -> Vec<String> {
    let glyph = d.ex_glyphs(ms);
    let q = d.frame(ms, &[]);
    // Rows by their top, from the EX score's (the lowest on the canvas).
    let mut rows = vec![String::new(); n];
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
    ..FrameReadouts::UNAVAILABLE
};

/// `n / d` ms.
const fn ratio(n: u128, d: u128) -> Option<DurationRatio> {
    Some(DurationRatio {
        numerator: n,
        denominator: d,
    })
}

/// 1312 to 1327 by numeric id, 10 cells each, unpadded, in id order.
fn variants() -> common::Loaded {
    let ids: Vec<(String, String)> = (1_312..=1_327)
        .map(|id| (format!("v{id}"), format!(r#""ref": {id}"#)))
        .collect();
    let rows: Vec<(&str, &str, i32, bool)> = ids
        .iter()
        .map(|(id, prop)| (id.as_str(), prop.as_str(), 10, false))
        .collect();
    numbers_of(&rows)
}

/// The variants Classic `H` 2.00 gives with no lift and SUDDEN+ 250 ‰
/// configured, at the current, MAIN, minimum and maximum tempos `bpms`
/// (`None` unknown): `240000 · v / (B · H)` ms, `v` 3/4 with the cover
/// and 1 without, so `90000 / B` and `120000 / B`.
fn classic_2_variants(bpms: [Option<u128>; 4]) -> [Option<DurationRatio>; 8] {
    let mut out = [None; 8];
    for (pair, b) in out.chunks_mut(2).zip(bpms) {
        if let Some(b) = b {
            pair[0] = ratio(90_000, b);
            pair[1] = ratio(120_000, b);
        }
    }
    out
}

/// 1312 to 1327 draw their variants' ms and green numbers, each rounded
/// half up from the exact ratio, every one of the 16 distinct at current
/// 150, MAIN 125, MIN 96 and MAX 240 BPM (MIN rounding half up in both),
/// each under its own validity; each is a D4 number.
#[test]
fn all_sixteen_variants_draw_distinct_values_with_independent_validity() {
    let loaded = variants();
    let notes = &loaded.report().notes;
    assert_eq!(notes.len(), 16, "{notes:?}");
    assert!(notes.iter().all(|n| n.kind == D4), "{notes:?}");
    let mut d = Drawn::over(loaded.ok().skin.clone(), &chart());
    let all = [Some(150), Some(125), Some(96), Some(240)];
    d.readouts = Some(FrameReadouts {
        duration_variants: classic_2_variants(all),
        ..KNOWN
    });
    #[rustfmt::skip]
    let want = [
        "600", "360", "800", "480",
        "720", "432", "960", "576",
        "938", "563", "1250", "750",
        "375", "225", "500", "300",
    ];
    let drawn = rows_of(&mut d, 0, 16);
    assert_eq!(drawn, want);
    let mut distinct = drawn.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 16);
    // References unavailable (a later tempo without an exact ratio): MAIN,
    // MIN and MAX draw nothing, the current tempo's four still draw.
    d.readouts = Some(FrameReadouts {
        duration_variants: classic_2_variants([Some(150), None, None, None]),
        ..KNOWN
    });
    #[rustfmt::skip]
    let current_only = [
        "600", "360", "800", "480",
        "", "", "", "", "", "", "", "", "", "", "", "",
    ];
    assert_eq!(rows_of(&mut d, 16, 16), current_only);
    // Classic `H` 0.10 with MIN at 0.001 BPM: covered, 240000 · 0.75 /
    // (0.001 · 0.1) = 1 800 000 000 ms (green 1 080 000 000) is within the
    // readout maximum; uncovered, 2 400 000 000 ms is past it, which the
    // producer leaves unavailable, and it draws nothing.
    let mut slow = classic_2_variants(all);
    slow[4] = ratio(1_800_000_000, 1);
    slow[5] = None;
    d.readouts = Some(FrameReadouts {
        duration_variants: slow,
        ..KNOWN
    });
    let r = rows_of(&mut d, 32, 16);
    assert_eq!(&r[8..12], ["1800000000", "1080000000", "", ""]);
    assert_eq!(&r[..8], &want[..8]);
    assert_eq!(&r[12..], &want[12..]);
}

/// LIFT 300 ‰ and SUDDEN+ 200 ‰ at 150 BPM in Floating at G 300: the
/// active `H` is 224/125 and the source's `Hs = H / (1 − l)` 2.56, so 10
/// draws 256, 310 and 311 2 and 56; the duration, `240000 · 0.8 / (150 ·
/// 2.56)`, is 500 ms (green 300); 14, 314 and 316 draw the configured
/// 200, 300 and `⌊700 · 200 / 1 000⌋` = 140.
#[test]
fn lift_and_sudden_draw_10_14_312_314_316() {
    let mut d = Drawn::over(readouts().ok().skin.clone(), &chart());
    (d.view.cover, d.view.lift) = (200, 300);
    d.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(15_000),
        hispeed_x100: Some(256),
        duration: ratio(500, 1),
        covers: CoverSettings {
            cover: 200,
            lift: 300,
            hidden: 0,
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: false,
            adjusting: false,
        },
        ..FrameReadouts::UNAVAILABLE
    });
    let r = rows(&mut d, 0);
    assert_eq!(
        &r[7..],
        ["200", "300", "0", "140", "256", "2", "56", "500", "300"]
    );
}

/// LIFT at 1 000 ‰ leaves no field: the source hi-speed and every number
/// depending on it (10, 310, 311, 312, 313) draw nothing, the configured
/// whites (14, 314, 315, 316) still draw.
#[test]
fn lift_1000_hides_hs_numbers_keeps_whites() {
    let mut d = Drawn::over(readouts().ok().skin.clone(), &chart());
    (d.view.cover, d.view.lift, d.view.hidden) = (200, 1_000, 300);
    d.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(15_000),
        covers: CoverSettings {
            cover: 200,
            lift: 1_000,
            hidden: 300,
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
        ..FrameReadouts::UNAVAILABLE
    });
    let r = rows(&mut d, 0);
    assert_eq!(&r[7..], ["200", "1000", "300", "0", "", "", "", "", ""]);
}

/// 316 is the configured lane cover over the lane above the lift the
/// lanes draw: 500 and 250 stored, it reads 500, as 14 does, while LIFT
/// is off and `⌊750 · 500 / 1 000⌋` = 375 while it is on, whatever
/// SUDDEN+'s switch.
#[test]
fn number_316_follows_the_lift_drawn() {
    let loaded = numbers_of(&[
        ("cov", r#""ref": 14"#, 4, false),
        ("abs", r#""ref": 316"#, 4, false),
    ]);
    let mut d = Drawn::over(loaded.ok().skin.clone(), &chart());
    for (ms, sudden, lift, want) in [
        (0, false, false, ["500", "500"]),
        (16, true, false, ["500", "500"]),
        (32, false, true, ["500", "375"]),
        (48, true, true, ["500", "375"]),
    ] {
        d.view.cover = if sudden { 500 } else { 0 };
        d.view.lift = if lift { 250 } else { 0 };
        d.readouts = Some(FrameReadouts {
            covers: CoverSettings {
                cover_enabled: sudden,
                lift_enabled: lift,
                ..KNOWN.covers
            },
            ..KNOWN
        });
        assert_eq!(
            rows_of(&mut d, ms, 2),
            want,
            "SUDDEN+ {sudden}, LIFT {lift}"
        );
    }
}

/// 502.5 ms: 312 rounds it half up to 503, 313 takes the source's order,
/// ⌊3 · 503 / 5⌋ = 301, and 1313 the green number half up from the exact
/// ratio, 301.5 → 302 (design.md "Play readouts").
#[test]
fn numbers_312_313_1313_show_503_301_302() {
    let loaded = numbers_of(&[
        ("dur", r#""ref": 312"#, 4, false),
        ("grn", r#""ref": 313"#, 4, false),
        ("var", r#""ref": 1313"#, 4, false),
    ]);
    let mut d = Drawn::over(loaded.ok().skin.clone(), &chart());
    let mut variants = [None; 8];
    variants[0] = ratio(1_005, 2);
    d.readouts = Some(FrameReadouts {
        duration: ratio(1_005, 2),
        duration_variants: variants,
        ..KNOWN
    });
    assert_eq!(rows_of(&mut d, 0, 3), ["503", "301", "302"]);
}

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
    let mut d = Drawn::over_with(loaded.ok().skin.clone(), &chart(), Some(range), None);
    d.readouts = Some(KNOWN);
    #[rustfmt::skip]
    let at_start = [
        "300", "75", "150", "0", "00", "2", "05",
        "500", "250", "120", "500", "105", "1", "05", "668", "400",
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
        "500", "250", "120", "500", "", "", "", "", "",
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

/// `ref` 96 shows the chart's play level with no deviation, a negative as
/// its magnitude on a ten-glyph strip, and draws nothing while it is
/// unknown or what a number reserves.
#[test]
fn the_play_level_shows_under_its_availability() {
    let loaded = number(r#""ref": 96"#);
    assert!(
        loaded.report().notes.is_empty(),
        "{:?}",
        loaded.report().notes
    );
    let skin = loaded.ok().skin.clone();
    let drawn = |level| {
        let mut d = Drawn::over_level(skin.clone(), &chart(), level);
        d.frame(0, &[]).len()
    };
    assert_eq!(drawn(Some(12)), 2);
    assert_eq!(drawn(Some(0)), 1);
    assert_eq!(drawn(Some(-3)), 1);
    for unknown in [None, Some(i32::MIN), Some(i32::MAX)] {
        assert_eq!(drawn(unknown), 0, "{unknown:?}");
    }
    assert_eq!(drawn(Some(-i32::MAX)), 4);
    assert_eq!(drawn(Some(i32::MAX - 1)), 4);
    let by_name = number(r#""value": "playlevel""#);
    assert!(by_name.report().notes.is_empty());
}

/// On a 24-glyph strip a negative play level takes the second half's digits
/// and sign, and a positive one the first half's: the sign is kept, not only
/// the magnitude.
#[test]
fn a_negative_play_level_shows_its_sign_and_digits() {
    let loaded = import_body(
        r#""value": [{"id": "v", "src": "n", "w": 120, "h": 20, "divx": 24, "digit": 4,
                      "ref": 96}],
           "destination": [{"id": "v", "dst": [{"w": 12, "h": 20}]}]"#,
    );
    let skin = loaded.ok().skin.clone();
    // Each drawn cell's index in the uv table, left to right.
    let cells = |level| {
        let mut d = Drawn::over_level(skin.clone(), &chart(), Some(level));
        let mut quads = d.frame(0, &[]);
        quads.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        quads
            .iter()
            .map(|q| {
                d.uvs
                    .frames()
                    .iter()
                    .position(|f| f.uv == q.uv && f.layer == q.layer)
                    .expect("a frame of the sheet")
            })
            .collect::<Vec<_>>()
    };
    // The first half: digits from 0, the sign at 11.
    let zero = cells(0)[1];
    assert_eq!(cells(3), [zero + 11, zero + 3]);
    assert_eq!(cells(12), [zero + 11, zero + 1, zero + 2]);
    // The second half: digits from 12, the sign at 23.
    assert_eq!(cells(-3), [zero + 23, zero + 15]);
    assert_eq!(cells(-12), [zero + 23, zero + 13, zero + 14]);
}

#[test]
fn ids_without_these_contracts_stay_unsupported() {
    for id in [372, 373] {
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
        covers: CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled,
            lift_enabled,
            hidden_enabled,
            adjusting,
        },
        ..FrameReadouts::UNAVAILABLE
    };

    for (op, d4) in [
        ("270", false),
        (r#""lanecover1_changing""#, false),
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

/// The chart's BPM-change conditions, by number or name and either sign,
/// each with one D4 note, read the play context's fact: 176 while it says
/// no change, 177 while it says a change, neither while it is unknown,
/// where the negations draw.
#[test]
fn bpm_change_conditions_read_the_charts_fact() {
    let chart = ChartBuilder::new(150_000).build().unwrap();
    // Shown with no change, a change, and an unknown fact.
    for (op, shown) in [
        ("176", [true, false, false]),
        ("-176", [false, true, true]),
        ("177", [false, true, false]),
        ("-177", [true, false, true]),
        (r#""chart_bpmchange""#, [false, true, false]),
        (r#""!chart_no_bpmchange""#, [false, true, true]),
    ] {
        let loaded = image(op);
        assert_eq!(common::kinds(loaded.report()), [D4], "{op}");
        for (fact, shown) in [Some(false), Some(true), None].into_iter().zip(shown) {
            let mut d = Drawn::over_with(loaded.ok().skin.clone(), &chart, None, fact);
            assert_eq!(
                d.frame(0, &[]).len(),
                usize::from(shown),
                "{op} at {fact:?}"
            );
        }
    }
}
