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

/// One selection frame as drawn: its quads, its device, and the pixels
/// at the stage file's and the banner's points when sampled.
#[derive(Clone, Debug)]
struct Seen {
    quads: Vec<Quad>,
    device: u64,
    pixels: Option<[[u8; 3]; 2]>,
}

/// The selection frames drawn, each read back while `sample` is set.
#[derive(Clone, Default)]
struct Frames {
    seen: Arc<Mutex<Vec<Seen>>>,
    sample: Arc<AtomicBool>,
}

impl FrameSink for Frames {
    fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
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
        let on = |s: &Seen| device.is_none_or(|d| s.device == d);
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
    (dir, skin, script)
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
