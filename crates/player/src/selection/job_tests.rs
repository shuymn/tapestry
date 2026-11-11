//! A chosen chart's load through its jobs (design.md "Load Pipeline",
//! "Play Lifecycle"): the Bank and Measure jobs on the real
//! [`PlayerRig::load_chart`](crate::play::PlayerRig) of a headless rig,
//! each event met at the job's own thread or the game thread's wait through
//! the load's test hook ([`Decodes`]), never by timing. A watchdog
//! (`TAPESTRY_TEST_WATCHDOG`, seconds) panics a wait that never ends.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use audio::{BankError, Cap, Found, Gain};
use bms_model::{Speed, TimeUs};
use game::coord::{Class, Coordinator, Failure, JobKind, JobPanic, Lease, LoadFailure, Spawner};
use game::{AppRig as _, Chain, DataRoot, Loaded, Loading};

use super::{ChartLoadError, Decodes, Hook, Probe, Selection};
use crate::background::tests::{Mode, Said, Stepper};
use crate::controls::Controls;
use crate::loudness::fixture::{square, wav};
use crate::play::PlayerRig;
use crate::render_side::tests::Installers;
use crate::select::{MenuQueue, SelectScreen};
use crate::shelf::tests::{coordinator, job, stepped_shelf, worker};

/// The output's rate, which the rig's engine runs at.
const RATE: u32 = 48_000;

fn watchdog() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Waits until `done`, looking in parked slices; panics past the watchdog,
/// which only catches what never happens.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < watchdog(), "{what} never happened");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// Writes `dir/song/key.wav`, half a second of a square wave of
/// `amplitude`.
fn write_sound(dir: &Path, amplitude: i16) {
    std::fs::write(
        dir.join("song").join("key.wav"),
        wav(&square(0.5, amplitude)),
    )
    .unwrap();
}

/// A chart `dir/song/a.bms` playing `key.wav` as BGM and on a note, with
/// `extra` header lines.
fn song(dir: &Path, extra: &str) -> PathBuf {
    let songs = dir.join("song");
    std::fs::create_dir_all(&songs).unwrap();
    write_sound(dir, 3_000);
    let path = songs.join("a.bms");
    let text =
        format!("#PLAYER 1\n#TITLE A\n#BPM 120\n{extra}#WAV01 key.wav\n#00101:01\n#00111:01\n");
    std::fs::write(&path, text).unwrap();
    path
}

/// [`song`] with `more` other sounds of their own contents beside
/// `key.wav`, each played as BGM: `1 + more` distinct files.
fn song_of(dir: &Path, more: usize) -> PathBuf {
    let path = song(dir, "");
    let mut text = std::fs::read_to_string(&path).unwrap();
    let mut bgm = String::from("01");
    for i in 0..more {
        let id = format!("{:02}", i + 2);
        let name = format!("more{i}.wav");
        std::fs::write(
            dir.join("song").join(&name),
            wav(&square(0.5, 1_000 + 500 * i as i16)),
        )
        .unwrap();
        text.push_str(&format!("#WAV{id} {name}\n"));
        bgm.push_str(&id);
    }
    text.push_str(&format!("#00201:{bgm}\n"));
    std::fs::write(&path, text).unwrap();
    path
}

fn settings() -> game::Settings {
    game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    }
}

/// The chart at `path` loaded at `speed`.
fn loaded(dir: &Path, path: &Path, speed: Speed) -> Loaded {
    let settings = settings();
    let mut config = crate::session_config(replay::ReceiveMethod::Scripted, None);
    config.speed = speed;
    let parts = game::Parts::unsaved(&settings.player);
    Loaded::load(
        DataRoot::new(dir.join("app")),
        settings,
        path,
        config,
        parts,
    )
    .unwrap()
}

/// A selection on a shelf without roots whose background measurement is
/// stepped by the test, on one coordinator with the load.
struct Bench {
    sel: Option<Selection>,
    coord: Coordinator,
    stepper: Stepper,
    mode: Arc<Mode>,
    decodes: Arc<Decodes>,
    witness: Arc<crate::bank_witness::BankWitness>,
}

fn bench(dir: &Path, decodes: Decodes, spawner: Spawner) -> Bench {
    bench_on(coordinator(), dir, decodes, spawner)
}

/// [`bench`] on `coord`.
fn bench_on(coord: Coordinator, dir: &Path, decodes: Decodes, spawner: Spawner) -> Bench {
    let mode = Arc::new(Mode::default());
    let (shelf, stepper) = stepped_shelf(dir, &coord, &mode);
    let decodes = Arc::new(decodes);
    let witness = crate::bank_witness::BankWitness::new();
    let sel = Selection {
        shelf,
        screen: SelectScreen::new(Vec::new()),
        menu: Arc::new(MenuQueue::default()),
        dropped: 0,
        feed: crate::selection_skin::SkinFeed::default(),
        preset_refusal: false,
        ime: None,
        rate: RATE,
        chosen_at: TimeUs(0),
        loading: None,
        practicing: false,
        entered_at: TimeUs(0),
        dirty: false,
        search_pending: false,
        preset_steps: std::collections::VecDeque::new(),
        script: None,
        coord: coord.clone(),
        spawner,
        page: crate::page_jobs::PageJobs::new(Arc::new(crate::priority::Unchanged)),
        decodes: Some(Arc::clone(&decodes)),
        after_bank: None,
        unload_fault: None,
        witness: Arc::clone(&witness),
    };
    Bench {
        sel: Some(sel),
        coord,
        stepper,
        mode,
        decodes,
        witness,
    }
}

/// `f` on a headless rig with `sel` as its selection (no adjustments).
fn on_rig<R>(
    sel: Selection,
    installers: &Installers,
    autoplay: bool,
    f: impl FnOnce(&mut PlayerRig<'_>) -> R,
) -> R {
    let (out, _) = crate::tests::on_rig_with(settings(), installers.spawn_bare(), |rig, _| {
        rig.adjust = None;
        rig.autoplay = autoplay;
        rig.selection = Some(sel);
        f(rig)
    });
    out
}

/// The load's outcome; a refusal panics with its reason.
fn outcome(got: Result<Loading, ChartLoadError>) -> Loading {
    got.unwrap_or_else(|e| panic!("refused: {}", Chain(&e)))
}

/// Whether the engine took this load's bank and schedule.
fn installed(rig: &PlayerRig<'_>, loaded: &Loaded) -> bool {
    rig.audio
        .control
        .seek_source()
        .matches(&loaded.audio_schedule())
}

fn sel<'r>(rig: &'r mut PlayerRig<'_>) -> &'r mut Selection {
    rig.selection.as_mut().unwrap()
}

fn hook(f: impl Fn(Probe, &Controls, &dyn Fn() -> bool) + Send + Sync + 'static) -> Option<Hook> {
    Some(Hook(Arc::new(f)))
}

fn decodes_with(hook: Option<Hook>) -> Decodes {
    Decodes {
        hook,
        ..Decodes::default()
    }
}

/// The speed of each decode begun.
fn begun(d: &Decodes) -> Vec<Speed> {
    d.begun.lock().unwrap().iter().map(|b| b.0).collect()
}

/// The speed of each decode begun, with the foreground permits held then.
fn begun_held(d: &Decodes) -> Vec<(Speed, usize)> {
    d.begun.lock().unwrap().clone()
}

/// Each decode that made a bank: its speed, its most workers at once and
/// the permits its other workers still held as its pool returned.
fn decoded(d: &Decodes) -> Vec<(Speed, usize, usize)> {
    d.decodes.lock().unwrap().clone()
}

/// The workers a decode of `files` distinct files runs beside `k`
/// foreground permits held elsewhere: one on its job's permit and one on
/// each free permit, at most the pool's cap (the default thread count, the
/// cores and the files).
fn granted(k: usize, files: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let cap = audio::BankLimits::default()
        .max_decode_threads
        .min(cores)
        .min(files)
        .max(1);
    (game::coord::FOREGROUND_PERMITS - k).min(cap)
}

fn analysis(d: &Decodes) -> Option<usize> {
    *d.analysis.lock().unwrap()
}

/// The quit key or a window close.
#[derive(Clone, Copy, Debug)]
enum Leave {
    Quit,
    Close,
}

impl Leave {
    fn press(self, controls: &Controls) {
        match self {
            Self::Quit => controls.command(game::Command::Quit),
            Self::Close => controls.close(),
        }
    }
}

/// At `at` on a job's thread: `how` pressed, then held until the game
/// thread's wait has asked the job to stop; the flag is set once it has.
fn leave_at(at: Probe, how: Leave) -> (Option<Hook>, Arc<AtomicBool>) {
    let seen = Arc::new(AtomicBool::new(false));
    let set = Arc::clone(&seen);
    let hook = hook(move |p, controls, stop| {
        if p == at {
            how.press(controls);
            until("the job's stop", stop);
            set.store(true, Ordering::SeqCst);
        }
    });
    (hook, seen)
}

/// The key of the chart at `path` at 1×, by its sounds as they are now.
fn key_now(loaded: &Loaded) -> (bms_model::Chart, Arc<audio::SoundBank>) {
    let bank = audio::load_bank_at(
        loaded.chart(),
        RATE,
        Speed::NORMAL,
        &audio::BankLimits::default(),
    )
    .unwrap();
    (loaded.chart().clone(), Arc::new(bank))
}

/// Whether the shelf has no measurement of the chart's sounds as they are
/// now, in memory or stored.
fn unmeasured(rig: &mut PlayerRig<'_>, loaded: &Loaded) -> bool {
    let (chart, bank) = key_now(loaded);
    sel(rig)
        .shelf
        .look_up(&chart, &bank, "a.bms", loaded.path())
        .is_some()
}

/// The background measurement ends its job and gives every permit back.
fn background_ends(b: &Bench, rig: &mut PlayerRig<'_>) {
    b.stepper.finish();
    assert!(worker(&sel(rig).shelf).wait_results(1, watchdog()));
    let coord = b.coord.clone();
    let shelf = &sel(rig).shelf;
    until("the background's end", || {
        worker(shelf).busy() == (false, true) && coord.held(Class::Speculative) == 0
    });
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// After a quit inside the Measure job, the chart is left to the
/// background, which measures it once selection releases it; after a
/// close, nothing is left to it.
fn left_to_background(b: &Bench, rig: &mut PlayerRig<'_>, how: Leave) {
    match how {
        Leave::Quit => {
            sel(rig).shelf.release();
            assert_eq!(b.stepper.said(), Said::Block(0));
            assert_eq!(b.coord.held(Class::Speculative), 1);
            background_ends(b, rig);
        }
        Leave::Close => assert_eq!(worker(&sel(rig).shelf).busy(), (false, true)),
    }
}

// --- Failures (F2, F3, F5, F7; F14p) ---

/// F2: the Bank job's thread cannot start: the load fails naming it,
/// nothing is decoded or installed, and no permit is held.
#[test]
fn a_bank_thread_that_cannot_start_fails_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(
        dir.path(),
        Decodes::default(),
        Spawner::refusing(&[JobKind::Bank]),
    );
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(
        got,
        Loading::Failed(LoadFailure {
            job: JobKind::Bank,
            cause: Failure::ThreadStart("refused (test)".to_owned()),
        })
    );
    assert!(!installed);
    assert!(begun(&b.decodes).is_empty());
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// F3: the Measure job's thread cannot start: the load fails naming it,
/// after the bank was made and installed; nothing is measured.
#[test]
fn a_measure_thread_that_cannot_start_fails_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(
        dir.path(),
        Decodes::default(),
        Spawner::refusing(&[JobKind::Measure]),
    );
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(
        got,
        Loading::Failed(LoadFailure {
            job: JobKind::Measure,
            cause: Failure::ThreadStart("refused (test)".to_owned()),
        })
    );
    assert!(installed);
    assert_eq!(begun(&b.decodes).len(), 1);
    assert_eq!(analysis(&b.decodes), None);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// F5: a panic in the Bank job's work, outside the per-sound decoder's
/// own catch, fails the load with its message; nothing is installed.
#[test]
fn a_bank_panic_fails_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let decodes = decodes_with(hook(|p, _, _| {
        assert!(
            !matches!(p, Probe::Decode(_)),
            "injected decode panic (test)"
        );
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(
        got,
        Loading::Failed(LoadFailure {
            job: JobKind::Bank,
            cause: Failure::Panicked(JobPanic("injected decode panic (test)".to_owned())),
        })
    );
    assert!(!installed);
    assert_eq!(analysis(&b.decodes), None);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// F14p: a panic in the Bank job after the quit asked it to stop still
/// fails the load.
#[test]
fn a_panic_after_a_quit_during_the_decode_still_fails_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let decodes = decodes_with(hook(|p, controls, stop| {
        if matches!(p, Probe::Decode(_)) {
            Leave::Quit.press(controls);
            until("the job's stop", stop);
            panic!("injected panic after the quit (test)");
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(
        got,
        Loading::Failed(LoadFailure {
            job: JobKind::Bank,
            cause: Failure::Panicked(JobPanic("injected panic after the quit (test)".to_owned())),
        })
    );
    assert!(!installed);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

// --- Refusals before and in the Bank job (F9, F10) ---

/// F9: a sound declaring one frame past the length cap is refused by the
/// Bank job's decode: the load is refused with the bank's reason.
#[test]
fn a_bank_too_large_is_refused_through_its_job() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    // 600 s + 1 frame declared at 8 kHz: 4 800 001 frames, 28 800 006 at
    // 48 kHz (by hand), past the 28 800 000-frame cap.
    let mut bytes = wav(&square(0.01, 1_000));
    let data: u32 = 4_800_001 * 2;
    bytes[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    bytes[40..44].copy_from_slice(&data.to_le_bytes());
    std::fs::write(dir.path().join("song").join("key.wav"), bytes).unwrap();
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (rig.load_chart(&loaded), installed(rig, &loaded)),
    );
    assert!(
        matches!(
            got,
            Err(ChartLoadError::Sounds(BankError::OverCap {
                cap: Cap::SoundLength,
                found: Found::Declared,
                ..
            }))
        ),
        "{got:?}"
    );
    assert!(!installed);
    assert_eq!(begun(&b.decodes).len(), 1, "not through the Bank job");
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// F10: a chart autoplay cannot play (a rank the evaluator refuses) is
/// refused before its Bank job begins: a refused Bank thread would have
/// failed the load instead.
#[test]
fn an_unplayable_autoplay_chart_begins_no_bank_job() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "#DEFEXRANK 3000000\n");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(
        dir.path(),
        Decodes::default(),
        Spawner::refusing(&[JobKind::Bank]),
    );
    let (got, installed) = on_rig(b.sel.take().unwrap(), &Installers::default(), true, |rig| {
        (rig.load_chart(&loaded), installed(rig, &loaded))
    });
    assert!(matches!(got, Err(ChartLoadError::Autoplay(_))), "{got:?}");
    assert!(!installed);
    assert!(begun(&b.decodes).is_empty());
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

// --- Quit and close (F11, F13a, F13b, F15, F16a, F16b, F17, F18, F19) ---

/// F11 (Bank): every foreground permit held elsewhere, `how` at the game
/// thread's first wait slice for the Bank job: the job, never admitted,
/// gives up and decodes nothing; the load is cancelled and installs
/// nothing; the permits held elsewhere are all that is held.
fn leave_while_the_bank_waits_for_a_permit(how: Leave) {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let pressed = AtomicBool::new(false);
    let decodes = decodes_with(hook(move |p, controls, _| {
        if p == Probe::Wait(JobKind::Bank) && !pressed.swap(true, Ordering::SeqCst) {
            how.press(controls);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let held: Vec<Lease> = (0..game::coord::FOREGROUND_PERMITS)
        .map(|_| b.coord.try_lease(Class::Foreground).unwrap())
        .collect();
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(got, Loading::Cancelled);
    assert!(!installed);
    assert!(begun(&b.decodes).is_empty(), "the decode ran");
    assert_eq!(
        b.coord.held(Class::Foreground),
        game::coord::FOREGROUND_PERMITS
    );
    drop(held);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

#[test]
fn a_quit_while_the_bank_waits_for_a_permit_leaves() {
    leave_while_the_bank_waits_for_a_permit(Leave::Quit);
}

#[test]
fn closing_while_the_bank_waits_for_a_permit_ends_the_run() {
    leave_while_the_bank_waits_for_a_permit(Leave::Close);
}

/// F11 (Measure): the bank installed, every foreground permit then taken
/// elsewhere, `how` at the game thread's first wait slice for the Measure
/// job: the job is never admitted and measures nothing; the load is
/// cancelled. A quit leaves the chart to the background; a close leaves
/// nothing.
fn leave_while_the_measurement_waits_for_a_permit(how: Leave) {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let coord = coordinator();
    let held: Arc<Mutex<Vec<Lease>>> = Arc::default();
    let pressed = AtomicBool::new(false);
    let (taker, leases) = (coord.clone(), Arc::clone(&held));
    let decodes = decodes_with(hook(move |p, controls, _| match p {
        Probe::BeforeMeasure => {
            let mut leases = leases.lock().unwrap();
            while let Some(l) = taker.try_lease(Class::Foreground) {
                leases.push(l);
            }
        }
        Probe::Wait(JobKind::Measure) if !pressed.swap(true, Ordering::SeqCst) => {
            how.press(controls);
        }
        _ => {}
    }));
    let mut b = bench_on(coord, dir.path(), decodes, Spawner::SYSTEM);
    let s = b.sel.take().unwrap();
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        let got = outcome(rig.load_chart(&loaded));
        assert_eq!(got, Loading::Cancelled);
        assert!(installed(rig, &loaded));
        assert_eq!(analysis(&b.decodes), None, "the analysis ran");
        assert_eq!(held.lock().unwrap().len(), game::coord::FOREGROUND_PERMITS);
        assert_eq!(
            b.coord.held(Class::Foreground),
            game::coord::FOREGROUND_PERMITS
        );
        held.lock().unwrap().clear();
        assert_eq!(b.coord.held(Class::Foreground), 0);
        match how {
            Leave::Quit => {
                sel(rig).shelf.release();
                assert_eq!(b.stepper.said(), Said::Block(0));
                b.stepper.finish();
                assert!(worker(&sel(rig).shelf).wait_results(1, watchdog()));
            }
            Leave::Close => assert_eq!(worker(&sel(rig).shelf).busy(), (false, true)),
        }
    });
}

#[test]
fn a_quit_while_the_measurement_waits_for_a_permit_leaves() {
    leave_while_the_measurement_waits_for_a_permit(Leave::Quit);
}

#[test]
fn closing_while_the_measurement_waits_for_a_permit_ends_the_run() {
    leave_while_the_measurement_waits_for_a_permit(Leave::Close);
}

/// F13a: the chart's background measurement holds a job the load waits
/// for; once the wait has promoted it (the worker left background mode),
/// `how` cancels the wait: no Bank job begins, nothing is installed, and
/// the measurement, demoted, goes on to its result (a Bank job begun would
/// fail the load: its thread is refused). The load's own
/// permits are back at once; the background's foreground permit returns
/// at its next check.
fn leave_while_the_load_waits_for_the_background(how: Leave) {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(
        dir.path(),
        Decodes::default(),
        Spawner::refusing(&[JobKind::Bank]),
    );
    let s = b.sel.take().unwrap();
    worker(&s.shelf).queue(job(&path));
    assert_eq!(b.stepper.said(), Said::Block(0));
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        let controls = rig.controls;
        let got = std::thread::scope(|scope| {
            scope.spawn(|| {
                // Steps the measurement until its check finds it promoted.
                while b.mode.leaves.load(Ordering::SeqCst) == 0 {
                    b.stepper.step();
                    assert!(matches!(b.stepper.said(), Said::Block(_)));
                }
                how.press(controls);
            });
            outcome(rig.load_chart(&loaded))
        });
        assert_eq!(got, Loading::Cancelled);
        assert!(!installed(rig, &loaded));
        assert!(begun(&b.decodes).is_empty());
        // The background's promoted permit, until its next check.
        assert_eq!(
            (
                b.coord.held(Class::Speculative),
                b.coord.held(Class::Foreground)
            ),
            (0, 1)
        );
        assert!(worker(&sel(rig).shelf).holds(&path));
        b.stepper.step();
        assert!(matches!(b.stepper.said(), Said::Block(_)));
        assert_eq!(
            (
                b.coord.held(Class::Speculative),
                b.coord.held(Class::Foreground)
            ),
            (1, 0)
        );
        background_ends(&b, rig);
    });
}

#[test]
fn a_quit_while_the_load_waits_for_the_background_leaves() {
    leave_while_the_load_waits_for_the_background(Leave::Quit);
}

#[test]
fn closing_while_the_load_waits_for_the_background_ends_the_run() {
    leave_while_the_load_waits_for_the_background(Leave::Close);
}

/// F13b / F18: `how` from inside the running Bank job, its permits taken,
/// held until the game thread's wait asked it to stop: the decode stops,
/// the load is cancelled, nothing is installed or measured.
fn leave_while_the_bank_decodes(how: Leave) {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let (hook, stopped) = leave_at(Probe::Decode(Speed::NORMAL), how);
    let decodes = decodes_with(hook);
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let (got, installed) = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| (outcome(rig.load_chart(&loaded)), installed(rig, &loaded)),
    );
    assert_eq!(got, Loading::Cancelled);
    assert!(
        stopped.load(Ordering::SeqCst),
        "the wait never stopped the job"
    );
    assert!(!installed);
    assert_eq!(begun(&b.decodes).len(), 1);
    assert_eq!(analysis(&b.decodes), None);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

#[test]
fn a_quit_while_the_bank_decodes_leaves() {
    leave_while_the_bank_decodes(Leave::Quit);
}

#[test]
fn closing_while_the_bank_decodes_ends_the_run() {
    leave_while_the_bank_decodes(Leave::Close);
}

/// Z14: a scoped decode's own cancel reads the run's controls. The quit
/// key pressed as the decode begins, the game thread's wait held so that it
/// never stops the job, stops the Bank's decode by itself. No bank is made.
#[test]
fn a_quit_stops_the_scoped_decode_by_itself() {
    let (speed, kind, ends) = (Speed::NORMAL, JobKind::Bank, 1);
    {
        let dir = tempfile::tempdir().unwrap();
        let path = song(dir.path(), "");
        let loaded = loaded(dir.path(), &path, speed);
        let ended = Arc::new(Mutex::new(None::<Arc<Decodes>>));
        let seen = Arc::clone(&ended);
        let decodes = decodes_with(hook(move |p, controls, _| {
            if p == Probe::Decode(Speed::NORMAL) {
                controls.command(game::Command::Quit);
            }
            if p == Probe::Wait(kind) {
                let d = seen.lock().unwrap().clone().unwrap();
                until("the decode's end", || {
                    d.ended.load(Ordering::SeqCst) >= ends
                });
            }
        }));
        let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
        *ended.lock().unwrap() = Some(Arc::clone(&b.decodes));
        let got = on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| outcome(rig.load_chart(&loaded)),
        );
        assert_eq!(got, Loading::Cancelled, "{kind:?}");
        let banks: Vec<Speed> = decoded(&b.decodes).iter().map(|d| d.0).collect();
        assert_eq!(banks, [], "{kind:?}: the decode made a bank");
        assert_eq!(b.coord.held(Class::Foreground), 0);
        // The hook held the record: let it go.
        ended.lock().unwrap().take();
    }
}

/// F15: the quit after the Bank job joined with its bank, before the
/// load's look: the bank is never installed, nothing is measured.
#[test]
fn a_quit_after_the_bank_is_made_installs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    let mut s = b.sel.take().unwrap();
    let (joined_tx, joined) = mpsc::channel::<()>();
    let (pressed_tx, pressed) = mpsc::channel::<()>();
    let pressed = Mutex::new(pressed);
    s.after_bank = Some(Arc::new(move || {
        joined_tx.send(()).unwrap();
        pressed
            .lock()
            .unwrap()
            .recv_timeout(watchdog())
            .expect("the quit is pressed");
    }));
    let (got, installed) = on_rig(s, &Installers::default(), false, |rig| {
        let controls = rig.controls;
        let got = std::thread::scope(|scope| {
            scope.spawn(move || {
                joined.recv_timeout(watchdog()).expect("the bank is made");
                Leave::Quit.press(controls);
                pressed_tx.send(()).unwrap();
            });
            outcome(rig.load_chart(&loaded))
        });
        (got, installed(rig, &loaded))
    });
    assert_eq!(got, Loading::Cancelled);
    assert!(!installed, "the bank was installed");
    assert_eq!(begun(&b.decodes).len(), 1);
    assert_eq!(analysis(&b.decodes), None);
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// F16a / F17: `how` from inside the running analysis (the Measure job
/// admitted, at the analysis's first check), held until the game thread's
/// wait asked the job to stop: the load is cancelled after its bank was
/// installed. A quit leaves the chart to the background; a close leaves
/// nothing.
fn leave_inside_the_running_analysis(how: Leave) {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let (hook, stopped) = leave_at(Probe::Analysis, how);
    let decodes = decodes_with(hook);
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let s = b.sel.take().unwrap();
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Cancelled);
        assert!(
            stopped.load(Ordering::SeqCst),
            "the wait never stopped the job"
        );
        assert!(installed(rig, &loaded));
        assert_eq!(analysis(&b.decodes), Some(1));
        assert_eq!(b.coord.held(Class::Foreground), 0);
        // Looked up only now: a look-up takes a job waiting in the
        // background as the load's own.
        left_to_background(&b, rig, how);
        assert!(unmeasured(rig, &loaded));
    });
}

#[test]
fn a_quit_inside_the_running_analysis_leaves_it_to_the_background() {
    leave_inside_the_running_analysis(Leave::Quit);
}

#[test]
fn closing_inside_the_running_analysis_ends_the_run() {
    leave_inside_the_running_analysis(Leave::Close);
}

/// F19: the screen's load of the chart held on the render thread, the
/// quit pressed from the game thread's wait for it: the wait does not
/// look at the quit, the load is ready, and the quit is left for the flow
/// to take.
#[test]
fn a_quit_during_the_screen_wait_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let installers = Installers::default();
    installers.hold_load.store(true, Ordering::Release);
    let screen = installers.clone();
    let decodes = decodes_with(hook(move |p, controls, _| {
        if p == Probe::Screen && screen.load_held.load(Ordering::Acquire) {
            Leave::Quit.press(controls);
            screen.hold_load.store(false, Ordering::Release);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let (got, installed, leaving) = on_rig(b.sel.take().unwrap(), &installers, false, |rig| {
        let got = outcome(rig.load_chart(&loaded));
        (got, installed(rig, &loaded), rig.controls.leaving())
    });
    assert_eq!(got, Loading::Ready);
    assert!(installers.load_held.load(Ordering::Acquire));
    assert!(installed);
    assert!(leaving, "the quit was taken");
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

// --- Permits (F21, F22, F23) ---

/// F21: the chart's background measurement, promoted by the load's wait,
/// ends; the wait returns with its foreground permit given back, so the
/// Bank decode of four files is granted every foreground permit its pool
/// runs a worker for.
#[test]
fn a_load_after_the_background_measurement_ends_is_granted_every_permit() {
    let dir = tempfile::tempdir().unwrap();
    let path = song_of(dir.path(), 3);
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), meeting(granted(0, 4)), Spawner::SYSTEM);
    let s = b.sel.take().unwrap();
    worker(&s.shelf).queue(job(&path));
    assert_eq!(b.stepper.said(), Said::Block(0));
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        let got = std::thread::scope(|scope| {
            scope.spawn(|| {
                while b.mode.leaves.load(Ordering::SeqCst) == 0 {
                    b.stepper.step();
                    assert!(matches!(b.stepper.said(), Said::Block(_)));
                }
                b.stepper.finish();
            });
            outcome(rig.load_chart(&loaded))
        });
        assert_eq!(got, Loading::Ready);
        // Its job's own permit only as it began: the background's is back.
        assert_eq!(begun_held(&b.decodes), [(Speed::NORMAL, 1)]);
        assert_eq!(decoded(&b.decodes), [(Speed::NORMAL, granted(0, 4), 0)]);
        assert_eq!(b.coord.held(Class::Foreground), 0);
    });
}

/// A record whose decodes each hold their first `workers` workers after
/// their first file until all of them are there, so they decode at once
/// whatever the scheduling. A worker that never comes fails the test.
fn meeting(workers: usize) -> Decodes {
    #[derive(Default)]
    struct Meet {
        asks: std::collections::HashMap<usize, usize>,
        arrived: usize,
    }
    let meet = Arc::new((Mutex::new(Meet::default()), std::sync::Condvar::new()));
    let reset = Arc::clone(&meet);
    let at = Arc::clone(&meet);
    Decodes {
        // Each decode meets on its own.
        hook: hook(move |p, _, _| {
            if matches!(p, Probe::Decode(_)) {
                *reset.0.lock().unwrap() = Meet::default();
            }
        }),
        at_file: Some(super::FileHook(Arc::new(move |worker| {
            let (state, changed) = &*at;
            let mut m = state.lock().unwrap();
            let nth = {
                let n = m.asks.entry(worker).or_default();
                *n += 1;
                *n
            };
            if nth != 2 || m.arrived >= workers {
                return;
            }
            m.arrived += 1;
            changed.notify_all();
            let began = Instant::now();
            while m.arrived < workers {
                assert!(began.elapsed() < watchdog() / 2, "only {} met", m.arrived);
                m = changed.wait_timeout(m, watchdog() / 2).unwrap().0;
            }
        }))),
        ..Decodes::default()
    }
}

/// A load at `speed` of a chart of `files` distinct sounds with `k`
/// foreground permits held elsewhere, its decodes' workers meeting as
/// [`meeting`] says for [`granted`]'s count: what its decodes and analysis
/// recorded. The permits held elsewhere are all that is held once it
/// returns.
fn load_beside(k: usize, files: usize, speed: Speed) -> Arc<Decodes> {
    let dir = tempfile::tempdir().unwrap();
    let path = song_of(dir.path(), files - 1);
    let loaded = loaded(dir.path(), &path, speed);
    let mut b = bench(dir.path(), meeting(granted(k, files)), Spawner::SYSTEM);
    let held: Vec<Lease> = (0..k)
        .map(|_| b.coord.try_lease(Class::Foreground).unwrap())
        .collect();
    let got = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| outcome(rig.load_chart(&loaded)),
    );
    assert_eq!(got, Loading::Ready, "k = {k}");
    assert_eq!(b.coord.held(Class::Foreground), k, "k = {k}");
    drop(held);
    assert_eq!(b.coord.held(Class::Foreground), 0);
    b.decodes
}

/// F22: with `k` foreground permits held elsewhere, the Bank decode of
/// four files runs on its job's permit and every free one its pool runs a
/// worker for: 4 − k workers on four cores, at least its own, the
/// foreground then full.
#[test]
fn the_decode_limit_is_the_free_foreground_permits() {
    for k in 0..game::coord::FOREGROUND_PERMITS {
        let d = load_beside(k, 4, Speed::NORMAL);
        assert_eq!(begun_held(&d), [(Speed::NORMAL, k + 1)], "k = {k}");
        assert_eq!(decoded(&d), [(Speed::NORMAL, granted(k, 4), 0)], "k = {k}");
        assert_eq!(analysis(&d), Some(k + 1), "k = {k}");
    }
}

/// Z12: a decode of one file takes no permit for a worker the pool would
/// not run: one worker, its job's permit only, whatever is free.
#[test]
fn a_decode_takes_permits_only_for_workers_that_run() {
    for speed in [Speed::NORMAL, Speed::FASTEST] {
        for k in 0..game::coord::FOREGROUND_PERMITS {
            let d = load_beside(k, 1, speed);
            assert_eq!(decoded(&d), [(speed, 1, 0)], "k = {k}, {speed:?}");
        }
    }
}

/// A load whose measurement plays at the headroom: ready and installed,
/// the gain the headroom, nothing kept.
fn at_the_headroom(decodes: Decodes, speed: Speed, spawner: Spawner) -> Arc<Decodes> {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, speed);
    let mut b = bench(dir.path(), decodes, spawner);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
            assert!(installed(rig, &loaded));
            assert_eq!(sel(rig).shelf.gain(), Gain::HEADROOM);
            assert!(unmeasured(rig, &loaded), "a measurement was kept");
        },
    );
    assert_eq!(b.coord.held(Class::Foreground), 0);
    b.decodes
}

/// F6: a panic in the analysis on the Measure job's thread is the
/// analysis's failure: the chart plays at the headroom.
#[test]
fn an_analysis_panic_in_the_measure_job_plays_at_the_headroom() {
    let decodes = decodes_with(hook(|p, _, _| {
        assert_ne!(p, Probe::Analysis, "injected analysis panic (test)");
    }));
    let d = at_the_headroom(decodes, Speed::NORMAL, Spawner::SYSTEM);
    assert_eq!(analysis(&d), Some(1));
}

/// F29: an analysis its meter refuses (an error, no panic) plays at the
/// headroom; it never reached its first check.
#[test]
fn a_failed_analysis_plays_at_the_headroom() {
    let decodes = Decodes::default();
    decodes.refuse_meter.store(true, Ordering::SeqCst);
    let d = at_the_headroom(decodes, Speed::NORMAL, Spawner::SYSTEM);
    assert_eq!(analysis(&d), None);
}

/// F28: a bank without a sound digest is never measured: no Measure job
/// begins (its thread would be refused), and it plays at the headroom.
#[test]
fn a_bank_without_a_digest_begins_no_measure_job() {
    let decodes = Decodes::default();
    decodes.no_digest.store(true, Ordering::SeqCst);
    let d = at_the_headroom(
        decodes,
        Speed::NORMAL,
        Spawner::refusing(&[JobKind::Measure]),
    );
    assert_eq!(analysis(&d), None);
}

/// F27: a chart measured once is measured no more: its next load begins
/// no Measure job (its thread would be refused) and plays at the gain
/// measured.
#[test]
fn a_cached_gain_begins_no_measure_job() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let mut b = bench(dir.path(), Decodes::default(), Spawner::SYSTEM);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
            assert_eq!(analysis(&b.decodes), Some(1));
            let measured = sel(rig).shelf.gain();
            assert_ne!(measured, Gain::HEADROOM);
            let again = Arc::new(Decodes::default());
            sel(rig).decodes = Some(Arc::clone(&again));
            sel(rig).spawner = Spawner::refusing(&[JobKind::Measure]);
            assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
            assert_eq!(sel(rig).shelf.gain(), measured);
            assert_eq!(begun(&again).len(), 1);
            assert_eq!(analysis(&again), None);
        },
    );
    assert_eq!(b.coord.held(Class::Foreground), 0);
}

/// Where a hook finds the bench's witness once it exists.
type WitnessSlot = Arc<Mutex<Option<Arc<crate::bank_witness::BankWitness>>>>;

/// What a hook saw of the witness: decodes in flight and banks alive.
type Seen = Arc<Mutex<Vec<(usize, usize)>>>;

/// What the witness recorded: its checks and their violations.
fn witnessed(b: &Bench) -> (usize, Vec<String>) {
    b.witness.record()
}

/// `loaded` at `speed` with a practice section, as a Start loads it: its
/// load keeps a restorer.
fn practiced(dir: &Path, path: &Path, speed: Speed) -> Loaded {
    let mut l = loaded(dir, path, speed);
    let screen = game::PracticeScreen::new(l.chart(), &rules::tapestry(), 0);
    let practice = screen.practice(&l);
    l.start_practice(practice).unwrap();
    l
}

/// A hook that records, at each release barrier before its unload, the
/// decodes in flight and the banks alive the witness sees.
fn at_each_barrier(witness: &WitnessSlot) -> (Option<Hook>, Seen) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (w, s) = (Arc::clone(witness), Arc::clone(&seen));
    let hook = hook(move |p, _, _| {
        if p == Probe::Barrier
            && let Some(w) = w.lock().unwrap().as_ref()
        {
            s.lock().unwrap().push(w.now());
        }
    });
    (hook, seen)
}

/// B1: a chart loaded, then another (Enter after a play): before the
/// second's unload the first bank is still alive; once the barrier has run,
/// no bank is alive and no decode is in flight, so its check finds nothing.
#[test]
fn entering_after_a_play_releases_every_owner_before_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let first = loaded(dir.path(), &path, Speed::NORMAL);
    let second = loaded(dir.path(), &path, Speed::NORMAL);
    let slot = Arc::new(Mutex::new(None));
    let (hook, seen) = at_each_barrier(&slot);
    let mut b = bench(dir.path(), decodes_with(hook), Spawner::SYSTEM);
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let s = b.sel.take().unwrap();
    on_rig(s, &Installers::default(), false, |rig| {
        assert_eq!(outcome(rig.load_chart(&first)), Loading::Ready);
        assert_eq!(b.witness.now(), (0, 1), "the installed bank");
        assert_eq!(outcome(rig.load_chart(&second)), Loading::Ready);
        assert!(installed(rig, &second));
    });
    assert_eq!(*seen.lock().unwrap(), [(0, 0), (0, 1)]);
    assert_eq!(witnessed(&b), (2, Vec::new()));
    slot.lock().unwrap().take();
}

/// B3a, B4, B5, B8: practice Starts in turn, each keeping a restorer: at
/// 1×, at 1.5× (another speed after 1×), at 0.5× (a second speed), at 1×
/// (a return to 1×) and at 1× again. Each barrier finds the previous
/// Start's bank alive before its unload, and none once it has run: its
/// restorer and the engine's bank are gone.
#[test]
fn every_start_releases_the_previous_starts_bank_first() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let speeds = [
        Speed::NORMAL,
        Speed::FASTEST,
        Speed::SLOWEST,
        Speed::NORMAL,
        Speed::NORMAL,
    ];
    let slot = Arc::new(Mutex::new(None));
    let (hook, seen) = at_each_barrier(&slot);
    let mut b = bench(dir.path(), decodes_with(hook), Spawner::SYSTEM);
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let s = b.sel.take().unwrap();
    on_rig(s, &Installers::default(), false, |rig| {
        for speed in speeds {
            let start = practiced(dir.path(), &path, speed);
            assert_eq!(outcome(rig.load_chart(&start)), Loading::Ready, "{speed:?}");
            assert!(rig.restorer.is_some(), "{speed:?}");
        }
    });
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), speeds.len());
    assert_eq!(seen[0], (0, 0));
    assert!(seen[1..].iter().all(|&s| s == (0, 1)), "{seen:?}");
    assert_eq!(witnessed(&b), (speeds.len(), Vec::new()));
    slot.lock().unwrap().take();
}

/// The witness's decode accounting begins before the Bank job is spawned:
/// with every foreground permit held, the job waits for admission, its
/// decode not entered, and the witness already counts it in flight.
#[test]
fn a_decode_is_accounted_from_its_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let slot: WitnessSlot = Arc::new(Mutex::new(None));
    let seen = Arc::new(Mutex::new(None));
    let (w, s) = (Arc::clone(&slot), Arc::clone(&seen));
    let decodes = decodes_with(hook(move |p, controls, _| {
        if p == Probe::Wait(JobKind::Bank) && s.lock().unwrap().is_none() {
            *s.lock().unwrap() = w.lock().unwrap().as_ref().map(|w| w.now());
            Leave::Quit.press(controls);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let held: Vec<Lease> = (0..game::coord::FOREGROUND_PERMITS)
        .map(|_| b.coord.try_lease(Class::Foreground).unwrap())
        .collect();
    let got = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| outcome(rig.load_chart(&loaded)),
    );
    assert_eq!(got, Loading::Cancelled);
    assert!(begun(&b.decodes).is_empty(), "the decode was entered");
    assert_eq!(*seen.lock().unwrap(), Some((1, 0)));
    assert_eq!(b.witness.now(), (0, 0));
    drop(held);
    slot.lock().unwrap().take();
}

/// The witness's decode accounting lasts until the bank is registered: the
/// Bank job joined, its bank not yet wrapped, the decode is still in
/// flight and the bank not yet alive in the registry; once registered and
/// installed, no decode is in flight.
#[test]
fn a_decode_is_accounted_until_its_bank_is_registered() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let slot: WitnessSlot = Arc::new(Mutex::new(None));
    let seen = Arc::new(Mutex::new(None));
    let (w, s) = (Arc::clone(&slot), Arc::clone(&seen));
    let decodes = decodes_with(hook(move |p, _, _| {
        if p == Probe::Joined {
            *s.lock().unwrap() = w.lock().unwrap().as_ref().map(|w| w.now());
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let s = b.sel.take().unwrap();
    on_rig(s, &Installers::default(), false, |rig| {
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
        assert_eq!(b.witness.now(), (0, 1));
    });
    assert_eq!(*seen.lock().unwrap(), Some((1, 0)));
    slot.lock().unwrap().take();
}

/// U6: a quit or a close pressed while the barrier unloads: the look after
/// the unload cancels the load before its Bank job, even one whose thread
/// would be refused (a load that spawned it would fail instead).
#[test]
fn a_close_or_quit_at_the_unload_cancels_before_the_decode() {
    for how in [Leave::Quit, Leave::Close] {
        let dir = tempfile::tempdir().unwrap();
        let path = song(dir.path(), "");
        let loaded = loaded(dir.path(), &path, Speed::NORMAL);
        let decodes = decodes_with(hook(move |p, controls, _| {
            if p == Probe::Barrier {
                how.press(controls);
            }
        }));
        let mut b = bench(dir.path(), decodes, Spawner::refusing(&[JobKind::Bank]));
        let got = on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| outcome(rig.load_chart(&loaded)),
        );
        assert_eq!(got, Loading::Cancelled, "{how:?}");
        assert!(begun(&b.decodes).is_empty());
        assert_eq!(witnessed(&b), (1, Vec::new()));
    }
}

/// U2, U5, U8: an unload the audio thread does not take (timed out, a
/// previous one still out, the thread gone) refuses the load before any
/// decode, with that error, whether or not a quit or a close was pressed
/// at the barrier; the barrier's check never runs.
#[test]
fn a_failed_unload_refuses_before_decoding() {
    use game::{AudioLoadError, AudioWaitError};
    for fault in [
        AudioLoadError::Wait(AudioWaitError::Timeout),
        AudioLoadError::Busy,
        AudioLoadError::Wait(AudioWaitError::Gone),
    ] {
        for how in [None, Some(Leave::Quit), Some(Leave::Close)] {
            let dir = tempfile::tempdir().unwrap();
            let path = song(dir.path(), "");
            let loaded = loaded(dir.path(), &path, Speed::NORMAL);
            let decodes = decodes_with(hook(move |p, controls, _| {
                if p == Probe::Barrier
                    && let Some(how) = how
                {
                    how.press(controls);
                }
            }));
            let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
            let mut s = b.sel.take().unwrap();
            s.unload_fault = Some(fault);
            let got = on_rig(s, &Installers::default(), false, |rig| {
                rig.load_chart(&loaded)
            });
            assert!(
                matches!(got, Err(ChartLoadError::Audio(e)) if e == fault),
                "{fault:?} {how:?}: {got:?}"
            );
            assert!(begun(&b.decodes).is_empty(), "{fault:?} {how:?}");
            assert_eq!(witnessed(&b), (0, Vec::new()));
        }
    }
}

/// U7: a load cancelled after the barrier's unload and its decode, before
/// its installation, leaves the engine empty; the next load's barrier
/// unloads that empty engine and the load goes through.
#[test]
fn a_refused_load_after_the_unload_leaves_the_next_load_working() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    // The first load: the quit pressed once its Bank job is joined, so
    // the look before the installation cancels it.
    let pressed = AtomicBool::new(false);
    let decodes = decodes_with(hook(move |p, controls, _| {
        if p == Probe::Joined && !pressed.swap(true, Ordering::SeqCst) {
            Leave::Quit.press(controls);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let s = b.sel.take().unwrap();
    on_rig(s, &Installers::default(), false, |rig| {
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Cancelled);
        assert!(!installed(rig, &loaded));
        assert_eq!(b.witness.now(), (0, 0), "a bank is still held");
        assert_eq!(rig.controls.take(), Some(game::Command::Quit));
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
        assert!(installed(rig, &loaded));
    });
    assert_eq!(witnessed(&b), (2, Vec::new()));
}

/// B10: a quit during the measurement, after the installation, leaves the
/// new bank installed (as before); the next load's barrier releases it.
#[test]
fn a_quit_during_the_measurement_leaves_its_bank_to_the_next_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let path = song(dir.path(), "");
    let loaded = loaded(dir.path(), &path, Speed::NORMAL);
    let pressed = Arc::new(AtomicBool::new(false));
    let p = Arc::clone(&pressed);
    let decodes = decodes_with(hook(move |at, controls, _| {
        if at == Probe::Analysis && !p.swap(true, Ordering::SeqCst) {
            Leave::Quit.press(controls);
        }
    }));
    let mut b = bench(dir.path(), decodes, Spawner::SYSTEM);
    let s = b.sel.take().unwrap();
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Cancelled);
        assert!(installed(rig, &loaded));
        assert_eq!(b.witness.now(), (0, 1), "the installed bank");
        assert_eq!(rig.controls.take(), Some(game::Command::Quit));
        left_to_background(&b, rig, Leave::Quit);
        assert_eq!(outcome(rig.load_chart(&loaded)), Loading::Ready);
    });
    assert_eq!(witnessed(&b), (2, Vec::new()));
}

mod page;
