//! The `judge` widget: what drives it, the combo and the shift, the slots
//! and their fallbacks, the parts' timers, edges, blends and visibility,
//! its region and logical box, and its load errors.
//!
//! Unless stated: the grade key `(800, 500, 200, 80)` on `judge`, a combo
//! of 4 ten-glyph cells, centred, blank-padded, space 0, at the relative
//! key `(210, 10, 20, 60)`, with the shift.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use lanes_support::rect;
use play_support::{Harness, MS, empty_poor, ev, gauge as gauge_effect, tally};
use rules::{GaugeValue, Judgment};
use skin_model::{
    Adjust, Align, Anchor, Attach, Bind, Blend, Canvas, ComboPart, DigitOffset, DigitsDef, Ease,
    ElementDef, End, Fit, GlyphSet, GradePart, JudgeDef, JudgeError, JudgePart, JudgeSlot, KeyDef,
    Layout, LoadError, MAX_COORD, MAX_SKIN_KEYS, MAX_TRACK_KEYS, NumberError, Pad, Place, Region,
    Side, Skin, Source, TimerRef, Track, WidgetDefs, timer,
};
use widget_support::{
    ELEVEN, GRADE, TEN, boolean, combo, digits, down, drawn, element, empty_judge, grade, harness,
    judge_all, judge_element, judgment, load, load_fit,
};

const GRADE_KEY: [i32; 4] = [800, 500, 200, 80];
const COMBO_KEY: [i32; 4] = [210, 10, 20, 60];

fn strip() -> DigitsDef {
    DigitsDef {
        space: 0,
        ..digits(TEN, GlyphSet::Ten, 4, Align::Center, Pad::Blank)
    }
}

/// Every grade image, and a combo on GREAT.
fn judge() -> JudgeDef {
    let mut j = judge_all(GRADE_KEY);
    j.combos[JudgeSlot::Great.index()] = Some(combo(strip(), COMBO_KEY));
    j
}

fn skin_of(j: JudgeDef) -> Skin {
    load(
        vec![judge_element(0)],
        WidgetDefs {
            judges: vec![j],
            ..WidgetDefs::default()
        },
    )
    .unwrap()
}

/// What a frame drew: per quad its image, frame and rect.
fn quads(h: &Harness) -> Vec<(u32, u16, [f32; 4])> {
    drawn(h)
        .into_iter()
        .map(|(i, f, q)| (i, f, q.rect))
        .collect()
}

fn great() -> u32 {
    GRADE + 1
}

#[test]
fn nothing_shows_before_the_first_judgment() {
    let mut h = harness(skin_of(judge()));
    h.tally = tally(5);
    h.frame(100, &[]);
    assert!(h.start(timer::READY).is_some());
    assert!(h.batch.quads().is_empty());
    h.frame(200, &[judgment(200, Judgment::Great, 5)]);
    assert!(!h.batch.quads().is_empty());
}

#[test]
fn the_grade_moves_left_by_half_the_drawn_combo() {
    let mut h = harness(skin_of(judge()));
    // The snapshot's combo, not the event's.
    h.tally = tally(123);
    h.frame(0, &[judgment(0, Judgment::Great, 10)]);
    // One blank cell, centred: cells 1–3 at 1010 + 20·j − 10. The drawn
    // width is 60, so the grade moves 30; the full cells' 80 would give
    // 760.
    let y = 510.0;
    assert_eq!(
        quads(&h),
        [
            (TEN, 1, [1_020.0, y, 20.0, 60.0]),
            (TEN, 2, [1_040.0, y, 20.0, 60.0]),
            (TEN, 3, [1_060.0, y, 20.0, 60.0]),
            (great(), 0, [770.0, 500.0, 200.0, 80.0]),
        ]
    );
    let mut j = judge();
    j.shift = false;
    let mut h = harness(skin_of(j));
    h.tally = tally(123);
    h.frame(0, &[judgment(0, Judgment::Great, 10)]);
    assert_eq!(quads(&h)[3], (great(), 0, [800.0, 500.0, 200.0, 80.0]));
}

#[test]
fn moved_combo_glyphs_keep_the_baseline_shift() {
    // Rows 1–3 move and grow the drawn cells 1–3; the grade still moves
    // by half the 60 px baseline width.
    let grow = |j: i32| DigitOffset {
        dx: 5 * j,
        dy: -j,
        dw: 10,
        dh: 2 * j,
    };
    let mut j = judge();
    j.combos[JudgeSlot::Great.index()] = Some(combo(
        DigitsDef {
            offsets: (0..4).map(grow).collect(),
            ..strip()
        },
        COMBO_KEY,
    ));
    let mut h = harness(skin_of(j));
    h.tally = tally(123);
    h.frame(0, &[judgment(0, Judgment::Great, 10)]);
    assert_eq!(
        quads(&h),
        [
            (TEN, 1, [1_025.0, 509.0, 30.0, 62.0]),
            (TEN, 2, [1_050.0, 508.0, 30.0, 64.0]),
            (TEN, 3, [1_075.0, 507.0, 30.0, 66.0]),
            (great(), 0, [770.0, 500.0, 200.0, 80.0]),
        ]
    );
}

#[test]
fn a_full_gauge_combo_falls_back_with_its_offsets() {
    // PERFECT's combo stands in for the full-gauge slot, its rows with it.
    let mut j = judge_all(GRADE_KEY);
    j.combos[0] = Some(combo(
        DigitsDef {
            offsets: vec![DigitOffset::default(), down(4), down(5), down(6)],
            ..strip()
        },
        COMBO_KEY,
    ));
    let mut h = harness(skin_of(j));
    h.tally = tally(7);
    h.gauges[0].value = GaugeValue::FULL;
    h.frame(0, &[ev(0, gauge_effect(0, GaugeValue::FULL))]);
    h.frame(10, &[judgment(10, Judgment::Perfect, 7)]);
    // One digit of four cells, centred: cell 3 at 1010 + 60 − 30.
    assert_eq!(
        quads(&h),
        [
            (TEN, 7, [1_040.0, 516.0, 20.0, 60.0]),
            (GRADE + 6, 0, [790.0, 500.0, 200.0, 80.0]),
        ]
    );
}

#[test]
fn a_combo_of_0_shows_nothing_and_shifts_nothing() {
    let mut h = harness(skin_of(judge()));
    h.tally = tally(11);
    h.frame(0, &[judgment(0, Judgment::Great, 10)]);
    // The slot shows its combo above 0.
    assert_eq!(quads(&h).len(), 3);
    assert_eq!(
        quads(&h)[..2].iter().map(|q| q.1).collect::<Vec<_>>(),
        [1, 1]
    );
    h.tally = tally(0);
    h.frame(16, &[]);
    assert_eq!(quads(&h), [(great(), 0, [800.0, 500.0, 200.0, 80.0])]);
}

#[test]
fn perfect_at_a_full_gauge_uses_its_own_slot() {
    let full = |h: &mut Harness| {
        h.gauges[0].value = GaugeValue::FULL;
        h.frame(0, &[ev(0, gauge_effect(0, GaugeValue::FULL))]);
        assert!(h.start(timer::GAUGE_MAX).is_some());
    };
    let mut h = harness(skin_of(judge()));
    full(&mut h);
    h.frame(10, &[judgment(10, Judgment::Perfect, 1)]);
    assert_eq!(quads(&h)[0].0, GRADE + 6);
    // Without the full gauge, PERFECT's own.
    h.gauges[0].value = GaugeValue(0);
    h.frame(20, &[ev(20, gauge_effect(0, GaugeValue(0)))]);
    assert_eq!(h.start(timer::GAUGE_MAX), None);
    assert_eq!(quads(&h)[0].0, GRADE);
    // Without the slot, PERFECT's.
    let mut j = judge();
    j.grades[JudgeSlot::PerfectAtMax.index()] = None;
    let mut h = harness(skin_of(j));
    full(&mut h);
    h.frame(10, &[judgment(10, Judgment::Perfect, 1)]);
    assert_eq!(quads(&h)[0].0, GRADE);
}

#[test]
fn the_grade_and_the_combo_fall_back_independently() {
    let ten = combo(strip(), COMBO_KEY);
    let eleven = combo(
        DigitsDef {
            image: ELEVEN,
            set: GlyphSet::Eleven,
            ..strip()
        },
        COMBO_KEY,
    );
    let shown = |j: JudgeDef| {
        let mut h = harness(skin_of(j));
        h.tally = tally(7);
        h.gauges[0].value = GaugeValue::FULL;
        h.frame(0, &[ev(0, gauge_effect(0, GaugeValue::FULL))]);
        h.frame(10, &[judgment(10, Judgment::Perfect, 7)]);
        quads(&h).into_iter().map(|(i, ..)| i).collect::<Vec<_>>()
    };
    // Image 6, combo 0.
    let mut j = judge_all(GRADE_KEY);
    j.combos[0] = Some(ten.clone());
    assert_eq!(shown(j), [TEN, GRADE + 6]);
    // Image 0, combo 6.
    let mut j = judge_all(GRADE_KEY);
    j.grades[6] = None;
    j.combos[0] = Some(ten);
    j.combos[6] = Some(eleven);
    assert_eq!(shown(j), [ELEVEN, GRADE]);
}

#[test]
fn each_judgment_uses_its_own_slot() {
    let mut h = harness(skin_of(judge()));
    h.tally = tally(50);
    // POOR has no combo part: only its grade.
    h.frame(0, &[judgment(0, Judgment::Poor, 0)]);
    assert_eq!(quads(&h), [(GRADE + 4, 0, [800.0, 500.0, 200.0, 80.0])]);
    h.frame(10, &[ev(10, empty_poor(1))]);
    assert_eq!(quads(&h)[0].0, GRADE + 5);
    // An empty slot draws nothing.
    let mut j = empty_judge();
    j.grades[JudgeSlot::Great.index()] = Some(grade(GRADE, GRADE_KEY));
    let mut h = harness(skin_of(j));
    h.frame(0, &[judgment(0, Judgment::Bad, 0)]);
    assert!(h.batch.quads().is_empty());
    h.frame(10, &[judgment(10, Judgment::Great, 1)]);
    assert_eq!(h.batch.quads().len(), 1);
}

/// A rect track `[x, y, w, h]` from 0 to `to` under `end`.
fn timed(from: [i32; 4], to: [i32; 4], at: i64, end: End) -> Track {
    let key = |at, r: [i32; 4]| {
        let mut props = [None; 9];
        for (p, v) in props.iter_mut().zip(r) {
            *p = Some(v);
        }
        KeyDef {
            at: TimeUs(at),
            props,
        }
    };
    Track::new(&[key(0, from), key(at, to)], Ease::Linear, end).unwrap()
}

#[test]
fn a_part_s_track_runs_on_its_timer() {
    let mut j = judge();
    j.grades[JudgeSlot::Great.index()] = Some(GradePart {
        track: timed(GRADE_KEY, GRADE_KEY, 500 * MS, End::Hide),
        ..grade(great(), GRADE_KEY)
    });
    let mut h = harness(skin_of(j));
    let t0 = 1_000 * MS;
    h.frame_us(
        t0,
        &[ev(
            1_000,
            play_support::judged(1, 0, Judgment::Great, Some(0), None, 1),
        )],
    );
    assert_eq!(h.batch.quads().len(), 1);
    h.frame_us(t0 + 499_999, &[]);
    assert_eq!(h.batch.quads().len(), 1);
    // `Hide` draws while t ≤ the last key.
    h.frame_us(t0 + 500_000, &[]);
    assert_eq!(h.batch.quads().len(), 1);
    h.frame_us(t0 + 500_001, &[]);
    assert!(h.batch.quads().is_empty());
    // A second GREAT restarts it.
    h.frame(2_000, &[judgment(2_000, Judgment::Great, 2)]);
    assert_eq!(h.batch.quads().len(), 1);
    h.frame_us(2_000 * MS + 500_001, &[]);
    assert!(h.batch.quads().is_empty());
}

#[test]
fn the_combo_follows_its_attach_edge() {
    // The grade grows from 80 to 100 tall over 400 ms on `judge`, which
    // starts at the judgment at 1 000 ms, its bottom fixed at 580. The
    // combo moves its relative y from −70 to −50 over 2 000 ms on `ready`,
    // which starts at 0: −60 at 1 000 ms and −56 at 1 400 ms, where the
    // grade's clock would give −70 and −66.
    let shown = |attach: Attach| {
        let mut j = empty_judge();
        j.grades[1] = Some(GradePart {
            track: timed(GRADE_KEY, [800, 480, 200, 100], 400 * MS, End::Hold),
            ..grade(great(), GRADE_KEY)
        });
        j.combos[1] = Some(ComboPart {
            attach,
            track: timed(
                [210, -70, 20, 60],
                [210, -50, 20, 60],
                2_000 * MS,
                End::Hold,
            ),
            timer: TimerRef::Builtin(timer::READY),
            ..combo(strip(), [210, -70, 20, 60])
        });
        let mut h = harness(skin_of(j));
        h.tally = tally(5);
        h.frame(1_000, &[judgment(1_000, Judgment::Great, 5)]);
        let before = quads(&h)[0].2[1];
        h.frame(1_400, &[]);
        let after = quads(&h)[0].2[1];
        (before, after)
    };
    // Bottom: 580 − 60 and 580 − 56. Top: 500 − 60 and 480 − 56.
    assert_eq!(shown(Attach::Bottom), (520.0, 524.0));
    assert_eq!(shown(Attach::Top), (440.0, 424.0));
}

#[test]
fn each_part_draws_with_its_own_blend_and_visibility() {
    let mut j = judge();
    j.grades[1] = Some(GradePart {
        blend: Blend::Add,
        ..grade(great(), GRADE_KEY)
    });
    let mut h = harness(skin_of(j.clone()));
    h.tally = tally(5);
    h.frame(0, &[judgment(0, Judgment::Great, 5)]);
    let runs: Vec<_> = h
        .batch
        .runs()
        .iter()
        .map(|r| (r.key.blend, r.end - r.start))
        .collect();
    assert_eq!(runs, [(Blend::Alpha, 1), (Blend::Add, 1)]);
    // A hidden combo neither draws nor shifts.
    j.combos[1] = Some(ComboPart {
        visible: Some(boolean("combo > 100")),
        ..combo(strip(), COMBO_KEY)
    });
    let mut h = harness(skin_of(j));
    h.tally = tally(5);
    h.frame(0, &[judgment(0, Judgment::Great, 5)]);
    assert_eq!(quads(&h), [(great(), 0, [800.0, 500.0, 200.0, 80.0])]);
}

#[test]
fn the_element_color_multiplies_the_parts() {
    let mut e = judge_element(0);
    let mut props = [None; 9];
    props[4] = Some(51);
    e.track = lanes_support::still(props);
    let mut j = judge();
    let mut part_props = [None; 9];
    for (p, v) in part_props.iter_mut().zip(GRADE_KEY) {
        *p = Some(v);
    }
    part_props[4] = Some(85);
    j.grades[1] = Some(GradePart {
        track: lanes_support::still(part_props),
        ..grade(great(), GRADE_KEY)
    });
    let skin = load(
        vec![e],
        WidgetDefs {
            judges: vec![j],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.frame(0, &[judgment(0, Judgment::Great, 0)]);
    let alpha = h.batch.quads()[0].color[3];
    assert_eq!(alpha, (85.0_f32 / 255.0) * (51.0_f32 / 255.0));
}

/// A playfield image and the judge, for the region tests.
fn placed(j: JudgeDef) -> Skin {
    let field = Place {
        region: Region::Playfield,
        ..Place::SCREEN
    };
    load(
        vec![
            element(Source::Image(0), [100, 100, 400, 800], field),
            judge_element(0),
        ],
        WidgetDefs {
            judges: vec![j],
            ..WidgetDefs::default()
        },
    )
    .unwrap()
}

#[test]
fn the_judge_moves_with_the_playfield() {
    let adjust = |x, y, scale| Adjust {
        x,
        y,
        scale,
        visible: true,
    };
    let mut big = Layout::AUTHOR;
    big.set(Region::Judge, adjust(0, 0, 150)).unwrap();
    let mut moved = big;
    moved.set(Region::Playfield, adjust(100, -40, 100)).unwrap();
    let frame = |layout: Layout| {
        let mut h = harness(placed(judge()));
        h.layout = layout;
        h.tally = tally(123);
        h.frame(0, &[judgment(0, Judgment::Great, 123)]);
        drawn(&h)
            .into_iter()
            .filter(|&(i, ..)| i != 0)
            .map(|(.., q)| (q.rect, q.pivot))
            .collect::<Vec<_>>()
    };
    let (a, b) = (frame(big), frame(moved));
    assert_eq!(a.len(), 4);
    for (a, b) in a.iter().zip(&b) {
        assert_eq!([b.0[0] - a.0[0], b.0[1] - a.0[1]], [100.0, -40.0]);
        assert_eq!([b.0[2], b.0[3]], [a.0[2], a.0[3]]);
        assert_eq!([b.1[0] - a.1[0], b.1[1] - a.1[1]], [100.0, -40.0]);
    }
}

fn judge_bounds(skin: &Skin) -> [f32; 4] {
    let canvas: Canvas = skin.canvas([1_920, 1_080]);
    skin.placement(&canvas, &Layout::AUTHOR, Side::Left)
        .region(Region::Judge)
        .bounds
        .unwrap()
}

#[test]
fn the_judge_box_holds_every_effective_state() {
    // The grade's keys [800, 1000] widened by half the 4 cells' 80 on
    // each side, and the combo's strip [210, 290] × [10, 70] from the
    // corner (800, 500).
    assert_eq!(judge_bounds(&placed(judge())), [760.0, 500.0, 330.0, 80.0]);
    // A negative pitch moves the grade right: space −40 gives pitch −20,
    // so 123 is drawn −60 wide and the grade at 830..1030.
    let mut j = judge_all(GRADE_KEY);
    j.combos[1] = Some(combo(
        DigitsDef {
            space: -40,
            ..strip()
        },
        [-400, 10, 20, 60],
    ));
    let mut h = harness(skin_of(j.clone()));
    h.tally = tally(123);
    h.frame(0, &[judgment(0, Judgment::Great, 123)]);
    assert_eq!(quads(&h)[3].2, [830.0, 500.0, 200.0, 80.0]);
    let [x, _, w, _] = judge_bounds(&placed(j));
    assert!(x + w >= 1_030.0, "{}", x + w);
    // The full-gauge state resolves grade 0 with combo 6: 800 + 500 + 3·20
    // + 20.
    let mut j = empty_judge();
    j.grades[0] = Some(grade(GRADE, [800, 500, 100, 80]));
    j.combos[6] = Some(combo(
        DigitsDef {
            space: 0,
            align: Align::Right,
            ..strip()
        },
        [500, 0, 20, 60],
    ));
    j.shift = false;
    let [x, _, w, _] = judge_bounds(&placed(j));
    assert_eq!(x + w, 1_380.0);
}

#[test]
fn capacity_counts_the_largest_resolved_state() {
    let mut j = judge_all(GRADE_KEY);
    j.grades[6] = None;
    j.combos[0] = Some(combo(strip(), COMBO_KEY));
    j.combos[6] = Some(combo(
        DigitsDef {
            cells: 11,
            ..strip()
        },
        COMBO_KEY,
    ));
    assert_eq!(skin_of(j).quad_capacity(), 12);
    // The other way round: only the full-gauge PERFECT has a grade, and
    // its combo falls back to PERFECT's eleven cells.
    let mut j = empty_judge();
    j.grades[6] = Some(grade(GRADE + 6, GRADE_KEY));
    j.combos[0] = Some(combo(
        DigitsDef {
            cells: 11,
            ..strip()
        },
        COMBO_KEY,
    ));
    assert_eq!(skin_of(j).quad_capacity(), 12);
}

fn refused(elements: Vec<ElementDef>, j: JudgeDef) -> LoadError {
    load(
        elements,
        WidgetDefs {
            judges: vec![j],
            ..WidgetDefs::default()
        },
    )
    .unwrap_err()
}

#[test]
fn judges_sit_unbound_in_their_region() {
    for region in [Region::Screen, Region::Playfield] {
        let e = ElementDef {
            place: Place {
                region,
                ..Place::SCREEN
            },
            ..judge_element(0)
        };
        assert!(matches!(
            refused(vec![e], judge()),
            LoadError::JudgeRegion { element: 0 }
        ));
    }
    let fixed = |e: ElementDef, lanes: bool| {
        let mut elements = vec![e];
        if lanes {
            elements.push(ElementDef {
                place: Place::SCREEN,
                ..lanes_support::lanes_element()
            });
        }
        load_fit(
            Fit::Fixed,
            play_support::REGIONS,
            lanes.then(lanes_support::simple),
            elements,
            WidgetDefs {
                judges: vec![judge()],
                ..WidgetDefs::default()
            },
        )
    };
    let screen = ElementDef {
        place: Place::SCREEN,
        ..judge_element(0)
    };
    assert!(fixed(screen.clone(), false).is_ok());
    // Bound, beside lanes in the same region.
    let bound = ElementDef {
        place: Place {
            bind: Bind::Lanes,
            ..Place::SCREEN
        },
        ..screen
    };
    assert!(matches!(
        fixed(bound, true),
        Err(LoadError::Judge {
            element: 0,
            error: JudgeError::Bound
        })
    ));
    let moving = ElementDef {
        track: rect([1, 0, 0, 0]),
        ..judge_element(0)
    };
    assert!(matches!(
        refused(vec![moving], judge()),
        LoadError::WidgetGeometry { element: 0 }
    ));
}

fn judge_error(j: JudgeDef) -> JudgeError {
    match refused(vec![judge_element(0)], j) {
        LoadError::Judge { element: 0, error } => error,
        e => panic!("{e:?}"),
    }
}

#[test]
fn judge_parts_are_checked() {
    let mut combo_only = empty_judge();
    combo_only.combos[0] = Some(combo(strip(), COMBO_KEY));
    assert_eq!(judge_error(combo_only), JudgeError::Empty);
    let mut j = judge();
    j.combos[1] = Some(ComboPart {
        timer: TimerRef::When(0),
        ..combo(strip(), COMBO_KEY)
    });
    assert_eq!(
        judge_error(j),
        JudgeError::UnknownTimer {
            part: JudgePart::Combo(JudgeSlot::Great)
        }
    );
    let mut j = judge();
    j.grades[3] = Some(grade(999, GRADE_KEY));
    assert_eq!(
        judge_error(j),
        JudgeError::UnknownImage {
            slot: JudgeSlot::Bad
        }
    );
    let mut j = judge();
    j.grades[2] = Some(GradePart {
        visible: Some(widget_support::int("combo")),
        ..grade(GRADE, GRADE_KEY)
    });
    assert_eq!(
        judge_error(j),
        JudgeError::VisibleNotBool {
            part: JudgePart::Grade(JudgeSlot::Good)
        }
    );
    for (v, ok) in [
        (MAX_COORD, true),
        (MAX_COORD + 1, false),
        (-MAX_COORD - 1, false),
    ] {
        let mut j = judge();
        j.grades[4] = Some(grade(GRADE, [v, 0, 10, 10]));
        let got = load(
            vec![judge_element(0)],
            WidgetDefs {
                judges: vec![j],
                ..WidgetDefs::default()
            },
        );
        if ok {
            assert!(got.is_ok());
        } else {
            assert!(matches!(
                got,
                Err(LoadError::Judge {
                    error: JudgeError::Coordinates {
                        part: JudgePart::Grade(JudgeSlot::Poor)
                    },
                    ..
                })
            ));
        }
    }
    let mut j = judge();
    j.combos[1] = Some(combo(
        DigitsDef {
            cells: 0,
            ..strip()
        },
        COMBO_KEY,
    ));
    assert_eq!(
        judge_error(j),
        JudgeError::Combo {
            slot: JudgeSlot::Great,
            error: NumberError::Cells
        }
    );
}

/// A track of `n` keys, 1 µs apart, all at the grade key.
fn keys(n: usize) -> Track {
    let mut props = [None; 9];
    props[0] = Some(800);
    let keys: Vec<_> = (0..n)
        .map(|i| KeyDef {
            at: TimeUs(i64::try_from(i).unwrap()),
            props,
        })
        .collect();
    Track::new(&keys, Ease::Linear, End::Hold).unwrap()
}

#[test]
fn judge_part_keys_count_toward_the_skin_s_keys() {
    // One 1-key element; 255 full grade parts over judges of seven grade
    // parts each, and a combo part of `last` keys on the last judge, every
    // stored part counted once.
    let judges = |last: usize| -> Vec<JudgeDef> {
        let tracks: Vec<Track> = (0..255).map(|_| keys(MAX_TRACK_KEYS)).collect();
        let mut judges: Vec<JudgeDef> = tracks
            .chunks(7)
            .map(|c| {
                let mut j = empty_judge();
                for (slot, t) in c.iter().enumerate() {
                    j.grades[slot] = Some(GradePart {
                        track: t.clone(),
                        ..grade(GRADE, GRADE_KEY)
                    });
                }
                j
            })
            .collect();
        if let Some(j) = judges.last_mut() {
            j.combos[0] = Some(ComboPart {
                track: keys(last),
                ..combo(strip(), COMBO_KEY)
            });
        }
        judges
    };
    let with = |last| {
        load(
            vec![judge_element(0)],
            WidgetDefs {
                judges: judges(last),
                ..WidgetDefs::default()
            },
        )
    };
    // The element alone is far below the cap; with the parts, exactly at
    // it.
    assert_eq!(255 * MAX_TRACK_KEYS + 255 + 1, MAX_SKIN_KEYS);
    assert!(with(255).is_ok());
    assert!(matches!(
        with(256),
        Err(LoadError::TooManyKeys { n }) if n == MAX_SKIN_KEYS + 1
    ));
}

#[test]
fn a_judge_part_s_pivot_turns_it() {
    let mut j = judge();
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip(GRADE_KEY) {
        *p = Some(v);
    }
    props[8] = Some(180);
    j.grades[1] = Some(GradePart {
        track: lanes_support::still(props),
        pivot: Anchor::BottomRight,
        ..grade(great(), GRADE_KEY)
    });
    let mut h = harness(skin_of(j));
    h.frame(0, &[judgment(0, Judgment::Great, 0)]);
    let q = h.batch.quads()[0];
    assert_eq!(q.pivot, [1_000.0, 580.0]);
    assert_eq!(q.angle, std::f32::consts::PI);
}

/// Three ten-glyph cells, centred, blank-padded, space 0.
fn three() -> DigitsDef {
    DigitsDef {
        space: 0,
        ..digits(TEN, GlyphSet::Ten, 3, Align::Center, Pad::Blank)
    }
}

/// GREAT's grade painting `paint`, its combo of [`three`] attached to the
/// bottom of `attachment` at the relative key `(−5, −20, 10, 20)`.
fn attached(paint: [i32; 4], attachment: Option<[i32; 4]>) -> JudgeDef {
    let mut j = empty_judge();
    j.grades[1] = Some(GradePart {
        attachment: attachment.map(rect),
        ..grade(great(), paint)
    });
    j.combos[1] = Some(ComboPart {
        attach: Attach::Bottom,
        ..combo(three(), [-5, -20, 10, 20])
    });
    j
}

/// What `j` draws on a GREAT at 0 ms with combo 12.
fn great_12(j: JudgeDef) -> Vec<(u32, u16, [f32; 4])> {
    let mut h = harness(skin_of(j));
    h.tally = tally(12);
    h.frame(0, &[judgment(0, Judgment::Great, 12)]);
    quads(&h)
}

#[test]
fn a_combo_attaches_to_the_attachment_rect() {
    // A square image fitted inside [100, 500, 80, 20] paints
    // [130, 500, 20, 20]; the combo attaches to the unfitted rect's
    // bottom-left: "12" centred in three 10 px cells from 95 draws at 100
    // and 110, 520 − 20 = 500. The baseline width 20 shifts the paint
    // alone, by 10.
    let cells = [
        (TEN, 1, [100.0, 500.0, 10.0, 20.0]),
        (TEN, 2, [110.0, 500.0, 10.0, 20.0]),
    ];
    let paint = [130, 500, 20, 20];
    let raw = Some([100, 500, 80, 20]);
    let shown = great_12(attached(paint, raw));
    assert_eq!(shown[..2], cells);
    assert_eq!(shown[2], (great(), 0, [120.0, 500.0, 20.0, 20.0]));
    let mut j = attached(paint, raw);
    j.shift = false;
    let shown = great_12(j);
    assert_eq!(shown[..2], cells);
    assert_eq!(shown[2], (great(), 0, [130.0, 500.0, 20.0, 20.0]));
    // Without an attachment, the combo attaches to the paint: from 130.
    let shown = great_12(attached(paint, None));
    assert_eq!(shown[0].2, [130.0, 500.0, 10.0, 20.0]);
    // The grade's native size and rotation change its paint alone: an
    // 8×8 frame centred in the shifted paint, turned 90°.
    let mut j = attached(paint, raw);
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip(paint) {
        *p = Some(v);
    }
    props[8] = Some(90);
    if let Some(g) = j.grades[1].as_mut() {
        g.track = lanes_support::still(props);
        g.native = Some(skin_model::Native::Exact([8, 8]));
    }
    let mut h = harness(skin_of(j));
    h.tally = tally(12);
    h.frame(0, &[judgment(0, Judgment::Great, 12)]);
    assert_eq!(quads(&h)[..2], cells);
    let q = h.batch.quads();
    assert_eq!([q[0].angle, q[1].angle], [0.0, 0.0]);
    assert_eq!(q[2].rect, [126.0, 506.0, 8.0, 8.0]);
    assert_eq!(q[2].angle, std::f32::consts::FRAC_PI_2);
}

#[test]
fn an_attached_judge_scales_with_its_region() {
    // At a judge scale of 200 % every offset between the parts doubles:
    // the attachment and the shift are placed before the region's map.
    let frame = |scale: u16| {
        let mut layout = Layout::AUTHOR;
        layout
            .set(
                Region::Judge,
                Adjust {
                    x: 0,
                    y: 0,
                    scale,
                    visible: true,
                },
            )
            .unwrap();
        let j = attached([130, 500, 20, 20], Some([100, 500, 80, 20]));
        let mut h = harness(placed(j));
        h.layout = layout;
        h.tally = tally(12);
        h.frame(0, &[judgment(0, Judgment::Great, 12)]);
        drawn(&h)
            .into_iter()
            .filter(|&(i, ..)| i != 0)
            .map(|(.., q)| q.rect)
            .collect::<Vec<_>>()
    };
    let (one, two) = (frame(100), frame(200));
    assert_eq!(one.len(), 3);
    // Each quad from the grade's corner.
    let from_grade = |r: &[[f32; 4]]| -> Vec<[f32; 4]> {
        r.iter()
            .map(|q| [q[0] - r[2][0], q[1] - r[2][1], q[2], q[3]])
            .collect()
    };
    let doubled: Vec<[f32; 4]> = from_grade(&one)
        .into_iter()
        .map(|q| q.map(|v| 2.0 * v))
        .collect();
    assert_eq!(from_grade(&two), doubled);
}

/// A rect track over `rects`, the i-th key at `times[i]`, under `ease`
/// and `end`.
fn keyed(times: &[i64], rects: &[[i32; 4]], ease: Ease, end: End) -> Track {
    let keys: Vec<KeyDef> = times
        .iter()
        .zip(rects)
        .map(|(&at, r)| {
            let mut props = [None; 9];
            for (p, v) in props.iter_mut().zip(*r) {
                *p = Some(v);
            }
            KeyDef {
                at: TimeUs(at),
                props,
            }
        })
        .collect();
    Track::new(&keys, ease, end).unwrap()
}

#[test]
fn the_attachment_runs_on_the_grade_s_clock_and_the_combo_on_its_own() {
    // The grade on `judge` from the GREAT at 1 000 ms; its paint and its
    // attachment move over 300 ms, the combo on `ready` over 2 000 ms.
    // Each frame the attachment is sampled at the grade's elapsed time and
    // the combo's relative key at its own, through every ease and end:
    // across the hold, the hide after the last key and the loop from
    // 100 ms.
    let times = [0, 100 * MS, 300 * MS];
    let combo_track = keyed(
        &[0, 2_000 * MS],
        &[[-5, -20, 10, 20], [15, -40, 10, 20]],
        Ease::Linear,
        End::Hold,
    );
    for ease in [Ease::Linear, Ease::In, Ease::Out, Ease::Step] {
        for end in [
            End::Hold,
            End::Hide,
            End::Loop {
                from: TimeUs(100 * MS),
            },
        ] {
            let paint = keyed(
                &times,
                &[[130, 500, 20, 20], [150, 470, 20, 40], [230, 400, 20, 40]],
                ease,
                end,
            );
            let raw = keyed(
                &times,
                &[[100, 500, 80, 20], [90, 460, 120, 60], [200, 380, 80, 60]],
                ease,
                end,
            );
            let mut j = attached([0; 4], None);
            if let Some(g) = j.grades[1].as_mut() {
                g.track = paint.clone();
                g.attachment = Some(raw.clone());
            }
            if let Some(c) = j.combos[1].as_mut() {
                c.track = combo_track.clone();
                c.timer = TimerRef::Builtin(timer::READY);
            }
            let mut h = harness(skin_of(j));
            h.tally = tally(12);
            h.frame(1_000, &[judgment(1_000, Judgment::Great, 12)]);
            let ready = h.start(timer::READY).unwrap();
            for ms in [1_000, 1_050, 1_100, 1_250, 1_300, 1_301, 1_450, 1_700] {
                h.frame(ms, &[]);
                let now = ms * MS;
                let gt = now - 1_000 * MS;
                let got = quads(&h);
                let Some(p) = paint.sample(gt) else {
                    assert!(got.is_empty(), "{ease:?} {end:?} {ms}");
                    continue;
                };
                let a = raw.sample(gt).unwrap().rect;
                let c = combo_track.sample(now - ready).unwrap().rect;
                // "12" in three cells of width c[2], centred: one blank
                // cell, halved. The baseline width 2·c[2] shifts the
                // paint by c[2].
                let x0 = a[0] + c[0] + c[2] / 2.0;
                let y = a[1] + a[3] + c[1];
                let want = [
                    [x0, y, c[2], c[3]],
                    [x0 + c[2], y, c[2], c[3]],
                    [p.rect[0] - c[2], p.rect[1], p.rect[2], p.rect[3]],
                ];
                assert_eq!(got.len(), 3, "{ease:?} {end:?} {ms}");
                for ((.., r), w) in got.iter().zip(want) {
                    for (g, w) in r.iter().zip(w) {
                        assert!((g - w).abs() < 1e-3, "{ease:?} {end:?} {ms}: {r:?} {w:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn attachments_fall_back_with_their_grades() {
    let shown = |j: JudgeDef| {
        let mut h = harness(skin_of(j));
        h.tally = tally(12);
        h.gauges[0].value = GaugeValue::FULL;
        h.frame(0, &[ev(0, gauge_effect(0, GaugeValue::FULL))]);
        h.frame(10, &[judgment(10, Judgment::Perfect, 12)]);
        quads(&h)
    };
    let with = |image: u32, raw: [i32; 4]| GradePart {
        attachment: Some(rect(raw)),
        ..grade(image, [130, 500, 20, 20])
    };
    let three_at = |attach| ComboPart {
        attach,
        ..combo(three(), [-5, -20, 10, 20])
    };
    // The full-gauge grade with PERFECT's combo: the chosen grade's
    // attachment places it.
    let mut j = empty_judge();
    j.grades[0] = Some(with(GRADE, [0, 0, 10, 10]));
    j.grades[6] = Some(with(GRADE + 6, [300, 600, 80, 20]));
    j.combos[0] = Some(three_at(Attach::Bottom));
    let q = shown(j);
    assert_eq!(q[0].2, [300.0, 600.0, 10.0, 20.0]);
    assert_eq!(q[2].0, GRADE + 6);
    // PERFECT's grade standing in, with the full-gauge combo.
    let mut j = empty_judge();
    j.grades[0] = Some(with(GRADE, [300, 600, 80, 20]));
    j.combos[0] = Some(three_at(Attach::Top));
    j.combos[6] = Some(three_at(Attach::Bottom));
    let q = shown(j);
    assert_eq!(q[0].2, [300.0, 600.0, 10.0, 20.0]);
    assert_eq!(q[2].0, GRADE);
}

#[test]
fn an_attachment_moves_the_judge_box_through_its_combo() {
    // The grade's paint [800, 1000] widened by 40 each side; the combo's
    // strip [210, 290] from the attachment's corner at 400, not the
    // paint's at 800; the attachment's own box [400, 600] is not drawn,
    // so not included.
    let mut j = judge();
    j.grades[1] = Some(GradePart {
        attachment: Some(rect([400, 500, 200, 80])),
        ..grade(great(), GRADE_KEY)
    });
    assert_eq!(judge_bounds(&placed(j)), [610.0, 500.0, 430.0, 80.0]);
    // From the bottom edge of a taller attachment: 500 + 100 + 70.
    let mut j = judge();
    j.grades[1] = Some(GradePart {
        attachment: Some(rect([800, 500, 200, 100])),
        ..grade(great(), GRADE_KEY)
    });
    if let Some(c) = j.combos[1].as_mut() {
        c.attach = Attach::Bottom;
    }
    assert_eq!(judge_bounds(&placed(j)), [760.0, 500.0, 330.0, 170.0]);
}

fn loads(judges: Vec<JudgeDef>) -> bool {
    load(
        vec![judge_element(0)],
        WidgetDefs {
            judges,
            ..WidgetDefs::default()
        },
    )
    .is_ok()
}

#[test]
fn attachments_are_checked_on_the_judges_drawn() {
    let with = |raw: Track| {
        let mut j = judge();
        j.grades[2] = Some(GradePart {
            attachment: Some(raw),
            ..grade(GRADE + 2, GRADE_KEY)
        });
        j
    };
    // The grade's own timing with other geometry loads.
    assert!(loads(vec![with(rect([0, 0, 10, 10]))]));
    // Other key times, ease or end refuse.
    let times = [0, 100 * MS];
    let two = [GRADE_KEY, GRADE_KEY];
    let timed_grade = keyed(&times, &two, Ease::Linear, End::Hold);
    for raw in [
        keyed(&[0, 101 * MS], &two, Ease::Linear, End::Hold),
        keyed(&times, &two, Ease::In, End::Hold),
        keyed(&times, &two, Ease::Linear, End::Hide),
        rect(GRADE_KEY),
    ] {
        let mut j = with(raw);
        if let Some(g) = j.grades[2].as_mut() {
            g.track = timed_grade.clone();
        }
        assert_eq!(
            judge_error(j.clone()),
            JudgeError::AttachmentTiming {
                slot: JudgeSlot::Good
            }
        );
        // An unreferenced judge is not checked.
        assert!(loads(vec![judge(), j]));
    }
    // Its coordinates stay within ±MAX_COORD.
    for (v, ok) in [
        (MAX_COORD, true),
        (MAX_COORD + 1, false),
        (-MAX_COORD - 1, false),
    ] {
        let j = with(rect([0, v, 10, 10]));
        if ok {
            assert!(loads(vec![j]));
        } else {
            assert_eq!(
                judge_error(j),
                JudgeError::Coordinates {
                    part: JudgePart::Attachment(JudgeSlot::Good)
                }
            );
        }
    }
}

#[test]
fn attachment_keys_count_toward_the_skin_s_keys_referenced_or_not() {
    // One 1-key element and its judge's 1-key grade; 127 unreferenced
    // judges whose grade and attachment have 256 keys each, and one whose
    // grade has 255 and attachment `last`, unchecked though its timing
    // differs when `last` is 256.
    let judges = |last: usize| -> Vec<JudgeDef> {
        let part = |grade_keys: usize, attachment_keys: usize| {
            let mut j = empty_judge();
            j.grades[0] = Some(GradePart {
                track: keys(grade_keys),
                attachment: Some(keys(attachment_keys)),
                ..grade(GRADE, GRADE_KEY)
            });
            j
        };
        let mut first = empty_judge();
        first.grades[0] = Some(grade(GRADE, GRADE_KEY));
        let mut judges = vec![first];
        judges.extend((0..127).map(|_| part(MAX_TRACK_KEYS, MAX_TRACK_KEYS)));
        judges.push(part(MAX_TRACK_KEYS - 1, last));
        judges
    };
    let with = |last| {
        load(
            vec![judge_element(0)],
            WidgetDefs {
                judges: judges(last),
                ..WidgetDefs::default()
            },
        )
    };
    assert_eq!(1 + 1 + 127 * 512 + 255 + 255, MAX_SKIN_KEYS);
    assert!(with(255).is_ok());
    assert!(matches!(
        with(256),
        Err(LoadError::TooManyKeys { n }) if n == MAX_SKIN_KEYS + 1
    ));
}
