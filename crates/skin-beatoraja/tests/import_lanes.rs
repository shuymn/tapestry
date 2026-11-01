//! The lanes: the lane map and scratch side, the rects and the vertical
//! mapping, the preconditions, the parts, the lifetime, the offsets, the
//! bar line groups and the lane approximations, through `build`.

mod common;

use bms_model::{Chart, ChartBuilder, Lane, LnMode, TimeUs};
use common::lanes::{NOTES, body, cover, eight, lane_rects, lift_object, note, note_with};
use common::play::{Drawn, MS, judged, round};
use common::{Loaded, dst, has, import_body, import_config, import_sized};
use eval::{JudgeEvent, JudgeEventKind};
use rules::Judgment;
use skin_beatoraja::{
    Config, CriticalPart, DeviationId, ImportError, Imported, LaneApprox, NoteKind, OffsetValue,
    Severity, TrackWhy,
};
use skin_model::{BarGroupError, LanesError, LoadError, Side};

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
    // SUDDEN+'s share runs from the region's top (180), LIFT's from the
    // bottom (780): the lift of 100 ‰ takes 60 px from 780, and a cover
    // of 500 ‰ lowers SUDDEN+'s paint by half of the 540 px left, its
    // lower edge to 450; the paint, 600 high, is drawn whole, and LIFT's,
    // raised with the line, is cut at the lanes' bottom.
    let sudden =
        r#"{"id": "cover", "offset": 4, "dst": [{"x": 20, "y": 600, "w": 430, "h": 400}]}"#;
    let lift_dst = r#"{"id": "liftc", "dst": [{"x": 20, "y": 0, "w": 430, "h": 200}]}"#;
    let objs = format!("{}, {}", objects(""), lift_object(""));
    let l = at(objs, &[NOTES, sudden, lift_dst]);
    let mut d = drawn(&l, &tap(1, 5_000));
    d.view.cover = 500;
    d.view.lift = 100;
    let r = d.shown(1_000);
    assert_eq!(r, [[30, -150, 645, 600], [30, 720, 645, 60]]);
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

/// Seven lane rects refuse; the part arrays are in `import_note_parts`.
#[test]
fn the_lane_rects_are_checked() {
    let all = lane_rects(200, 600);
    let seven: Vec<&str> = all.rsplitn(2, "}, ").collect();
    let seven = seven[1].to_owned() + "}";
    refused(
        import_body(&body("", &note_with(&seven, ""), &[NOTES])),
        "dst",
    );
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
    // The lanes take no run-time offset.
    for (id, part) in [
        (1, CriticalPart::Lanes),
        (3, CriticalPart::Lanes),
        (4, CriticalPart::Lanes),
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

/// The bar image across the lanes on their bottom edge (source y 200).
const ACROSS: &str = r#""x": 20, "y": 200, "w": 430, "h": 2"#;

/// A tap at 5 s and bars at `ms`.
fn barred(ms: &[i64]) -> Chart {
    let mut chart = tap(1, 5_000);
    chart.bar_lines = ms
        .iter()
        .map(|&t| chart.scroll.position_at(TimeUs(t * MS)))
        .collect();
    chart
}

/// Whether `r` drops something for `kind`.
fn drops(r: &skin_beatoraja::Report, kind: NoteKind) -> bool {
    r.notes
        .iter()
        .any(|n| n.severity == Severity::Dropped && n.kind == kind)
}

const BAR_LINE: NoteKind = NoteKind::Deviation(DeviationId::D9(LaneApprox::BarLine));

#[test]
fn a_group_draws_where_its_destination_stands() {
    let l = grouped(&dst("bar", "", ACROSS));
    let r = l.report();
    assert!(has(r, BAR_LINE));
    assert_eq!(r.usage.bar_groups, 1);
    // At its time the bar is drawn at its destination, not at the
    // notes' centre line.
    assert_eq!(
        rects_over(&l, &barred(&[1_000]), 1_000),
        [[20, 878, 430, 2]]
    );
    // A coming bar is higher by its distance, a passed one lower, kept
    // while it overlaps the lanes: one pixel, then none, its top on
    // their bottom.
    assert_eq!(
        rects_over(&l, &barred(&[1_100]), 1_000),
        [[20, 778, 430, 2]]
    );
    assert_eq!(
        rects_over(&l, &barred(&[1_000]), 1_001),
        [[20, 879, 430, 2]]
    );
    assert_eq!(
        rects_over(&l, &barred(&[1_000]), 1_002),
        Vec::<[i32; 4]>::new()
    );
    assert_eq!(
        rects_over(&l, &barred(&[1_000]), 1_100),
        Vec::<[i32; 4]>::new()
    );
    // During a STOP at the bar, it holds.
    let mut chart = ChartBuilder::new(150_000)
        .tap(Lane::new(1).unwrap(), TimeUs(5_000 * MS))
        .stop(TimeUs(1_000 * MS), TimeUs(500 * MS))
        .build()
        .unwrap();
    chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
    assert_eq!(rects_over(&l, &chart, 1_400), [[20, 878, 430, 2]]);
}

#[test]
fn groups_keep_their_own_look_in_bar_then_group_order() {
    let tinted = dst(
        "deco",
        r#""blend": 2"#,
        r#""x": 100, "y": 197, "w": 50, "h": 6, "a": 128, "r": 255, "g": 0, "b": 0"#,
    );
    let bar = dst("bar", "", ACROSS);
    let l = grouped(&format!("{bar}, {tinted}, {bar}"));
    let r = l.report();
    assert_eq!(r.usage.bar_groups, 3);
    assert_eq!(r.notes.iter().filter(|n| n.kind == BAR_LINE).count(), 3);
    let q = drawn(&l, &barred(&[1_000, 1_100])).frame(1_000, &[]);
    let rects: Vec<[i32; 4]> = q.iter().map(|q| round(q.rect)).collect();
    assert_eq!(
        rects,
        [
            [20, 878, 430, 2],
            [100, 877, 50, 6],
            [20, 878, 430, 2],
            [20, 778, 430, 2],
            [100, 777, 50, 6],
            [20, 778, 430, 2],
        ]
    );
    assert_eq!(q[1].color, [1.0, 0.0, 0.0, 128.0 / 255.0]);
    assert_eq!(q[0].color, [1.0; 4]);
}

#[test]
fn groups_on_timers_conditions_and_cycles_map() {
    let cycled = r#"{"id": "bar4", "src": "a", "x": 0, "y": 20, "w": 8, "h": 8, "divy": 4, "cycle": 100, "timer": 41}"#;
    for (images, g) in [
        ("", dst("bar", r#""timer": 41"#, ACROSS)),
        ("", dst("bar", r#""op": [271]"#, ACROSS)),
        (
            "",
            dst(
                "bar",
                r#""loop": -1"#,
                r#""time": 0, "x": 20, "y": 200, "w": 430, "h": 2"#,
            ),
        ),
        ("", dst("bar", "", &format!(r#"{ACROSS}, "angle": 30"#))),
        (cycled, dst("bar4", "", ACROSS)),
    ] {
        let l = import_body(&body(
            images,
            &note(&format!(r#""group": [{g}]"#)),
            &[NOTES],
        ));
        let r = l.report();
        assert!(has(r, BAR_LINE), "{g}: {:?}", r.notes);
        assert!(
            r.notes.iter().all(|n| n.severity != Severity::Dropped),
            "{g}: {:?}",
            r.notes
        );
    }
}

#[test]
fn an_image_of_an_image_group_gives_its_ordinary_frames() {
    // `len` and `ref` are unused: no selector, the whole grid cycling.
    let frames = r#"{"id": "bars", "src": "a", "x": 0, "y": 20, "w": 8, "h": 8, "divy": 4, "len": 4, "ref": 500, "cycle": 400}"#;
    let l = import_body(&body(
        frames,
        &note(&format!(r#""group": [{}]"#, dst("bars", "", ACROSS))),
        &[NOTES],
    ));
    let r = l.report();
    assert!(has(r, BAR_LINE));
    assert!(
        r.notes.iter().all(|n| n.severity != Severity::Dropped),
        "{:?}",
        r.notes
    );
    let uv = |ms: i64| drawn(&l, &barred(&[ms])).frame(ms, &[])[0].uv;
    assert_ne!(uv(1_000), uv(1_100));
    assert_eq!(uv(1_000), uv(1_400));
}

#[test]
fn a_group_following_the_lift_moves_with_it() {
    let follower = dst("bar", r#""offset": 3"#, ACROSS);
    let still = dst("bar", "", ACROSS);
    let l = grouped(&format!("{follower}, {still}"));
    let r = l.report();
    let motion = NoteKind::Deviation(DeviationId::D9(LaneApprox::CoverMotion));
    assert_eq!(r.notes.iter().filter(|n| n.kind == motion).count(), 1);
    let mut d = drawn(&l, &barred(&[1_000]));
    d.view.lift = 250;
    // LIFT raises the follower by its 150 px; the other stays.
    assert_eq!(d.rects(1_000), [[20, 728, 430, 2], [20, 878, 430, 2]]);
}

#[test]
fn groups_that_cannot_draw_drop_alone() {
    let gone = r#"{"id": "gone", "src": "zz", "x": 0, "y": 0, "w": 8, "h": 2}"#;
    let cases = [
        (dst("nope", "", ACROSS), NoteKind::UndefinedObject),
        (
            format!(r#"{{"id": null, "dst": [{{{ACROSS}}}]}}"#),
            NoteKind::UndefinedObject,
        ),
        (dst("gone", "", ACROSS), NoteKind::MissingAsset),
        (dst("bar", r#""stretch": 3"#, ACROSS), NoteKind::Stretch(3)),
        (
            dst(
                "bar",
                r#""mouseRect": {"x": 0, "y": 0, "w": 1, "h": 1}"#,
                ACROSS,
            ),
            NoteKind::UnsupportedField { field: "mouseRect" },
        ),
        (
            dst("bar", r#""offset": 1"#, ACROSS),
            NoteKind::RuntimeOffset(1),
        ),
    ];
    for (g, why) in cases {
        let kept = dst("bar", "", ACROSS);
        let l = import_body(&body(
            gone,
            &note(&format!(r#""group": [{g}, {kept}]"#)),
            &[NOTES],
        ));
        let r = l.report();
        assert!(drops(r, why), "{g}: {:?}", r.notes);
        assert_eq!(r.usage.bar_groups, 2, "{g}");
        assert_eq!(
            r.notes.iter().filter(|n| n.kind == BAR_LINE).count(),
            1,
            "{g}"
        );
        // The kept group and the notes still draw.
        let mut chart = tap(1, 1_000);
        chart.bar_lines = vec![chart.scroll.position_at(TimeUs(1_000 * MS))];
        assert_eq!(
            rects_over(&l, &chart, 1_000),
            [[20, 878, 430, 2], [100, 860, 50, 20]],
            "{g}"
        );
    }
}

#[test]
fn a_group_without_keys_refuses() {
    for g in [r#"{"id": "bar", "dst": []}"#, r#"{"id": "bar"}"#] {
        match grouped(g).err() {
            ImportError::Type {
                field: "dst",
                expected: "a non-empty array",
                ..
            } => {}
            e => panic!("{g}: {e:?}"),
        }
    }
}

#[test]
fn a_native_group_of_an_empty_destination_draws_its_frame() {
    // Stretch 9 keeps the 8×2 frame, centred on the empty rect at
    // (100, 880).
    let l = grouped(&dst(
        "bar",
        r#""stretch": 9"#,
        r#""x": 100, "y": 200, "w": 0, "h": 0"#,
    ));
    assert_eq!(rects_over(&l, &barred(&[1_000]), 1_000), [[96, 879, 8, 2]]);
}

#[test]
fn a_group_clip_is_ignored() {
    let clipped = dst(
        "bar",
        "",
        &format!(r#"{ACROSS}, "clip_x": 0, "clip_y": 0, "clip_w": 9, "clip_h": 9"#),
    );
    let l = grouped(&clipped);
    let r = l.report();
    assert!(has(r, NoteKind::Ignored { field: "clip" }));
    assert!(has(r, BAR_LINE));
    assert_eq!(
        rects_over(&l, &barred(&[1_000]), 1_000),
        [[20, 878, 430, 2]]
    );
}

#[test]
fn a_group_model_error_points_at_the_group() {
    // A follower whose keys LIFT can move past the coordinate bound, on a
    // line of its own, apart from the lanes' destination.
    let far = dst(
        "bar",
        r#""offset": 3"#,
        r#""x": 20, "y": 1049400, "w": 430, "h": 2"#,
    );
    let groups = format!("{},\n{far}\n", dst("bar", "", ACROSS));
    let note = note(&format!(r#""group": [{groups}]"#));
    let src = body("", &note, &[NOTES]);
    // The import's text starts its body on line 2.
    let line = 2 + src[..src.find("1049400").unwrap()].matches('\n').count();
    assert_eq!(
        model(import_body(&src)),
        (
            LanesError::BarGroup {
                group: 1,
                error: BarGroupError::Coordinates,
            },
            u32::try_from(line).unwrap()
        )
    );
}

/// Imports a 1920×1080 skin of `body` beside the files `files`.
fn import_beside(body: &str, files: &[(&str, &str)]) -> Result<Imported, ImportError> {
    let json = format!(
        r#"{{"type": 0, "w": 1920, "h": 1080, "source": [{{"id": "a", "path": "a.png"}}], {body}}}"#
    );
    let d = common::skin_with(&json, &[("a.png", 64, 64)]);
    for (name, text) in files {
        common::write(d.path(), name, text.as_bytes());
    }
    common::import_dir(d.path())
}

#[test]
fn malformed_groups_refuse() {
    let bar = dst("bar", "", ACROSS);
    let typed = |field: &str, files: &[(&str, &str)]| {
        let src = body("", &note(&format!(r#""group": {field}"#)), &[NOTES]);
        match import_beside(&src, files) {
            Err(ImportError::Type {
                field, expected, ..
            }) => (field, expected),
            other => panic!("{field}: {other:?}"),
        }
    };
    assert_eq!(typed("42", &[]), ("group", "an array"));
    assert_eq!(
        typed(r#"{"include": "g.json"}"#, &[("g.json", "42")]),
        ("group", "an array")
    );
    assert_eq!(
        typed(r#"[{"include": "g.json"}]"#, &[("g.json", "[true]")]),
        ("group", "an object")
    );
    assert_eq!(typed("[42]", &[]), ("group", "an object"));
    assert_eq!(
        typed(r#"[{"id": "bar", "dst": 5}]"#, &[]),
        ("dst", "an array")
    );
    assert_eq!(
        typed(r#"[{"id": "bar", "dst": [5]}]"#, &[]),
        ("dst", "an object")
    );
    // A wrapper choosing nothing, or a null field, gives no group.
    for field in [
        format!(r#"[{{"if": 901, "value": {bar}}}]"#),
        r#"[{"if": 901, "values": 42}]"#.to_owned(),
        "null".to_owned(),
    ] {
        let src = body("", &note(&format!(r#""group": {field}"#)), &[NOTES]);
        let i = import_beside(&src, &[]).unwrap();
        assert_eq!(i.report.usage.bar_groups, 0, "{field}");
    }
    // Chosen, a wrapper's group maps.
    let src = body(
        "",
        &note(&format!(r#""group": [{{"if": -901, "value": {bar}}}]"#)),
        &[NOTES],
    );
    let i = import_beside(&src, &[]).unwrap();
    assert_eq!(i.report.usage.bar_groups, 1);
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
    // HIDDEN+ is looked up before the note: the destination is HIDDEN+'s
    // paint, dropped without its file, and there are no lanes.
    let hidden = format!(
        r#"{}, "hiddenCover": [{{"id": "notes", "src": "a"}}]"#,
        note("")
    );
    let l = import_body(&body("", &hidden, &[NOTES]));
    assert_eq!(l.ok().scratch, None);
    assert!(has(l.report(), NoteKind::MissingAsset));
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
