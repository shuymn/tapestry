//! The loading runtime: the play screen's state from a chart's choice to
//! its countdown, built without a chart (stage3.md R3, "The loading
//! runtime").
//!
//! Unless stated: the loading lifetime's origin at 0, its progress 0, a
//! 1 920 × 1 080 target, the author's layout, no covers.

mod lanes_support;
mod play_support;
mod widget_support;

use bms_model::TimeUs;
use skin_model::{
    Align, BarGroupDef, BuildError, CoverSettings, ElementDef, Fit, GaugeLook, GlyphSet,
    ImageChoiceDef, ImageChoicePart, ImageSpan, JudgeSlot, LaneParts, LanesDef, Layout, LoadState,
    Loading, LoadingFrame, LoadingGauge, Motion, NumberDef, PLAY, Pad, Place, Program, Quad,
    QuadBatch, Region, Scope, Skin, SkinDef, Source, TextDef, TextField, TextImage, TextSlots,
    TextTable, TextValue, TimerRef, Ty, UvTable, WidgetDefs, compile, field, timer,
};

use lanes_support::{bar_group, simple};
use play_support::{Harness, MS, VIEW, readouts_for, uvs};
use widget_support::{
    GAUGE, GRADE, IMAGES, TEN, desc, digits, empty_judge, frames_of, gauge, grade, int, sheet,
};

/// Image 0 is the background; the grade images past the first one stand
/// for the lanes' notes, judgment line and bars and for plain elements.
const BACKGROUND: u32 = 0;
const NOTES: u32 = GRADE + 1;
const LINE: u32 = GRADE + 2;
const BARS: u32 = GRADE + 3;
const MARK: u32 = GRADE + 4;
const OTHER: u32 = GRADE + 5;
/// The gauge's first look set: front, back and tip, each at or above then
/// below the border.
const FRONT: [u32; 2] = [GAUGE, GAUGE + 1];
const BACK: [u32; 2] = [GAUGE + 2, GAUGE + 3];
const TIP: [u32; 2] = [GAUGE + 4, GAUGE + 5];

fn boolean(src: &str) -> Program {
    compile(src, &Scope::new(&PLAY), Ty::Bool).unwrap()
}

/// An element on `screen` drawing `source` over `r` in `place`.
fn untimed(source: Source, r: [i32; 4], place: Place) -> ElementDef {
    ElementDef {
        timer: TimerRef::Builtin(timer::SCREEN),
        ..widget_support::element(source, r, place)
    }
}

/// [`untimed`] image `image` at `r`, shown while `visible`.
fn shown_when(image: u32, r: [i32; 4], visible: &str) -> ElementDef {
    ElementDef {
        visible: Some(boolean(visible)),
        ..untimed(Source::Image(image), r, Place::SCREEN)
    }
}

/// The simple lanes drawing [`NOTES`], [`LINE`] and [`BARS`].
fn lanes() -> LanesDef {
    LanesDef {
        parts: LaneParts {
            note: [NOTES; 8],
            head: [NOTES; 8],
            tail: [NOTES; 8],
            body_on: [NOTES; 8],
            body_off: [NOTES; 8],
            mine: [NOTES; 8],
            line: Some(LINE),
        },
        bar_groups: vec![BarGroupDef {
            image: BARS,
            ..bar_group(879, 2)
        }],
        ..simple()
    }
}

/// A skin over the widget sheet with `whens`, `lanes`, `elements` and
/// `widgets`.
fn skin(
    whens: Vec<Program>,
    lanes: Option<LanesDef>,
    elements: Vec<ElementDef>,
    widgets: WidgetDefs,
) -> Skin {
    Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: play_support::REGIONS,
        whens,
        sheet: sheet(),
        lanes,
        elements,
        widgets,
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
    .unwrap()
}

/// The descriptor of a preset's gauge 0 with `look`, its border at 80 %.
fn descriptor(look: GaugeLook) -> Option<LoadingGauge> {
    Some(LoadingGauge::of(0, &desc(look, 80)))
}

/// A skin's loading state, the frame it is built with and a batch.
struct Loader {
    skin: Skin,
    uvs: UvTable,
    loading: Loading,
    batch: QuadBatch,
    since: TimeUs,
    percent: i32,
    gauge: Option<LoadingGauge>,
    covers: CoverSettings,
}

impl Loader {
    fn new(skin: Skin) -> Self {
        let loading = skin.prepare_loading(skin_model::Side::Left);
        let batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        Self {
            uvs: uvs(&skin),
            skin,
            loading,
            batch,
            since: TimeUs(0),
            percent: 0,
            gauge: None,
            covers: readouts_for(VIEW).covers,
        }
    }

    fn try_build_us(&mut self, now_us: i64) -> Result<(), BuildError> {
        self.batch.clear();
        let layout = Layout::AUTHOR;
        let frame = LoadingFrame {
            now: TimeUs(now_us),
            load: LoadState {
                since: self.since,
                percent: self.percent,
            },
            target: [1_920, 1_080],
            layout: &layout,
            lanes: VIEW,
            covers: self.covers,
            gauge: self.gauge,
        };
        self.skin
            .build_loading(&mut self.loading, &frame, &self.uvs, &mut self.batch)
    }

    /// Builds the frame at `now_ms`, which must build.
    fn build(&mut self, now_ms: i64) {
        self.try_build_us(now_ms * MS)
            .expect("the loading frame builds");
    }

    /// The image and frame of each quad drawn from the sheet, in draw
    /// order; text lines are left out.
    fn drawn(&self) -> Vec<(u32, u16, Quad)> {
        self.batch
            .quads()
            .iter()
            .filter_map(|q| {
                let mut first = 0;
                for i in 0..IMAGES {
                    for f in 0..frames_of(i) {
                        if self
                            .uvs
                            .get(first + u32::from(f))
                            .is_some_and(|u| u.uv == q.uv && u.layer == q.layer)
                        {
                            return Some((i, f, *q));
                        }
                    }
                    first += u32::from(frames_of(i));
                }
                None
            })
            .collect()
    }

    /// How many quads of `image` drew.
    fn count(&self, image: u32) -> usize {
        self.drawn().iter().filter(|(i, ..)| *i == image).count()
    }

    /// The rects of `image`'s quads.
    fn rects(&self, image: u32) -> Vec<[f32; 4]> {
        self.drawn()
            .iter()
            .filter(|(i, ..)| *i == image)
            .map(|(.., q)| q.rect)
            .collect()
    }

    /// The glyph frames of [`TEN`] drawn, in draw order.
    fn digits(&self) -> Vec<u16> {
        self.drawn()
            .iter()
            .filter(|(i, ..)| *i == TEN)
            .map(|(_, f, _)| *f)
            .collect()
    }

    fn start(&self, slot: u16) -> Option<i64> {
        self.loading.timer(slot).map(|t| t.0)
    }

    fn reading(&self, slot: u16) -> i32 {
        self.loading.state().timers[usize::from(slot)]
    }

    fn field(&self, slot: u16) -> i32 {
        self.loading.state().fields[usize::from(slot)]
    }
}

/// A number of [`TEN`] in four right-aligned cells reading `src` at `x`.
fn number(src: &str) -> NumberDef {
    NumberDef {
        digits: digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Blank),
        value: int(src),
    }
}

/// The background, the lanes, the gauge and the judge, untimed; an
/// element on `ready`; the gauge in 50 segments at y = 100.
fn structural() -> Skin {
    let mut judge = empty_judge();
    judge.grades[JudgeSlot::ALL[0].index()] = Some(grade(GRADE, [0, 0, 10, 10]));
    let mut ready = untimed(Source::Image(MARK), [0, 0, 10, 10], Place::SCREEN);
    ready.timer = TimerRef::Builtin(timer::READY);
    skin(
        Vec::new(),
        Some(lanes()),
        vec![
            untimed(
                Source::Image(BACKGROUND),
                [0, 0, 1_920, 1_080],
                Place::SCREEN,
            ),
            untimed(
                Source::Lanes,
                [0; 4],
                Place {
                    region: Region::Playfield,
                    ..Place::SCREEN
                },
            ),
            untimed(Source::Gauge(0), [0, 100, 500, 20], Place::SCREEN),
            untimed(
                Source::Judge(0),
                [0; 4],
                Place {
                    region: Region::Judge,
                    ..Place::SCREEN
                },
            ),
            ready,
        ],
        WidgetDefs {
            gauges: vec![gauge(50, Motion::Still)],
            judges: vec![judge],
            ..WidgetDefs::default()
        },
    )
}

/// While an untimed structural skin loads, its background, its lanes'
/// judgment line where a play draws it and its gauge's empty frame draw,
/// with both background images on either side of the border; the element
/// on `ready`, notes, bars and the judge stay absent. Without a
/// descriptor the gauge draws nothing.
#[test]
fn a_structural_skin_draws_its_frame_while_loading() {
    let mut l = Loader::new(structural());
    l.gauge = descriptor(GaugeLook::Normal);
    l.build(250);
    assert_eq!(l.count(BACKGROUND), 1);
    // The gauge's 50 segments: 11 at or above the border at 80 %, 39
    // below, none lit.
    assert_eq!((l.count(BACK[0]), l.count(BACK[1])), (11, 39));
    for image in FRONT.into_iter().chain(TIP) {
        assert_eq!(l.count(image), 0, "gauge image {image}");
    }
    for image in [NOTES, BARS, MARK, GRADE] {
        assert_eq!(l.count(image), 0, "image {image}");
    }
    // The judgment line, where a play over a chart without notes draws it.
    let mut play = Harness::new(
        structural(),
        &widget_support::chart(),
        &play_support::full(0),
    );
    play.frame(250, &[]);
    let line = l.rects(LINE);
    assert_eq!(line.len(), 1);
    let played: Vec<_> = play
        .batch
        .quads()
        .iter()
        .filter(|q| {
            l.uvs
                .get(frame_index(LINE))
                .is_some_and(|u| u.uv == q.uv && u.layer == q.layer)
        })
        .map(|q| q.rect)
        .collect();
    assert_eq!(line, played);

    l.gauge = None;
    l.build(266);
    assert_eq!(l.count(BACKGROUND), 1);
    assert_eq!(l.count(LINE), 1);
    for image in FRONT.into_iter().chain(BACK).chain(TIP) {
        assert_eq!(l.count(image), 0, "gauge image {image}");
    }
}

/// The first frame index of `image` in the uv table.
fn frame_index(image: u32) -> u32 {
    (0..image).map(|i| u32::from(frames_of(i))).sum()
}

/// `loading` and `screen` run from the lifetime's origin, every other
/// built-in timer is off, and `load.percent` reads the frame's progress,
/// clamped to 0..=100.
#[test]
fn the_loading_timer_and_progress_follow_the_frame() {
    let mut l = Loader::new(skin(Vec::new(), None, Vec::new(), WidgetDefs::default()));
    l.since = TimeUs(-5_250 * MS);
    l.percent = 40;
    l.build(-5_000);
    for slot in [timer::LOADING, timer::SCREEN] {
        assert_eq!(l.start(slot), Some(-5_250 * MS), "{slot}");
        assert_eq!(l.reading(slot), 250, "{slot}");
    }
    for (slot, t) in (0..).zip(PLAY.timers) {
        if slot != timer::LOADING && slot != timer::SCREEN {
            assert_eq!(l.start(slot), None, "{}", t.name);
        }
    }
    assert_eq!(l.field(field::LOAD_PERCENT), 40);
    l.percent = 140;
    l.build(-4_900);
    assert_eq!(l.field(field::LOAD_PERCENT), 100);
    l.percent = -3;
    l.build(-4_800);
    assert_eq!(l.field(field::LOAD_PERCENT), 0);
}

/// A play keeps `loading` off and `load.percent` at 100, before and
/// after `ready`.
#[test]
fn a_play_keeps_loading_off_and_progress_full() {
    let mut h = Harness::empty(&widget_support::chart(), &play_support::full(1_000));
    for ms in [-500, 0, 500, 2_000] {
        h.frame(ms, &[]);
        assert_eq!(h.start(timer::LOADING), None, "{ms}");
        assert_eq!(h.field(field::LOAD_PERCENT), 100, "{ms}");
    }
}

/// While frames advance with unchanged progress, an animation on
/// `loading` moves and a condition on its reading turns on; when the
/// progress becomes 40 a number reads 40 and a `when` on it starts once
/// and keeps aging.
#[test]
fn loading_animations_progress_and_whens_advance() {
    let mut anim = untimed(Source::Image(BACKGROUND), [0, 0, 10, 10], Place::SCREEN);
    anim.timer = TimerRef::Builtin(timer::LOADING);
    anim.track = skin_model::Track::new(
        &[0, 1_000].map(|ms| skin_model::KeyDef {
            at: TimeUs(ms * MS),
            props: {
                let mut p = [None; 9];
                p[0] = Some(i32::try_from(ms).unwrap());
                p[1] = Some(0);
                p[2] = Some(10);
                p[3] = Some(10);
                p
            },
        }),
        skin_model::Ease::Linear,
        skin_model::End::Hold,
    )
    .unwrap();
    let mut on_when = untimed(Source::Image(OTHER), [0, 0, 10, 10], Place::SCREEN);
    on_when.timer = TimerRef::When(0);
    let mut l = Loader::new(skin(
        vec![boolean("load.percent >= 40")],
        None,
        vec![
            anim,
            shown_when(MARK, [0, 0, 10, 10], "elapsed(loading) >= 500"),
            untimed(Source::Number(0), [0, 200, 80, 20], Place::SCREEN),
            on_when,
        ],
        WidgetDefs {
            numbers: vec![number("load.percent")],
            ..WidgetDefs::default()
        },
    ));
    l.percent = 10;
    l.build(100);
    assert_eq!(l.rects(BACKGROUND)[0][0], 100.0);
    assert_eq!(l.count(MARK), 0);
    assert_eq!(l.digits(), [1, 0]);
    assert_eq!(l.start(timer::BUILTIN), None);
    l.build(600);
    assert_eq!(l.rects(BACKGROUND)[0][0], 600.0);
    assert_eq!(l.count(MARK), 1);
    assert_eq!(l.digits(), [1, 0]);
    l.percent = 40;
    l.build(700);
    assert_eq!(l.digits(), [4, 0]);
    assert_eq!(l.start(timer::BUILTIN), Some(700 * MS));
    assert_eq!(l.count(OTHER), 1);
    l.build(800);
    assert_eq!(l.start(timer::BUILTIN), Some(700 * MS));
    assert_eq!(l.reading(timer::BUILTIN), 100);
    l.percent = 55;
    l.build(900);
    assert_eq!(l.start(timer::BUILTIN), Some(700 * MS));
    assert_eq!(l.reading(timer::BUILTIN), 200);
}

/// A program reading the tally, the score, the combo, the gauge's value
/// or the tempo, in either arm of a conditional, draws nothing while
/// loading: its number, its condition (negated too), its selector and its
/// `when`. A program on live fields alone draws.
#[test]
fn programs_reading_unavailable_fields_draw_nothing() {
    let reads = [
        "combo",
        "ex_score",
        "count.great",
        "gauge",
        "bpm.current_x100",
        "if load.percent > 50 then 1 else combo",
        "if load.percent > 50 then max_combo else 1",
    ];
    for src in reads {
        let choice = ImageChoiceDef {
            value: int(src),
            choices: vec![Some(ImageChoicePart {
                span: ImageSpan {
                    image: OTHER,
                    first: 0,
                    frames: 1,
                },
                cycle: None,
                track: None,
                native: None,
            })],
        };
        let mut on_when = untimed(Source::Image(GRADE), [0, 0, 10, 10], Place::SCREEN);
        on_when.timer = TimerRef::When(0);
        let mut l = Loader::new(skin(
            vec![boolean(&format!("({src}) == 0"))],
            None,
            vec![
                untimed(Source::Number(0), [0, 200, 80, 20], Place::SCREEN),
                shown_when(MARK, [0, 0, 10, 10], &format!("({src}) == 0")),
                shown_when(MARK, [0, 0, 10, 10], &format!("!(({src}) != 0)")),
                untimed(Source::ImageChoice(0), [0, 0, 10, 10], Place::SCREEN),
                on_when,
            ],
            WidgetDefs {
                numbers: vec![number(src)],
                image_choices: vec![choice],
                ..WidgetDefs::default()
            },
        ));
        l.percent = 60;
        l.gauge = descriptor(GaugeLook::Normal);
        l.build(100);
        assert!(l.batch.quads().is_empty(), "{src}: {:?}", l.drawn());
        assert_eq!(l.start(timer::BUILTIN), None, "{src}");
    }
    // A conditional on live fields alone draws.
    let mut l = Loader::new(skin(
        Vec::new(),
        None,
        vec![untimed(Source::Number(0), [0, 200, 80, 20], Place::SCREEN)],
        WidgetDefs {
            numbers: vec![number("if load.percent > 50 then 1 else cover.permille")],
            ..WidgetDefs::default()
        },
    ));
    l.percent = 60;
    l.build(100);
    assert_eq!(l.digits(), [1]);
}

/// The family conditions an imported skin's gauge-type images use.
const GROOVE: &str = "gauge.valid && (gauge.kind == assist_easy || gauge.kind == easy \
                      || gauge.kind == normal)";
const HARD: &str =
    "gauge.valid && (gauge.kind == hard || gauge.kind == ex_hard || gauge.kind == hazard)";

/// A descriptor arriving after preparation lets the programs reading it
/// draw from that frame on, following its look; without one they draw
/// nothing, negated included, and a `when` on a negated condition that
/// started clears.
#[test]
fn descriptor_programs_follow_the_frame() {
    let mut on_when = untimed(Source::Image(GRADE), [0, 0, 10, 10], Place::SCREEN);
    on_when.timer = TimerRef::When(0);
    let mut l = Loader::new(skin(
        vec![boolean(&format!("!({GROOVE})"))],
        None,
        vec![
            shown_when(MARK, [0, 0, 10, 10], GROOVE),
            shown_when(OTHER, [0, 0, 10, 10], HARD),
            shown_when(NOTES, [0, 0, 10, 10], &format!("!({GROOVE})")),
            on_when,
        ],
        WidgetDefs::default(),
    ));
    let shown = |l: &Loader| [MARK, OTHER, NOTES, GRADE].map(|i| l.count(i));
    l.build(100);
    assert_eq!(shown(&l), [0; 4]);
    l.gauge = descriptor(GaugeLook::Normal);
    l.build(200);
    assert_eq!(shown(&l), [1, 0, 0, 0]);
    assert_eq!(l.field(field::GAUGE_VALID), 1);
    l.gauge = descriptor(GaugeLook::Hard);
    l.build(300);
    assert_eq!(shown(&l), [0, 1, 1, 1]);
    assert_eq!(l.start(timer::BUILTIN), Some(300 * MS));
    l.gauge = None;
    l.build(400);
    assert_eq!(shown(&l), [0; 4]);
    assert_eq!(l.start(timer::BUILTIN), None);
}

/// The test lines' uv, apart from every sheet frame.
const UV: skin_model::FrameUv = skin_model::FrameUv {
    uv: [0.125, 0.25, 0.375, 0.5],
    layer: 3,
};

/// Installed text lines draw until their slots are cleared, a composite
/// cleared with its parts, and draw again once installed; neither touches
/// a timer: a `when` started before keeps its start and ages.
#[test]
fn loading_text_draws_until_cleared() {
    let text = |value: TextValue| TextDef {
        value,
        font: skin_model::FontRef::Default,
        align: Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    let values = [
        TextValue::Field(TextField::Title),
        TextValue::Field(TextField::FullTitle),
        TextValue::Field(TextField::Artist),
        TextValue::Literal("X".to_owned()),
    ];
    let elements = (0..4)
        .map(|i| {
            untimed(
                Source::Text(i),
                [100 * i32::try_from(i).unwrap(), 0, 90, 20],
                Place::SCREEN,
            )
        })
        .collect();
    let mut l = Loader::new(skin(
        vec![boolean("load.percent >= 0")],
        None,
        elements,
        WidgetDefs {
            texts: values.map(text).to_vec(),
            ..WidgetDefs::default()
        },
    ));
    let line = || Some(TextImage::new(UV, [0.0, 0.0, 1.0, 1.0], 1.0).unwrap());
    let table = TextTable::new(vec![line(); 4]);
    let lines = |l: &Loader| {
        let mut x: Vec<_> = l
            .batch
            .quads()
            .iter()
            .filter(|q| q.uv == UV.uv)
            .map(|q| (q.rect[0] / 100.0) as i32)
            .collect();
        x.sort_unstable();
        x
    };
    // The `when` on the progress starts with the first build.
    let when = |l: &Loader| (l.start(timer::BUILTIN), l.reading(timer::BUILTIN));
    l.build(100);
    assert_eq!(lines(&l), [] as [i32; 0]);
    assert_eq!(when(&l), (Some(100 * MS), 0));
    l.skin
        .install_loading_text(&mut l.loading, TextSlots::ALL, &table)
        .unwrap();
    l.build(200);
    assert_eq!(lines(&l), [0, 1, 2, 3]);
    assert_eq!(when(&l), (Some(100 * MS), 100));
    let start = l.start(timer::LOADING);
    l.skin
        .clear_loading_text(&mut l.loading, TextSlots::field(TextField::Title));
    l.build(300);
    assert_eq!(lines(&l), [2, 3]);
    assert_eq!(when(&l), (Some(100 * MS), 200));
    l.skin
        .clear_loading_text(&mut l.loading, TextSlots::LITERALS);
    l.build(400);
    assert_eq!(lines(&l), [2]);
    assert_eq!(when(&l), (Some(100 * MS), 300));
    l.skin
        .install_loading_text(
            &mut l.loading,
            TextSlots::field(TextField::FullTitle),
            &table,
        )
        .unwrap();
    l.build(500);
    assert_eq!(lines(&l), [1, 2]);
    assert_eq!(l.start(timer::LOADING), start);
    assert_eq!(l.reading(timer::LOADING), 500);
    assert_eq!(when(&l), (Some(100 * MS), 400));
    // A table of another length is refused and installs nothing.
    assert!(
        l.skin
            .install_loading_text(
                &mut l.loading,
                TextSlots::ALL,
                &TextTable::new(vec![line()])
            )
            .is_err()
    );
    l.build(600);
    assert_eq!(lines(&l), [1, 2]);
}

/// A skin of a title and a full title in a bitmap font of "A" with a
/// fallback font of "B", each glyph 8 px apart.
fn bitmap_text_skin(dir: &std::path::Path) -> Skin {
    for (name, code) in [("a", 65), ("b", 66)] {
        std::fs::write(
            dir.join(format!("{name}.fnt")),
            format!(
                "info face=\"t\" size=10 padding=0,0,0,0\n\
                 common lineHeight=12 base=10 scaleW=16 scaleH=16 pages=1\n\
                 page id=0 file=\"{name}.png\"\n\
                 char id={code} x=0 y=0 width=6 height=9 xoffset=1 yoffset=1 xadvance=8 page=0\n"
            ),
        )
        .unwrap();
        image::RgbaImage::from_pixel(16, 16, image::Rgba([255; 4]))
            .save(dir.join(format!("{name}.png")))
            .unwrap();
    }
    let mut b = skin_model::SheetBuilder::new(&skin_model::Root::new(dir).unwrap());
    let a = b.admit_fnt("a.fnt", &mut |_| Ok(())).unwrap();
    let f = b.admit_fnt("b.fnt", &mut |_| Ok(())).unwrap();
    b.add_bitmap_font(a, &[f]).unwrap();
    let text = |f| TextDef {
        value: TextValue::Field(f),
        font: skin_model::FontRef::Bitmap(0),
        align: Align::Left,
        overflow: skin_model::Overflow::Spill,
    };
    Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: play_support::REGIONS,
        whens: vec![boolean("load.percent >= 0")],
        sheet: b.finish(),
        lanes: None,
        elements: vec![
            untimed(Source::Text(0), [0, 0, 200, 20], Place::SCREEN),
            untimed(Source::Text(1), [0, 100, 200, 20], Place::SCREEN),
        ],
        widgets: WidgetDefs {
            texts: vec![text(TextField::Title), text(TextField::FullTitle)],
            ..WidgetDefs::default()
        },
        bomb_up_to: Some(skin_model::Grade::Bad),
    })
    .unwrap()
}

/// Bitmap lines draw what was installed, are replaced by other text and
/// draw nothing once cleared, a composite with its parts; a `when`
/// started before keeps its start and ages.
#[test]
fn loading_bitmap_text_is_replaced_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut l = Loader::new(bitmap_text_skin(dir.path()));
    // Glyphs per line, by the line's y: the title, then the full title.
    let glyphs = |l: &Loader| {
        let q = l.batch.quads();
        [0.0, 100.0].map(|y| {
            q.iter()
                .filter(|q| q.rect[1] >= y && q.rect[1] < y + 20.0)
                .count()
        })
    };
    let when = |l: &Loader| (l.start(timer::BUILTIN), l.reading(timer::BUILTIN));
    let chart = |title| skin_model::ChartText {
        title,
        subtitle: "B",
        ..skin_model::ChartText::default()
    };
    l.build(100);
    assert_eq!(glyphs(&l), [0, 0]);
    assert_eq!(when(&l), (Some(100 * MS), 0));
    l.skin
        .install_loading_bitmap_text(&mut l.loading, TextSlots::ALL, &chart("AB"));
    l.build(200);
    // "AB", and "AB" and "B" joined by a space the fonts lack.
    let first = glyphs(&l);
    assert_eq!(first[0], 2);
    assert!(first[1] > first[0], "{first:?}");
    assert_eq!(when(&l), (Some(100 * MS), 100));
    l.skin.install_loading_bitmap_text(
        &mut l.loading,
        TextSlots::field(TextField::Title),
        &chart("ABAB"),
    );
    l.build(300);
    assert_eq!(glyphs(&l), [4, first[1] + 2]);
    assert_eq!(when(&l), (Some(100 * MS), 200));
    l.skin
        .clear_loading_text(&mut l.loading, TextSlots::field(TextField::Subtitle));
    l.build(400);
    assert_eq!(glyphs(&l), [4, 0]);
    l.skin
        .clear_loading_text(&mut l.loading, TextSlots::field(TextField::Title));
    l.build(500);
    assert_eq!(glyphs(&l), [0, 0]);
    assert_eq!(when(&l), (Some(100 * MS), 400));
}

/// A refused loading build leaves the state refused: later builds return
/// the same error, while a fresh state builds.
#[test]
fn a_refused_loading_state_builds_no_more() {
    let mut l = Loader::new(structural());
    l.build(100);
    l.covers.cover = 300;
    assert_eq!(l.try_build_us(200 * MS), Err(BuildError::InvalidReadouts));
    assert_eq!(l.loading.refused(), Some(BuildError::InvalidReadouts));
    l.covers = readouts_for(VIEW).covers;
    assert_eq!(l.try_build_us(300 * MS), Err(BuildError::InvalidReadouts));
    l.loading = l.skin.prepare_loading(skin_model::Side::Left);
    assert_eq!(l.try_build_us(400 * MS), Ok(()));
    assert_eq!(l.count(BACKGROUND), 1);
}
