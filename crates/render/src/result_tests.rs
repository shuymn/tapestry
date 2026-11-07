//! The result kind's installations on WARP (S10-09i i-2; design.md
//! "Rendering and Skins" and "Memory Budget"): a layout staged on the
//! pages installed keeps their array and installs its own layout (C8),
//! never drawing a frame of two layouts (C2's renderer half); the
//! pages-only installation; play admitted by releasing the result (C9);
//! the result disabled for the run at every entry; the witness counting
//! shared pages once (C21); and the result's lines staged and installed.

use std::sync::Arc;

use bms_model::{ChartBuilder, TimeUs};
use render::{
    Blend, ClipTable, CommitError, RenderError, Renderer, SHARED_TEXELS, SkinBundle, Sprite,
    pages_bytes, pages_texels,
};
use skin_model::{
    Anchor, BestView, Ease, ElementDef, End, Filter, Fit, KeyDef, LampKey, Layout, Place, Region,
    RegionDef, ResultChart, ResultDefs, ResultFrame, ResultLabels, ResultScore, ResultScreen,
    ResultStatus, ResultTimers, ResultView, Root, ScreenKind, Side, Skin, SkinDef, SolidImageDef,
    Source, TimerRef, Track, Variant, WidgetDefs, result_timer,
};

use crate::test_common::{GAUGES, attempt, context, offscreen, read_back, rgb_at};
use crate::test_decl::{Look, solid_bundle};

const W: u32 = 1_920;
const H: u32 = 1_080;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// Where A draws red and B draws green: full-height bands.
const A_AT: (u32, u32) = (200, 540);
const B_AT: (u32, u32) = (1_200, 540);

/// A result skin over the sheet of a red and a green cell in `dir`,
/// drawing `elements` (image, left edge) as full-height bands 400 wide.
fn skin(dir: &std::path::Path, elements: &[(u32, i32)]) -> Skin {
    let mut b = skin_model::SheetBuilder::new(&Root::new(dir).unwrap());
    for color in [RED, GREEN] {
        b.add_solid_image(SolidImageDef {
            size: [16, 16],
            color,
            filter: Filter::Nearest,
            grid: None,
        })
        .unwrap();
    }
    let elements = elements
        .iter()
        .map(|&(image, x)| {
            let mut props = [None; 9];
            props[..4].copy_from_slice(&[Some(x), Some(0), Some(400), Some(1_080)]);
            ElementDef {
                source: Source::Image(image),
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
                clip: None,
                place: Place::SCREEN,
            }
        })
        .collect();
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
    };
    Skin::new_result(def, ResultDefs::default()).unwrap()
}

/// A: red at x 0 (and green past the bands read back, so its pages hold
/// both cells). B: green at x 1000, on A's pages, with a clip of its own;
/// C: red at x 0 on pages of its own.
struct Bundles {
    a: Arc<SkinBundle>,
    b: Arc<SkinBundle>,
    c: Arc<SkinBundle>,
}

fn bundles(r: &Renderer) -> Bundles {
    let dir = tempfile::tempdir().unwrap();
    let a = skin(dir.path(), &[(0, 0), (1, 1_500)]);
    let atlas = a.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    let b = skin(dir.path(), &[(1, 1_000)]);
    let rebound = atlas.rebind(b.sheet(), Variant::Lo, false).unwrap();
    let c = skin(dir.path(), &[(0, 0)]);
    let c_atlas = c.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    let clip = ClipTable::new(vec![[0.0, 0.0, 10.0, 10.0]]).unwrap();
    let bundle = |s: Skin, atlas, clips| {
        Arc::new(SkinBundle::new(Arc::new(s), atlas, clips, Side::Left).unwrap())
    };
    let none = || ClipTable::new(Vec::new()).unwrap();
    let out = Bundles {
        a: bundle(a, atlas, none()),
        b: bundle(b, rebound, clip),
        c: bundle(c, c_atlas, none()),
    };
    assert_eq!(out.a.atlas_id(), out.b.atlas_id());
    assert_ne!(out.a.atlas_id(), out.c.atlas_id());
    out
}

/// A ready view with nothing to show but the skin's images.
fn view() -> ResultView {
    ResultView {
        status: ResultStatus::Ready,
        chart: ResultChart::default(),
        score: ResultScore {
            clear: LampKey::Normal,
            ..ResultScore::default()
        },
        best: BestView::Unavailable,
        timing: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
        timelines: None,
    }
}

/// `bundle`'s result state with the view.
fn state(bundle: &SkinBundle) -> ResultScreen {
    let mut res = bundle.skin().prepare_result();
    bundle
        .skin()
        .update_result(&mut res, &view(), &ResultLabels::default())
        .unwrap();
    res
}

/// Builds, submits and reads back a result frame of `res`: the colours at
/// A's and B's bands.
fn frame(r: &mut Renderer, res: &mut ResultScreen) -> ([u8; 3], [u8; 3]) {
    let layout = Layout::AUTHOR;
    let f = ResultFrame {
        now: TimeUs(0),
        target: [W, H],
        layout: &layout,
        timers: ResultTimers {
            screen: Some(TimeUs(0)),
            fadeout: None,
        },
    };
    r.build_result(res, &f, &[]).unwrap();
    assert!(r.submit_result(&[]).unwrap().presented);
    let px = read_back(r);
    (rgb_at(&px, W, A_AT), rgb_at(&px, W, B_AT))
}

const BLACK: [u8; 3] = [0, 0, 0];
const R3: [u8; 3] = [255, 0, 0];
const G3: [u8; 3] = [0, 255, 0];

/// C8, C2: B staged on A's pages uploads nothing and draws nothing until
/// its commit, A drawing whole meanwhile; committed, B draws its own quads
/// with its own clips, A's layout gone, on the same array.
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
    assert_eq!(r.result_bundle().unwrap().clips().len(), 1);
    let mut b = state(&x.b);
    assert_eq!(frame(&mut r, &mut b), (BLACK, G3));
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.a.atlas_id()), None]
    );
    // A bundle on other pages uploads its own array.
    let token = r.install(ScreenKind::Result, &x.c, 3).unwrap();
    assert_eq!(r.atlas_uploads(), uploads + 1);
    assert_eq!(
        r.retained_atlases(ScreenKind::Result),
        [Some(x.a.atlas_id()), Some(x.c.atlas_id())]
    );
    r.commit(token).unwrap();
    let mut c = state(&x.c);
    assert_eq!(frame(&mut r, &mut c), (R3, BLACK));
}

/// The pages-only installation: uploaded and charged with no layout
/// drawing; a layout on it then uploads nothing; the same pages again
/// upload nothing; other pages release the installation before they are
/// uploaded. Only the result kind has one.
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
    let mut b = state(&x.b);
    let layout = Layout::AUTHOR;
    let f = ResultFrame {
        now: TimeUs(0),
        target: [W, H],
        layout: &layout,
        timers: ResultTimers::default(),
    };
    assert!(r.build_result(&mut b, &f, &[]).is_err(), "no layout yet");
    let token = r.install(ScreenKind::Result, &x.b, 2).unwrap();
    r.commit(token).unwrap();
    assert_eq!(r.atlas_uploads(), before + 1);
    assert!(r.installed_pages(ScreenKind::Result).is_none());
    assert_eq!(frame(&mut r, &mut b), (BLACK, G3));
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
    // A request not newer than the renderer's, as a sequence local to its
    // owner would give, is refused.
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
/// admitted after the result is released; one that the others keep out
/// is refused with the result kept.
#[test]
fn play_releases_the_result_that_alone_refuses_it() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let play = solid_bundle(&r, &Look::left(GREEN), [16, 16]);
    let play_charge = play.atlas_texels();
    let token = r.install(ScreenKind::Result, &x.a, 1).unwrap();
    r.commit(token).unwrap();
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    // The result's charge alone puts play one past the limit.
    r.state_charge(&x.a, SHARED_TEXELS - play_charge + 1);
    let token = r.stage(&play, &attempt(&r, &chart, &ctx), 2).unwrap();
    assert!(r.result_bundle().is_none(), "the result is released");
    assert_eq!(r.admission().charge(ScreenKind::Result), 0);
    r.commit(token).unwrap();
    // Selection fills the limit: play is refused whatever the result.
    let token = r.install(ScreenKind::Result, &x.c, 3).unwrap();
    r.commit(token).unwrap();
    let sel = solid_bundle(&r, &Look::left(RED), [16, 16]);
    let token = r.install(ScreenKind::Selection, &sel, 4).unwrap();
    r.commit(token).unwrap();
    r.state_charge(&sel, SHARED_TEXELS);
    let other = solid_bundle(&r, &Look::left(RED), [16, 16]);
    let e = r.stage(&other, &attempt(&r, &chart, &ctx), 5).unwrap_err();
    assert!(matches!(e, RenderError::Admission { .. }), "{e:?}");
    assert!(Arc::ptr_eq(r.result_bundle().unwrap(), &x.c), "kept");
}

/// The result disabled for the run: its installation and staging go, and
/// every entry refuses it: a pages-only installation, a staging, a commit
/// and a restore of either.
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
    assert_eq!(r.commit(staged), Err(CommitError::ResultDisabled));
    let disabled = |e: Option<RenderError>| matches!(e, Some(RenderError::ResultDisabled));
    assert!(disabled(r.install(ScreenKind::Result, &x.a, 3).err()));
    assert!(disabled(
        r.install_pages(ScreenKind::Result, x.a.atlas_pages(), 4)
            .err()
    ));
    assert!(disabled(r.restore(ScreenKind::Result, &installation).err()));
    assert!(disabled(
        r.restore_pages(ScreenKind::Result, x.a.atlas_pages()).err()
    ));
    assert!(r.result_bundle().is_none());
    assert!(r.installed_pages(ScreenKind::Result).is_none());
    // Another kind is not touched.
    let token = r.install(ScreenKind::Selection, &x.a, 5).unwrap();
    r.commit(token).unwrap();
}

/// C21: shared pages are counted once wherever they are held, a bundle's
/// own bytes apart from them, and pages of another build again.
#[test]
fn shared_pages_are_counted_once() {
    let mut r = offscreen(W, H);
    let x = bundles(&r);
    let pages = x.a.atlas_pages();
    let shared = pages_bytes(pages);
    assert_eq!(x.a.shared_bytes(), shared);
    assert_eq!(x.b.shared_bytes(), shared);
    assert!(
        x.b.resident_bytes() < shared,
        "a bundle's own bytes exclude its pages"
    );
    let token = r.install_pages(ScreenKind::Result, pages, 1).unwrap();
    r.commit(token).unwrap();
    let m = r.memory();
    assert_eq!(m.shared_cpu, shared);
    assert!(m.result_atlas > 0);
    let array = m.result_atlas;
    assert_eq!(r.shared_bytes(x.a.atlas_id()), shared + array);
    // B staged on the same pages: no second copy, no staged array.
    let token = r.install(ScreenKind::Result, &x.b, 2).unwrap();
    let m = r.memory();
    assert_eq!(m.shared_cpu, shared, "shared pages counted twice");
    assert_eq!((m.staged_atlas, m.upload), (0, 0));
    assert!(m.staged_batch > 0);
    assert_eq!(m.staged_cpu, x.b.resident_bytes());
    assert_eq!(m.result_atlas, array);
    r.commit(token).unwrap();
    let m = r.memory();
    assert_eq!(m.shared_cpu, shared);
    assert_eq!(m.result_cpu, x.b.resident_bytes());
    assert_eq!(m.result_atlas, array);
    // C on its own pages, staged beside them: counted again.
    let _token = r.install(ScreenKind::Result, &x.c, 3).unwrap();
    let m = r.memory();
    assert_eq!(m.shared_cpu, shared + x.c.shared_bytes());
    assert!(m.staged_atlas > 0);
    assert_eq!(
        r.shared_bytes(x.c.atlas_id()),
        x.c.shared_bytes() + m.staged_atlas
    );
    assert_eq!(r.shared_bytes(skin_model_unheld(&r)), 0);
}

/// An identity the renderer holds nothing of.
fn skin_model_unheld(r: &Renderer) -> skin_model::AtlasId {
    let dir = tempfile::tempdir().unwrap();
    let s = skin(dir.path(), &[(0, 0)]);
    let atlas = s.atlas(Variant::Lo, r.atlas_limits(), &|| false).unwrap();
    atlas.shared_pages().id()
}

/// The result's lines are staged by their preparation and installed at the
/// commit point only on the array they were placed in; a refused frame or
/// text is refused once.
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
    let mut c = state(&x.c);
    r.refuse_next_result(true);
    assert!(r.prepare_result_text(&mut c).is_err());
    r.prepare_result_text(&mut c).unwrap();
    r.refuse_next_result(false);
    let layout = Layout::AUTHOR;
    let f = ResultFrame {
        now: TimeUs(0),
        target: [W, H],
        layout: &layout,
        timers: ResultTimers::default(),
    };
    assert!(r.build_result(&mut c, &f, &[]).is_err());
    let strip = [Sprite {
        rect: [0.0, 0.0, 1_920.0, 42.0],
        uv: crate::solid_uv(),
        color: [0.0, 0.0, 0.0, 0.75],
        ..Sprite::default()
    }; render::RESULT_STRIP_SPRITES + 2];
    r.build_result(&mut c, &f, &strip).unwrap();
    let drawn = r.submit_result(&[]).unwrap().sprites;
    // Its screen timer off, the skin draws nothing: the strip alone,
    // bounded.
    assert_eq!(drawn, render::RESULT_STRIP_SPRITES, "the strip bounded");
}
