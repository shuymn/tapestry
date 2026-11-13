//! The decision screen served inside a play's release barrier and its
//! install (design.md "Play Lifecycle", the decision screen): the audio
//! thread held at its gate ([`AudioGate`]) keeps the unload in the control
//! queue, untaken, while an event reaches the watched decision screen; the
//! gate is let go once the look has acted on it, never by timing.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};

use bms_model::{Speed, TimeSource as _};
use game::coord::Spawner;
use game::{AppRig as _, Loading};

use super::{
    Decodes, Probe, bench, decodes_with, hook, installed, loaded, on_gated_rig, outcome, song,
    until, watchdog,
};
use crate::controls::Controls;
use crate::dev::AudioGate;

/// What reaches the watched decision screen while the barrier's unload is
/// held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// The window closed.
    Close,
    /// The audio output failed.
    AudioFailure,
    /// The deadline passed (the play's step).
    Deadline,
    /// The deadline passed, then the quit key in the loading state.
    DeadlineThenQuit,
}

/// How a watched load ended with `event` inside its barrier's unload.
#[derive(Debug)]
struct Ended {
    got: Loading,
    installed: bool,
    decision: game::Decision,
    audio_failed: bool,
}

/// A play's load watched by its decision screen (its deadline 1.5 s after
/// the load begins), its release barrier's unload held at the audio
/// thread's gate until `event` has been acted on by a look: a leaving
/// verdict latched, or the play's step run (the ingress open).
fn barrier_held(event: Event) -> Ended {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let gate = Arc::new(AudioGate::default());
    let (at_tx, at_rx) = mpsc::channel::<()>();
    let at_tx = Mutex::new(at_tx);
    let held = Arc::clone(&gate);
    let decodes: Decodes = decodes_with(hook(move |p, controls, _| {
        if p == Probe::Barrier {
            assert!(
                !controls.ingress().is_open(),
                "the deadline before the barrier"
            );
            held.hold();
            until("the audio thread at its gate", || held.stopped());
            at_tx.lock().unwrap().send(()).unwrap();
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let released: &AudioGate = &gate;
    on_gated_rig(b.sel.take().unwrap(), &gate, |rig| {
        let controls: &Controls = rig.controls;
        let lost = Arc::clone(&rig.audio_lost);
        // The choice closed the epoch; its deadline is 1.5 s away.
        controls.close_epoch();
        let now = rig.clock.now();
        let mut decision = game::Decision::begin(
            now,
            game::DecisionLifetime(1),
            Some(game::DecisionShown::BuiltIn),
            false,
        );
        let got = std::thread::scope(|s| {
            s.spawn(move || {
                at_rx.recv_timeout(watchdog()).expect("the barrier reached");
                match event {
                    Event::Close => controls.close(),
                    Event::AudioFailure => lost.store(true, Ordering::Release),
                    Event::Deadline | Event::DeadlineThenQuit => {}
                }
                match event {
                    Event::Close | Event::AudioFailure => {
                        // Left, or the load already gone with it.
                        until("the screen left", || {
                            controls.with_watched(|d| d.is_none_or(|d| d.left().is_some()))
                        });
                    }
                    Event::Deadline | Event::DeadlineThenQuit => {
                        until("the step", || controls.ingress().is_open());
                    }
                }
                if event == Event::DeadlineThenQuit {
                    controls.command(game::Command::Quit);
                }
                released.release();
            });
            outcome(rig.load_chart(&loaded, &mut decision))
        });
        Ended {
            got,
            installed: installed(rig, &loaded),
            decision,
            audio_failed: rig.failures.output().is_some(),
        }
    })
}

/// C2: inside the play's release barrier, its unload held, a close or an
/// audio failure leaves the decision screen with `End` (the run ends, App
/// routing it with its cause) and nothing is installed; the deadline steps
/// once inside it and the load goes on to install its bank.
#[test]
fn the_barrier_serves_the_decision_screen() {
    for event in [Event::Close, Event::AudioFailure] {
        let ended = barrier_held(event);
        assert_eq!(ended.got, Loading::Cancelled, "{event:?}");
        assert!(!ended.installed, "{event:?}: installed after the leave");
        assert_eq!(ended.decision.left(), Some(game::Verdict::End), "{event:?}");
        assert!(!ended.decision.stepped(), "{event:?}");
        assert_eq!(ended.audio_failed, event == Event::AudioFailure);
    }
    let ended = barrier_held(Event::Deadline);
    assert_eq!(ended.got, Loading::Ready);
    assert!(ended.installed);
    assert!(ended.decision.stepped(), "the step inside the barrier");
    assert_eq!(ended.decision.left(), None);
}

/// C2d: the deadline's step inside the barrier, then the quit key in the
/// loading state: the ordinary loading cancellation, no leaving verdict
/// latched, nothing installed.
#[test]
fn a_quit_after_the_step_inside_the_barrier_cancels_the_load() {
    let ended = barrier_held(Event::DeadlineThenQuit);
    assert_eq!(ended.got, Loading::Cancelled);
    assert!(!ended.installed, "the quit was ignored");
    assert!(ended.decision.stepped());
    assert_eq!(ended.decision.left(), None, "a loading cancellation");
}

/// C2b: a close and an audio failure together, seen by the load's own
/// cancellation check (on the decode's job thread, held until the game
/// thread's wait asks the job to stop): the look runs first, so the
/// screen is left with `End`, which App routes with the audio failure,
/// never as the close's clean quit.
#[test]
fn a_close_with_an_audio_failure_in_the_load_reports_the_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let lost = Arc::new(Mutex::new(None::<Arc<std::sync::atomic::AtomicBool>>));
    let at = Arc::clone(&lost);
    let decodes = decodes_with(hook(move |p, controls, stop| {
        if p == Probe::Decode(Speed::NORMAL) {
            if let Some(lost) = at.lock().unwrap().as_ref() {
                lost.store(true, Ordering::Release);
            }
            controls.close();
            until("the job's stop", stop);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let gate = Arc::new(AudioGate::default());
    let (got, decision, audio_failed) = on_gated_rig(b.sel.take().unwrap(), &gate, |rig| {
        *lost.lock().unwrap() = Some(Arc::clone(&rig.audio_lost));
        rig.controls.close_epoch();
        let now = rig.clock.now();
        let mut decision = game::Decision::begin(
            now,
            game::DecisionLifetime(1),
            Some(game::DecisionShown::BuiltIn),
            false,
        );
        let got = outcome(rig.load_chart(&loaded, &mut decision));
        (got, decision, rig.failures.output().is_some())
    });
    assert_eq!(got, Loading::Cancelled);
    assert!(audio_failed);
    assert_eq!(
        decision.left(),
        Some(game::Verdict::End),
        "the close taken as the clean quit"
    );
}

/// C4b: the install's switch kept unsent (its pushes find the control
/// queue full): the deadline steps inside its wait, then a close leaves
/// with `End`; nothing is installed and no load stays outstanding (the
/// next unload goes through).
#[test]
fn the_install_serves_the_decision_screen_while_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let mut s = b.sel.take().unwrap();
    let full = Arc::new(Mutex::new(None::<Arc<std::sync::atomic::AtomicU32>>));
    let (made_tx, made) = mpsc::channel::<()>();
    let set = Arc::clone(&full);
    s.after_bank = Some(Arc::new(move || {
        if let Some(full) = set.lock().unwrap().as_ref() {
            full.store(u32::MAX, Ordering::Release);
        }
        made_tx.send(()).unwrap();
    }));
    let gate = Arc::new(AudioGate::default());
    let ended = on_gated_rig(s, &gate, |rig| {
        let full_for = rig.audio.control.forced_full();
        *full.lock().unwrap() = Some(Arc::clone(&full_for));
        let controls: &Controls = rig.controls;
        controls.close_epoch();
        let now = rig.clock.now();
        let mut decision = game::Decision::begin(
            now,
            game::DecisionLifetime(1),
            Some(game::DecisionShown::BuiltIn),
            false,
        );
        let got = std::thread::scope(|s| {
            s.spawn(move || {
                made.recv_timeout(watchdog()).expect("the bank made");
                assert!(
                    !controls.ingress().is_open(),
                    "the deadline before the install"
                );
                until("the step", || controls.ingress().is_open());
                controls.close();
            });
            outcome(rig.load_chart(&loaded, &mut decision))
        });
        let installed = installed(rig, &loaded);
        full_for.store(0, Ordering::Release);
        let next = rig.audio.control.unload();
        (got, installed, decision, next)
    });
    let (got, installed, decision, next) = ended;
    assert_eq!(got, Loading::Cancelled);
    assert!(!installed);
    assert!(decision.stepped(), "the step inside the unsent install");
    assert_eq!(decision.left(), Some(game::Verdict::End));
    assert_eq!(next, Ok(()), "a load left outstanding");
}

/// A play's load watched by its decision screen, its deadline already due
/// when `due`, on a rig with its adjustments and its input thread: the
/// attach fence at the load's start held on the input thread until `met`
/// holds, `act` run once the thread is at it.
fn attach_held(
    due: bool,
    act: &(dyn Fn(&Controls) + Sync),
    met: &(dyn Fn(&Controls) -> bool + Sync),
) -> Ended {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let sel = b.sel.take().unwrap();
    let (ended, _) = crate::tests::on_rig_hooked(
        super::settings(),
        crate::render_side::tests::Installers::default().spawn_bare(),
        input::QpcClock::start(),
        |rig, _, end| {
            rig.selection = Some(sel);
            let (thread, _jobs, _waker, _clock) = crate::selection::tests::input_thread(end);
            let (at_tx, at_rx) = mpsc::channel::<()>();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let (reached, reached_rx) = mpsc::channel();
            let (resume_tx, resume) = mpsc::channel();
            rig.input.fence_gate(crate::input_side::FenceGate {
                before: Box::new(move |_| {
                    at_tx.send(()).unwrap();
                    let _ = go_rx.recv_timeout(watchdog());
                }),
                reached,
                resume,
            });
            let controls: &Controls = rig.controls;
            controls.close_epoch();
            let now = rig.clock.now();
            let begun = if due {
                bms_model::TimeUs(now.0 - game::DECISION_TIME.0)
            } else {
                now
            };
            let mut decision = game::Decision::begin(
                begun,
                game::DecisionLifetime(1),
                Some(game::DecisionShown::BuiltIn),
                false,
            );
            let got = std::thread::scope(|s| {
                let go = go_tx.clone();
                s.spawn(move || {
                    at_rx
                        .recv_timeout(watchdog())
                        .expect("the input thread at the fence");
                    act(controls);
                    until("the fence's event acted on", || met(controls));
                    let _ = go.send(());
                });
                let got = outcome(rig.load_chart(&loaded, &mut decision));
                let _ = go_tx.send(());
                got
            });
            assert!(
                reached_rx.recv_timeout(watchdog()).is_ok(),
                "the fence answered"
            );
            resume_tx.send(()).unwrap();
            let ended = Ended {
                got,
                installed: installed(rig, &loaded),
                decision,
                audio_failed: rig.failures.output().is_some(),
            };
            assert!(
                rig.failures.input().is_none(),
                "a leave latched as the input's failure"
            );
            thread.stop().unwrap();
            ended
        },
    );
    ended
}

/// A4: the play's attach fence, held on the input thread, serves the
/// decision screen through the watched load: a close leaves with `End`
/// before anything is decoded or installed, nothing latched; the deadline
/// steps once inside it and the load goes on to install.
#[test]
fn the_attach_fence_serves_the_decision_screen() {
    let left = |c: &Controls| c.with_watched(|d| d.is_none_or(|d| d.left().is_some()));
    let close = |c: &Controls| c.close();
    let ended = attach_held(false, &close, &left);
    assert_eq!(ended.got, Loading::Cancelled);
    assert!(!ended.installed);
    assert_eq!(ended.decision.left(), Some(game::Verdict::End));
    let stepped = |c: &Controls| c.ingress().is_open();
    let nothing = |_: &Controls| {};
    let ended = attach_held(true, &nothing, &stepped);
    assert_eq!(ended.got, Loading::Ready);
    assert!(ended.installed);
    assert!(ended.decision.stepped());
    assert_eq!(ended.decision.left(), None);
}

/// A rig on a render thread of `screens`, the decision page drawn and its
/// screen lost (a frame failed), a play's decision screen watched with the
/// screen served by its looks; `f` then runs on it.
fn decision_screen_lost(
    screens: &crate::render_side::tests::Installers,
    f: impl FnOnce(&mut crate::play::PlayerRig<'_>),
) {
    decision_screen_lost_within(screens, None, f);
}

/// [`decision_screen_lost`], a rebuild's answer bounded by `bound`.
fn decision_screen_lost_within(
    screens: &crate::render_side::tests::Installers,
    bound: Option<std::time::Duration>,
    f: impl FnOnce(&mut crate::play::PlayerRig<'_>),
) {
    let mut link = screens.spawn_bare();
    if let Some(bound) = bound {
        link.set_timeouts(bound, std::time::Duration::from_secs(5));
    }
    let _ = crate::tests::on_rig_with(super::settings(), link, |rig, _| {
        let page = crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: "decision".to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        };
        rig.render.show_decision(page, None);
        until("the decision page drawn", || {
            screens
                .menu_frames
                .lock()
                .unwrap()
                .iter()
                .any(|l| l == "decision")
        });
        screens.fail.store(true, Ordering::Release);
        until("the screen lost", || rig.render.failed());
        screens.fail.store(false, Ordering::Release);
        let now = rig.clock.now();
        rig.controls.watch_decision(crate::controls::DecisionWatch {
            decision: game::Decision::begin(
                now,
                game::DecisionLifetime(1),
                Some(game::DecisionShown::BuiltIn),
                false,
            ),
            origin: game::Origin::Load,
            reveal: Box::new(|| {}),
            failures: rig.failures.clone(),
            screen: Some(rig.render.recovery_handle()),
        });
        f(rig);
        let _ = rig.controls.unwatch_decision();
    });
}

/// B4: the screen lost inside a wait the rig owns: the wait's looks build
/// it again, one rebuild, and the render thread draws the decision page
/// on it again.
#[test]
fn a_screen_lost_inside_a_rig_wait_is_rebuilt() {
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost(&screens, |rig| {
        let drawn = screens.menu_frames.lock().unwrap().len();
        until("rebuilt from the looks", || {
            let _ = rig.controls.look();
            !rig.render.failed() && rig.render.recovery_state() == "idle"
        });
        assert_eq!(rig.render.prepares(), 1);
        until("the decision page drawn again", || {
            screens.menu_frames.lock().unwrap()[drawn..]
                .iter()
                .any(|l| l == "decision")
        });
    });
}

/// B5: the rebuild the looks began fails: its failure stays for the flow,
/// later looks begin no second rebuild, and the flow takes it as the
/// screen's error from the rig's recovery, again with no second rebuild.
#[test]
fn a_failed_rig_rebuild_ends_the_run_through_app() {
    use game::AppRig as _;
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost(&screens, |rig| {
        screens.fail_builds.store(1, Ordering::Release);
        until("the rebuild failed", || {
            let _ = rig.controls.look();
            rig.render.recovery_state() == "failed"
        });
        for _ in 0..20 {
            let _ = rig.controls.look();
        }
        assert_eq!(rig.render.prepares(), 1, "a second rebuild from the looks");
        rig.begin_recovery();
        let got = rig.poll_recovery(rig.clock.now());
        assert!(matches!(got, Some(Err(_))), "{got:?}");
        assert_eq!(rig.render.prepares(), 1, "a second rebuild from the flow");
    });
}

/// B6: the rebuild the looks began is still running when the wait ends:
/// the flow adopts it, so one rebuild in all, and goes on once it answers.
#[test]
fn app_adopts_a_rebuild_begun_inside_a_rig_wait() {
    use game::AppRig as _;
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost(&screens, |rig| {
        screens.hold_builds.store(true, Ordering::Release);
        until("the rebuild held", || {
            let _ = rig.controls.look();
            screens.build_held.load(Ordering::Acquire)
        });
        // The wait ends; the flow takes the rebuild over.
        let _ = rig.controls.unwatch_decision();
        rig.begin_recovery();
        screens.hold_builds.store(false, Ordering::Release);
        let mut got = None;
        until("the rebuild answered", || {
            let now = rig.clock.now();
            got = rig.poll_recovery(bms_model::TimeUs(now.0 + 1_000));
            got.is_some()
        });
        assert!(matches!(got, Some(Ok(()))), "{got:?}");
        assert_eq!(rig.render.prepares(), 1, "a second rebuild");
        assert!(!rig.render.failed());
    });
}

/// B1r (rig): the flow's poll of a rebuild that is held waits no longer
/// than its slice, so the flow looks at the screen between polls; once
/// let go, the rebuild answers.
#[test]
fn the_flow_polls_a_held_rebuild_without_waiting_for_it() {
    use game::AppRig as _;
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost(&screens, |rig| {
        screens.hold_builds.store(true, Ordering::Release);
        let _ = rig.controls.unwatch_decision();
        rig.begin_recovery();
        until("the rebuild held", || {
            screens.build_held.load(Ordering::Acquire)
        });
        for _ in 0..3 {
            let now = rig.clock.now();
            assert!(
                rig.poll_recovery(bms_model::TimeUs(now.0 + 20_000))
                    .is_none(),
                "answered while held"
            );
        }
        screens.hold_builds.store(false, Ordering::Release);
        let mut got = None;
        until("the rebuild answered", || {
            let now = rig.clock.now();
            got = rig.poll_recovery(bms_model::TimeUs(now.0 + 1_000));
            got.is_some()
        });
        assert!(matches!(got, Some(Ok(()))), "{got:?}");
    });
}

/// E1, E2 (the load): a chart's screen load that lost the device once is
/// built again with the chart and the load is ready; lost twice, the load
/// fails as the screen's failure, held for the flow, not as a refusal.
#[test]
fn a_device_loss_in_the_chart_load_is_rebuilt_or_ends_the_run() {
    use game::AppRig as _;
    for lost in [1, 2] {
        let dir = tempfile::tempdir().unwrap();
        let path = song(dir.path(), "");
        let loaded = loaded(dir.path(), &path, Speed::NORMAL);
        let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
        let installers = crate::render_side::tests::Installers::default();
        installers.lose_chart_loads.store(lost, Ordering::Release);
        super::on_rig(b.sel.take().unwrap(), &installers, false, |rig| {
            let got = rig.load_chart(&loaded, &mut game::Decision::off());
            if lost == 1 {
                assert_eq!(outcome(got), Loading::Ready);
                assert!(installed(rig, &loaded));
                assert!(rig.screen_load_failed().is_none());
            } else {
                assert!(
                    matches!(got, Err(super::super::ChartLoadError::ScreenFailed)),
                    "{got:?}"
                );
                assert!(rig.screen_load_failed().is_some(), "not held for the flow");
            }
        });
        assert_eq!(installers.chart_loads.lock().unwrap().len(), 2, "{lost}");
    }
}

/// M1: a rebuild a wait's looks began fails after the deadline: the next
/// preparation of the screen reports that failure, once, and asks no
/// second rebuild for it.
#[test]
fn a_saved_rebuild_failure_is_reported_by_the_next_preparation() {
    use game::Rig as _;
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost(&screens, |rig| {
        screens.fail_builds.store(1, Ordering::Release);
        until("the rebuild failed", || {
            let _ = rig.controls.look();
            rig.render.recovery_state() == "failed"
        });
        let _ = rig.controls.unwatch_decision();
        let got = rig.prepare_screen();
        assert!(got.is_err(), "the saved failure not reported: {got:?}");
        assert_eq!(rig.render.prepares(), 1, "a second rebuild for it");
        assert_eq!(rig.render.recovery_state(), "idle");
    });
}

/// M2: the flow begins a rebuild and leaves the decision screen before it
/// answers: the rebuild stays with its owner, and selection's preparation
/// takes it over (one rebuild in all) and reports how it ended, a failure
/// included.
#[test]
fn a_rebuild_left_by_the_flow_is_taken_over_by_the_next_screen() {
    use game::AppRig as _;
    use game::Rig as _;
    for fails in [false, true] {
        let screens = crate::render_side::tests::Installers::default();
        decision_screen_lost(&screens, |rig| {
            let _ = rig.controls.unwatch_decision();
            screens.hold_builds.store(true, Ordering::Release);
            rig.begin_recovery();
            until("the rebuild held", || {
                screens.build_held.load(Ordering::Acquire)
            });
            // The flow leaves (a refusal at the deadline, a Back) without
            // polling it; selection prepares its screen.
            if fails {
                screens.fail_builds.store(1, Ordering::Release);
            }
            let release = screens.hold_builds.clone();
            let got = std::thread::scope(|s| {
                s.spawn(move || release.store(false, Ordering::Release));
                rig.prepare_screen()
            });
            assert_eq!(got.is_err(), fails, "{fails}: {got:?}");
            assert_eq!(rig.render.prepares(), 1, "{fails}: a second rebuild");
        });
    }
}

/// M3: a screen failure another load left is not this load's: a load that
/// the skin refuses reports a refusal, with no screen failure for the flow.
#[test]
fn a_load_takes_no_screen_failure_another_load_left() {
    use game::AppRig as _;
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let installers = crate::render_side::tests::Installers::default();
    installers.refuse_chart_loads.store(true, Ordering::Release);
    super::on_rig(b.sel.take().unwrap(), &installers, false, |rig| {
        rig.screen_load_failure = Some(crate::render_side::ScreenError::NoAnswer);
        let got = rig.load_chart(&loaded, &mut game::Decision::off());
        assert!(
            matches!(got, Err(super::super::ChartLoadError::Screen(_))),
            "{got:?}"
        );
        assert!(
            rig.screen_load_failed().is_none(),
            "the other load's failure"
        );
    });
}

/// Round 2 M1: a rebuild from a wait's looks reaches its bound, saved as a
/// failure; the render thread then builds the screen and clears the
/// device's flag. The screen is still to be served (the saved failure),
/// and the next preparation (selection's entry at the latest) reports it,
/// once; then nothing is outstanding.
#[test]
fn a_rebuild_failure_saved_before_the_device_came_back_is_reported() {
    use game::Rig as _;
    let screens = crate::render_side::tests::Installers::default();
    decision_screen_lost_within(
        &screens,
        Some(std::time::Duration::from_millis(200)),
        |rig| {
            screens.hold_builds.store(true, Ordering::Release);
            until("the rebuild timed out", || {
                let _ = rig.controls.look();
                rig.render.recovery_state() == "failed"
            });
            let _ = rig.controls.unwatch_decision();
            screens.hold_builds.store(false, Ordering::Release);
            until("the device back", || !rig.render.failed());
            assert!(rig.screen_failed(), "the saved failure not outstanding");
            let got = rig.prepare_screen();
            assert!(
                matches!(got, Err(crate::render_side::ScreenError::NoAnswer)),
                "{got:?}"
            );
            assert!(!rig.screen_failed(), "reported twice");
            assert!(rig.prepare_screen().is_ok());
        },
    );
}

/// Round 2 M2: a rebuild failure saved during the chart's load, the play
/// stepped: the opening serves it before the attempt's preparation
/// begins, so the run ends with it and the screen builder is not called
/// again by the preparation.
#[test]
fn the_opening_reports_a_saved_rebuild_failure_before_its_preparation() {
    use bms_model::TimeSource as _;
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let mut loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let installers = crate::render_side::tests::Installers::default();
    super::on_rig(b.sel.take().unwrap(), &installers, false, |rig| {
        assert_eq!(
            outcome(rig.load_chart(&loaded, &mut game::Decision::off())),
            Loading::Ready
        );
        let page = crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: "decision".to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        };
        rig.render.show_decision(page, None);
        until("the decision page drawn", || {
            installers
                .menu_frames
                .lock()
                .unwrap()
                .iter()
                .any(|l| l == "decision")
        });
        installers.fail.store(true, Ordering::Release);
        until("the screen lost", || rig.render.failed());
        installers.fail.store(false, Ordering::Release);
        installers.fail_builds.store(1, Ordering::Release);
        let now = rig.clock.now();
        rig.controls.watch_decision(crate::controls::DecisionWatch {
            decision: game::Decision::begin(
                now,
                game::DecisionLifetime(1),
                Some(game::DecisionShown::BuiltIn),
                false,
            ),
            origin: game::Origin::Load,
            reveal: Box::new(|| {}),
            failures: rig.failures.clone(),
            screen: Some(rig.render.recovery_handle()),
        });
        until("the rebuild failed", || {
            let _ = rig.controls.look();
            rig.render.recovery_state() == "failed"
        });
        let _ = rig.controls.unwatch_decision();
        let calls = installers.build_calls.load(Ordering::Acquire);
        let got = game::load_and_open(&mut loaded, rig);
        assert!(matches!(got, Err(game::LiveError::Screen(_))), "{got:?}");
        assert_eq!(
            installers.build_calls.load(Ordering::Acquire),
            calls,
            "the screen built again before the saved failure was reported"
        );
    });
}
