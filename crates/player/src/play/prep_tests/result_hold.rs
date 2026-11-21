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
/// The rounding a counted wait is allowed past its bound.
const SLACK_US: u64 = 100;

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

/// The blocking GPU waits counted so far: those of held passes and those
/// of presenting passes, each one's length in µs.
fn gpu_waits() -> (Vec<u64>, Vec<u64>) {
    let all = render::probe::gpu_waits();
    let of = |k| {
        all.iter()
            .filter(|(p, _)| *p == k)
            .map(|&(_, us)| us)
            .collect()
    };
    (
        of(render::probe::PASS_HELD),
        of(render::probe::PASS_PRESENTING),
    )
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
        let (held_waits, presenting) = gpu_waits();
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
            !held_waits.is_empty(),
            "the held passes' bounded wait counted"
        );
        let holds = frames.holds();
        assert!(
            matches!(holds.as_slice(), [h] if h.ended_by == Some(crate::measure::HoldEnd::Skin)),
            "the record's mark: the outgoing phase ended by the first view: {holds:?}"
        );
        assert!(
            held_waits.iter().all(|&us| us <= HELD_PACE_US + SLACK_US),
            "each held wait at most HELD_PACE: {held_waits:?}"
        );
        assert_eq!(presenting, Vec::<u64>::new(), "no blocking wait presenting");
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
    let d = result::skin(
        "",
        &format!(
            "{{ {{ id = \"dot\", src = \"s\", x = {side} * (nr % 3), y = 0, w = {side}, h = {side} }} }}"
        ),
    );
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

/// The blocking GPU waits [`staged_past_the_bound`] counted from the
/// bound to the first view's commit: those of held passes and those of
/// presenting passes.
type Waits = (Vec<u64>, Vec<u64>);

/// A miss's first view whose staging steps past the bound after the
/// hand-over, its writes held by the test's flag, then let go with each
/// step's completion delayed; the counter's scope the uploader's and
/// stagings' waits, or with `pacing` the frame's own pacing too. Asserts
/// V19 (c)'s frames; the waits counted from the bound on.
fn staged_past_the_bound(pacing: bool) -> Waits {
    let (dir, a) = chart(Some(RED));
    let mut waits = None;
    on_warp(a.chart(), |rig, frames| {
        let (report, screen) = result_of(dir.path());
        let (from, _skin) = held_miss(rig, frames, &a, &report, &screen);
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
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, held_at, "the skin's view", |f| first_view(f).is_some());
        waits = Some(gpu_waits());
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
    let (held, presenting) = staged_past_the_bound(false);
    assert_eq!(held, Vec::<u64>::new(), "a held wait after the bound");
    assert_eq!(presenting, Vec::<u64>::new(), "a blocking wait presenting");
}

/// The control of V9b's counter: widened to the frame's own pacing, the
/// same run counts waits in presenting passes, so the zero above is the
/// counter's scope, not frames that never paced.
#[test]
fn the_blocking_wait_counter_widened_counts_the_frames_pacing() {
    let _serial = imports::serial();
    let (_, presenting) = staged_past_the_bound(true);
    assert!(!presenting.is_empty(), "no pacing wait counted presenting");
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
        let stepping = frames.len();
        let unpaused = Instant::now();
        rig.render.pause_result_staging(false);
        let shown = frames.until(rig, stepping, "the skin's view", |f| {
            first_view(f).is_some()
        });
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
