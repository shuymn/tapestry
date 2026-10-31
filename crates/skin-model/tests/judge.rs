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
    Adjust, Align, Anchor, Attach, Bind, Blend, Canvas, ComboPart, DigitsDef, Ease, ElementDef,
    End, Fit, GlyphSet, GradePart, JudgeDef, JudgeError, JudgePart, JudgeSlot, KeyDef, Layout,
    LoadError, MAX_COORD, MAX_SKIN_KEYS, MAX_TRACK_KEYS, NumberError, Pad, Place, Region, Side,
    Skin, Source, TimerRef, Track, WidgetDefs, timer,
};
use widget_support::{
    ELEVEN, GRADE, TEN, boolean, combo, digits, drawn, element, empty_judge, grade, harness,
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
    // The grade grows from 80 to 100 tall over 400 ms on `judge`, its
    // bottom fixed at 580; the combo is still, on `ready`.
    let shown = |attach: Attach| {
        let mut j = empty_judge();
        j.grades[1] = Some(GradePart {
            track: timed(GRADE_KEY, [800, 480, 200, 100], 400 * MS, End::Hold),
            ..grade(great(), GRADE_KEY)
        });
        j.combos[1] = Some(ComboPart {
            attach,
            timer: TimerRef::Builtin(timer::READY),
            ..combo(strip(), [210, -70, 20, 60])
        });
        let mut h = harness(skin_of(j));
        h.tally = tally(5);
        h.frame(0, &[judgment(0, Judgment::Great, 5)]);
        let before = quads(&h)[0].2[1];
        h.frame(400, &[]);
        let after = quads(&h)[0].2[1];
        (before, after)
    };
    assert_eq!(shown(Attach::Bottom), (510.0, 510.0));
    assert_eq!(shown(Attach::Top), (430.0, 410.0));
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
    // One 1-key element; 255 full parts and one of `last` keys, over
    // judges of seven grade parts each, every stored part counted once.
    let judges = |last: usize| -> Vec<JudgeDef> {
        let mut tracks: Vec<Track> = (0..255).map(|_| keys(MAX_TRACK_KEYS)).collect();
        tracks.push(keys(last));
        tracks
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
            .collect()
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
