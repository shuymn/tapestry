//! The result's hold through the player's flow on WARP (design.md
//! "Rendering and Skins", the result's hold): begun at the result's
//! preparation, the outgoing screen drawn until its bound and the pending
//! presentation after it, released by a withdrawal, kept by a screen built
//! again, and ended by the first committed view. Each frame drawn while
//! armed is recorded with what its result stamp names and how many of its
//! texels are lit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::Rig as _;
use input::QpcClock;
use skin_model::ScreenKind;

use super::result_flow::{RED, chart, fixed_skin, prepared};
use super::{LetGo, imports, result, settings};
use crate::measure::ResultPath;
use crate::menu_screen::MenuScreen;
use crate::play::PlayerRig;
use crate::render_side::tests::{bundle, measure, view};
use crate::render_side::{FrameSink, MakeScreen, RenderLink, RenderMsg, Screen};
use crate::result_skin::{ResultSkin, ResultStart};
use bms_model::TimeUs;

/// The hold's bound in these tests, in µs.
const BOUND: i64 = 200_000;
/// A frame of the wait page lights more texels than this; a cleared
/// canvas none.
const LIT: usize = 100;
/// The held pass's longest wait for a step in flight, in µs.
const HELD_PACE_US: u64 = 4_000;
/// The longest frame interval the offscreen pacer allows here: its wait
/// for the frame before (100 ms, after which that frame is skipped) and
/// the frame's own work on WARP in a debug build, beside other WARP tests
/// (up to about 80 ms). A step's writes waited for in the frame (one
/// step of 2048² cells takes about 700 ms on WARP) exceed it.
const PACER_BOUND: Duration = Duration::from_millis(250);

/// Every screen built while set is refused, as a device that cannot be
/// built again (each test runs in a process of its own).
static REFUSE_SCREEN: AtomicBool = AtomicBool::new(false);

/// One frame drawn while armed.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// What its result stamp names; `None` for a frame of another screen.
    path: Option<ResultPath>,
    /// Its texels with a channel above 200; `usize::MAX` when it was not
    /// read back.
    lit: usize,
    /// The instant a result frame evaluated its skin at.
    sampled: Option<TimeUs>,
    /// When the sink saw it.
    at: Instant,
    /// Its size read back; zero sides when it was not.
    size: (u32, u32),
}

impl Frame {
    /// Whether it is the result's pending presentation: the wait page's
    /// line drawn.
    fn pending(self) -> bool {
        self.path == Some(ResultPath::Pending) && self.lit > LIT
    }
}

/// The frames recorded while armed.
#[derive(Clone, Default)]
struct Frames {
    armed: Arc<AtomicBool>,
    /// No frame is read back while set: its texels count as lit.
    blind: Arc<AtomicBool>,
    /// The result's hold's outgoing phases ended, as the record marks them.
    holds: Arc<Mutex<Vec<crate::measure::HoldStamp>>>,
    log: Arc<Mutex<Vec<Frame>>>,
    /// The meeting point: while `[0]` is set, the next frame's sink holds
    /// the render thread, `[1]` saying it is held there.
    meet: Arc<[AtomicBool; 2]>,
}

impl Frames {
    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// Reads no frame back from now on (`true`): frames come at the
    /// render thread's own pace.
    fn blind(&self, on: bool) {
        self.blind.store(on, Ordering::Release);
    }

    /// Holds the render thread in the sink of the next frame it draws,
    /// after that frame is recorded: once this returns, its next loop turn
    /// has not begun.
    fn meet(&self) {
        let [hold, held] = &*self.meet;
        held.store(false, Ordering::Release);
        hold.store(true, Ordering::Release);
        let end = Instant::now() + Duration::from_secs(20);
        while !held.load(Ordering::Acquire) {
            assert!(Instant::now() < end, "no frame reached the meeting point");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Lets the render thread held at the meeting point go on.
    fn part(&self) {
        let [hold, _] = &*self.meet;
        hold.store(false, Ordering::Release);
    }

    fn len(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// The result's hold's outgoing phases ended so far.
    fn holds(&self) -> Vec<crate::measure::HoldStamp> {
        self.holds.lock().unwrap().clone()
    }

    fn since(&self, from: usize) -> Vec<Frame> {
        self.log.lock().unwrap().get(from..).unwrap_or(&[]).to_vec()
    }

    /// Waits until the frames from `from` on satisfy `want`, at most 20 s,
    /// landing the result's layouts meanwhile: those frames.
    fn until(
        &self,
        rig: &mut PlayerRig<'_>,
        from: usize,
        what: &str,
        want: impl Fn(&[Frame]) -> bool,
    ) -> Vec<Frame> {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let f = self.since(from);
            if want(&f) {
                return f;
            }
            assert!(Instant::now() < end, "never {what}: {f:?}");
            rig.land_result();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Recorder(Frames);

impl FrameSink for Recorder {
    fn result_hold(&mut self, hold: crate::measure::HoldStamp) {
        self.0.holds.lock().unwrap().push(hold);
    }

    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        if !self.0.armed.load(Ordering::Acquire) {
            return;
        }
        let blind = self.0.blind.load(Ordering::Acquire);
        let mut size = (0, 0);
        let lit = if blind {
            usize::MAX
        } else {
            screen.read_back().map_or(0, |(w, h, pixels)| {
                size = (w, h);
                pixels
                    .chunks(4)
                    .filter(|p| p.iter().take(3).any(|&c| c > 200))
                    .count()
            })
        };
        self.0.log.lock().unwrap().push(Frame {
            path: drawn.result.map(|r| r.path),
            lit,
            sampled: drawn.result.map(|r| r.sampled),
            at: Instant::now(),
            size,
        });
        let [hold, held] = &*self.0.meet;
        if hold.load(Ordering::Acquire) {
            held.store(true, Ordering::Release);
            let end = Instant::now() + Duration::from_secs(20);
            while hold.load(Ordering::Acquire) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// `f` on a player's rig over a render thread on a WARP menu screen,
/// `chart` loaded, its frames recorded into the [`Frames`] given, the
/// hold's bound [`BOUND`] and its clock stopped at the hold's start.
fn on_warp(chart: &bms_model::Chart, f: impl FnOnce(&mut PlayerRig<'_>, &Frames)) {
    on_warp_fonts(chart, true, f);
}

/// [`on_warp`] on a screen that draws menus (`fonts`), its font read as it
/// is built, or on one made for plays only, with none.
fn on_warp_fonts(
    chart: &bms_model::Chart,
    fonts: bool,
    f: impl FnOnce(&mut PlayerRig<'_>, &Frames),
) {
    let frames = Frames::default();
    let make: MakeScreen = Box::new(move || {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        if REFUSE_SCREEN.load(Ordering::SeqCst) {
            return Err(render::RenderError::SurfaceLost);
        }
        let fonts = if fonts {
            Some(render::text::Fonts::runtime()?)
        } else {
            None
        };
        Ok(Box::new(MenuScreen::new(r, fonts)) as Box<dyn Screen>)
    });
    let clock = QpcClock::start();
    let render: RenderLink = crate::render_side::spawn(
        make,
        None,
        None,
        clock,
        view(),
        measure(clock),
        Box::new(Recorder(frames.clone())),
    )
    .unwrap();
    render.load(Arc::new(chart.clone()), None).unwrap();
    assert_eq!(
        render
            .select(ScreenKind::Play, Some(bundle()))
            .wait()
            .unwrap(),
        game::Preparation::Ready
    );
    render.set_first_frame_hold(TimeUs(BOUND));
    render.set_hold_clock(Some(0));
    let out = crate::tests::on_rig_with(settings(), render, |rig, _| {
        let _let_go = LetGo;
        rig.play_chart = None;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(rig, &frames)))
    })
    .0;
    out.unwrap_or_else(|p| std::panic::resume_unwind(p));
}

/// The result of `dir`'s chart: its report and screen.
fn result_of(dir: &std::path::Path) -> (game::PlayReport, game::ResultScreen) {
    let report = crate::end_screen::tests::report(rules::tapestry());
    let screen = crate::end_screen::tests::screen(dir, &report);
    (report, screen)
}

/// A fixed result skin's attempt prepared and committed, a cleared page
/// drawn as the outgoing screen, then the result's preparation begun: the
/// hold. The frames' count at the begin, and the skin's folder.
fn held(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
) -> (usize, tempfile::TempDir) {
    let (skin, fixed) = fixed_skin();
    rig.result_skin = ResultSkin::new(Some(ResultStart::Fixed(Arc::clone(&fixed), None)));
    prepared(rig, a);
    assert!(rig.result_chart.is_some(), "the attempt's result chart");
    rig.render.send(RenderMsg::Menu(Box::default()));
    frames.arm();
    let at = frames.len();
    rig.begin_result(report, screen);
    (at, skin)
}

/// Before the hand-over, the outgoing screen draws until the hold's
/// bound and the pending presentation after it, with no origin and no
/// timing; the hand-over's first committed view ends it, and no pending
/// frame comes after that view (V19 (a); V13's first result case).
#[test]
fn a_result_held_past_its_bound_draws_the_pending_presentation() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        // Before the bound: the outgoing screen, a cleared canvas.
        let before = frames.until(rig, from, "five outgoing frames", |f| f.len() >= 5);
        assert!(
            before.iter().all(|f| f.path.is_none() && f.lit == 0),
            "only the outgoing screen before the bound: {before:?}"
        );
        // Past it: the pending presentation on every frame.
        rig.render.set_hold_clock(Some(BOUND + 1));
        let past = frames.len();
        let after = frames.until(rig, past, "five pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 5
        });
        let first = after.iter().position(|f| f.pending()).unwrap();
        assert!(
            after[first..].iter().all(|f| f.pending()),
            "every frame past the bound pending: {after:?}"
        );
        let holds = frames.holds();
        assert!(
            matches!(holds.as_slice(), [h] if h.ended_by == Some(crate::measure::HoldEnd::Bound) && h.to >= h.from),
            "the record's mark: the outgoing phase ended by the bound: {holds:?}"
        );
        assert_eq!(
            rig.render.presented_origin(),
            None,
            "no origin while pending"
        );
        assert_eq!(
            rig.render.presented_timing(),
            None,
            "no timing while pending"
        );
        // The hand-over: the first committed view, no pending frame after.
        let shown_from = frames.len();
        rig.show_end(&report, &screen);
        let shown = frames.until(rig, shown_from, "the skin's view", |f| {
            f.iter()
                .filter(|f| f.path == Some(ResultPath::Skin))
                .count()
                >= 3
        });
        let skin = shown
            .iter()
            .position(|f| f.path == Some(ResultPath::Skin))
            .unwrap();
        assert!(
            shown[..skin].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        assert!(
            shown[skin..]
                .iter()
                .all(|f| f.path == Some(ResultPath::Skin)),
            "no pending frame after the first view: {shown:?}"
        );
        rig.leave_result();
    });
}

/// The hold's bound passing during a frame's pacing, after the pass
/// looked at it: that frame is already the pending presentation, so no
/// outgoing frame comes after the bound, the frame before the first
/// pending one included (V19 (a); V13's first result case).
#[test]
fn a_bound_passing_during_pacing_draws_no_outgoing_frame_after_it() {
    use std::sync::atomic::AtomicUsize;
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        let mark = Arc::new(AtomicUsize::new(usize::MAX));
        let (seen, at) = (frames.clone(), Arc::clone(&mark));
        rig.render.after_next_pacing(
            BOUND + 1,
            Box::new(move || at.store(seen.len(), Ordering::SeqCst)),
        );
        until_true(rig, "the hook", |_| {
            mark.load(Ordering::SeqCst) != usize::MAX
        });
        let at = mark.load(Ordering::SeqCst);
        let before = frames.since(from);
        let before = before.get(..at - from).unwrap_or(&[]);
        assert!(
            before.iter().all(|f| f.path.is_none()),
            "only the outgoing screen before the bound: {before:?}"
        );
        let after = frames.until(rig, at, "five frames after the bound", |f| f.len() >= 5);
        assert!(
            after.iter().all(|f| f.pending()),
            "an outgoing frame after the bound: {after:?}"
        );
    });
}

/// A preparation withdrawn by a retry releases the hold: once the
/// retry's screen preparation is answered, no pending frame draws (V19
/// (f) before the hand-over).
#[test]
fn a_withdrawn_result_draws_no_pending_frame_after_the_next_message() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.until(rig, from, "a pending frame", |f| {
            f.iter().any(|f| f.pending())
        });
        rig.withdraw_result(Some(game::MenuCommand::Retry));
        rig.prepare_screen().unwrap();
        let after = frames.len();
        // The time a retry's preparation takes before its next message:
        // frames a hold still current would draw.
        std::thread::sleep(Duration::from_millis(200));
        rig.render.send(RenderMsg::Menu(Box::new(
            crate::render_side::tests::one_line("select"),
        )));
        let next = frames.until(rig, after, "the next screen's frames", |f| {
            f.iter().filter(|f| f.lit > LIT && f.path.is_none()).count() >= 3
        });
        assert!(
            next.iter().all(|f| f.path != Some(ResultPath::Pending)),
            "a pending frame after the retry's preparation: {next:?}"
        );
    });
}

/// A screen built again in ResultPreparing, before the bound, keeps the
/// hold and ends its outgoing phase: the rebuilt screen's first frame is
/// the pending presentation (V19 (g), its first half).
#[test]
fn a_screen_built_again_while_held_draws_the_pending_presentation() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        let before = frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        assert!(before.iter().all(|f| f.path.is_none()), "{before:?}");
        rig.prepare_screen().unwrap();
        let rebuilt = frames.len();
        let after = frames.until(rig, rebuilt, "frames on the rebuilt screen", |f| {
            f.len() >= 3
        });
        assert!(
            after.iter().all(|f| f.pending()),
            "the pending presentation from the rebuilt screen's first frame: {after:?}"
        );
    });
}

/// The device lost in ResultPreparing, past the bound (a pending frame
/// failed as a lost device), and the screen built again as the flow does
/// between polls: the rebuilt screen, a real one, draws the pending
/// presentation from its first frame, and the hand-over's first view
/// follows it (V19 (g), its first half, with a real loss).
#[test]
fn a_device_lost_while_held_draws_the_pending_presentation_rebuilt() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.until(rig, from, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        crate::dev::lose_result_frame_at(1);
        until_true(rig, "the loss", |rig| rig.screen_failed());
        crate::dev::lose_result_frame_at(0);
        let lost = frames.len();
        rig.prepare_screen().unwrap();
        let rebuilt = frames.until(rig, lost, "frames on the rebuilt screen", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        assert!(
            rebuilt.iter().all(|f| f.pending()),
            "the pending presentation from the rebuilt screen's first frame: {rebuilt:?}"
        );
        let handed = frames.len();
        rig.show_end(&report, &screen);
        let shown = frames.until(rig, handed, "the skin's view", |f| first_view(f).is_some());
        let at = first_view(&shown).unwrap();
        assert!(
            shown[..at].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        rig.leave_result();
    });
}

/// The device lost in ResultPreparing and the screen refused when built
/// again, which ends the run with the screen's error (the run's record
/// was saved before; the game's flow tests pin that): the hold ends at the
/// next screen's message, so a screen built after it draws no pending
/// presentation, only that screen's page.
#[test]
fn a_refused_rebuild_while_held_leaves_no_hold_past_the_next_message() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.until(rig, from, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        REFUSE_SCREEN.store(true, Ordering::SeqCst);
        crate::dev::lose_result_frame_at(1);
        until_true(rig, "the loss", |rig| rig.screen_failed());
        crate::dev::lose_result_frame_at(0);
        assert!(rig.prepare_screen().is_err(), "the screen built again");
        REFUSE_SCREEN.store(false, Ordering::SeqCst);
        let after = frames.len();
        rig.render.send(RenderMsg::Menu(Box::new(
            crate::render_side::tests::one_line("select"),
        )));
        // A screen built again later, then the time a hold still current
        // would draw on it before the screen's page comes again.
        rig.prepare_screen().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        rig.render.send(RenderMsg::Menu(Box::new(
            crate::render_side::tests::one_line("select"),
        )));
        let next = frames.until(rig, after, "the next screen's frames", |f| {
            f.iter().filter(|f| f.lit > LIT && f.path.is_none()).count() >= 3
        });
        assert!(
            next.iter().all(|f| f.path.is_none()),
            "a pending frame after the next screen's message: {next:?}"
        );
    });
}

/// The blocking GPU waits counted so far, by the kind of their pass.
#[derive(Debug, Default)]
struct Waits {
    /// In held passes (presenting nothing).
    held: Vec<render::probe::GpuWait>,
    /// In passes that present a frame.
    presenting: Vec<render::probe::GpuWait>,
    /// In the pacing between two frames (the wait for the frame's
    /// deadline, looking at the step in flight meanwhile).
    between: Vec<render::probe::GpuWait>,
}

/// How late past its bound a counted wait may return. The bound asked is
/// checked exactly; what it took is not clipped to it, and on WARP beside
/// the other WARP tests of a run a 4 ms poll was seen to return up to 46 ms
/// late (the CPU renderer and the scheduler under load). A wait that does
/// not honour its bound (one asked for 1 s) exceeds this.
const LATENESS_US: u64 = 100_000;

fn gpu_waits() -> Waits {
    let all = render::probe::gpu_waits();
    let of = |k| all.iter().filter(|w| w.pass == k).copied().collect();
    Waits {
        held: of(render::probe::PASS_HELD),
        presenting: of(render::probe::PASS_PRESENTING),
        between: of(render::probe::PASS_BETWEEN),
    }
}

/// Each of `waits` asked with a bound of at most `HELD_PACE` and returned
/// within that bound and the scheduler's lateness.
fn bounded(waits: &[render::probe::GpuWait]) -> bool {
    waits.iter().all(|w| {
        w.bound_us
            .is_some_and(|b| b <= HELD_PACE_US && w.elapsed_us <= b + LATENESS_US)
    })
}

/// A hit's first view whose steps are delayed within the bound: held
/// passes only from the hand-over, each waiting at most `HELD_PACE`,
/// then the first view with no pending frame; no blocking GPU wait in a
/// presenting pass, and the held passes' bounded wait counted (V19 (b);
/// V9b (a)).
#[test]
fn a_first_view_staged_within_the_bound_holds_without_a_pending_frame() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.delay_steps(3 * HELD_PACE_US);
        render::probe::count_gpu_waits(Some(false));
        let passes = rig.render.held_passes();
        let handed = frames.len();
        rig.show_end(&report, &screen);
        let shown = frames.until(rig, handed, "the skin's view", |f| {
            f.iter().any(|f| f.path == Some(ResultPath::Skin))
        });
        let waits = gpu_waits();
        render::probe::count_gpu_waits(None);
        rig.render.delay_steps(0);
        let skin = shown
            .iter()
            .position(|f| f.path == Some(ResultPath::Skin))
            .unwrap();
        assert!(
            shown[..skin].iter().all(|f| f.path.is_none()),
            "no pending frame before the first view: {shown:?}"
        );
        assert!(
            rig.render.held_passes() > passes,
            "held passes from the hand-over"
        );
        assert!(
            !waits.held.is_empty(),
            "the held passes' bounded wait counted"
        );
        let holds = frames.holds();
        assert!(
            matches!(holds.as_slice(), [h] if h.ended_by == Some(crate::measure::HoldEnd::Skin)),
            "the record's mark: the outgoing phase ended by the first view: {holds:?}"
        );
        assert!(
            bounded(&waits.held) && bounded(&waits.between),
            "each held or between-frame wait at most HELD_PACE: {waits:?}"
        );
        assert_eq!(waits.presenting, Vec::new(), "a blocking wait presenting");
        rig.leave_result();
    });
}

/// A pending result skin whose view's cell needs a build (154's residue
/// at the attempt differs from the view's), the attempt prepared and
/// committed, a cleared page drawn as the outgoing screen, then the
/// result's preparation begun: a miss whose layout the result builds. The
/// frames' count at the begin, and the skin's folder.
fn held_miss(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
) -> (usize, tempfile::TempDir) {
    held_miss_of(rig, frames, a, report, screen, 8, false)
}

/// [`held_miss`] with cells of `side` texels a side, cut from a source
/// image of three; with `gated`, the result's layout build held at the
/// layout gate from the result's preparation on.
fn held_miss_of(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
    side: u32,
    gated: bool,
) -> (usize, tempfile::TempDir) {
    held_miss_in(rig, frames, a, report, screen, side, gated, false)
}

/// [`held_miss`] whose skin also has a line of text in a TrueType font,
/// so its first view's staging writes result text in the uploader's
/// steps.
fn held_miss_with_text(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
) -> (usize, tempfile::TempDir) {
    held_miss_in(rig, frames, a, report, screen, 8, false, true)
}

/// [`held_miss_of`], with a line of text in a TrueType font when `text`.
#[expect(clippy::too_many_arguments, reason = "a test fixture's options")]
fn held_miss_in(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
    side: u32,
    gated: bool,
    text: bool,
) -> (usize, tempfile::TempDir) {
    let d = result::skin(
        if text {
            "skin.font = { { id = \"f\", path = \"f.ttf\" } }
             skin.text = { { id = \"t\", font = \"f\", size = 60, constantText = \"Result\" } }"
        } else {
            ""
        },
        &format!(
            "{{ {{ id = \"dot\", src = \"s\", x = {side} * (nr % 3), y = 0, w = {side}, h = {side} }} }}"
        ),
    );
    if text {
        let entry = std::fs::read_to_string(d.path().join("s.luaskin"))
            .unwrap()
            .replace(
                "skin.destination = d
",
                "d[#d + 1] = { id = \"t\", dst = { { x = 0, y = 600, w = 400, h = 60 } } }
                 skin.destination = d
",
            );
        std::fs::write(d.path().join("s.luaskin"), entry).unwrap();
        std::fs::write(
            d.path().join("f.ttf"),
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )
        .unwrap();
    }
    if side > 8 {
        let file = std::fs::File::create(d.path().join("s.png")).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), 3 * side, side);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let texels = [90, 90, 90, 255].repeat(usize::try_from(3 * side * side).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
    }
    let p = result::pending(d.path(), result::LIMITS);
    rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(p)));
    prepared(rig, a);
    rig.render.send(RenderMsg::Menu(Box::default()));
    frames.arm();
    let at = frames.len();
    if gated {
        result::hold_builds(true);
    }
    rig.begin_result(report, screen);
    rig.result_compared(report, screen);
    (at, d)
}

/// The result's layout landed, as the flow waits for it in
/// ResultPreparing before the hand-over; at most 20 s.
fn laid_out(rig: &mut PlayerRig<'_>) {
    let end = Instant::now() + Duration::from_secs(20);
    while !rig.land_result() {
        assert!(Instant::now() < end, "the result's layout never landed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The skin's view drawn: the frames until then, the first view's index.
fn first_view(frames: &[Frame]) -> Option<usize> {
    frames.iter().position(|f| f.path == Some(ResultPath::Skin))
}

/// A miss's first view, with a line of text, whose staging steps past
/// the bound after the hand-over, its writes held by the test's flag, then
/// let go with each step's completion delayed; the counter's scope the
/// uploader's and stagings' waits, or with `pacing` the frame's own pacing
/// too. Asserts V19 (c)'s frames and that the staging wrote its text in
/// the uploader's steps; the waits counted from the bound to the first
/// view's commit.
fn staged_past_the_bound(pacing: bool) -> Waits {
    let (dir, a) = chart(Some(RED));
    let mut waits = None;
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss_with_text(rig, frames, &a, &report, &screen);
        laid_out(rig);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.pause_result_staging(true);
        let acked = rig.render.result_acknowledged();
        let uploads = crate::dev::result_uploads();
        let handed = frames.len();
        rig.show_end(&report, &screen);
        // Held passes until the bound: nothing presents.
        let passes = rig.render.held_passes();
        let end = Instant::now() + Duration::from_secs(20);
        while rig.render.held_passes() < passes + 10 {
            assert!(Instant::now() < end, "no held passes after the hand-over");
            rig.land_result();
            std::thread::sleep(Duration::from_millis(5));
        }
        // The frames the render thread drew before it took the hand-over
        // are the outgoing screen's; none after it.
        let held_at = frames.len();
        let before = frames.since(handed);
        assert!(
            before.iter().all(|f| f.path.is_none()),
            "only the outgoing screen until held: {before:?}"
        );
        let passes = rig.render.held_passes();
        std::thread::sleep(Duration::from_millis(100));
        assert!(rig.render.held_passes() > passes, "the held passes stopped");
        assert_eq!(frames.len(), held_at, "a frame presented while held");
        // Past the bound: the pending presentation while the flag holds.
        render::probe::count_gpu_waits(Some(pacing));
        rig.render.set_hold_clock(Some(BOUND + 1));
        let pending = frames.until(rig, held_at, "ten pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 10
        });
        let after_bound = rig.render.held_passes();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.render.held_passes(),
            after_bound,
            "a held pass after the bound"
        );
        assert!(
            pending.iter().all(|f| f.pending()),
            "every frame after the hand-over pending: {pending:?}"
        );
        assert_eq!(
            rig.render.presented_origin(),
            None,
            "an origin while pending"
        );
        assert_eq!(
            rig.render.presented_timing(),
            None,
            "a timing while pending"
        );
        assert_eq!(
            rig.render.result_acknowledged(),
            acked,
            "the entry's first picture acknowledged while pending"
        );
        // The flag cleared: the steps complete, each seen late, and the
        // view commits.
        rig.render.delay_steps(3 * HELD_PACE_US);
        let unpaused = Instant::now();
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, held_at, "the skin's view", |f| first_view(f).is_some());
        waits = Some(gpu_waits());
        let records = rig.render.step_records();
        assert!(
            records.iter().any(|r| !r.atlas && r.at >= unpaused),
            "no text step written by the uploader: {records:?}"
        );
        render::probe::count_gpu_waits(None);
        rig.render.delay_steps(0);
        let at = first_view(&shown).unwrap();
        assert!(
            shown[..at].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        assert!(
            crate::dev::result_uploads() > uploads,
            "the first view's staging uploaded no array: not a miss"
        );
        assert_eq!(
            rig.render.presented_origin(),
            shown[at].sampled,
            "the origin at the first view's frame"
        );
        assert!(
            rig.render.result_acknowledged() > acked,
            "the entry's first picture acknowledged at the first view"
        );
        rig.leave_result();
    });
    waits.unwrap()
}

/// A first view whose staging steps past the bound after the hand-over
/// (its writes held by the test's flag): held passes until the bound and
/// none after it, then the pending presentation on every frame while the
/// flag stays set, with no built-in page, no origin, no timing and no
/// acknowledgement of the entry's first picture; the flag cleared, the
/// view commits, its frame the origin and the acknowledgement's (V19 (c);
/// V13's second result case). Past the bound no pass waits for the GPU
/// (V9b (b)).
#[test]
fn a_first_view_staging_past_the_bound_draws_the_pending_presentation() {
    let _serial = imports::serial();
    let waits = staged_past_the_bound(false);
    assert_eq!(waits.held, Vec::new(), "a held wait after the bound");
    assert_eq!(waits.presenting, Vec::new(), "a blocking wait presenting");
    assert!(
        !waits.between.is_empty() && bounded(&waits.between),
        "the waits between frames, each at most HELD_PACE: {waits:?}"
    );
}

/// The control of V9b's counter: widened to the frame's own pacing, the
/// same run counts waits in presenting passes, so the zero above is the
/// counter's scope, not frames that never paced.
#[test]
fn the_blocking_wait_counter_widened_counts_the_frames_pacing() {
    let _serial = imports::serial();
    let waits = staged_past_the_bound(true);
    assert!(
        !waits.presenting.is_empty(),
        "no pacing wait counted presenting"
    );
}

/// Waits until `done`, at most 20 s, landing the result's layouts.
fn until_true(rig: &mut PlayerRig<'_>, what: &str, done: impl Fn(&PlayerRig<'_>) -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !done(rig) {
        assert!(Instant::now() < end, "never {what}");
        rig.land_result();
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A hand-over just before the bound whose union's creation ends past
/// it: no frame presents from the hand-over to the creation's end, the
/// first frame after it is the pending presentation, no held pass runs
/// after the bound, and the boundary whose creation ended past the bound
/// begins no write (V19 (d)).
#[test]
fn a_creation_ending_past_the_bound_draws_the_pending_presentation_next() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss_of(rig, frames, &a, &report, &screen, 8, true);
        until_true(rig, "the layout at its build", |_| result::build_reached());
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.set_hold_clock(Some(BOUND - 1_000));
        result::hold_builds(false);
        laid_out(rig);
        rig.render.delay_result_creation(50_000, BOUND + 1);
        let handed = frames.len();
        rig.show_end(&report, &screen);
        until_true(rig, "the view's creation", |rig| {
            rig.render.creation_pass() > 0
        });
        let created = rig.render.creation_pass();
        let passes = rig.render.held_passes();
        let shown = frames.until(rig, handed, "the skin's view", |f| first_view(f).is_some());
        assert_eq!(
            rig.render.held_passes(),
            passes,
            "a held pass after the bound"
        );
        let first = shown.iter().position(|f| f.path.is_some()).unwrap();
        assert!(
            shown[..first].iter().all(|f| f.path.is_none()),
            "only the outgoing screen before the creation's end: {shown:?}"
        );
        assert!(
            shown[first].pending(),
            "the pending presentation first after the creation: {shown:?}"
        );
        let records = rig.render.step_records();
        assert!(
            records.iter().all(|r| r.pass != created),
            "a write in the boundary whose creation ended past the bound: {records:?} at {created}"
        );
        assert!(
            records.iter().all(|r| !r.held),
            "a held step after the bound: {records:?}"
        );
        rig.leave_result();
    });
}

/// A step begun just before the bound, its cells 2048² so the union's
/// first atlas step spans at least four writes, each write moving the
/// hold's clock 400 µs: the held pass closes and submits the step after
/// its third write, no held pass begins after it, the next frame is the
/// pending presentation, and the remaining writes run between pending
/// frames (V19 (d2)).
#[test]
fn a_step_crossing_the_bound_closes_at_it() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss_of(rig, frames, &a, &report, &screen, 2_048, false);
        laid_out(rig);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        until_true(rig, "the view's creation", |rig| {
            rig.render.creation_pass() > 0
        });
        let handed = frames.len();
        let passes = rig.render.held_passes();
        until_true(rig, "held passes", |rig| {
            rig.render.held_passes() > passes + 3
        });
        let paused_at = rig.render.creation_pass();
        assert!(
            rig.render
                .step_records()
                .iter()
                .all(|r| !r.atlas || r.pass <= paused_at),
            "a step written while paused"
        );
        // 1 ms before the bound as the first write ends: the boundary looks
        // at the bound once before its first write, and the hook moves the
        // clock at each look after that one.
        rig.render.set_hold_clock(Some(BOUND - 1_400));
        rig.render.set_write_clock_step(400);
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, handed, "the skin's view", |f| first_view(f).is_some());
        rig.render.set_write_clock_step(0);
        let records: Vec<_> = rig
            .render
            .step_records()
            .into_iter()
            .filter(|r| r.atlas && r.pass > paused_at)
            .collect();
        let first = records.first().copied().expect("a step written");
        assert!(first.held, "the first step in a held pass: {records:?}");
        assert_eq!(
            first.writes,
            Some(3),
            "the held step closed after its third write: {records:?}"
        );
        assert!(
            records[1..].iter().all(|r| !r.held),
            "a held step after the bound: {records:?}"
        );
        let pending = shown
            .iter()
            .position(|f| f.path.is_some())
            .expect("a result frame");
        assert!(
            shown[pending].pending(),
            "the next frame the pending presentation: {shown:?}"
        );
        let first_pending = shown[pending].at;
        assert!(
            records[1..].iter().any(|r| r.at > first_pending),
            "no write after the first pending frame: {records:?}"
        );
        rig.leave_result();
    });
}

/// The staging after the bound, each step's completion delayed: pending
/// frames between its steps and every frame interval within the
/// offscreen pacer's bound (V19 (e), its frames; on WARP a step's writes
/// take longer than a frame, so two steps between two frames are not
/// shown here).
#[test]
fn a_staging_after_the_bound_steps_between_pending_frames() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss_of(rig, frames, &a, &report, &screen, 2_048, false);
        laid_out(rig);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        until_true(rig, "the view's creation", |rig| {
            rig.render.creation_pass() > 0
        });
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.blind(true);
        let past = frames.len();
        frames.until(rig, past, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        rig.render.delay_steps(2_000);
        // From the last frame before the staging steps: a stall on its
        // first write lengthens the interval after that frame.
        let stepping = frames.len() - 1;
        let unpaused = Instant::now();
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, stepping, "the skin's view", |f| {
            first_view(f).is_some()
        });
        assert!(
            shown.first().is_some_and(|f| f.at < unpaused),
            "the frame before the steps left out: {shown:?}"
        );
        rig.render.delay_steps(0);
        frames.blind(false);
        let records: Vec<_> = rig
            .render
            .step_records()
            .into_iter()
            .filter(|r| r.at >= unpaused)
            .collect();
        let at = first_view(&shown).unwrap();
        assert!(
            shown[..at].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        assert!(records.len() >= 2, "steps: {records:?}");
        let intervals: Vec<_> = shown.windows(2).map(|w| w[1].at - w[0].at).collect();
        assert!(
            intervals.iter().all(|d| *d <= PACER_BOUND),
            "a frame interval past the pacer's bound: {intervals:?}"
        );
        // Pending frames between the staging's steps: one after its first
        // and before its last.
        let (first, last) = (records[0].at, records[records.len() - 1].at);
        assert!(
            shown[..at].iter().any(|f| f.at > first && f.at < last),
            "no pending frame between the steps: {records:?}"
        );
        rig.leave_result();
    });
}

/// Waits until the frames from `from` on satisfy `want`, at most 20 s,
/// landing the result's layouts and building a failed screen again as
/// the flow does: those frames.
fn until_rebuilt(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    from: usize,
    what: &str,
    want: impl Fn(&[Frame]) -> bool,
) -> Vec<Frame> {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let f = frames.since(from);
        if want(&f) {
            return f;
        }
        assert!(Instant::now() < end, "never {what}: {f:?}");
        if rig.screen_failed() {
            rig.prepare_screen().unwrap();
        }
        rig.land_result();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A miss handed over with its staging held by the flag and the bound
/// passed: the pending presentation drawing. The frames' count then.
fn pending_after_hand_over(
    rig: &mut PlayerRig<'_>,
    frames: &Frames,
    a: &game::Loaded,
    report: &game::PlayReport,
    screen: &game::ResultScreen,
) -> (usize, tempfile::TempDir) {
    let (from, skin) = held_miss(rig, frames, a, report, screen);
    laid_out(rig);
    frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
    rig.render.pause_result_staging(true);
    rig.show_end(report, screen);
    until_true(rig, "the view's creation", |rig| {
        rig.render.creation_pass() > 0
    });
    rig.render.set_hold_clock(Some(BOUND + 1));
    let past = frames.len();
    frames.until(rig, past, "pending frames", |f| {
        f.iter().filter(|f| f.pending()).count() >= 3
    });
    (frames.len(), skin)
}

/// Result left after the hand-over while its first view still stages
/// (the flag set): no pending frame after the next screen's message
/// (V19 (f) after the hand-over).
#[test]
fn a_result_left_while_its_first_view_stages_draws_no_pending_frame_after() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (_, _skin) = pending_after_hand_over(rig, frames, &a, &report, &screen);
        rig.leave_result();
        rig.render.pause_result_staging(false);
        let after = frames.len();
        rig.render.send(RenderMsg::Menu(Box::new(
            crate::render_side::tests::one_line("select"),
        )));
        let next = frames.until(rig, after, "the next screen's frames", |f| {
            f.iter().filter(|f| f.lit > LIT && f.path.is_none()).count() >= 3
        });
        let first = next
            .iter()
            .position(|f| f.lit > LIT && f.path.is_none())
            .unwrap();
        assert!(
            next[first..].iter().all(|f| f.path.is_none()),
            "a result frame after the next screen's message: {next:?}"
        );
        assert_eq!(rig.render.presented_origin(), None, "an origin");
    });
}

/// Result left right after a pending frame, the render thread held at
/// that frame's end until the leaving and the next screen's message were
/// both made: the next loop turn withdraws the staging, its partial array
/// filled into its own retirement entry, before that message is handled,
/// and no step of it is written after (V19 (f) after the hand-over).
#[test]
fn a_result_left_after_a_pending_frame_withdraws_its_staging_before_the_next_message() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (_, _skin) = pending_after_hand_over(rig, frames, &a, &report, &screen);
        assert_eq!(rig.render.withdrawals(), Vec::new(), "a staging withdrawn");
        frames.meet();
        let taken = rig.render.messages_taken();
        let left = Instant::now();
        rig.leave_result();
        rig.render.pause_result_staging(false);
        let after = frames.len();
        rig.render.send(RenderMsg::Menu(Box::new(
            crate::render_side::tests::one_line("select"),
        )));
        frames.part();
        let next = frames.until(rig, after, "the next screen's frames", |f| {
            f.iter().filter(|f| f.lit > LIT && f.path.is_none()).count() >= 3
        });
        assert!(
            next.iter().all(|f| f.path.is_none()),
            "a result frame after the leaving: {next:?}"
        );
        let withdrawn = rig.render.withdrawals();
        let [w] = withdrawn.as_slice() else {
            panic!("the staging withdrawn once: {withdrawn:?}");
        };
        assert_eq!(
            w.messages, taken,
            "withdrawn before the next message was handled: {withdrawn:?}"
        );
        assert_eq!(
            w.filled[1],
            w.filled[0] + 1,
            "its partial array filled into its own entry: {withdrawn:?}"
        );
        let written: Vec<_> = rig
            .render
            .step_records()
            .into_iter()
            .filter(|r| r.atlas && r.at > left)
            .collect();
        assert_eq!(
            written.len(),
            0,
            "a step written after the leaving: {written:?}"
        );
    });
}

/// A leaving command pins the result after the bound, before its first
/// view commits, and the view's staging then ends: the commit point
/// discards that view, and every frame until the leaving is the pending
/// presentation, never the built-in end page.
#[test]
fn a_leaving_pinned_past_the_bound_keeps_the_pending_presentation() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (_, _skin) = pending_after_hand_over(rig, frames, &a, &report, &screen);
        let ended = rig.render.stagings_ended();
        let at = rig.now();
        assert_eq!(rig.begin_outro(at, None), game::Outro::Begun(None));
        rig.render.pause_result_staging(false);
        until_true(rig, "the staging's end", |rig| {
            rig.render.stagings_ended() > ended
        });
        let after = frames.len();
        let next = frames.until(rig, after, "frames after the staging's end", |f| {
            f.len() >= 5
        });
        assert!(
            next.iter().all(|f| f.pending()),
            "every frame pending after the pinned staging ended: {next:?}"
        );
        rig.leave_result();
    });
}

/// A leaving command pins the result before the bound, while its first
/// view stages in held passes, and the staging then ends: the commit
/// point never commits it, the passes stay held and nothing presents, so
/// the outgoing picture stays until the leaving.
#[test]
fn a_leaving_pinned_while_held_keeps_the_outgoing_picture() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss(rig, frames, &a, &report, &screen);
        laid_out(rig);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        let passes = rig.render.held_passes();
        until_true(rig, "held passes", |rig| {
            rig.render.held_passes() > passes + 3
        });
        let ended = rig.render.stagings_ended();
        let at = rig.now();
        assert_eq!(rig.begin_outro(at, None), game::Outro::Begun(None));
        rig.render.pause_result_staging(false);
        until_true(rig, "the staging's end", |rig| {
            rig.render.stagings_ended() > ended
        });
        let held_at = frames.len();
        let passes = rig.render.held_passes();
        std::thread::sleep(Duration::from_millis(300));
        assert!(rig.render.held_passes() > passes, "the held passes stopped");
        let presented = frames.since(held_at);
        assert!(
            presented.is_empty(),
            "a frame presented while held after the pin: {presented:?}"
        );
        rig.leave_result();
    });
}

/// A leaving command pins the result during the pacing of the frame whose
/// commit point would commit a first view prepared at once (the attempt
/// fell back to the built-in view), with the built-in rows' message
/// queued behind it: the commit discards that view and the hold decides
/// the frame at the barrier, so nothing presents before the bound, and
/// the pinned hold survives the rows' message: no built-in page, no
/// pending frame before the bound, the pending presentation after it.
#[test]
fn a_leaving_pinned_during_pacing_keeps_the_hold_through_the_rows() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.presenter
            .attempt_fallback("the test's fallback".to_owned());
        // The render thread held at a frame's end: the view and the rows
        // queued behind each other, taken one a turn.
        frames.meet();
        rig.show_end(&report, &screen);
        let gate = Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
        let g = Arc::clone(&gate);
        rig.render.after_next_pacing(
            1,
            Box::new(move || {
                let [reached, go] = &*g;
                reached.store(true, Ordering::SeqCst);
                let end = Instant::now() + Duration::from_secs(20);
                while !go.load(Ordering::SeqCst) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }),
        );
        let at = frames.len();
        frames.part();
        let [reached, go] = &*gate;
        let end = Instant::now() + Duration::from_secs(20);
        while !reached.load(Ordering::SeqCst) {
            assert!(Instant::now() < end, "the frame's pacing never reached");
            std::thread::sleep(Duration::from_millis(1));
        }
        // The leaving pinned after the frame's held check, before its
        // commit point.
        let pinned = rig.now();
        assert_eq!(rig.begin_outro(pinned, None), game::Outro::Begun(None));
        go.store(true, Ordering::SeqCst);
        let passes = rig.render.held_passes();
        until_true(rig, "held passes after the rows", |rig| {
            rig.render.held_passes() > passes + 3
        });
        std::thread::sleep(Duration::from_millis(200));
        let before = frames.since(at);
        assert!(
            before.is_empty(),
            "a frame presented before the bound after the pin: {before:?}"
        );
        rig.render.set_hold_clock(Some(BOUND + 1));
        let after = frames.until(rig, at, "pending frames past the bound", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        assert!(
            after.iter().all(|f| f.pending()),
            "a frame other than the pending presentation: {after:?}"
        );
        rig.leave_result();
    });
}

/// A device loss after the hand-over while the first view stages (the
/// flag set): the view is staged again on the rebuilt screen, and pending
/// frames draw until it commits (V19 (g), its second half, past the
/// bound).
#[test]
fn a_device_lost_while_the_first_view_stages_stages_it_again() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (at, _skin) = pending_after_hand_over(rig, frames, &a, &report, &screen);
        let creations = rig.render.creation_pass();
        crate::dev::lose_result_frame_at(1);
        let lost = until_rebuilt(rig, frames, at, "frames on the rebuilt screen", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        crate::dev::lose_result_frame_at(0);
        assert!(
            lost.iter().all(|f| f.pending()),
            "the pending presentation on the rebuilt screen: {lost:?}"
        );
        let rebuilt = frames.len();
        rig.render.pause_result_staging(false);
        let shown = until_rebuilt(rig, frames, rebuilt, "the skin's view", |f| {
            first_view(f).is_some()
        });
        assert!(
            rig.render.creation_pass() > creations,
            "the view not staged again on the rebuilt screen"
        );
        let first = first_view(&shown).unwrap();
        assert!(
            shown[..first].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        rig.leave_result();
    });
}

/// A size notice in the held phase after the hand-over: no further held
/// pass, and the pending presentation at the new size (V19 (h)).
#[test]
fn a_size_notice_while_held_ends_the_held_phase() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let extent = Arc::new(crate::render_side::Extent::default());
        extent.publish(1_920, 1_080);
        rig.render.follow(&extent);
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss(rig, frames, &a, &report, &screen);
        laid_out(rig);
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        let passes = rig.render.held_passes();
        until_true(rig, "held passes", |rig| {
            rig.render.held_passes() > passes + 3
        });
        let held_at = frames.len();
        extent.publish(1_280, 720);
        let after = frames.until(rig, held_at, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        let passes = rig.render.held_passes();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            rig.render.held_passes(),
            passes,
            "a held pass after the notice"
        );
        assert!(
            after.iter().all(|f| f.pending() && f.size == (1_280, 720)),
            "the pending presentation at the new size: {after:?}"
        );
        rig.render.pause_result_staging(false);
        rig.leave_result();
    });
}

/// A union's step refused after the bound: the pending presentation,
/// then the built-in view as the lifetime's first committed view, the
/// origin at its frame (V19 (j)).
#[test]
fn a_step_refused_after_the_bound_commits_the_built_in_view() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (at, _skin) = pending_after_hand_over(rig, frames, &a, &report, &screen);
        rig.render.refuse_next_step();
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, at, "a committed view", |f| {
            f.iter().any(|f| f.path.is_some() && !f.pending())
        });
        let first = shown
            .iter()
            .position(|f| f.path.is_some() && !f.pending())
            .unwrap();
        assert!(
            shown[..first].iter().all(|f| f.pending()),
            "pending until the first view: {shown:?}"
        );
        assert_eq!(
            shown[first].path,
            Some(ResultPath::BuiltIn),
            "the built-in view first: {shown:?}"
        );
        assert_eq!(
            rig.render.presented_origin(),
            shown[first].sampled,
            "the origin at the built-in view's frame"
        );
        rig.leave_result();
    });
}

/// A screen made for plays only, with no font, begins the result's hold:
/// the font is read ahead on a thread of its own, and the pending
/// presentation's frames draw the wait line with no font read in a frame
/// (V19 (k), the screen made for plays only).
#[test]
fn a_screen_for_plays_only_reads_the_pending_font_ahead() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp_fonts(a.chart(), false, |rig, frames| {
        use crate::menu_screen::FONT_READS;
        let reads = |i: usize| FONT_READS[i].load(Ordering::SeqCst);
        let (report, screen) = result_of(dir.path());
        let d = result::skin(
            "",
            "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
        );
        let p = result::pending(d.path(), result::LIMITS);
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(p)));
        prepared(rig, &a);
        frames.arm();
        let (in_frame, ahead) = (reads(0), reads(1));
        rig.begin_result(&report, &screen);
        rig.result_compared(&report, &screen);
        laid_out(rig);
        until_true(rig, "the font read ahead", |_| reads(1) > ahead);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        until_true(rig, "the view's creation", |rig| {
            rig.render.creation_pass() > 0
        });
        rig.render.set_hold_clock(Some(BOUND + 1));
        let from = frames.len();
        let pending = frames.until(rig, from, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        assert!(
            pending.iter().all(|f| f.pending()),
            "the wait line on every frame: {pending:?}"
        );
        assert_eq!(reads(0), in_frame, "a font read in a frame");
        rig.render.pause_result_staging(false);
        rig.leave_result();
    });
}

/// Lets a held font read go when dropped, a test's panic included.
struct FontGo;

impl Drop for FontGo {
    fn drop(&mut self) {
        crate::menu_screen::HOLD_FONT_READ.store(false, Ordering::SeqCst);
    }
}

/// A screen made for plays only whose font is still being read past the
/// bound (its worker held): no pass joins the worker, so the render
/// thread keeps taking its messages and presents nothing while the font
/// is read, reads none in a frame, and the pending presentation draws
/// once the worker ends (V19 (k), the screen made for plays only).
#[test]
fn a_font_read_held_past_the_bound_blocks_no_pass() {
    use crate::menu_screen::{FONT_READS, HOLD_FONT_READ};
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp_fonts(a.chart(), false, |rig, frames| {
        let reads = |i: usize| FONT_READS[i].load(Ordering::SeqCst);
        let (report, screen) = result_of(dir.path());
        let d = result::skin(
            "",
            "{ { id = \"dot\", src = \"s\", x = 8 * (nr % 3), y = 0, w = 8, h = 8 } }",
        );
        let p = result::pending(d.path(), result::LIMITS);
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(p)));
        prepared(rig, &a);
        frames.arm();
        let _go = FontGo;
        HOLD_FONT_READ.store(true, Ordering::SeqCst);
        let in_frame = reads(0);
        rig.begin_result(&report, &screen);
        rig.result_compared(&report, &screen);
        laid_out(rig);
        rig.render.pause_result_staging(true);
        rig.show_end(&report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        std::thread::sleep(Duration::from_millis(300));
        // The render thread past the bound with the font unread: its
        // messages still taken, each pass bounded.
        let taken = rig.render.messages_taken();
        rig.render.send(RenderMsg::Resize);
        let asked = Instant::now();
        while rig.render.messages_taken() == taken {
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "the render thread stopped taking messages while the font was read"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let waiting = frames.len();
        std::thread::sleep(Duration::from_millis(200));
        let presented = frames.since(waiting);
        assert!(
            presented.is_empty(),
            "a frame presented before the font was read: {presented:?}"
        );
        assert!(rig.render.font_waits() > 0, "no pass waited for the font");
        HOLD_FONT_READ.store(false, Ordering::SeqCst);
        let pending = frames.until(rig, waiting, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        assert!(
            pending.iter().all(|f| f.pending()),
            "the wait line on every frame: {pending:?}"
        );
        assert_eq!(reads(0), in_frame, "a font read in a frame");
        rig.render.pause_result_staging(false);
        rig.leave_result();
    });
}

/// A screen made for plays only lost past the bound, its font read, and
/// built again by the plays-only factory, which reads none: the font
/// goes to the new screen, so its pending presentation draws from its
/// first frame with no font read again, ahead or in a frame, even with
/// any new read held (V19 (g) and (k), a screen made for plays only).
#[test]
fn a_plays_only_screen_built_again_keeps_the_font() {
    use crate::menu_screen::{FONT_READS, HOLD_FONT_READ};
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp_fonts(a.chart(), false, |rig, frames| {
        let reads = |i: usize| FONT_READS[i].load(Ordering::SeqCst);
        let (report, screen) = result_of(dir.path());
        let _go = FontGo;
        let (from, _skin) = held(rig, frames, &a, &report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.until(rig, from, "pending frames", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        let (in_frame, ahead) = (reads(0), reads(1));
        HOLD_FONT_READ.store(true, Ordering::SeqCst);
        crate::dev::lose_result_frame_at(1);
        until_true(rig, "the loss", |rig| rig.screen_failed());
        crate::dev::lose_result_frame_at(0);
        let lost = frames.len();
        rig.prepare_screen().unwrap();
        let rebuilt = frames.until(rig, lost, "frames on the rebuilt screen", |f| {
            f.iter().filter(|f| f.pending()).count() >= 3
        });
        assert!(
            rebuilt.iter().all(|f| f.pending()),
            "the pending presentation from the rebuilt screen's first frame: {rebuilt:?}"
        );
        assert_eq!(
            (reads(0), reads(1)),
            (in_frame, ahead),
            "the font read again for the screen built again"
        );
        HOLD_FONT_READ.store(false, Ordering::SeqCst);
        rig.leave_result();
    });
}

/// No result skin: no hold, and the built-in end page at once from the
/// hand-over, past the bound with no pending frame (V19 (k), its second
/// case).
#[test]
fn a_result_without_a_skin_draws_the_end_page_at_once() {
    let _serial = imports::serial();
    let (dir, a) = chart(Some(RED));
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        rig.result_skin = ResultSkin::new(None);
        prepared(rig, &a);
        rig.render.send(RenderMsg::Menu(Box::default()));
        frames.arm();
        let from = frames.len();
        rig.begin_result(&report, &screen);
        rig.render.set_hold_clock(Some(BOUND + 1));
        frames.until(rig, from, "outgoing frames", |f| f.len() >= 3);
        let handed = frames.len();
        rig.show_end(&report, &screen);
        let shown = frames.until(rig, handed, "the end page", |f| {
            f.iter()
                .filter(|f| f.path == Some(ResultPath::BuiltIn))
                .count()
                >= 3
        });
        assert!(
            shown.iter().all(|f| f.path != Some(ResultPath::Pending)),
            "a pending frame with no skin: {shown:?}"
        );
        let first = shown.iter().position(|f| f.path.is_some()).unwrap();
        assert!(
            shown[first..]
                .iter()
                .all(|f| f.path == Some(ResultPath::BuiltIn)),
            "the end page from its first frame: {shown:?}"
        );
        rig.leave_result();
    });
}
