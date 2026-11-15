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
    on_rig_with(game::SoundSettings::default(), gate, f)
}

/// [`on_rig`] with `player.toml`'s `[sounds]` as `sounds` at startup.
fn on_rig_with<R>(
    sounds: game::SoundSettings,
    gate: Option<Arc<AudioGate>>,
    f: impl FnOnce(&mut PlayerRig<'_>) -> R,
) -> R {
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings {
            sounds,
            ..game::PlayerSettings::default()
        },
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
/// other waits read (what `entered_selection` passes), with no loop
/// started.
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
/// counting no skip, the current generation still the newest; `App::run`'s
/// entry then ends the run before selection's first look, with the
/// signal's cause, and selection never chooses. Both waits are
/// `entered_selection`'s, and C24-1: an entry whose wait ended starts no
/// `select`.
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
            // The third entry, selection's own, waits for B's report; the
            // signal comes during that wait.
            let polls = sounds(rig).polls;
            let probe = rig.audio.control.failure_probe();
            let loops = probe.system_starts(SystemKind::Select);
            let raise = raise_later(rig, signal, Duration::from_millis(100));
            let (raised, returned) = std::thread::scope(|s| {
                let raiser = s.spawn(raise);
                rig.entered_selection();
                let returned = Instant::now();
                (raiser.join().unwrap(), returned)
            });
            assert!(
                returned.saturating_duration_since(raised) < LEAVES_WITHIN,
                "{signal:?}: {:?} after the signal",
                returned.saturating_duration_since(raised)
            );
            let s = sounds(rig);
            assert_eq!(s.newest, a, "{signal:?}: the current generation");
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
            assert_eq!(
                probe.system_starts(SystemKind::Select),
                loops,
                "{signal:?}: an ended entry's loop"
            );
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
/// thread held of theirs was dropped before it. C24-4: the close stops
/// every set sound first: with `select` looping and `decide` sounding,
/// the shutdown's own stops (counted from just before it, after every
/// earlier stop) are one of each, numbered above both starts.
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

    // The close's stops.
    let _fx = arm_bgm(&Seam::new(Arc::new(RealClock)), &Arc::default());
    let ((probe, before, numbered), _) =
        crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
            let dir = tempfile::tempdir().unwrap();
            one_chart(rig, dir.path());
            let probe = rig.audio.control.failure_probe();
            let a = installed_past(rig, None);
            rig.entered_selection();
            rig.system_sounds.as_mut().unwrap().start(
                &mut rig.audio.control,
                SystemKind::Decide,
                None,
            );
            wait("both sounding", || {
                probe.reserved(SystemKind::Select) == Some((a, true))
                    && probe.reserved(SystemKind::Decide) == Some((a, false))
            });
            // Every start so far is numbered below this.
            let numbered = rig.audio.control.system_seq_word();
            let before = [SystemKind::Select, SystemKind::Decide].map(|k| probe.system_stops(k));
            (probe, before, numbered)
        });
    let kinds = [SystemKind::Select, SystemKind::Decide];
    assert_eq!(
        kinds.map(|k| probe.system_stops(k)),
        before.map(|n| n + 1),
        "the shutdown's stops"
    );
    for kind in kinds {
        assert!(
            probe.system_stop_word(kind) >= numbered,
            "{kind:?}'s stop is not above its start"
        );
    }
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
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    with_a_held_refusal(screens, render, sel, |rig, name| {
        // The cursor onto the chart; nothing named yet.
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        let line = name(rig);
        f(rig, line)
    })
}

/// `sel` on the select screen of `screens`, on a rig whose startup pick
/// refuses a file, its report held: `f` with the rig and `name`, which
/// lets the report in and gives the refusal's line once it is received
/// and queued as the receipt at the head of a look's `choose` leaves it,
/// before any look took it.
fn with_a_held_refusal<R>(
    screens: &crate::render_side::tests::Installers,
    render: crate::render_side::RenderLink,
    sel: crate::selection::Selection,
    f: impl FnOnce(&mut PlayerRig<'_>, &dyn Fn(&mut PlayerRig<'_>) -> String) -> R,
) -> R {
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    fx.write("sound/A/scratch.wav", b"not a sound");
    let seam = Seam::new(Arc::new(RealClock));
    // The report waits until `name`.
    seam.rule("A/clear.wav", BLOCKED);
    arm_run(&fx, &seam, &Arc::default(), Some(MemFs::new()));
    let name = |rig: &mut PlayerRig<'_>| {
        seam.open_gate();
        wait("the refusal named", || {
            rig.poll_system_sounds();
            !rig.system_notices.lock().unwrap().is_empty()
        });
        let named = rig.system_notices.lock().unwrap().clone();
        assert!(
            named
                .iter()
                .any(|l| l.contains("scratch.wav") && l.contains("was refused")),
            "{named:?}"
        );
        named.join(" / ")
    };
    crate::selection::tests::on_selection(sel, screens, render, |rig| f(rig, &name))
}

/// A look on `rig` that chooses nothing: the notice of the page it
/// publishes, once a selection frame of `screens` applied that page.
fn notice_of_the_look(
    rig: &mut PlayerRig<'_>,
    screens: &crate::render_side::tests::Installers,
) -> Option<String> {
    assert!(rig.choose().is_none());
    published_notice(rig, screens)
}

/// The notice of the page `rig` published last, once a selection frame
/// of `screens` applied that page.
fn published_notice(
    rig: &PlayerRig<'_>,
    screens: &crate::render_side::tests::Installers,
) -> Option<String> {
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

/// design.md "Screen skins": a skin refused at startup is on the entry's
/// page, which is no look's; a look that publishes no page leaves it for
/// the next look's page, where a file the pick refused, named after that
/// look, joins it rather than replacing it.
#[test]
fn a_look_that_publishes_nothing_keeps_the_notices() {
    use crate::render_side::tests::Installers;
    let screens = Installers::default();
    let render = screens.spawn_bare();
    let dir = tempfile::tempdir().unwrap();
    // No pass runs again: a look with no input publishes nothing.
    let (mut sel, _) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    sel.shelf.set_rescan_after(Duration::from_secs(3_600));
    let refused = crate::imports::notice(ScreenKind::Selection, "refused at startup for the test");
    sel.started(vec![refused.clone()], None);
    with_a_held_refusal(&screens, render, sel, |rig, name| {
        assert_eq!(published_notice(rig, &screens), Some(refused.clone()));
        let entered = rig.selection.as_ref().unwrap().feed.published();
        assert!(rig.choose().is_none());
        let quiet = rig.selection.as_ref().unwrap().feed.published();
        assert_eq!(quiet, entered, "the quiet look published a page");
        let line = name(rig);
        assert_eq!(
            notice_of_the_look(rig, &screens),
            Some(format!("{refused} / {line}"))
        );
        assert_ne!(
            rig.selection.as_ref().unwrap().feed.published(),
            quiet,
            "the look after the line published no page"
        );
    });
}

/// design.md "Screen skins", "Play Lifecycle" (Selection): a chart
/// refused because the shelf would not open, its refusal on no look's page
/// yet, and the shelf opened by a preset step taken in the look that
/// chooses a chart: back on the select screen, with no other shelf change,
/// the first look's page shows the refusal and the next look's page has
/// none.
#[test]
fn a_preset_refusal_goes_after_the_page_that_showed_it() {
    use crate::render_side::tests::Installers;
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let screens = Installers::default();
    let render = screens.spawn_bare();
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, roots) = crate::selection::tests::tree(dir.path(), &[&["a.bms"]]);
    // No pass runs again: a look with no input publishes nothing.
    sel.shelf.set_rescan_after(Duration::from_secs(3_600));
    let presets = dir.path().join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let broken = || game::Shelf::open(presets.clone(), "missing").unwrap_err();
    crate::selection::tests::on_selection(sel, &screens, render, |rig| {
        let published = |rig: &PlayerRig<'_>| rig.selection.as_ref().unwrap().feed.published();
        // Back from the refused load, as the flow reports it.
        rig.presets_changed(Err(&broken()));
        rig.show_refusal(&game::LoadRefusal {
            chart: roots[0].join("a.bms"),
            reason: game::Refused::Chart(game::SessionError::Preset(broken())),
        });
        let refused = published_notice(rig, &screens).expect("the refusal shows");
        assert!(refused.contains("a.bms"), "{refused}");
        // F12, then Enter, taken by one look: the flow opens the shelf
        // with the step and reports it before the chart's load.
        press(rig, MenuKey::NextPreset);
        press(rig, MenuKey::Enter);
        assert!(
            matches!(rig.choose(), Some(game::Pick::Play(_))),
            "nothing chosen"
        );
        while rig.take_preset_step().is_some() {}
        let opened = game::Shelf::open(presets.clone(), rig.files.preset()).unwrap();
        rig.presets_changed(Ok(&opened));
        // Back on the select screen, the shelf reported at the entry.
        rig.entered_selection();
        rig.presets_changed(Ok(&opened));
        let entered = published(rig);
        let first = notice_of_the_look(rig, &screens);
        let shown = published(rig);
        let next = notice_of_the_look(rig, &screens);
        assert_eq!(first.as_deref(), Some(refused.as_str()));
        assert_eq!(next, None);
        assert_ne!(shown, entered, "the first look published no page");
        assert_ne!(published(rig), shown, "the next look published no page");
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
    crate::render_side::tests::rebuild(screens, &rig.render).unwrap();
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

/// Arms the next rig's owner with a BGM set `bgm/A` whose `decide` sounds
/// for 1 s (48 000 frames), its recovery state in `fs`; the set's root
/// lives as long as the returned fixture.
fn arm_decide(fs: MemFs) -> Fixture {
    let fx = Fixture::new();
    fx.sound("bgm/A", SystemKind::Select, 96_000);
    fx.sound("bgm/A", SystemKind::Decide, 48_000);
    arm_run(
        &fx,
        &Seam::new(Arc::new(RealClock)),
        &Arc::default(),
        Some(fs),
    );
    fx
}

/// The select screen's selection over a library of one chart, on `rig`:
/// the chart.
fn one_chart(rig: &mut PlayerRig<'_>, dir: &std::path::Path) -> std::path::PathBuf {
    let (sel, roots) = crate::selection::tests::tree(dir, &[&["a.bms"]]);
    rig.selection = Some(sel);
    roots[0].join("a.bms")
}

/// C25-3, T18: a choice while the run is latched (the recovery state's
/// lock held by another instance) asks for the tone and queues no
/// decision sound.
#[test]
fn a_latched_choice_sounds_the_tone() {
    let fs = MemFs::new();
    fs.hold_lock();
    let _fx = arm_decide(fs);
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        let chart = one_chart(rig, dir.path());
        let probe = rig.audio.control.failure_probe();
        wait("the latch", || {
            rig.poll_system_sounds();
            sounds(rig).latched.is_some()
        });
        // Positive controls: a start queued and a tone asked for move
        // their counts.
        rig.audio
            .control
            .system_start(SystemKind::Scratch, false, None)
            .unwrap();
        assert_eq!(probe.system_starts(SystemKind::Scratch), 1);
        rig.audio.control.play_tone();
        assert_eq!(probe.tone_requests(), 1);
        let _ = rig.entered_decision(&game::Pick::Play(chart), game::Decision::off());
        assert_eq!(
            probe.system_starts(SystemKind::Decide),
            0,
            "a decision sound queued"
        );
        assert_eq!(probe.tone_requests(), 2, "the choice's tone");
    });
}

/// C25-4: the decision screen ending for loading stops no decision
/// sound: the set's `decide`, sounding in its reserved voice, has no stop
/// drawn for it through the decision screen's entry and loading's.
// Source: SF-009
#[test]
fn decide_plays_on_into_loading() {
    let _fx = arm_decide(MemFs::new());
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        let chart = one_chart(rig, dir.path());
        let probe = rig.audio.control.failure_probe();
        wait("the set installed", || {
            rig.poll_system_sounds();
            rig.audio.control.system_installed().is_some()
        });
        let a = rig.audio.control.system_installed().unwrap();
        // Positive control: a stop drawn moves its kind's count.
        rig.audio.control.system_stop(SystemKind::Scratch);
        assert_eq!(probe.system_stops(SystemKind::Scratch), 1);
        let (stops, word) = (
            probe.system_stops(SystemKind::Decide),
            probe.system_stop_word(SystemKind::Decide),
        );
        let _ = rig.entered_decision(&game::Pick::Play(chart.clone()), game::Decision::off());
        assert_eq!(probe.system_starts(SystemKind::Decide), 1);
        wait("the decision sound", || {
            probe.reserved(SystemKind::Decide).is_some()
        });
        assert_eq!(probe.reserved(SystemKind::Decide), Some((a, false)));
        rig.entered_loading(&chart);
        assert_eq!(probe.system_stops(SystemKind::Decide), stops);
        assert_eq!(probe.system_stop_word(SystemKind::Decide), word);
    });
}

/// BGM sets A and B, each with a `select` longer than any test's look at
/// it (A 2 s, B 1.5 s) and A with a 10 s `decide`, armed for the next rig
/// with `seam`'s decoder and `pick`; the sets' root lives as long as the
/// returned fixture.
fn arm_bgm(seam: &Seam, pick: &Arc<AtomicUsize>) -> Fixture {
    let fx = Fixture::new();
    fx.sound("bgm/A", SystemKind::Select, 96_000);
    fx.sound("bgm/A", SystemKind::Decide, 480_000);
    fx.sound("bgm/B", SystemKind::Select, 72_000);
    arm_run(&fx, seam, pick, Some(MemFs::new()));
    fx
}

/// Receives until the audio thread acknowledges a generation other than
/// `not`, and returns it.
fn installed_past(rig: &mut PlayerRig<'_>, not: Option<GenId>) -> GenId {
    wait("an install", || {
        rig.poll_system_sounds();
        rig.audio
            .control
            .system_installed()
            .is_some_and(|g| Some(g) != not)
    });
    rig.audio.control.system_installed().unwrap()
}

/// Every kind's stop count on `probe`.
fn stops(probe: &game::OutputProbe) -> [u64; audio::SYSTEM_KINDS] {
    SystemKind::ALL.map(|k| probe.system_stops(k))
}

/// C24-1 (design.md "State Model", system sounds, Selection): each entry
/// stops every set sound, picks again and loops `select` from that pick:
/// the first entry's from the startup pick A, the next entry's, after a
/// choice, from B once it installs.
// Source: SF-011
#[test]
fn each_entry_stops_rerolls_and_loops_its_pick() {
    let pick = Arc::new(AtomicUsize::new(0));
    let _fx = arm_bgm(&Seam::new(Arc::new(RealClock)), &pick);
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        let chart = one_chart(rig, dir.path());
        let probe = rig.audio.control.failure_probe();
        let a = installed_past(rig, None);
        // Positive control: a stop drawn moves its kind's count.
        rig.audio.control.system_stop(SystemKind::Scratch);
        assert_eq!(probe.system_stops(SystemKind::Scratch), 1);
        let before = stops(&probe);
        rig.entered_selection();
        assert_eq!(stops(&probe), before.map(|n| n + 1), "every kind stopped");
        assert_eq!(probe.system_starts(SystemKind::Select), 1);
        wait("A's loop", || probe.reserved(SystemKind::Select).is_some());
        assert_eq!(probe.reserved(SystemKind::Select), Some((a, true)));
        pick.store(1, Ordering::Release);
        let _ = rig.entered_decision(&game::Pick::Play(chart), game::Decision::off());
        wait("the loop's stop", || {
            probe.reserved(SystemKind::Select).is_none()
        });
        let before = stops(&probe);
        rig.entered_selection();
        assert_eq!(stops(&probe), before.map(|n| n + 1), "every kind stopped");
        assert_eq!(probe.system_starts(SystemKind::Select), 2);
        let b = installed_past(rig, Some(a));
        assert!(b > a, "B picked at the entry");
        wait("B's loop", || probe.reserved(SystemKind::Select).is_some());
        assert_eq!(probe.reserved(SystemKind::Select), Some((b, true)));
    });
}

/// C24-3 (design.md "State Model", system sounds, Selection): leaving
/// selection for the decision screen, the practice page or loading stops
/// `select` at once, a loop still waiting for its generation included:
/// with B's read blocked at the entry, the loop bound to B never sounds
/// once B installs after the way out.
// Source: SF-011
#[test]
fn leaving_selection_stops_the_loop() {
    for way in ["decision", "practice", "loading"] {
        let seam = Seam::new(Arc::new(RealClock));
        let pick = Arc::new(AtomicUsize::new(0));
        let _fx = arm_bgm(&seam, &pick);
        on_rig(None, |rig| {
            let dir = tempfile::tempdir().unwrap();
            let chart = one_chart(rig, dir.path());
            let probe = rig.audio.control.failure_probe();
            let a = installed_past(rig, None);
            rig.entered_selection();
            wait("A's loop", || probe.reserved(SystemKind::Select).is_some());
            // B admitted at the next entry, its read blocked.
            seam.rule("B/select.wav", BLOCKED);
            pick.store(1, Ordering::Release);
            rig.entered_loading(&chart);
            rig.entered_selection();
            wait("B's read", || seam.opens("B/select.wav") > 0);
            assert_eq!(probe.system_starts(SystemKind::Select), 2, "{way}");
            let stopped = probe.system_stops(SystemKind::Select);
            match way {
                "decision" => {
                    let _ = rig.entered_decision(&game::Pick::Play(chart), game::Decision::off());
                }
                "practice" => {
                    let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
                    rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
                }
                _ => rig.entered_loading(&chart),
            }
            assert_eq!(probe.system_stops(SystemKind::Select), stopped + 1, "{way}");
            seam.open_gate();
            let b = installed_past(rig, Some(a));
            // Long enough for the control loop to have started a loop
            // still waiting: it would start at the install's own step.
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(rig.audio.control.system_installed(), Some(b), "{way}");
            assert_eq!(probe.reserved(SystemKind::Select), None, "{way}");
        });
    }
}

/// C24-1, the latch (design.md "Audio", tone-only recovery): a latched
/// run's entry queues no `select`; the owner refuses the start and counts
/// it.
#[test]
fn a_latched_entry_starts_no_loop() {
    let fs = MemFs::new();
    fs.hold_lock();
    let fx = Fixture::new();
    fx.sound("bgm/A", SystemKind::Select, 96_000);
    arm_run(
        &fx,
        &Seam::new(Arc::new(RealClock)),
        &Arc::default(),
        Some(fs),
    );
    on_rig(None, |rig| {
        let probe = rig.audio.control.failure_probe();
        wait("the latch", || {
            rig.poll_system_sounds();
            sounds(rig).latched.is_some()
        });
        // Positive control: a start queued moves its count.
        rig.audio
            .control
            .system_start(SystemKind::Select, false, None)
            .unwrap();
        assert_eq!(probe.system_starts(SystemKind::Select), 1);
        let refused = sounds(rig).refused;
        rig.entered_selection();
        assert_eq!(probe.system_starts(SystemKind::Select), 1, "a loop queued");
        assert_eq!(sounds(rig).refused, refused + 1, "the refused start");
    });
}

/// C24-9 (design.md "State Model", system sounds, Selection): selection
/// recovered on a screen built again goes on with its entry: nothing is
/// stopped, picked or started, and the loop plays on.
#[test]
fn a_recovered_selection_keeps_its_loop() {
    let _fx = arm_bgm(&Seam::new(Arc::new(RealClock)), &Arc::default());
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        one_chart(rig, dir.path());
        let probe = rig.audio.control.failure_probe();
        let a = installed_past(rig, None);
        rig.entered_selection();
        wait("A's loop", || probe.reserved(SystemKind::Select).is_some());
        let admitted = |rig: &mut PlayerRig<'_>| {
            sounds(rig)
                .events
                .iter()
                .filter(|e| matches!(e, Event::Admitted(_)))
                .count()
        };
        let before = (
            stops(&probe),
            probe.system_starts(SystemKind::Select),
            admitted(rig),
        );
        rig.recovered_selection();
        let after = (
            stops(&probe),
            probe.system_starts(SystemKind::Select),
            admitted(rig),
        );
        assert_eq!(after, before, "a stop, a start or a re-roll");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(probe.reserved(SystemKind::Select), Some((a, true)));
    });
}

/// C24-1, design.md "Audio" (Mixing): the decision sound plays on into
/// loading, and the next entry to selection stops it with every set sound.
#[test]
fn the_next_entry_stops_decide() {
    let _fx = arm_bgm(&Seam::new(Arc::new(RealClock)), &Arc::default());
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        let chart = one_chart(rig, dir.path());
        let probe = rig.audio.control.failure_probe();
        let a = installed_past(rig, None);
        rig.entered_selection();
        let _ = rig.entered_decision(&game::Pick::Play(chart.clone()), game::Decision::off());
        rig.entered_loading(&chart);
        wait("the decision sound", || {
            probe.reserved(SystemKind::Decide).is_some()
        });
        assert_eq!(probe.reserved(SystemKind::Decide), Some((a, false)));
        let stopped = probe.system_stops(SystemKind::Decide);
        rig.entered_selection();
        assert_eq!(probe.system_stops(SystemKind::Decide), stopped + 1);
        wait("its stop", || probe.reserved(SystemKind::Decide).is_none());
    });
}

/// C24-5, C24-6, C24-8 through the rig's look: the keys a look drains ask
/// for their cues, and the look starts each once, from the select screen's
/// input to the audio thread's requests; a look with no input starts none.
// Source: SF-012
#[test]
fn a_look_requests_its_cues() {
    use crate::select::MenuKey;
    use crate::selection::image_tests::press;
    let screens = crate::render_side::tests::Installers::default();
    let render = screens.spawn_bare();
    let dir = tempfile::tempdir().unwrap();
    let (sel, _) = crate::selection::tests::tree(
        dir.path(),
        &[&["c1.bms", "c2.bms", "c3.bms", "c4.bms", "c5.bms"]],
    );
    crate::selection::tests::on_selection(sel, &screens, render, |rig| {
        let probe = rig.audio.control.failure_probe();
        let kinds = [
            SystemKind::Scratch,
            SystemKind::FolderOpen,
            SystemKind::FolderClose,
            SystemKind::OptionChange,
        ];
        let starts = |p: &game::OutputProbe| kinds.map(|k| p.system_starts(k));
        // Positive control: a start queued moves its count.
        rig.audio
            .control
            .system_start(SystemKind::FolderClose, false, None)
            .unwrap();
        assert_eq!(probe.system_starts(SystemKind::FolderClose), 1);
        let before = starts(&probe);
        for key in [
            MenuKey::Enter,
            MenuKey::Down,
            MenuKey::Down,
            MenuKey::Down,
            MenuKey::Right,
            MenuKey::NextPreset,
        ] {
            press(rig, key);
        }
        assert!(rig.choose().is_none());
        let after = starts(&probe);
        let took: Vec<u64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
        assert_eq!(took, [3, 1, 0, 2], "scratch, f-open, f-close, o-change");
        assert!(rig.choose().is_none());
        assert_eq!(starts(&probe), after, "an empty look");
    });
}

/// A sound set with a 1.25 s `clear` and a 0.83 s `fail`, longer than
/// any look at them, armed for the next rig; the set's root lives as long
/// as the returned fixture.
fn arm_result() -> Fixture {
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 60_000);
    fx.sound("sound/A", SystemKind::Fail, 40_000);
    arm_run(
        &fx,
        &Seam::new(Arc::new(RealClock)),
        &Arc::default(),
        Some(MemFs::new()),
    );
    fx
}

/// A chart of one tap at 1 s, which a record of no input misses.
fn one_tap() -> bms_model::Chart {
    bms_model::ChartBuilder::new(150_000)
        .tap(
            bms_model::Lane::new(1).unwrap(),
            bms_model::TimeUs(1_000_000),
        )
        .build()
        .unwrap()
}

/// A completed play's report under the tapestry preset whose own view's
/// lamp is cleared when `own` is (its gauges cleared) and failed
/// otherwise (an evaluation that did not complete), recorded to its end
/// with no input of [`one_tap`]; its result screen stepped to a user
/// preset's view, re-evaluated from the record, whose lamp differs: for an
/// own clear one whose gauges start empty, take no change and clear only
/// full, and otherwise one whose gauges start full and take no damage.
fn switched_result(own: bool, dir: &std::path::Path) -> (game::PlayReport, game::ResultScreen) {
    let chart = one_tap();
    let presets = dir.join("presets");
    std::fs::create_dir_all(&presets).unwrap();
    let mut other = rules::tapestry();
    "other".clone_into(&mut other.id);
    let none = rules::GaugeDelta::Fixed(rules::GaugeValue::EMPTY);
    for g in &mut other.gauges {
        if own {
            g.kind = rules::GaugeKind::EndThreshold { clear: g.max };
            g.start = g.min;
        } else {
            g.start = g.max;
        }
        g.change = rules::PerJudgment {
            perfect: none,
            great: none,
            good: none,
            bad: none,
            poor: none,
        };
        g.empty_poor = none;
        g.low_damage.clear();
    }
    std::fs::write(presets.join("other.toml"), game::preset_to_toml(&other)).unwrap();
    let mut report = crate::end_screen::tests::report(rules::tapestry());
    let evaluation = report
        .result
        .as_mut()
        .and_then(|r| r.evaluation.as_mut().ok())
        .expect("an own evaluation");
    evaluation.completed = own;
    for g in &mut evaluation.gauges {
        g.cleared = own;
    }
    let rec = report.record.as_mut().expect("a record");
    rec.status.end = replay::EndStatus::PlayedToEnd;
    rec.status.finalized = bms_model::TimeUs(10_000_000);
    let shelf = game::Shelf::open(presets, "tapestry").unwrap();
    let mut screen = game::ResultScreen::new(&report, &shelf, &chart, game::RECORDING_MEMORY);
    assert_eq!(screen.own_cleared(), Some(own), "the own view's lamp");
    // The user preset lists last: one back from the played one.
    screen.step(-1, &report, &chart);
    assert_eq!(viewed_cleared(&screen), !own, "the viewed lamp differs");
    assert_ne!(
        screen.viewed().and_then(|e| e.hash),
        Some(screen.played()),
        "another preset viewed"
    );
    (report, screen)
}

/// Whether `screen`'s viewed view cleared.
fn viewed_cleared(screen: &game::ResultScreen) -> bool {
    match screen.status() {
        game::ViewStatus::Ready(s) => s.figures.cleared,
        other => panic!("the viewed view: {other:?}"),
    }
}

/// C27-1 (design.md "State Model", system sounds, Result): the result's
/// first show starts the sound of the play's own view's lamp, not the
/// view on screen's: a screen already switched to another preset whose
/// lamp differs (cleared against an own failed play, failed against an
/// own cleared one) starts `fail` and `clear` respectively.
// Source: SF-015
#[test]
fn the_first_result_sound_follows_the_own_view() {
    for (own, kind) in [(false, SystemKind::Fail), (true, SystemKind::Clear)] {
        let _fx = arm_result();
        on_rig(None, |rig| {
            let dir = tempfile::tempdir().unwrap();
            let probe = rig.audio.control.failure_probe();
            let a = installed_past(rig, None);
            let (report, screen) = switched_result(own, dir.path());
            // Positive control: a start queued moves its count.
            rig.audio
                .control
                .system_start(SystemKind::Scratch, false, None)
                .unwrap();
            assert_eq!(probe.system_starts(SystemKind::Scratch), 1);
            rig.show_end(&report, &screen);
            let starts = [SystemKind::Clear, SystemKind::Fail].map(|k| probe.system_starts(k));
            let want = if own { [1, 0] } else { [0, 1] };
            assert_eq!(starts, want, "own cleared {own}: clear, fail");
            wait("the result's sound", || probe.reserved(kind).is_some());
            assert_eq!(probe.reserved(kind), Some((a, false)), "own cleared {own}");
        });
    }
}

/// C27-3, C27-4 (design.md "State Model", system sounds, Result): the
/// result's later shows start and stop nothing: a view switch, and the
/// built-in result's screen built again (the rebuild, then the show the
/// flow makes after it), `clear` playing on through both; leaving Result
/// then stops `clear`, `fail` and `resultclose` once.
// Source: SF-015
#[test]
fn a_view_switch_keeps_the_result_sounds() {
    let one = one_tap();
    let _fx = arm_result();
    on_rig(None, |rig| {
        let dir = tempfile::tempdir().unwrap();
        let probe = rig.audio.control.failure_probe();
        let a = installed_past(rig, None);
        let (report, mut screen) = switched_result(true, dir.path());
        let kinds = [SystemKind::Clear, SystemKind::Fail, SystemKind::ResultClose];
        let counts = |p: &game::OutputProbe| {
            (
                kinds.map(|k| p.system_starts(k)),
                kinds.map(|k| p.system_stops(k)),
            )
        };
        // Positive control: the first show starts `clear`.
        rig.show_end(&report, &screen);
        let first = counts(&probe);
        assert_eq!(first, ([1, 0, 0], [0, 0, 0]), "the first show");
        // A view switch.
        screen.step(1, &report, &one);
        rig.show_end(&report, &screen);
        assert_eq!(counts(&probe), first, "a view switch");
        // The built-in result's screen built again, then shown again.
        assert!(!rig.presenter.current(), "the built-in result");
        rig.before_rebuild();
        rig.show_end(&report, &screen);
        assert_eq!(counts(&probe), first, "a rebuild");
        wait("the result's sound", || {
            probe.reserved(SystemKind::Clear).is_some()
        });
        assert_eq!(probe.reserved(SystemKind::Clear), Some((a, false)));
        // Leaving Result stops all three, once.
        rig.leave_result();
        assert_eq!(counts(&probe).1, [1, 1, 1], "Result left");
        rig.leave_result();
        assert_eq!(counts(&probe).1, [1, 1, 1], "left once");
    });
}

/// C27-5 (design.md "Settings Files"): `[sounds] loop_result` as read at
/// startup decides the result's loop, and a change to the settings the rig
/// holds after startup does not: started with it on, `clear` loops;
/// started with it off and the rig's settings then holding it on, `clear`
/// plays once.
// Source: SF-015
#[test]
fn loop_result_is_read_at_startup() {
    for at_startup in [true, false] {
        let _fx = arm_result();
        let sounds = game::SoundSettings {
            loop_result: at_startup,
            ..game::SoundSettings::default()
        };
        on_rig_with(sounds, None, |rig| {
            let dir = tempfile::tempdir().unwrap();
            let probe = rig.audio.control.failure_probe();
            let a = installed_past(rig, None);
            // The settings the rig holds, changed after startup.
            let mut later = game::Settings {
                device: game::DeviceSettings::default(),
                player: game::PlayerSettings::default(),
            };
            later.player.sounds.loop_result = true;
            rig.files = crate::tuning::SettingsFiles::new(DataRoot::new(dir.path().into()), later);
            let (report, screen) = switched_result(true, dir.path());
            rig.show_end(&report, &screen);
            wait("the result's sound", || {
                probe.reserved(SystemKind::Clear).is_some()
            });
            assert_eq!(
                probe.reserved(SystemKind::Clear),
                Some((a, at_startup)),
                "loop_result {at_startup} at startup"
            );
        });
    }
}
