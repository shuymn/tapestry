//! The decision kind of the model (docs/stage3.md S10-09f): a decision
//! skin loads against the decision schema, which has timers and no field,
//! and draws images, constant image choices and the chosen chart's text;
//! its fade-out (timer 2) starts at the declared scene time.

mod result_support;
mod selection_support;

use bms_model::{ChartBuilder, TimeUs};
use result_support::labels;
use selection_support::{frame_of, rect, skin_def, uvs};
use skin_model::{
    Attempt, Blend, BuildError, ChartMetadata, ChartText, DECISION, DecisionError, DecisionFrame,
    DecisionScreen, DecisionTimers, ElementDef, FontRef, ImageChoiceDef, ImageChoicePart,
    ImageSpan, KeyDef, Layout, LoadError, Overflow, PastTable, Place, PlayContext, PrepareError,
    Program, QuadBatch, ResultLabels, Scope, ScreenKind, SelectionText, Side, Skin, SkinDef,
    Source, TextDef, TextField, TextImage, TextTable, TextTableError, TextValue, TimerRef, Ty,
    UvTable, WidgetDefs, compile, decision_timer,
};

fn dint(src: &str) -> Program {
    compile(src, &Scope::new(&DECISION), Ty::Int).unwrap()
}

/// An element drawing `source` over `r` on the screen timer.
fn delement(source: Source, r: [i32; 4]) -> ElementDef {
    ElementDef {
        source,
        track: rect(r),
        timer: TimerRef::Builtin(decision_timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip: None,
        place: Place::SCREEN,
    }
}

fn text(value: TextValue) -> TextDef {
    TextDef {
        value,
        font: FontRef::Default,
        align: skin_model::Align::Left,
        overflow: Overflow::Spill,
    }
}

/// An image choice of images 0 to 2 selected by `value`.
fn choice(value: Program) -> ImageChoiceDef {
    ImageChoiceDef {
        value,
        choices: (0..3)
            .map(|image| {
                Some(ImageChoicePart {
                    span: ImageSpan {
                        image,
                        first: 0,
                        frames: 1,
                    },
                    cycle: None,
                    track: None,
                    native: None,
                })
            })
            .collect(),
        past_table: PastTable::Hide,
    }
}

/// A decision skin of seven texts, one per chart field (title, subtitle,
/// full title, genre, artist, subartist, full artist), slot `i` at y
/// `30 i` in the default font.
fn label_fields() -> SkinDef {
    let fields = [
        TextField::Title,
        TextField::Subtitle,
        TextField::FullTitle,
        TextField::Genre,
        TextField::Artist,
        TextField::Subartist,
        TextField::FullArtist,
    ];
    let elements = (0..fields.len())
        .map(|i| {
            let i = u32::try_from(i).unwrap();
            delement(
                Source::Text(i),
                [0, 30 * i32::try_from(i).unwrap(), 300, 20],
            )
        })
        .collect();
    let widgets = WidgetDefs {
        texts: fields.iter().map(|&f| text(TextValue::Field(f))).collect(),
        ..WidgetDefs::default()
    };
    skin_def(elements, widgets)
}

/// A decision skin with its state, uv table and batch.
struct DFx {
    skin: Skin,
    dec: DecisionScreen,
    uvs: UvTable,
    batch: QuadBatch,
    target: [u32; 2],
    layout: Layout,
    timers: DecisionTimers,
}

impl DFx {
    fn new(def: SkinDef) -> Self {
        let skin = Skin::new_decision(def).unwrap();
        let dec = skin.prepare_decision();
        Self {
            uvs: uvs(&skin),
            batch: QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity()),
            skin,
            dec,
            target: [1_920, 1_080],
            layout: Layout::AUTHOR,
            timers: DecisionTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
            },
        }
    }

    fn frame(&mut self, ms: i64) {
        self.batch.clear();
        let frame = DecisionFrame {
            now: TimeUs(ms * 1_000),
            target: self.target,
            layout: &self.layout,
            timers: self.timers,
        };
        self.skin
            .build_decision(&mut self.dec, &frame, &self.uvs, &mut self.batch)
            .unwrap();
    }

    /// The image, frame and alpha of each quad whose top is at `y`.
    fn at_y(&self, y: f32) -> Vec<(Option<(u32, u16)>, f32)> {
        self.batch
            .quads()
            .iter()
            .filter(|q| q.rect[1] == y)
            .map(|q| (frame_of(&self.uvs, q), q.color[3]))
            .collect()
    }

    /// The text lines drawn: quads of no image.
    fn lines(&self) -> usize {
        self.batch
            .quads()
            .iter()
            .filter(|q| frame_of(&self.uvs, q).is_none())
            .count()
    }
}

/// A decision skin draws images, constant image choices and chart text,
/// and refuses the lanes, a number, a gauge, a judge, a cover paint, the
/// song list, a ratio bar, each graph, a selection string and an image
/// choice whose selector is not a constant, each at its element; and only
/// its two built-in timers.
#[test]
fn a_decision_skin_refuses_what_other_screens_draw() {
    let widgets = || WidgetDefs {
        texts: vec![
            text(TextValue::Field(TextField::Title)),
            text(TextValue::Selection(SelectionText::Query)),
        ],
        image_choices: vec![choice(dint("1")), choice(dint("elapsed(screen) / 100"))],
        ..WidgetDefs::default()
    };
    let ok = vec![
        delement(Source::Image(0), [0, 0, 10, 10]),
        delement(Source::ImageChoice(0), [0, 20, 10, 10]),
        delement(Source::Text(0), [0, 40, 100, 20]),
    ];
    let skin = Skin::new_decision(skin_def(ok.clone(), widgets())).unwrap();
    assert_eq!(skin.kind(), ScreenKind::Decision);
    // The constant choice draws its entry 1: image 1's first frame.
    let mut fx = DFx::new(skin_def(ok.clone(), widgets()));
    fx.frame(0);
    assert_eq!(fx.at_y(20.0), [(Some((1, 0)), 1.0)]);
    for refused in [
        Source::Lanes,
        Source::Number(0),
        Source::Gauge(0),
        Source::Judge(0),
        Source::CoverPaint(0),
        Source::SongList,
        Source::RatioBar(0),
        Source::JudgeGraph(0),
        Source::GaugeGraph(0),
        Source::TimingGraph(0),
        Source::Text(1),
        Source::ImageChoice(1),
    ] {
        let mut elements = ok.clone();
        elements.push(delement(refused, [0, 60, 10, 10]));
        let got = Skin::new_decision(skin_def(elements, widgets()));
        assert!(
            matches!(
                got,
                Err(LoadError::SourceKind {
                    element: 3,
                    kind: ScreenKind::Decision
                })
            ),
            "{refused:?}: {got:?}"
        );
    }
    // A timer past the decision's two, which play has, is not a decision
    // timer.
    let mut elements = ok;
    elements.push(ElementDef {
        timer: TimerRef::Builtin(decision_timer::BUILTIN),
        ..delement(Source::Image(0), [0, 0, 10, 10])
    });
    assert!(matches!(
        Skin::new_decision(skin_def(elements, widgets())),
        Err(LoadError::UnknownTimer { element: 3 })
    ));
}

/// The decision entry points refuse a skin of another kind, or stay
/// inert; a decision skin's play, selection and result entry points
/// refuse; and a state another decision skin made is refused.
#[test]
fn decision_entry_points_refuse_other_kinds() {
    let elements = || vec![delement(Source::Image(0), [0, 0, 10, 10])];
    let play = Skin::new(skin_def(elements(), WidgetDefs::default())).unwrap();
    assert_eq!(play.kind(), ScreenKind::Play);
    let mut inert = play.prepare_decision();
    assert_eq!(
        play.update_decision(&mut inert, &labels()),
        Err(DecisionError::Kind)
    );
    let mut fx = DFx::new(skin_def(elements(), WidgetDefs::default()));
    let frame = DecisionFrame {
        now: TimeUs(0),
        target: fx.target,
        layout: &fx.layout,
        timers: fx.timers,
    };
    assert_eq!(
        play.build_decision(&mut inert, &frame, &fx.uvs, &mut fx.batch),
        Err(BuildError::Kind)
    );
    // The decision skin refuses a play, selection and a result.
    let chart = ChartBuilder::new(150_000).build().unwrap();
    let ctx = PlayContext {
        meta: ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &[],
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
    };
    assert!(matches!(
        fx.skin.prepare(&chart, &ctx),
        Err(PrepareError::Kind)
    ));
    let mut sel = fx.skin.prepare_selection();
    assert!(
        fx.skin
            .update_selection(
                &mut sel,
                &selection_support::view(Vec::new(), None),
                &selection_support::labels_of()
            )
            .is_err()
    );
    let mut res = fx.skin.prepare_result();
    let result_frame = skin_model::ResultFrame {
        now: TimeUs(0),
        target: fx.target,
        layout: &fx.layout,
        timers: skin_model::ResultTimers::default(),
    };
    assert_eq!(
        fx.skin
            .build_result(&mut res, &result_frame, &fx.uvs, &mut fx.batch),
        Err(BuildError::Kind)
    );
    // Another decision skin's state.
    let other = Skin::new_decision(skin_def(elements(), WidgetDefs::default())).unwrap();
    let mut foreign = other.prepare_decision();
    assert_eq!(
        fx.skin.update_decision(&mut foreign, &labels()),
        Err(DecisionError::Kind)
    );
    assert_eq!(
        fx.skin
            .attach_decision_text(&mut foreign, TextTable::default()),
        Err(TextTableError::Foreign)
    );
    assert!(
        fx.skin
            .decision_text_requests(&foreign, fx.target, &fx.layout)
            .is_empty()
    );
    fx.skin.update_decision(&mut fx.dec, &labels()).unwrap();
    fx.frame(0);
    assert_eq!(fx.at_y(0.0), [(Some((0, 0)), 1.0)]);
    // A build of another skin's state, or with another sheet's uv table,
    // draws nothing.
    let frame = DecisionFrame {
        now: TimeUs(0),
        target: fx.target,
        layout: &fx.layout,
        timers: fx.timers,
    };
    fx.batch.clear();
    fx.skin
        .build_decision(&mut foreign, &frame, &fx.uvs, &mut fx.batch)
        .unwrap();
    assert!(fx.batch.quads().is_empty());
    let other_uvs = uvs(&Skin::new_decision(skin_def(elements(), WidgetDefs::default())).unwrap());
    assert_ne!(other_uvs.sheet_id(), fx.uvs.sheet_id());
    fx.skin
        .build_decision(&mut fx.dec, &frame, &other_uvs, &mut fx.batch)
        .unwrap();
    assert!(fx.batch.quads().is_empty());
}

/// The text elements show the chosen chart's labels as play builds them,
/// the full title and artist joined, an empty one giving no line; each
/// attached line draws, and a table of another length is refused.
#[test]
fn decision_text_shows_the_labels() {
    let mut fx = DFx::new(label_fields());
    let chosen = ResultLabels {
        subartist: "Other".to_owned(),
        ..labels()
    };
    // Nothing before the labels.
    assert!(
        fx.skin
            .decision_text_requests(&fx.dec, fx.target, &fx.layout)
            .is_empty()
    );
    fx.skin.update_decision(&mut fx.dec, &chosen).unwrap();
    let requests = fx
        .skin
        .decision_text_requests(&fx.dec, fx.target, &fx.layout);
    let texts: Vec<(u16, &str)> = requests.iter().map(|r| (r.slot, r.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            (0, "Song"),
            (1, "[X]"),
            (2, "Song [X]"),
            (3, "Genre"),
            (4, "Someone"),
            (5, "Other"),
            (6, "Someone Other"),
        ]
    );
    fx.skin.update_decision(&mut fx.dec, &labels()).unwrap();
    let slots: Vec<u16> = fx
        .skin
        .decision_text_requests(&fx.dec, fx.target, &fx.layout)
        .iter()
        .map(|r| r.slot)
        .collect();
    assert_eq!(slots, [0, 1, 2, 3, 4, 6]);
    assert_eq!(
        fx.skin
            .attach_decision_text(&mut fx.dec, TextTable::new(vec![None])),
        Err(TextTableError::Len {
            expected: 7,
            got: 1
        })
    );
    let line = || {
        TextImage::new(
            skin_model::FrameUv {
                uv: [0.0, 0.0, 0.5, 0.5],
                layer: 0,
            },
            [0.0, 0.0, 1.0, 1.0],
            1.0,
        )
    };
    let table = (0..7)
        .map(|s| slots.contains(&s).then(line).flatten())
        .collect();
    fx.frame(0);
    assert_eq!(fx.lines(), 0);
    fx.skin
        .attach_decision_text(&mut fx.dec, TextTable::new(table))
        .unwrap();
    fx.frame(0);
    assert_eq!(fx.lines(), 6);
}

/// Bitmap text on the decision screen: laid out from the labels by
/// `attach_decision_bitmap_text`, one glyph quad per character, nothing
/// before it is called, and laid out again for new labels.
#[test]
fn decision_bitmap_text_follows_the_labels() {
    let (_dir, sheet) = selection_support::bitmap_sheet();
    let mut def = skin_def(
        vec![
            delement(Source::Text(0), [0, 0, 200, 20]),
            delement(Source::Text(1), [0, 40, 200, 20]),
        ],
        WidgetDefs {
            texts: [TextField::Title, TextField::Genre]
                .map(|f| TextDef {
                    font: FontRef::Bitmap(0),
                    ..text(TextValue::Field(f))
                })
                .to_vec(),
            ..WidgetDefs::default()
        },
    );
    def.sheet = sheet;
    let skin = Skin::new_decision(def.clone()).unwrap();
    let mut dec = skin.prepare_decision();
    let atlas_uvs = |skin: &Skin| {
        skin.sheet()
            .atlas(
                skin_model::Variant::Lo,
                skin_model::AtlasLimits {
                    max_side: 2_048,
                    max_layers: 4,
                    reserved_layers: 1,
                },
                &|| false,
            )
            .unwrap()
            .into_parts()
            .1
    };
    let uvs = atlas_uvs(&skin);
    let layout = Layout::AUTHOR;
    let glyphs_of = |skin: &Skin, uvs: &UvTable, dec: &mut DecisionScreen| {
        let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let frame = DecisionFrame {
            now: TimeUs(0),
            target: [1_920, 1_080],
            layout: &layout,
            timers: DecisionTimers {
                screen: Some(TimeUs(0)),
                fadeout: None,
            },
        };
        skin.build_decision(dec, &frame, uvs, &mut batch).unwrap();
        // Each line's glyphs sit above its box's bottom: the title's
        // (box at y 0) above y 20, the genre's (box at y 40) below it.
        let first = batch.quads().iter().filter(|q| q.rect[1] < 20.0).count();
        [first, batch.quads().len() - first]
    };
    let glyphs_at = |dec: &mut DecisionScreen| glyphs_of(&skin, &uvs, dec);
    let chosen = |title: &str, genre: &str| ResultLabels {
        title: title.to_owned(),
        genre: genre.to_owned(),
        ..labels()
    };
    skin.update_decision(&mut dec, &chosen("ABA", "B")).unwrap();
    assert_eq!(glyphs_at(&mut dec), [0, 0]);
    skin.attach_decision_bitmap_text(&mut dec);
    assert_eq!(glyphs_at(&mut dec), [3, 1]);
    skin.update_decision(&mut dec, &chosen("BB", "AAAB"))
        .unwrap();
    skin.attach_decision_bitmap_text(&mut dec);
    assert_eq!(glyphs_at(&mut dec), [2, 4]);
    // Control characters are dropped before a line is cut, as play does:
    // 511 tabs between two glyphs leave both.
    let tabs = format!("A{}B", "\t".repeat(511));
    skin.update_decision(&mut dec, &chosen(&tabs, "B")).unwrap();
    skin.attach_decision_bitmap_text(&mut dec);
    assert_eq!(glyphs_at(&mut dec), [2, 1]);
    // A state of another skin of the same definition is left as it is:
    // its own skin still draws no glyph for it.
    let other = Skin::new_decision(def).unwrap();
    let other_uvs = atlas_uvs(&other);
    let mut foreign = other.prepare_decision();
    other
        .update_decision(&mut foreign, &chosen("AB", "B"))
        .unwrap();
    skin.attach_decision_bitmap_text(&mut foreign);
    assert_eq!(glyphs_of(&other, &other_uvs, &mut foreign), [0, 0]);
    other.attach_decision_bitmap_text(&mut foreign);
    assert_eq!(glyphs_of(&other, &other_uvs, &mut foreign), [2, 1]);
}

/// The labels' lines are play's for the same strings: control characters
/// dropped before a line is cut (511 tabs inside a 513-byte title), a field
/// of control characters alone still joined as written, a long field cut.
#[test]
fn decision_text_matches_plays_lines() {
    let mut fx = DFx::new(label_fields());
    let raw = ResultLabels {
        title: format!("A{}B", "\t".repeat(511)),
        subtitle: "\t".to_owned(),
        artist: "é".repeat(400),
        subartist: format!("\u{7}{}", "y".repeat(600)),
        genre: "\n\n".to_owned(),
    };
    assert_eq!(raw.title.len(), 513);
    fx.skin.update_decision(&mut fx.dec, &raw).unwrap();
    let decision = fx
        .skin
        .decision_text_requests(&fx.dec, fx.target, &fx.layout);
    let play = fx.skin.text_requests(
        &ChartText {
            title: &raw.title,
            subtitle: &raw.subtitle,
            artist: &raw.artist,
            subartist: &raw.subartist,
            genre: &raw.genre,
        },
        fx.target,
        &fx.layout,
    );
    let lines = |r: &[skin_model::TextRequest]| -> Vec<(u16, String)> {
        r.iter().map(|r| (r.slot, r.text.clone())).collect()
    };
    assert_eq!(lines(&decision), lines(&play));
    assert_eq!(decision.first().map(|r| r.text.as_str()), Some("AB"));
}

/// The fade-out follows the scene: an element on the fade-out timer, its
/// alpha rising from 0 to 255 over 500 ms, draws nothing until the
/// fade-out starts at the scene's end (2 500 ms here), then fades in to
/// the screen's end at 3 000 ms; an element on the screen timer draws
/// from the choice.
#[test]
fn the_fade_out_starts_at_the_scene() {
    let fade = {
        let key = |ms: i64, alpha: i32| {
            let mut props = [None; 9];
            props[..5].copy_from_slice(&[Some(0), Some(100), Some(10), Some(10), Some(alpha)]);
            KeyDef {
                at: TimeUs(ms * 1_000),
                props,
            }
        };
        skin_model::Track::new(
            &[key(0, 0), key(500, 255)],
            skin_model::Ease::Linear,
            skin_model::End::Hold,
        )
        .unwrap()
    };
    let mut fx = DFx::new(skin_def(
        vec![
            delement(Source::Image(0), [0, 0, 10, 10]),
            ElementDef {
                track: fade,
                timer: TimerRef::Builtin(decision_timer::FADEOUT),
                ..delement(Source::Image(1), [0, 100, 10, 10])
            },
        ],
        WidgetDefs::default(),
    ));
    fx.skin.update_decision(&mut fx.dec, &labels()).unwrap();
    let scene = 2_500;
    let mut alpha_at = |ms: i64| {
        fx.timers.fadeout = (ms >= scene).then_some(TimeUs(scene * 1_000));
        fx.frame(ms);
        assert_eq!(fx.at_y(0.0), [(Some((0, 0)), 1.0)], "{ms} ms");
        fx.at_y(100.0)
            .into_iter()
            .map(|(f, a)| {
                assert_eq!(f, Some((1, 0)));
                a
            })
            .next()
    };
    assert_eq!(alpha_at(0), None);
    assert_eq!(alpha_at(2_499), None);
    for (ms, want) in [(2_500, 0.0), (2_750, 127.5), (2_999, 254.49)] {
        match alpha_at(ms) {
            Some(a) => assert!((a - want / 255.0).abs() < 0.005, "{ms} ms: {a}"),
            // A fully transparent quad may be left out.
            None => assert_eq!(want, 0.0, "{ms} ms"),
        }
    }
}
