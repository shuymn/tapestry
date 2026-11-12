//! The play state's screen origin (design.md "Timers", docs/stage3.md D3)
//! through the render thread: a loading lifetime's origin handed over by
//! its pause, end or clear, the first one of each play state kept, taken by
//! the play state's first play and copied into each play's frames; with no
//! lifetime, anchored the skin's loading minimum before READY opens. Each
//! test reads every frame an [`Installer`] built on the shared clock and
//! projects its screen clock to one instant, so a frame at any real time
//! checks the origin exactly.

use super::*;

const MS: i64 = 1_000;

/// The loading time the skin declares in most of these tests.
const LOADEND: TimeUs = TimeUs(2_500 * MS);

/// A render thread on [`Installer`]s with a chart whose one note lies at
/// 60 s and a skin declaring the loading time `loadend` chosen.
struct Rig {
    chart: Arc<Chart>,
    screens: Installers,
    link: super::super::RenderLink,
    clock: QpcClock,
    rule: rules::RulePreset,
    device: DeviceProfile,
}

impl Rig {
    fn new(loadend: Option<TimeUs>) -> Self {
        Self::with_frames(loadend, Box::new(NoFrames))
    }

    /// [`new`](Self::new) with the frame sink `frames`.
    fn with_frames(loadend: Option<TimeUs>, frames: Box<dyn super::super::FrameSink>) -> Self {
        let chart = Arc::new(late_tap());
        let screens = Installers::default();
        screens.screen_clocks.lock().unwrap().reserve(1 << 16);
        let link = screens.spawn_with(frames);
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(bundle_declaring(None, loadend)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let clock = link.clock;
        Self {
            chart,
            screens,
            link,
            clock,
            rule: rules::tapestry(),
            device: DeviceProfile::default(),
        }
    }

    /// A play state begins (a chart chosen, a retry outside practice).
    fn play_state(&self, number: u64) {
        self.link.play_state(game::PlayState {
            number,
            origin: self.clock.now(),
        });
    }

    /// A loading lifetime begins: its origin on the clock.
    fn begin(&self) -> TimeUs {
        self.link.begin_loading(&chosen("T"), None);
        self.origin()
    }

    /// The current loading lifetime's origin, as the game thread reads it.
    fn origin(&self) -> TimeUs {
        self.link
            .shared
            .current_origin()
            .1
            .expect("a current lifetime")
    }

    /// An attempt prepared and committed, then its play started with READY
    /// opened at `ready_state_at` and chart time 0 a second after it; the
    /// frames noted from here are its own.
    fn play(&self, ready_state_at: TimeUs) {
        drop(self.play_from(ready_state_at, after(ready_state_at, 1_000)));
    }

    /// [`play`](Self::play) with chart time 0 at `zero` on the clock: the
    /// logic step, for a rearm.
    fn play_from(&self, ready_state_at: TimeUs, zero: TimeUs) -> Logic<'_, SourceReader> {
        let ready = self
            .link
            .prepare_attempt(attempt(&self.chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        self.link.commit_attempt().unwrap();
        let (logic, side) = Logic::<SourceReader>::new(
            &self.chart,
            &self.rule,
            &self.device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let mut measure = play_measure(zero);
        measure.ready_state_at = ready_state_at;
        self.link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure,
            // The state clock's origin, apart from every screen origin.
            state_origin: TimeUs(ready_state_at.0 - 7_654 * MS),
        });
        self.fresh();
        logic
    }

    /// Forgets the frames noted so far, once the render thread has taken
    /// every message sent: those noted from here were built after them.
    fn fresh(&self) {
        assert!(self.link.fence(Duration::from_secs(5)));
        self.screens.screen_clocks.lock().unwrap().clear();
    }

    /// The screen clock at `at` on the clock as each of at least three
    /// frames noted since [`fresh`](Self::fresh) gives it: one value, or
    /// the frames disagree.
    #[track_caller]
    fn screen_at(&self, at: TimeUs) -> i64 {
        let noted = &self.screens.screen_clocks;
        assert!(wait_until(|| noted.lock().unwrap().len() >= 3));
        let read: Vec<i64> = noted
            .lock()
            .unwrap()
            .iter()
            .map(|&(sample, screen, _)| screen + (at.0 - sample))
            .collect();
        assert!(read.windows(2).all(|w| w[0] == w[1]), "{read:?}");
        read[0]
    }

    fn exit(self) {
        let _ = self.link.finish(self.clock.now());
        self.link.exit();
    }
}

/// `t` plus `ms`.
fn after(t: TimeUs, ms: i64) -> TimeUs {
    TimeUs(t.0 + ms * MS)
}

/// A real-time gap, so origins stamped on either side of it differ.
fn gap() {
    std::thread::sleep(Duration::from_millis(30));
}

/// C1: a lifetime paused at the start gate and cleared by the opening
/// hands its origin to the play: 1 734 ms at T + 1 734 ms, not the
/// anchor's 3 000 000 (READY at T + 1 234 ms less 2 500 ms), nor READY's
/// or play's.
#[test]
fn the_screen_origin_survives_pause_clear_and_play() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(t, 1_234));
    assert_eq!(rig.screen_at(after(t, 1_734)), 1_734 * MS);
    rig.exit();
}

/// The play frames' measured screen clock (dev-tools: a scenario's
/// sidecar, design.md "Shortening Launch-Based Checks") is the one each
/// frame was built with, paired by chart time, under a state clock whose
/// origin is apart from the screen's (design.md "Timers").
#[cfg(feature = "dev-tools")]
#[test]
fn a_play_frame_s_measured_screen_clock_is_the_built_one() {
    /// Each play frame's chart time and measured screen clock.
    struct Measured(Arc<std::sync::Mutex<Vec<(TimeUs, Option<i64>)>>>);
    impl super::super::FrameSink for Measured {
        fn frame(&mut self, _: &dyn super::super::Screen, drawn: &crate::measure::Drawn<'_>) {
            if let Some(now) = drawn.play {
                let mut seen = self.0.lock().unwrap();
                if seen.len() < seen.capacity() {
                    seen.push((now, drawn.screen_us));
                }
            }
        }
    }
    let measured = Arc::new(std::sync::Mutex::new(Vec::with_capacity(1 << 16)));
    let rig = Rig::with_frames(Some(LOADEND), Box::new(Measured(Arc::clone(&measured))));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(t, 1_234));
    // The screen clock of the lifetime, and the state clock 6 420 ms
    // ahead of it (its origin 7 654 ms before READY at T + 1 234 ms).
    assert_eq!(rig.screen_at(after(t, 1_734)), 1_734 * MS);
    let built = rig.screens.screen_clocks.lock().unwrap().clone();
    let measured = measured.lock().unwrap().clone();
    let pairs: Vec<(i64, Option<i64>)> = built
        .iter()
        .filter_map(|&(_, screen, now)| {
            let m = measured.iter().find(|&&(n, _)| n == now)?;
            Some((screen, m.1))
        })
        .collect();
    assert!(pairs.len() >= 3, "{built:?} {measured:?}");
    for (screen, sidecar) in pairs {
        assert_eq!(sidecar, Some(screen));
    }
    rig.exit();
}

/// C1: a lifetime ended without a gate (`end_loading`) hands its origin
/// over with the end, read before the end dropped it.
#[test]
fn an_ended_lifetime_s_origin_reaches_the_play() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.end_loading().unwrap();
    rig.play(after(t, 1_234));
    assert_eq!(rig.screen_at(after(t, 1_734)), 1_734 * MS);
    rig.exit();
}

/// C1: a deferred opening resumes the paused lifetime and pauses it
/// again: the same lifetime, the same origin.
#[test]
fn a_deferred_resume_keeps_the_screen_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    gap();
    rig.link.pause_loading().unwrap();
    rig.link.resume_loading();
    gap();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(t, 1_600));
    assert_eq!(rig.screen_at(after(t, 2_000)), 2_000 * MS);
    rig.exit();
}

/// C1: the pending origin is the render thread's, outside the screen: a
/// screen failure and a renderer built again while the lifetime is paused
/// keep it, and the play takes it with no second pause (the clear comes
/// after the play, so nothing hands it over again): 2 345 ms at
/// T + 2 345 ms, not the anchor's 3 345 000.
#[test]
fn a_device_recovery_while_paused_keeps_the_pending_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.screens.fail.store(true, Ordering::Release);
    rig.link.send(RenderMsg::Menu(Box::default()));
    assert!(wait_until(|| rig.link.failed()));
    rig.screens.fail.store(false, Ordering::Release);
    rig.link.prepare().unwrap();
    assert_eq!(rig.screens.builds.load(Ordering::SeqCst), 2);
    rig.play(after(t, 1_500));
    assert_eq!(rig.screen_at(after(t, 2_345)), 2_345 * MS);
    rig.link.clear_loading();
    rig.exit();
}

/// C3: a lifetime held at the choice, its play state begun after it, is
/// revealed at the decision deadline: the screen runs from the reveal,
/// neither from the choice (the state clock's origin) nor from the
/// lifetime's begin, nor anchored.
#[test]
fn a_held_lifetime_s_screen_starts_at_its_reveal() {
    let rig = Rig::new(Some(LOADEND));
    rig.link.begin_loading_held(&chosen("T"), None);
    let t = rig.origin();
    rig.play_state(1);
    gap();
    rig.link.reveal_loading();
    let revealed = rig.origin();
    assert!(revealed.0 >= t.0 + 30 * MS, "{t:?} {revealed:?}");
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(revealed, 1_500));
    assert_eq!(rig.screen_at(after(revealed, 1_400)), 1_400 * MS);
    rig.exit();
}

/// C10, C5: a retry's play state leaves the finished play's frames on its
/// origin: a frame built after the `PlayState` was taken still reads
/// 5 000 ms at T + 5 000 ms. The retry's play, with no lifetime, reads
/// exactly the skin's loading minimum when READY opens.
#[test]
fn an_old_frame_after_play_state_keeps_the_previous_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(t, 1_234));
    assert_eq!(rig.screen_at(after(t, 5_000)), 5_000 * MS);
    let _ = rig.link.finish(rig.clock.now());
    rig.play_state(2);
    rig.fresh();
    assert_eq!(rig.screen_at(after(t, 5_000)), 5_000 * MS);
    let ready = rig.clock.now();
    rig.play(ready);
    assert_eq!(rig.screen_at(ready), 2_500 * MS);
    rig.exit();
}

/// C2: a rearm draws a new candidate from its own chart origin, but the
/// screen runs on from the lifetime's origin.
#[test]
fn a_rearm_keeps_the_lifetime_s_screen() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    let mut logic = rig.play_from(after(t, 1_234), after(t, 2_234));
    assert_eq!(rig.screen_at(after(t, 2_500)), 2_500 * MS);
    logic.rearm(
        &InputStart::default(),
        TimeUs(rig.clock.now().0 + 10_000 * MS),
    );
    let noted = &rig.screens.screen_clocks;
    assert!(wait_until(|| {
        noted
            .lock()
            .unwrap()
            .iter()
            .filter(|&&(_, _, now)| now.0 < -9_000 * MS)
            .count()
            >= 3
    }));
    assert_eq!(rig.screen_at(after(t, 2_500)), 2_500 * MS);
    drop(logic);
    rig.exit();
}

/// C2, C4: with no lifetime the screen reads the skin's loading minimum
/// when READY opens and runs on through a rearm: 3 500 ms a second later.
#[test]
fn a_rearm_keeps_the_anchored_screen() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let ready = rig.clock.now();
    let mut logic = rig.play_from(ready, after(ready, 1_000));
    assert_eq!(rig.screen_at(ready), 2_500 * MS);
    logic.rearm(
        &InputStart::default(),
        TimeUs(rig.clock.now().0 + 10_000 * MS),
    );
    rig.fresh();
    let noted = &rig.screens.screen_clocks;
    assert!(wait_until(|| {
        noted
            .lock()
            .unwrap()
            .iter()
            .filter(|&&(_, _, now)| now.0 < -9_000 * MS)
            .count()
            >= 3
    }));
    assert_eq!(rig.screen_at(after(ready, 1_000)), 3_500 * MS);
    drop(logic);
    rig.exit();
}

/// C5: a lifetime ended before its play state's play (a quit while
/// loading, then the next chart's play state) leaves nothing: the next
/// play, with no lifetime, is anchored.
#[test]
fn a_lifetime_left_before_play_gives_no_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let _ = rig.begin();
    rig.link.clear_loading();
    rig.play_state(2);
    let ready = rig.clock.now();
    rig.play(ready);
    assert_eq!(rig.screen_at(ready), 2_500 * MS);
    rig.exit();
}

/// C4: the anchor is the loading minimum, at least a second: 1 000 ms at
/// READY for a skin declaring 500 ms and for one declaring nothing.
#[test]
fn a_direct_launch_floors_the_screen_lead() {
    for loadend in [Some(TimeUs(500 * MS)), None] {
        let rig = Rig::new(loadend);
        rig.play_state(1);
        let ready = rig.clock.now();
        rig.play(ready);
        assert_eq!(rig.screen_at(ready), 1_000 * MS, "{loadend:?}");
        rig.exit();
    }
}

/// C4: a round anchors at READY as the lifecycle opens it (recording's
/// opening), not at the skin's READY timer 200 ms later nor at chart time
/// 0: a section from 30 000 ms with a 1 000 ms lead-in opens at chart
/// 28 800 ms, where the screen reads 2 500 ms, and 2 700 ms 200 ms later.
#[test]
fn a_direct_launch_round_anchors_at_ready_s_opening() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    // READY opens now, at chart 28 800 ms: chart time 0 lies 28 800 ms
    // before, the skin's READY timer (the section's start less its
    // lead-in) 200 ms after.
    let ready_state_at = rig.clock.now();
    let zero = after(ready_state_at, -28_800);
    drop(rig.play_from(ready_state_at, zero));
    assert_eq!(rig.screen_at(ready_state_at), 2_500 * MS);
    assert_eq!(rig.screen_at(after(ready_state_at, 200)), 2_700 * MS);
    rig.exit();
}

/// C6: a practice session's screen origin is its first Start's lifetime:
/// a later round reads on from it, and a later Start's lifetime replaces
/// it neither at its pause nor at its clear.
#[test]
fn a_practice_start_keeps_the_session_s_screen_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let s = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(s, 1_234));
    assert_eq!(rig.screen_at(after(s, 1_734)), 1_734 * MS);
    rig.play(rig.clock.now());
    assert_eq!(rig.screen_at(after(s, 9_876)), 9_876 * MS);
    gap();
    let second = rig.begin();
    assert!(second.0 >= s.0 + 30 * MS);
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(second, 1_000));
    assert_eq!(rig.screen_at(after(s, 21_234)), 21_234 * MS);
    rig.exit();
}

/// C6, the clear's hand-over: a first Start cancelled while loading,
/// never paused, hands its origin over with its clear, so the next Start's
/// lifetime does not become the session's.
#[test]
fn a_practice_start_cancelled_before_its_pause_keeps_the_session_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let s = rig.begin();
    rig.link.clear_loading();
    gap();
    let second = rig.begin();
    assert!(second.0 >= s.0 + 30 * MS);
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(second, 1_000));
    assert_eq!(rig.screen_at(after(s, 21_234)), 21_234 * MS);
    rig.exit();
}

/// C6, first wins: a first Start paused and then refused keeps its origin
/// pending through its clear, and the next Start's pause and clear do not
/// replace it.
#[test]
fn a_practice_start_refused_after_its_pause_keeps_the_session_origin() {
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let s = rig.begin();
    gap();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    gap();
    let second = rig.begin();
    assert!(second.0 >= s.0 + 60 * MS);
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(second, 1_000));
    assert_eq!(rig.screen_at(after(s, 21_234)), 21_234 * MS);
    rig.exit();
}

/// C8: the play frames that carry the screen clock allocate nothing in
/// the app region, a lifetime's origin and an anchored one, the first
/// frames included; the positive control proves the region counts a
/// build's allocation.
#[cfg(feature = "dev-tools")]
#[test]
fn play_frames_screen_alloc() {
    let (control, _) = app_region_count(true, false);
    assert!(control.is_some_and(|n| n > 0), "{control:?}");
    let rig = Rig::new(Some(LOADEND));
    rig.play_state(1);
    let t = rig.begin();
    rig.link.pause_loading().unwrap();
    rig.link.clear_loading();
    rig.play(after(t, 1_234));
    assert_eq!(rig.screen_at(after(t, 1_734)), 1_734 * MS);
    let fig = rig
        .link
        .finish(rig.clock.now())
        .expect("the play's figures");
    assert_eq!(fig.render_app_allocations, Some(0));
    rig.play_state(2);
    let ready = rig.clock.now();
    rig.play(ready);
    assert_eq!(rig.screen_at(ready), 2_500 * MS);
    let fig = rig
        .link
        .finish(rig.clock.now())
        .expect("the play's figures");
    assert_eq!(fig.render_app_allocations, Some(0));
    rig.link.exit();
}
