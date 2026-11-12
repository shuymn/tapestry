//! The result kind's installations on WARP (S10-09i i-2; design.md
//! "Rendering and Skins" and "Memory Budget"): a layout staged on the
//! pages installed keeps their array and installs its own layout, uv
//! table, capacities and clips (C8), never drawing a frame of two layouts
//! and drawing the newest staging's number, text and strip whole (C2's
//! renderer half); its own text producer's lines, never another bundle's
//! on the same pages; the pages-only installation; play admitted by
//! releasing the result, reported (C9); the result disabled for the run
//! at every entry, another kind's staging untouched; the witness, whole
//! (C21), and shared pages counted once; and the result's lines staged and
//! installed.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bms_model::{ChartBuilder, TimeUs};
use render::{
    AttemptText, Blend, ClipTable, CommitError, Installation, MAX_SPRITES, MemoryWitness,
    RESULT_STRIP_SPRITES, RenderError, Renderer, SHARED_TEXELS, SkinBundle, Sprite, Staging,
    pages_bytes, pages_texels, solid_uv,
};
use skin_model::{
    Align, Anchor, BestView, ClipId, DigitsDef, Ease, ElementDef, End, FileDef, Filter, Fit,
    FontRef, GlyphSet, ImageDef, KeyDef, LampKey, Layout, NumberDef, Overflow, Pad, Place, RESULT,
    Region, RegionDef, ResultChart, ResultDefs, ResultFrame, ResultLabels, ResultScore,
    ResultScreen, ResultStatus, ResultTimers, ResultView, Root, Scope, ScreenKind, Side, Skin,
    SkinDef, SolidImageDef, Source, TextDef, TextField, TextValue, TimerRef, Track, Ty, Variant,
    WidgetDefs, compile, result_timer,
};

use crate::test_common::{GAUGES, attempt, context, offscreen, read_back, rgb_at};
use crate::test_decl::{Look, Text, solid_bundle};

const W: u32 = 1_920;
const H: u32 = 1_080;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const BLACK: [u8; 3] = [0, 0, 0];
const R3: [u8; 3] = [255, 0, 0];
const G3: [u8; 3] = [0, 255, 0];
const B3: [u8; 3] = [0, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];
const YELLOW: [u8; 3] = [255, 255, 0];
/// Where the image bands are read back.
const A_AT: (u32, u32) = (200, 540);
const B_AT: (u32, u32) = (1_200, 540);

/// What a test skin draws, on the screen timer.
#[derive(Clone, Copy)]
struct Draws {
    /// Full-height bands 400 wide: (image, left edge).
    bands: &'static [(u32, i32)],
    /// The sheet's two cells in the other order: green is image 0.
    swapped: bool,
    /// The first band under clip 0.
    clipped: bool,
    /// One-texel copies of image 0 past every read point, raising the
    /// quad capacity.
    copies: usize,
    /// A one-cell number of `ex_score` in blue at this left edge.
    number: Option<i32>,
    /// A text element of the chart's title at this left edge.
    text: Option<i32>,
}

/// A square PNG of `side` texels of `rgba`.
fn png(path: &Path, side: u32, rgba: [u8; 4]) {
    let w = BufWriter::new(File::create(path).unwrap());
    let mut e = png::Encoder::new(w, side, side);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = (0..side * side).flat_map(|_| rgba).collect();
    e.write_header().unwrap().write_image_data(&data).unwrap();
}

/// An element of `source` over `rect` on the screen timer.
fn element(source: Source, [x, y, w, h]: [i32; 4], clip: Option<ClipId>) -> ElementDef {
    let mut props = [None; 9];
    props[..4].copy_from_slice(&[Some(x), Some(y), Some(w), Some(h)]);
    ElementDef {
        source,
        track: Track::new(
            &[KeyDef {
                at: TimeUs(0),
                props,
            }],
            Ease::Linear,
            End::Hold,
        )
        .unwrap(),
        timer: TimerRef::Builtin(result_timer::SCREEN),
        visible: None,
        blend: Blend::Alpha,
        clip,
        place: Place::SCREEN,
    }
}

/// A result skin over `dir`'s sheet: a red and a green cell (swapped when
/// asked), then a ten-glyph blue digit strip, drawing `d`.
fn skin(dir: &Path, d: Draws) -> Skin {
    png(&dir.join("digits.png"), 160, BLUE);
    let mut b = skin_model::SheetBuilder::new(&Root::new(dir).unwrap());
    let cells = if d.swapped {
        [GREEN, RED]
    } else {
        [RED, GREEN]
    };
    for color in cells {
        b.add_solid_image(SolidImageDef {
            size: [16, 16],
            color,
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    }
    let file = b
        .admit_file(FileDef {
            path: "digits.png".into(),
            hi: None,
        })
        .unwrap();
    let digits = b
        .add_image(ImageDef {
            file,
            rect: None,
            grid: [10, 1],
            cycle: None,
            filter: Filter::Nearest,
        })
        .unwrap();
    let mut widgets = WidgetDefs::default();
    let mut elements: Vec<ElementDef> = d
        .bands
        .iter()
        .enumerate()
        .map(|(k, &(image, x))| {
            let clip = (k == 0 && d.clipped).then_some(ClipId(0));
            element(Source::Image(image), [x, 0, 400, 1_080], clip)
        })
        .collect();
    elements.extend(std::iter::repeat_n(
        element(Source::Image(0), [1_919, 1_079, 1, 1], None),
        d.copies,
    ));
    if let Some(x) = d.number {
        widgets.numbers.push(NumberDef {
            digits: DigitsDef {
                image: digits,
                set: GlyphSet::Ten,
                cells: 1,
                align: Align::Left,
                pad: Pad::Blank,
                space: 0,
                offsets: Vec::new(),
            },
            value: compile("ex_score", &Scope::new(&RESULT), Ty::Int).unwrap(),
        });
        elements.push(element(Source::Number(0), [x, 300, 40, 100], None));
    }
    if let Some(x) = d.text {
        widgets.texts.push(TextDef {
            value: TextValue::Field(TextField::Title),
            font: FontRef::Default,
            align: Align::Left,
            overflow: Overflow::Spill,
        });
        elements.push(element(Source::Text(0), [x, 700, 100, 100], None));
    }
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements,
        widgets,
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    };
    Skin::new_result(def, ResultDefs::default()).unwrap()
}

/// A text producer of its own ([`Text`]), counting its rasters.
fn producer() -> Arc<Text> {
    Arc::new(Text {
        calls: AtomicU32::new(0),
        rasters: AtomicU32::new(0),
        refuse: AtomicBool::new(false),
        strings: Mutex::new(Vec::new()),
    })
}

/// `s`'s bundle on `atlas` with `clips`, with `text` producing its lines.
fn bundle(
    s: Skin,
    atlas: skin_model::Atlas,
    clips: Vec<[f32; 4]>,
    text: Option<&Arc<Text>>,
) -> Arc<SkinBundle> {
    let b = SkinBundle::new(
        Arc::new(s),
        atlas,
        ClipTable::new(clips).unwrap(),
        Side::Left,
    )
    .unwrap();
    Arc::new(match text {
        Some(t) => {
            let t: Arc<dyn AttemptText> = Arc::clone(t) as Arc<dyn AttemptText>;
            b.with_text(t).unwrap()
        }
        None => b,
    })
}

/// A first skin drawing `a`, with its atlas, and the skins drawing each of
/// `others` with their atlases rebound to its pages.
type OnPages = (Skin, skin_model::Atlas, Vec<(Skin, skin_model::Atlas)>);

fn on_pages(r: &Renderer, dir: &Path, a: Draws, others: &[Draws]) -> OnPages {
    let first = skin(dir, a);
    let text = a.text.is_some();
    let atlas = first
        .atlas(Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    let rest = others
        .iter()
        .map(|&d| {
            let s = skin(dir, d);
            let rebound = atlas.rebind(s.sheet(), Variant::Lo, text).unwrap();
            (s, rebound)
        })
        .collect();
    (first, atlas, rest)
}

/// A: red at x 0 (and green past the read points, so its pages hold both
/// cells). B: on A's pages, its sheet's cells swapped so its uv table
/// differs, image 0 (green) at x 1000 under a clip that keeps x 1000 to
/// 1200, with more quads than [`MAX_SPRITES`]. C: red at x 0 on pages of
/// its own.
struct Bundles {
    a: Arc<SkinBundle>,
    b: Arc<SkinBundle>,
    c: Arc<SkinBundle>,
}

const A: Draws = Draws {
    bands: &[(0, 0), (1, 1_500)],
    swapped: false,
    clipped: false,
    copies: 0,
    number: None,
    text: None,
};

fn bundles(r: &Renderer) -> Bundles {
    let dir = tempfile::tempdir().unwrap();
    let b_draws = Draws {
        bands: &[(0, 1_000)],
        swapped: true,
        clipped: true,
        copies: MAX_SPRITES + 100,
        ..A
    };
    let (a, atlas, mut rest) = on_pages(r, dir.path(), A, &[b_draws]);
    let (b, rebound) = rest.pop().unwrap();
    let other = tempfile::tempdir().unwrap();
    let c = skin(other.path(), A);
    let c_atlas = c.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    let out = Bundles {
        a: bundle(a, atlas, Vec::new(), None),
        b: bundle(b, rebound, vec![[1_000.0, 0.0, 200.0, 1_080.0]], None),
        c: bundle(c, c_atlas, Vec::new(), None),
    };
    assert_eq!(out.a.atlas_id(), out.b.atlas_id());
    assert_ne!(out.a.atlas_id(), out.c.atlas_id());
    assert_ne!(out.a.uvs().frames(), out.b.uvs().frames());
    assert!(out.b.skin().quad_capacity() > MAX_SPRITES as u32);
    out
}

/// A ready view of `ex` EX score.
fn view(ex: u32) -> ResultView {
    let mut score = ResultScore {
        clear: LampKey::Normal,
        ..ResultScore::default()
    };
    score.tally.ex_score = ex;
    ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score,
        best: BestView::Unavailable,
        timing: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
        timelines: None,
    }
}

/// The chart's labels: titled "T".
fn labels() -> ResultLabels {
    ResultLabels {
        title: "T".to_owned(),
        ..ResultLabels::default()
    }
}

/// `bundle`'s result state with the view of EX 7.
fn state(bundle: &SkinBundle) -> ResultScreen {
    let mut res = bundle.skin().prepare_result();
    bundle
        .skin()
        .update_result(&mut res, &view(7), &labels())
        .unwrap();
    res
}

/// The frame at time 0 with the screen timer on.
fn at_zero(layout: &Layout) -> ResultFrame<'_> {
    ResultFrame {
        now: TimeUs(0),
        target: [W, H],
        layout,
        timers: ResultTimers {
            screen: Some(TimeUs(0)),
            fadeout: None,
        },
    }
}

/// Builds, submits and reads back a result frame of `res` with `strip`.
fn pixels(r: &mut Renderer, res: &mut ResultScreen, strip: &[Sprite]) -> Vec<u8> {
    let layout = Layout::AUTHOR;
    r.build_result(res, &at_zero(&layout), strip).unwrap();
    assert!(r.submit_result(&[]).unwrap().presented);
    read_back(r)
}

/// The colours at A's and B's bands.
fn frame(r: &mut Renderer, res: &mut ResultScreen) -> ([u8; 3], [u8; 3]) {
    let px = pixels(r, res, &[]);
    (rgb_at(&px, W, A_AT), rgb_at(&px, W, B_AT))
}

/// A yellow strip band at layout `x`, at the bottom.
fn strip_at(x: f32) -> Sprite {
    Sprite {
        rect: [x, 1_000.0, 100.0, 42.0],
        uv: solid_uv(),
        color: [1.0, 1.0, 0.0, 1.0],
        ..Sprite::default()
    }
}

/// C8, C2: B staged on A's pages uploads nothing and draws nothing until
/// its commit, A drawing whole meanwhile; committed, B draws its own quads
/// through its own uv table (its image 0 is green), under its own clip,
/// with its own capacities, A's layout gone, on the same array.
#[test]
fn a_layout_on_the_pages_installed_draws_its_own_quads() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let token = r.install(ScreenKind::Result, &x.a, 1).unwrap();
    r.commit(token).unwrap();
    let uploads = r.atlas_uploads();
    let mut a = state(&x.a);
    assert_eq!(frame(&mut r, &mut a), (R3, BLACK));
    let token = r.install(ScreenKind::Result, &x.b, 2).unwrap();
    assert_eq!(r.atlas_uploads(), uploads, "B's staging uploaded an array");
    // Staged, not presented: A's frames whole.
    assert_eq!(frame(&mut r, &mut a), (R3, BLACK));
    r.commit(token).unwrap();
    assert_eq!(r.atlas_uploads(), uploads);
    assert!(Arc::ptr_eq(r.result_bundle().unwrap(), &x.b));
    let mut b = state(&x.b);
    let px = pixels(&mut r, &mut b, &[]);
    assert_eq!(rgb_at(&px, W, A_AT), BLACK, "A's layout drawn");
    assert_eq!(rgb_at(&px, W, (1_100, 540)), G3, "B's uv table not used");
    assert_eq!(rgb_at(&px, W, (1_300, 540)), BLACK, "B's clip not applied");
    // Its capacity past the floor: every copy staged.
    let layout = Layout::AUTHOR;
    r.build_result(&mut b, &at_zero(&layout), &[]).unwrap();
    let drawn = r.submit_result(&[]).unwrap();
    assert_eq!(drawn.sprites, 1 + MAX_SPRITES + 100);
    assert_eq!(drawn.dropped, 0);
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.a.atlas_id()), None, None]
    );
    // A bundle on other pages uploads its own array; committed, the one it
    // replaces is kept until the next staging.
    let token = r.install(ScreenKind::Result, &x.c, 3).unwrap();
    assert_eq!(r.atlas_uploads(), uploads + 1);
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.a.atlas_id()), Some(x.c.atlas_id()), None]
    );
    r.commit(token).unwrap();
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.c.atlas_id()), None, Some(x.a.atlas_id())]
    );
    let mut c = state(&x.c);
    assert_eq!(frame(&mut r, &mut c), (R3, BLACK));
    let _token = r.install(ScreenKind::Result, &x.a, 4).unwrap();
    assert_eq!(r.retained_atlases(ScreenKind::Result)[2], None, "dropped");
}

/// C2's renderer half: of several stagings on the same pages, only the
/// newest commits; until then A's frames draw A's band, number, text and
/// strip whole, none of B's; committed, B2's draw B2's whole and none of
/// A's or B1's.
#[test]
fn the_newest_staging_draws_its_marks_whole() {
    let mut r = offscreen(W, H);
    let dir = tempfile::tempdir().unwrap();
    // One red band (and green past every read point), a number and a text.
    let marks = |bands: &'static [(u32, i32)], at: i32| Draws {
        bands,
        number: Some(at),
        text: Some(at),
        ..A
    };
    let (a, atlas, mut rest) = on_pages(
        &r,
        dir.path(),
        marks(&[(0, 0), (1, 1_919)], 500),
        &[
            marks(&[(0, 600), (1, 1_919)], 900),
            marks(&[(0, 1_000), (1, 1_919)], 1_500),
        ],
    );
    let (b2, b2_atlas) = rest.pop().unwrap();
    let (b1, b1_atlas) = rest.pop().unwrap();
    let a = bundle(a, atlas, Vec::new(), Some(&producer()));
    let b1 = bundle(b1, b1_atlas, Vec::new(), Some(&producer()));
    let b2 = bundle(b2, b2_atlas, Vec::new(), Some(&producer()));
    let token = r.install(ScreenKind::Result, &a, 1).unwrap();
    r.commit(token).unwrap();
    let mut ra = state(&a);
    r.prepare_result_text(&mut ra).unwrap();
    assert!(r.install_staged_result_text());
    let first = r.install(ScreenKind::Result, &b1, 2).unwrap();
    let newest = r.install(ScreenKind::Result, &b2, 3).unwrap();
    let mut rb2 = state(&b2);
    r.prepare_result_text(&mut rb2).unwrap();
    assert!(r.commit(first).is_err(), "a superseded staging committed");
    // A's marks whole: its band, number, text and strip; none of B's.
    let px = pixels(&mut r, &mut ra, &[strip_at(100.0)]);
    let at = |x: u32, y: u32| rgb_at(&px, W, (x, y));
    assert_eq!(at(200, 540), R3);
    assert_eq!(at(510, 350), B3, "A's number");
    assert_eq!(at(505, 705), WHITE, "A's text");
    assert_eq!(at(150, 1_020), YELLOW, "A's strip");
    for (x, y) in [
        (800, 540),
        (1_200, 540),
        (910, 350),
        (1_510, 350),
        (1_505, 705),
    ] {
        assert_eq!(at(x, y), BLACK, "B's mark at {x}, {y} in A's frame");
    }
    r.commit(newest).unwrap();
    assert!(r.install_staged_result_text());
    let px = pixels(&mut r, &mut rb2, &[strip_at(1_800.0)]);
    let at = |x: u32, y: u32| rgb_at(&px, W, (x, y));
    assert_eq!(at(1_200, 540), R3, "B2's band");
    assert_eq!(at(1_510, 350), B3, "B2's number");
    assert_eq!(at(1_505, 705), WHITE, "B2's text");
    assert_eq!(at(1_850, 1_020), YELLOW, "B2's strip");
    for (x, y) in [
        (200, 540),
        (800, 540),
        (510, 350),
        (505, 705),
        (910, 350),
        (150, 1_020),
    ] {
        assert_eq!(
            at(x, y),
            BLACK,
            "A's or B1's mark at {x}, {y} in B2's frame"
        );
    }
}

/// Two bundles on the same pages, each with its own text producer, whose
/// skins name the same font: B's lines are laid out and rasterized by B's
/// producer, never taken from A's in the shared text layer.
#[test]
fn bundles_on_one_array_keep_their_own_lines() {
    let mut r = offscreen(W, H);
    let dir = tempfile::tempdir().unwrap();
    let draws = Draws {
        text: Some(500),
        ..A
    };
    let (a, atlas, mut rest) = on_pages(&r, dir.path(), draws, &[draws]);
    let (b, rebound) = rest.pop().unwrap();
    let (pa, pb) = (producer(), producer());
    let a = bundle(a, atlas, Vec::new(), Some(&pa));
    let b = bundle(b, rebound, Vec::new(), Some(&pb));
    assert_eq!(a.atlas_id(), b.atlas_id());
    assert_ne!(a.text_scope(), b.text_scope());
    let token = r.install(ScreenKind::Result, &a, 1).unwrap();
    r.commit(token).unwrap();
    let mut ra = state(&a);
    r.prepare_result_text(&mut ra).unwrap();
    assert!(r.install_staged_result_text());
    assert_eq!(pa.rasters.load(Ordering::SeqCst), 1);
    let token = r.install(ScreenKind::Result, &b, 2).unwrap();
    let mut rb = state(&b);
    r.prepare_result_text(&mut rb).unwrap();
    assert_eq!(pb.rasters.load(Ordering::SeqCst), 1, "B took A's line");
    r.commit(token).unwrap();
    assert!(r.install_staged_result_text());
    let px = pixels(&mut r, &mut rb, &[]);
    assert_eq!(rgb_at(&px, W, (505, 705)), WHITE);
    // The witness counts the shared text layer's cache with the result.
    let cache = r.result_text_cache_bytes();
    assert!(cache > 0);
    assert!(
        r.memory().result_text > cache,
        "the result's text cache not counted"
    );
}

/// G1's timing (the S10-09i design's §8.1): one per upload, taken once,
/// with the queue's report of the submitted work; none for pages already
/// installed, and one again after the installation is removed (G1's cold
/// precondition).
#[test]
fn an_upload_times_itself_once() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let pages = x.a.atlas_pages();
    let _ = r.take_upload_timing();
    let token = r.install_pages(ScreenKind::Result, pages, 1).unwrap();
    let t = r.take_upload_timing().expect("the upload's timing");
    assert!(t.done.is_some(), "the queue reported the work done");
    assert!(r.take_upload_timing().is_none(), "taken once");
    r.commit(token).unwrap();
    let token = r.install_pages(ScreenKind::Result, pages, 2).unwrap();
    assert!(r.take_upload_timing().is_none(), "installed: no upload");
    r.commit(token).unwrap();
    r.remove(ScreenKind::Result);
    assert_eq!(r.retained_atlases(ScreenKind::Result), [None; 3]);
    let token = r.install_pages(ScreenKind::Result, pages, 3).unwrap();
    assert!(r.take_upload_timing().is_some(), "cold: uploaded again");
    r.commit(token).unwrap();
}

/// The pages-only installation: uploaded and charged with no layout
/// drawing; a layout on it then uploads nothing; the same pages again
/// upload nothing; other pages release the installation before they are
/// uploaded. Only the result kind has one, and a request not newer than
/// the renderer's is refused.
#[test]
fn the_pages_only_installation() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let pages = x.a.atlas_pages();
    let before = r.atlas_uploads();
    let token = r.install_pages(ScreenKind::Result, pages, 1).unwrap();
    assert_eq!(r.atlas_uploads(), before + 1);
    r.commit(token).unwrap();
    assert!(r.result_bundle().is_none());
    assert!(Arc::ptr_eq(
        r.installed_pages(ScreenKind::Result).unwrap(),
        pages
    ));
    assert_eq!(
        r.admission().charge(ScreenKind::Result),
        pages_texels(pages)
    );
    let mut a = state(&x.a);
    let layout = Layout::AUTHOR;
    assert!(
        r.build_result(&mut a, &at_zero(&layout), &[]).is_err(),
        "no layout yet"
    );
    let token = r.install(ScreenKind::Result, &x.a, 2).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.atlas_uploads(), before + 1);
    assert!(r.installed_pages(ScreenKind::Result).is_none());
    assert_eq!(frame(&mut r, &mut a), (R3, BLACK));
    // The same pages again: nothing uploaded, the layout kept.
    let token = r.install_pages(ScreenKind::Result, pages, 3).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.atlas_uploads(), before + 1);
    assert!(r.result_bundle().is_some());
    // Other pages: the installation released before the upload.
    let token = r
        .install_pages(ScreenKind::Result, x.c.atlas_pages(), 4)
        .unwrap();
    assert!(r.result_bundle().is_none(), "released before the upload");
    assert_eq!(r.atlas_uploads(), before + 2);
    r.commit(token).unwrap();
    assert!(Arc::ptr_eq(
        r.installed_pages(ScreenKind::Result).unwrap(),
        x.c.atlas_pages()
    ));
    assert!(matches!(
        r.install_pages(ScreenKind::Result, pages, 2),
        Err(RenderError::Superseded { .. })
    ));
    assert!(matches!(
        r.install(ScreenKind::Result, &x.a, 1),
        Err(RenderError::Superseded { .. })
    ));
    for kind in [ScreenKind::Play, ScreenKind::Selection] {
        assert!(matches!(
            r.install_pages(kind, pages, 10),
            Err(RenderError::PagesKind { .. })
        ));
    }
}

/// C9: play's candidate that only the result's charge keeps out is
/// admitted after the result is released, and the release is reported
/// once, also when the staging then fails; one that the others keep out
/// is refused with the result kept, and nothing is reported.
#[test]
fn play_releases_the_result_that_alone_refuses_it() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let play = solid_bundle(&r, &Look::left(GREEN), [16, 16]);
    let play_charge = play.atlas_texels();
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    let token = r.install(ScreenKind::Result, &x.a, 1).unwrap();
    r.commit(token).unwrap();
    assert!(!r.take_result_release());
    // The result's charge alone puts play one past the limit.
    r.state_charge(&x.a, SHARED_TEXELS - play_charge + 1);
    let token = r.stage(&play, &attempt(&r, &chart, &ctx), 2).unwrap();
    assert!(r.result_bundle().is_none(), "the result is released");
    assert_eq!(r.admission().charge(ScreenKind::Result), 0);
    assert!(r.take_result_release());
    assert!(!r.take_result_release(), "reported once");
    r.commit(token).unwrap();
    // Released, then the staging fails after its admission: still reported.
    let token = r.install(ScreenKind::Result, &x.c, 3).unwrap();
    r.commit(token).unwrap();
    r.state_charge(&x.c, SHARED_TEXELS - play_charge + 1);
    let other = solid_bundle(&r, &Look::left(BLUE), [16, 16]);
    r.fail_staged_buffers(true);
    let e = r.stage(&other, &attempt(&r, &chart, &ctx), 4).unwrap_err();
    r.fail_staged_buffers(false);
    assert!(e.is_refusal(), "{e:?}");
    assert!(r.result_bundle().is_none());
    assert!(r.take_result_release(), "a failed staging's release lost");
    // Selection fills the limit: play is refused whatever the result.
    r.state_charge(&x.c, x.c.atlas_texels());
    let token = r.install(ScreenKind::Result, &x.c, 5).unwrap();
    r.commit(token).unwrap();
    let sel = solid_bundle(&r, &Look::left(RED), [16, 16]);
    let token = r.install(ScreenKind::Selection, &sel, 6).unwrap();
    r.commit(token).unwrap();
    r.state_charge(&sel, SHARED_TEXELS);
    let e = r.stage(&other, &attempt(&r, &chart, &ctx), 7).unwrap_err();
    assert!(matches!(e, RenderError::Admission { .. }), "{e:?}");
    assert!(Arc::ptr_eq(r.result_bundle().unwrap(), &x.c), "kept");
    assert!(!r.take_result_release());
}

/// The result disabled for the run: its installation and staging go, and
/// every entry refuses it: a pages-only installation, a staging, a commit
/// and a restore of either; a refused result commit leaves another kind's
/// staging in place.
#[test]
fn a_disabled_result_is_refused_at_every_entry() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let token = r.install(ScreenKind::Result, &x.a, 1).unwrap();
    r.commit(token).unwrap();
    let installation = r.installation(ScreenKind::Result).unwrap();
    let staged = r.install(ScreenKind::Result, &x.c, 2).unwrap();
    r.disable_result();
    assert!(r.result_disabled());
    assert!(r.result_bundle().is_none());
    assert_eq!(r.admission().charge(ScreenKind::Result), 0);
    // Another kind staged, then the disabled token: refused, the other kept.
    let sel = solid_bundle(&r, &Look::left(RED), [16, 16]);
    let other = r.install(ScreenKind::Selection, &sel, 3).unwrap();
    assert_eq!(r.commit(staged), Err(CommitError::ResultDisabled));
    assert_eq!(r.staged(), Some(other));
    r.commit(other).unwrap();
    let disabled = |e: Option<RenderError>| matches!(e, Some(RenderError::ResultDisabled));
    assert!(disabled(r.install(ScreenKind::Result, &x.a, 4).err()));
    assert!(disabled(
        r.install_pages(ScreenKind::Result, x.a.atlas_pages(), 5)
            .err()
    ));
    assert!(disabled(r.restore(ScreenKind::Result, &installation).err()));
    assert!(disabled(
        r.restore_pages(ScreenKind::Result, x.a.atlas_pages()).err()
    ));
    assert!(r.result_bundle().is_none());
    assert!(r.installed_pages(ScreenKind::Result).is_none());
}

/// The bytes of `b`'s layout as the renderer sizes it for this target.
fn layout_bytes(b: &SkinBundle) -> u64 {
    let skin = b.skin();
    let mut st = Staging::with_runs(
        skin.quad_capacity().max(MAX_SPRITES as u32),
        skin.run_capacity().max(MAX_SPRITES as u32),
        &skin.canvas([W, H]),
    );
    st.set_clips(b.clips().clone());
    st.bytes()
}

/// The loading presentation's bytes of a fresh installation of `b`.
fn loading_bytes(b: &Arc<SkinBundle>) -> u64 {
    Installation::new(Arc::clone(b)).loading().bytes()
}

/// C21: the witness whole, every term, through a pages-only installation,
/// a layout staged and committed on it, the identical bundle staged again,
/// another layout on the same pages, and a bundle on other pages staged
/// and committed: shared pages counted
/// once wherever they are held, with `shared_bytes` keeping the array
/// whatever holder comes first, each bundle's own bytes in one term, a
/// committed layout counted, and what a commit replaced kept until the
/// next staging.
#[test]
fn the_witness_counts_every_result_term_once() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let base = r.memory();
    let pages = x.a.atlas_pages();
    let shared = pages_bytes(pages);
    assert_eq!(x.b.shared_bytes(), shared);
    assert!(
        x.b.resident_bytes() < shared,
        "a bundle's own bytes exclude its pages"
    );
    let token = r.install_pages(ScreenKind::Result, pages, 1).unwrap();
    r.commit(token).unwrap();
    let array = r.shared_bytes(x.a.atlas_id()) - shared;
    assert!(array > 0);
    let installed = MemoryWitness {
        shared_cpu: shared,
        result_atlas: array,
        ..base
    };
    assert_eq!(r.memory(), installed);
    // B staged on the same pages: its layout and own bytes, nothing else;
    // the array still counted by `shared_bytes`.
    let token = r.install(ScreenKind::Result, &x.b, 2).unwrap();
    assert_eq!(
        r.memory(),
        MemoryWitness {
            staged_cpu: x.b.resident_bytes(),
            staged_batch: layout_bytes(&x.b),
            staged_loading: loading_bytes(&x.b),
            ..installed
        }
    );
    assert_eq!(
        r.shared_bytes(x.a.atlas_id()),
        shared + array,
        "the array lost"
    );
    r.commit(token).unwrap();
    let committed = MemoryWitness {
        result_cpu: x.b.resident_bytes(),
        result_batch: layout_bytes(&x.b),
        result_loading: loading_bytes(&x.b),
        ..installed
    };
    assert_eq!(r.memory(), committed);
    // The identical bundle staged: its own bytes and its presentation
    // counted once, as the result's.
    let token = r.install(ScreenKind::Result, &x.b, 3).unwrap();
    assert_eq!(r.memory(), committed);
    r.commit(token).unwrap();
    // A on the same pages replacing B: B's layout is kept until the next
    // staging, its installation gone.
    let token = r.install(ScreenKind::Result, &x.a, 4).unwrap();
    r.commit(token).unwrap();
    let committed = MemoryWitness {
        result_cpu: x.a.resident_bytes(),
        result_batch: layout_bytes(&x.a),
        result_loading: loading_bytes(&x.a),
        ..installed
    };
    assert_eq!(
        r.memory(),
        MemoryWitness {
            retired: layout_bytes(&x.b),
            ..committed
        }
    );
    // C on its own pages: its array and pages too; committed, A's whole
    // installation is kept until the next staging.
    let token = r.install(ScreenKind::Result, &x.c, 5).unwrap();
    let m = r.memory();
    let c_array = r.shared_bytes(x.c.atlas_id()) - x.c.shared_bytes();
    assert!(m.upload > 0);
    assert_eq!(
        m,
        MemoryWitness {
            shared_cpu: shared + x.c.shared_bytes(),
            staged_cpu: x.c.resident_bytes(),
            staged_atlas: c_array,
            upload: m.upload,
            staged_batch: layout_bytes(&x.c),
            staged_loading: loading_bytes(&x.c),
            ..committed
        }
    );
    r.commit(token).unwrap();
    assert_eq!(
        r.memory(),
        MemoryWitness {
            shared_cpu: shared + x.c.shared_bytes(),
            result_cpu: x.c.resident_bytes(),
            result_batch: layout_bytes(&x.c),
            result_atlas: c_array,
            result_loading: loading_bytes(&x.c) + loading_bytes(&x.a),
            retired: array + layout_bytes(&x.a) + x.a.resident_bytes(),
            ..base
        }
    );
    let token = r.install(ScreenKind::Result, &x.c, 6).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.memory().retired, 0, "kept past the next staging");
    assert_eq!(r.memory().shared_cpu, x.c.shared_bytes());
    assert_eq!(r.memory().result_loading, loading_bytes(&x.c));
}

/// A text producer holding the default font's bytes and nothing else.
#[derive(Debug)]
struct WithFont(Arc<Text>);

impl AttemptText for WithFont {
    fn layout(
        &self,
        req: &skin_model::TextRequest,
        em_px: f32,
        bounds: render::LineBounds,
    ) -> render::LineFit {
        self.0.layout(req, em_px, bounds)
    }

    fn raster(&self, layout: &render::LineLayout, out: &mut [u8]) -> Result<(), render::SkinError> {
        self.0.raster(layout, out)
    }

    fn resident(&self) -> render::TextResidency {
        render::TextResidency {
            skin_fonts: 0,
            default_font: 4_096,
        }
    }
}

/// S10-09i Q3's handshake on the renderer's side: a commit on another
/// array keeps the array it replaced, reported as retained, its default
/// font still witnessed, until the owner drains it outside the commit
/// point; the drain returns the released identity once, then the renderer
/// retains only what draws, so another atlas build can be considered. A
/// same-array replacement drains no array.
#[test]
fn a_drain_releases_the_retired_array_outside_the_commit() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let dir = tempfile::tempdir().unwrap();
    let (a, atlas, _) = on_pages(
        &r,
        dir.path(),
        Draws {
            text: Some(500),
            ..A
        },
        &[],
    );
    let fonted = SkinBundle::new(
        Arc::new(a),
        atlas,
        ClipTable::new(Vec::new()).unwrap(),
        Side::Left,
    )
    .unwrap();
    let fonted = Arc::new(
        fonted
            .with_text(Arc::new(WithFont(producer())) as Arc<dyn AttemptText>)
            .unwrap(),
    );
    let token = r.install(ScreenKind::Result, &fonted, 1).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.memory().default_font, 4_096);
    // Replaced by a producer-free bundle on another array.
    let token = r.install(ScreenKind::Result, &x.c, 2).unwrap();
    r.commit(token).unwrap();
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.c.atlas_id()), None, Some(fonted.atlas_id())]
    );
    assert_eq!(r.memory().default_font, 4_096, "the retired font left out");
    // The owner drains it: released once, then only what draws is kept.
    assert_eq!(r.drain_retired(), Some(fonted.atlas_id()));
    assert_eq!(r.drain_retired(), None);
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.c.atlas_id()), None, None]
    );
    let m = r.memory();
    assert_eq!((m.retired, m.default_font), (0, 0));
    // A layout replaced on the same array releases no array.
    let token = r.install(ScreenKind::Result, &x.a, 3).unwrap();
    r.commit(token).unwrap();
    let token = r.install(ScreenKind::Result, &x.b, 4).unwrap();
    r.commit(token).unwrap();
    assert!(r.memory().retired > 0);
    assert_eq!(r.drain_retired(), None);
    assert_eq!(r.memory().retired, 0);
}

/// The result's lines are staged by their preparation and installed at the
/// commit point only on the array they were placed in; a refused frame or
/// text is refused once; the strip is bounded.
#[test]
fn staged_result_lines_install_on_their_array() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let token = r.install(ScreenKind::Result, &x.a, 1).unwrap();
    r.commit(token).unwrap();
    assert!(!r.install_staged_result_text(), "nothing staged");
    let mut a = state(&x.a);
    r.prepare_result_text(&mut a).unwrap();
    assert!(r.install_staged_result_text());
    assert!(!r.install_staged_result_text(), "installed once");
    // Staged for A's array, then C committed on its own array: dropped.
    r.prepare_result_text(&mut a).unwrap();
    let token = r.install(ScreenKind::Result, &x.c, 2).unwrap();
    r.commit(token).unwrap();
    assert!(!r.install_staged_result_text());
    // A pages-only installation drops what is staged too.
    let mut c = state(&x.c);
    r.prepare_result_text(&mut c).unwrap();
    let token = r
        .install_pages(ScreenKind::Result, x.a.atlas_pages(), 3)
        .unwrap();
    r.commit(token).unwrap();
    assert!(!r.install_staged_result_text());
    let token = r.install(ScreenKind::Result, &x.c, 4).unwrap();
    r.commit(token).unwrap();
    // A refused frame and a refused text, each once.
    r.refuse_next_result(true);
    assert!(r.prepare_result_text(&mut c).is_err());
    r.prepare_result_text(&mut c).unwrap();
    r.refuse_next_result(false);
    let layout = Layout::AUTHOR;
    let f = ResultFrame {
        timers: ResultTimers::default(),
        ..at_zero(&layout)
    };
    assert!(r.build_result(&mut c, &f, &[]).is_err());
    let strip = [strip_at(0.0); RESULT_STRIP_SPRITES + 2];
    r.build_result(&mut c, &f, &strip).unwrap();
    let drawn = r.submit_result(&[]).unwrap().sprites;
    // Its screen timer off, the skin draws nothing: the strip alone,
    // bounded.
    assert_eq!(drawn, RESULT_STRIP_SPRITES, "the strip bounded");
}

/// A result skin over `dir` drawing the stage file (S10-12) over A's band
/// (x 0, 400 wide), and a red cell at x `mark` 10 wide when `mark` is set,
/// so two such skins share pages.
fn chart_skin(dir: &Path, mark: Option<i32>) -> Skin {
    let mut b = skin_model::SheetBuilder::new(&Root::new(dir).unwrap());
    b.add_solid_image(SolidImageDef {
        size: [16, 16],
        color: RED,
        filter: Filter::Nearest,
        grid: None,
    })
    .unwrap();
    let mut elements = vec![element(
        Source::ChartImage {
            which: skin_model::chart_image::ChartImage::Stage,
            filter: Filter::Nearest,
            stretch: skin_model::chart_image::ChartStretch::Fill,
        },
        [0, 0, 400, 1_080],
        None,
    )];
    if let Some(x) = mark {
        elements.push(element(Source::Image(0), [x, 0, 10, 10], None));
    }
    let def = SkinDef {
        fit: Fit::Fixed,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: b.finish(),
        lanes: None,
        elements,
        widgets: WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: None,
    };
    Skin::new_result(def, ResultDefs::default()).unwrap()
}

/// A `w`×`h` picture of `rgb` fitted for the stage slot of a chart layer
/// of side `side`.
fn stage_picture(
    side: u32,
    [w, h]: [u32; 2],
    [r, g, b]: [u8; 3],
) -> skin_model::chart_image::Fitted {
    use skin_model::chart_image::{ChartDir, ChartImage, read};
    let d = tempfile::tempdir().unwrap();
    let file = File::create(d.path().join("p.png")).unwrap();
    let mut e = png::Encoder::new(BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    let dir = ChartDir::new(d.path()).unwrap();
    let slot = ChartImage::Stage.slot(side).unwrap().1;
    read(&dir, "p.png", &[], slot, &|| false, &|_| None)
        .image
        .unwrap()
}

/// The result's chart images and the array's record: a view's images are
/// written into the array its staging uses and keyed
/// to its installation, so its first frame after the commit draws them; a
/// layout staged on the installed array of the same set writes nothing
/// again and draws them with its own installation's key; the record is the
/// set's id, so a set whose every copy is gone is still skipped while a
/// new set writes; a refused write clears the record and leaves the view
/// to draw nothing; and a restore makes the tables of the old array
/// undrawable until the images are written again into the new one.
#[test]
fn result_chart_images_are_written_into_the_staged_array() {
    use render::ResultChartTarget;
    let mut r = offscreen(W, H);
    let dir = tempfile::tempdir().unwrap();
    let a_skin = chart_skin(dir.path(), None);
    let atlas = a_skin
        .atlas(Variant::Lo, r.atlas_limits(), &|| false)
        .unwrap();
    let b_skin = chart_skin(dir.path(), Some(1_800));
    let kept = b_skin.kept_layers();
    assert!(kept.chart);
    let rebound = atlas
        .rebind_keeping(b_skin.sheet(), Variant::Lo, kept)
        .unwrap();
    let a = bundle(a_skin, atlas, Vec::new(), None);
    let b = bundle(b_skin, rebound, Vec::new(), None);
    let side = a.side();
    let red = stage_picture(side, [64, 48], [200, 10, 10]);
    let green = stage_picture(side, [64, 48], [10, 200, 10]);
    const RED_PX: [u8; 3] = [200, 10, 10];
    const GREEN_PX: [u8; 3] = [10, 200, 10];

    // A on its own array: written, keyed to its staging's installation.
    let token = r.install(ScreenKind::Result, &a, 1).unwrap();
    let t1 = r
        .write_result_chart(
            ResultChartTarget::Staged(token),
            [Some(&red), None, None],
            1,
        )
        .unwrap();
    assert_eq!(r.result_chart_writes(), 1);
    r.commit(token).unwrap();
    assert_eq!(
        t1.key(),
        r.result_chart_key(),
        "keyed to the committed array"
    );
    let mut sa = state(&a);
    sa.set_chart_images(t1);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sa, &[]), W, A_AT), RED_PX);

    // B on A's pages, the same set: nothing written, its own key.
    let token = r.install(ScreenKind::Result, &b, 2).unwrap();
    let t2 = r
        .write_result_chart(
            ResultChartTarget::Staged(token),
            [Some(&red), None, None],
            1,
        )
        .unwrap();
    assert_eq!(r.result_chart_writes(), 1, "the installed array's set");
    assert_ne!(t2.key(), t1.key());
    r.commit(token).unwrap();
    assert_eq!(t2.key(), r.result_chart_key());
    let mut sb = state(&b);
    sb.set_chart_images(t2);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sb, &[]), W, A_AT), RED_PX);
    // A's table is of another installation: it draws nothing now.
    sb.set_chart_images(t1);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sb, &[]), W, A_AT), BLACK);

    // The record is an id: set 1 gone, still skipped; set 2 writes.
    drop(red);
    r.write_result_chart(ResultChartTarget::Committed, [None, None, None], 1)
        .unwrap();
    assert_eq!(r.result_chart_writes(), 1);
    let t3 = r
        .write_result_chart(ResultChartTarget::Committed, [Some(&green), None, None], 2)
        .unwrap();
    assert_eq!(r.result_chart_writes(), 2);
    sb.set_chart_images(t3);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sb, &[]), W, A_AT), GREEN_PX);

    // A refused write: absent, the record cleared, so set 2 writes again.
    r.refuse_next_result_chart();
    let refused = r
        .write_result_chart(ResultChartTarget::Committed, [Some(&green), None, None], 4)
        .unwrap_err();
    assert_eq!(refused.chart_outcome(), render::ChartOutcome::Absent);
    assert_eq!(r.result_chart_writes(), 2);
    r.write_result_chart(ResultChartTarget::Committed, [Some(&green), None, None], 2)
        .unwrap();
    assert_eq!(r.result_chart_writes(), 3, "the record was cleared");

    // A restore: a new array, the old tables undrawable until written.
    let installation = r.installation(ScreenKind::Result).unwrap();
    let before = r.result_chart_key();
    r.restore(ScreenKind::Result, &installation).unwrap();
    assert_ne!(r.result_chart_key(), before);
    sb.set_chart_images(t3);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sb, &[]), W, A_AT), BLACK);
    let t4 = r
        .write_result_chart(ResultChartTarget::Committed, [Some(&green), None, None], 2)
        .unwrap();
    sb.set_chart_images(t4);
    assert_eq!(rgb_at(&pixels(&mut r, &mut sb, &[]), W, A_AT), GREEN_PX);
}
