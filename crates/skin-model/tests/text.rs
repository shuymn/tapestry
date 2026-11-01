//! The `text` widget: its load checks, the lines a skin asks to
//! rasterize, and the quad a build lays out from an attached line.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::{Lane, TimeUs};
use proptest::prelude::*;
use skin_model::{
    Adjust, Align, Anchor, Bind, ChartText, Ease, ElementDef, End, Fill, Fit, Follow, FontDef,
    FontRef, FrameUv, KeyDef, Layout, LoadError, MAX_TEXT_BYTES, MAX_TEXT_ELEMENTS, MAX_TEXT_EM,
    Overflow, Place, Quad, Region, RegionDef, Sheet, Skin, SkinDef, Source, TextDef, TextError,
    TextField, TextImage, TextRequest, TextTable, TextTableError, TextValue, Track, Variant,
    WidgetDefs,
};

use play_support::{Harness, MS, REGIONS, full};
use widget_support::{boolean, chart, element};

/// Where every test line lies in the atlas.
const UV: FrameUv = FrameUv {
    uv: [0.125, 0.25, 0.375, 0.5],
    layer: 3,
};

/// The geometry tests' line: advance 2 em, ink 2 em wide from the pen,
/// 1.25 em tall from 0.125 em below the box top.
const INK: [f32; 4] = [0.0, 0.125, 2.0, 1.25];

/// The geometry tests' box: 300 wide, an em of 40.
const BOX: [i32; 4] = [100, 50, 300, 40];

fn line(ink: [f32; 4], advance: f32) -> TextImage {
    TextImage::new(UV, ink, advance).unwrap()
}

fn def(value: TextValue, align: Align, overflow: Overflow) -> TextDef {
    TextDef {
        value,
        font: FontRef::Default,
        align,
        overflow,
    }
}

fn literal(s: &str) -> TextValue {
    TextValue::Literal(s.to_owned())
}

fn field(f: TextField) -> TextValue {
    TextValue::Field(f)
}

/// A left-aligned spilling literal "X".
fn plain() -> TextDef {
    def(literal("X"), Align::Left, Overflow::Spill)
}

/// An element on `ready` drawing text `i` over `r` in `screen`.
fn text(i: u32, r: [i32; 4]) -> ElementDef {
    element(Source::Text(i), r, Place::SCREEN)
}

/// A track through `rects`, 100 ms apart, linear, holding.
fn keys(rects: &[[i32; 4]]) -> Track {
    let keys: Vec<_> = (0_i64..)
        .zip(rects)
        .map(|(i, &[x, y, w, h])| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
            KeyDef {
                at: TimeUs(i * 100 * MS),
                props,
            }
        })
        .collect();
    Track::new(&keys, Ease::Linear, End::Hold).unwrap()
}

fn skin_def(
    fit: Fit,
    regions: [RegionDef; Region::COUNT],
    sheet: Sheet,
    elements: Vec<ElementDef>,
    texts: Vec<TextDef>,
) -> SkinDef {
    SkinDef {
        fit,
        regions,
        whens: Vec::new(),
        sheet,
        lanes: None,
        elements,
        widgets: WidgetDefs {
            texts,
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    }
}

/// An adapted skin without images of `elements` over `texts`.
fn load(elements: Vec<ElementDef>, texts: Vec<TextDef>) -> Result<Skin, LoadError> {
    Skin::new(skin_def(
        Fit::Adapt,
        REGIONS,
        Sheet::empty(),
        elements,
        texts,
    ))
}

/// The text error of element `element`, or a panic.
fn text_error(r: Result<Skin, LoadError>) -> (usize, TextError) {
    match r {
        Err(LoadError::Text { element, error }) => (element, error),
        other => panic!("not a text error: {other:?}"),
    }
}

/// A harness over `skin` with `lines` attached, after a frame at 100 ms.
fn drawn(skin: Skin, lines: Vec<Option<TextImage>>) -> Harness {
    let mut h = widget_support::harness(skin);
    h.skin
        .attach_text(&mut h.play, TextTable::new(lines))
        .unwrap();
    h.frame(100, &[]);
    h
}

/// The quad of one text element drawing `d` with `image` over `r`.
fn quad(d: TextDef, r: [i32; 4], image: TextImage) -> Option<Quad> {
    let h = drawn(load(vec![text(0, r)], vec![d]).unwrap(), vec![Some(image)]);
    assert!(h.batch.quads().len() <= 1);
    h.batch.quads().first().copied()
}

fn rect(d: TextDef, r: [i32; 4], image: TextImage) -> [f32; 4] {
    quad(d, r, image).expect("a quad").rect
}

const NO_CHART: ChartText<'static> = ChartText {
    title: "",
    subtitle: "",
    artist: "",
    subartist: "",
    genre: "",
};

fn requests(skin: &Skin, chart: &ChartText<'_>) -> Vec<TextRequest> {
    skin.text_requests(chart, [1_920, 1_080], &Layout::AUTHOR)
}

// Load checks.

#[test]
fn a_skin_holds_at_most_64_text_elements() {
    let elements = |n| (0..n).map(|_| text(0, BOX)).collect::<Vec<_>>();
    let skin = load(elements(MAX_TEXT_ELEMENTS), vec![plain()]).unwrap();
    assert_eq!(skin.text_slots(), 64);
    let err = load(elements(MAX_TEXT_ELEMENTS + 1), vec![plain()]).unwrap_err();
    assert!(matches!(err, LoadError::TooManyTexts { n: 65 }), "{err:?}");
}

#[test]
fn literals_are_short_and_have_no_controls() {
    let at_cap = format!("{}a", "あ".repeat(85));
    assert_eq!(at_cap.len(), MAX_TEXT_BYTES);
    let one = |s: &str| {
        load(
            vec![text(0, BOX)],
            vec![def(literal(s), Align::Left, Overflow::Spill)],
        )
    };
    one(&at_cap).unwrap();
    let over = format!("{at_cap}b");
    for bad in [over.as_str(), "", "A\nB", "A\u{7f}", "A\u{85}B"] {
        assert_eq!(text_error(one(bad)), (0, TextError::Literal), "{bad:?}");
    }
}

#[test]
fn text_is_unbound_unfilled_and_positive_in_height() {
    let placed = |place: Place, r: [i32; 4]| element(Source::Text(0), r, place);
    let one = |e: ElementDef| load(vec![e], vec![plain()]);
    let playfield = Place {
        region: Region::Playfield,
        bind: Bind::Lane(Lane::SCRATCH),
        ..Place::SCREEN
    };
    assert_eq!(
        text_error(one(placed(playfield, BOX))),
        (0, TextError::Bound)
    );
    let cover = Place {
        fill: Fill::Cover,
        ..Place::SCREEN
    };
    assert_eq!(text_error(one(placed(cover, BOX))), (0, TextError::Fill));
    // Every key: h > 0 and w ≥ 0, at the bounds.
    let sized = |rects: &[[i32; 4]]| ElementDef {
        track: keys(rects),
        ..text(0, BOX)
    };
    one(sized(&[[0, 0, 0, 1]])).unwrap();
    for bad in [
        &[[0, 0, 10, 0]][..],
        &[[0, 0, -1, 1]],
        &[[0, 0, 10, 20], [0, 0, 10, 0]],
        &[[0, 0, 10, 20], [0, 0, -1, 20]],
    ] {
        assert_eq!(text_error(one(sized(bad))), (0, TextError::Size), "{bad:?}");
    }
    // The lower element is reported, though its check comes later.
    let err = load(
        vec![sized(&[[0, 0, -1, 1]]), placed(playfield, BOX)],
        vec![plain()],
    );
    assert_eq!(text_error(err), (0, TextError::Size));
}

fn font_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.ttf"), [0, 1, 0, 0, 0]).unwrap();
    d
}

#[test]
fn a_text_font_exists_in_the_sheet() {
    let d = font_dir();
    let sheet = |fonts: usize| {
        let s = Sheet::new(d.path(), Vec::new(), Vec::new()).unwrap();
        let fonts = (0..fonts)
            .map(|_| FontDef {
                path: "a.ttf".into(),
            })
            .collect();
        s.with_fonts(fonts).unwrap()
    };
    let one = |font: u32, fonts: usize| {
        let d = TextDef {
            font: FontRef::File(font),
            ..plain()
        };
        Skin::new(skin_def(
            Fit::Adapt,
            REGIONS,
            sheet(fonts),
            vec![text(0, BOX)],
            vec![d],
        ))
    };
    assert_eq!(text_error(one(0, 0)), (0, TextError::UnknownFont));
    // The request carries the element's font.
    let fonts: Vec<_> = requests(&one(0, 1).unwrap(), &NO_CHART)
        .into_iter()
        .map(|r| r.font)
        .collect();
    assert_eq!(fonts, [FontRef::File(0)]);
    assert_eq!(text_error(one(1, 1)), (0, TextError::UnknownFont));
}

#[test]
fn an_element_without_its_text_is_unknown() {
    let err = load(vec![text(1, BOX)], vec![plain()]).unwrap_err();
    assert!(
        matches!(err, LoadError::UnknownWidget { element: 0 }),
        "{err:?}"
    );
}

// Requests.

#[test]
fn requests_join_the_full_fields_and_skip_empty_strings() {
    let texts = vec![
        def(field(TextField::FullTitle), Align::Left, Overflow::Spill),
        def(field(TextField::Genre), Align::Left, Overflow::Spill),
        def(literal("MAX"), Align::Left, Overflow::Spill),
        def(field(TextField::FullArtist), Align::Left, Overflow::Spill),
        def(field(TextField::Title), Align::Left, Overflow::Spill),
        def(field(TextField::Subtitle), Align::Left, Overflow::Spill),
        def(field(TextField::Artist), Align::Left, Overflow::Spill),
        def(field(TextField::Subartist), Align::Left, Overflow::Spill),
    ];
    // An image between them takes no slot.
    let mut elements: Vec<_> = (0..8).map(|i| text(i, BOX)).collect();
    elements.insert(2, element(Source::Image(0), BOX, Place::SCREEN));
    let skin = Skin::new(skin_def(
        Fit::Adapt,
        REGIONS,
        widget_support::sheet(),
        elements,
        texts,
    ))
    .unwrap();
    assert_eq!(skin.text_slots(), 8);
    let got = |chart| {
        requests(&skin, &chart)
            .into_iter()
            .map(|r| (r.slot, r.text))
            .collect::<Vec<_>>()
    };
    let chart = ChartText {
        title: "T",
        subtitle: "S",
        artist: "A",
        ..NO_CHART
    };
    let s = |slot: u16, t: &str| (slot, t.to_owned());
    assert_eq!(
        got(chart),
        [
            s(0, "T S"),
            s(2, "MAX"),
            s(3, "A"),
            s(4, "T"),
            s(5, "S"),
            s(6, "A")
        ]
    );
    let chart = ChartText {
        subartist: "B",
        genre: "G",
        ..chart
    };
    assert_eq!(
        got(chart),
        [
            s(0, "T S"),
            s(1, "G"),
            s(2, "MAX"),
            s(3, "A B"),
            s(4, "T"),
            s(5, "S"),
            s(6, "A"),
            s(7, "B")
        ]
    );
    // An empty title with a subtitle keeps the joining space.
    let chart = ChartText { title: "", ..chart };
    assert_eq!(got(chart)[0], s(0, " S"));
}

#[test]
fn chart_strings_lose_controls_and_are_cut_at_a_character() {
    let skin = load(
        vec![text(0, BOX)],
        vec![def(field(TextField::Title), Align::Left, Overflow::Spill)],
    )
    .unwrap();
    let title = |title: &str| {
        let r = requests(&skin, &ChartText { title, ..NO_CHART });
        r.into_iter().map(|r| r.text).collect::<Vec<_>>()
    };
    assert_eq!(title("A\tB\u{7}\u{7f}\u{85}"), ["AB"]);
    let a255 = "a".repeat(255);
    assert_eq!(title(&format!("{a255}あ")), [a255.as_str()]);
    let a256 = "a".repeat(256);
    assert_eq!(title(&a256), [a256.as_str()]);
    assert_eq!(title(&format!("{a256}b")), [a256]);
    // Controls are dropped before the cut.
    assert_eq!(title(&format!("\n{a255}b")), [format!("{a255}b")]);
    // Nothing left: no request.
    assert!(title("\t\r\n").is_empty());
}

/// The em of one text element in `region` with `rects` under `fit`, at
/// `target` with `layout`.
fn em(fit: Fit, region: Region, rects: &[[i32; 4]], target: [u32; 2], layout: &Layout) -> f32 {
    let e = ElementDef {
        track: keys(rects),
        place: Place {
            region,
            ..Place::SCREEN
        },
        ..text(0, BOX)
    };
    let skin = Skin::new(skin_def(
        fit,
        REGIONS,
        Sheet::empty(),
        vec![e],
        vec![plain()],
    ))
    .unwrap();
    let r = skin.text_requests(&NO_CHART, target, layout);
    assert_eq!(r.len(), 1);
    r[0].em_px
}

#[test]
fn the_em_follows_the_target_the_region_and_the_tallest_key() {
    let author = Layout::AUTHOR;
    let h23 = [[0, 0, 100, 23]];
    let screen =
        |rects: &[[i32; 4]], target| em(Fit::Adapt, Region::Screen, rects, target, &author);
    assert_eq!(screen(&h23, [1_920, 1_080]), 23.0);
    assert_eq!(screen(&h23, [3_840, 2_160]), 46.0);
    let mut top = Layout::AUTHOR;
    top.set(
        Region::Top,
        Adjust {
            scale: 150,
            ..Adjust::IDENTITY
        },
    )
    .unwrap();
    assert_eq!(
        em(Fit::Adapt, Region::Top, &h23, [1_920, 1_080], &top),
        34.5
    );
    assert_eq!(
        screen(&[[0, 0, 10, 20], [0, 0, 10, 40]], [1_920, 1_080]),
        40.0
    );
    assert_eq!(
        screen(&[[0, 0, 10, 40], [0, 0, 10, 20]], [1_920, 1_080]),
        40.0
    );
    assert_eq!(screen(&[[0, 0, 10, 128]], [3_840, 2_160]), 256.0);
    assert_eq!(screen(&[[0, 0, 10, 127]], [3_840, 2_160]), 254.0);
    assert_eq!(screen(&[[0, 0, 10, 200]], [3_840, 2_160]), 256.0);
    let fixed = |target| em(Fit::Fixed, Region::Screen, &h23, target, &author);
    assert_eq!(fixed([2_560, 1_080]), 23.0);
    assert_eq!(fixed([3_840, 2_160]), 46.0);
    // A narrow target scales by its width.
    assert_eq!(screen(&h23, [960, 1_080]), 11.5);
}

#[test]
fn hidden_regions_ask_and_folded_visibilities_do_not() {
    let mut regions = REGIONS;
    regions[Region::Top.index()].optional = true;
    let in_top = Place {
        region: Region::Top,
        ..Place::SCREEN
    };
    let elements = vec![
        element(Source::Text(0), BOX, in_top),
        ElementDef {
            visible: Some(boolean("false && combo >= 1")),
            ..text(0, BOX)
        },
        ElementDef {
            visible: Some(boolean("combo >= 1")),
            ..text(0, BOX)
        },
    ];
    let skin = Skin::new(skin_def(
        Fit::Adapt,
        regions,
        Sheet::empty(),
        elements,
        vec![plain()],
    ))
    .unwrap();
    let mut hidden = Layout::AUTHOR;
    hidden
        .set(
            Region::Top,
            Adjust {
                visible: false,
                ..Adjust::IDENTITY
            },
        )
        .unwrap();
    assert!(skin.controls(Region::Top).hide);
    let slots: Vec<_> = skin
        .text_requests(&NO_CHART, [1_920, 1_080], &hidden)
        .into_iter()
        .map(|r| r.slot)
        .collect();
    assert_eq!(slots, [0, 2]);
}

// Build.

#[test]
fn lines_sit_by_their_alignment_and_shrink_only_when_wider() {
    let image = line(INK, 2.0);
    let at = |align, overflow, r: [i32; 4]| rect(def(literal("X"), align, overflow), r, image);
    use Align::{Center, Left, Right};
    use Overflow::{Shrink, Spill};
    assert_eq!(at(Left, Spill, BOX), [100.0, 55.0, 80.0, 50.0]);
    assert_eq!(at(Center, Spill, BOX)[0], 210.0);
    assert_eq!(at(Right, Spill, BOX)[0], 320.0);
    // Shrink never enlarges: 80 of 300.
    assert_eq!(at(Left, Shrink, BOX), [100.0, 55.0, 80.0, 50.0]);
    assert_eq!(at(Center, Shrink, BOX), [210.0, 55.0, 80.0, 50.0]);
    assert_eq!(at(Right, Shrink, BOX), [320.0, 55.0, 80.0, 50.0]);
    let w60 = [100, 50, 60, 40];
    for align in [Left, Center, Right] {
        assert_eq!(
            at(align, Shrink, w60),
            [100.0, 55.0, 60.0, 50.0],
            "{align:?}"
        );
    }
    assert_eq!(at(Center, Spill, w60), [90.0, 55.0, 80.0, 50.0]);
    assert_eq!(at(Right, Spill, w60), [80.0, 55.0, 80.0, 50.0]);
}

#[test]
fn ink_left_of_the_pen_widens_the_line() {
    // Ink from −0.0625 em: the line spans [−0.0625, 2] em, so its left
    // edge, not the pen, sits on the box's.
    let image = line([-0.0625, 0.125, 2.0, 1.25], 2.0);
    assert_eq!(image.width(), 2.0625);
    let spill = |align| rect(def(literal("X"), align, Overflow::Spill), BOX, image);
    assert_eq!(spill(Align::Left), [100.0, 55.0, 80.0, 50.0]);
    assert_eq!(spill(Align::Right)[0], 400.0 - 82.5);
    // Overhanging ink on both sides: advance 2, ink [−0.25, 2.25] em, a
    // line 2.5 em wide. Squeezed into 50 at an em of 40 (100 → 50), the
    // ink fills the box exactly; x would be 95 measured from the pen.
    let image = line([-0.25, 0.125, 2.5, 1.25], 2.0);
    assert_eq!(image.width(), 2.5);
    let shrink = |align| {
        rect(
            def(literal("X"), align, Overflow::Shrink),
            [100, 50, 50, 40],
            image,
        )
    };
    for align in [Align::Left, Align::Center, Align::Right] {
        assert_eq!(shrink(align), [100.0, 55.0, 50.0, 50.0], "{align:?}");
    }
    // Ink right of the advance: the line ends at the ink.
    let image = line([0.5, 0.0, 3.0, 1.0], 2.0);
    assert_eq!(image.width(), 3.5);
    let right = rect(def(literal("X"), Align::Right, Overflow::Spill), BOX, image);
    assert_eq!(right, [400.0 - 140.0 + 20.0, 50.0, 120.0, 40.0]);
}

#[test]
fn a_line_turns_about_the_box() {
    // Turned 90° about the pivot.
    let mut props = [None; 9];
    for (p, v) in props.iter_mut().zip([100, 50, 300, 40]) {
        *p = Some(v);
    }
    props[8] = Some(90);
    let e = |pivot| {
        let place = Place {
            pivot,
            ..Place::SCREEN
        };
        ElementDef {
            track: lanes_support::still(props),
            ..element(Source::Text(0), BOX, place)
        }
    };
    let skin = load(
        vec![e(Anchor::Center), e(Anchor::BottomRight)],
        vec![plain()],
    )
    .unwrap();
    let image = Some(line(INK, 2.0));
    let h = drawn(skin, vec![image, image]);
    let pivots: Vec<_> = h.batch.quads().iter().map(|q| q.pivot).collect();
    assert_eq!(pivots, [[250.0, 70.0], [400.0, 90.0]]);
    let quarter = 90_f64.to_radians() as f32;
    for q in h.batch.quads() {
        assert_eq!(q.angle, quarter);
        assert_eq!(q.rect, [100.0, 55.0, 80.0, 50.0]);
    }
}

#[test]
fn a_following_line_moves_without_changing_its_request() {
    // The simple lanes are 800 long; a cover of 500 ‰ travels C = 400. A
    // line following the cover moves 400 down with its pivot; its
    // request, from its keys, is that of a line following nothing.
    let skin = |follow| {
        let lanes = ElementDef {
            timer: skin_model::TimerRef::Builtin(skin_model::timer::FULL_COMBO),
            ..lanes_support::lanes_element()
        };
        let place = Place {
            follow,
            pivot: Anchor::Center,
            ..Place::SCREEN
        };
        let mut def = skin_def(
            Fit::Adapt,
            REGIONS,
            lanes_support::sheet(),
            vec![lanes, element(Source::Text(0), BOX, place)],
            vec![plain()],
        );
        def.lanes = Some(lanes_support::simple());
        def.elements = lanes_support::with_markers(def.lanes.as_ref(), def.elements);
        Skin::new(def).unwrap()
    };
    let chart = ChartText {
        title: "T",
        ..NO_CHART
    };
    assert_eq!(
        requests(&skin(Follow::Cover), &chart),
        requests(&skin(Follow::None), &chart)
    );
    let at = |follow, cover| {
        let mut h = widget_support::harness(skin(follow));
        h.skin
            .attach_text(&mut h.play, TextTable::new(vec![Some(line(INK, 2.0))]))
            .unwrap();
        h.view.cover = cover;
        h.frame(100, &[]);
        let q = h.batch.quads()[0];
        (q.rect, q.pivot)
    };
    let (rect, pivot) = at(Follow::None, 500);
    assert_eq!(at(Follow::Cover, 0), (rect, pivot));
    let [x, y, w, hh] = rect;
    assert_eq!(
        at(Follow::Cover, 500),
        ([x, y + 400.0, w, hh], [pivot[0], pivot[1] + 400.0])
    );
}

#[test]
fn a_line_takes_the_tracks_color_and_its_own_uv() {
    let mut props = [None; 9];
    props[..8].copy_from_slice(&[
        Some(100),
        Some(50),
        Some(300),
        Some(40),
        Some(64),
        Some(128),
        Some(255),
        Some(0),
    ]);
    let colored = lanes_support::still(props);
    let skin = Skin::new(skin_def(
        Fit::Adapt,
        REGIONS,
        widget_support::sheet(),
        vec![
            ElementDef {
                track: colored.clone(),
                ..element(Source::Image(0), BOX, Place::SCREEN)
            },
            ElementDef {
                track: colored,
                ..text(0, BOX)
            },
        ],
        vec![plain()],
    ))
    .unwrap();
    let h = drawn(skin, vec![Some(line(INK, 2.0))]);
    let [image, text] = h.batch.quads() else {
        panic!("{:?}", h.batch.quads());
    };
    assert_eq!(text.color, image.color);
    assert!(
        text.color[3] < 0.5 && text.color[3] > 0.0,
        "{:?}",
        text.color
    );
    assert_eq!((text.uv, text.layer), (UV.uv, UV.layer));
    assert_eq!(text.filter, skin_model::Filter::Linear);
}

#[test]
fn a_zero_wide_box_shrinks_to_nothing_and_spills_around_its_x() {
    let image = line(INK, 2.0);
    let zero = [100, 50, 0, 40];
    assert_eq!(
        quad(
            def(literal("X"), Align::Center, Overflow::Shrink),
            zero,
            image
        ),
        None
    );
    let spill = rect(
        def(literal("X"), Align::Center, Overflow::Spill),
        zero,
        image,
    );
    assert_eq!(spill, [60.0, 55.0, 80.0, 50.0]);
}

#[test]
fn each_attempt_draws_text_only_once_its_table_is_attached() {
    let skin = load(vec![text(0, BOX), text(0, BOX)], vec![plain()]).unwrap();
    let mut h = drawn(skin, vec![Some(line(INK, 2.0)); 2]);
    assert_eq!(h.batch.quads().len(), 2);
    // A new attempt starts without a table.
    h.play = h.skin.prepare(&chart(), &full(0)).unwrap();
    h.frame(100, &[]);
    assert!(h.batch.quads().is_empty());
    let err = h
        .skin
        .attach_text(&mut h.play, TextTable::new(vec![Some(line(INK, 2.0))]))
        .unwrap_err();
    assert_eq!(
        err,
        TextTableError::Len {
            expected: 2,
            got: 1
        }
    );
    h.frame(200, &[]);
    assert!(h.batch.quads().is_empty());
    h.skin
        .attach_text(
            &mut h.play,
            TextTable::new(vec![None, Some(line(INK, 2.0))]),
        )
        .unwrap();
    h.frame(300, &[]);
    // A slot without a line draws nothing; the others draw.
    assert_eq!(h.batch.quads().len(), 1);
}

#[test]
fn a_table_of_another_length_draws_no_text() {
    // Two skins over one sheet, one text element apart: a play prepared
    // and attached by the first is built by the second.
    let sheet = Sheet::empty();
    let skin = |n: usize| {
        let elements = (0..n).map(|_| text(0, BOX)).collect();
        Skin::new(skin_def(
            Fit::Adapt,
            REGIONS,
            sheet.clone(),
            elements,
            vec![plain()],
        ))
        .unwrap()
    };
    let mut h = drawn(skin(2), vec![Some(line(INK, 2.0)); 2]);
    assert_eq!(h.batch.quads().len(), 2);
    h.skin = skin(3);
    h.frame(200, &[]);
    assert!(h.batch.quads().is_empty());
}

#[test]
fn a_text_element_costs_one_quad() {
    let skin = load((0..3).map(|_| text(0, BOX)).collect(), vec![plain()]).unwrap();
    assert_eq!(skin.quad_capacity(), 3);
    assert_eq!(skin.run_capacity(), 3);
    let h = drawn(skin, vec![Some(line(INK, 2.0)); 3]);
    assert_eq!(h.batch.quads().len(), 3);
    assert_eq!(h.batch.dropped(), 0);
}

#[test]
fn text_has_its_own_pixel_size_for_the_variant() {
    let skin = |elements| {
        Skin::new(skin_def(
            Fit::Adapt,
            REGIONS,
            widget_support::sheet(),
            elements,
            vec![plain()],
        ))
        .unwrap()
    };
    let only = skin(vec![text(0, BOX)]);
    assert_eq!(only.variant([3_840, 2_160], &Layout::AUTHOR), Variant::Lo);
    let with_image = skin(vec![
        text(0, BOX),
        element(Source::Image(0), BOX, Place::SCREEN),
    ]);
    assert_eq!(
        with_image.variant([3_840, 2_160], &Layout::AUTHOR),
        Variant::Hi
    );
}

#[test]
fn a_text_box_counts_for_its_region() {
    let left = Place {
        region: Region::Left,
        ..Place::SCREEN
    };
    let skin = load(vec![element(Source::Text(0), BOX, left)], vec![plain()]).unwrap();
    assert!(skin.controls(Region::Left).position);
    let mut pushed = Layout::AUTHOR;
    pushed
        .set(
            Region::Left,
            Adjust {
                x: -4_320,
                ..Adjust::IDENTITY
            },
        )
        .unwrap();
    let p = skin.placement(
        &skin.canvas([1_920, 1_080]),
        &pushed,
        skin_model::Side::Left,
    );
    let [x, _, w, _] = p.region(Region::Left).bounds.unwrap();
    assert_eq!(x + w, skin_model::MIN_VISIBLE as f32);
}

#[test]
fn text_images_refuse_bad_metrics() {
    assert!(TextImage::new(UV, INK, 2.0).is_some());
    assert!(TextImage::new(UV, INK, 0.0).is_some());
    let max = MAX_TEXT_EM;
    assert!(TextImage::new(UV, [-max / 2.0, 0.0, max / 2.0, 1.0], max / 2.0).is_some());
    let above = 4_097.0;
    for (ink, advance) in [
        ([f32::NAN, 0.0, 1.0, 1.0], 1.0),
        ([0.0, f32::INFINITY, 1.0, 1.0], 1.0),
        ([0.0, 0.0, 1.0, 1.0], f32::NEG_INFINITY),
        ([0.0, 0.0, 0.0, 1.0], 1.0),
        ([0.0, 0.0, -1.0, 1.0], 1.0),
        ([0.0, 0.0, 1.0, 0.0], 1.0),
        ([0.0, 0.0, 1.0, -1.0], 1.0),
        ([0.0, 0.0, 1.0, 1.0], -0.5),
        ([0.0, 0.0, 1.0, 1.0], above),
        ([0.0, -above, 1.0, 1.0], 1.0),
        ([0.0, 0.0, 1.0, above], 1.0),
        // Each value fits, the derived extent does not.
        ([-max, 0.0, 1.0, 1.0], max),
        ([max, 0.0, 1.0, 1.0], 1.0),
    ] {
        assert!(
            TextImage::new(UV, ink, advance).is_none(),
            "{ink:?} {advance}"
        );
    }
    let bad_uv = FrameUv {
        uv: [f32::NAN, 0.0, 1.0, 1.0],
        layer: 0,
    };
    assert!(TextImage::new(bad_uv, INK, 2.0).is_none());
}

// Properties.

fn any_align() -> impl Strategy<Value = Align> {
    prop_oneof![Just(Align::Left), Just(Align::Center), Just(Align::Right)]
}

fn any_overflow() -> impl Strategy<Value = Overflow> {
    prop_oneof![Just(Overflow::Spill), Just(Overflow::Shrink)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Every valid line: a squeezed line stays inside its box, a spilled
    /// one keeps its width, and the line's left edge, centre or right
    /// edge sits on the box's.
    #[test]
    fn lines_stay_on_their_box(
        ink_x in -8.0_f32..8.0,
        ink_y in -2.0_f32..2.0,
        ink_w in 0.001_f32..16.0,
        ink_h in 0.001_f32..4.0,
        advance in 0.0_f32..16.0,
        x in -2_000_i32..2_000,
        y in -2_000_i32..2_000,
        w in 0_i32..=61_440,
        h in 1_i32..=2_048,
        align in any_align(),
        overflow in any_overflow(),
    ) {
        let image = line([ink_x, ink_y, ink_w, ink_h], advance);
        let r = [x, y, w, h];
        let q = quad(def(literal("X"), align, overflow), r, image);
        let (x, w, h) = (x as f32, w as f32, h as f32);
        let tw = image.width() * h;
        let squeezed = overflow == Overflow::Shrink && tw > w;
        if squeezed && w == 0.0 {
            prop_assert!(q.is_none());
            return Ok(());
        }
        let q = q.unwrap().rect;
        let sx = q[2] / (ink_w * h);
        let d = tw * sx;
        let tol = |v: f32| v.abs() * 8.0 * f32::EPSILON + 1e-3;
        if overflow == Overflow::Shrink {
            prop_assert!(d <= w * (1.0 + 4.0 * f32::EPSILON) + 1e-3, "{d} > {w}");
            prop_assert!(q[0] >= x - tol(x) - tol(w), "{q:?}");
            prop_assert!(q[0] + q[2] <= x + w + tol(x) + tol(w), "{q:?}");
        } else {
            prop_assert!((d - tw).abs() <= tol(tw), "{d} ≠ {tw}");
        }
        if !squeezed {
            prop_assert_eq!(sx, 1.0);
        }
        // The line's left edge, from the ink's place on it.
        let left = ink_x.min(0.0);
        let start = q[0] - (ink_x - left) * h * sx;
        let (at, want) = match align {
            Align::Left => (start, x),
            Align::Center => (start + d / 2.0, x + w / 2.0),
            Align::Right => (start + d, x + w),
        };
        let slack = tol(x) + tol(w) + tol(d) + tol(q[0]);
        prop_assert!((at - want).abs() <= slack, "{align:?}: {at} ≠ {want}");
        prop_assert_eq!(q[1], y as f32 + ink_y * h);
        prop_assert_eq!(q[3], ink_h * h);
    }
}
