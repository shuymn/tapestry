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
use game::{AppRig as _, AudioReadiness, Command, DataRoot, LiveError, Rig as _};

use super::tests::{Fixture, MemFs, Rule, Seam, wait};
use super::{Event, Hooks, RealClock, Setup, SystemSounds, arm};
use crate::dev::AudioGate;
use crate::imports::STOP_WAIT;
use crate::play::PlayerRig;
use skin_model::ScreenKind;

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
            // A run selection's looks would not end is closed after 10 s,
            // so a missed entry's end fails below rather than hanging.
            let controls = rig.controls;
            let done = std::sync::atomic::AtomicBool::new(false);
            let ran = std::thread::scope(|s| {
                s.spawn(|| {
                    let until = Instant::now() + Duration::from_secs(10);
                    while !done.load(Ordering::Acquire) && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if !done.load(Ordering::Acquire) {
                        controls.close();
                    }
                });
                let ran = app.run(rig);
                done.store(true, Ordering::Release);
                ran
            });
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

/// The shutdown order: `shut_down` closes the system sounds before the
/// audio thread ends, so with a pick's read blocked the audio thread ends
/// only after the close's bounded wait for the worker, and what the game
/// thread held of theirs was dropped before it.
#[test]
fn the_shutdown_ends_the_system_sounds_before_the_audio_thread() {
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    let seam = Seam::new(Arc::new(RealClock));
    seam.rule("A/clear.wav", BLOCKED);
    arm_run(&fx, &seam, &Arc::default(), Some(MemFs::new()));
    let ended = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let theirs = Arc::clone(&ended);
    let silent = |rate, _| {
        (
            Arc::new(audio::SoundBank::from_sounds(rate, Vec::new())),
            Vec::new(),
        )
    };
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let render = crate::render_side::tests::Installers::default().spawn_bare();
    let (shut_from, _) = crate::tests::on_rig_playing(
        settings,
        render,
        input::QpcClock::start(),
        None,
        (silent, 1),
        move |_| *theirs.lock().unwrap() = Some(Instant::now()),
        |rig, _, _| {
            wait("A's read", || seam.opens("A/clear.wav") > 0);
            assert!(sounds(rig).pick_running().is_some());
            Instant::now()
        },
    );
    let ended = ended.lock().unwrap().expect("the audio thread ended");
    let after = ended.saturating_duration_since(shut_from);
    assert!(
        after >= STOP_WAIT,
        "the audio thread ended {after:?} into the shutdown"
    );
    seam.open_gate();
}

/// The select screen of `screens` on a rig whose startup pick refuses
/// a file, its report held until the screen is up: `f` with the rig and
/// the refusal's line, received and queued as the receipt at the head of
/// a look's `choose` leaves it, before any look took it.
fn with_a_named_refusal<R>(
    screens: &crate::render_side::tests::Installers,
    render: crate::render_side::RenderLink,
    f: impl FnOnce(&mut PlayerRig<'_>, String) -> R,
) -> R {
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    fx.write("sound/A/scratch.wav", b"not a sound");
    let seam = Seam::new(Arc::new(RealClock));
    // The report waits until the select screen is up.
    seam.rule("A/clear.wav", BLOCKED);
    arm_run(&fx, &seam, &Arc::default(), Some(MemFs::new()));
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    crate::selection::tests::on_selection(sel, screens, render, |rig| {
        // The cursor onto the chart; nothing named yet.
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        seam.open_gate();
        wait("the refusal named", || {
            rig.system_sounds
                .as_mut()
                .expect("rig() made the system sounds")
                .poll(&mut rig.audio.control);
            !rig.system_notices.lock().unwrap().is_empty()
        });
        let named = rig.system_notices.lock().unwrap().clone();
        assert!(
            named
                .iter()
                .any(|l| l.contains("scratch.wav") && l.contains("was refused")),
            "{named:?}"
        );
        f(rig, named.join(" / "))
    })
}

/// A look on `rig` that chooses nothing: the notice of the page it
/// publishes, once a selection frame of `screens` applied that page.
fn notice_of_the_look(
    rig: &mut PlayerRig<'_>,
    screens: &crate::render_side::tests::Installers,
) -> Option<String> {
    assert!(rig.choose().is_none());
    let published = rig
        .selection
        .as_ref()
        .expect("on the select screen")
        .feed
        .published();
    let page = || {
        screens
            .updates
            .lock()
            .unwrap()
            .iter()
            .find(|u| (u.lifetime, u.revision) == published)
            .cloned()
    };
    assert!(
        crate::render_side::tests::wait_until(|| page().is_some()),
        "{published:?} never drawn"
    );
    page().and_then(|u| u.strip.notice)
}

/// T18, design.md "Audio" (Loading): a file the pick refused, named
/// before the look whose Enter chooses a chart, is not lost with that
/// look, which sends no page: the first look on the select screen
/// returned to that chooses nothing (`entered_selection` publishes a page
/// of its own before it) shows it on its page's notice line, and the
/// next look's page shows it again, not twice over.
// Covers: T18
#[test]
fn a_refusal_named_before_a_choice_shows_on_the_return() {
    use crate::render_side::tests::Installers;
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let screens = Installers::default();
    let render = screens.spawn_bare();
    with_a_named_refusal(&screens, render, |rig, line| {
        // The look that chooses the chart.
        press(rig, MenuKey::Enter);
        assert!(
            matches!(rig.choose(), Some(game::Pick::Play(_))),
            "nothing chosen"
        );
        rig.entered_selection();
        assert_eq!(notice_of_the_look(rig, &screens), Some(line.clone()));
        assert!(rig.system_notices.lock().unwrap().is_empty());
        rig.selection.as_mut().unwrap().dirty = true;
        assert_eq!(notice_of_the_look(rig, &screens), Some(line));
    });
}

/// T18, design.md "Screen skins": a skin refused at runtime and a file
/// the pick refused, both taken by one look that chooses nothing, are
/// both on its page's notice line, the skin's reason first; neither
/// replaces the other, as each is taken once.
fn a_skin_refusal_and_a_named_file_share_the_page(
    kind: ScreenKind,
    refuse: impl FnOnce(&crate::render_side::tests::Installers, &PlayerRig<'_>),
    why: &str,
) {
    use crate::render_side::tests::{Installers, bundle};
    let screens = Installers::default();
    let render = screens.spawn_bare();
    assert_eq!(
        render.select(kind, Some(bundle())).wait().unwrap(),
        game::Preparation::Ready
    );
    with_a_named_refusal(&screens, render, |rig, line| {
        refuse(&screens, rig);
        let refused = crate::imports::notice(kind, why);
        assert_eq!(
            notice_of_the_look(rig, &screens),
            Some(format!("{refused} / {line}")),
            "{kind:?}"
        );
        assert!(rig.system_notices.lock().unwrap().is_empty());
    });
}

/// The selection skin refused by a selection frame.
#[test]
fn a_selection_refusal_and_a_named_file_share_the_page() {
    a_skin_refusal_and_a_named_file_share_the_page(
        ScreenKind::Selection,
        |screens, _| refuse_selection(screens),
        SELECTION_REFUSED,
    );
}

/// The decision skin refused by the restore of a screen built again on a
/// healthy device.
#[test]
fn a_decision_refusal_and_a_named_file_share_the_page() {
    a_skin_refusal_and_a_named_file_share_the_page(
        ScreenKind::Decision,
        |screens, rig| {
            refuse_decision(screens, rig);
        },
        &decision_refused(),
    );
}

/// design.md "Play Lifecycle" (Selection), "Screen skins": a look whose
/// Enter the play skin refuses shows the refusal and the file the pick
/// refused, named before it, both on its page's notice line in the order
/// they were set; the next look's refusal then replaces what that look
/// set, as a page showed it.
#[test]
fn an_admission_refusal_and_a_named_file_share_the_page() {
    use crate::render_side::SkinState;
    use crate::render_side::tests::Installers;
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let screens = Installers::default();
    let render = screens.spawn_bare();
    with_a_named_refusal(&screens, render, |rig, line| {
        let refused = "the play skin was refused for the test";
        rig.skin = SkinState::Refused(refused.to_owned());
        press(rig, MenuKey::Enter);
        assert_eq!(
            notice_of_the_look(rig, &screens),
            Some(format!("{refused} / {line}"))
        );
        assert!(rig.system_notices.lock().unwrap().is_empty());
        // An earlier look's notice gives way to the next look's.
        let again = "the play skin was refused again";
        rig.skin = SkinState::Refused(again.to_owned());
        press(rig, MenuKey::Enter);
        assert_eq!(notice_of_the_look(rig, &screens), Some(again.to_owned()));
    });
}

/// design.md "Screen skins": the selection skin and the decision skin,
/// both refused before one look that chooses nothing, and a file the pick
/// refused: all three on its page's notice line, in the order the look
/// took them.
#[test]
fn a_selection_and_a_decision_refusal_share_the_page() {
    use crate::render_side::tests::{Installers, bundle};
    let screens = Installers::default();
    let render = screens.spawn_bare();
    for kind in [ScreenKind::Selection, ScreenKind::Decision] {
        assert_eq!(
            render.select(kind, Some(bundle())).wait().unwrap(),
            game::Preparation::Ready
        );
    }
    with_a_named_refusal(&screens, render, |rig, line| {
        refuse_selection(&screens);
        let why = refuse_decision(&screens, rig);
        let selection = crate::imports::notice(ScreenKind::Selection, SELECTION_REFUSED);
        let decision = crate::imports::notice(ScreenKind::Decision, &why);
        assert_eq!(
            notice_of_the_look(rig, &screens),
            Some(format!("{selection} / {decision} / {line}"))
        );
        assert!(rig.system_notices.lock().unwrap().is_empty());
    });
}

/// design.md "Screen skins": a selection skin refused before a look whose
/// Enter chooses a chart, which sends no page, and a file the pick refused,
/// named before it: both on the page of the first look back on the select
/// screen that chooses nothing, the refusal first; neither the choice nor
/// that look's line replaces the refusal.
#[test]
fn a_refusal_taken_by_a_choice_shows_on_the_return() {
    use crate::render_side::tests::{Installers, bundle};
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let screens = Installers::default();
    let render = screens.spawn_bare();
    assert_eq!(
        render
            .select(ScreenKind::Selection, Some(bundle()))
            .wait()
            .unwrap(),
        game::Preparation::Ready
    );
    with_a_named_refusal(&screens, render, |rig, line| {
        refuse_selection(&screens);
        press(rig, MenuKey::Enter);
        assert!(
            matches!(rig.choose(), Some(game::Pick::Play(_))),
            "nothing chosen"
        );
        rig.entered_selection();
        let refused = crate::imports::notice(ScreenKind::Selection, SELECTION_REFUSED);
        assert_eq!(
            notice_of_the_look(rig, &screens),
            Some(format!("{refused} / {line}"))
        );
        assert!(rig.system_notices.lock().unwrap().is_empty());
    });
}

/// Why a selection frame of the tests' screens refuses the selection skin.
const SELECTION_REFUSED: &str = "refused for the test";

/// The selection skin refused by a selection frame of `screens`.
fn refuse_selection(screens: &crate::render_side::tests::Installers) {
    screens.refuse_selection.store(true, Ordering::Release);
    assert!(crate::render_side::tests::wait_until(|| {
        screens
            .selections
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.state.is_none())
    }));
}

/// The decision skin refused by the restore of a screen built again on a
/// healthy device; the reason it was refused for.
fn refuse_decision(screens: &crate::render_side::tests::Installers, rig: &PlayerRig<'_>) -> String {
    *screens.refuse_restore.lock().unwrap() = Some(ScreenKind::Decision);
    screens.fail.store(true, Ordering::Release);
    rig.render
        .send(crate::render_side::RenderMsg::Menu(Box::default()));
    assert!(crate::render_side::tests::wait_until(|| rig
        .render
        .failed()));
    screens.fail.store(false, Ordering::Release);
    rig.render.prepare().unwrap();
    *screens.refuse_restore.lock().unwrap() = None;
    decision_refused()
}

/// Why the restore in [`refuse_decision`] refuses the decision skin.
fn decision_refused() -> String {
    let admission = render::RenderError::Admission {
        kind: ScreenKind::Decision,
        needed: render::SHARED_TEXELS + 1,
        limit: render::SHARED_TEXELS,
    };
    format!(
        "the decision skin was not restored on the screen built again: {}",
        game::Chain(&admission)
    )
}
