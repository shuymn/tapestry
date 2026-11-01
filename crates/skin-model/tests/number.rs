//! The `number` widget: the glyph layout (alignment, padding, overflow,
//! signs, the `i32` extremes), glyph sets on the image's cycle, per-cell
//! pivots, binding and regions, its load errors and its operations.
//!
//! Unless stated: a number at x = 100, y = 50, 20 wide and 30 tall, space
//! 2 (pitch 22), 4 cells, at the author's layout on 1920×1080.

mod lanes_support;
mod play_support;
mod widget_support;

use rules::Judgment;
use skin_model::{
    Adjust, Align, Anchor, Bind, Blend, DigitOffset, DigitsDef, ElementDef, Fit, Follow, GlyphSet,
    JudgeSlot, Layout, LoadError, MAX_COORD, MAX_DIGIT_OFFSETS, MAX_SKIN_OPS, Native, NumberError,
    Pad, Place, Region, Skin, Source, TimerRef, Track, WidgetDefs, timer,
};
use widget_support::{
    ELEVEN, TEN, TEN_CYCLED, TWENTY_FIVE, TWENTY_FOUR, boolean, digits, drawn, element, glyphs,
    harness, int, judgment, load, number,
};

const RECT: [i32; 4] = [100, 50, 20, 30];

/// A skin of one number element over `d` showing `src`, placed by
/// `place`.
fn skin_of(d: DigitsDef, src: &str, place: Place) -> Result<Skin, LoadError> {
    load(
        vec![element(Source::Number(0), RECT, place)],
        WidgetDefs {
            numbers: vec![number(d, src)],
            ..WidgetDefs::default()
        },
    )
}

/// The glyphs and x of `d` showing `src` at 0 ms.
fn show(d: DigitsDef, src: &str) -> Vec<(u16, f32)> {
    let mut h = harness(skin_of(d, src, Place::SCREEN).unwrap());
    h.frame(0, &[]);
    glyphs(&h)
}

fn ten(align: Align, pad: Pad) -> DigitsDef {
    digits(TEN, GlyphSet::Ten, 4, align, pad)
}

fn signed(pad: Pad) -> DigitsDef {
    digits(TWENTY_FOUR, GlyphSet::TwentyFour, 4, Align::Right, pad)
}

#[test]
fn blank_cells_move_the_drawn_cells_by_the_alignment() {
    // 7 leaves 3 blank cells: right draws cell 3 at 100 + 3·22.
    assert_eq!(show(ten(Align::Right, Pad::Blank), "7"), [(7, 166.0)]);
    assert_eq!(show(ten(Align::Left, Pad::Blank), "7"), [(7, 100.0)]);
    // Centre: half the 3 blanks, 166 − 33. An integer half (1 cell) would
    // draw it at 144.
    assert_eq!(show(ten(Align::Center, Pad::Blank), "7"), [(7, 133.0)]);
    assert_eq!(
        show(ten(Align::Center, Pad::Blank), "42"),
        [(4, 122.0), (2, 144.0)]
    );
    // Pitch 21 (space 1): 100 + 63 − 31.5.
    let odd = DigitsDef {
        space: 1,
        ..ten(Align::Center, Pad::Blank)
    };
    assert_eq!(show(odd, "7"), [(7, 131.5)]);
}

#[test]
fn padding_fills_the_cells_left_of_the_digits() {
    assert_eq!(
        show(ten(Align::Right, Pad::Zero), "42"),
        [(0, 100.0), (0, 122.0), (4, 144.0), (2, 166.0)]
    );
    let glyph = digits(ELEVEN, GlyphSet::Eleven, 4, Align::Right, Pad::Glyph);
    assert_eq!(
        show(glyph, "42"),
        [(10, 100.0), (10, 122.0), (4, 144.0), (2, 166.0)]
    );
    // The last cell always shows a digit.
    assert_eq!(show(ten(Align::Right, Pad::Blank), "0"), [(0, 166.0)]);
}

#[test]
fn a_value_longer_than_the_strip_keeps_its_low_digits() {
    // Left-aligned, so a blank cell would move the cells left of 100.
    let left = ten(Align::Left, Pad::Blank);
    let at = |f: [u16; 4]| -> Vec<(u16, f32)> {
        f.into_iter().zip([100.0, 122.0, 144.0, 166.0]).collect()
    };
    assert_eq!(show(left.clone(), "9999"), at([9, 9, 9, 9]));
    assert_eq!(show(left.clone(), "10000"), at([0, 0, 0, 0]));
    assert_eq!(show(left.clone(), "12345"), at([2, 3, 4, 5]));
}

#[test]
fn blank_padding_puts_the_sign_in_the_first_blank() {
    let d = signed(Pad::Blank);
    // The negative half is 12 on: "−" is 23.
    assert_eq!(
        show(d.clone(), "-42"),
        [(23, 122.0), (16, 144.0), (14, 166.0)]
    );
    assert_eq!(show(d.clone(), "42"), [(11, 122.0), (4, 144.0), (2, 166.0)]);
    // Boundary: −999 leaves one blank for the sign; −1000 none.
    assert_eq!(
        show(d.clone(), "-999"),
        [(23, 100.0), (21, 122.0), (21, 144.0), (21, 166.0)]
    );
    assert_eq!(
        show(d.clone(), "-1000"),
        [(13, 100.0), (12, 122.0), (12, 144.0), (12, 166.0)]
    );
}

#[test]
fn zero_padding_keeps_cell_0_for_the_sign() {
    let d = signed(Pad::Zero);
    assert_eq!(
        show(d.clone(), "-42"),
        [(23, 100.0), (12, 122.0), (16, 144.0), (14, 166.0)]
    );
    assert_eq!(
        show(d.clone(), "-1234"),
        [(23, 100.0), (14, 122.0), (15, 144.0), (16, 166.0)]
    );
}

#[test]
fn the_i32_extremes_show_every_digit() {
    let min = digits(
        TWENTY_FOUR,
        GlyphSet::TwentyFour,
        11,
        Align::Right,
        Pad::Zero,
    );
    let frames: Vec<u16> = show(min, "-2147483647 - 1")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    let digits_of = |s: &str, half: u16| -> Vec<u16> {
        s.bytes().map(|b| u16::from(b - b'0') + half).collect()
    };
    let mut want = vec![23];
    want.extend(digits_of("2147483648", 12));
    assert_eq!(frames, want);
    let max = digits(TEN, GlyphSet::Ten, 10, Align::Right, Pad::Blank);
    let frames: Vec<u16> = show(max, "2147483647")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    assert_eq!(frames, digits_of("2147483647", 0));
    // Without signed glyphs a negative value shows its magnitude.
    assert_eq!(show(ten(Align::Right, Pad::Blank), "-5"), [(5, 166.0)]);
}

#[test]
fn glyph_sets_follow_the_image_cycle() {
    // Two sets of ten over 100 ms on `judge`: set 1 (frames 10–19) shows
    // from 50 ms.
    let d = digits(TEN_CYCLED, GlyphSet::Ten, 4, Align::Right, Pad::Blank);
    let mut h = harness(skin_of(d, "7", Place::SCREEN).unwrap());
    // The element is on `ready`; the cycle's timer is still off.
    h.frame(500, &[]);
    assert_eq!(h.start(timer::JUDGE), None);
    assert_eq!(glyphs(&h), [(7, 166.0)]);
    h.frame(1_000, &[judgment(1_000, Judgment::Great, 1)]);
    assert_eq!(glyphs(&h), [(7, 166.0)]);
    h.frame_us(1_049_999, &[]);
    assert_eq!(glyphs(&h), [(7, 166.0)]);
    h.frame_us(1_050_000, &[]);
    assert_eq!(glyphs(&h), [(17, 166.0)]);
    h.frame_us(1_100_000, &[]);
    assert_eq!(glyphs(&h), [(7, 166.0)]);
}

#[test]
fn each_cell_turns_about_its_own_pivot() {
    let mut track_props = [None; 9];
    for (p, v) in track_props.iter_mut().zip(RECT) {
        *p = Some(v);
    }
    track_props[8] = Some(90);
    let e = ElementDef {
        track: lanes_support::still(track_props),
        place: Place {
            pivot: Anchor::Center,
            ..Place::SCREEN
        },
        ..element(Source::Number(0), RECT, Place::SCREEN)
    };
    let skin = load(
        vec![e],
        WidgetDefs {
            numbers: vec![number(ten(Align::Right, Pad::Blank), "42")],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.frame(0, &[]);
    let quads: Vec<_> = drawn(&h).into_iter().map(|(_, _, q)| q).collect();
    assert_eq!(quads.len(), 2);
    assert_eq!(quads[0].rect, [144.0, 50.0, 20.0, 30.0]);
    assert_eq!(quads[0].pivot, [154.0, 65.0]);
    assert_eq!(quads[1].pivot, [176.0, 65.0]);
    for q in quads {
        assert_eq!(q.angle, std::f32::consts::FRAC_PI_2);
    }
}

#[test]
fn a_bound_number_spaces_its_cells_with_the_lane_width() {
    // The simple lanes span 160..704; anchored left, 150 % scales them
    // about 160.
    let mut regions = play_support::REGIONS;
    regions[Region::Playfield.index()].anchor = Anchor::Left;
    let bound = Place {
        region: Region::Playfield,
        bind: Bind::Lanes,
        ..Place::SCREEN
    };
    let skin = widget_support::load_fit(
        skin_model::Fit::Adapt,
        regions,
        Some(lanes_support::simple()),
        vec![
            lanes_support::lanes_element(),
            element(Source::Number(0), [200, 50, 20, 30], bound),
        ],
        WidgetDefs {
            numbers: vec![number(ten(Align::Right, Pad::Blank), "42")],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.layout.set_lane_width(150).unwrap();
    h.frame(0, &[]);
    let cells: Vec<_> = drawn(&h)
        .into_iter()
        .filter(|&(i, ..)| i == TEN)
        .map(|(_, f, q)| (f, q.rect))
        .collect();
    // Authored at 244 and 266: 160 + 84·1.5 and 160 + 106·1.5, 30 wide.
    assert_eq!(
        cells,
        [
            (4, [286.0, 50.0, 30.0, 30.0]),
            (2, [319.0, 50.0, 30.0, 30.0])
        ]
    );
}

#[test]
fn a_number_moves_with_its_region() {
    let mut regions = play_support::REGIONS;
    regions[Region::Right.index()].anchor = Anchor::Right;
    let place = Place {
        region: Region::Right,
        ..Place::SCREEN
    };
    let skin = widget_support::load_fit(
        skin_model::Fit::Adapt,
        regions,
        None,
        vec![element(Source::Number(0), RECT, place)],
        WidgetDefs {
            numbers: vec![number(ten(Align::Right, Pad::Blank), "42")],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.frame(0, &[]);
    assert_eq!(glyphs(&h), [(4, 144.0), (2, 166.0)]);
    h.target = [2_560, 1_080];
    h.frame(0, &[]);
    assert_eq!(glyphs(&h), [(4, 784.0), (2, 806.0)]);
}

fn refused(d: DigitsDef, src: &str) -> Option<NumberError> {
    match skin_of(d, src, Place::SCREEN) {
        Ok(_) => None,
        Err(LoadError::Number { element: 0, error }) => Some(error),
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn strips_are_checked_at_their_limits() {
    let base = ten(Align::Right, Pad::Blank);
    let cells = |n| DigitsDef {
        cells: n,
        ..base.clone()
    };
    assert_eq!(refused(cells(11), "1"), None);
    assert_eq!(refused(cells(12), "1"), Some(NumberError::Cells));
    assert_eq!(refused(cells(0), "1"), Some(NumberError::Cells));
    let space = |s| DigitsDef {
        space: s,
        ..base.clone()
    };
    assert_eq!(refused(space(MAX_COORD), "1"), None);
    assert_eq!(refused(space(-MAX_COORD), "1"), None);
    assert_eq!(refused(space(MAX_COORD + 1), "1"), Some(NumberError::Space));
    assert_eq!(
        refused(space(-MAX_COORD - 1), "1"),
        Some(NumberError::Space)
    );
    let tf = |image, pad, n| DigitsDef {
        image,
        cells: n,
        ..signed(pad)
    };
    assert_eq!(
        refused(tf(TWENTY_FIVE, Pad::Blank, 4), "1"),
        Some(NumberError::Glyphs)
    );
    assert_eq!(
        refused(tf(TWENTY_FOUR, Pad::Zero, 1), "1"),
        Some(NumberError::SignCells)
    );
    assert_eq!(refused(tf(TWENTY_FOUR, Pad::Zero, 2), "1"), None);
    assert_eq!(refused(tf(TWENTY_FOUR, Pad::Blank, 1), "1"), None);
    assert_eq!(
        refused(
            DigitsDef {
                pad: Pad::Glyph,
                ..base.clone()
            },
            "1"
        ),
        Some(NumberError::PadGlyph)
    );
    assert_eq!(
        refused(
            DigitsDef {
                image: 999,
                ..base.clone()
            },
            "1"
        ),
        Some(NumberError::UnknownImage)
    );
    let not_int = load(
        vec![element(Source::Number(0), RECT, Place::SCREEN)],
        WidgetDefs {
            numbers: vec![skin_model::NumberDef {
                digits: base.clone(),
                value: boolean("combo > 1"),
            }],
            ..WidgetDefs::default()
        },
    );
    assert!(matches!(
        not_int,
        Err(LoadError::Number {
            element: 0,
            error: NumberError::NotInt
        })
    ));
    let unknown = load(
        vec![element(Source::Number(1), RECT, Place::SCREEN)],
        WidgetDefs {
            numbers: vec![number(base.clone(), "1")],
            ..WidgetDefs::default()
        },
    );
    assert!(matches!(
        unknown,
        Err(LoadError::UnknownWidget { element: 0 })
    ));
}

#[test]
fn each_element_is_charged_its_number_value_and_judge_parts() {
    // A 255-op value shared by `n` number elements, a judge whose grade
    // and combo visibilities take 127 ops each shared by two judge
    // elements, and visibilities of one op for the rest.
    let value = int(&widget_support::sum(128));
    assert_eq!(value.op_count(), 255);
    let part = boolean(&format!("{} > 0", widget_support::sum(64)));
    let p = part.op_count();
    let one = boolean("true");
    assert_eq!(one.op_count(), 1);
    let n = (MAX_SKIN_OPS - 4 * p) / 255;
    let rest = MAX_SKIN_OPS - 4 * p - n * 255;
    let elements = |extra: usize| {
        let mut out: Vec<ElementDef> = (0..n)
            .map(|_| element(Source::Number(0), RECT, Place::SCREEN))
            .collect();
        out.push(widget_support::judge_element(0));
        out.push(widget_support::judge_element(0));
        out.extend((0..rest + extra).map(|_| ElementDef {
            visible: Some(one.clone()),
            ..element(Source::Image(0), [0, 0, 1, 1], Place::SCREEN)
        }));
        out
    };
    let widgets = || {
        let mut judge = widget_support::empty_judge();
        judge.grades[0] = Some(skin_model::GradePart {
            visible: Some(part.clone()),
            ..widget_support::grade(widget_support::GRADE, [0, 0, 10, 10])
        });
        judge.combos[0] = Some(skin_model::ComboPart {
            visible: Some(part.clone()),
            ..widget_support::combo(ten(Align::Right, Pad::Blank), [0, 0, 10, 10])
        });
        WidgetDefs {
            numbers: vec![skin_model::NumberDef {
                digits: ten(Align::Right, Pad::Blank),
                value: value.clone(),
            }],
            judges: vec![judge],
            ..WidgetDefs::default()
        }
    };
    let skin = load(elements(0), widgets()).unwrap();
    // Each shared program counted per element that evaluates it.
    assert_eq!(skin.ops().total(), MAX_SKIN_OPS);
    assert_eq!(n * 255 + 2 * (2 * p) + rest, MAX_SKIN_OPS);
    assert!(matches!(
        load(elements(1), widgets()),
        Err(LoadError::Ops(_))
    ));
    // One number element and one judge element charge each once.
    let single = load(
        vec![
            element(Source::Number(0), RECT, Place::SCREEN),
            widget_support::judge_element(0),
        ],
        widgets(),
    )
    .unwrap();
    assert_eq!(single.ops().total(), 255 + 2 * p);
}

#[test]
fn a_number_element_s_own_gates_still_apply() {
    // Its timer and visibility gate it like any element.
    let e = ElementDef {
        timer: TimerRef::Builtin(timer::JUDGE),
        blend: Blend::Add,
        ..element(Source::Number(0), RECT, Place::SCREEN)
    };
    let skin = load(
        vec![e],
        WidgetDefs {
            numbers: vec![number(ten(Align::Right, Pad::Blank), "combo")],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.tally = play_support::tally(5);
    h.frame(0, &[]);
    assert!(h.batch.quads().is_empty());
    h.frame(10, &[judgment(10, Judgment::Great, 5)]);
    assert_eq!(glyphs(&h), [(5, 166.0)]);
    assert_eq!(h.batch.runs()[0].key.blend, Blend::Add);
}

/// The source's row `{x: 3, y: 5, w: 4, h: 6}` as the importer lowers it.
const MOVE: DigitOffset = DigitOffset {
    dx: 3,
    dy: -11,
    dw: 4,
    dh: 6,
};

/// `d` with `offsets`.
fn moved(d: DigitsDef, offsets: Vec<DigitOffset>) -> DigitsDef {
    DigitsDef { offsets, ..d }
}

/// A delta of `dy` alone.
fn down(dy: i32) -> DigitOffset {
    DigitOffset {
        dy,
        ..DigitOffset::default()
    }
}

/// A one-cell strip of ten glyphs.
fn single(offsets: Vec<DigitOffset>) -> DigitsDef {
    moved(
        digits(TEN, GlyphSet::Ten, 1, Align::Right, Pad::Blank),
        offsets,
    )
}

/// The frame and rect of each glyph drawn, the lanes' quads aside, in
/// draw order.
fn cells(h: &play_support::Harness) -> Vec<(u16, [f32; 4])> {
    drawn(h)
        .into_iter()
        .filter(|&(i, ..)| [TEN, ELEVEN, TWENTY_FOUR].contains(&i))
        .map(|(_, f, q)| (f, q.rect))
        .collect()
}

/// The frame, x and y of each glyph drawn.
fn corners(h: &play_support::Harness) -> Vec<(u16, f32, f32)> {
    cells(h).iter().map(|&(f, r)| (f, r[0], r[1])).collect()
}

/// One element over `d` showing `src` on `track`, its first frame.
fn one_on(d: DigitsDef, src: &str, track: Track, place: Place) -> play_support::Harness {
    let skin = load(
        vec![ElementDef {
            track,
            ..element(Source::Number(0), RECT, place)
        }],
        WidgetDefs {
            numbers: vec![number(d, src)],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    let mut h = harness(skin);
    h.frame(0, &[]);
    h
}

/// [`one_on`] at `r`.
fn one(d: DigitsDef, src: &str, r: [i32; 4], place: Place) -> play_support::Harness {
    one_on(d, src, lanes_support::rect(r), place)
}

/// A still track at `r` turned `angle` degrees.
fn turned(r: [i32; 4], angle: i32) -> Track {
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip(r) {
        *p = Some(v);
    }
    props[8] = Some(angle);
    lanes_support::still(props)
}

#[test]
fn a_digit_offset_moves_the_placed_cell() {
    let h = one(single(vec![MOVE]), "7", [100, 200, 20, 30], Place::SCREEN);
    let q = drawn(&h)[0].2;
    assert_eq!(q.rect, [103.0, 189.0, 24.0, 36.0]);
    // The pivot is the moved rect's own top left.
    assert_eq!(q.pivot, [103.0, 189.0]);
}

#[test]
fn the_native_size_fits_the_moved_cell() {
    // Exactly 20×30 at scale 1, centred in the moved [103, 189, 24, 36]:
    // fitted before the move it would stay [103, 189, 24, 36].
    let place = Place {
        pivot: Anchor::Center,
        native: Some(Native::Exact([20, 30])),
        ..Place::SCREEN
    };
    let track = turned([100, 200, 20, 30], 90);
    let h = one_on(single(vec![MOVE]), "7", track, place);
    let q = drawn(&h)[0].2;
    assert_eq!(q.rect, [105.0, 192.0, 20.0, 30.0]);
    assert_eq!(q.pivot, [115.0, 207.0]);
    assert_eq!(q.angle, std::f32::consts::FRAC_PI_2);
}

#[test]
fn offsets_are_target_pixels_at_any_resolution() {
    // The moved rect less the unmoved one, in target pixels, is the
    // offset at 1920×1080 (scale 1) and 3840×2160 (scale 2): in
    // `screen`, in a region at 150 %, bound to a lane at 120 % width, and
    // following SUDDEN+.
    let mut regions = play_support::REGIONS;
    regions[Region::Right.index()].anchor = Anchor::Right;
    let places = [
        Place::SCREEN,
        Place {
            region: Region::Right,
            ..Place::SCREEN
        },
        Place {
            region: Region::Playfield,
            bind: Bind::Lane(bms_model::Lane::new(3).unwrap()),
            ..Place::SCREEN
        },
        Place {
            region: Region::Playfield,
            follow: Follow::Cover,
            ..Place::SCREEN
        },
    ];
    let mut layout = Layout::AUTHOR;
    for r in [Region::Right, Region::Playfield] {
        let adjust = Adjust {
            scale: 150,
            ..Adjust::IDENTITY
        };
        layout.set(r, adjust).unwrap();
    }
    layout.set_lane_width(120).unwrap();
    for place in places {
        let rect = |offsets: Vec<DigitOffset>, target: [u32; 2]| {
            let skin = widget_support::load_fit(
                Fit::Adapt,
                regions,
                Some(lanes_support::simple()),
                vec![
                    lanes_support::lanes_element(),
                    element(Source::Number(0), [300, 200, 20, 30], place),
                ],
                WidgetDefs {
                    numbers: vec![number(single(offsets), "7")],
                    ..WidgetDefs::default()
                },
            )
            .unwrap();
            let mut h = harness(skin);
            h.target = target;
            h.view.cover = 300;
            h.layout = layout;
            h.frame(0, &[]);
            let scale = h.skin.canvas(target).scale();
            (cells(&h)[0].1, scale)
        };
        for target in [[1_920, 1_080], [3_840, 2_160]] {
            let (base, scale) = rect(Vec::new(), target);
            let (moved, _) = rect(vec![MOVE], target);
            let delta: Vec<f32> = moved
                .iter()
                .zip(base)
                .map(|(m, b)| (m - b) * scale)
                .collect();
            for (got, want) in delta.iter().zip([3.0, -11.0, 4.0, 6.0]) {
                assert!((got - want).abs() < 1e-3, "{place:?} {target:?}: {delta:?}");
            }
        }
    }
}

#[test]
fn offsets_follow_the_fixed_cells_under_every_alignment() {
    // 12 in four blank-padded cells is cells 2 and 3: rows 2 and 3 move
    // them wherever the alignment draws them; row 0 stays cell 0's.
    let far = DigitOffset {
        dx: 500,
        ..DigitOffset::default()
    };
    let offsets = vec![far, DigitOffset::default(), down(7), down(9)];
    for (align, x) in [
        (Align::Right, [144.0, 166.0]),
        (Align::Left, [100.0, 122.0]),
        (Align::Center, [122.0, 144.0]),
    ] {
        let d = moved(ten(align, Pad::Blank), offsets.clone());
        let h = one(d, "12", RECT, Place::SCREEN);
        assert_eq!(corners(&h), [(1, x[0], 57.0), (2, x[1], 59.0)], "{align:?}");
    }
    let d = moved(ten(Align::Right, Pad::Blank), offsets);
    let h = one(d, "1234", RECT, Place::SCREEN);
    assert_eq!(corners(&h)[0], (1, 600.0, 50.0));
}

#[test]
fn signs_and_padding_take_their_cells_rows() {
    let rows: Vec<DigitOffset> = (1..=4).map(down).collect();
    let h = one(
        moved(signed(Pad::Zero), rows.clone()),
        "-42",
        RECT,
        Place::SCREEN,
    );
    assert_eq!(
        corners(&h),
        [
            (23, 100.0, 51.0),
            (12, 122.0, 52.0),
            (16, 144.0, 53.0),
            (14, 166.0, 54.0)
        ]
    );
    // With blank padding the sign is cell 1, and takes row 1.
    let h = one(
        moved(signed(Pad::Blank), rows.clone()),
        "-42",
        RECT,
        Place::SCREEN,
    );
    assert_eq!(
        corners(&h),
        [(23, 122.0, 52.0), (16, 144.0, 53.0), (14, 166.0, 54.0)]
    );
    let glyph = digits(ELEVEN, GlyphSet::Eleven, 4, Align::Right, Pad::Glyph);
    let h = one(moved(glyph, rows), "42", RECT, Place::SCREEN);
    assert_eq!(
        corners(&h),
        [
            (10, 100.0, 51.0),
            (10, 122.0, 52.0),
            (4, 144.0, 53.0),
            (2, 166.0, 54.0)
        ]
    );
}

#[test]
fn overlapping_cells_keep_their_order() {
    // Cell 1 moved onto cell 0 still draws after it.
    let back = DigitOffset {
        dx: -22,
        ..DigitOffset::default()
    };
    let d = moved(
        ten(Align::Right, Pad::Zero),
        vec![DigitOffset::default(), back],
    );
    let h = one(d, "1234", RECT, Place::SCREEN);
    let xs: Vec<(u16, f32)> = corners(&h).iter().map(|&(f, x, _)| (f, x)).collect();
    assert_eq!(xs, [(1, 100.0), (2, 100.0), (3, 144.0), (4, 166.0)]);
}

#[test]
fn zero_offsets_draw_as_none() {
    for native in [None, Some(Native::NoLarger([3, 7]))] {
        let quads = |offsets: Vec<DigitOffset>| {
            let place = Place {
                pivot: Anchor::BottomRight,
                native,
                ..Place::SCREEN
            };
            let d = moved(ten(Align::Center, Pad::Blank), offsets);
            let skin = load(
                vec![ElementDef {
                    track: turned(RECT, 33),
                    ..element(Source::Number(0), RECT, place)
                }],
                WidgetDefs {
                    numbers: vec![number(d, "42")],
                    ..WidgetDefs::default()
                },
            )
            .unwrap();
            let mut h = harness(skin);
            h.target = [2_560, 1_440];
            h.frame(0, &[]);
            h.batch.quads().to_vec()
        };
        assert_eq!(quads(vec![DigitOffset::default(); 4]), quads(Vec::new()));
    }
}

#[test]
fn offsets_are_checked_at_their_limits() {
    let base = ten(Align::Right, Pad::Blank);
    assert_eq!(refused(moved(base.clone(), vec![MOVE; 4]), "1"), None);
    assert_eq!(
        refused(moved(base.clone(), vec![MOVE; 5]), "1"),
        Some(NumberError::OffsetCount { n: 5 })
    );
    let zero = DigitOffset::default();
    let edge = |v: i32| {
        [
            DigitOffset { dx: v, ..zero },
            DigitOffset { dy: v, ..zero },
            DigitOffset { dw: v, ..zero },
            DigitOffset { dh: v, ..zero },
        ]
    };
    for v in [MAX_COORD, -MAX_COORD] {
        assert_eq!(refused(moved(base.clone(), edge(v).to_vec()), "1"), None);
    }
    for v in [MAX_COORD + 1, -MAX_COORD - 1] {
        for (cell, o) in edge(v).into_iter().enumerate() {
            let mut rows = vec![zero; 4];
            rows[cell] = o;
            assert_eq!(
                refused(moved(base.clone(), rows), "1"),
                Some(NumberError::OffsetRange { cell })
            );
        }
    }
}

/// F13b: 5,957 eleven-entry strips and one nine-entry strip, exactly
/// [`MAX_DIGIT_OFFSETS`]; one number referenced, the rest counted
/// unreferenced, and one entry more refused before the loaded tables are
/// built.
#[test]
fn digit_offsets_are_counted_across_every_strip() {
    let strip = |n: usize| {
        moved(
            digits(TEN, GlyphSet::Ten, 11, Align::Right, Pad::Blank),
            vec![MOVE; n],
        )
    };
    let skin = |last: usize| {
        let mut numbers: Vec<_> = (0..5_957).map(|_| number(strip(11), "1")).collect();
        numbers.push(number(strip(last), "1"));
        load(
            vec![element(Source::Number(0), RECT, Place::SCREEN)],
            WidgetDefs {
                numbers,
                ..WidgetDefs::default()
            },
        )
    };
    assert_eq!(5_957 * 11 + 9, MAX_DIGIT_OFFSETS);
    assert_eq!(skin(9).unwrap().digit_offset_count(), MAX_DIGIT_OFFSETS);
    assert!(matches!(
        skin(10),
        Err(LoadError::TooManyDigitOffsets { n }) if n == MAX_DIGIT_OFFSETS + 1
    ));
    // A judge's combos count once per stored copy.
    let mut judge = widget_support::empty_judge();
    judge.grades[0] = Some(widget_support::grade(widget_support::GRADE, [0, 0, 10, 10]));
    for s in [JudgeSlot::Perfect, JudgeSlot::Great] {
        judge.combos[s.index()] = Some(widget_support::combo(strip(3), [0, 0, 10, 10]));
    }
    let skin = load(
        vec![widget_support::judge_element(0)],
        WidgetDefs {
            judges: vec![judge],
            ..WidgetDefs::default()
        },
    )
    .unwrap();
    assert_eq!(skin.digit_offset_count(), 6);
}
