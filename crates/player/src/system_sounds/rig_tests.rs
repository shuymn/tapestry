//! The system sounds on the player's rig, as `rig()` makes them on both
//! launch paths: an entry's wait a run-ending signal ends ends the run
//! through `game::App::run` (ST7e (c)), the settlement before recording
//! leaves within a slice on each signal (ST15), and a close gives up a
//! pick blocked in its read (ST16). The owner is armed with the tests'
//! seams ([`super::arm`]): the sets under a test's data root, a decoder
//! that can block, the recovery state in memory or on disk, and a picker.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use audio::{GenId, SystemKind};
use game::{AudioReadiness, Command, DataRoot, LiveError, Rig as _};

use super::tests::{Fixture, MemFs, Rule, Seam, wait};
use super::{Event, Hooks, RealClock, Setup, SystemSounds, arm};
use crate::dev::AudioGate;
use crate::imports::STOP_WAIT;
use crate::play::PlayerRig;

/// How long after a signal a wait that reads it at each 1 ms slice may
/// take to leave on a loaded machine: far below every bound it could
/// otherwise have left at (the entry's 2 s, the acknowledgement's 2 s).
const LEAVES_WITHIN: Duration = Duration::from_millis(500);

/// A blocked read: the decode waits until the seam's gate opens.
const BLOCKED: Rule = Rule {
    block: true,
    ignore_cancel: false,
    panic: false,
    fake: None,
    delay: None,
};

/// Arms the next rig's owner: `fx`'s data root (its `sound\` sets and
/// `defaultsound\`), `seam`'s decoder, the recovery state in `fs` (on
/// `fx`'s disk when `None`), and the `pick`-th set of each role.
fn arm_run(fx: &Fixture, seam: &Seam, pick: &Arc<AtomicUsize>, fs: Option<MemFs>) {
    let root = fx.root();
    let seam = seam.clone();
    let pick = Arc::clone(pick);
    arm(move |setup, _| {
        setup.root = root;
        setup.decoder = Box::new(seam);
        if let Some(fs) = fs {
            setup.fs = Box::new(fs);
        }
        setup.picker = Box::new(move |n| pick.load(Ordering::Acquire).min(n.saturating_sub(1)));
    });
}

/// `f` on a play's rig with the default settings, its audio thread held
/// by `gate` while it is.
fn on_rig<R>(gate: Option<Arc<AudioGate>>, f: impl FnOnce(&mut PlayerRig<'_>) -> R) -> R {
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let render = crate::render_side::tests::Installers::default().spawn_bare();
    crate::tests::on_rig_gated(
        settings,
        render,
        input::QpcClock::start(),
        gate,
        |rig, _, _| {
            rig.adjust = None;
            f(rig)
        },
    )
    .0
}

fn sounds<'r>(rig: &'r mut PlayerRig<'_>) -> &'r mut SystemSounds {
    rig.system_sounds
        .as_mut()
        .expect("rig() made the system sounds")
}

/// An entry's re-roll on the rig, its wait ended by the signals the rig's
/// other waits read (what S10-24's call in `entered_selection` passes).
fn entry(rig: &mut PlayerRig<'_>) -> GenId {
    let PlayerRig {
        system_sounds,
        audio,
        controls,
        input,
        failures,
        ..
    } = rig;
    let controls: &crate::controls::Controls = controls;
    system_sounds
        .as_mut()
        .expect("rig() made the system sounds")
        .repick(&mut audio.control, || {
            crate::play::run_ends(controls, input, failures)
        })
}

/// Receives until the audio thread acknowledges `id`.
fn until_installed(rig: &mut PlayerRig<'_>, id: GenId) {
    let until = Instant::now() + Duration::from_secs(20);
    while rig.audio.control.system_installed() != Some(id) {
        assert!(Instant::now() < until, "{id:?} never installed");
        let _ = rig.receive_system_sounds();
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// What a test raises from beside the game thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    Quit,
    Close,
    Input,
    Output,
}

/// Raises `signal` on `rig`'s threads from another thread, `after` from
/// now; the instant it was raised.
fn raise_later<'a>(
    rig: &PlayerRig<'a>,
    signal: Signal,
    after: Duration,
) -> impl FnOnce() -> Instant + Send + use<'a> {
    let controls: &'a crate::controls::Controls = rig.controls;
    let fail_input = rig.input.failer();
    let lost = Arc::clone(&rig.audio_lost);
    move || {
        std::thread::sleep(after);
        match signal {
            Signal::Quit => controls.command(Command::Quit),
            Signal::Close => controls.close(),
            Signal::Input => fail_input(),
            Signal::Output => lost.store(true, Ordering::Release),
        }
        Instant::now()
    }
}

/// ST7e (c), I8: a close, an input failure or an audio output failure
/// during an entry's wait ends it at the next slice, admitting nothing and
/// counting no skip, with the current generation's id; `App::run`'s entry
/// then ends the run before selection's first look, with the signal's
/// cause, and selection never chooses. The test calls the rig's `repick`
/// just before `App::run`, standing in for the call S10-24 puts last in
/// `entered_selection`.
#[test]
fn a_signal_in_the_entrys_wait_ends_the_run_through_app_run() {
    for signal in [Signal::Close, Signal::Input, Signal::Output] {
        let fx = Fixture::new();
        fx.sound("sound/A", SystemKind::Clear, 10);
        fx.sound("sound/B", SystemKind::Clear, 12);
        let seam = Seam::new(Arc::new(RealClock));
        seam.rule("B/clear.wav", BLOCKED);
        let pick = Arc::new(AtomicUsize::new(0));
        arm_run(&fx, &seam, &pick, Some(MemFs::new()));
        let app_dir = tempfile::tempdir().unwrap();
        on_rig(None, |rig| {
            // A installed (the first entry takes the startup pick), then B
            // admitted at the second entry and blocked in its read.
            let a = entry(rig);
            until_installed(rig, a);
            pick.store(1, Ordering::Release);
            let b = entry(rig);
            assert_ne!(a, b, "{signal:?}");
            wait("B's read", || seam.opens("B/clear.wav") > 0);
            // The third entry waits for B's report; the signal comes
            // during that wait.
            let polls = sounds(rig).polls;
            let raise = raise_later(rig, signal, Duration::from_millis(100));
            let (ended, raised, returned) = std::thread::scope(|s| {
                let raiser = s.spawn(raise);
                let ended = entry(rig);
                let returned = Instant::now();
                (ended, raiser.join().unwrap(), returned)
            });
            assert!(
                returned.saturating_duration_since(raised) < LEAVES_WITHIN,
                "{signal:?}: {:?} after the signal",
                returned.saturating_duration_since(raised)
            );
            let s = sounds(rig);
            assert_eq!(ended, a, "{signal:?}: the current generation");
            assert_eq!(s.skipped_rerolls, 0, "{signal:?}");
            let admitted = s
                .events
                .iter()
                .filter(|e| matches!(e, Event::Admitted(_)))
                .count();
            assert_eq!(admitted, 2, "{signal:?}: the startup pick and B");
            assert!(s.entry_ended, "{signal:?}");
            let mut app = game::App::new(
                DataRoot::new(app_dir.path().to_owned()),
                game::Settings {
                    device: game::DeviceSettings::default(),
                    player: game::PlayerSettings::default(),
                },
                crate::session_config(replay::ReceiveMethod::Scripted, None),
                Box::new(|s: &game::Settings| game::Parts::unsaved(&s.player)),
                Box::new(game::NoSessionEvents),
            );
            let ran = app.run(rig);
            match signal {
                Signal::Input => {
                    assert!(matches!(ran, Err(LiveError::Input(_))), "{ran:?}");
                }
                Signal::Output => {
                    assert!(matches!(ran, Err(LiveError::Output(_))), "{ran:?}");
                }
                _ => assert!(ran.is_ok(), "{ran:?}"),
            }
            let s = sounds(rig);
            assert_eq!(s.polls, polls, "{signal:?}: selection chose");
            assert!(!s.entry_ended, "{signal:?}: the entry's end was not taken");
            seam.open_gate();
        });
    }
}

/// ST15, the settlement's signals: the quit key, a close, an input
/// failure or an audio output failure coming while the settlement waits
/// for the pick's report (its read blocked) or for the acknowledgement of
/// what it pushed (the audio thread held) makes `prepare_audio` answer
/// `Cancelled` within a slice; the pick is neither cancelled nor
/// abandoned, and the quit is left for the caller to take. A worker that
/// could not start settles at once.
#[test]
fn the_settlement_leaves_on_each_signal() {
    for report in [true, false] {
        for signal in [Signal::Quit, Signal::Close, Signal::Input, Signal::Output] {
            let case = format!(
                "{signal:?} while waiting for the {}",
                if report { "report" } else { "acknowledgement" }
            );
            let fx = Fixture::new();
            fx.sound("sound/A", SystemKind::Clear, 10);
            let seam = Seam::new(Arc::new(RealClock));
            if report {
                seam.rule("A/clear.wav", BLOCKED);
            }
            arm_run(&fx, &seam, &Arc::default(), Some(MemFs::new()));
            let gate = Arc::new(AudioGate::default());
            on_rig(Some(Arc::clone(&gate)), |rig| {
                rig.controls.quit_leaves();
                if report {
                    wait("A's read", || seam.opens("A/clear.wav") > 0);
                } else {
                    gate.hold();
                    wait("the audio thread held", || gate.stopped());
                    wait("A decoded", || seam.opens("A/clear.wav") > 0);
                }
                let raise = raise_later(rig, signal, Duration::from_millis(100));
                let (answer, raised, returned) = std::thread::scope(|s| {
                    let raiser = s.spawn(raise);
                    let answer = rig.prepare_audio();
                    let returned = Instant::now();
                    (answer, raiser.join().unwrap(), returned)
                });
                assert_eq!(answer.unwrap(), AudioReadiness::Cancelled, "{case}");
                assert!(
                    returned.saturating_duration_since(raised) < LEAVES_WITHIN,
                    "{case}: {:?} after the signal",
                    returned.saturating_duration_since(raised)
                );
                let s = sounds(rig);
                if report {
                    let flight = s.in_flight.as_ref().expect("the pick runs on");
                    assert!(!flight.cancel.load(Ordering::Acquire), "{case}");
                } else {
                    assert!(s.in_flight.is_none(), "{case}");
                    assert!(s.unacked.is_some(), "{case}: acknowledged");
                }
                assert!(!s.closed, "{case}");
                if signal == Signal::Quit {
                    assert!(rig.controls.leaving(), "{case}: the quit was taken");
                }
                gate.release();
                seam.open_gate();
            });
        }
    }
    // A worker that could not start: latched, nothing to wait for.
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    let root = fx.root();
    arm(move |setup, hooks| {
        setup.root = root;
        hooks.spawn = |_| Err(std::io::Error::other("a seamed spawn failure"));
    });
    on_rig(None, |rig| {
        let began = Instant::now();
        assert_eq!(rig.prepare_audio().unwrap(), AudioReadiness::Ready);
        assert!(began.elapsed() < LEAVES_WITHIN, "{:?}", began.elapsed());
        assert!(sounds(rig).latched.is_some());
    });
}

/// ST16, T18: a close while the pick's read is blocked past the stop
/// bound ends the run within the bound, the worker left to end with the
/// process and holding the recovery lock until it does. The marker it
/// committed stays while it runs: a launch reading the state as it is
/// then (the process ending here) skips that set, named, and decodes
/// nothing of it.
// Covers: T18
#[test]
fn a_close_abandons_a_blocked_pick() {
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    let seam = Seam::new(Arc::new(RealClock));
    seam.rule("A/clear.wav", BLOCKED);
    arm_run(&fx, &seam, &Arc::default(), None);
    let root = fx.root();
    let state = on_rig(None, |rig| {
        wait("A's read", || seam.opens("A/clear.wav") > 0);
        let state = std::fs::read(root.sound_sets()).unwrap();
        assert!(
            String::from_utf8_lossy(&state).contains("decoding"),
            "{}",
            String::from_utf8_lossy(&state)
        );
        let began = Instant::now();
        let mut sounds = rig.system_sounds.take().unwrap();
        sounds.close(&mut rig.audio.control);
        let took = began.elapsed();
        assert!(
            took >= STOP_WAIT && took < STOP_WAIT + LEAVES_WITHIN,
            "{took:?}"
        );
        drop(sounds);
        state
    });
    // The run ended: the worker still holds the lock while its read is
    // blocked.
    let mut fs = crate::sound_sets_disk::recovery_fs();
    assert!(
        !fs.lock(&root.sound_sets_lock()).unwrap(),
        "the lock was let go"
    );
    drop(fs);
    // A launch reading the state the process left: the same set, its
    // marker found.
    let next = Fixture::new();
    next.write("sound-sets.toml", &state);
    let said = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let lines = Arc::clone(&said);
    let mut setup = Setup::new(next.root(), game::SoundSettings::default(), 48_000);
    setup.sounds.sound_root = Some(fx.path("sound"));
    setup.decoder = Box::new(seam.clone());
    let hooks = Hooks {
        say: Box::new(move |line| lines.lock().unwrap().push(line.to_owned())),
        log: Box::new(|_| {}),
        clock: Arc::new(RealClock),
        spawn: super::spawn_worker,
    };
    let bank = Arc::new(audio::SoundBank::from_sounds(48_000, Vec::new()));
    let engine = audio::AudioEngine::new(bank, &[], 48_000, 480, 64, 64).unwrap();
    let (mut control, _worker) = game::audio_control(engine);
    let mut launch = SystemSounds::new(&mut control, setup, hooks);
    wait("the launch's pick", || {
        launch.poll(&mut control);
        launch.in_flight.is_none()
    });
    launch.close(&mut control);
    let said = said.lock().unwrap().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("is skipped: a launch ended while it was decoding")),
        "{said:?}"
    );
    assert_eq!(
        seam.opens("A/clear.wav"),
        1,
        "the quarantined set was read again"
    );
    // Once its read returns, the abandoned worker clears its marker and
    // ends, and the lock is free.
    seam.open_gate();
    wait("the abandoned worker's end", || {
        crate::sound_sets_disk::recovery_fs()
            .lock(&root.sound_sets_lock())
            .unwrap()
    });
}
