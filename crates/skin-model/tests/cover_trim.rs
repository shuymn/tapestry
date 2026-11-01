//! LIFT's and HIDDEN+'s paint on the source player's lanes
//! (`lanes_support::source`: the region 600 high from 280 to 880) at
//! 1920×1080 with the author's layout: their motion, the trim line gating
//! and clipping each paint by its unstretched rect, the clip never turned
//! with the paint, the draw order, HIDDEN+'s switch, the logical coverage
//! the paint never changes, and the load checks.

mod lanes_support;
mod play_support;

use bms_model::{Chart, ChartBuilder, TimeUs};
use lanes_support::{
    HIDDEN, LIFT, NOTE, WHITE, image, lanes_element, moving, paint, quads_of, rect, rects, sheet,
    source,
};
use play_support::{Harness, MS, REGIONS, full, lane, readouts_for};
use skin_model::{
    Adjust, Anchor, Bind, Blend, CoverMotion, CoverPaintDef, CoverPaintError, CoverTrim, Ease,
    ElementDef, End, Fit, Follow, KeyDef, LanesDef, Layout, LoadError, MAX_COORD, Native, Place,
    Quad, Region, Skin, SkinDef, Track, WidgetDefs,
};

const BPM: i64 = 150_000;

fn def(lanes: Option<LanesDef>, elements: Vec<ElementDef>, defs: Vec<CoverPaintDef>) -> SkinDef {
    SkinDef {
        fit: Fit::Adapt,
        regions: REGIONS,
        whens: Vec::new(),
        sheet: sheet(),
        elements,
        lanes,
        widgets: WidgetDefs {
            cover_paints: defs,
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    }
}

/// The source lanes, then `elements`, with the paints `defs`.
fn skin(elements: Vec<ElementDef>, defs: Vec<CoverPaintDef>) -> Skin {
    let mut all = vec![lanes_element()];
    all.extend(elements);
    Skin::new(def(Some(source()), all, defs)).unwrap()
}

fn tap_chart() -> Chart {
    ChartBuilder::new(BPM)
        .tap(lane(3), TimeUs(1_000 * MS))
        .tap(lane(3), TimeUs(1_300 * MS))
        .build()
        .unwrap()
}

/// A harness on `skin` showing `[cover, lift, hidden]` at 700 ms.
fn shown(skin: Skin, [cover, lift, hidden]: [u16; 3]) -> Harness {
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    (h.view.cover, h.view.lift, h.view.hidden) = (cover, lift, hidden);
    h.frame(700, &[]);
    h
}

/// The inline clip of every quad in the batch, in draw order.
fn clips(h: &Harness) -> Vec<Option<[f32; 4]>> {
    h.batch
        .runs()
        .iter()
        .flat_map(|r| (r.start..r.end).map(move |_| r.clip_rect.map(|c| c.rect())))
        .collect()
}

/// `image`'s quads with their inline clips, in draw order.
fn clipped(h: &Harness, image: u32) -> Vec<([f32; 4], Option<[f32; 4]>)> {
    let ours = quads_of(h, image);
    h.batch
        .quads()
        .iter()
        .zip(clips(h))
        .filter(|(q, _)| ours.contains(q))
        .map(|(q, c)| (q.rect, c))
        .collect()
}

/// LIFT's paint over `r`, trimmed by `trim`, gated by `gate` if given.
fn lift_def(trim: CoverTrim, gate: Option<[i32; 4]>) -> CoverPaintDef {
    CoverPaintDef {
        gate_track: gate.map(rect),
        ..moving(LIFT, CoverMotion::Lift, trim)
    }
}

#[test]
fn lift_and_hidden_paint_move_by_their_covers() {
    // H = 600: lift 300 ‰ is L = 180 and HIDDEN+ 500 ‰ of the 420 left is
    // K = 210, so LIFT's paint rises 180 and HIDDEN+'s 390; LIFT's linked
    // trim base 800 is 620 and cuts its paint, at 520, 100 down.
    let elements = vec![
        paint(0, [160, 700, 544, 200], Follow::None),
        paint(1, [160, 280, 544, 600], Follow::None),
    ];
    let defs = vec![
        lift_def(CoverTrim::Lift(800), None),
        moving(HIDDEN, CoverMotion::Hidden, CoverTrim::None),
    ];
    let h = shown(skin(elements.clone(), defs.clone()), [0, 300, 500]);
    assert_eq!(
        clipped(&h, LIFT),
        [(
            [160.0, 520.0, 544.0, 200.0],
            Some([160.0, 520.0, 544.0, 100.0])
        )]
    );
    assert_eq!(clipped(&h, HIDDEN), [([160.0, -110.0, 544.0, 600.0], None)]);
    // At rest they keep their authored rects; LIFT's line is 800.
    let h = shown(skin(elements, defs), [0, 0, 0]);
    assert_eq!(
        clipped(&h, LIFT),
        [(
            [160.0, 700.0, 544.0, 200.0],
            Some([160.0, 700.0, 544.0, 100.0])
        )]
    );
    assert_eq!(rects(&h, HIDDEN), [[160.0, 280.0, 544.0, 600.0]]);
}

#[test]
fn following_the_cover_adds_its_motion_once() {
    // Lift 300 ‰ (L = 180) and the cover 500 ‰ of the 420 left (C = 210):
    // LIFT's paint following the cover moves −180 + 210, its gate alike,
    // and its fixed line stays put.
    let elements = vec![paint(0, [160, 700, 544, 200], Follow::Cover)];
    let defs = vec![lift_def(CoverTrim::Fixed(800), None)];
    let h = shown(skin(elements, defs), [500, 300, 0]);
    assert_eq!(
        clipped(&h, LIFT),
        [(
            [160.0, 730.0, 544.0, 200.0],
            Some([160.0, 730.0, 544.0, 70.0])
        )]
    );
}

/// LIFT's paint fitted over `[160, 55, 544, 100]`, gated by the raw
/// `[160, 100, 544, 10]` and trimmed at `trim`, without lanes.
fn fitted(trim: i32) -> Harness {
    let elements = vec![paint(0, [160, 55, 544, 100], Follow::None)];
    let defs = vec![lift_def(CoverTrim::Fixed(trim), Some([160, 100, 544, 10]))];
    let skin = Skin::new(def(None, elements, defs)).unwrap();
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    h.frame(700, &[]);
    h
}

#[test]
fn the_raw_rect_gates_the_fitted_paint() {
    // The line above the raw rect: nothing, though the fitted paint
    // reaches above the line.
    assert!(rects(&fitted(90), LIFT).is_empty());
    assert!(rects(&fitted(100), LIFT).is_empty());
    // The raw rect wholly above the line: the fitted paint is whole,
    // reaching below the line.
    let whole = [([160.0, 55.0, 544.0, 100.0], None)];
    assert_eq!(clipped(&fitted(120), LIFT), whole);
    assert_eq!(clipped(&fitted(110), LIFT), whole);
    // The line through the raw rect: the fitted paint clipped to the raw
    // rect's part above it.
    assert_eq!(
        clipped(&fitted(105), LIFT),
        [(
            [160.0, 55.0, 544.0, 100.0],
            Some([160.0, 100.0, 544.0, 5.0])
        )]
    );
}

/// A 32×32 paint drawn at its native size over the empty `[0, 100, 0,
/// 10]`, trimmed at `trim`.
fn native_on_empty(trim: i32) -> Harness {
    let mut e = paint(0, [0, 100, 0, 10], Follow::None);
    e.place.native = Some(Native::Exact([32, 32]));
    let defs = vec![lift_def(CoverTrim::Fixed(trim), None)];
    let skin = Skin::new(def(None, vec![e], defs)).unwrap();
    let mut h = Harness::new(skin, &tap_chart(), &full(0));
    h.frame(700, &[]);
    h
}

#[test]
fn an_empty_scissor_draws_nothing() {
    // The line crosses the empty gate: its part above has no area, so
    // nothing draws.
    assert!(h_quads(&native_on_empty(105)).is_empty());
    // The gate wholly above the line: the native paint is whole.
    assert_eq!(
        clipped(&native_on_empty(120), LIFT),
        [([-16.0, 89.0, 32.0, 32.0], None)]
    );
}

fn h_quads(h: &Harness) -> Vec<Quad> {
    h.batch.quads().to_vec()
}

/// A track holding `[x, y, w, h]` turned `angle` degrees.
fn turned([x, y, w, h]: [i32; 4], angle: i32) -> Track {
    Track::new(
        &[KeyDef {
            at: TimeUs(0),
            props: [
                Some(x),
                Some(y),
                Some(w),
                Some(h),
                None,
                None,
                None,
                None,
                Some(angle),
            ],
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap()
}

/// The author's layout, and the playfield moved, scaled and its lanes
/// narrowed.
fn layouts() -> [Layout; 2] {
    let mut moved = Layout::AUTHOR;
    moved
        .set(
            Region::Playfield,
            Adjust {
                x: 40,
                y: -30,
                scale: 80,
                visible: true,
            },
        )
        .unwrap();
    moved.set_lane_width(70).unwrap();
    [Layout::AUTHOR, moved]
}

#[test]
fn a_turned_or_native_paint_keeps_its_quad_under_an_upright_clip() {
    // Trimmed and untrimmed twins of a turned paint, natively sized or
    // not, beside a plain image over the gate's part above the line, all
    // bound alike: the trimmed quad is its twin's, uv and pivot whole, and
    // its clip is the plain image's rect, never turned.
    let targets = [[1_920, 1_080], [3_840, 2_160], [1_366, 768]];
    for native in [None, Some(Native::Exact([12, 32]))] {
        for bind in [Bind::None, Bind::Lanes] {
            for pivot in Anchor::ALL {
                for target in targets {
                    for layout in layouts() {
                        let place = Place {
                            pivot,
                            native,
                            bind,
                            ..paint(0, [0; 4], Follow::None).place
                        };
                        let at = |index: u32| ElementDef {
                            track: turned([160, 300, 100, 200], 30),
                            place,
                            ..paint(index, [0; 4], Follow::None)
                        };
                        let plain = Place {
                            pivot: Anchor::TopLeft,
                            native: None,
                            ..place
                        };
                        let elements =
                            vec![at(0), at(1), image(WHITE, [160, 300, 100, 100], plain)];
                        let defs = vec![
                            moving(HIDDEN, CoverMotion::None, CoverTrim::Fixed(400)),
                            moving(HIDDEN, CoverMotion::None, CoverTrim::None),
                        ];
                        let mut h = Harness::new(skin(elements, defs), &tap_chart(), &full(0));
                        h.target = target;
                        h.layout = layout;
                        h.frame(700, &[]);
                        let case = format!("{native:?} {bind:?} {pivot:?} {target:?}");
                        let quads = quads_of(&h, HIDDEN);
                        assert_eq!(quads.len(), 2, "{case}");
                        assert_eq!(quads[0], quads[1], "{case}");
                        assert_eq!(quads[0].uv, h.uvs.get(HIDDEN).unwrap().uv, "{case}");
                        let gate = rects(&h, WHITE);
                        assert_eq!(gate.len(), 1, "{case}");
                        assert_eq!(
                            clipped(&h, HIDDEN),
                            [(quads[0].rect, Some(gate[0])), (quads[0].rect, None)],
                            "{case}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn every_clip_keeps_the_draw_order() {
    // Two LIFT paints of one clip merge into a run; a third with another
    // clip, an image between, or another blend each start one.
    let at = |index, y, blend| ElementDef {
        blend,
        ..paint(index, [160, y, 544, 200], Follow::None)
    };
    let elements = vec![
        at(0, 700, Blend::Alpha),
        at(0, 700, Blend::Alpha),
        at(1, 700, Blend::Alpha),
        image(WHITE, [0, 0, 10, 10], Place::SCREEN),
        at(0, 700, Blend::Alpha),
        at(0, 700, Blend::Add),
    ];
    let defs = vec![
        lift_def(CoverTrim::Fixed(800), None),
        lift_def(CoverTrim::Fixed(850), None),
    ];
    let h = shown(skin(elements, defs), [0, 0, 0]);
    let runs: Vec<_> = h
        .batch
        .runs()
        .iter()
        .map(|r| {
            (
                r.key.blend,
                r.clip_rect.map(|c| c.rect()[3]),
                r.end - r.start,
            )
        })
        .collect();
    assert_eq!(
        runs[runs.len() - 5..],
        [
            (Blend::Alpha, Some(100.0), 2),
            (Blend::Alpha, Some(150.0), 1),
            (Blend::Alpha, None, 1),
            (Blend::Alpha, Some(100.0), 1),
            (Blend::Add, Some(100.0), 1),
        ]
    );
}

/// The lanes' notes for `view`, with or without every kind of paint:
/// transparent, timed off, hidden by its region's paint, gated away.
fn notes(view: [u16; 3], with_paint: bool) -> Vec<[f32; 4]> {
    let mut elements = Vec::new();
    if with_paint {
        let mut clear = paint(1, [160, 280, 544, 600], Follow::None);
        clear.track = Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props: [
                    Some(160),
                    Some(280),
                    Some(544),
                    Some(600),
                    Some(0),
                    None,
                    None,
                    None,
                    None,
                ],
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap();
        let off = ElementDef {
            timer: skin_model::TimerRef::Builtin(skin_model::timer::FULL_COMBO),
            ..paint(0, [160, 880, 544, 200], Follow::None)
        };
        elements = vec![
            paint(0, [160, 880, 544, 200], Follow::Cover),
            clear,
            off,
            paint(0, [160, 2_000, 544, 10], Follow::None),
        ];
    }
    let defs = vec![
        lift_def(CoverTrim::Lift(880), None),
        moving(HIDDEN, CoverMotion::Hidden, CoverTrim::Lift(880)),
    ];
    let h = shown(skin(elements, defs), view);
    rects(&h, NOTE)
}

#[test]
fn paint_never_changes_the_logical_coverage() {
    for view in [
        [0, 0, 0],
        [1_000, 0, 0],
        [0, 1_000, 0],
        [0, 0, 1_000],
        [300, 250, 400],
    ] {
        assert_eq!(notes(view, true), notes(view, false), "{view:?}");
    }
}

/// HIDDEN+'s paint over `[160, 280, 544, 600]` fading from 255 to 0 over a
/// second with `ease`, at `ms` with HIDDEN+ `enabled` at `amount`.
fn fading(ease: Ease, ms: i64, enabled: bool, amount: u16) -> Vec<[f32; 4]> {
    let key = |at: i64, a: i32| KeyDef {
        at: TimeUs(at * MS),
        props: [
            Some(160),
            Some(280),
            Some(544),
            Some(600),
            Some(a),
            None,
            None,
            None,
            None,
        ],
    };
    let e = ElementDef {
        track: Track::new(&[key(0, 255), key(1_000, 0)], ease, End::Hold).unwrap(),
        ..paint(0, [0; 4], Follow::None)
    };
    let defs = vec![moving(HIDDEN, CoverMotion::Hidden, CoverTrim::None)];
    let mut h = Harness::new(skin(vec![e], defs), &tap_chart(), &full(0));
    h.view.hidden = if enabled { amount } else { 0 };
    let mut readouts = readouts_for(h.view);
    (readouts.covers.hidden, readouts.covers.hidden_enabled) = (amount, enabled);
    h.readouts = Some(readouts);
    h.frame(ms, &[]);
    rects(&h, HIDDEN)
}

#[test]
fn hidden_paint_shows_only_while_hidden_is_on() {
    // At a key, while fading and on a step, with any stored amount: off,
    // nothing; on, the paint, at 0 ‰ at its authored rect.
    let h = h_ready_ms();
    for ease in [Ease::Linear, Ease::Step] {
        for ms in [h, h + 500, h + 1_000] {
            for amount in [0, 500] {
                assert!(fading(ease, ms, false, amount).is_empty(), "{ease:?} {ms}");
            }
            assert_eq!(
                fading(ease, ms, true, 0),
                [[160.0, 280.0, 544.0, 600.0]],
                "{ease:?} {ms}"
            );
            assert_eq!(fading(ease, ms, true, 500).len(), 1, "{ease:?} {ms}");
        }
    }
}

/// When `ready` starts, in ms.
fn h_ready_ms() -> i64 {
    let mut h = Harness::new(
        Skin::new(def(None, Vec::new(), Vec::new())).unwrap(),
        &tap_chart(),
        &full(0),
    );
    h.frame(0, &[]);
    h.start_ms(skin_model::timer::READY).unwrap_or(0)
}

#[test]
fn a_hidden_gate_track_draws_nothing() {
    let late = Track::new(
        &[KeyDef {
            at: TimeUs(10_000 * MS),
            props: [
                Some(160),
                Some(100),
                Some(544),
                Some(10),
                None,
                None,
                None,
                None,
                None,
            ],
        }],
        Ease::Linear,
        End::Hold,
    )
    .unwrap();
    let defs = vec![CoverPaintDef {
        gate_track: Some(late),
        ..lift_def(CoverTrim::Fixed(105), None)
    }];
    let elements = vec![paint(0, [160, 55, 544, 100], Follow::None)];
    let h = shown(skin(elements, defs), [0, 0, 0]);
    assert!(rects(&h, LIFT).is_empty());
}

#[test]
fn neither_gates_nor_trims_change_the_layout() {
    // A far, vast gate and line: the same variant at every target and
    // the same controls.
    let at = |defs| {
        Skin::new(def(
            Some(source()),
            vec![
                lanes_element(),
                paint(0, [160, 700, 544, 200], Follow::None),
            ],
            defs,
        ))
        .unwrap()
    };
    let plain = at(vec![moving(LIFT, CoverMotion::Lift, CoverTrim::None)]);
    let gated = at(vec![lift_def(
        CoverTrim::Fixed(-100_000),
        Some([-100_000, -100_000, 300_000, 300_000]),
    )]);
    for target in [[1_920, 1_080], [3_840, 2_160], [640, 480]] {
        assert_eq!(
            plain.variant(target, &Layout::AUTHOR),
            gated.variant(target, &Layout::AUTHOR)
        );
    }
    for r in [Region::Playfield, Region::Judge] {
        assert_eq!(plain.controls(r), gated.controls(r));
    }
}

/// A load error, comparable.
fn dbg(e: LoadError) -> String {
    format!("{e:?}")
}

fn paint_err(def_: CoverPaintDef) -> String {
    Skin::new(def(
        None,
        vec![paint(0, [0, 0, 10, 10], Follow::None)],
        vec![def_],
    ))
    .map(drop)
    .map_err(dbg)
    .unwrap_err()
}

#[test]
fn paints_are_checked_at_load() {
    let fail = |error| dbg(LoadError::CoverPaint { paint: 0, error });
    let slider = |d: CoverPaintDef| CoverPaintDef {
        slider_range: Some(100),
        ..d
    };
    // A slider moves SUDDEN+ alone.
    for d in [
        moving(LIFT, CoverMotion::Lift, CoverTrim::None),
        moving(LIFT, CoverMotion::None, CoverTrim::Fixed(0)),
        CoverPaintDef {
            gate_track: Some(rect([0, 0, 1, 1])),
            ..moving(LIFT, CoverMotion::None, CoverTrim::None)
        },
    ] {
        assert_eq!(paint_err(slider(d)), fail(CoverPaintError::SliderMotion));
    }
    assert_eq!(
        paint_err(CoverPaintDef {
            gate_track: Some(rect([0, 0, 1, 1])),
            ..moving(LIFT, CoverMotion::Lift, CoverTrim::None)
        }),
        fail(CoverPaintError::GateWithoutTrim)
    );
    for r in [[0, MAX_COORD + 1, 1, 1], [0, 0, MAX_COORD + 1, 1]] {
        assert_eq!(
            paint_err(lift_def(CoverTrim::Fixed(0), Some(r))),
            fail(CoverPaintError::GateCoordinates),
            "{r:?}"
        );
    }
    assert!(
        Skin::new(def(
            None,
            vec![paint(0, [0, 0, 10, 10], Follow::None)],
            vec![lift_def(CoverTrim::Fixed(0), Some([0, MAX_COORD, 0, 0]))],
        ))
        .is_ok()
    );
    for r in [[0, 0, -1, 1], [0, 0, 1, -1]] {
        assert_eq!(
            paint_err(lift_def(CoverTrim::Fixed(0), Some(r))),
            fail(CoverPaintError::GateSize),
            "{r:?}"
        );
    }
    for trim in [
        CoverTrim::Fixed(MAX_COORD + 1),
        CoverTrim::Lift(-MAX_COORD - 1),
    ] {
        assert_eq!(
            paint_err(moving(LIFT, CoverMotion::Lift, trim)),
            fail(CoverPaintError::TrimRange),
            "{trim:?}"
        );
    }
    for trim in [CoverTrim::Fixed(MAX_COORD), CoverTrim::Lift(-MAX_COORD)] {
        assert!(
            Skin::new(def(
                None,
                vec![paint(0, [0, 0, 10, 10], Follow::None)],
                vec![moving(LIFT, CoverMotion::Lift, trim)],
            ))
            .is_ok()
        );
    }
}

#[test]
fn trimmed_paint_elements_are_checked_at_load() {
    let load = |e: ElementDef, d: CoverPaintDef| {
        Skin::new(def(Some(source()), vec![lanes_element(), e], vec![d])).map_err(dbg)
    };
    // Gated by its own track, a trimmed paint has no negative size.
    for r in [[0, 0, -1, 10], [0, 0, 10, -1]] {
        let e = paint(0, r, Follow::None);
        assert_eq!(
            load(e.clone(), lift_def(CoverTrim::Fixed(0), None)).unwrap_err(),
            dbg(LoadError::CoverGate { element: 1 }),
            "{r:?}"
        );
        // Its own gate track lifts that.
        assert!(load(e, lift_def(CoverTrim::Fixed(0), Some([0, 0, 1, 1]))).is_ok());
    }
    // A moving paint follows SUDDEN+ alone.
    for follow in [Follow::Lift, Follow::LiftAndCover] {
        assert_eq!(
            load(
                paint(0, [0, 0, 10, 10], follow),
                moving(LIFT, CoverMotion::Lift, CoverTrim::None)
            )
            .unwrap_err(),
            dbg(LoadError::UnsupportedFollow { element: 1 }),
            "{follow:?}"
        );
    }
    assert!(
        load(
            paint(0, [0, 0, 10, 10], Follow::Cover),
            moving(LIFT, CoverMotion::Hidden, CoverTrim::None)
        )
        .is_ok()
    );
    // Moved the lanes' length, a moving paint's keys, its gate's and its
    // linked line stay within ±MAX_COORD.
    let low = MAX_COORD - 600 + 1;
    assert_eq!(
        load(
            paint(0, [0, -low, 10, 10], Follow::None),
            moving(LIFT, CoverMotion::Lift, CoverTrim::None)
        )
        .unwrap_err(),
        dbg(LoadError::Coordinates { element: 1 })
    );
    assert_eq!(
        load(
            paint(0, [0, 0, 10, 10], Follow::None),
            lift_def(CoverTrim::Fixed(0), Some([0, -low, 10, 10]))
        )
        .unwrap_err(),
        dbg(LoadError::Coordinates { element: 1 })
    );
    assert_eq!(
        load(
            paint(0, [0, 0, 10, 10], Follow::None),
            moving(LIFT, CoverMotion::Lift, CoverTrim::Lift(-low))
        )
        .unwrap_err(),
        dbg(LoadError::Coordinates { element: 1 })
    );
    assert!(
        load(
            paint(0, [0, 0, 10, 10], Follow::None),
            moving(LIFT, CoverMotion::Lift, CoverTrim::Lift(1 - low))
        )
        .is_ok()
    );
}
