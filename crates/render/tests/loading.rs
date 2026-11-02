//! The loading presentation on WARP (design.md "Rendering and Skins",
//! loading state and declarative skin installation): a declarative skin's
//! loading frames drawn from its installation's presentation, animating on
//! the loading clock while the load stalls; the simple skin's loading frame
//! as before; a refused loading frame submitting nothing; the presentation
//! kept by the same bundle's staging and restoration, fresh for another
//! bundle and for a new loading lifetime; chart-independent installation
//! under the attempts' request numbers; a failed replacement leaving the
//! old installation's loading frames; resize and minimize; and the memory
//! a replacement holds.
//!
//! The skins are fixed 1920×1080 canvases on a 1920×1080 target, so canvas
//! pixels are target pixels ([`decl_support`]).

mod common;
mod decl_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, VISIBLE, attempt, context, input, offscreen, view};
use decl_support::{
    ANIMATION, ARTIST, BACKGROUND, FULL_TITLE, GENRE, Look, TITLE, WHEN_MARK, WHEN_SLOT, bundle,
};
use render::text::MenuText;
use render::{
    AttemptInput, CommitError, FrameInput, Gpu, Installation, LoadingFrame, LoadingTextError,
    PlaySkin, PreparedToken, RenderError, Renderer, SkinBundle, SkinError, Sprite, SpriteBatch,
};
use skin_model::{BuildError, TextField, TextImage, TextSlots, TextTable, TextTableError};

const BLACK: [u8; 3] = [0; 3];
const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const MS: i64 = 1_000;

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// `r`'s read back once its submitted frames finished.
#[track_caller]
fn read_back(r: &Renderer) -> Vec<u8> {
    let done = r.gpu().device().poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(10)),
    });
    assert!(done.is_ok(), "frames: {done:?}");
    r.read_back()
}

/// The RGB at the centre of canvas rect `rect` of a 1920-wide read back.
fn at(pixels: &[u8], rect: [i32; 4]) -> [u8; 3] {
    let [x, y, w, h] = rect.map(|v| u32::try_from(v).unwrap());
    common::rgb_at(pixels, 1_920, (x + w / 2, y + h / 2))
}

/// `rect` moved right by `dx` canvas pixels.
const fn right(rect: [i32; 4], dx: i32) -> [i32; 4] {
    [rect[0] + dx, rect[1], rect[2], rect[3]]
}

/// A loading frame of lifetime `lifetime` at `now` ms on the loading
/// clock, from origin 0.
fn frame_at(lifetime: u64, now: i64) -> LoadingFrame<'static> {
    LoadingFrame::interim(lifetime, TimeUs(now * MS), TimeUs(0), VISIBLE)
}

/// Draws loading frame `f` with `text` and reads it back.
#[track_caller]
fn loading(r: &mut Renderer, text: &MenuText, f: &LoadingFrame<'_>) -> Vec<u8> {
    let timing = r.loading_frame(f, text).unwrap().unwrap();
    assert!(timing.presented);
    read_back(r)
}

/// Red, with the loading elements.
fn loader() -> Look {
    Look {
        loading: true,
        ..Look::left([255, 0, 0, 255])
    }
}

/// Installs `b` chart-independently for `request` and commits it.
fn install(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64) {
    let token = r.install(b, request).unwrap();
    r.commit(token).unwrap();
}

/// Stages `b` with a fresh attempt for `request`.
fn stage(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    chart: &Chart,
    request: u64,
) -> Result<PreparedToken, RenderError> {
    let ctx = context(&GAUGES, false);
    let a = attempt(r, chart, &ctx);
    r.stage(b, &a, request)
}

/// While `now` advances and the load stands still, a declarative skin's
/// loading frames move its `loading`-timed animation (100 canvas pixels
/// per 100 ms) and draw its untimed background and its `when` mark, from
/// the chart-independent installation alone: no attempt is prepared, and
/// the element on the `ready` timer, which loading keeps off, draws
/// nothing. Built from a frozen state, the animation would stay put.
#[test]
fn a_loading_animation_advances_while_the_load_stalls() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    install(&mut r, &a, 1);
    let first = loading(&mut r, &text, &frame_at(1, 0));
    assert_eq!(at(&first, BACKGROUND), RED);
    assert_eq!(at(&first, ANIMATION), RED);
    assert_eq!(at(&first, WHEN_MARK), RED);
    assert_eq!(common::rgb_at(&first, 1_920, (480, 540)), BLACK);
    for k in 1..=3 {
        let p = loading(&mut r, &text, &frame_at(1, 200 * k));
        let dx = i32::try_from(200 * k).unwrap();
        assert_eq!(at(&p, ANIMATION), BLACK, "{k}: moved off its start");
        assert_eq!(at(&p, right(ANIMATION, dx)), RED, "{k}: moved {dx}");
        assert_eq!(at(&p, BACKGROUND), RED, "{k}");
    }
}

/// The simple skin's loading frame draws as it always has, whatever the
/// frame: the scratch lane, a key lane, the judgment line and the gauge's
/// frame in their colours, nothing right of the lanes.
#[test]
fn the_simple_skins_loading_frame_is_unchanged() {
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let p = loading(&mut r, &text, &frame_at(1, 0));
    let px = |x, y| common::rgb_at(&p, 1_920, (x, y));
    // SCRATCH_BG, LANE_BG, JUDGE_LINE and GAUGE_BG as unorm bytes.
    assert_eq!(px(208, 500), [18, 10, 10]);
    assert_eq!(px(288, 500), [10, 10, 13]);
    assert_eq!(px(400, 880), [255, 51, 51]);
    assert_eq!(px(400, 1_012), [31, 31, 31]);
    assert_eq!(px(1_500, 500), BLACK);
    assert_eq!(px(80, 500), BLACK);
}

/// A loading skin that pushes a green quad over the left half, then
/// refuses while `refuse` is set, counting its builds.
struct Refusing {
    refuse: Arc<std::sync::atomic::AtomicBool>,
    builds: Arc<AtomicU32>,
}

impl PlaySkin for Refusing {
    fn prepare(&mut self, _: &Gpu, _: &AttemptInput<'_>) -> Result<(), SkinError> {
        Ok(())
    }

    fn build(&mut self, _: &FrameInput<'_>, _: &mut SpriteBatch) -> Result<(), BuildError> {
        Ok(())
    }

    fn build_loading(
        &mut self,
        _: &LoadingFrame<'_>,
        out: &mut SpriteBatch,
    ) -> Result<(), BuildError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        out.push(
            render::Blend::Alpha,
            Sprite {
                rect: [0.0, 0.0, 960.0, 1_080.0],
                uv: render::solid_uv(),
                color: [0.0, 1.0, 0.0, 1.0],
                ..Sprite::default()
            },
        );
        if self.refuse.load(Ordering::SeqCst) {
            return Err(BuildError::InvalidReadouts);
        }
        Ok(())
    }
}

/// A loading frame refused after the skin pushed its quad submits nothing
/// (the target keeps the frame before it), and that lifetime's loading
/// frames are refused again without building; the renderer stays usable,
/// and a new lifetime builds and draws again.
#[test]
fn a_refused_loading_frame_submits_nothing() {
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let builds = Arc::new(AtomicU32::new(0));
    r.set_skin(Box::new(Refusing {
        refuse: Arc::clone(&refuse),
        builds: Arc::clone(&builds),
    }))
    .unwrap();
    let p = loading(&mut r, &text, &frame_at(1, 0));
    assert_eq!(common::rgb_at(&p, 1_920, (480, 540)), GREEN);
    // Black the target with a menu frame, then refuse.
    r.menu_frame(|_| {}, &text).unwrap();
    assert_eq!(common::rgb_at(&read_back(&r), 1_920, (480, 540)), BLACK);
    refuse.store(true, Ordering::SeqCst);
    let refused = r.loading_frame(&frame_at(1, 16), &text).unwrap();
    assert_eq!(refused, Err(BuildError::InvalidReadouts));
    assert_eq!(r.loading_refused(1), Some(BuildError::InvalidReadouts));
    assert_eq!(common::rgb_at(&read_back(&r), 1_920, (480, 540)), BLACK);
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    refuse.store(false, Ordering::SeqCst);
    let again = r.loading_frame(&frame_at(1, 32), &text).unwrap();
    assert_eq!(again, Err(BuildError::InvalidReadouts));
    assert_eq!(builds.load(Ordering::SeqCst), 2, "not built again");
    let p = loading(&mut r, &text, &frame_at(2, 48));
    assert_eq!(builds.load(Ordering::SeqCst), 3);
    assert_eq!(r.loading_refused(2), None);
    assert_eq!(common::rgb_at(&p, 1_920, (480, 540)), GREEN);
}

/// A declarative skin's loading refusal (covers its lanes disagree with)
/// stays with its presentation for the lifetime, the same bundle's
/// staging keeping it, and a new lifetime starts the presentation afresh
/// and draws; the attempt's state is untouched throughout.
#[test]
fn a_new_lifetime_draws_after_a_refused_one() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    let token = stage(&mut r, &a, &chart, 1).unwrap();
    r.commit(token).unwrap();
    let attempt = r.memory().attempt;
    let mut bad = frame_at(1, 0);
    bad.covers.cover_enabled = true;
    bad.covers.cover = 100;
    assert_eq!(
        r.loading_frame(&bad, &text).unwrap(),
        Err(BuildError::InvalidReadouts)
    );
    let presentation = r.installation().unwrap().loading().clone();
    assert_eq!(presentation.refused(), Some(BuildError::InvalidReadouts));
    let token = stage(&mut r, &a, &chart, 2).unwrap();
    r.commit(token).unwrap();
    assert_eq!(
        r.loading_frame(&frame_at(1, 16), &text).unwrap(),
        Err(BuildError::InvalidReadouts),
        "the same lifetime and installation stay refused"
    );
    let p = loading(&mut r, &text, &frame_at(2, 32));
    assert_eq!(presentation.refused(), None);
    assert_eq!(presentation.lifetime(), Some(2));
    assert_eq!(at(&p, BACKGROUND), RED);
    assert_eq!(r.memory().attempt, attempt);
    // The attempt draws as prepared.
    let v = view(0, 0);
    r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert_eq!(common::rgb_at(&read_back(&r), 1_920, (480, 540)), RED);
}

/// The `when` timer's start, once its condition held, is kept by the same
/// bundle staged for an attempt and by its installation restored on a
/// renderer built again; another bundle committed starts its own `when`
/// timer at its first frame, and a new lifetime on the same bundle starts
/// it afresh, never at the earlier lifetime's start.
#[test]
fn when_starts_follow_the_installation_and_the_lifetime() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    install(&mut r, &a, 1);
    let start = |r: &Renderer| r.installation().unwrap().loading().timer(WHEN_SLOT);
    loading(&mut r, &text, &frame_at(1, 100));
    assert_eq!(start(&r), Some(TimeUs(100 * MS)));
    let token = stage(&mut r, &a, &chart, 2).unwrap();
    r.commit(token).unwrap();
    loading(&mut r, &text, &frame_at(1, 200));
    assert_eq!(start(&r), Some(TimeUs(100 * MS)), "the same bundle staged");

    // Restored on another renderer: the very state.
    let kept = r.installation().unwrap();
    drop(r);
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    r.restore(&kept).unwrap();
    assert!(r.installation().unwrap().loading().same(kept.loading()));
    loading(&mut r, &text, &frame_at(1, 300));
    assert_eq!(start(&r), Some(TimeUs(100 * MS)), "restored");

    // Another bundle: its own, from its first frame.
    let (b, _) = bundle(
        &r,
        dir.path(),
        &Look {
            loading: true,
            ..Look::left([0, 255, 0, 255])
        },
    );
    install(&mut r, &b, 3);
    let p = loading(&mut r, &text, &frame_at(1, 400));
    assert_eq!(start(&r), Some(TimeUs(400 * MS)), "another bundle");
    assert_eq!(at(&p, BACKGROUND), GREEN);
    // A new lifetime on it: afresh.
    loading(&mut r, &text, &frame_at(2, 500));
    assert_eq!(start(&r), Some(TimeUs(500 * MS)), "a new lifetime");
}

/// A chart-independent installation and an attempt's staging share the
/// request numbers: the newer staging replaces the older, whose commit
/// is refused; once a request is committed, no older one stages or
/// commits; and a staging older than the one staged is refused, keeping
/// it.
#[test]
fn installations_and_attempts_keep_the_newer_request() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &loader());
    let (b, _) = bundle(&r, dir.path(), &Look::left([0, 255, 0, 255]));
    let selection = r.install(&a, 5).unwrap();
    let play = stage(&mut r, &a, &chart, 6).unwrap();
    assert_eq!(
        r.commit(selection),
        Err(CommitError::Request {
            staged: 6,
            asked: 5
        })
    );
    let stale = r.install(&b, 4).unwrap_err();
    assert!(
        matches!(
            stale,
            RenderError::Superseded {
                request: 4,
                newer: 6
            }
        ),
        "{stale:?}"
    );
    assert!(stale.is_refusal());
    assert_eq!(r.staged(), Some(play));
    r.commit(play).unwrap();
    assert!(Arc::ptr_eq(r.installed().unwrap(), &a));
    // A late selection of an older request installs nothing.
    assert!(matches!(
        r.install(&b, 5),
        Err(RenderError::Superseded {
            request: 5,
            newer: 6
        })
    ));
    assert_eq!(
        r.commit(selection),
        Err(CommitError::Superseded {
            committed: 6,
            asked: 5
        })
    );
    assert!(Arc::ptr_eq(r.installed().unwrap(), &a));
    let token = r.install(&b, 7).unwrap();
    r.commit(token).unwrap();
    assert!(Arc::ptr_eq(r.installed().unwrap(), &b));
    // A discarded staging and a refused one still bound what stages: a
    // late older request never stages after them.
    stage(&mut r, &a, &chart, 9).unwrap();
    r.discard();
    assert!(matches!(
        r.install(&a, 8),
        Err(RenderError::Superseded {
            request: 8,
            newer: 9
        })
    ));
    let (refusing, text) = bundle(
        &r,
        dir.path(),
        &Look {
            text: true,
            ..Look::left([0, 0, 255, 255])
        },
    );
    text.unwrap().refuse.store(true, Ordering::SeqCst);
    assert!(stage(&mut r, &refusing, &chart, 11).is_err());
    assert!(matches!(
        r.install(&a, 10),
        Err(RenderError::Superseded {
            request: 10,
            newer: 11
        })
    ));
    assert!(Arc::ptr_eq(r.installed().unwrap(), &b));
}

/// A replacement refused after its resources were made leaves the
/// installation in use whole: the next loading frame draws from it, its
/// presentation the same.
#[test]
fn a_failed_replacement_keeps_the_old_loading_frames() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    install(&mut r, &a, 1);
    loading(&mut r, &text, &frame_at(1, 0));
    let before = r.installation().unwrap();
    let look = Look {
        text: true,
        loading: true,
        ..Look::left([0, 255, 0, 255])
    };
    let (b, producer) = bundle(&r, dir.path(), &look);
    producer.unwrap().refuse.store(true, Ordering::SeqCst);
    assert!(stage(&mut r, &b, &chart, 2).unwrap_err().is_refusal());
    assert!(r.staged().is_none());
    let p = loading(&mut r, &text, &frame_at(1, 100));
    assert_eq!(at(&p, BACKGROUND), RED);
    assert_eq!(at(&p, right(ANIMATION, 100)), RED);
    let after = r.installation().unwrap();
    assert!(Arc::ptr_eq(after.bundle(), &a));
    assert!(after.loading().same(before.loading()));
}

/// Text installed for a loading lifetime stays with it: the next
/// lifetime entered starts without it, though the lifetime it was
/// installed for never drew a frame. The fixture's text element, on the
/// `screen` timer so loading frames draw it, draws its line, the
/// bundle's red image, as a 100 × 100 square at `(1000, 500)`.
#[test]
fn installed_text_never_passes_to_the_next_lifetime() {
    const TEXT: [i32; 4] = [1_000, 500, 100, 100];
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, producer) = bundle(
        &r,
        dir.path(),
        &Look {
            text: true,
            text_on_screen: true,
            ..loader()
        },
    );
    let uv = producer.unwrap().uv;
    let lines = || {
        let mut slots = vec![None; a.skin().text_slots()];
        *slots.first_mut().unwrap() = TextImage::new(uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        TextTable::new(slots)
    };
    install(&mut r, &a, 1);
    let key = r.loading_text_key().unwrap();
    assert_eq!(at(&loading(&mut r, &text, &frame_at(1, 0)), TEXT), BLACK);
    r.install_loading_text(key, TextSlots::ALL, &lines())
        .unwrap();
    assert_eq!(at(&loading(&mut r, &text, &frame_at(1, 100)), TEXT), RED);
    // Lifetime 2 gets its own text but never draws; lifetime 3 follows.
    r.enter_loading(2);
    r.install_loading_text(key, TextSlots::ALL, &lines())
        .unwrap();
    r.enter_loading(3);
    let p = loading(&mut r, &text, &frame_at(3, 200));
    assert_eq!(at(&p, TEXT), BLACK);
    assert_eq!(at(&p, BACKGROUND), RED);
}

/// Loading text installs only under the key of the installation in use
/// and the renderer and resources its lines were laid out on: the same
/// bundle's attempt keeps the key, a staging not yet committed changes
/// nothing, and another bundle's commit, a restoration on the same
/// renderer (its resources replaced) and one on another renderer each
/// make the key before stale, which then installs nothing. A table that
/// does not fit the slots is refused as such.
#[test]
fn loading_text_installs_only_for_its_installation() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let lines = |b: &SkinBundle| TextTable::new(vec![None; b.skin().text_slots()]);
    let (a, _) = bundle(
        &r,
        dir.path(),
        &Look {
            text: true,
            ..loader()
        },
    );
    let green = Look {
        text: true,
        ..Look::left([0, 255, 0, 255])
    };
    let (b, _) = bundle(&r, dir.path(), &green);
    assert_eq!(r.loading_text_key(), None);
    install(&mut r, &a, 1);
    let key = r.loading_text_key().unwrap();
    r.install_loading_text(key, TextSlots::ALL, &lines(&a))
        .unwrap();
    assert_eq!(
        r.install_loading_text(key, TextSlots::ALL, &TextTable::new(Vec::new())),
        Err(LoadingTextError::Table(TextTableError::Len {
            expected: a.skin().text_slots(),
            got: 0
        }))
    );
    let play = stage(&mut r, &a, &chart, 2).unwrap();
    r.commit(play).unwrap();
    assert_eq!(r.loading_text_key(), Some(key));
    let selection = r.install(&b, 3).unwrap();
    assert_eq!(r.loading_text_key(), Some(key));
    r.commit(selection).unwrap();
    assert_eq!(
        r.install_loading_text(key, TextSlots::ALL, &lines(&b)),
        Err(LoadingTextError::Stale)
    );
    let fresh = r.loading_text_key().unwrap();
    r.install_loading_text(fresh, TextSlots::ALL, &lines(&b))
        .unwrap();
    let installation = r.installation().unwrap();
    r.restore(&installation).unwrap();
    assert_eq!(
        r.install_loading_text(fresh, TextSlots::ALL, &lines(&b)),
        Err(LoadingTextError::Stale)
    );
    let restored = r.loading_text_key().unwrap();
    let mut other = offscreen(1_920, 1_080);
    other.restore(&installation).unwrap();
    assert_eq!(
        other.install_loading_text(restored, TextSlots::ALL, &lines(&b)),
        Err(LoadingTextError::Stale)
    );
    let elsewhere = other.loading_text_key().unwrap();
    other
        .install_loading_text(elsewhere, TextSlots::ALL, &lines(&b))
        .unwrap();
}

/// Loading frames follow the applied target: at 960×540 the background
/// draws at half scale; minimized, a frame is built but not presented;
/// back at full size, it draws there.
#[test]
fn loading_frames_follow_resize_and_minimize() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    install(&mut r, &a, 1);
    r.resize(960, 540);
    let timing = r.loading_frame(&frame_at(1, 0), &text).unwrap().unwrap();
    assert!(timing.presented);
    let p = read_back(&r);
    assert_eq!(common::rgb_at(&p, 960, (25, 475)), RED);
    assert_eq!(common::rgb_at(&p, 960, (25, 425)), BLACK);
    r.resize(0, 0);
    let timing = r.loading_frame(&frame_at(1, 16), &text).unwrap().unwrap();
    assert!(!timing.presented);
    assert!(timing.sprites > 0, "built while minimized");
    r.resize(1_920, 1_080);
    let p = loading(&mut r, &text, &frame_at(1, 32));
    assert_eq!(at(&p, BACKGROUND), RED);
}

/// The memory a replacement holds counts both installations' loading
/// presentations, the installed one being the very state the render
/// thread keeps for recovery; a staging of the installed bundle adds no
/// presentation, and after the commit the new one alone remains.
#[test]
fn a_replacement_counts_both_presentations() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, _) = bundle(&r, dir.path(), &loader());
    install(&mut r, &a, 1);
    loading(&mut r, &text, &frame_at(1, 0));
    let kept: Installation = r.installation().unwrap();
    let installed = r.memory();
    assert_eq!(installed.loading, kept.loading().bytes());
    assert!(installed.loading > 0);
    assert_eq!(installed.staged_loading, 0);

    let same = stage(&mut r, &a, &chart, 2).unwrap();
    assert_eq!(r.memory().staged_loading, 0);
    r.commit(same).unwrap();

    let (b, _) = bundle(&r, dir.path(), &loader());
    let token = r.install(&b, 3).unwrap();
    let peak = r.memory();
    assert_eq!(peak.loading, kept.loading().bytes());
    assert!(peak.staged_loading > 0);
    assert!(peak.total() >= installed.total() + peak.staged_loading);
    r.commit(token).unwrap();
    let after = r.memory();
    assert_eq!(after.loading, peak.staged_loading);
    assert_eq!(after.staged_loading, 0);
    // A presentation counts its runtime, itself included, once: what it
    // adds is the lifetime and its flag, far less than a second runtime.
    let fresh = render::LoadingPresentation::new(&b).bytes();
    let runtime = b.skin().prepare_loading(b.scratch()).resident_bytes();
    let size = u64::try_from(size_of::<skin_model::Loading>()).unwrap();
    assert!(
        fresh > runtime && fresh - runtime < size,
        "{fresh} {runtime}"
    );
}

/// The chosen chart's fields as a skin's text resolves them, an
/// unavailable one empty.
fn chosen<'a>(title: &'a str, artist: &'a str, genre: &'a str) -> skin_model::ChartText<'a> {
    skin_model::ChartText {
        title,
        subtitle: "",
        artist,
        subartist: "",
        genre,
    }
}

/// Red, with the loading elements and the chosen chart's text slots.
fn chart_texts(rgba: [u8; 4]) -> Look {
    Look {
        loading: true,
        chart_text: true,
        ..Look::left(rgba)
    }
}

/// The RGB of the title, artist, genre and full title slots in `p`.
fn texts(p: &[u8]) -> [[u8; 3]; 4] {
    [TITLE, ARTIST, GENRE, FULL_TITLE].map(|r| at(p, r))
}

/// The chosen chart's text prepared between frames waits staged, counted
/// in the memory witness, and installs at the next frame's start only on
/// the installation, lifetime and revision it was prepared for: until
/// then its slots draw nothing; only the fields available draw; text
/// prepared on bundle A is dropped once bundle B was committed in the
/// same lifetime with the same revision; text of another lifetime and an
/// older revision are dropped too. A cleared field's slots and the
/// composite using them draw nothing while the others stay.
#[test]
fn chosen_chart_text_installs_only_where_it_was_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let text = MenuText::new(&r);
    let (a, producer) = bundle(&r, dir.path(), &chart_texts([255, 0, 0, 255]));
    let (b, _) = bundle(&r, dir.path(), &chart_texts([0, 255, 0, 255]));
    install(&mut r, &a, 1);
    r.enter_loading(1);
    assert_eq!(r.memory().staged_text, 0);
    let all = TextSlots::field(TextField::Title)
        .with(TextSlots::field(TextField::Artist))
        .with(TextSlots::field(TextField::Genre));
    r.prepare_loading_text(1, 1, all, &chosen("B", "", ""))
        .unwrap();
    assert_eq!(r.staged_loading_text(), Some((1, 1)));
    assert!(r.memory().staged_text > 0);
    // Staged, nothing of it draws yet.
    assert_eq!(texts(&loading(&mut r, &text, &frame_at(1, 0))), [BLACK; 4]);
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    assert_eq!(r.memory().staged_text, 0);
    assert_eq!(
        texts(&loading(&mut r, &text, &frame_at(1, 16))),
        [RED, BLACK, BLACK, RED],
        "the title and the full title, nothing unavailable"
    );
    assert_eq!(
        producer.unwrap().charts.lock().unwrap().last().unwrap(),
        &["B".to_owned(), String::new(), String::new()]
    );

    // The title cleared: its slot and the full title's draw nothing.
    r.clear_loading_text(TextSlots::field(TextField::Title));
    assert_eq!(texts(&loading(&mut r, &text, &frame_at(1, 32))), [BLACK; 4]);
    r.prepare_loading_text(1, 2, all, &chosen("B", "b", ""))
        .unwrap();
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    assert_eq!(
        texts(&loading(&mut r, &text, &frame_at(1, 48))),
        [RED, RED, BLACK, RED]
    );
    r.clear_loading_text(TextSlots::field(TextField::Artist));
    assert_eq!(
        texts(&loading(&mut r, &text, &frame_at(1, 64))),
        [RED, BLACK, BLACK, RED],
        "the other slots stay"
    );

    // Prepared on A, then B committed within the lifetime: dropped.
    r.prepare_loading_text(1, 3, all, &chosen("B", "b", "g"))
        .unwrap();
    install(&mut r, &b, 2);
    r.enter_loading(1);
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Stale)
    );
    assert_eq!(r.staged_loading_text(), None);
    assert_eq!(texts(&loading(&mut r, &text, &frame_at(1, 80))), [BLACK; 4]);
    r.prepare_loading_text(1, 3, all, &chosen("B", "b", "g"))
        .unwrap();
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    assert_eq!(
        texts(&loading(&mut r, &text, &frame_at(1, 96))),
        [GREEN, GREEN, GREEN, GREEN]
    );
    // An older revision and another lifetime are dropped.
    r.prepare_loading_text(1, 2, all, &chosen("", "", ""))
        .unwrap();
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Older {
            installed: 3,
            revision: 2
        })
    );
    r.prepare_loading_text(2, 4, all, &chosen("", "", ""))
        .unwrap();
    assert_eq!(
        r.install_staged_loading_text(),
        Err(LoadingTextError::Lifetime)
    );
    assert_eq!(
        texts(&loading(&mut r, &text, &frame_at(1, 112))),
        [GREEN, GREEN, GREEN, GREEN]
    );
}

/// Text requested again and again keeps one table staged, the newest,
/// and the memory witness counts it: the same as one request's.
#[test]
fn staged_chart_text_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &chart_texts([255, 0, 0, 255]));
    install(&mut r, &a, 1);
    r.enter_loading(1);
    r.prepare_loading_text(1, 1, TextSlots::ALL, &chosen("A", "", ""))
        .unwrap();
    let one = r.memory();
    assert!(one.staged_text > 0);
    for revision in 2..20 {
        r.prepare_loading_text(1, revision, TextSlots::ALL, &chosen("A", "a", ""))
            .unwrap();
        assert_eq!(r.memory(), one, "revision {revision}");
    }
    assert_eq!(r.staged_loading_text(), Some((1, 19)));
    assert_eq!(r.install_staged_loading_text(), Ok(true));
    assert_eq!(r.memory().staged_text, 0);
    assert_eq!(r.install_staged_loading_text(), Ok(false));
}
