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
use super::{LetGo, imports, settings};
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

/// One frame drawn while armed.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// What its result stamp names; `None` for a frame of another screen.
    path: Option<ResultPath>,
    /// Its texels with a channel above 200.
    lit: usize,
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
    log: Arc<Mutex<Vec<Frame>>>,
}

impl Frames {
    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn len(&self) -> usize {
        self.log.lock().unwrap().len()
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
    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        if !self.0.armed.load(Ordering::Acquire) {
            return;
        }
        let lit = screen.read_back().map_or(0, |(_, _, pixels)| {
            pixels
                .chunks(4)
                .filter(|p| p.iter().take(3).any(|&c| c > 200))
                .count()
        });
        self.0.log.lock().unwrap().push(Frame {
            path: drawn.result.map(|r| r.path),
            lit,
        });
    }
}

/// `f` on a player's rig over a render thread on a WARP menu screen,
/// `chart` loaded, its frames recorded into the [`Frames`] given, the
/// hold's bound [`BOUND`] and its clock stopped at the hold's start.
fn on_warp(chart: &bms_model::Chart, f: impl FnOnce(&mut PlayerRig<'_>, &Frames)) {
    let frames = Frames::default();
    let make: MakeScreen = Box::new(move || {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        Ok(Box::new(MenuScreen::new(r, Some(render::text::Fonts::runtime()?))) as Box<dyn Screen>)
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
