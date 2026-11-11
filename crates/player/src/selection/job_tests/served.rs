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
        let mut decision = game::Decision::begin(now, Some(game::DECISION_TIME), false);
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
                            controls.watched().is_none_or(|d| d.left().is_some())
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
        let mut decision = game::Decision::begin(now, Some(game::DECISION_TIME), false);
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
        let mut decision = game::Decision::begin(now, Some(game::DECISION_TIME), false);
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
            let mut decision = game::Decision::begin(begun, Some(game::DECISION_TIME), false);
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
    let left = |c: &Controls| c.watched().is_none_or(|d| d.left().is_some());
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
