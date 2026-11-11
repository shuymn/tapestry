//! The page jobs on a real coordinator (Z15-Z26, ZI1, ZI7): each event is
//! met at the job's probe, where the job waits until the test lets it go,
//! never by timing. A wait for something that never happens fails its
//! test by name, after `TAPESTRY_TEST_WATCHDOG` seconds (half, for the
//! test's own waits).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use game::coord::{Caps, Class, Coordinator, Observed, OwnedJob, Owner, Scope, Spawner, Waited};

use super::{
    JobMode, PageAt, PageBank, PageMeasure, PageProbe, PageSink, spawn_page_bank,
    spawn_page_measure,
};
use crate::loudness::fixture::{RATE, square, wav};
use crate::priority::{PriorityError, ThreadMode};

fn long() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600)) / 2
}

/// Waits until `done`, failing the test past [`long`].
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < long(), "{what} never happened");
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

/// `n` distinct sounds of `seconds` each in `dir`, each played as BGM at 0.
fn chart(dir: &std::path::Path, n: usize, seconds: f64) -> Arc<Chart> {
    let mut builder = ChartBuilder::new(120_000);
    for i in 0..n {
        builder = builder
            .sound(&format!("{i}.wav"))
            .bgm(TimeUs(0), SoundId(i as u16));
    }
    let mut chart = builder.build().unwrap();
    for (i, sound) in chart.sounds.iter_mut().enumerate() {
        let path = dir.join(format!("{i}.wav"));
        std::fs::write(&path, wav(&square(seconds, 1_000 + 100 * i as i16))).unwrap();
        sound.path = Some(path);
    }
    Arc::new(chart)
}

fn coordinator(foreground: usize, speculative: usize) -> Coordinator {
    Coordinator::new(Caps {
        foreground,
        speculative,
    })
}

/// The foreground and speculative permits held.
fn held(coord: &Coordinator) -> (usize, usize) {
    (
        coord.held(Class::Foreground),
        coord.held(Class::Speculative),
    )
}

/// The pool's cap for `files` files: the default thread count, the cores
/// and the files.
fn cap(files: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    audio::BankLimits::default()
        .max_decode_threads
        .min(cores)
        .min(files)
        .max(1)
}

/// A mode call, on the thread named.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Enter(String),
    Leave(String),
}

/// A thread mode that behaves as the system's (a second entry, or a leave
/// outside background mode, is refused) and can refuse or panic on
/// purpose; it records each call.
#[derive(Default)]
struct Seam {
    background: Mutex<HashMap<std::thread::ThreadId, bool>>,
    refuse_enter: AtomicBool,
    refuse_leave: AtomicBool,
    panic_enter: AtomicBool,
    calls: Mutex<Vec<Call>>,
}

impl Seam {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("").to_owned()
}

impl ThreadMode for Seam {
    fn enter_background(&self) -> Result<(), PriorityError> {
        self.calls.lock().unwrap().push(Call::Enter(thread_name()));
        assert!(!self.panic_enter.load(Ordering::SeqCst), "entry (test)");
        let mut b = self.background.lock().unwrap();
        let mine = b.entry(std::thread::current().id()).or_default();
        if *mine || self.refuse_enter.load(Ordering::SeqCst) {
            return Err(PriorityError("refused (test)".to_owned()));
        }
        *mine = true;
        Ok(())
    }

    fn leave_background(&self) -> Result<(), PriorityError> {
        self.calls.lock().unwrap().push(Call::Leave(thread_name()));
        let mut b = self.background.lock().unwrap();
        let mine = b.entry(std::thread::current().id()).or_default();
        if !*mine || self.refuse_leave.load(Ordering::SeqCst) {
            return Err(PriorityError("refused (test)".to_owned()));
        }
        *mine = false;
        Ok(())
    }
}

/// What a job's probe saw: each event with its count (from 1) and the
/// permits held then; points where the job waits until let go.
struct Script {
    coord: Coordinator,
    state: Mutex<ScriptState>,
    changed: Condvar,
}

/// Something a job does at one of its events (tests).
type Action = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct ScriptState {
    counts: HashMap<PageAt, usize>,
    log: Vec<(PageAt, usize, (usize, usize))>,
    holds: Vec<(PageAt, usize)>,
    arrived: Vec<(PageAt, usize)>,
    /// Called on the job's thread at an event, once.
    actions: Vec<((PageAt, usize), Action)>,
}

impl Script {
    fn new(coord: &Coordinator) -> Arc<Self> {
        Arc::new(Self {
            coord: coord.clone(),
            state: Mutex::new(ScriptState::default()),
            changed: Condvar::new(),
        })
    }

    fn probe(self: &Arc<Self>) -> PageProbe {
        let script = Arc::clone(self);
        Arc::new(move |at| script.at(at))
    }

    fn at(&self, at: PageAt) {
        let permits = held(&self.coord);
        let mut s = self.state.lock().unwrap();
        let n = {
            let c = s.counts.entry(at).or_default();
            *c += 1;
            *c
        };
        s.log.push((at, n, permits));
        let action = s
            .actions
            .iter()
            .position(|(k, _)| *k == (at, n))
            .map(|i| s.actions.remove(i).1);
        if let Some(action) = action {
            drop(s);
            action();
            s = self.state.lock().unwrap();
        }
        if s.holds.contains(&(at, n)) {
            s.arrived.push((at, n));
            self.changed.notify_all();
            let began = Instant::now();
            while s.holds.contains(&(at, n)) {
                assert!(began.elapsed() < long(), "{at:?} #{n} never let go");
                s = self.changed.wait_timeout(s, long()).unwrap().0;
            }
        }
    }

    /// The job waits at the `n`th `at` until [`release`](Self::release).
    fn hold(&self, at: PageAt, n: usize) {
        self.state.lock().unwrap().holds.push((at, n));
    }

    /// Runs `action` on the job's thread at the `n`th `at`.
    fn act(&self, at: PageAt, n: usize, action: impl FnOnce() + Send + 'static) {
        self.state
            .lock()
            .unwrap()
            .actions
            .push(((at, n), Box::new(action)));
    }

    fn reach(&self, at: PageAt, n: usize) {
        let began = Instant::now();
        let mut s = self.state.lock().unwrap();
        while !s.arrived.contains(&(at, n)) {
            assert!(began.elapsed() < long(), "{at:?} #{n} never reached");
            s = self.changed.wait_timeout(s, long()).unwrap().0;
        }
    }

    fn release(&self, at: PageAt, n: usize) {
        self.state.lock().unwrap().holds.retain(|h| *h != (at, n));
        self.changed.notify_all();
    }

    fn log(&self) -> Vec<(PageAt, usize, (usize, usize))> {
        self.state.lock().unwrap().log.clone()
    }

    fn count(&self, at: PageAt) -> usize {
        self.state
            .lock()
            .unwrap()
            .counts
            .get(&at)
            .copied()
            .unwrap_or(0)
    }
}

fn outcome<T: Clone>(job: &OwnedJob<T>) -> Observed<T> {
    let began = Instant::now();
    match job.wait(Duration::from_millis(1), || began.elapsed() > long()) {
        Waited::Done(o) => o,
        Waited::Stopped(_) => panic!("the job never ended"),
    }
}

/// The page bank of `chart`, its probe `script`'s, its mode through `seam`.
fn bank(
    coord: &Coordinator,
    chart: &Arc<Chart>,
    promoted: bool,
    seam: &Arc<Seam>,
    script: &Arc<Script>,
) -> (OwnedJob<PageBank>, Arc<JobMode>) {
    let mode = JobMode::new(Arc::clone(seam) as Arc<dyn ThreadMode>, "sounds: test");
    let job = spawn_page_bank(
        coord,
        Spawner::SYSTEM,
        coord.subscriber(Scope::Choice),
        ("page.bms".into(), Arc::clone(chart), RATE),
        (promoted, Owner::Speculative),
        Arc::clone(&mode),
        PageSink::default(),
        #[cfg(feature = "dev-tools")]
        None,
        Some(script.probe()),
    )
    .unwrap();
    (job, mode)
}

/// The page measurement of `chart` over its 1× bank.
fn measure(
    coord: &Coordinator,
    chart: &Arc<Chart>,
    promoted: bool,
    seam: &Arc<Seam>,
    script: &Arc<Script>,
) -> (OwnedJob<PageMeasure>, Arc<JobMode>) {
    let bank = audio::load_bank_at(
        chart,
        RATE,
        audio::Speed::NORMAL,
        &audio::BankLimits::default(),
    )
    .unwrap();
    let mode = JobMode::new(Arc::clone(seam) as Arc<dyn ThreadMode>, "loudness: test");
    let job = spawn_page_measure(
        coord,
        Spawner::SYSTEM,
        coord.subscriber(Scope::Choice),
        (
            "page.bms".into(),
            Arc::clone(chart),
            Arc::new(bank),
            "page.bms".to_owned(),
        ),
        (promoted, Owner::Speculative),
        Arc::clone(&mode),
        Some(script.probe()),
    )
    .unwrap();
    (job, mode)
}

/// The sounds of the bank published.
fn sounds(o: &Observed<PageBank>) -> usize {
    match &o.outcome {
        game::coord::Outcome::Done(Ok(bank)) => {
            (0..64).filter(|i| bank.get(SoundId(*i)).is_some()).count()
        }
        other => panic!("no bank: {other:?}"),
    }
}

const DECODE: &str = "job-sound-decode";
const MEASURE: &str = "job-loudness-measurement";

/// Z15 (P2): the 1× bank begun promoted (Decision) and demoted at page
/// entry. Its extras, held at their next file, retire there and give their
/// permits back while its own thread is still held at its next file; that
/// thread's checkpoint then moves it to the speculative permit and enters
/// background mode on it; no extra takes another file; the bank is whole.
#[test]
fn the_1x_bank_moves_to_speculation_at_its_next_file() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 8, 0.2);
    let coord = coordinator(4, 1);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    let extras = cap(8) - 1;
    for k in 1..=extras {
        script.hold(PageAt::File(k), 2);
    }
    script.hold(PageAt::File(0), 2);
    let (job, _mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::File(0), 2);
    for k in 1..=extras {
        script.reach(PageAt::File(k), 2);
    }
    assert_eq!(held(&coord), (1 + extras, 0));
    assert!(seam.calls().is_empty());
    job.demote();
    for k in 1..=extras {
        script.release(PageAt::File(k), 2);
    }
    // Back before the job's own thread moves on.
    until("the extras' permits back", || held(&coord) == (1, 0));
    script.release(PageAt::File(0), 2);
    let o = outcome(&job);
    assert_eq!(sounds(&o), 8);
    assert_eq!(seam.calls(), [Call::Enter(DECODE.to_owned())]);
    for k in 1..=extras {
        assert_eq!(script.count(PageAt::File(k)), 2, "worker {k} went on");
    }
    // Worker 0's files after the move ran on the speculative permit alone.
    let after: Vec<_> = script
        .log()
        .into_iter()
        .filter(|(at, n, _)| *at == PageAt::File(0) && *n > 2)
        .collect();
    assert!(!after.is_empty());
    assert!(after.iter().all(|(_, _, p)| *p == (0, 1)), "{after:?}");
    until("every permit back", || held(&coord) == (0, 0));
}

/// Z16 (P3): with speculation off, the demoted bank finds no speculative
/// permit at its next file: it ends cancelled, makes no mode call and
/// holds nothing.
#[test]
fn with_speculation_off_the_page_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 8, 0.2);
    let coord = coordinator(4, 0);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    script.hold(PageAt::File(0), 2);
    let (job, _mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::File(0), 2);
    job.demote();
    script.release(PageAt::File(0), 2);
    let o = outcome(&job);
    assert!(
        matches!(o.outcome, game::coord::Outcome::Cancelled),
        "{o:?}"
    );
    assert!(seam.calls().is_empty());
    assert_eq!(script.count(PageAt::File(0)), 2);
    until("every permit back", || held(&coord) == (0, 0));
}

/// Z17 (P3b): demoted with speculation off, then promoted again before its
/// next file: it goes on in the foreground and makes the whole bank.
#[test]
fn a_quick_start_keeps_the_decision_bank() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 8, 0.2);
    let coord = coordinator(4, 0);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    script.hold(PageAt::File(0), 2);
    let (job, _mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::File(0), 2);
    job.demote();
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    script.release(PageAt::File(0), 2);
    let o = outcome(&job);
    assert_eq!(sounds(&o), 8);
    assert!(seam.calls().is_empty());
    until("every permit back", || held(&coord) == (0, 0));
}

/// Z18 (P18): the exit refused at a promotion: logged once, the job goes on
/// in the foreground (at background priority); a demotion then makes no
/// entry (the thread is still in background mode), and the next promotion
/// tries the exit again, still logged once; the bank is whole.
#[test]
fn a_refused_background_exit_runs_foreground_at_background_priority() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 6, 0.2);
    // One foreground permit: its own thread decodes alone.
    let coord = coordinator(1, 1);
    let seam = Arc::new(Seam::default());
    seam.refuse_leave.store(true, Ordering::SeqCst);
    let script = Script::new(&coord);
    for n in [2, 3, 4] {
        script.hold(PageAt::File(0), n);
    }
    let (job, mode) = bank(&coord, &chart, false, &seam, &script);
    script.reach(PageAt::File(0), 2);
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    script.release(PageAt::File(0), 2);
    script.reach(PageAt::File(0), 3);
    assert_eq!(held(&coord), (1, 0), "in the foreground");
    job.demote();
    script.release(PageAt::File(0), 3);
    script.reach(PageAt::File(0), 4);
    assert_eq!(held(&coord), (0, 1));
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    script.release(PageAt::File(0), 4);
    let o = outcome(&job);
    assert_eq!(sounds(&o), 6);
    let d = DECODE.to_owned();
    assert_eq!(
        seam.calls(),
        [
            Call::Enter(d.clone()),
            Call::Leave(d.clone()),
            Call::Leave(d)
        ]
    );
    assert!(!mode.refused());
    assert_eq!(mode.refusals(), (1, 2), "(logged, refused)");
}

/// Z19, Z20 (P18b): the entry refused at the first speculative admission:
/// no file begun, its own cancel, logged once, nothing held; refused at a
/// demotion: no further file, a promotion after it still `Live` and the
/// result still its cancel.
#[test]
fn a_refused_background_entry_runs_no_speculation() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 6, 0.2);
    let coord = coordinator(4, 1);

    let seam = Arc::new(Seam::default());
    seam.refuse_enter.store(true, Ordering::SeqCst);
    let script = Script::new(&coord);
    let (job, mode) = bank(&coord, &chart, false, &seam, &script);
    let o = outcome(&job);
    assert!(
        matches!(
            o.outcome,
            game::coord::Outcome::Done(Err(audio::BankError::Cancelled))
        ),
        "{o:?}"
    );
    let events: Vec<PageAt> = script.log().into_iter().map(|e| e.0).collect();
    assert_eq!(events, [PageAt::File(0), PageAt::Cancel, PageAt::Stopped]);
    assert_eq!(mode.refusals(), (1, 1));
    until("every permit back", || held(&coord) == (0, 0));

    let seam = Arc::new(Seam::default());
    seam.refuse_enter.store(true, Ordering::SeqCst);
    let script = Script::new(&coord);
    script.hold(PageAt::File(0), 2);
    // Its first cancel that says stop, after the refusal: held there, the
    // job unpublished.
    script.hold(PageAt::Stopped, 1);
    let (job, mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::File(0), 2);
    job.demote();
    script.release(PageAt::File(0), 2);
    script.reach(PageAt::Stopped, 1);
    assert!(mode.refused());
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    script.release(PageAt::Stopped, 1);
    let o = outcome(&job);
    assert!(
        matches!(
            o.outcome,
            game::coord::Outcome::Done(Err(audio::BankError::Cancelled))
        ),
        "{o:?}"
    );
    assert_eq!(script.count(PageAt::File(0)), 2, "a file after the refusal");
    assert_eq!(mode.refusals(), (1, 1));
    until("every permit back", || held(&coord) == (0, 0));
}

/// Z21: a mode call that unwinds fails the job: at its first speculative
/// admission, and at a demotion's checkpoint inside its decode.
#[test]
fn a_panicking_mode_call_fails_the_page_job() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 4, 0.2);
    let coord = coordinator(4, 1);
    for promoted in [false, true] {
        let seam = Arc::new(Seam::default());
        seam.panic_enter.store(true, Ordering::SeqCst);
        let script = Script::new(&coord);
        script.hold(PageAt::File(0), 2);
        let (job, _mode) = bank(&coord, &chart, promoted, &seam, &script);
        if promoted {
            script.reach(PageAt::File(0), 2);
            job.demote();
            script.release(PageAt::File(0), 2);
        }
        let o = outcome(&job);
        assert!(
            matches!(o.outcome, game::coord::Outcome::Panicked(ref p) if p.0 == "entry (test)"),
            "promoted {promoted}: {o:?}"
        );
        until("every permit back", || held(&coord) == (0, 0));
    }
}

/// Z22, Z23 (finding 8): the measurement begun promoted, demoted while it
/// analyses: at its next block it is on the speculative permit, background
/// mode entered on its own thread; promoted again, at the next block it is
/// on a foreground permit, background mode left.
#[test]
fn the_page_measurement_moves_at_its_next_block() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 2, 3.0);
    let coord = coordinator(4, 1);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    for n in [2, 3, 4] {
        script.hold(PageAt::Block, n);
    }
    let (job, _mode) = measure(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::Block, 2);
    assert_eq!(held(&coord), (1, 0));
    job.demote();
    script.release(PageAt::Block, 2);
    script.reach(PageAt::Block, 3);
    assert_eq!(held(&coord), (0, 1));
    assert_eq!(seam.calls(), [Call::Enter(MEASURE.to_owned())]);
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    script.release(PageAt::Block, 3);
    script.reach(PageAt::Block, 4);
    assert_eq!(held(&coord), (1, 0));
    script.release(PageAt::Block, 4);
    let o = outcome(&job);
    assert!(
        matches!(o.outcome, game::coord::Outcome::Done(Ok(Some(_)))),
        "{o:?}"
    );
    assert_eq!(
        seam.calls(),
        [
            Call::Enter(MEASURE.to_owned()),
            Call::Leave(MEASURE.to_owned())
        ]
    );
}

/// Z24: demoted with speculation off, the measurement ends cancelled at
/// its next block.
#[test]
fn the_page_measurement_ends_at_a_demotion_with_no_speculative_permit() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 2, 3.0);
    let coord = coordinator(4, 0);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    script.hold(PageAt::Block, 2);
    let (job, _mode) = measure(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::Block, 2);
    job.demote();
    script.release(PageAt::Block, 2);
    let o = outcome(&job);
    assert_eq!(o.outcome, game::coord::Outcome::Cancelled);
    assert_eq!(script.count(PageAt::Block), 2);
    assert!(seam.calls().is_empty());
    until("every permit back", || held(&coord) == (0, 0));
}

/// Z25: a demotion whose entry is refused stops the measurement at that
/// block (its own cancel, logged once); one whose entry unwinds fails it,
/// although the analysis catches the unwinding.
#[test]
fn a_refused_or_failing_mode_call_stops_the_page_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 2, 3.0);
    let coord = coordinator(4, 1);
    for panics in [false, true] {
        let seam = Arc::new(Seam::default());
        if panics {
            seam.panic_enter.store(true, Ordering::SeqCst);
        } else {
            seam.refuse_enter.store(true, Ordering::SeqCst);
        }
        let script = Script::new(&coord);
        script.hold(PageAt::Block, 2);
        let (job, mode) = measure(&coord, &chart, true, &seam, &script);
        script.reach(PageAt::Block, 2);
        job.demote();
        script.release(PageAt::Block, 2);
        let o = outcome(&job);
        if panics {
            assert!(
                matches!(o.outcome, game::coord::Outcome::Panicked(ref p) if p.0 == "entry (test)"),
                "{o:?}"
            );
        } else {
            assert_eq!(
                o.outcome,
                game::coord::Outcome::Done(Err(crate::loudness::Cancelled))
            );
            assert_eq!(mode.refusals(), (1, 1));
        }
        assert_eq!(script.count(PageAt::Block), 2, "panics {panics}");
        until("every permit back", || held(&coord) == (0, 0));
    }
}

/// Z26: the bank's own thread mid-file, held at an ask of its cancel, and
/// the choice advanced (its cancel requested): that ask says stop, so the
/// file stops at its packet and no further file begins; settled, it is
/// joined, cancelled.
#[test]
fn a_page_bank_stops_at_the_next_packet_once_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    // Long files: many packets each.
    let chart = chart(dir.path(), 2, 4.0);
    let coord = coordinator(1, 1);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    // Ask 1 is worker 0's before its first file, 2 the file's own, 3 its
    // first packet's.
    script.hold(PageAt::Cancel, 3);
    let (job, _mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::Cancel, 3);
    coord.advance(Scope::Choice);
    script.release(PageAt::Cancel, 3);
    let o = outcome(&job);
    assert!(
        matches!(o.outcome, game::coord::Outcome::Cancelled),
        "{o:?}"
    );
    let log = script.log();
    // The held ask itself, answered after the cancel, says stop.
    let held_ask = log
        .iter()
        .position(|e| e.0 == PageAt::Cancel && e.1 == 3)
        .unwrap();
    assert_eq!(log[held_ask + 1].0, PageAt::Stopped, "{log:?}");
    // Its next file boundary says stop: no further ask of the cancel.
    assert_eq!(script.count(PageAt::Cancel), 3, "a file after the cancel");
    let settled = job.settle(long(), &Instant::now, &mut || None::<()>);
    assert!(matches!(
        settled,
        game::coord::Settled::Joined {
            observed: Some(Observed {
                outcome: game::coord::Outcome::Cancelled,
                ..
            }),
            after_panic: None,
            ..
        }
    ));
}

/// ZI1: demoted while another holder has the speculative permit: its own
/// thread gives its foreground permit back and waits holding none, while
/// the extras keep theirs until their next file, where they retire; a
/// promotion then admits it in the foreground at once and it finishes.
#[test]
fn a_demoted_decode_waits_for_its_permit_holding_none() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 8, 0.2);
    let coord = coordinator(4, 1);
    let blocker = coord.try_lease(Class::Speculative).unwrap();
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    let extras = cap(8) - 1;
    for k in 1..=extras {
        script.hold(PageAt::File(k), 2);
    }
    script.hold(PageAt::File(0), 2);
    let (job, _mode) = bank(&coord, &chart, true, &seam, &script);
    script.reach(PageAt::File(0), 2);
    for k in 1..=extras {
        script.reach(PageAt::File(k), 2);
    }
    job.demote();
    script.release(PageAt::File(0), 2);
    until("its own permit back", || held(&coord) == (extras, 1));
    for k in 1..=extras {
        script.release(PageAt::File(k), 2);
    }
    until("the extras' permits back", || held(&coord) == (0, 1));
    assert!(!job.poll().is_some(), "it ended while waiting");
    assert!(matches!(
        job.promote_unless_ended(),
        game::coord::Promotion::Live
    ));
    let o = outcome(&job);
    assert_eq!(sounds(&o), 8);
    assert!(seam.calls().is_empty());
    drop(blocker);
    until("every permit back", || held(&coord) == (0, 0));
}

/// ZI7: promoted after its own thread's checkpoint left it on the
/// speculative permit: no extra starts at that file; at its next file it
/// moves to the foreground, leaves background mode, and extras start.
#[test]
fn a_promotion_after_the_checkpoint_widens_at_the_next_file() {
    let dir = tempfile::tempdir().unwrap();
    let chart = chart(dir.path(), 8, 0.2);
    let coord = coordinator(4, 1);
    let seam = Arc::new(Seam::default());
    let script = Script::new(&coord);
    let slot: Arc<OnceLock<OwnedJob<PageBank>>> = Arc::new(OnceLock::new());
    let promote = Arc::clone(&slot);
    // Its first ask of the cancel comes after its first checkpoint.
    script.act(PageAt::Cancel, 1, move || {
        until("the job's handle", || promote.get().is_some());
        let _ = promote.get().unwrap().promote_unless_ended();
    });
    // An extra started at the first file waits at its first ask, holding
    // its permit, until worker 0 asks for its second file.
    let extras = cap(8) - 1;
    for k in 1..=extras {
        script.hold(PageAt::File(k), 1);
    }
    let release = Arc::clone(&script);
    script.act(PageAt::File(0), 2, move || {
        for k in 1..=extras {
            release.release(PageAt::File(k), 1);
        }
    });
    let (job, _mode) = bank(&coord, &chart, false, &seam, &script);
    let _ = slot.set(job);
    let job = slot.get().unwrap();
    let o = outcome(job);
    assert_eq!(sounds(&o), 8);
    let log = script.log();
    let second = log
        .iter()
        .position(|e| e.0 == PageAt::File(0) && e.1 == 2)
        .unwrap();
    // Its own speculative permit only: no extra holds a foreground one.
    assert_eq!(log[second].2, (0, 1), "{log:?}");
    assert!(
        log[..second]
            .iter()
            .all(|e| !matches!(e.0, PageAt::File(k) if k > 0)),
        "an extra started on the speculative permit: {log:?}"
    );
    if cap(8) > 1 {
        assert!(
            log.iter().any(|e| matches!(e.0, PageAt::File(k) if k > 0)),
            "no extra started: {log:?}"
        );
    }
    let d = DECODE.to_owned();
    assert_eq!(seam.calls(), [Call::Enter(d.clone()), Call::Leave(d)]);
}
