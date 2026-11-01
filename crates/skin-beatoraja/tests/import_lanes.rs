//! The lanes: the lane map and scratch side, the rects and the vertical
//! mapping, the preconditions, the parts, the lifetime, the offsets, the
//! bar line and the lane approximations, through `build`.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, TimeUs};
use common::lanes::{NOTES, body, cover, eight, lane_rects, lift_object, note, note_with};
use common::play::{Drawn, MS, judged, round};
use common::{Loaded, has, import_body, import_config, import_sized};
use eval::{JudgeEvent, JudgeEventKind};
use rules::Judgment;
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, LaneApprox, NoteKind, OffsetValue, Severity,
    TrackWhy,
};
use skin_model::{LanesError, LoadError, Side};

/// The lanes skin with `note_extra` note fields and `dsts` after the
/// note destination.
fn lanes(note_extra: &str, dsts: &[&str]) -> Loaded {
    let mut all = vec![NOTES];
    all.extend_from_slice(dsts);
    import_body(&body("", &note(note_extra), &all))
}

/// The lanes skin whose note destination is `notes`.
fn lanes_dst(notes: &str) -> Loaded {
    import_body(&body("", &note(""), &[notes]))
}

/// A chart of taps, each a model lane and a time in ms.
fn taps(list: &[(u8, i64)]) -> Chart {
    list.iter()
        .fold(ChartBuilder::new(150_000), |b, &(l, ms)| {
            b.tap(Lane::new(l).unwrap(), TimeUs(ms * MS))
        })
        .build()
        .unwrap()
}

/// A tap on model lane `l` at `ms`.
fn tap(l: u8, ms: i64) -> Chart {
    taps(&[(l, ms)])
}

/// The rects at `ms` of `loaded` played over `chart`.
fn rects_over(loaded: &Loaded, chart: &Chart, ms: i64) -> Vec<[i32; 4]> {
    drawn(loaded, chart).rects(ms)
}

fn drawn(loaded: &Loaded, chart: &Chart) -> Drawn {
    Drawn::over(loaded.ok().skin.clone(), chart)
}

#[track_caller]
fn critical(l: Loaded) -> (CriticalPart, NoteKind) {
    match l.err() {
        ImportError::Critical { part, why, .. } => (part, why),
        e => panic!("{e:?}"),
    }
}

#[track_caller]
fn refused(l: Loaded, field: &'static str) {
    assert_eq!(
        critical(l),
        (CriticalPart::Lanes, NoteKind::UnsupportedField { field })
    );
}

/// The model's lanes error and the line it is located at.
#[track_caller]
fn model(l: Loaded) -> (LanesError, u32) {
    match l.err() {
        ImportError::Model {
            at: Some(at),
            source: LoadError::Lanes(e),
        } => (e, at.line),
        e => panic!("{e:?}"),
    }
}

/// The source's lanes with lane `i` replaced by `rect`.
fn rects_with(i: usize, rect: &str) -> String {
    let mut v: Vec<String> = lane_rects(200, 600)
        .split("}, ")
        .map(|s| s.trim_end_matches('}').to_owned() + "}")
        .collect();
    v[i] = rect.to_owned();
    v.join(", ")
}

#[test]
fn lanes_import_with_their_approximations() {
    let l = lanes(r#""size": [20, 20, 20, 20, 20, 20, 20, 20]"#, &[]);
    let r = l.report();
    for a in [LaneApprox::TopCrop, LaneApprox::LnBody] {
        assert!(has(r, NoteKind::Deviation(DeviationId::D9(a))), "{a:?}");
    }
    for a in [LaneApprox::Dst2, LaneApprox::Expansion, LaneApprox::BarLine] {
        assert!(!has(r, NoteKind::Deviation(DeviationId::D9(a))), "{a:?}");
    }
    assert_eq!(l.ok().scratch, Some(Side::Left));
    // Six distinct part images, one synthesized key.
    assert_eq!(r.usage.images, 6);
    assert_eq!(r.usage.keys, 1);
    // `dst2` and an expansion are approximations too.
    let l = lanes(r#""dst2": 900, "expansionrate": [120, 100]"#, &[]);
    for a in [LaneApprox::Dst2, LaneApprox::Expansion] {
        assert!(has(l.report(), NoteKind::Deviation(DeviationId::D9(a))));
    }
    let l = lanes(r#""expansionrate": [100, 100]"#, &[]);
    let d9 = NoteKind::Deviation(DeviationId::D9(LaneApprox::Expansion));
    assert!(!has(l.report(), d9));
}

#[test]
fn lanes_map_source_seven_to_the_scratch() {
    // r = 3/2: the keys from 150, 75 wide; the scratch at 30, 120 wide.
    let l = import_sized(
        [1_280, 720],
        &body(
            "",
            &note_with(
                &lane_rects(200, 400),
                r#""size": [13, 13, 13, 13, 13, 13, 13, 13]"#,
            ),
            &[NOTES],
        ),
        &[],
    );
    let chart = taps(&[(0, 1_000), (1, 1_000), (7, 1_000)]);
    let mut r = rects_over(&l, &chart, 1_000);
    r.sort_unstable();
    assert_eq!(
        r,
        [[30, 761, 120, 20], [150, 761, 75, 20], [600, 761, 75, 20]]
    );
    assert_eq!(l.ok().scratch, Some(Side::Left));
    // The scratch right of the keys.
    let right = rects_with(7, r#"{"x": 450, "y": 200, "w": 80, "h": 600}"#);
    let l = import_body(&body("", &note_with(&right, ""), &[NOTES]));
    assert_eq!(l.ok().scratch, Some(Side::Right));
    // Played with the scratch on the left, the model mirrors the lanes.
    assert_eq!(rects_over(&l, &tap(0, 1_000), 1_000), [[100, 860, 80, 20]]);
    // The scratch between the keys: the model's refusal, at the note.
    let mut v: Vec<String> = (0..7)
        .map(|i| {
            let x = if i < 3 { 100 + 50 * i } else { 180 + 50 * i };
            format!(r#"{{"x": {x}, "y": 200, "w": 50, "h": 600}}"#)
        })
        .collect();
    v.push(r#"{"x": 250, "y": 200, "w": 80, "h": 600}"#.to_owned());
    let l = import_body(&body("", &note_with(&v.join(", "), ""), &[NOTES]));
    assert_eq!(model(l), (LanesError::ScratchNotAtSide, 20));
}

#[test]
fn overlapping_lanes_refuse() {
    // One pixel into the first key lane.
    let wide = rects_with(7, r#"{"x": 20, "y": 200, "w": 81, "h": 600}"#);
    refused(
        import_body(&body("", &note_with(&wide, ""), &[NOTES])),
        "dst",
    );
    // Touching lanes import (the default lanes touch).
    lanes("", &[]).ok();
    // A configured width that makes each lane reach into the next.
    let header = r#""offset": [{"name": "w", "id": 40, "x": true, "w": true}]"#;
    let objects = format!("{header}, {}", note(""));
    let notes = r#"{"id": "notes", "offset": 40, "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    let with = |ov: OffsetValue| {
        import_config(
            [1_920, 1_080],
            &body("", &objects, &[notes]),
            &[],
            |c: &mut Config| {
                c.offsets.insert(40, ov);
            },
        )
    };
    let wide = with(OffsetValue {
        w: 1,
        ..OffsetValue::default()
    });
    refused(wide, "dst");
    // The offsets apply as direct deltas: x moves, w widens to the right.
    let moved = with(OffsetValue {
        x: 10,
        w: -4,
        ..OffsetValue::default()
    });
    assert_eq!(
        rects_over(&moved, &tap(1, 1_000), 1_000),
        [[110, 860, 46, 20]]
    );
}

#[test]
fn the_vertical_mapping_follows_the_source() {
    // r = 3/2, y 200, h 400, size 13: top 170, line 770, bottom 780, a
    // region 600 high and notes 19.5 high, kept unrounded: at its
    // judgment time a note spans 760.5–780 (rounded here: 761, 20 high).
    let objects = |extra: &str| {
        note_with(
            &lane_rects(200, 400),
            &format!(r#""size": [13, 13, 13, 13, 13, 13, 13, 13]{extra}"#),
        )
    };
    let at =
        |objects: String, dsts: &[&str]| import_sized([1_280, 720], &body("", &objects, dsts), &[]);
    let l = at(objects(""), &[NOTES]);
    // A note on the line, and one 600 ms ahead with its lower edge on the
    // region's top, cut at the lanes' top (D9 TopCrop).
    let chart = taps(&[(1, 1_000), (2, 1_600)]);
    assert_eq!(
        rects_over(&l, &chart, 1_000),
        [[150, 761, 75, 20], [225, 170, 75, 10]]
    );
    // Nothing is drawn below the line.
    assert_eq!(
        rects_over(&l, &tap(1, 1_000), 1_005),
        Vec::<[i32; 4]>::new()
    );
    // SUDDEN+ anchors at the region's top (180), LIFT at the bottom
    // (780): the lift of 100 ‰ takes 60 px from 780, and a cover of
    // 500 ‰ lowers SUDDEN+'s edge by half of the 540 px left, to 450;
    // the cover, 600 high, shows from the lanes' top.
    let sudden =
        r#"{"id": "cover", "offset": 4, "dst": [{"x": 20, "y": 600, "w": 430, "h": 400}]}"#;
    let lift_dst = r#"{"id": "liftc", "dst": [{"x": 20, "y": 0, "w": 430, "h": 200}]}"#;
    let objs = format!("{}, {}", objects(""), lift_object(""));
    let l = at(objs, &[NOTES, sudden, lift_dst]);
    let mut d = drawn(&l, &tap(1, 5_000));
    d.view.cover = 500;
    d.view.lift = 100;
    let r = d.rects(1_000);
    assert_eq!(r, [[30, 170, 645, 280], [30, 720, 645, 60]]);
    // Without them, 100 ‰ of lift still raises the line by 60 px of the
    // region: a note's lower edge on 720.
    let l = at(objects(""), &[NOTES]);
    let mut d = drawn(&l, &tap(1, 1_000));
    d.view.lift = 100;
    assert_eq!(d.rects(1_000), [[150, 701, 75, 20]]);
}

#[test]
fn note_heights_come_from_size_or_the_cell() {
    // A 40 px tall note of two rows: one row, 20 px; size overrides.
    let tall = r#"{"id": "nt", "src": "a", "x": 0, "y": 0, "w": 8, "h": 40, "divy": 2}"#;
    let objects = note("").replace(r#""note": ["nt""#, r#""note": ["tall""#);
    let objects = objects.replace(r#", "nt""#, r#", "tall""#);
    let tall = tall.replace(r#""id": "nt""#, r#""id": "tall""#);
    let l = import_body(&body(&tall, &objects, &[NOTES]));
    assert_eq!(rects_over(&l, &tap(1, 1_000), 1_000), [[100, 860, 50, 20]]);
    // Every lane's size is one height, kept unrounded: unequal sizes
    // refuse, even below a pixel.
    let sizes = |s: &str| lanes(&format!(r#""size": [{s}]"#), &[]);
    let l = sizes("20.5, 20.5, 20.5, 20.5, 20.5, 20.5, 20.5, 20.5");
    let q = drawn(&l, &tap(1, 1_000)).frame(1_000, &[]);
    assert_eq!(
        q.iter().map(|q| q.rect).collect::<Vec<_>>(),
        [[100.0, 859.5, 50.0, 20.5]]
    );
    refused(sizes("20, 20, 20, 20, 20, 20, 20, 20.4"), "size");
    refused(sizes("20, 20, 20, 20, 20, 20, 20, 21"), "size");
    refused(sizes("0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4, 0.4"), "size");
    // −2^127 as `f32`, the rationals' negative boundary, and beyond.
    for v in ["-1.7014118346046923e38", "-1.7014119e38", "-3.4e38"] {
        let l = sizes(&format!("{v}, 20, 20, 20, 20, 20, 20, 20"));
        assert!(
            matches!(l.err(), ImportError::Range { field: "size", .. }),
            "{v}"
        );
    }
    // A note taller than the lane still fits above its bottom: it loads,
    // with SUDDEN+ or without, the covers free to hide the whole lane.
    for covers in [&[][..], &[cover("")][..]] {
        let dsts: Vec<&str> = covers.iter().map(String::as_str).collect();
        lanes(
            r#""size": [1140, 1140, 1140, 1140, 1140, 1140, 1140, 1140]"#,
            &dsts,
        )
        .ok();
    }
}

#[test]
fn lane_rows_must_share_their_y() {
    let off = rects_with(3, r#"{"x": 250, "y": 201, "w": 50, "h": 600}"#);
    refused(
        import_body(&body("", &note_with(&off, ""), &[NOTES])),
        "dst",
    );
    // Each lane's height is unused.
    let short = rects_with(3, r#"{"x": 250, "y": 200, "w": 50, "h": 10}"#);
    import_body(&body("", &note_with(&short, ""), &[NOTES])).ok();
}

#[test]
fn the_part_lists_are_checked() {
    let all = lane_rects(200, 600);
    let seven: Vec<&str> = all.rsplitn(2, "}, ").collect();
    let seven = seven[1].to_owned() + "}";
    refused(
        import_body(&body("", &note_with(&seven, ""), &[NOTES])),
        "dst",
    );
    let with = |from: &str, to: &str| {
        let n = note("");
        assert!(n.contains(from), "{from}");
        import_body(&body("", &n.replacen(from, to, 1), &[NOTES]))
    };
    let short = format!(r#""hcnbody": {}"#, eight("lb"));
    refused(with(&short, r#""hcnbody": ["lb"]"#), "lnend");
    let notes = eight("nt");
    let bad = notes.replacen(r#""nt", "nt", "nt", "nt""#, r#""nt", "nt", "nt", "no""#, 1);
    refused(
        with(&format!(r#""note": {notes}"#), &format!(r#""note": {bad}"#)),
        "note",
    );
    let mines = format!(r#""mine": {}, "#, eight("mi"));
    refused(with(&mines, ""), "mine");
    // An image of the empty id resolves an empty name, never a missing
    // entry or a `null` one.
    let empty = r#"{"id": "", "src": "a", "x": 40, "y": 0, "w": 8, "h": 20}"#;
    let with_empty = |to: &str| {
        let n = note("").replacen(&mines, to, 1);
        import_body(&body(empty, &n, &[NOTES]))
    };
    with_empty(&format!(r#""mine": {}, "#, eight(""))).ok();
    refused(with_empty(""), "mine");
    let seven = r#""mine": ["", "", "", "", "", "", ""], "#;
    refused(with_empty(seven), "mine");
    let null = r#""mine": ["", "", "", "", "", "", "", null], "#;
    refused(with_empty(null), "mine");
    let ends = format!(r#""lnend": {}"#, eight("le"));
    refused(with(&ends, r#""lnends": []"#), "lnend");
}

#[test]
fn bodies_follow_the_active_branch() {
    // With `lnbodyActive`, the body pair is (lnbodyActive, lnbody);
    // otherwise (lnbody, lnactive). The other three distinct images
    // count either way.
    let l = lanes("", &[]);
    assert_eq!(l.report().usage.images, 6);
    let l = lanes(&format!(r#""lnbodyActive": {}"#, eight("deco")), &[]);
    assert_eq!(l.report().usage.images, 6);
    // A held long note draws the active body, an idle one the other.
    let chart = ChartBuilder::new(150_000)
        .long(
            Lane::new(1).unwrap(),
            TimeUs(1_000 * MS),
            TimeUs(1_300 * MS),
        )
        .build()
        .unwrap();
    let uvs = |l: &Loaded, held: bool| {
        let mut d = drawn(l, &chart);
        let ev = JudgeEvent {
            seq: 0,
            time: TimeUs(1_000 * MS),
            origin: None,
            kind: JudgeEventKind::LnHeld {
                lane: Lane::new(1).unwrap(),
                note: 0,
                judgment: Judgment::Perfect,
                deviation: TimeUs(0),
            },
        };
        let events = if held { vec![ev] } else { Vec::new() };
        d.frame(1_000, &events)
            .iter()
            .map(|q| q.uv)
            .collect::<Vec<_>>()
    };
    let plain = lanes("", &[]);
    let active = lanes(&format!(r#""lnbodyActive": {}"#, eight("deco")), &[]);
    assert_ne!(uvs(&plain, true), uvs(&plain, false));
    assert_ne!(uvs(&plain, true), uvs(&active, true));
    assert_eq!(uvs(&plain, true), uvs(&active, false));
}

#[test]
fn the_lanes_track_is_neutral() {
    // A coloured, moved note destination draws white at the lane rects.
    let notes = r#"{"id": "notes", "dst": [{"x": 5, "y": 5, "w": 9, "h": 9, "a": 100, "r": 0, "g": 0, "b": 0}]}"#;
    let l = lanes_dst(notes);
    let q = drawn(&l, &tap(1, 1_000)).frame(1_000, &[]);
    assert_eq!(q.len(), 1);
    assert_eq!(round(q[0].rect), [100, 860, 50, 20]);
    assert_eq!(q[0].color, [1.0; 4]);
}

#[test]
fn the_lifetime_starts_at_zero() {
    let timed = r#"{"id": "notes", "timer": 41, "loop": -1, "dst": [
        {"time": 500, "x": 0, "y": 0, "w": 0, "h": 0},
        {"time": 1000, "x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    let l = lanes_dst(timed);
    assert_eq!(l.report().usage.keys, 2);
    let chart = tap(1, 1_400);
    // Shown from 0 (the constructor key), hidden after the last key.
    assert_eq!(rects_over(&l, &chart, 900), [[100, 360, 50, 20]]);
    assert_eq!(rects_over(&l, &chart, 1_001), Vec::<[i32; 4]>::new());
    // No key: shown from 0 for good.
    let l = lanes_dst(r#"{"id": "notes", "dst": []}"#);
    assert_eq!(l.report().usage.keys, 1);
    assert_eq!(rects_over(&l, &tap(1, 5_000), 5_000), [[100, 860, 50, 20]]);
    // Untimed and hidden after its keys: D3 refuses.
    let untimed = r#"{"id": "notes", "loop": -1, "dst": [
        {"time": 500, "x": 0, "y": 0, "w": 0, "h": 0},
        {"time": 1000, "x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    assert!(matches!(
        critical(lanes_dst(untimed)),
        (CriticalPart::Lanes, NoteKind::Timer(_))
    ));
    // A negative first key refuses.
    let early = r#"{"id": "notes", "dst": [{"time": -1, "x": 0, "y": 0, "w": 0, "h": 0}]}"#;
    assert_eq!(
        critical(lanes_dst(early)),
        (
            CriticalPart::Lanes,
            NoteKind::Track(TrackWhy::NegativeStart)
        )
    );
}

#[test]
fn runtime_offsets_on_the_lanes_refuse() {
    // Offset 4 on anything but an image is SUDDEN+'s refusal.
    for (id, part) in [
        (1, CriticalPart::Lanes),
        (3, CriticalPart::Lanes),
        (4, CriticalPart::Cover),
    ] {
        let notes = format!(
            r#"{{"id": "notes", "offset": {id}, "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}"#
        );
        assert_eq!(
            critical(lanes_dst(&notes)),
            (part, NoteKind::RuntimeOffset(id)),
            "{id}"
        );
    }
}

/// The note member with a `group` of `groups` destinations.
fn grouped(groups: &str) -> Loaded {
    lanes(&format!(r#""group": [{groups}]"#), &[])
}

const BAR: &str = r#"{"id": "bar", "dst": [{"x": 0, "y": 0, "w": 8, "h": 2}]}"#;

#[test]
fn a_plain_bar_maps() {
    let bar = |l: &Loaded| {
        let mut chart = tap(1, 5_000);
        chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
        drawn(l, &chart).rects(1_000)
    };
    let l = grouped(BAR);
    assert!(has(
        l.report(),
        NoteKind::Deviation(DeviationId::D9(LaneApprox::BarLine))
    ));
    assert_eq!(bar(&l), [[20, 869, 430, 2]]);
    // The model keeps a passed bar until it leaves the bottom.
    let mut chart = tap(1, 5_000);
    chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
    assert_eq!(drawn(&l, &chart).rects(1_001), [[20, 870, 430, 2]]);
    // During a STOP at the bar, it holds on the line.
    let mut chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .stop(TimeUs(1_000 * MS), TimeUs(500 * MS))
        .build()
        .unwrap();
    chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
    assert_eq!(drawn(&l, &chart).rects(1_400), [[20, 869, 430, 2]]);
    // Four frames with no cycle: frame 0 for good.
    let four = r#"{"id": "bar4", "src": "a", "x": 0, "y": 20, "w": 8, "h": 8, "divy": 4}"#;
    let l = import_body(&body(
        four,
        &note(&format!(
            r#""group": [{}]"#,
            BAR.replace("\"bar\"", "\"bar4\"")
        )),
        &[NOTES],
    ));
    assert_eq!(bar(&l), [[20, 869, 430, 2]]);
}

#[test]
fn other_bars_drop() {
    let cyc = r#"{"id": "bar4", "src": "a", "x": 0, "y": 20, "w": 8, "h": 8, "divy": 4, "cycle": 100, "timer": 41}"#;
    let dropped = |l: Loaded| {
        let r = l.report();
        assert!(
            r.notes.iter().any(|n| n.severity == Severity::Dropped
                && n.kind == NoteKind::UnsupportedField { field: "group" }),
            "{:?}",
            r.notes
        );
        assert!(!has(
            r,
            NoteKind::Deviation(DeviationId::D9(LaneApprox::BarLine))
        ));
    };
    dropped(import_body(&body(
        cyc,
        &note(&format!(
            r#""group": [{}]"#,
            BAR.replace("\"bar\"", "\"bar4\"")
        )),
        &[NOTES],
    )));
    dropped(grouped(
        r#"{"id": "bar", "dst": [{"x": 0, "y": 0, "w": 8, "h": 2, "a": 128}]}"#,
    ));
    dropped(grouped(&format!("{BAR}, {BAR}")));
    dropped(grouped(
        r#"{"id": "bar", "timer": 41, "dst": [{"x": 0, "y": 0, "w": 8, "h": 2}]}"#,
    ));
    dropped(grouped(
        r#"{"id": "bar", "loop": -1, "dst": [{"time": 0, "x": 0, "y": 0, "w": 8, "h": 2}]}"#,
    ));
    dropped(grouped(
        r#"{"id": "bar", "op": [40], "dst": [{"x": 0, "y": 0, "w": 8, "h": 2}]}"#,
    ));
    // A held key at 0 is perpetual.
    let held = grouped(
        r#"{"id": "bar", "loop": 0, "dst": [{"time": 0, "x": 0, "y": 0, "w": 8, "h": 2}]}"#,
    );
    assert!(has(
        held.report(),
        NoteKind::Deviation(DeviationId::D9(LaneApprox::BarLine))
    ));
}

fn mine_hit(ms: i64, l: u8) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(ms * MS),
        origin: None,
        kind: JudgeEventKind::MineHit {
            lane: Lane::new(l).unwrap(),
            note: 0,
        },
    }
}

#[test]
fn taps_and_mines_stay_until_their_time() {
    let l = lanes("", &[]);
    let none = Vec::<[i32; 4]>::new();
    // A late tap is not drawn below the line.
    assert_eq!(rects_over(&l, &tap(1, 1_000), 1_005), none);
    // An early-judged tap stays until its time.
    let mut d = drawn(&l, &tap(1, 1_000));
    d.frame(900, &[judged(900, Judgment::Great, 1)]);
    assert_eq!(d.rects(950), [[100, 810, 50, 20]]);
    // A mine leaves at its time, hit or not.
    let mines = ChartBuilder::new(150_000)
        .mine(Lane::new(2).unwrap(), TimeUs(1_000 * MS), 10)
        .build()
        .unwrap();
    let mut d = drawn(&l, &mines);
    assert_eq!(d.rects(999), [[150, 859, 50, 20]]);
    assert_eq!(d.rects(1_001), none);
    let mut d = drawn(&l, &mines);
    d.frame(900, &[mine_hit(900, 2)]);
    assert_eq!(d.rects(910), [[150, 770, 50, 20]]);
}

/// A long note on model lane 1 from 1 s to 1.3 s in `mode`, with a STOP
/// of `stop` ms at its end.
fn long_note(mode: Option<LnMode>, stop: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000).long(
        Lane::new(1).unwrap(),
        TimeUs(1_000 * MS),
        TimeUs(1_300 * MS),
    );
    if stop > 0 {
        b = b.stop(TimeUs(1_300 * MS), TimeUs(stop * MS));
    }
    let mut chart = b.build().unwrap();
    chart.meta.ln_mode = mode;
    chart
}

#[test]
fn long_notes_draw_as_the_source_draws_them() {
    let l = lanes("", &[]);
    // At 1.15 s the head is held on the line (860–880) and the end is
    // 150 px ahead: the body runs from the end's lower edge (730) to the
    // head's upper edge. A plain LN shows no end cap; CN and HCN do.
    for (mode, end) in [
        (None, false),
        (Some(LnMode::Ln), false),
        (Some(LnMode::Cn), true),
        (Some(LnMode::Hcn), true),
    ] {
        let mut want = vec![[100, 730, 50, 130]];
        if end {
            want.push([100, 710, 50, 20]);
        }
        want.push([100, 860, 50, 20]);
        assert_eq!(rects_over(&l, &long_note(mode, 0), 1_150), want, "{mode:?}");
    }
}

#[test]
fn a_long_note_expires_at_its_end_time() {
    let l = lanes("", &[]);
    for stop in [0, 200] {
        let chart = long_note(Some(LnMode::Cn), stop);
        let mut d = drawn(&l, &chart);
        assert_eq!(d.rects(1_250).len(), 3, "{stop}");
        // Within a note of its end: its caps, no body (D9 LnBody).
        assert_eq!(d.rects(1_299).len(), 2, "{stop}");
        assert!(d.rects(1_300).is_empty(), "{stop}");
        assert!(d.rects(1_400).is_empty(), "{stop}");
    }
}

#[test]
fn a_note_offset_off_the_lanes_refuses() {
    let header = r#""offset": [{"name": "n", "id": 40, "y": true, "h": true}]"#;
    let with = |extra: &str, ov: OffsetValue| {
        let objects = format!("{header}, {}", note(extra));
        let notes = r#"{"id": "notes", "offset": 40, "dst": [{"x": 0, "y": 0, "w": 0, "h": 0}]}"#;
        import_config(
            [1_920, 1_080],
            &body("", &objects, &[notes]),
            &[],
            |c: &mut Config| {
                c.offsets.insert(40, ov);
            },
        )
    };
    let y = |y: i32| OffsetValue {
        y,
        ..OffsetValue::default()
    };
    // Below the lanes' bottom at its judgment time: refused.
    refused(with("", y(-1)), "offset");
    // On it, or raised by the offset: drawn with the lower edge raised.
    let l = with("", y(0));
    assert_eq!(rects_over(&l, &tap(1, 1_000), 1_000), [[100, 860, 50, 20]]);
    let l = with("", y(30));
    assert_eq!(rects_over(&l, &tap(1, 1_000), 1_000), [[100, 830, 50, 20]]);
    // Above the lanes' top at its judgment time (a note more than twice
    // the region's height, with the top half a note above the region):
    // refused at 1201, drawn at 1200.
    let size = |s: &str| format!(r#""size": [{s}, {s}, {s}, {s}, {s}, {s}, {s}, {s}]"#);
    refused(with(&size("1201"), y(0)), "offset");
    // At 1200 the note spans the lanes from their top, −320, to 880.
    let l = with(&size("1200"), y(0));
    assert_eq!(
        rects_over(&l, &tap(1, 1_000), 1_000),
        [[100, -320, 50, 1_200]]
    );
}

#[test]
fn ids_resolve_in_the_source_order() {
    // HIDDEN+ is looked up before the note: the destination is HIDDEN+,
    // dropped without lanes.
    let hidden = format!(
        r#"{}, "hiddenCover": [{{"id": "notes", "src": "a"}}]"#,
        note("")
    );
    let l = import_body(&body("", &hidden, &[NOTES]));
    assert!(
        l.report()
            .notes
            .iter()
            .any(|n| n.severity == Severity::Dropped
                && n.kind
                    == NoteKind::UnsupportedField {
                        field: "hiddenCover"
                    })
    );
    assert_eq!(l.ok().scratch, None);
    // A pmchara sharing the note's id refuses.
    let chara = format!(
        r#"{}, "pmchara": [{{"id": "notes", "src": "a"}}]"#,
        note("")
    );
    refused(import_body(&body("", &chara, &[NOTES])), "id");
    // A second note destination refuses.
    refused(lanes("", &[NOTES]), "note");
}

#[test]
fn what_the_model_does_not_draw_is_noted() {
    let nested = |f: &str| {
        format!(r#""{f}": [{{"id": "deco", "dst": [{{"x": 0, "y": 0, "w": 8, "h": 8}}]}}]"#)
    };
    let extra = format!(
        r#"{}, {}, {}, "hidden": {}, "processed": {}"#,
        nested("bpm"),
        nested("stop"),
        nested("time"),
        eight("deco"),
        eight("deco")
    );
    let l = lanes(&extra, &[]);
    // The guides the source draws under player settings are dropped.
    for field in ["bpm", "stop", "time"] {
        assert!(
            l.report()
                .notes
                .iter()
                .any(|n| n.severity == Severity::Dropped
                    && n.kind == NoteKind::UnsupportedField { field }),
            "{field}"
        );
        assert!(!has(l.report(), NoteKind::Ignored { field }), "{field}");
    }
    for field in ["hidden", "processed", "hcn"] {
        assert!(has(l.report(), NoteKind::Ignored { field }), "{field}");
    }
}
