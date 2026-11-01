//! Expression-selected images: the selection rules, each entry's span,
//! cycle, track and native size, the region box, the charges and the load
//! errors.
//!
//! Unless stated: one element on `ready` at `(100, 100, 40, 40)` in
//! `screen`, choosing by `combo - 1`, so a combo of `n` selects entry
//! `n − 1` and a combo of 0 hides it.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use lanes_support::rect;
use play_support::{MS, tally};
use rules::Judgment;
use skin_model::{
    CycleDef, Ease, End, ImageChoiceDef, ImageChoiceError, ImageChoicePart, ImageSpan, KeyDef,
    LoadError, MAX_COORD, MAX_ELEMENTS, MAX_IMAGE_CHOICE_REFS, MAX_KEY_US, MAX_SKIN_KEYS,
    MAX_SKIN_OPS, MAX_TRACK_KEYS, Native, Place, Region, Source, TimerRef, Track, WidgetDefs,
    timer,
};
use widget_support::{
    ELEVEN, TEN, TWENTY_FOUR, boolean, drawn, element, harness, int, judgment, load,
};

const KEY: [i32; 4] = [100, 100, 40, 40];

/// An entry of `frames` frames of `image` from `first`, without a cycle,
/// track or native size.
fn part(image: u32, first: u16, frames: u16) -> ImageChoicePart {
    ImageChoicePart {
        span: ImageSpan {
            image,
            first,
            frames,
        },
        cycle: None,
        track: None,
        native: None,
    }
}

fn cycled(p: ImageChoicePart, period_ms: i64, timer: u16) -> ImageChoicePart {
    ImageChoicePart {
        cycle: Some(CycleDef {
            period: TimeUs(period_ms * MS),
            timer: TimerRef::Builtin(timer),
        }),
        ..p
    }
}

fn table(value: &str, choices: Vec<Option<ImageChoicePart>>) -> ImageChoiceDef {
    ImageChoiceDef {
        value: int(value),
        choices,
    }
}

fn widgets(tables: Vec<ImageChoiceDef>) -> WidgetDefs {
    WidgetDefs {
        image_choices: tables,
        ..WidgetDefs::default()
    }
}

fn chooser(i: u32) -> skin_model::ElementDef {
    element(Source::ImageChoice(i), KEY, Place::SCREEN)
}

/// [`chooser`] of table 0 in `left`, a region with a box.
fn left_chooser() -> skin_model::ElementDef {
    let place = Place {
        region: Region::Left,
        ..Place::SCREEN
    };
    element(Source::ImageChoice(0), KEY, place)
}

/// The image and frame drawn at `ms` with `combo`, and the quad's rect.
fn shown_at(h: &mut play_support::Harness, ms: i64, combo: u32) -> Vec<(u32, u16, [f32; 4])> {
    h.tally = tally(combo);
    h.frame(ms, &[]);
    drawn(h)
        .into_iter()
        .map(|(i, f, q)| (i, f, q.rect))
        .collect()
}

#[test]
fn selection_hides_below_zero_and_falls_back_to_entry_zero() {
    let t = table(
        "combo - 1",
        vec![Some(part(TEN, 2, 1)), None, Some(part(ELEVEN, 4, 1))],
    );
    let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
    let rect = [100.0, 100.0, 40.0, 40.0];
    // −1 hides; 0 and the last select themselves; a hole is blank; the
    // length selects entry 0, as does anything past it.
    for (combo, want) in [
        (0, vec![]),
        (1, vec![(TEN, 2, rect)]),
        (2, vec![]),
        (3, vec![(ELEVEN, 4, rect)]),
        (4, vec![(TEN, 2, rect)]),
        (1_000, vec![(TEN, 2, rect)]),
    ] {
        assert_eq!(shown_at(&mut h, 0, combo), want, "combo {combo}");
    }
}

#[test]
fn integer_extremes_are_ordinary_selections() {
    for (src, want) in [("2147483647", vec![(TEN, 0)]), ("-2147483647 - 1", vec![])] {
        let t = table(src, vec![Some(part(TEN, 0, 1)), Some(part(ELEVEN, 0, 1))]);
        let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
        let got: Vec<_> = shown_at(&mut h, 0, 0)
            .into_iter()
            .map(|(i, f, _)| (i, f))
            .collect();
        assert_eq!(got, want, "{src}");
    }
}

#[test]
fn a_hole_at_entry_zero_hides_every_fallback() {
    let t = table("combo - 1", vec![None, Some(part(TEN, 0, 1))]);
    let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
    assert_eq!(shown_at(&mut h, 0, 2).len(), 1);
    for combo in [1, 3, 9] {
        assert!(shown_at(&mut h, 0, combo).is_empty(), "combo {combo}");
    }
}

#[test]
fn a_span_animates_only_its_frames() {
    // Frames 3–5 of ten over 300 ms on `ready`; the image's own cycle
    // is not used.
    let t = table("0", vec![Some(cycled(part(TEN, 3, 3), 300, timer::READY))]);
    let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
    h.frame(0, &[]);
    let ready = h.start_ms(timer::READY).unwrap();
    let mut seen = Vec::new();
    for ms in [0, 99, 100, 199, 200, 299, 300, 450] {
        let got = shown_at(&mut h, ready + ms, 0);
        seen.push(got.first().map(|&(_, f, _)| f));
    }
    let want = [3, 3, 4, 4, 5, 5, 3, 4].map(Some);
    assert_eq!(seen, want);
}

#[test]
fn each_entry_keeps_its_own_clock() {
    // Entry 0: frames 0–3 over 400 ms on `ready`; entry 1: frames 5–9 over
    // 500 ms on `judge`.
    let t = table(
        "combo",
        vec![
            Some(cycled(part(TEN, 0, 4), 400, timer::READY)),
            Some(cycled(part(TEN, 5, 5), 500, timer::JUDGE)),
        ],
    );
    let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
    h.frame(0, &[]);
    let ready = h.start_ms(timer::READY).unwrap();
    let frame = |h: &mut play_support::Harness, ms: i64, combo: u32| {
        shown_at(h, ready + ms, combo).first().map(|&(_, f, _)| f)
    };
    // `judge` is off: entry 1 shows its first frame.
    assert_eq!(frame(&mut h, 150, 1), Some(5));
    assert_eq!(frame(&mut h, 150, 0), Some(1));
    h.tally = tally(1);
    h.frame(ready + 200, &[judgment(ready + 200, Judgment::Great, 1)]);
    // 100 ms into `judge`'s 500: frame 6; `ready` runs on meanwhile.
    assert_eq!(frame(&mut h, 300, 1), Some(6));
    assert_eq!(frame(&mut h, 450, 0), Some(0));
    // Back on entry 1, its clock did not restart.
    assert_eq!(frame(&mut h, 700, 1), Some(5));
    assert_eq!(frame(&mut h, 900, 1), Some(7));
}

#[test]
fn an_entry_samples_its_own_track_on_the_element_timer() {
    let mut p1 = part(ELEVEN, 0, 1);
    p1.track = Some(rect([300, 300, 80, 20]));
    let t = table("combo", vec![Some(part(TEN, 0, 1)), Some(p1)]);
    let skin = load(vec![left_chooser()], widgets(vec![t])).unwrap();
    // The region's box holds both entries' tracks, whichever is shown.
    let p = skin.placement(
        &skin.canvas([1_920, 1_080]),
        &skin_model::Layout::AUTHOR,
        skin_model::Side::Left,
    );
    assert_eq!(
        p.region(Region::Left).bounds,
        Some([100.0, 100.0, 280.0, 220.0])
    );
    let mut h = harness(skin);
    assert_eq!(
        shown_at(&mut h, 0, 1),
        [(ELEVEN, 0, [300.0, 300.0, 80.0, 20.0])]
    );
    assert_eq!(
        shown_at(&mut h, 0, 0),
        [(TEN, 0, [100.0, 100.0, 40.0, 40.0])]
    );
}

#[test]
fn every_entry_with_its_own_track_leaves_the_element_track_out_of_the_box() {
    let mut p0 = part(TEN, 0, 1);
    p0.track = Some(rect([500, 500, 10, 10]));
    let t = table("0", vec![Some(p0), None]);
    let skin = load(vec![left_chooser()], widgets(vec![t])).unwrap();
    let p = skin.placement(
        &skin.canvas([1_920, 1_080]),
        &skin_model::Layout::AUTHOR,
        skin_model::Side::Left,
    );
    assert_eq!(
        p.region(Region::Left).bounds,
        Some([500.0, 500.0, 10.0, 10.0])
    );
}

/// The quad sizes of a 16×16 and a 64×32 entry kept at their native size
/// over a 40×40 key, on a 16:9 target of `height` lines.
fn native_sizes(native: fn([u32; 2]) -> Native, height: u32) -> Vec<[f32; 2]> {
    let mut a = part(TEN, 0, 1);
    a.native = Some(native([16, 16]));
    let mut b = part(ELEVEN, 0, 1);
    b.native = Some(native([64, 32]));
    let t = table("combo", vec![Some(a), Some(b)]);
    let mut h = harness(load(vec![chooser(0)], widgets(vec![t])).unwrap());
    h.target = [height * 16 / 9, height];
    [0, 1]
        .into_iter()
        .map(|combo| {
            let r = shown_at(&mut h, 0, combo)[0].2;
            [r[2], r[3]]
        })
        .collect()
}

#[test]
fn each_entry_keeps_its_own_native_size() {
    // Stretch 9: the frame's size in target pixels, whatever the key.
    let exact = |h| native_sizes(Native::Exact, h);
    assert_eq!(exact(720), [[24.0, 24.0], [96.0, 48.0]]);
    assert_eq!(exact(1_080), [[16.0, 16.0], [64.0, 32.0]]);
    assert_eq!(exact(2_160), [[8.0, 8.0], [32.0, 16.0]]);
    // Stretch 8: shrunk to fit the 40×40 key, never enlarged. The 64-wide
    // entry fits from a scale of 1.6 up.
    let fit = |h| native_sizes(Native::NoLarger, h);
    assert_eq!(fit(720), [[24.0, 24.0], [40.0, 20.0]]);
    assert_eq!(fit(1_080), [[16.0, 16.0], [40.0, 20.0]]);
    assert_eq!(fit(1_728), [[10.0, 10.0], [40.0, 20.0]]);
    assert_eq!(fit(2_160), [[8.0, 8.0], [32.0, 16.0]]);
}

#[test]
fn one_quad_per_element_and_ops_per_reference() {
    // Table 0 (4 ops) referenced by every element, table 1 (6 ops) by the
    // last one or by none: each reference charges its table's selector,
    // an unreferenced table nothing.
    let shared = table("abs(combo - 1)", vec![Some(part(TEN, 0, 1)); 5]);
    let other = table("abs(combo - 1) + 1", vec![Some(part(TEN, 0, 1))]);
    assert_eq!((shared.value.op_count(), other.value.op_count()), (4, 6));
    let tables = || widgets(vec![shared.clone(), other.clone()]);
    let skin = load(vec![chooser(0), chooser(0)], tables()).unwrap();
    assert_eq!(skin.quad_capacity(), 2);
    assert_eq!(skin.ops().total(), 2 * 4);
    assert_eq!(skin.choice_ref_count(), 6);
    // At the op cap: every element references table 0.
    assert_eq!(MAX_ELEMENTS * 4, MAX_SKIN_OPS);
    let skin = load(vec![chooser(0); MAX_ELEMENTS], tables()).unwrap();
    assert_eq!(skin.ops().total(), MAX_SKIN_OPS);
    // Past it: the last references table 1 instead.
    let mut elements = vec![chooser(0); MAX_ELEMENTS - 1];
    elements.push(chooser(1));
    assert!(matches!(load(elements, tables()), Err(LoadError::Ops(_))));
}

/// A track of [`MAX_TRACK_KEYS`] keys, 1 ms apart.
fn long_track() -> Track {
    let keys: Vec<_> = (0..i64::try_from(MAX_TRACK_KEYS).unwrap())
        .map(|i| KeyDef {
            at: TimeUs(i * MS),
            props: [
                Some(0),
                Some(0),
                Some(1),
                Some(1),
                None,
                None,
                None,
                None,
                None,
            ],
        })
        .collect();
    Track::new(&keys, Ease::Linear, End::Hold).unwrap()
}

#[test]
fn override_tracks_count_their_keys_once_per_table() {
    let mut p = part(TEN, 0, 1);
    p.track = Some(long_track());
    // 255 tables of one long track, each referenced by two elements, and
    // the elements' own keys: exactly at the cap.
    let with = |tables: usize, refs: usize| {
        let elements = (0..tables)
            .flat_map(|i| std::iter::repeat_n(chooser(u32::try_from(i).unwrap()), refs))
            .collect();
        load(
            elements,
            widgets(vec![table("0", vec![Some(p.clone())]); tables]),
        )
    };
    assert_eq!(255 * MAX_TRACK_KEYS + 2 * 255 + 1, MAX_SKIN_KEYS + 255);
    assert_eq!(255 * MAX_TRACK_KEYS + 255, MAX_SKIN_KEYS - 1);
    assert!(with(255, 1).is_ok());
    // A second reference adds its element's key only.
    assert!(matches!(
        with(255, 2),
        Err(LoadError::TooManyKeys { n }) if n == MAX_SKIN_KEYS - 1 + 255
    ));
    assert!(matches!(
        with(256, 1),
        Err(LoadError::TooManyKeys { n }) if n == MAX_SKIN_KEYS + 256
    ));
}

fn load_err(tables: Vec<ImageChoiceDef>) -> LoadError {
    load(vec![chooser(0)], widgets(tables)).unwrap_err()
}

#[test]
fn tables_refuse_what_they_cannot_draw() {
    let entry = |p| table("0", vec![Some(part(TEN, 0, 1)), Some(p)]);
    let err = |e: LoadError| match e {
        LoadError::ImageChoice { table: 0, error } => error,
        e => panic!("{e:?}"),
    };
    assert!(matches!(
        err(load_err(vec![ImageChoiceDef {
            value: boolean("true"),
            choices: vec![Some(part(TEN, 0, 1))],
        }])),
        ImageChoiceError::NotInt
    ));
    assert!(matches!(
        err(load_err(vec![table("0", Vec::new())])),
        ImageChoiceError::Empty
    ));
    assert!(matches!(
        err(load_err(vec![entry(part(9_999, 0, 1))])),
        ImageChoiceError::UnknownImage { choice: 1 }
    ));
    for (first, frames) in [(0, 0), (10, 1), (9, 2), (u16::MAX, u16::MAX)] {
        assert!(matches!(
            err(load_err(vec![entry(part(TEN, first, frames))])),
            ImageChoiceError::Span { choice: 1 }
        ));
    }
    // The last frame and a span of every frame are inside.
    assert!(load(vec![chooser(0)], widgets(vec![entry(part(TEN, 9, 1))])).is_ok());
    assert!(
        load(
            vec![chooser(0)],
            widgets(vec![entry(part(TWENTY_FOUR, 0, 24))])
        )
        .is_ok()
    );
    for period in [0, -1, MAX_KEY_US + 1] {
        let mut p = part(TEN, 0, 2);
        p.cycle = Some(CycleDef {
            period: TimeUs(period),
            timer: TimerRef::Builtin(timer::READY),
        });
        assert!(matches!(
            err(load_err(vec![entry(p)])),
            ImageChoiceError::Cycle { choice: 1 }
        ));
    }
    let mut p = part(TEN, 0, 2);
    p.cycle = Some(CycleDef {
        period: TimeUs(MAX_KEY_US),
        timer: TimerRef::When(0),
    });
    assert!(matches!(
        err(load_err(vec![entry(p)])),
        ImageChoiceError::UnknownTimer { choice: 1 }
    ));
    let mut p = part(TEN, 0, 1);
    p.track = Some(rect([MAX_COORD + 1, 0, 1, 1]));
    assert!(matches!(
        err(load_err(vec![entry(p)])),
        ImageChoiceError::Coordinates { choice: 1 }
    ));
    let mut p = part(TEN, 0, 1);
    p.track = Some(rect([MAX_COORD, 0, 1, 1]));
    assert!(load(vec![chooser(0)], widgets(vec![entry(p)])).is_ok());
}

#[test]
fn unreferenced_tables_are_checked_too() {
    let good = table("0", vec![Some(part(TEN, 0, 1))]);
    let bad = table("0", Vec::new());
    assert!(matches!(
        load(vec![chooser(0)], widgets(vec![good, bad])).unwrap_err(),
        LoadError::ImageChoice {
            table: 1,
            error: ImageChoiceError::Empty
        }
    ));
}

#[test]
fn elements_refuse_a_missing_table_and_a_native_size_of_their_own() {
    let t = table("0", vec![Some(part(TEN, 0, 1))]);
    assert!(matches!(
        load(vec![chooser(1)], widgets(vec![t.clone()])).unwrap_err(),
        LoadError::UnknownWidget { element: 0 }
    ));
    let mut e = chooser(0);
    e.place.native = Some(Native::Exact([1, 1]));
    assert!(matches!(
        load(vec![e], widgets(vec![t])).unwrap_err(),
        LoadError::ChoiceNative { element: 0 }
    ));
}

#[test]
fn entries_are_capped_across_tables() {
    let half = MAX_IMAGE_CHOICE_REFS / 2;
    let at_cap = || {
        vec![
            table("0", vec![None; half]),
            table("0", vec![Some(part(TEN, 0, 1)); half]),
        ]
    };
    let skin = load(vec![chooser(1)], widgets(at_cap())).unwrap();
    assert_eq!(skin.choice_ref_count(), MAX_IMAGE_CHOICE_REFS);
    let mut over = at_cap();
    over[0].choices.push(None);
    assert!(matches!(
        load_err(over),
        LoadError::TooManyImageChoiceRefs { n } if n == MAX_IMAGE_CHOICE_REFS + 1
    ));
    let tables = vec![table("0", vec![None]); MAX_ELEMENTS + 1];
    assert!(matches!(
        load_err(tables),
        LoadError::TooManyWidgets { n } if n == MAX_ELEMENTS + 1
    ));
}

#[test]
fn a_follower_checks_every_entry_track() {
    let mut p = part(TEN, 0, 1);
    p.track = Some(rect([0, MAX_COORD, 1, 1]));
    let t = table("0", vec![Some(part(TEN, 0, 1)), Some(p)]);
    let mut e = chooser(0);
    e.place = Place {
        follow: skin_model::Follow::Cover,
        ..Place::SCREEN
    };
    // Without lanes the reach is 0: at the bound is inside.
    assert!(load(vec![e.clone()], widgets(vec![t.clone()])).is_ok());
    let lanes = Some(lanes_support::simple());
    let err = widget_support::load_fit(
        skin_model::Fit::Adapt,
        play_support::REGIONS,
        lanes,
        vec![e.clone(), lanes_support::lanes_element()],
        widgets(vec![t]),
    );
    assert!(matches!(
        err.unwrap_err(),
        LoadError::Coordinates { element: 0 }
    ));
    let _: Track = rect(KEY);
}
