//! The system sounds' owner, driven against a real `audio_control` worker
//! stepped on a thread of the test's, with the worker's seams: the recovery
//! state in memory, a decoder that can block, slow, panic or stand in for a
//! large sound, a set picker, a test clock and a spawner.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use audio::{
    AudioEngine, BankLimits, Decoded, FileRefusal, GenId, PickBudget, RecordingOutput, SimConfig,
    SimDevice, Sound, SoundBank, SystemKind, SystemVoices,
};
use bms_model::TimeUs;
use game::sound_sets::{
    DecodeFile, LinkCount, OneFile, RecoveryFs, ScanLimits, Unavailable, file_stem,
};
use game::{AudioWaitError, AudioWorker, DataRoot, SoundSettings, audio_control};

use super::{Clock, Event, Hooks, RealClock, Reason, Settled, Setup, Spawn, SystemSounds};

const RATE: u32 = 48_000;
const MIB: u64 = 1024 * 1024;
/// How long a test waits for real work (a decode, a step) before failing.
const PATIENCE: Duration = Duration::from_secs(20);
/// A few of the owner's slices: how far past its bound a bounded wait may
/// end.
const SLICES: Duration = Duration::from_millis(10);

/// A mono 16-bit WAV at [`RATE`] of `frames` frames at `level`.
fn wav(frames: usize, level: i16) -> Vec<u8> {
    let data = u32::try_from(frames * 2).unwrap();
    let mut b = Vec::with_capacity(44 + frames * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16_u32.to_le_bytes());
    b.extend_from_slice(&[1, 0, 1, 0]);
    b.extend_from_slice(&RATE.to_le_bytes());
    b.extend_from_slice(&(RATE * 2).to_le_bytes());
    b.extend_from_slice(&[2, 0, 16, 0]);
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for _ in 0..frames {
        b.extend_from_slice(&level.to_le_bytes());
    }
    b
}

/// A data root in a temporary folder.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> DataRoot {
        DataRoot::new(self.dir.path().to_owned())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Writes `bytes` at `rel` under the data root.
    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// `kind`'s `.wav` in `folder` (under the data root), `frames` long.
    fn sound(&self, folder: &str, kind: SystemKind, frames: usize) {
        self.write(
            &format!("{folder}/{}.wav", file_stem(kind)),
            &wav(frames, 1_000),
        );
    }
}

/// The audio thread's worker, stepped every millisecond on a thread of the
/// test's unless held.
struct Audio {
    control: game::AudioControl,
    shared: Arc<Stepped>,
    thread: Option<thread::JoinHandle<()>>,
}

struct Stepped {
    worker: Mutex<(AudioWorker, RecordingOutput<SimDevice>)>,
    held: AtomicBool,
    stop: AtomicBool,
    steps: AtomicU64,
}

impl Audio {
    fn new() -> Self {
        let bank = Arc::new(SoundBank::from_sounds(RATE, Vec::new()));
        let engine = AudioEngine::new(bank, &[], RATE, 480, 64, 64).unwrap();
        let (control, mut worker) = audio_control(engine);
        let mut out =
            RecordingOutput::new(SimDevice::new(SimConfig::steady(RATE, 480, TimeUs(0))), 0);
        for _ in 0..3 {
            worker.step(&mut out).unwrap();
        }
        let shared = Arc::new(Stepped {
            worker: Mutex::new((worker, out)),
            held: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            steps: AtomicU64::new(0),
        });
        let theirs = Arc::clone(&shared);
        let thread = thread::spawn(move || {
            while !theirs.stop.load(Ordering::Acquire) {
                if !theirs.held.load(Ordering::Acquire) {
                    let mut w = theirs.worker.lock().unwrap();
                    let (worker, out) = &mut *w;
                    worker.step(out).unwrap();
                    theirs.steps.fetch_add(1, Ordering::AcqRel);
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            control,
            shared,
            thread: Some(thread),
        }
    }

    fn hold(&self, held: bool) {
        // Taking the lock waits out a step under way.
        let _w = self.shared.worker.lock().unwrap();
        self.shared.held.store(held, Ordering::Release);
    }

    /// Steps it `n` times on this thread, held or not.
    #[cfg(feature = "dev-tools")]
    fn step_now(&self, n: usize) {
        let mut w = self.shared.worker.lock().unwrap();
        let (worker, out) = &mut *w;
        for _ in 0..n {
            worker.step(out).unwrap();
        }
    }

    /// Waits for `n` steps of the stepping thread.
    fn steps(&self, n: u64) {
        let from = self.shared.steps.load(Ordering::Acquire);
        let until = Instant::now() + PATIENCE;
        while self.shared.steps.load(Ordering::Acquire) < from + n {
            assert!(Instant::now() < until, "the audio worker did not step");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn system<R>(&self, f: impl FnOnce(&SystemVoices) -> R) -> R {
        let w = self.shared.worker.lock().unwrap();
        f(w.0.engine().system())
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A test's clock: it moves only as the owner's waits pause.
struct TestClock {
    base: Instant,
    us: AtomicU64,
}

impl TestClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            us: AtomicU64::new(0),
        })
    }

    fn elapsed(&self) -> Duration {
        Duration::from_micros(self.us.load(Ordering::Acquire))
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.base + self.elapsed()
    }

    /// Moves on at once, without giving up the thread (a yield can cost a
    /// whole scheduling quantum on a busy machine): a wait on what other
    /// threads do lets real time pass in its own closures
    /// ([`real_slice`]).
    fn pause(&self, d: Duration) {
        self.us
            .fetch_add(u64::try_from(d.as_micros()).unwrap(), Ordering::AcqRel);
    }
}

/// The system's clock, counting the owner's pauses.
#[derive(Default)]
struct CountingClock {
    pauses: AtomicU64,
}

impl CountingClock {
    fn pauses(&self) -> u64 {
        self.pauses.load(Ordering::Acquire)
    }
}

impl Clock for CountingClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self, d: Duration) {
        self.pauses.fetch_add(1, Ordering::AcqRel);
        thread::sleep(d);
    }
}

/// What the decoder seam does with a file.
#[derive(Clone, Copy, Debug, Default)]
struct Rule {
    /// Waits until the gate opens (a blocked read).
    block: bool,
    /// Decodes on after a cancel.
    ignore_cancel: bool,
    panic: bool,
    /// A silent sound of this many mono samples, not read from the file.
    fake: Option<usize>,
    delay: Option<Duration>,
}

/// One decode's entry or return, at the seam clock's time.
#[derive(Clone, Debug)]
struct Call {
    path: PathBuf,
    returned: bool,
    at: Instant,
}

/// The worker's decoder: `OneFile` unless a rule names the file.
#[derive(Clone)]
struct Seam(Arc<SeamState>);

struct SeamState {
    rules: Mutex<Vec<(PathBuf, Rule)>>,
    calls: Mutex<Vec<Call>>,
    gate: Mutex<bool>,
    opened: Condvar,
    clock: Arc<dyn Clock>,
}

impl Seam {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self(Arc::new(SeamState {
            rules: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            gate: Mutex::new(false),
            opened: Condvar::new(),
            clock,
        }))
    }

    /// `rule` for the file whose path ends with `suffix` (`B/clear.wav`).
    fn rule(&self, suffix: &str, rule: Rule) {
        let mut rules = self.0.rules.lock().unwrap();
        rules.retain(|(p, _)| p != Path::new(suffix));
        rules.push((PathBuf::from(suffix), rule));
    }

    fn open_gate(&self) {
        *self.0.gate.lock().unwrap() = true;
        self.0.opened.notify_all();
    }

    fn calls(&self) -> Vec<Call> {
        self.0.calls.lock().unwrap().clone()
    }

    /// The decodes entered of the file ending with `suffix`.
    fn opens(&self, suffix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| !c.returned && c.path.ends_with(suffix))
            .count()
    }

    fn log(&self, path: &Path, returned: bool) {
        self.0.calls.lock().unwrap().push(Call {
            path: path.to_owned(),
            returned,
            at: self.0.clock.now(),
        });
    }
}

impl DecodeFile for Seam {
    fn decode(
        &self,
        path: &Path,
        rate: u32,
        budget: &PickBudget,
        limits: &BankLimits,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<Decoded, FileRefusal> {
        self.log(path, false);
        let rule = self
            .0
            .rules
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| path.ends_with(p))
            .map(|&(_, r)| r)
            .unwrap_or_default();
        if rule.block {
            let mut open = self.0.gate.lock().unwrap();
            while !*open {
                open = self.0.opened.wait(open).unwrap();
            }
        }
        if let Some(d) = rule.delay {
            thread::sleep(d);
        }
        assert!(!rule.panic, "a seamed decoder panic");
        let decoded = if !rule.ignore_cancel && cancel() {
            Err(FileRefusal::Cancelled)
        } else if let Some(n) = rule.fake {
            budget.charge(n as u64 * 2).map(|()| {
                let mut h = DefaultHasher::new();
                path.hash(&mut h);
                let mut digest = [0_u8; 32];
                for (i, chunk) in digest.chunks_mut(8).enumerate() {
                    i.hash(&mut h);
                    chunk.copy_from_slice(&h.finish().to_le_bytes());
                }
                Decoded::Sound {
                    sound: Sound::mono(vec![0; n]),
                    digest,
                }
            })
        } else if rule.ignore_cancel {
            OneFile.decode(path, rate, budget, limits, &|| false)
        } else {
            OneFile.decode(path, rate, budget, limits, cancel)
        };
        self.log(path, true);
        decoded
    }
}

/// The recovery state in memory, a write failing from a chosen one on.
#[derive(Clone)]
struct MemFs(Arc<MemState>);

struct MemState {
    files: Mutex<HashMap<PathBuf, Vec<u8>>>,
    writes: AtomicUsize,
    fail_from: AtomicUsize,
    /// Another instance holds the lock.
    held: AtomicBool,
}

impl MemFs {
    fn new() -> Self {
        Self(Arc::new(MemState {
            files: Mutex::new(HashMap::new()),
            writes: AtomicUsize::new(0),
            fail_from: AtomicUsize::new(usize::MAX),
            held: AtomicBool::new(false),
        }))
    }

    /// Another instance holds the lock: it is refused.
    fn hold_lock(&self) {
        self.0.held.store(true, Ordering::Release);
    }

    /// Every write from the `n`th (1 the first) on fails.
    fn fail_from(&self, n: usize) {
        self.0.fail_from.store(n, Ordering::Release);
    }

    /// Every write from the `n`th from now (1 the next) on fails.
    #[cfg(feature = "dev-tools")]
    fn fail_from_now(&self, n: usize) {
        self.fail_from(self.0.writes.load(Ordering::Acquire) + n);
    }
}

impl RecoveryFs for MemFs {
    fn lock(&mut self, _: &Path) -> io::Result<bool> {
        Ok(!self.0.held.load(Ordering::Acquire))
    }

    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        Ok(self.0.files.lock().unwrap().get(path).cloned())
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let n = self.0.writes.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= self.0.fail_from.load(Ordering::Acquire) {
            return Err(io::Error::other("a seamed write failure"));
        }
        self.0
            .files
            .lock()
            .unwrap()
            .insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }

    /// No count of names: the tests' sets count by their names alone.
    fn links(&self) -> LinkCount {
        |_| None
    }
}

fn failing_spawn(_: Box<dyn FnOnce() + Send>) -> io::Result<thread::JoinHandle<()>> {
    Err(io::Error::other("a seamed spawn failure"))
}

/// Runs the worker to its end before the thread it returns is made: a
/// worker that ends at once has ended before the startup admission. Only
/// for a worker that cannot open the recovery state (one that opens it
/// waits for asks here).
fn finished_spawn(body: Box<dyn FnOnce() + Send>) -> io::Result<thread::JoinHandle<()>> {
    body();
    Ok(thread::spawn(|| {}))
}

/// How a run is made.
struct Opts {
    clock: Arc<dyn Clock>,
    spawn: Spawn,
    sounds: SoundSettings,
    /// The picker panics once this is set, ending the worker's thread.
    picker_panics: Arc<AtomicBool>,
    fs: MemFs,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            clock: Arc::new(RealClock),
            spawn: super::spawn_worker,
            sounds: SoundSettings::default(),
            picker_panics: Arc::new(AtomicBool::new(false)),
            fs: MemFs::new(),
        }
    }
}

/// An owner made against a stepped audio worker.
struct Run {
    sounds: SystemSounds,
    audio: Audio,
    seam: Seam,
    #[cfg_attr(
        not(feature = "dev-tools"),
        expect(dead_code, reason = "only the dev-tools tests fail its writes")
    )]
    fs: MemFs,
    said: Arc<Mutex<Vec<String>>>,
    /// The index the picker gives (held to the sets found).
    pick: Arc<AtomicUsize>,
    _fx: Fixture,
}

impl Run {
    fn new(fx: Fixture, opts: Opts) -> Self {
        Self::with_seam(fx, opts, |_| {})
    }

    /// A run whose seam `arm` sets up before the startup pick.
    fn with_seam(fx: Fixture, opts: Opts, arm: impl FnOnce(&Seam)) -> Self {
        let mut audio = Audio::new();
        let seam = Seam::new(Arc::clone(&opts.clock));
        arm(&seam);
        let said = Arc::new(Mutex::new(Vec::new()));
        let pick = Arc::new(AtomicUsize::new(0));
        let theirs = Arc::clone(&pick);
        let panics = Arc::clone(&opts.picker_panics);
        let setup = Setup {
            root: fx.root(),
            sounds: opts.sounds,
            rate: RATE,
            limits: BankLimits::default(),
            scan: ScanLimits::default(),
            fs: Box::new(opts.fs.clone()),
            decoder: Box::new(seam.clone()),
            picker: Box::new(move |n| {
                assert!(!panics.load(Ordering::Acquire), "a seamed picker panic");
                theirs.load(Ordering::Acquire).min(n.saturating_sub(1))
            }),
        };
        let lines = Arc::clone(&said);
        let hooks = Hooks {
            say: Box::new(move |line| lines.lock().unwrap().push(line.to_owned())),
            clock: opts.clock,
            spawn: opts.spawn,
        };
        let sounds = SystemSounds::new(&mut audio.control, setup, hooks);
        Self {
            sounds,
            audio,
            seam,
            fs: opts.fs,
            said,
            pick,
            _fx: fx,
        }
    }

    fn poll(&mut self) {
        self.sounds.poll(&mut self.audio.control);
    }

    /// Polls until `cond` holds.
    fn until(&mut self, what: &str, cond: impl Fn(&SystemSounds) -> bool) {
        let until = Instant::now() + PATIENCE;
        loop {
            self.poll();
            if cond(&self.sounds) {
                return;
            }
            assert!(Instant::now() < until, "waited too long for {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Polls until nothing is decoding, unacknowledged or retired.
    fn settle(&mut self) {
        self.until("I3", SystemSounds::settled);
    }

    fn entry(&mut self) -> GenId {
        self.sounds.repick(&mut self.audio.control, || false)
    }

    /// The next pick takes the `n`th set of each role (sorted by path).
    fn pick(&self, n: usize) {
        self.pick.store(n, Ordering::Release);
    }

    fn start(&mut self, kind: SystemKind, looped: Option<GenId>) {
        self.sounds.start(&mut self.audio.control, kind, looped);
    }

    /// Waits until the worker's decode of the file ending with `suffix`
    /// has begun: a read blocked there is under way.
    fn entered(&self, suffix: &str) {
        let until = Instant::now() + PATIENCE;
        while self.seam.opens(suffix) == 0 {
            assert!(Instant::now() < until, "{suffix} was never read");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn said(&self) -> Vec<String> {
        self.said.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<Event> {
        self.sounds.events.clone()
    }

    fn admitted(&self) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::Admitted(_)))
            .count()
    }

    /// The sound the mapping keeps for `kind`.
    fn kept(&self, kind: SystemKind) -> Option<Arc<Sound>> {
        self.sounds.mapping.get(kind).map(|k| Arc::clone(&k.sound))
    }

    fn sounding(&self) -> usize {
        self.audio.system(SystemVoices::sounding)
    }

    fn installed(&self) -> Option<GenId> {
        self.audio.control.system_installed()
    }
}

/// S3, S15: a pick reserves 128 MiB at admission and its generation's bytes
/// once ready; a refused file leaves only its kind silent; identical bytes
/// at another path are stored and charged once; a kind with no candidate
/// keeps the previous sound, the same PCM, read from no file and charged to
/// the new generation, while a refused candidate's kind is silent, a kind
/// no pick has found is silent, and a sound from a source skipped for the
/// run is not kept; the kept PCM is freed on this thread with its last
/// holder.
#[test]
fn a_pick_reserves_then_charges_its_bytes() {
    // The charges: A of 100 MiB current, nothing retired; B of 90 MiB.
    let fx = Fixture::new();
    fx.sound("sound/A", SystemKind::Clear, 10);
    fx.sound("sound/B", SystemKind::Clear, 10);
    let mut run = Run::with_seam(fx, Opts::default(), |seam| {
        let fake = |mib: u64| Rule {
            fake: Some(usize::try_from(mib * MIB / 2).unwrap()),
            ..Rule::default()
        };
        seam.rule("A/clear.wav", fake(100));
        seam.rule(
            "B/clear.wav",
            Rule {
                block: true,
                ..fake(90)
            },
        );
    });
    run.settle();
    assert_eq!(run.sounds.charged(), 100 * MIB);
    let a = run.entry();
    run.pick(1);
    let b = run.entry();
    assert!(b > a);
    assert_eq!(
        run.sounds.charged(),
        228 * MIB,
        "admitted with its reservation"
    );
    run.audio.hold(true);
    run.seam.open_gate();
    run.until("B's push", |s| s.unacked == Some(b));
    assert_eq!(run.sounds.charged(), 190 * MIB, "B's 90 MiB beside A's 100");
    run.audio.hold(false);
    run.settle();
    assert_eq!(run.sounds.charged(), 90 * MIB, "A uncharged once returned");
    drop(run);

    // Five files, two refused, one a byte copy of another: three kinds,
    // the copy stored and charged once.
    let fx = Fixture::new();
    let clear = wav(100, 1_000);
    fx.write("sound/A/clear.wav", &clear);
    fx.write("sound/A/fail.wav", &clear);
    fx.write("sound/A/scratch.wav", b"not a sound");
    fx.write("sound/A/f-open.wav", b"not a sound either");
    fx.sound("sound/A", SystemKind::PlayReady, 200);
    let mut run = Run::new(fx, Opts::default());
    run.settle();
    let kinds: Vec<SystemKind> = SystemKind::ALL
        .into_iter()
        .filter(|&k| run.kept(k).is_some())
        .collect();
    assert_eq!(
        kinds,
        [SystemKind::PlayReady, SystemKind::Clear, SystemKind::Fail]
    );
    let clear = run.kept(SystemKind::Clear).unwrap();
    assert!(Arc::ptr_eq(&clear, &run.kept(SystemKind::Fail).unwrap()));
    let ready = run.kept(SystemKind::PlayReady).unwrap();
    assert_eq!(run.sounds.charged(), clear.bytes() + ready.bytes());
    let refused = run
        .said()
        .iter()
        .filter(|l| l.contains("was refused"))
        .count();
    assert_eq!(refused, 2, "{:?}", run.said());
    drop(run);

    // Kept sounds: A with `scratch` and `fail`; B with neither of its own
    // (its `fail.wav` a folder); C with no `scratch`.
    let fx = Fixture::new();
    for (set, frames) in [("A", 10), ("B", 11), ("C", 12)] {
        fx.sound(&format!("sound/{set}"), SystemKind::Clear, frames);
    }
    fx.sound("sound/A", SystemKind::Scratch, 5 * 48_000);
    fx.sound("sound/A", SystemKind::Fail, 400);
    std::fs::create_dir_all(fx.path("sound/B/fail.wav")).unwrap();
    let mut run = Run::new(fx, Opts::default());
    run.settle();
    run.entry();
    let scratch = run.kept(SystemKind::Scratch).unwrap();
    let weak: Weak<Sound> = Arc::downgrade(&scratch);
    run.pick(1);
    run.entry();
    run.settle();
    let kept = run.kept(SystemKind::Scratch).unwrap();
    assert!(Arc::ptr_eq(&kept, &scratch), "B plays A's scratch");
    assert_eq!(run.seam.opens("A/scratch.wav"), 1, "no file read for it");
    assert!(run.kept(SystemKind::Fail).is_none(), "B's refused fail");
    assert!(
        run.said()
            .iter()
            .any(|l| l.contains("fail.wav") && l.contains("not a regular file")),
        "{:?}",
        run.said()
    );
    let b_clear = run.kept(SystemKind::Clear).unwrap();
    assert_eq!(run.sounds.charged(), b_clear.bytes() + scratch.bytes());
    drop((kept, scratch, b_clear));
    // A is returned and dropped: B's scratch still sounds.
    assert_eq!(run.sounds.alive.len(), 1);
    run.start(SystemKind::Scratch, None);
    run.audio.steps(2);
    assert_eq!(run.sounding(), 1);
    run.pick(2);
    run.entry();
    run.settle();
    assert!(Arc::ptr_eq(
        &run.kept(SystemKind::Scratch).unwrap(),
        &weak.upgrade().unwrap()
    ));
    // A again, its scratch decoded anew: once B and C are returned and the
    // mapping replaced, the first PCM is freed, here.
    run.pick(0);
    run.entry();
    run.settle();
    assert!(
        weak.upgrade().is_none(),
        "the kept PCM outlived its holders"
    );
    drop(run);

    // A startup pick with no `scratch` anywhere: silent.
    let fx = Fixture::new();
    fx.sound("sound/C", SystemKind::Clear, 10);
    let mut run = Run::new(fx, Opts::default());
    run.settle();
    assert!(run.kept(SystemKind::Scratch).is_none());
    drop(run);

    // A source skipped for the run: A is both a sound set and the BGM set;
    // B keeps A's scratch, then A's `select` panics in B's pick, so C keeps
    // nothing of A.
    let fx = Fixture::new();
    for set in ["A", "B", "C"] {
        fx.sound(&format!("sets/{set}"), SystemKind::Clear, 10);
    }
    fx.sound("sets/A", SystemKind::Scratch, 300);
    fx.sound("sets/A", SystemKind::Select, 300);
    let sets = fx.path("sets");
    let opts = Opts {
        sounds: SoundSettings {
            sound_root: Some(sets.clone()),
            bgm_root: Some(sets),
        },
        ..Opts::default()
    };
    let mut run = Run::new(fx, opts);
    run.settle();
    run.entry();
    assert!(run.kept(SystemKind::Scratch).is_some());
    run.seam.rule(
        "A/select.wav",
        Rule {
            panic: true,
            ..Rule::default()
        },
    );
    run.pick(1);
    run.entry();
    run.settle();
    assert!(
        run.kept(SystemKind::Scratch).is_some(),
        "kept before the panic"
    );
    assert_eq!(run.sounds.skipped.len(), 1);
    run.pick(2);
    run.entry();
    run.settle();
    assert!(
        run.kept(SystemKind::Scratch).is_none(),
        "a sound of a skipped source"
    );
}

/// The sets of the visit tests: sound sets A to F, each with `clear`, A
/// with a 20 s `scratch`, and the BGM set G with `select`.
fn visits() -> Fixture {
    let fx = sound_sets();
    fx.sound("bgm/G", SystemKind::Select, 4_800);
    fx
}

/// [`visits`] without the BGM set: a pick's only source is its sound set.
fn sound_sets() -> Fixture {
    let fx = Fixture::new();
    for (set, frames) in [
        ("A", 10),
        ("B", 11),
        ("C", 12),
        ("D", 13),
        ("E", 14),
        ("F", 15),
    ] {
        fx.sound(&format!("sound/{set}"), SystemKind::Clear, frames);
    }
    fx.sound("sound/A", SystemKind::Scratch, 20 * 48_000);
    fx
}

/// S3, S9: an entry stops every sound of the last visit first, the loop
/// whose exit stop never came included, so the generation it played from
/// goes back and the next entry admits.
#[test]
fn an_entry_stops_the_last_visit_first() {
    let mut run = Run::new(visits(), Opts::default());
    run.settle();
    let a = run.entry();
    run.start(SystemKind::Select, Some(a));
    run.audio.steps(2);
    assert_eq!(run.sounding(), 1);
    run.pick(1);
    let b = run.entry();
    assert!(b > a, "B admitted");
    run.audio.steps(2);
    assert_eq!(run.sounding(), 0, "the loop ended at the entry's stop");
    run.settle();
    assert_eq!(run.installed(), Some(b));
    assert!(run.events().contains(&Event::Released(a)));
    run.pick(2);
    let before = run.admitted();
    let c = run.entry();
    assert!(c > b);
    assert_eq!(run.admitted(), before + 1);
}

/// S3, S9: the first entry takes the startup pick; a later one waits for
/// the pick decoding, its acknowledgement and the retired generation's
/// return, then admits at once; an entry after a settled visit admits with
/// no wait; `poll` never admits; at most one generation is ever retired
/// and no install waits.
#[test]
fn an_entry_waits_then_rerolls() {
    let clock = Arc::new(CountingClock::default());
    let opts = Opts {
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        ..Opts::default()
    };
    let mut run = Run::with_seam(visits(), opts, |seam| {
        seam.rule(
            "B/clear.wav",
            Rule {
                delay: Some(Duration::from_millis(300)),
                ..Rule::default()
            },
        );
    });
    run.settle();
    // (a) The first entry: the startup pick's id, nothing admitted.
    let a = run.entry();
    assert_eq!(Some(a), run.installed());
    assert_eq!(run.admitted(), 1);
    // (b) B decoding at an entry: it is pushed, acknowledged and A
    // returned, then C is admitted at once.
    run.pick(1);
    let b = run.entry();
    run.pick(2);
    let at = Instant::now();
    let c = run.entry();
    let took = at.elapsed();
    assert!(c > b && b > a);
    assert!(took < super::ENTRY_SETTLE_WAIT, "{took:?}");
    let events = run.events();
    let place = |e: Event| events.iter().position(|&x| x == e).unwrap();
    assert!(place(Event::Pushed(b)) < place(Event::Released(a)));
    assert!(place(Event::Released(a)) < place(Event::Admitted(c)));
    // (c) C retired holding a one-shot (A's kept 20 s scratch, started
    // after the entry's stop and before D installs): the next entry's stop
    // cuts it, C goes back and E is admitted within a few steps.
    run.settle();
    run.pick(3);
    let d = run.entry();
    run.start(SystemKind::Scratch, None);
    run.until("D acknowledged", |s| {
        s.unacked.is_none() && s.in_flight.is_none()
    });
    assert_eq!(run.installed(), Some(d));
    assert_eq!(run.sounds.retired_count(), 1, "C retired with its voice");
    run.pick(4);
    let at = Instant::now();
    let e = run.entry();
    let took = at.elapsed();
    assert!(e > d);
    assert!(took < Duration::from_secs(1), "{took:?}");
    // (d) After a settled visit: admitted with no wait (no slice paused).
    run.settle();
    run.pick(5);
    let paused = clock.pauses();
    let f = run.entry();
    assert!(f > e);
    assert_eq!(clock.pauses(), paused, "the entry waited");
    // (e) Polls admit nothing.
    run.settle();
    let before = run.admitted();
    for _ in 0..100 {
        run.poll();
    }
    assert_eq!(run.admitted(), before);
    assert!(run.sounds.max_retired <= 1);
    assert_eq!(run.audio.control.counters().system_install_waits(), 0);
}

/// S5, S7: an entry whose stop finds the sequence exhausted stops every
/// kind at the terminal number, latches and returns the terminal empty
/// generation, which ends the loop and the retired one-shot; set starts are
/// then refused, and the decision sound is the tone, asked for without a
/// number, sounding in the step that applies the terminal install. A first
/// refused draw that is a decision sound latches and sounds the tone the
/// same way, and a later refused draw makes no second empty install.
#[cfg(feature = "dev-tools")]
#[test]
fn the_sequence_exhausted_latches_with_the_terminal_install() {
    use game::{SEQ_LIMIT, TERMINAL};

    let mut run = Run::new(visits(), Opts::default());
    run.settle();
    let a = run.entry();
    // B admitted; A's scratch started before B installs, so A retires
    // holding it; then B's loop.
    run.audio.hold(true);
    run.pick(1);
    let b = run.entry();
    run.start(SystemKind::Scratch, None);
    run.until("B's push", |s| s.unacked == Some(b));
    run.audio.hold(false);
    run.until("B acknowledged", |s| s.unacked.is_none());
    run.start(SystemKind::Select, Some(b));
    run.audio.steps(2);
    assert_eq!(run.sounding(), 2);
    assert_eq!(run.sounds.retired_count(), 1);
    assert!(b > a);

    run.audio.hold(true);
    run.audio.control.seed_system_seq(SEQ_LIMIT);
    let id = run.entry();
    assert_eq!(id, GenId(TERMINAL));
    assert_eq!(run.sounds.latched, Some(Reason::SequenceExhausted));
    for kind in SystemKind::ALL {
        assert_eq!(run.audio.control.system_stop_word(kind), TERMINAL);
    }
    assert_eq!(run.admitted(), 2, "nothing admitted");
    // The tone, asked for with no number before the step that installs.
    let word = run.audio.control.system_seq_word();
    let tones = run.audio.control.failure_probe().tone_requests();
    run.start(SystemKind::Decide, None);
    assert_eq!(run.audio.control.system_seq_word(), word);
    assert_eq!(run.audio.control.failure_probe().tone_requests(), tones + 1);
    run.audio.step_now(1);
    assert_eq!(run.installed(), Some(GenId(TERMINAL)));
    assert!(run.audio.system(SystemVoices::tone_sounding));
    assert_eq!(run.sounding(), 0, "the loop and the retired one-shot ended");
    run.audio.hold(false);
    run.until("A and B back", |s| s.alive.is_empty());
    run.start(SystemKind::Scratch, None);
    assert_eq!(run.sounds.refused, 1);
    for _ in 0..100 {
        run.start(SystemKind::Decide, None);
        run.audio.step_now(1);
        assert!(run.audio.system(SystemVoices::tone_sounding));
    }
    assert_eq!(
        run.audio.control.failure_probe().tone_requests(),
        tones + 101
    );
    assert_eq!(run.audio.control.system_seq_word(), word);
    assert_eq!(run.sounds.latch_installs, 1);
    drop(run);

    // The first refused draw a decision sound's.
    let mut run = Run::new(visits(), Opts::default());
    run.settle();
    run.entry();
    run.audio.hold(true);
    run.audio.control.seed_system_seq(SEQ_LIMIT);
    run.start(SystemKind::Decide, None);
    assert_eq!(run.sounds.latched, Some(Reason::SequenceExhausted));
    run.audio.step_now(1);
    assert_eq!(run.installed(), Some(GenId(TERMINAL)));
    assert!(run.audio.system(SystemVoices::tone_sounding));
    // A later refused draw latches nothing more.
    run.sounds.stop(&mut run.audio.control, SystemKind::Select);
    run.start(SystemKind::Scratch, None);
    assert_eq!(run.sounds.latch_installs, 1);
    assert_eq!(run.sounds.newest, GenId(TERMINAL));
}

/// A real millisecond inside a test clock's slice, for waits on the
/// worker or the audio thread: the slice's closures call it.
fn real_slice() {
    thread::sleep(Duration::from_millis(1));
}

/// A run on a [`TestClock`], its seam armed by `arm`.
fn virtual_run(fx: Fixture, arm: impl FnOnce(&Seam)) -> (Run, Arc<TestClock>) {
    let clock = TestClock::new();
    let opts = Opts {
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        ..Opts::default()
    };
    (Run::with_seam(fx, opts, arm), clock)
}

/// Every decode entered had returned by `returned`, a settlement's
/// return: the clock moves on past it and real time passes, so a call
/// still to come would be stamped later, then each path's entries equal
/// its returns and every call is stamped at or before `returned`.
fn decodes_ended_by(run: &Run, clock: &TestClock, returned: Instant) {
    clock.pause(SLICES);
    thread::sleep(Duration::from_millis(50));
    let calls = run.seam.calls();
    assert!(!calls.is_empty());
    let mut open: HashMap<&Path, i64> = HashMap::new();
    for c in &calls {
        *open.entry(&c.path).or_default() += if c.returned { -1 } else { 1 };
    }
    assert!(
        open.values().all(|&n| n == 0),
        "a decode under way: {open:?}"
    );
    assert!(
        calls.iter().all(|c| c.at <= returned),
        "a decode after the settlement's return"
    );
}

/// A blocked read for `suffix`, cancelled at the gate unless `ready`.
fn blocked(seam: &Seam, suffix: &str, ready: bool) {
    seam.rule(
        suffix,
        Rule {
            block: true,
            ignore_cancel: ready,
            ..Rule::default()
        },
    );
}

/// S9: an entry that cannot settle within the bound keeps the current
/// generation for the visit, says so once and counts it, by what was
/// outstanding: the pick's report (the pick cancelled, later entries not
/// waiting again, its late report dropped), the acknowledgement (the
/// newest pushed returned) or the retired generation's return.
#[test]
fn an_entry_that_cannot_settle_skips_its_reroll() {
    // (a) The report: B's read blocked (B the pick's only source, so a
    // late report can be ready).
    for late_ready in [false, true] {
        let (mut run, clock) = virtual_run(sound_sets(), |seam| {
            blocked(seam, "B/clear.wav", late_ready);
        });
        run.settle();
        let a = run.entry();
        run.pick(1);
        let b = run.entry();
        run.entered("B/clear.wav");
        let from = clock.elapsed();
        assert_eq!(run.entry(), a, "the current one kept");
        let waited = clock.elapsed() - from;
        assert!(waited >= super::ENTRY_SETTLE_WAIT, "{waited:?}");
        assert!(waited < super::ENTRY_SETTLE_WAIT + SLICES, "{waited:?}");
        assert!(
            run.sounds
                .in_flight
                .as_ref()
                .unwrap()
                .cancel
                .load(Ordering::Acquire)
        );
        let lines: Vec<String> = run
            .said()
            .into_iter()
            .filter(|l| l.contains("not picked again"))
            .collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("the pick's report"), "{lines:?}");
        assert_eq!(run.sounds.skipped_rerolls, 1);
        run.start(SystemKind::Scratch, None);
        run.audio.steps(2);
        assert_eq!(run.sounding(), 1, "the visit's sounds from A");
        // A second entry while B is still blocked: at once, counted.
        let from = clock.elapsed();
        assert_eq!(run.entry(), a);
        assert_eq!(clock.elapsed(), from);
        assert_eq!(run.sounds.skipped_rerolls, 2);
        // B's late report, cancelled or ready, is dropped unpushed.
        run.seam.open_gate();
        run.until("B's late report", |s| s.events.contains(&Event::Dropped(b)));
        assert!(!run.events().contains(&Event::Pushed(b)));
        run.settle();
        assert!(!run.sounds.stalled);
        assert_eq!(run.installed(), Some(a));
        assert_eq!(run.sounds.charged(), run.sounds.alive[0].1);
        let before = run.admitted();
        run.pick(2);
        assert!(run.entry() > b);
        assert_eq!(run.admitted(), before + 1, "the next entry admits");
    }

    // (b) The acknowledgement: the audio worker held with B pushed.
    for stepped in [false, true] {
        let (mut run, _clock) = virtual_run(visits(), |_| {});
        run.settle();
        run.entry();
        run.audio.hold(true);
        run.pick(1);
        let b = run.entry();
        run.until("B's push", |s| s.unacked == Some(b));
        assert_eq!(run.entry(), b, "the newest pushed");
        assert_eq!(run.sounds.skipped_rerolls, 1);
        assert!(
            run.said()
                .iter()
                .any(|l| l.contains("the install's acknowledgement")),
            "{:?}",
            run.said()
        );
        if stepped {
            run.start(SystemKind::Select, Some(b));
            run.audio.hold(false);
            let settled = run.sounds.settle_before_recording(
                &mut run.audio.control,
                || {
                    real_slice();
                    false
                },
                || false,
            );
            assert_eq!(settled, Ok(Settled::Ready));
            run.audio.steps(2);
            assert_eq!(run.installed(), Some(b));
            assert_eq!(run.sounding(), 1, "B's loop started at its install");
        } else {
            let settled =
                run.sounds
                    .settle_before_recording(&mut run.audio.control, || false, || false);
            assert_eq!(settled, Err(AudioWaitError::Timeout));
        }
    }

    // (d) The return only: A retired holding a voice, the worker held
    // after the entry's stop.
    let (mut run, clock) = virtual_run(visits(), |_| {});
    run.settle();
    run.entry();
    run.audio.hold(true);
    run.pick(1);
    let b = run.entry();
    run.start(SystemKind::Scratch, None);
    run.until("B's push", |s| s.unacked == Some(b));
    run.audio.hold(false);
    run.until("B acknowledged", |s| s.unacked.is_none());
    assert_eq!(run.sounds.retired_count(), 1);
    run.audio.hold(true);
    let from = clock.elapsed();
    assert_eq!(run.entry(), b);
    let waited = clock.elapsed() - from;
    assert!(waited < super::ENTRY_SETTLE_WAIT + SLICES, "{waited:?}");
    assert_eq!(run.sounds.skipped_rerolls, 1);
    assert!(
        run.said()
            .iter()
            .any(|l| l.contains("the retired generation's return")),
        "{:?}",
        run.said()
    );
    let mut slices = 0;
    let settled = run.sounds.settle_before_recording(
        &mut run.audio.control,
        || {
            slices += 1;
            assert!(slices < 100, "the settlement waits for the return");
            false
        },
        || false,
    );
    assert_eq!(settled, Ok(Settled::Ready), "READY needs no return");
    run.start(SystemKind::Select, Some(b));
    run.audio.hold(false);
    run.until("A back", |s| s.retired_count() == 0);
    run.audio.steps(2);
    assert_eq!(run.sounding(), 1, "the loop from B");
}

/// S5, S13: latched by a failed clear, set starts are refused and counted
/// with nothing pushed, and the decision sound asks for the tone without a
/// number; the empty install applies, then the tone sounds.
#[cfg(feature = "dev-tools")]
#[test]
fn a_latch_refuses_set_starts_and_requests_the_tone() {
    let mut run = Run::new(visits(), Opts::default());
    run.settle();
    run.entry();
    // B's marker commits, its clear fails.
    run.fs.fail_from_now(2);
    run.audio.hold(true);
    run.pick(1);
    run.entry();
    run.until("the latch", |s| s.latched.is_some());
    assert!(matches!(run.sounds.latched, Some(Reason::Recovery(_))));
    let word = run.audio.control.system_seq_word();
    let tones = run.audio.control.failure_probe().tone_requests();
    let newest = run.sounds.newest;
    run.start(SystemKind::Scratch, None);
    run.start(SystemKind::Select, Some(newest));
    assert_eq!(run.sounds.refused, 2);
    run.start(SystemKind::Decide, None);
    assert_eq!(run.audio.control.system_seq_word(), word, "no number drawn");
    assert_eq!(run.audio.control.failure_probe().tone_requests(), tones + 1);
    run.audio.step_now(1);
    assert_eq!(run.installed(), Some(newest));
    assert!(run.audio.system(SystemVoices::tone_sounding));
    assert_eq!(run.sounding(), 0);
}

/// S3: a ready report after a latch, and one after the close request, are
/// dropped and uncharged, never pushed.
#[test]
fn superseded_values_are_dropped() {
    let mut run = Run::with_seam(visits(), Opts::default(), |seam| {
        blocked(seam, "B/clear.wav", true);
    });
    run.settle();
    let a = run.entry();
    run.pick(1);
    let b = run.entry();
    run.sounds.latch(&mut run.audio.control, Reason::WorkerGone);
    run.seam.open_gate();
    run.until("B's report", |s| s.events.contains(&Event::Dropped(b)));
    assert!(!run.events().contains(&Event::Pushed(b)));
    run.until("A back", |s| s.alive.is_empty());
    assert_eq!(run.sounds.charged(), 0);
    assert_ne!(run.installed(), Some(b));
    assert!(a < b);
    drop(run);

    let mut run = Run::with_seam(sound_sets(), Opts::default(), |seam| {
        seam.rule(
            "B/clear.wav",
            Rule {
                delay: Some(Duration::from_millis(200)),
                ignore_cancel: true,
                ..Rule::default()
            },
        );
    });
    run.settle();
    let a = run.entry();
    let a_bytes = run.sounds.charged();
    run.pick(1);
    let b = run.entry();
    run.sounds.close(&mut run.audio.control);
    assert!(run.events().contains(&Event::Dropped(b)));
    assert!(!run.events().contains(&Event::Pushed(b)));
    assert_eq!(run.sounds.charged(), a_bytes, "B uncharged");
    run.audio.steps(2);
    assert_eq!(run.installed(), Some(a));
}

/// S5: `close` right after a latch, no poll between: the deferred-drop
/// slot goes before the charge of the generation the empty install sent
/// back is released.
#[test]
fn close_drains_the_slot_before_the_returns() {
    let mut run = Run::new(visits(), Opts::default());
    run.settle();
    let a = run.entry();
    run.sounds
        .latch(&mut run.audio.control, Reason::SequenceExhausted);
    let empty = run.sounds.newest;
    let until = Instant::now() + PATIENCE;
    while run.installed() != Some(empty) {
        assert!(Instant::now() < until, "the empty install never applied");
        thread::sleep(Duration::from_millis(1));
    }
    run.audio.steps(2);
    assert!(!run.events().contains(&Event::Drained));
    run.sounds.close(&mut run.audio.control);
    let events = run.events();
    let drained = events.iter().position(|&e| e == Event::Drained).unwrap();
    let released = events
        .iter()
        .position(|&e| e == Event::Released(a))
        .unwrap();
    assert!(drained < released, "{events:?}");
}

/// S5: a latch the receivers find (here the worker's end, with A
/// installed) is drained in the same service, before the returns its
/// empty install sends back are taken.
#[test]
fn a_latch_found_while_receiving_is_drained_before_the_returns() {
    let opts = Opts::default();
    let panic = Arc::clone(&opts.picker_panics);
    let mut run = Run::new(visits(), opts);
    run.settle();
    run.entry();
    panic.store(true, Ordering::Release);
    run.pick(1);
    run.entry();
    let until = Instant::now() + PATIENCE;
    while !run
        .sounds
        .worker
        .as_ref()
        .and_then(|w| w.handle.as_ref())
        .is_some_and(thread::JoinHandle::is_finished)
    {
        assert!(Instant::now() < until, "the worker never ended");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(run.sounds.latched.is_none());
    run.sounds.service(&mut run.audio.control);
    assert_eq!(run.sounds.latched, Some(Reason::WorkerGone));
    assert!(
        run.sounds.deferred.is_none(),
        "the slot outlived the receivers"
    );
    assert!(run.events().contains(&Event::Drained));
}

/// S5: a panic in one source's decode leaves its kinds silent, skips it
/// for the run and installs the rest; with its clear failing too, the run
/// latches; a worker that cannot start latches with nothing reserved; a
/// worker that ended at the lock before the startup admission latches by
/// the lock; a worker that ends mid-pick releases the reservation and
/// latches.
#[test]
fn worker_failures() {
    let panics = |seam: &Seam| {
        seam.rule(
            "A/clear.wav",
            Rule {
                panic: true,
                ..Rule::default()
            },
        );
    };
    let mut run = Run::with_seam(visits(), Opts::default(), panics);
    run.settle();
    assert!(run.sounds.latched.is_none());
    assert!(run.kept(SystemKind::Clear).is_none());
    assert!(run.kept(SystemKind::Select).is_some(), "the rest installed");
    assert_eq!(run.sounds.skipped.len(), 1);
    assert!(
        run.said()
            .iter()
            .any(|l| l.contains("skipped for this run")),
        "{:?}",
        run.said()
    );
    assert!(run.installed().is_some());
    drop(run);

    // The clear after the panic fails: the state's creation is the first
    // write, A's marker the second, its clear the third.
    let fs = MemFs::new();
    fs.fail_from(3);
    let opts = Opts {
        fs,
        ..Opts::default()
    };
    let mut run = Run::with_seam(visits(), opts, panics);
    run.until("the latch", |s| s.latched.is_some());
    assert!(matches!(run.sounds.latched, Some(Reason::Recovery(_))));
    run.settle();
    assert_eq!(run.sounds.charged(), 0);
    drop(run);

    let opts = Opts {
        spawn: failing_spawn,
        ..Opts::default()
    };
    let run = Run::new(visits(), opts);
    assert_eq!(run.sounds.latched, Some(Reason::WorkerGone));
    assert_eq!(run.sounds.charged(), 0);
    assert!(run.sounds.in_flight.is_none());
    assert!(
        run.said().iter().any(|l| l.contains("cannot start")),
        "{:?}",
        run.said()
    );
    drop(run);

    // The lock held by another instance, the worker ended before the
    // startup admission: the latch names the lock, not the worker.
    let fs = MemFs::new();
    fs.hold_lock();
    let opts = Opts {
        fs,
        spawn: finished_spawn,
        ..Opts::default()
    };
    let run = Run::new(visits(), opts);
    assert!(
        matches!(
            run.sounds.latched,
            Some(Reason::Recovery(Unavailable::Locked { .. }))
        ),
        "{:?}",
        run.sounds.latched
    );
    assert!(
        run.said()
            .iter()
            .any(|l| l.contains("held by another tapestry")),
        "{:?}",
        run.said()
    );
    assert_eq!(run.sounds.charged(), 0);
    drop(run);

    let opts = Opts::default();
    let panic = Arc::clone(&opts.picker_panics);
    let mut run = Run::new(visits(), opts);
    run.settle();
    run.entry();
    panic.store(true, Ordering::Release);
    run.pick(1);
    let b = run.entry();
    assert_eq!(run.sounds.in_flight.as_ref().map(|f| f.id), Some(b));
    run.until("the worker's end", |s| s.latched.is_some());
    assert_eq!(run.sounds.latched, Some(Reason::WorkerGone));
    assert!(run.sounds.in_flight.is_none(), "the reservation released");
    run.until("A back", |s| s.alive.is_empty());
    assert_eq!(run.sounds.charged(), 0);
}

/// S10: the settlement before recording waits, with no time bound, for the
/// pick in flight and for one an entry gave up on, so every decode of a
/// pick ends before it returns; the quit key leaves within a slice without
/// cancelling the pick, and the next settlement waits again.
#[test]
fn a_pick_holds_ready_until_it_reports() {
    // (a) In flight: the startup pick's read blocked.
    let (mut run, clock) = virtual_run(visits(), |seam| blocked(seam, "A/clear.wav", false));
    let gate = run.seam.clone();
    let cancel = Arc::clone(&run.sounds.in_flight.as_ref().unwrap().cancel);
    let said = Arc::clone(&run.said);
    let mut checked = false;
    let watch = Arc::clone(&clock);
    let settled = run.sounds.settle_before_recording(
        &mut run.audio.control,
        || {
            let now = watch.elapsed();
            if now > Duration::from_secs(60) {
                if !checked {
                    assert!(!cancel.load(Ordering::Acquire), "nothing cancelled");
                    assert!(said.lock().unwrap().is_empty(), "nothing said");
                    checked = true;
                    gate.open_gate();
                }
                real_slice();
            }
            false
        },
        || false,
    );
    let returned = clock.now();
    assert_eq!(settled, Ok(Settled::Ready));
    assert!(checked && clock.elapsed() > Duration::from_secs(60));
    let a = run.sounds.newest;
    assert_eq!(run.installed(), Some(a));
    decodes_ended_by(&run, &clock, returned);
    drop(run);

    // (b) Superseded at an entry's timeout: B's first read (its scratch)
    // blocked; B, cancelled once released, decodes no further file.
    let fx = visits();
    fx.sound("sound/B", SystemKind::Scratch, 10);
    let (mut run, clock) = virtual_run(fx, |seam| blocked(seam, "B/scratch.wav", false));
    run.settle();
    let a = run.entry();
    run.pick(1);
    let b = run.entry();
    run.entered("B/scratch.wav");
    assert_eq!(run.entry(), a);
    assert!(run.sounds.stalled);
    let gate = run.seam.clone();
    let watch = Arc::clone(&clock);
    let settled = run.sounds.settle_before_recording(
        &mut run.audio.control,
        || {
            if watch.elapsed() > Duration::from_secs(70) {
                gate.open_gate();
                real_slice();
            }
            false
        },
        || false,
    );
    let returned = clock.now();
    assert_eq!(settled, Ok(Settled::Ready));
    assert!(clock.elapsed() > Duration::from_secs(70));
    assert!(run.events().contains(&Event::Dropped(b)));
    assert!(!run.sounds.stalled);
    assert_eq!(run.installed(), Some(a));
    assert_eq!(run.seam.opens("B/clear.wav"), 0, "no further file");
    let b_calls: Vec<Call> = run
        .seam
        .calls()
        .into_iter()
        .filter(|c| c.path.ends_with("B/scratch.wav"))
        .collect();
    assert_eq!(b_calls.len(), 2);
    decodes_ended_by(&run, &clock, returned);
    drop(run);

    // (c) The quit key, in (a) and in (b): within the slice, the pick left
    // as it is, and the next settlement waits again.
    for superseded in [false, true] {
        let suffix = if superseded {
            "B/clear.wav"
        } else {
            "A/clear.wav"
        };
        let (mut run, clock) = virtual_run(visits(), |seam| blocked(seam, suffix, false));
        if superseded {
            run.settle();
            run.entry();
            run.pick(1);
            run.entry();
            run.entered("B/clear.wav");
            run.entry();
            assert!(run.sounds.stalled);
        }
        let cancelled_before = run
            .sounds
            .in_flight
            .as_ref()
            .unwrap()
            .cancel
            .load(Ordering::Acquire);
        for round in 0..2 {
            let mut slices = 0;
            let mut pressed_at = None;
            let watch = Arc::clone(&clock);
            let settled = run.sounds.settle_before_recording(
                &mut run.audio.control,
                || {
                    slices += 1;
                    if slices >= 5 + round {
                        pressed_at.get_or_insert(watch.elapsed());
                        return true;
                    }
                    false
                },
                || false,
            );
            assert_eq!(settled, Ok(Settled::Cancelled));
            assert_eq!(pressed_at, Some(clock.elapsed()), "left within the slice");
            let flight = run.sounds.in_flight.as_ref().unwrap();
            assert_eq!(
                flight.cancel.load(Ordering::Acquire),
                cancelled_before,
                "the pick left as it was"
            );
        }
        run.seam.open_gate();
        let settled = run.sounds.settle_before_recording(
            &mut run.audio.control,
            || {
                real_slice();
                false
            },
            || false,
        );
        assert_eq!(settled, Ok(Settled::Ready));
    }
}

/// S10: `start` of each set kind, the decision sound, `stop`, a full
/// ring's refusal, starts refused while latched and the decision sound as
/// the tone allocate and free nothing on this thread, the first exhaustion
/// latch inside the window included: the ring drained, the window's first
/// call latches by its refused draw, a `start(Scratch)`, a
/// `start(Decide)` (which asks for the tone) or a `stop`, one per run. The
/// mapping it invalidates moves into the slot, dropped and named only at
/// the next poll, before that poll releases the charge of the generation
/// the empty install sent back.
#[cfg(feature = "dev-tools")]
#[test]
fn system_sounds_start_and_stop_alloc() {
    use crate::alloc_count::{this_thread, this_thread_frees};
    use game::SEQ_LIMIT;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum First {
        Start,
        Decide,
        Stop,
    }

    let kinds = SystemKind::ALL;
    let decides = (0..3_000_usize)
        .filter(|k| k % 3 == 1 || (k % 3 == 0 && kinds[k % kinds.len()] == SystemKind::Decide))
        .count();
    let decides = u64::try_from(decides).unwrap();
    for first in [First::Start, First::Decide, First::Stop] {
        let fx = Fixture::new();
        for kind in [SystemKind::Clear, SystemKind::Scratch, SystemKind::Fail] {
            fx.sound("sound/A", kind, 100);
        }
        let mut run = Run::new(fx, Opts::default());
        run.settle();
        let a = run.entry();
        assert!(run.kept(SystemKind::Scratch).is_some() && run.kept(SystemKind::Fail).is_some());
        run.audio.hold(true);
        let control = &mut run.audio.control;
        let sounds = &mut run.sounds;
        let counts = || (this_thread(), this_thread_frees());

        // The positive control: one counted allocation and free.
        let before = counts();
        drop(std::hint::black_box(vec![0_u8; 16]));
        let after = counts();
        assert!(after.0 > before.0 && after.1 > before.1);

        // Unlatched, the worker held: the start ring fills and refuses.
        let before = counts();
        for k in 0..3_000_usize {
            let kind = kinds[k % kinds.len()];
            match k % 4 {
                0 => sounds.start(control, kind, None),
                1 => sounds.start(control, SystemKind::Select, Some(a)),
                2 => sounds.start(control, SystemKind::Decide, None),
                _ => sounds.stop(control, kind),
            }
        }
        assert_eq!(counts(), before, "allocations and frees, unlatched");
        assert!(
            control.counters().system_refused() > 0,
            "the full ring refused"
        );
        assert!(sounds.latched.is_none());

        // The first exhaustion latch inside the window, the ring drained so
        // a start reaches its draw.
        run.audio.step_now(2);
        let control = &mut run.audio.control;
        let sounds = &mut run.sounds;
        control.seed_system_seq(SEQ_LIMIT);
        let tones = control.failure_probe().tone_requests();
        let before = counts();
        match first {
            First::Start => sounds.start(control, SystemKind::Scratch, None),
            First::Decide => sounds.start(control, SystemKind::Decide, None),
            First::Stop => sounds.stop(control, SystemKind::Select),
        }
        assert!(sounds.latched.is_some(), "the first call latched");
        for k in 0..3_000_usize {
            let kind = kinds[k % kinds.len()];
            match k % 3 {
                0 => sounds.start(control, kind, None),
                1 => sounds.start(control, SystemKind::Decide, None),
                _ => sounds.stop(control, kind),
            }
        }
        assert_eq!(counts(), before, "allocations and frees, latching");
        assert_eq!(sounds.latched, Some(Reason::SequenceExhausted));
        assert_eq!(
            control.failure_probe().tone_requests(),
            tones + decides + u64::from(first == First::Decide),
            "the tone for each decision sound"
        );
        assert!(sounds.mapping.is_empty(), "the mapping moved out");
        assert!(sounds.deferred.is_some(), "the slot holds it, undropped");
        assert!(!sounds.events.contains(&Event::Drained));
        assert!(
            !run.said().iter().any(|l| l.contains("tone sounds")),
            "named only by the drain"
        );

        // The empty install sends A back; the next poll drops the slot
        // before it releases A's charge, and names the latch once.
        run.audio.step_now(2);
        let frees = this_thread_frees();
        run.poll();
        assert!(this_thread_frees() > frees);
        let events = run.events();
        let drained = events.iter().position(|&e| e == Event::Drained).unwrap();
        let released = events
            .iter()
            .position(|&e| e == Event::Released(a))
            .unwrap();
        assert!(drained < released, "{events:?}");
        run.poll();
        let named = run
            .said()
            .iter()
            .filter(|l| l.contains("tone sounds"))
            .count();
        assert_eq!(named, 1);
    }
}
