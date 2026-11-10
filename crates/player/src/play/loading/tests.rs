//! The player's rig in the loading state, on a real input thread and
//! driver: acceptance closing and reopening on both sides (design.md "Play
//! Lifecycle", tuning while loading), and the opening's copy taken
//! without a nested lock or an allocation. A watchdog
//! (`TAPESTRY_TEST_WATCHDOG`, seconds) panics a wait that never ends, or
//! ends the process where the wait is inside a thread scope.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bms_model::{TimeSource as _, TimeUs};
use game::{Adjust, LoadingRig as _, Revalidated, Rig as _};

use crate::play::PlayerRig;
use crate::tuning::{HoldClass, KeyEvent};

mod flow;

/// SELECT and START on the keyboard as installed.
const SELECT: u32 = 0x11;
const START: u32 = 0x12;

/// How long a test waits for what must come; `TAPESTRY_TEST_WATCHDOG`
/// (seconds) shortens it for mutation runs.
fn watchdog() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Work for the rig's input thread to run on its source set.
type Job = Box<dyn FnOnce(&mut input::SourceSet) + Send>;

/// The rig's input thread's keyboard, for a test to press on.
struct Keyboard {
    jobs: std::sync::mpsc::Sender<Job>,
    waker: input::Waker,
    clock: input::QpcClock,
}

impl Keyboard {
    /// Key `code` to `value` in a batch of its own at the input thread's
    /// next wake.
    fn key(&self, code: u32, value: i32) {
        let clock = self.clock;
        self.batch(code, value, move || clock.now());
    }

    /// [`key`](Self::key) in a batch stamped `at`: the time the activity
    /// record takes for it.
    fn key_at(&self, code: u32, value: i32, at: TimeUs) {
        self.batch(code, value, move || at);
    }

    fn batch(&self, code: u32, value: i32, at: impl FnOnce() -> TimeUs + Send + 'static) {
        let _ = self.jobs.send(Box::new(move |s: &mut input::SourceSet| {
            let at = at();
            s.begin_batch(at);
            s.packet(&input::Packet {
                device: input::DeviceId(1),
                control: eval::ControlId(code),
                value,
                background: false,
            });
            s.end_batch(at);
        }));
        self.waker.wake();
    }

    /// A batch with no packet, which samples the window's focus, and what
    /// the adjustment tap sampled there: focus, and the losses counted.
    fn sample_focus(&self) -> (bool, u64) {
        let (tx, rx) = std::sync::mpsc::channel();
        let clock = self.clock;
        let _ = self.jobs.send(Box::new(move |s: &mut input::SourceSet| {
            let at = clock.now();
            s.begin_batch(at);
            s.end_batch(at);
            let tap = s.adjustment().expect("the tap");
            let _ = tx.send((tap.focused(), tap.focus_losses()));
        }));
        self.waker.wake();
        rx.recv_timeout(watchdog())
            .expect("the input thread sampled")
    }

    /// SELECT held, then START with it, then both released: the selected
    /// cover switches between LIFT and HIDDEN, one accepted adjustment.
    fn both(&self) {
        self.key(SELECT, 1);
        self.key(START, 1);
        self.key(START, 0);
        self.key(SELECT, 0);
    }
}

/// Settings with LIFT and HIDDEN on.
fn covers() -> game::Settings {
    game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings {
            covers: game::CoverSettings {
                lift_on: true,
                hidden_on: true,
                ..game::CoverSettings::default()
            },
            ..game::PlayerSettings::default()
        },
    }
}

/// `f` on a player's rig whose input thread serves its link, a keyboard
/// connected to it, with LIFT and HIDDEN on.
fn on_input_rig<R>(f: impl FnOnce(&mut PlayerRig<'_>, &Keyboard) -> R) -> R {
    let render = crate::render_side::tests::Installers::default().spawn_bare();
    on_input_rig_with(covers(), render, f)
}

/// [`on_input_rig`] with `settings` and the render thread `render`.
fn on_input_rig_with<R>(
    settings: game::Settings,
    render: crate::render_side::RenderLink,
    f: impl FnOnce(&mut PlayerRig<'_>, &Keyboard) -> R,
) -> R {
    use input::{DeviceId, DeviceKind, SourceConfig, SourceSet};
    crate::tests::on_rig_hooked(settings, render, input::QpcClock::start(), |rig, _, end| {
        let crate::tests::InputEnd {
            mut hook,
            waker,
            clock,
        } = end;
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 16,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(0));
        // The window's focus latches the source set's gate, as at startup.
        rig.controls.set_latch(sources.gate().handle());
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        let thread = input::ClockedThread::spawn(clock, sources, &waker, move |s, now| {
            while let Ok(job) = rx.try_recv() {
                job(s);
            }
            hook.wake(s, now)
        })
        .unwrap();
        let keyboard = Keyboard { jobs, waker, clock };
        let out = f(rig, &keyboard);
        thread.stop().unwrap();
        out
    })
    .0
}

/// The reducer's count after a fence: every key sent before is in it.
fn fenced(rig: &mut PlayerRig<'_>) -> u64 {
    let (driver, mut pump) = rig.adjustments().expect("the adjustments");
    driver.fence(&mut pump).unwrap();
    driver.accepted()
}

/// Whether the reducer takes presses.
fn accepting(rig: &PlayerRig<'_>) -> bool {
    rig.adjust
        .as_ref()
        .is_some_and(game::AdjustDriver::accepting)
}

/// A13, the input side's half: closed, START and SELECT through the input
/// thread act on nothing in the reducer, though the input side records
/// their holds; reopened, a fresh press acts again.
#[test]
fn a_press_in_the_closed_gap_is_discarded_and_rearms_through_the_input_thread() {
    on_input_rig(|rig, keys| {
        let base = fenced(rig);
        keys.both();
        assert_eq!(fenced(rig), base + 1, "open: accepted");
        let closed = rig.close_acceptance(1).unwrap();
        assert!(!accepting(rig));
        assert_eq!(closed.revision.reducer, base + 1);
        let input_before = rig.controls.tuning().snapshot().revision.input;
        keys.both();
        assert_eq!(fenced(rig), base + 1, "closed: discarded");
        assert!(
            rig.controls.tuning().snapshot().revision.input > input_before,
            "the input side recorded the holds"
        );
        rig.reopen_acceptance().unwrap();
        assert!(accepting(rig));
        keys.both();
        assert_eq!(fenced(rig), base + 2, "reopened: a fresh press acts");
    });
}

/// A19: deferred (acceptance reopened after the opening's close), a
/// settings key and a play adjustment both act, and the release's close
/// carries them into the starting settings the rearm opens with.
#[test]
fn an_adjustment_during_a_deferral_reaches_the_rearms_tuning() {
    on_input_rig(|rig, keys| {
        let base = fenced(rig);
        let at_o1 = rig.tuning_snapshot().unwrap();
        let mut settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        rig.apply_snapshot(&at_o1, &mut settings);
        assert_eq!(rig.accepted_at_o1, Some(base));
        assert_eq!(
            rig.revalidate_and_close(&at_o1, 1).unwrap(),
            Revalidated::Closed
        );
        // Deferred: reopened.
        rig.reopen_acceptance().unwrap();
        rig.controls
            .settings_key(Adjust::DisplayOffsetUp, KeyEvent::Press);
        assert_eq!(
            rig.controls.tuning().held(),
            [(Adjust::DisplayOffsetUp, HoldClass::Fresh)]
        );
        rig.controls
            .settings_key(Adjust::DisplayOffsetUp, KeyEvent::Release);
        keys.both();
        assert_eq!(fenced(rig), base + 1);
        // Released: closed again on a fresh copy, applied.
        let rearm = rig.close_acceptance(1).unwrap();
        assert!(!accepting(rig));
        assert_eq!(rearm.revision.reducer, base + 1);
        let offset = at_o1.values.unwrap().display_offset_us + 1_000;
        assert_eq!(rearm.values.unwrap().display_offset_us, offset);
        rig.apply_snapshot(&rearm, &mut settings);
        assert_eq!(settings.device.display_offset_us, offset);
        assert_eq!(rig.accepted_at_o1, Some(base + 1));
    });
}

/// A21: closed again after the release, a settings key pressed while the
/// end of loading is acknowledged steps nothing and holds nothing, and
/// START and SELECT act on nothing in the reducer.
#[test]
fn a_press_during_the_second_acknowledgement_is_discarded() {
    on_input_rig(|rig, keys| {
        let base = fenced(rig);
        let closed = rig.close_acceptance(2).unwrap();
        rig.controls
            .settings_key(Adjust::DisplayOffsetUp, KeyEvent::Press);
        keys.both();
        rig.end_loading().unwrap();
        rig.controls
            .settings_key(Adjust::DisplayOffsetUp, KeyEvent::Release);
        assert_eq!(fenced(rig), base, "the reducer discarded it");
        let now = rig.controls.tuning().snapshot();
        assert_eq!(now.values, closed.values);
        assert_eq!(now.revision.window, closed.revision.window);
        assert!(rig.controls.tuning().held().is_empty());
        assert!(!now.held);
    });
}

/// The opening's copy, its revalidation, the release's close and the
/// reopening through the rig, repeated while the window thread routes
/// settings keys and focus changes and the input thread publishes START
/// and SELECT: none waits on a lock it already holds or on the other
/// threads. The rig's work runs on its own thread; the watchdog ends the
/// process if it never ends.
#[test]
fn the_opening_snapshot_takes_no_nested_lock() {
    on_input_rig(|rig, keys| {
        // A deadlock ends the process: the scope's threads never end.
        let _watchdog = crate::tests::Watchdog::arm("the rig's work");
        let done = AtomicBool::new(false);
        let controls = rig.controls;
        let worker = &mut *rig;
        std::thread::scope(|s| {
            let window = s.spawn(|| {
                let mut n = 0_u32;
                while !done.load(Ordering::Acquire) {
                    for a in [Adjust::DisplayOffsetUp, Adjust::DisplayOffsetDown] {
                        controls.settings_key(a, KeyEvent::Press);
                        controls.settings_key(a, KeyEvent::Repeat);
                        controls.settings_key(a, KeyEvent::Release);
                    }
                    n = n.wrapping_add(1);
                    if n.is_multiple_of(16) {
                        controls.focus(false);
                        controls.focus(true);
                    }
                    std::thread::yield_now();
                }
            });
            let input = s.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    keys.key(START, 1);
                    keys.key(SELECT, 1);
                    keys.key(SELECT, 0);
                    keys.key(START, 0);
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let game = s.spawn(move || {
                let mut settings = game::Settings {
                    device: game::DeviceSettings::default(),
                    player: game::PlayerSettings::default(),
                };
                let mut outcomes = [0_u32; 2];
                for generation in 1..=200 {
                    let at_o1 = worker.tuning_snapshot().unwrap();
                    worker.apply_snapshot(&at_o1, &mut settings);
                    let i = match worker.revalidate_and_close(&at_o1, generation).unwrap() {
                        Revalidated::Closed => 0,
                        Revalidated::Changed => 1,
                    };
                    outcomes[i] += 1;
                    let released = worker.close_acceptance(generation).unwrap();
                    worker.apply_snapshot(&released, &mut settings);
                    worker.reopen_acceptance().unwrap();
                    let _ = worker.focus_deferred();
                }
                outcomes
            });
            let outcomes = game.join();
            done.store(true, Ordering::Release);
            let outcomes = outcomes.unwrap();
            assert_eq!(outcomes.iter().sum::<u32>(), 200, "{outcomes:?}");
            window.join().unwrap();
            input.join().unwrap();
        });
        // Nothing failed meanwhile.
        assert!(rig.input_failure().is_none());
        assert!(rig.controls.focused());
    });
}

/// F1 and F2 on the keyboard: hi-speed slower and faster, the tuning
/// controls the reducer takes without START or SELECT.
const SLOWER: u32 = game::SPEED_KEYS[0];
const FASTER: u32 = game::SPEED_KEYS[1];

/// §2.10 R9-R11 and I7 (finding 1): F2 tapped, then F1 held, repeated
/// and released, each in a batch at a time the test sets, through the
/// input thread's tap: the opening's copy (read after a fence) holds while
/// F1 is held, and is quiet exactly [`game::QUIET`] after the last of
/// them, F1's repeat restarting the second. The reducer took each step.
#[test]
fn speed_keys_restart_the_quiet_wait_at_exact_deadlines() {
    let q = game::QUIET.0;
    on_input_rig(|rig, keys| {
        let base = fenced(rig);
        // Past any batch the input thread makes on its own.
        let t = rig.clock.now().0 + 100_000_000;
        let quiet = |rig: &mut PlayerRig<'_>, last: i64| {
            fenced(rig);
            let s = rig.tuning_snapshot().unwrap();
            assert!(s.stable);
            assert_eq!(s.last_activity, Some(TimeUs(last)));
            assert!(!s.quiet_at(TimeUs(last + q - 1)), "{last}");
            assert!(s.quiet_at(TimeUs(last + q)), "{last}");
            s
        };
        keys.key_at(FASTER, 1, TimeUs(t));
        keys.key_at(FASTER, 0, TimeUs(t + 10_000));
        let tapped = quiet(rig, t + 10_000);
        keys.key_at(SLOWER, 1, TimeUs(t + 2_000_000));
        fenced(rig);
        let held = rig.tuning_snapshot().unwrap();
        assert!(held.held, "F1 holds");
        assert!(!held.quiet_at(TimeUs(i64::MAX)));
        assert_eq!(held.last_activity, Some(TimeUs(t + 2_000_000)));
        keys.key_at(SLOWER, 1, TimeUs(t + 2_500_000));
        fenced(rig);
        let repeated = rig.tuning_snapshot().unwrap();
        assert!(repeated.held);
        assert_eq!(repeated.last_activity, Some(TimeUs(t + 2_500_000)));
        assert_eq!(repeated.revision.input, held.revision.input + 1);
        keys.key_at(SLOWER, 0, TimeUs(t + 3_000_000));
        let released = quiet(rig, t + 3_000_000);
        assert_eq!(released.revision.input, tapped.revision.input + 3);
        assert_eq!(fenced(rig), base + 3, "F2, F1 and F1's repeat stepped");
    });
}

/// §2.10 R15 (finding 3): a START tap ends just before a fence, and the
/// input thread is held right after it answered the fence. The game
/// thread, let go by the answer, already reads the tap in the activity
/// record: its revision moved twice, its last activity is the tap's
/// batch, nothing held.
#[test]
fn the_fence_answers_after_the_activity_is_published() {
    on_input_rig(|rig, _| {
        fenced(rig);
        let before = rig.tuning_snapshot().unwrap();
        let at = TimeUs(rig.clock.now().0 + 100_000_000);
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        rig.input.fence_gate(crate::input_side::FenceGate {
            before: Box::new(move |s: &mut input::SourceSet| {
                for value in [1, 0] {
                    s.begin_batch(at);
                    s.packet(&input::Packet {
                        device: input::DeviceId(1),
                        control: eval::ControlId(START),
                        value,
                        background: false,
                    });
                    s.end_batch(at);
                }
            }),
            reached: reached_tx,
            resume: resume_rx,
        });
        fenced(rig);
        reached
            .recv_timeout(watchdog())
            .expect("the fence answered");
        // The input thread waits right after the answer.
        let after = rig.tuning_snapshot().unwrap();
        assert!(after.stable);
        assert_eq!(after.revision.input, before.revision.input + 2);
        assert_eq!(after.last_activity, Some(at));
        assert!(!after.held);
        resume.send(()).unwrap();
    });
}

/// §2.10 R22: a write of the activity record held in progress (its
/// sequence odd) makes every copy unstable: never quiet, never unchanged
/// since the opening's, so the revalidation does not close. Ended, the
/// same copy closes. (No input thread serves this rig, so the input
/// side's section has no other writer.)
#[test]
fn a_torn_snapshot_is_never_quiet_or_unchanged() {
    let ((), _) = crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
        let tuning = std::sync::Arc::clone(rig.controls.tuning());
        let at_o1 = rig.tuning_snapshot().unwrap();
        assert!(at_o1.stable && at_o1.quiet_at(TimeUs(i64::MAX)));
        let writing = tuning.activity().input.begin();
        let torn = rig.tuning_snapshot().unwrap();
        assert!(!torn.stable);
        assert!(!torn.quiet_at(TimeUs(i64::MAX)));
        assert!(!torn.unchanged_since(&at_o1, TimeUs(i64::MAX)));
        assert!(!at_o1.unchanged_since(&torn, TimeUs(i64::MAX)));
        assert!(!tuning.close_unchanged(&at_o1, 1, TimeUs(i64::MAX)));
        assert_eq!(tuning.acceptance(), crate::tuning::Acceptance::Open);
        drop(writing);
        assert!(tuning.close_unchanged(&at_o1, 1, TimeUs(i64::MAX)));
    });
}

/// The opening's copy on the practice path (a round's section kept by
/// the chart): the tuning's copy under its step lock, the revalidation's
/// close and the release's close allocate nothing; the preparation's key,
/// which clones the section, is built outside them.
#[test]
fn the_opening_snapshot_allocates_nothing_alloc() {
    let mut loaded = crate::imports::facts::tests::loaded("", "tapestry");
    let initial_gauges = loaded.preset().gauges.iter().map(|g| g.start).collect();
    loaded
        .set_practice(Some(game::Practice {
            start: TimeUs(0),
            end: TimeUs(4_000_000),
            lead_in: TimeUs(1_000_000),
            initial_gauges,
            repeat: true,
        }))
        .unwrap();
    let ((), _) = crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
        let key = loaded.prep_key();
        let tuning = std::sync::Arc::clone(rig.controls.tuning());
        let warm = rig.tuning_snapshot().unwrap();
        let _ = tuning.close_unchanged(&warm, 1, TimeUs(i64::MAX));
        tuning.reopen();
        let before = crate::alloc_count::this_thread();
        let at_o1 = rig.tuning_snapshot().unwrap();
        let closed = tuning.close_unchanged(&at_o1, 1, TimeUs(i64::MAX));
        tuning.reopen();
        let released = tuning.close(2);
        tuning.reopen();
        let allocated = crate::alloc_count::this_thread() - before;
        assert_eq!(allocated, 0);
        assert!(closed);
        assert_eq!(released.revision.window, at_o1.revision.window);
        assert_eq!(key, loaded.prep_key());
    });
}

/// O-21 and §2.10 R7: the result shown, the input offset keys work; one is
/// pressed and still held when the result is left (to selection, the
/// practice page, or a retry's preparation). Leaving revokes them and
/// drops the held key in the same step: its value stays, the loading state
/// that follows is not held by it, and its repeat and release, like every
/// offset key from then on, change no value and are no activity.
#[test]
fn the_offset_keys_are_revoked_on_leaving_the_result() {
    use game::AppRig as _;
    for exit in ["selection", "page", "retry"] {
        on_input_rig(|rig, _| {
            let dir = tempfile::tempdir().unwrap();
            let report = crate::end_screen::tests::report(rules::tapestry());
            let screen = crate::end_screen::tests::screen(dir.path(), &report);
            let tuning = std::sync::Arc::clone(rig.controls.tuning());
            rig.show_end(&report, &screen);
            let shown = tuning.get().input_offset_us;
            rig.controls
                .settings_key(Adjust::InputOffsetUp, KeyEvent::Press);
            assert_eq!(tuning.get().input_offset_us, shown + 1_000, "{exit}");
            assert_eq!(
                tuning.held(),
                [(Adjust::InputOffsetUp, HoldClass::Fresh)],
                "{exit}"
            );
            match exit {
                "selection" => rig.entered_selection(),
                "page" => {
                    let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
                    rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
                }
                _ => {
                    rig.serve_leaving(game::MenuCommand::Retry);
                    rig.prepare_screen().unwrap();
                }
            }
            assert!(!tuning.end_screen_shown(), "{exit}");
            assert!(
                tuning.held().is_empty(),
                "{exit}: dropped at the revocation"
            );
            // The loading state that follows is not held by it.
            rig.begin_loading_epoch();
            let before = tuning.snapshot();
            assert!(before.stable && !before.held, "{exit}");
            assert!(before.quiet_at(TimeUs(i64::MAX)), "{exit}");
            // Its own repeat and release, still to come, count nothing.
            for e in [KeyEvent::Repeat, KeyEvent::Release] {
                rig.controls.settings_key(Adjust::InputOffsetUp, e);
                assert_eq!(tuning.snapshot(), before, "{exit} {e:?}");
            }
            for a in [Adjust::InputOffsetDown, Adjust::InputOffsetUp] {
                for e in [KeyEvent::Press, KeyEvent::Repeat, KeyEvent::Release] {
                    rig.controls.settings_key(a, e);
                    assert!(tuning.held().is_empty(), "{exit} {a:?} {e:?}");
                    assert_eq!(tuning.snapshot(), before, "{exit} {a:?} {e:?}");
                }
            }
            assert_eq!(tuning.get().input_offset_us, shown + 1_000, "{exit}");
        });
    }
}
