//! The cursor's chart images on a real selection screen, through the
//! render thread on WARP (dev-tools): a publication is admitted only for
//! the game thread's cursor generation and the live selection
//! installation, its table drawn while both stay current; another cursor
//! generation, a screen built again or no selection installation draws
//! nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use game::Preparation;
use skin_model::chart_image::ChartKey;
use skin_model::{Quad, ScreenKind, SelectionPlace};

use super::super::wait_until;
use super::update_of;
use super::warp::{listed, lossy_spawn};
use crate::chart_images::{Ack, ChartImages, PublishedTo};
use crate::render_side::{FrameSink, RenderLink, Screen};
use crate::selection_skin::{BANNER_AT, BANNER_DST, STAGE_AT, STAGE_DST, chart_mark};

const RED: [u8; 3] = [200, 10, 10];
const GREEN: [u8; 3] = [10, 200, 10];

/// One selection frame as drawn: its quads, its device, the update it
/// applied (lifetime, revision), and the pixels at the stage file's and
/// the banner's points when sampled.
#[derive(Clone, Debug)]
struct Seen {
    quads: Vec<Quad>,
    device: u64,
    update: Option<(u64, u64)>,
    pixels: Option<[[u8; 3]; 2]>,
}

/// The selection frames drawn, each read back while `sample` is set.
#[derive(Clone, Default)]
struct Frames {
    seen: Arc<Mutex<Vec<Seen>>>,
    sample: Arc<AtomicBool>,
}

impl FrameSink for Frames {
    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        let Some(quads) = screen.selection_quads() else {
            return;
        };
        let pixels = self
            .sample
            .load(Ordering::Acquire)
            .then(|| screen.read_back())
            .flatten()
            .map(|(w, _, px)| {
                [STAGE_AT, BANNER_AT].map(|[x, y]| {
                    let at = (y * usize::try_from(w).unwrap() + x) * 4;
                    [px[at], px[at + 1], px[at + 2]]
                })
            });
        self.seen.lock().unwrap().push(Seen {
            quads,
            device: screen.device(),
            update: drawn.selection,
            pixels,
        });
    }
}

impl Frames {
    /// Forgets the frames drawn so far.
    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Waits for `n` frames drawn on `device` (any, for `None`) since the
    /// last [`clear`](Self::clear), and returns them.
    fn next(&self, n: usize, device: Option<u64>) -> Vec<Seen> {
        self.next_of(n, |s| device.is_none_or(|d| s.device == d))
    }

    /// Waits for `n` frames drawn since the last [`clear`](Self::clear)
    /// for which `on` holds, and returns them.
    fn next_of(&self, n: usize, on: impl Fn(&Seen) -> bool) -> Vec<Seen> {
        assert!(
            wait_until(|| self.seen.lock().unwrap().iter().filter(|s| on(s)).count() >= n),
            "no {n} selection frames"
        );
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| on(s))
            .cloned()
            .collect()
    }

    /// Waits for a frame drawn since the last [`clear`](Self::clear) for
    /// which `f` holds, and returns it.
    fn until(&self, f: impl Fn(&Seen) -> bool) -> Seen {
        let found = Mutex::new(None);
        let held = wait_until(|| {
            let s = self.seen.lock().unwrap().iter().find(|s| f(s)).cloned();
            let some = s.is_some();
            *found.lock().unwrap() = s;
            some
        });
        assert!(
            held,
            "no such frame: {:?}",
            self.seen.lock().unwrap().last()
        );
        found.into_inner().unwrap().unwrap()
    }
}

/// Whether `q` holds a quad on `rect`, rounded, from `layer` if given.
fn has(q: &[Quad], rect: [f32; 4], layer: Option<u16>) -> bool {
    q.iter()
        .any(|q| q.rect.map(f32::round) == rect && layer.is_none_or(|l| q.layer == l))
}

/// Whether options 190 to 193 read `want` in every way the fixture reads
/// them: its `op`s and its callbacks.
fn reads(s: &Seen, want: [bool; 4]) -> bool {
    (0..3).all(|row| {
        want.iter()
            .enumerate()
            .all(|(i, w)| has(&s.quads, chart_mark(row, i), None) == *w)
    })
}

/// Whether a frame drew both images from `layer` with 191 and 193 holding
/// in its conditions and its callbacks, and 190 and 192 not.
fn draws_images(s: &Seen, layer: u16) -> bool {
    has(&s.quads, STAGE_DST, Some(layer))
        && has(&s.quads, BANNER_DST, Some(layer))
        && reads(s, [false, true, false, true])
}

/// Whether a frame drew neither image, with 190 and 192 holding and 191
/// and 193 not.
fn draws_none(s: &Seen) -> bool {
    !has(&s.quads, STAGE_DST, None)
        && !has(&s.quads, BANNER_DST, None)
        && reads(s, [true, false, true, false])
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

/// What [`rig`] gives: the fixture's folder, the link, its frames, the
/// screens' `lose` flag, and the skin's chart layer and side.
struct Rig {
    _dir: tempfile::TempDir,
    link: RenderLink,
    frames: Frames,
    lose: Arc<AtomicBool>,
    layer: u16,
    side: u32,
}

/// The chart fixture's selection skin, with its runtime, built for a WARP
/// target.
fn chart_skin() -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
    let dir = crate::selection_skin::chart_fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let (skin, script) =
        crate::selection_skin::fixture_scripted(limits, dir.path(), "chart.luaskin");
    (dir, skin, script.untimed())
}

/// A render thread on WARP with the chart fixture's selection skin
/// installed, its callbacks the runtime's, a selection shown, and a frame
/// of it drawn with no table.
fn rig() -> Rig {
    let (dir, skin, script) = chart_skin();
    let layer = skin.chart_layer().expect("the skin keeps a chart layer");
    let side = skin.side();
    let frames = Frames::default();
    let (link, lose, _) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.enter_selection(1);
    link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
    frames.until(draws_none);
    Rig {
        _dir: dir,
        link,
        frames,
        lose,
        layer,
        side,
    }
}

/// The answer to a selection publication of `set` for `cursor`.
fn publish(link: &RenderLink, cursor: u64, set: &Arc<ChartImages>) -> Ack {
    link.publish_selection_chart(cursor, Some(Arc::clone(set)))
        .recv_timeout(Duration::from_secs(30))
        .expect("answered")
}

/// The live selection key the render thread mirrored.
fn live(link: &RenderLink) -> Option<ChartKey> {
    link.chart_now(0).selection
}

/// A publication for the cursor generation the game thread set is
/// written into the selection installation's array and answered keyed to
/// its live key: the frames draw both images from the chart layer, their
/// texels sampled, and 191 and 193 hold in the conditions and the
/// callbacks. Another cursor generation drops the table at the next frame
/// (190 and 192 hold, nothing drawn), and a publication of an older one is
/// answered unavailable and draws nothing; one of the new one draws again
/// (SC2, SC3).
#[test]
fn a_selection_publication_draws_only_for_its_cursor_generation() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (_images, set) = crate::selection_skin::selection_images(r.side, RED, Some(GREEN));
    let cursor = r.link.cursor_generation();
    cursor.store(1, Ordering::SeqCst);
    let key = live(&r.link);
    assert!(key.is_some());
    r.frames.sample.store(true, Ordering::Release);
    r.frames.clear();
    let ack = publish(&r.link, 1, &set);
    assert_eq!(ack.key.generation, 1);
    assert_eq!(ack.key.request, None);
    assert_eq!(ack.key.target, PublishedTo::Selection(key));
    let table = ack.answer.expect("published");
    assert_eq!(table.key(), key);
    assert!(ack.key.current(&r.link.chart_now(0), None));
    // The publisher reads the same present, from any thread.
    assert_eq!(r.link.chart_publisher().now(0), r.link.chart_now(0));
    let drawn = r
        .frames
        .until(|s| draws_images(s, r.layer) && s.pixels.is_some());
    let [stage, banner] = drawn.pixels.unwrap();
    assert!(near(stage, RED), "{stage:?}");
    assert!(near(banner, GREEN), "{banner:?}");
    r.frames.sample.store(false, Ordering::Release);

    // Another cursor generation: the table is dropped before the next
    // frame, though nothing was published.
    cursor.store(2, Ordering::SeqCst);
    // The render thread takes the fence between frames: a frame drawn
    // before another cursor generation is pushed by then, so every frame after the clear
    // began after it.
    assert!(r.link.fence(Duration::from_secs(60)));
    r.frames.clear();
    assert!(r.frames.next(3, None).iter().all(draws_none));
    // A publication of the older generation publishes nothing.
    let ack = publish(&r.link, 1, &set);
    assert_eq!(ack.answer, Err(render::ChartOutcome::Unavailable));
    assert!(!ack.key.current(&r.link.chart_now(0), None));
    r.frames.clear();
    assert!(r.frames.next(3, None).iter().all(draws_none));
    // The current one draws again.
    let ack = publish(&r.link, 2, &set);
    assert!(ack.answer.is_ok(), "{ack:?}");
    r.frames.clear();
    r.frames.until(|s| draws_images(s, r.layer));
    r.link.exit();
}

/// With no selection installation there is no target: a publication is
/// answered `NoTarget` for no key, and nothing is recorded, so the skin
/// installed after it draws no image (SX18).
#[test]
fn a_selection_publication_without_an_installation_has_no_target() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let (_dir, skin, script) = chart_skin();
    let (_images, set) = crate::selection_skin::selection_images(skin.side(), RED, Some(GREEN));
    let frames = Frames::default();
    let (link, ..) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    link.enter_selection(1);
    link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
    assert_eq!(live(&link), None);
    let ack = publish(&link, 0, &set);
    assert_eq!(ack.key.target, PublishedTo::Selection(None));
    assert_eq!(ack.answer, Err(render::ChartOutcome::NoTarget));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.show_selection(update_of(1, 2, listed(SelectionPlace::All, "")));
    frames.until(draws_none);
    frames.clear();
    assert!(frames.next(3, None).iter().all(draws_none));
    link.exit();
}

/// A screen built again changes the selection installation's key: the
/// table drawn before is dropped, so the new screen's frames draw nothing
/// for it, and the same set published again (the same set id, into the
/// new array) draws again (SC6, ST10's render half).
#[test]
fn a_selection_table_is_dropped_by_a_screen_built_again() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (_images, set) = crate::selection_skin::selection_images(r.side, RED, Some(GREEN));
    let before = live(&r.link);
    let ack = publish(&r.link, 0, &set);
    assert!(ack.answer.is_ok(), "{ack:?}");
    r.frames.clear();
    let device = r.frames.until(|s| draws_images(s, r.layer)).device;
    // The device lost: no screen, no key, until the game thread's next
    // preparation builds one again.
    r.lose.store(true, Ordering::Release);
    assert!(wait_until(|| r.link.failed()));
    assert!(wait_until(|| live(&r.link).is_none()));
    r.link.prepare().unwrap();
    r.link
        .show_selection(update_of(1, 2, listed(SelectionPlace::All, "")));
    assert!(wait_until(
        || live(&r.link).is_some_and(|k| Some(k) != before)
    ));
    r.frames.clear();
    let rebuilt = r.frames.until(|s| s.device != device).device;
    assert!(r.frames.next(3, Some(rebuilt)).iter().all(draws_none));
    assert!(!ack.key.current(&r.link.chart_now(0), None));
    let again = publish(&r.link, 0, &set);
    assert_eq!(again.key.target, PublishedTo::Selection(live(&r.link)));
    assert!(again.answer.is_ok(), "{again:?}");
    r.frames.clear();
    r.frames
        .until(|s| s.device == rebuilt && draws_images(s, r.layer));
    r.link.exit();
}

/// A view update while the table is current: the frame that applies it
/// (its view's fields reset) still draws both images, with 191 and 193
/// holding in its conditions and its callbacks, since the image fields
/// are written after the view's and before the callbacks (ST9, through
/// the render thread's selection frame).
#[test]
fn the_frame_of_a_view_update_reads_the_current_table() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (_images, set) = crate::selection_skin::selection_images(r.side, RED, Some(GREEN));
    let ack = publish(&r.link, 0, &set);
    assert!(ack.answer.is_ok(), "{ack:?}");
    r.frames.clear();
    r.frames.until(|s| draws_images(s, r.layer));
    // Another view, the table still current.
    r.frames.clear();
    r.link
        .show_selection(update_of(1, 2, listed(SelectionPlace::All, "x")));
    let first = r.frames.until(|s| s.update == Some((1, 2)));
    assert!(draws_images(&first, r.layer), "{first:?}");
    r.link.exit();
}

/// The worker and the coordinator of a cursor on the select screen of
/// `link`.
fn cursor_worker(
    link: &RenderLink,
) -> (
    crate::chart_images::ChartImageWorker,
    game::coord::Coordinator,
) {
    let w = crate::chart_images::ChartImageWorker::new(
        link.chart_generation(),
        link.cursor_generation(),
    );
    let coord = game::coord::Coordinator::new(game::coord::Caps {
        foreground: 4,
        speculative: 1,
    });
    (w, coord)
}

/// The names [`crate::selection_skin::selection_images`] writes.
fn fixture_names() -> [library::Name; 2] {
    [
        library::Name::Named("s.png".to_owned()),
        library::Name::Named("n.png".to_owned()),
    ]
}

/// Steps `w` as the select screen does until `done`, at most 30 s.
fn step_until(
    w: &crate::chart_images::ChartImageWorker,
    coord: &game::coord::Coordinator,
    link: &RenderLink,
    done: impl Fn(&crate::chart_images::ChartImageWorker) -> bool,
) {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !done(w) {
        assert!(std::time::Instant::now() < end, "{w:?}");
        w.cursor_step(coord, game::coord::Spawner::SYSTEM, &link.chart_publisher());
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The selection set current, its job ended and its publication answered.
fn settled(w: &crate::chart_images::ChartImageWorker) -> bool {
    w.current_selection().is_some() && w.idle() && !w.selection_pending()
}

/// The cursor rested on a chart: the worker reads its stage file and
/// banner and publishes them, and the frames draw both, sampled, with 191
/// and 193 holding in the conditions and the callbacks; a move draws
/// nothing from the next frame, and a chart naming no image is published
/// absent, 190 and 192 holding (SC1-SC3).
#[test]
fn a_rested_cursors_images_are_drawn() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (images, _) = crate::selection_skin::selection_images(r.side, RED, Some(GREEN));
    let (w, coord) = cursor_worker(&r.link);
    r.frames.sample.store(true, Ordering::Release);
    w.rest(w.cursor_request(images.path().to_path_buf(), fixture_names(), r.side, &coord));
    step_until(&w, &coord, &r.link, settled);
    r.frames.clear();
    let drawn = r
        .frames
        .until(|s| draws_images(s, r.layer) && s.pixels.is_some());
    let [stage, banner] = drawn.pixels.unwrap();
    assert!(near(stage, RED), "{stage:?}");
    assert!(near(banner, GREEN), "{banner:?}");
    r.frames.sample.store(false, Ordering::Release);
    // A move: not current from the next frame.
    w.cursor_changed(&coord);
    // The render thread takes the fence between frames: a frame drawn
    // before the move is pushed by then, so every frame after the clear
    // began after it.
    assert!(r.link.fence(Duration::from_secs(60)));
    r.frames.clear();
    assert!(r.frames.next(3, None).iter().all(draws_none));
    // A chart naming no image.
    let none = tempfile::tempdir().unwrap();
    w.rest(w.cursor_request(
        none.path().to_path_buf(),
        [library::Name::None, library::Name::None],
        r.side,
        &coord,
    ));
    step_until(&w, &coord, &r.link, settled);
    assert_eq!(w.begun(), 1);
    r.frames.clear();
    assert!(r.frames.next(3, None).iter().all(draws_none));
    r.link.exit();
}

/// A screen built again with the cursor still: the worker publishes the
/// same set again on its next step, with no decode, no job and no move,
/// and the new screen draws it, whether the set's answer came before the
/// screen failed or was asked while no screen was there.
#[test]
fn a_rebuilt_screen_draws_the_cursors_images_again() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    for answered_before in [true, false] {
        let r = rig();
        let (images, _) = crate::selection_skin::selection_images(r.side, RED, Some(GREEN));
        let (w, coord) = cursor_worker(&r.link);
        let rest = || {
            w.rest(w.cursor_request(images.path().to_path_buf(), fixture_names(), r.side, &coord));
        };
        let device = r.frames.until(|_| true).device;
        if answered_before {
            rest();
            step_until(&w, &coord, &r.link, settled);
            r.frames.clear();
            r.frames.until(|s| draws_images(s, r.layer));
        }
        r.lose.store(true, Ordering::Release);
        assert!(wait_until(|| r.link.failed()));
        assert!(wait_until(|| live(&r.link).is_none()));
        if !answered_before {
            // Read and published while no screen is there.
            rest();
            step_until(&w, &coord, &r.link, settled);
        }
        let (begun, decodes) = (w.begun(), w.counts().0);
        assert_eq!((begun, decodes), (1, 2));
        r.link.prepare().unwrap();
        r.link
            .show_selection(update_of(1, 2, listed(SelectionPlace::All, "")));
        assert!(wait_until(|| live(&r.link).is_some()));
        r.frames.clear();
        let rebuilt = r.frames.until(|s| s.device != device).device;
        step_until(&w, &coord, &r.link, |w| {
            settled(w) && w.selection_publications() >= 2
        });
        r.frames.clear();
        r.frames
            .until(|s| s.device == rebuilt && draws_images(s, r.layer));
        assert_eq!(
            (w.begun(), w.counts().0),
            (begun, decodes),
            "answered before {answered_before}"
        );
        r.link.exit();
    }
}

/// A selection skin refused at runtime leaves no installation: the worker
/// publishes nothing again and begins no job (SX18).
#[test]
fn a_refused_selection_skin_is_not_published_again() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let (_dir, skin, script) = chart_skin();
    let side = skin.side();
    let layer = skin.chart_layer().unwrap();
    let frames = Frames::default();
    let bad = Arc::new(AtomicBool::new(false));
    let (link, ..) = lossy_spawn(None, &bad, Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.enter_selection(1);
    link.show_selection(update_of(1, 1, listed(SelectionPlace::All, "")));
    frames.until(draws_none);
    let (images, _) = crate::selection_skin::selection_images(side, RED, Some(GREEN));
    let (w, coord) = cursor_worker(&link);
    w.rest(w.cursor_request(images.path().to_path_buf(), fixture_names(), side, &coord));
    step_until(&w, &coord, &link, settled);
    frames.clear();
    frames.until(|s| draws_images(s, layer));
    // Refused at runtime: its installation removed.
    bad.store(true, Ordering::Release);
    assert!(wait_until(|| live(&link).is_none()));
    let (sent, begun) = (w.selection_publications(), w.begun());
    for _ in 0..50 {
        w.cursor_step(
            &coord,
            game::coord::Spawner::SYSTEM,
            &link.chart_publisher(),
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(w.selection_publications(), sent, "published again");
    assert_eq!(w.begun(), begun);
    link.exit();
}

/// A chart refused before selection's departure (a practice the session
/// refuses) enters selection again with no departure: the entry is a new
/// cursor generation whatever row the cursor is on, so with the cursor
/// moved meanwhile to the all-charts row, the new visit's frames draw
/// nothing of the last chart's images, and 190 and 192 hold, 191 and 193
/// not (SX17, through the player's selection on the render thread).
#[test]
fn an_entry_with_no_departure_draws_nothing_of_the_last_visit() {
    use game::AppRig as _;

    use crate::select::MenuKey;
    use crate::selection::image_tests::{library, look_until, past_rest, press};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let (_skin, skin, script) = chart_skin();
    let (layer, side) = (skin.chart_layer().unwrap(), skin.side());
    let frames = Frames::default();
    let (link, ..) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    crate::tests::on_rig_with(settings, link, |rig, _| {
        rig.adjust = None;
        rig.selection = Some(sel);
        rig.selection_chart = Some(side);
        rig.entered_selection();
        frames.until(draws_none);
        // The cursor rests on the chart: its images are drawn.
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        past_rest();
        look_until(rig, |w| {
            w.current_selection().is_some() && w.idle() && !w.selection_pending()
        });
        frames.clear();
        let last = frames.until(|s| draws_images(s, layer));
        let (last, _) = last.update.expect("a selection frame's update");
        // Refused before the departure: the chart left and selection
        // entered again, the cursor on the all-charts row meanwhile.
        assert!(rig.selection.as_mut().unwrap().screen.move_to(0));
        rig.left_chart().unwrap();
        rig.entered_selection();
        for _ in 0..3 {
            assert!(rig.choose().is_none());
        }
        // The new visit's frames: one the render thread drew before it
        // took the entry is the last visit's, however late it lands.
        let seen = frames.next_of(3, |s| s.update.is_some_and(|(lifetime, _)| lifetime > last));
        let drew = seen.iter().find(|s| !draws_none(s));
        assert!(drew.is_none(), "{drew:?}");
    });
}
