//! Loudness normalization on the game thread (design.md "Algorithms"): the
//! gain of the loaded chart's plays, the cache it is read from, and the
//! background job that analyzes a chart the cache misses.
//!
//! - **Lookup at load.** A chart's key is its resolved hash, its bank's
//!   sound-content digest, the mixer and analysis versions and the device
//!   rate. A measurement kept in memory, then the library's `loudness`
//!   table, gives the gain from the chart's first play; a miss plays at
//!   the 0.5 headroom and starts the job (at 1× only: other speeds read
//!   the 1× key and start none).
//! - **The job.** One thread named `loudness` per analysis, in background
//!   mode, that mixes the chart's 1× autoplay schedule offline and meters
//!   it ([`audio::Analysis`]). It pauses with the scan: a pause is asked
//!   when a chart is chosen and acknowledged before each countdown, so the
//!   job never runs during a play; it checks for a pause before building
//!   the analysis and before each block. However it ends, a guard
//!   publishes the end, a panic included.
//! - **Results.** A finished measurement is taken into memory, keyed by
//!   its whole key, when the job is collected or cancelled; it is stored
//!   in the library only before a countdown, once the scan's pause is
//!   acknowledged (or no scan runs), or at a graceful exit whose scan
//!   stopped, so the store never races a scan's commit. The loaded chart's
//!   gain changes only between plays, from a measurement of its own key:
//!   "from the next play".

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audio::{Analysis, AnalysisError, Gain, Measurement, SoundBank, Speed, Step};
use bms_model::Chart;
use game::Chain;
use library::{Library, LoudnessKey, LoudnessRow};

/// Where the job's thread is, for its hook: its start in the game (where
/// it enters background mode), and the barriers tests hold it at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// The thread started, before anything else.
    Start,
    /// Before the analysis is built (after the first pause check).
    #[cfg(test)]
    Construct,
    /// Before each block's step (after its pause check).
    #[cfg(test)]
    Step,
    /// Before the result is published.
    #[cfg(test)]
    Publish,
}

/// The hook the job's thread calls at each [`Point`].
pub type Hook = Arc<dyn Fn(Point) + Send + Sync>;

/// Why an analysis gave no measurement.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    /// The analysis failed.
    #[error("the loudness analysis failed")]
    Analysis(#[source] AnalysisError),
    /// The job's thread panicked.
    #[error("the loudness analysis stopped unexpectedly")]
    Panicked,
}

#[derive(Debug, Default)]
struct State {
    /// A pause is requested and not released.
    pause: bool,
    /// Counts pause requests, so each is acknowledged on its own.
    pause_gen: u64,
    /// The latest request the thread acknowledged.
    acked_gen: u64,
    /// The job is being stopped.
    shutdown: bool,
    /// The thread has ended, or is ending and will do nothing more.
    finished: bool,
    /// What the analysis came to, once it did.
    result: Option<Result<Measurement, JobError>>,
    /// The time spent analyzing, pauses excluded.
    busy: Duration,
}

impl State {
    fn paused(&self) -> bool {
        (self.pause && self.acked_gen == self.pause_gen) || self.finished
    }
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// Before the next piece of work: on a pause, acknowledges every
    /// request made of it and waits for the release. `false` when the job
    /// is being stopped.
    fn checkpoint(&self) -> bool {
        let mut state = self.lock();
        loop {
            if state.shutdown {
                return false;
            }
            if !state.pause {
                return true;
            }
            if state.acked_gen != state.pause_gen {
                state.acked_gen = state.pause_gen;
                self.changed.notify_all();
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Publishes the end of the job's thread however it ends, a panic
/// included, and wakes every waiter.
struct Ending(Arc<Shared>);

impl Drop for Ending {
    fn drop(&mut self) {
        let panicked = thread::panicking();
        self.0.update(|state| {
            if panicked && state.result.is_none() {
                state.result = Some(Err(JobError::Panicked));
            }
            state.finished = true;
        });
    }
}

/// A running analysis of one chart. Dropping it stops the analysis (it
/// ends within one block) and joins its thread.
pub struct Job {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    key: LoudnessKey,
    /// The chart's file name, for the log.
    name: String,
}

impl Job {
    /// Starts analyzing `chart`'s 1× autoplay schedule over `bank`, the
    /// measurement to be kept under `key` (`name` names the chart in the
    /// log); paused from the start if
    /// `paused`, so it holds only its two `Arc`s until released.
    ///
    /// # Errors
    ///
    /// Why the thread cannot be started.
    pub fn start(
        chart: Arc<Chart>,
        bank: Arc<SoundBank>,
        key: LoudnessKey,
        name: String,
        paused: bool,
        hook: Hook,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        if paused {
            shared.update(|s| {
                s.pause = true;
                s.pause_gen = 1;
            });
        }
        let ending = Ending(Arc::clone(&shared));
        let thread = thread::Builder::new()
            .name("loudness".to_owned())
            .spawn(move || {
                // `ending` drops last and sees any panic.
                let ending = ending;
                hook(Point::Start);
                let result = analyze(&ending.0, &chart, bank, &*hook);
                if let Some((result, busy)) = result {
                    #[cfg(test)]
                    hook(Point::Publish);
                    ending.0.update(|s| {
                        s.result = Some(result);
                        s.busy = busy;
                    });
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            key,
            name,
        })
    }

    /// The key its measurement is kept under.
    pub const fn key(&self) -> &LoudnessKey {
        &self.key
    }

    /// Asks the job to pause before its next block. Returns at once.
    pub fn request_pause(&self) {
        self.shared.update(|s| {
            s.pause = true;
            s.pause_gen += 1;
        });
    }

    /// Waits up to `timeout` for the latest pause request to be
    /// acknowledged. `true` once the job does nothing: paused, or
    /// finished.
    pub fn wait_paused(&self, timeout: Duration) -> bool {
        self.wait_for(timeout, State::paused)
    }

    /// Releases a pause; the analysis goes on.
    pub fn release(&self) {
        self.shared.update(|s| s.pause = false);
    }

    /// Whether the job has ended, with or without a measurement.
    pub fn finished(&self) -> bool {
        self.shared.lock().finished
    }

    /// The result of a finished job, once: `None` while it runs, or once
    /// taken, or when it was stopped before its end.
    pub fn take_result(&self) -> Option<Result<Measurement, JobError>> {
        let mut state = self.shared.lock();
        if state.finished {
            state.result.take()
        } else {
            None
        }
    }

    /// The time the analysis spent working, pauses and the wait for its
    /// collection excluded.
    pub fn busy(&self) -> Duration {
        self.shared.lock().busy
    }

    fn wait_for(&self, timeout: Duration, done: impl Fn(&State) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.lock();
        loop {
            if done(&state) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Stops the job, waiting at most `timeout` for it to end: `true` once
    /// it has (its thread is joined), `false` when it had not, and its
    /// thread is left to end on its own.
    pub fn stop_within(mut self, timeout: Duration) -> bool {
        self.shared.update(|s| s.shutdown = true);
        let finished = self.wait_for(timeout, |s| s.finished);
        if finished {
            self.join();
        } else {
            drop(self.thread.take());
        }
        finished
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            // A panic is published by the guard.
            let _ = t.join();
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.shared.update(|s| s.shutdown = true);
        self.join();
    }
}

/// The job's work, with the time spent on it outside pauses: `None` when
/// it was stopped.
#[cfg_attr(not(test), expect(unused_variables, reason = "the hook's test points"))]
fn analyze(
    shared: &Shared,
    chart: &Chart,
    bank: Arc<SoundBank>,
    hook: &dyn Fn(Point),
) -> Option<(Result<Measurement, JobError>, Duration)> {
    if !shared.checkpoint() {
        return None;
    }
    #[cfg(test)]
    hook(Point::Construct);
    let began = Instant::now();
    let mut analysis = match Analysis::new(bank) {
        Ok(a) => a,
        Err(e) => return Some((Err(JobError::Analysis(e)), began.elapsed())),
    };
    let mut events = game::schedule_iter(chart);
    let mut busy = began.elapsed();
    loop {
        if !shared.checkpoint() {
            return None;
        }
        #[cfg(test)]
        hook(Point::Step);
        let began = Instant::now();
        let step = analysis.step(&mut events);
        busy += began.elapsed();
        match step {
            Ok(Step::More) => {}
            Ok(Step::Done(m)) => return Some((Ok(m), busy)),
            Err(e) => return Some((Err(JobError::Analysis(e)), busy)),
        }
    }
}

/// A measurement as a cached row.
pub const fn row(m: &Measurement) -> LoudnessRow {
    LoudnessRow {
        integrated: m.integrated,
        true_peak: m.true_peak,
        over_range: m.over_range,
    }
}

/// The gain a cached row gives.
pub fn gain(row: &LoudnessRow) -> Gain {
    let g = audio::normalization_gain(&Measurement {
        integrated: row.integrated,
        true_peak: row.true_peak,
        over_range: row.over_range,
        frames: 0,
    });
    Gain::new(g).unwrap_or(Gain::HEADROOM)
}

/// The key of a chart loaded with `bank` at `rate`, when its sounds have a
/// digest.
pub fn key(chart: &Chart, bank: &SoundBank) -> Option<LoudnessKey> {
    Some(LoudnessKey {
        resolved_hash: chart.identity.resolved_hash,
        sounds: bank.sound_digest()?,
        mixer_version: audio::MIXER_VERSION,
        analysis_version: audio::ANALYSIS_VERSION,
        rate: bank.rate(),
    })
}

fn log_line(line: &str, warn: bool) {
    if warn {
        eprintln!("{line}");
        log::warn!("{line}");
    } else {
        println!("{line}");
        log::info!("{line}");
    }
}

/// The gain of the loaded chart's plays, the measurements not yet stored,
/// and the job.
pub struct Loudness {
    job: Option<Job>,
    /// Finished measurements not yet stored, by their whole key.
    measured: HashMap<LoudnessKey, LoudnessRow>,
    /// The loaded chart's key, when its bank has a digest.
    loaded: Option<LoudnessKey>,
    /// The gain of the loaded chart's next play.
    gain: Gain,
    /// Whether `gain` comes from a measurement of `loaded`.
    normalized: bool,
    hook: Hook,
}

impl Loudness {
    /// No chart loaded; jobs call `hook` at each [`Point`].
    pub fn new(hook: Hook) -> Self {
        Self {
            job: None,
            measured: HashMap::new(),
            loaded: None,
            gain: Gain::HEADROOM,
            normalized: false,
            hook,
        }
    }

    /// The gain of the loaded chart's next play.
    pub const fn gain(&self) -> Gain {
        self.gain
    }

    /// Whether a job is analyzing (not yet finished).
    pub fn analyzing(&self) -> bool {
        self.job.as_ref().is_some_and(|j| !j.finished())
    }

    /// The running job, for the pause.
    pub const fn job(&self) -> Option<&Job> {
        self.job.as_ref()
    }

    /// A finished measurement kept in memory under `key`.
    #[cfg(test)]
    pub fn in_memory(&self, key: &LoudnessKey) -> Option<LoudnessRow> {
        self.measured.get(key).copied()
    }

    /// Takes a finished job's result into memory, logging it, and drops
    /// the job; a job still running is left.
    fn take_finished(&mut self) {
        let Some(job) = self.job.take_if(|j| j.finished()) else {
            return;
        };
        let name = &job.name;
        match job.take_result() {
            Some(Ok(m)) => {
                let g = audio::normalization_gain(&m);
                log_line(
                    &format!(
                        "loudness: {name} analyzed in {} ms: {} LUFS{}, {:.2} dBTP; gain {:.2} dB from the next play",
                        job.busy().as_millis(),
                        m.integrated
                            .map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
                        if m.over_range { " (over range)" } else { "" },
                        20.0 * m.true_peak.max(1e-10).log10(),
                        20.0 * f64::from(g).log10(),
                    ),
                    false,
                );
                self.measured.insert(*job.key(), row(&m));
            }
            Some(Err(e)) => log_line(
                &format!(
                    "loudness: {name}: {}; it plays at the headroom until it is loaded again",
                    Chain(&e)
                ),
                true,
            ),
            None => {}
        }
    }

    /// Before another chart's sounds are decoded: a finished measurement
    /// is taken into memory; a running job stops (within one block) and
    /// drops its bank, so the bank peak stays the engine's old and new.
    pub fn cancel(&mut self) {
        self.take_finished();
        self.job = None;
    }

    /// The chart `name` was loaded with `bank`: its gain from a
    /// measurement of its key in memory, else in `library`; on a miss, the
    /// headroom, and an analysis started (paused if `paused`) when the bank
    /// is at 1× and has a digest.
    pub fn load(
        &mut self,
        library: &Library,
        chart: Arc<Chart>,
        bank: Arc<SoundBank>,
        name: String,
        paused: bool,
    ) {
        self.cancel();
        self.loaded = key(&chart, &bank);
        self.gain = Gain::HEADROOM;
        self.normalized = false;
        let Some(key) = self.loaded else {
            return;
        };
        if let Some(row) = self.lookup(library, &key) {
            self.gain = gain(&row);
            self.normalized = true;
            return;
        }
        if bank.speed() != Speed::NORMAL {
            return;
        }
        match Job::start(chart, bank, key, name, paused, Arc::clone(&self.hook)) {
            Ok(job) => self.job = Some(job),
            Err(e) => log_line(
                &format!("loudness: the analysis cannot start: {}", Chain(&e)),
                true,
            ),
        }
    }

    /// The row of `key`: in memory, else in `library`; a database error is
    /// logged and counts as a miss.
    fn lookup(&self, library: &Library, key: &LoudnessKey) -> Option<LoudnessRow> {
        if let Some(row) = self.measured.get(key) {
            return Some(*row);
        }
        match library.loudness(key) {
            Ok(row) => row,
            Err(e) => {
                log_line(&format!("loudness: {}", Chain(&e)), true);
                None
            }
        }
    }

    /// Before a countdown, with the scan quiet (its pause acknowledged, or
    /// none running) and the job paused: a finished measurement is taken,
    /// every measurement in memory is stored, and the loaded chart's gain
    /// is updated from a measurement of its own key. A store that fails is
    /// logged and kept in memory for this run.
    pub fn store(&mut self, library: &mut Library) {
        self.take_finished();
        self.flush(library);
        if !self.normalized
            && let Some(key) = self.loaded
            && let Some(row) = self.lookup(library, &key)
        {
            self.gain = gain(&row);
            self.normalized = true;
        }
    }

    /// Stores every measurement in memory; one the database refuses is
    /// logged and kept.
    fn flush(&mut self, library: &mut Library) {
        self.measured
            .retain(|key, row| match library.store_loudness(key, row) {
                Ok(()) => false,
                Err(e) => {
                    log_line(
                        &format!(
                            "loudness: the measurement cannot be stored: {}; it is kept for this run",
                            Chain(&e)
                        ),
                        true,
                    );
                    true
                }
            });
    }

    /// At the end of a run: a finished measurement is stored if the scan
    /// stopped (`quiet`), a running analysis is stopped and discarded.
    pub fn close(&mut self, library: &mut Library, quiet: bool, timeout: Duration) {
        self.take_finished();
        if let Some(job) = self.job.take()
            && !job.stop_within(timeout)
        {
            log_line(
                "loudness: the analysis did not stop in time; it ends with the process",
                true,
            );
        }
        if quiet {
            self.flush(library);
        } else if !self.measured.is_empty() {
            log_line(
                "loudness: the scan did not stop, so the measurements are not stored",
                true,
            );
        }
    }
}

/// Charts, banks and a gate for the job's tests, shared with the shelf's.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use audio::SoundBank;
    use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};

    use super::{Hook, Point};

    /// The rate the test banks are decoded at: low, so a debug build
    /// analyzes seconds of sound quickly.
    pub const RATE: u32 = 8_000;

    /// How long a test waits for what must happen however late a loaded
    /// machine runs it: it only catches what never happens.
    pub const WATCHDOG: Duration = Duration::from_secs(600);

    /// A mono 16-bit WAV of `samples` at [`RATE`].
    pub fn wav(samples: &[i16]) -> Vec<u8> {
        let data = u32::try_from(samples.len() * 2).unwrap();
        let mut b = Vec::new();
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
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    /// `seconds` of a square wave of `amplitude` (a period of 20 samples).
    pub fn square(seconds: f64, amplitude: i16) -> Vec<i16> {
        let n = (seconds * f64::from(RATE)) as usize;
        (0..n)
            .map(|i| if i % 20 < 10 { amplitude } else { -amplitude })
            .collect()
    }

    /// Writes `samples` as `dir/key.wav`.
    pub fn write_sound(dir: &Path, samples: &[i16]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("key.wav"), wav(samples)).unwrap();
    }

    /// A chart playing `dir/key.wav` as BGM at 0, its resolved hash all
    /// `id`.
    pub fn chart(dir: &Path, id: u8) -> Arc<Chart> {
        let mut chart = ChartBuilder::new(120_000)
            .sound("key.wav")
            .bgm(TimeUs(0), SoundId(0))
            .build()
            .unwrap();
        chart.sounds[0].path = Some(dir.join("key.wav"));
        chart.identity.resolved_hash = [id; 32];
        Arc::new(chart)
    }

    /// `chart`'s sounds, decoded at [`RATE`].
    pub fn bank(chart: &Chart) -> Arc<SoundBank> {
        Arc::new(audio::load_bank(chart, RATE, &audio::BankLimits::default()).unwrap())
    }

    #[derive(Debug, Default)]
    struct Counts {
        /// Per point, how many may pass (`usize::MAX`: all).
        allowed: [usize; 4],
        /// Per point, how many reached it.
        reached: [usize; 4],
        /// The step to panic at.
        panic_at: Option<usize>,
    }

    /// Holds the job's thread at its points until the test lets it pass.
    #[derive(Debug, Default)]
    pub struct Gate {
        counts: Mutex<Counts>,
        changed: Condvar,
    }

    const fn index(p: Point) -> usize {
        match p {
            Point::Start => 0,
            Point::Construct => 1,
            Point::Step => 2,
            Point::Publish => 3,
        }
    }

    impl Gate {
        /// A gate letting everything pass but `held` points.
        pub fn holding(held: &[Point]) -> Arc<Self> {
            let gate = Self::default();
            {
                let mut c = gate.counts.lock().unwrap();
                c.allowed = [usize::MAX; 4];
                for &p in held {
                    c.allowed[index(p)] = 0;
                }
            }
            Arc::new(gate)
        }

        /// The hook that passes through this gate.
        pub fn hook(self: &Arc<Self>) -> Hook {
            let gate = Arc::clone(self);
            Arc::new(move |p| gate.pass(p))
        }

        fn pass(&self, p: Point) {
            let i = index(p);
            let mut c = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
            let n = c.reached[i];
            c.reached[i] += 1;
            self.changed.notify_all();
            while c.allowed[i] <= n {
                c = self.changed.wait(c).unwrap_or_else(PoisonError::into_inner);
            }
            if p == Point::Step && c.panic_at == Some(n) {
                drop(c);
                panic!("a step failed (test)");
            }
        }

        /// Lets `n` more of point `p` pass.
        pub fn allow(&self, p: Point, n: usize) {
            let mut c = self.counts.lock().unwrap();
            let i = index(p);
            c.allowed[i] = c.allowed[i].saturating_add(n);
            self.changed.notify_all();
        }

        /// Lets every point pass from now on.
        pub fn open(&self) {
            self.counts.lock().unwrap().allowed = [usize::MAX; 4];
            self.changed.notify_all();
        }

        /// Panics the thread at step `n` (from 0).
        pub fn panic_at(&self, n: usize) {
            self.counts.lock().unwrap().panic_at = Some(n);
        }

        /// Waits until `n` of point `p` were reached.
        pub fn reached(&self, p: Point, n: usize) {
            let begun = Instant::now();
            let mut c = self.counts.lock().unwrap();
            while c.reached[index(p)] < n {
                assert!(begun.elapsed() < WATCHDOG, "{p:?} {n} never reached");
                c = self
                    .changed
                    .wait_timeout(c, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }

        /// How many of point `p` were reached.
        pub fn count(&self, p: Point) -> usize {
            self.counts.lock().unwrap().reached[index(p)]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use audio::Gain;
    use game::DataRoot;
    use library::Library;

    use super::fixture::{Gate, WATCHDOG, bank, chart, square, write_sound};
    use super::{Job, JobError, Loudness, Point};

    fn library(dir: &Path) -> Library {
        crate::shelf::open(&DataRoot::new(dir.join("data")))
            .unwrap()
            .0
    }

    fn open_hook() -> super::Hook {
        Arc::new(|_| {})
    }

    /// Waits for the job to finish, the shelf released.
    fn finished(loudness: &Loudness) {
        let begun = Instant::now();
        while loudness.analyzing() {
            assert!(begun.elapsed() < WATCHDOG, "the analysis never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A chart and its `key.wav` in a folder, analyzed and stored; the file
    /// is then overwritten with other samples of the same size and its old
    /// modification time, which the chart's resolved hash (sound names
    /// only) cannot see: the reload's key differs by its sound digest, the
    /// stored row misses, and a new analysis measures the new file.
    #[test]
    fn replacing_a_sound_file_invalidates_the_analysis() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 4_000));
        let wav = songs.join("key.wav");
        let mtime = std::fs::metadata(&wav).unwrap().modified().unwrap();
        let mut loudness = Loudness::new(open_hook());

        let c = chart(&songs, 1);
        let b = bank(&c);
        let first = super::key(&c, &b).unwrap();
        loudness.load(&lib, c, b, "a.bms".to_owned(), false);
        assert!(loudness.job().is_some());
        finished(&loudness);
        loudness.store(&mut lib);
        let stored = lib.loudness(&first).unwrap().unwrap();
        assert_eq!(loudness.gain(), super::gain(&stored));

        // The same size, other samples, the old time.
        write_sound(&songs, &square(2.0, 400));
        std::fs::File::options()
            .write(true)
            .open(&wav)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(std::fs::metadata(&wav).unwrap().modified().unwrap(), mtime);
        let c = chart(&songs, 1);
        let b = bank(&c);
        let second = super::key(&c, &b).unwrap();
        assert_eq!(second.resolved_hash, first.resolved_hash);
        assert_ne!(second.sounds, first.sounds);
        assert_eq!(lib.loudness(&second).unwrap(), None);
        loudness.load(&lib, c, b, "a.bms".to_owned(), false);
        assert!(loudness.job().is_some(), "the stale row was taken");
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        finished(&loudness);
        loudness.store(&mut lib);
        let again = lib.loudness(&second).unwrap().unwrap();
        // 20 dB quieter.
        let l = |r: &library::LoudnessRow| r.integrated.unwrap();
        assert!(
            (l(&stored) - l(&again) - 20.0).abs() < 0.5,
            "{stored:?} {again:?}"
        );
    }

    /// A measurement taken from a finished job is kept in memory by its
    /// whole key until stored: the same chart loaded again before the
    /// store plays at its gain at once and starts no analysis; another
    /// chart does not take it; a store the database refuses keeps it for
    /// the run.
    #[test]
    fn measurements_are_handed_over_by_their_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let (a_dir, b_dir) = (dir.path().join("a"), dir.path().join("b"));
        write_sound(&a_dir, &square(1.0, 3_000));
        write_sound(&b_dir, &square(1.0, 3_000));
        let mut loudness = Loudness::new(open_hook());
        let a = chart(&a_dir, 1);
        let a_bank = bank(&a);
        let a_key = super::key(&a, &a_bank).unwrap();
        loudness.load(
            &lib,
            Arc::clone(&a),
            Arc::clone(&a_bank),
            "a".to_owned(),
            false,
        );
        finished(&loudness);
        // Reloaded before any store: from memory.
        loudness.load(
            &lib,
            Arc::clone(&a),
            Arc::clone(&a_bank),
            "a".to_owned(),
            false,
        );
        assert!(loudness.job().is_none());
        assert_ne!(loudness.gain(), Gain::HEADROOM);
        let a_gain = loudness.gain();
        assert!(loudness.in_memory(&a_key).is_some());
        assert_eq!(lib.loudness(&a_key).unwrap(), None);
        // Another chart: the headroom and its own analysis.
        let b = chart(&b_dir, 2);
        let b_bank = bank(&b);
        loudness.load(&lib, b, b_bank, "b".to_owned(), true);
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        assert!(loudness.job().is_some());
        loudness.store(&mut lib);
        assert_eq!(loudness.gain(), Gain::HEADROOM, "b took a's gain");
        assert!(lib.loudness(&a_key).unwrap().is_some());

        // A store the database refuses: kept in memory for the run.
        let db = dir.path().join("data").join("library.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("DROP TABLE loudness;")
            .unwrap();
        let mut fresh = Loudness::new(open_hook());
        fresh.load(
            &lib,
            Arc::clone(&a),
            Arc::clone(&a_bank),
            "a".to_owned(),
            false,
        );
        finished(&fresh);
        fresh.store(&mut lib);
        assert!(fresh.in_memory(&a_key).is_some());
        assert_eq!(fresh.gain(), a_gain);
        fresh.load(&lib, a, a_bank, "a".to_owned(), false);
        assert_eq!(fresh.gain(), a_gain);
        assert!(fresh.job().is_none());
    }

    /// A job whose step panics ends Failed: no measurement, no row, the
    /// chart stays at the headroom; the next load analyzes again.
    #[test]
    fn a_failed_analysis_leaves_the_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(1.0, 3_000));
        // One second at 8 kHz is two blocks: the second fails.
        let gate = Gate::holding(&[]);
        gate.panic_at(1);
        let mut loudness = Loudness::new(gate.hook());
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();
        loudness.load(&lib, Arc::clone(&c), Arc::clone(&b), "a".to_owned(), false);
        finished(&loudness);
        let job = loudness.job().unwrap();
        assert!(matches!(job.take_result(), Some(Err(JobError::Panicked))));
        loudness.store(&mut lib);
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        assert_eq!(lib.loudness(&key).unwrap(), None);
        assert!(loudness.in_memory(&key).is_none());
        // The next load starts another analysis.
        let mut retried = Loudness::new(open_hook());
        retried.load(&lib, c, b, "a".to_owned(), false);
        assert!(retried.job().is_some());
    }

    /// The job's pause, block by block: a pause asked during a block is
    /// acknowledged at the next block's check, and each request on its own
    /// (successive generations); a job paused from the start builds
    /// nothing until released; a pause asked during the last block is
    /// answered by the end, and so is one asked during a failing block.
    #[test]
    fn the_job_pauses_between_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 3_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();

        // Paused from the start: nothing is built until the release.
        let gate = Gate::holding(&[Point::Step, Point::Publish]);
        let begun = Instant::now();
        let job = Job::start(
            Arc::clone(&c),
            Arc::clone(&b),
            key,
            "a".to_owned(),
            true,
            gate.hook(),
        )
        .unwrap();
        assert!(job.wait_paused(WATCHDOG));
        assert_eq!(gate.count(Point::Construct), 0);
        job.release();
        gate.reached(Point::Step, 1);
        assert_eq!(gate.count(Point::Construct), 1);
        // Asked during block 0: not acknowledged until the block is over.
        job.request_pause();
        assert!(!job.wait_paused(Duration::from_millis(50)));
        gate.allow(Point::Step, 1);
        assert!(job.wait_paused(WATCHDOG));
        assert_eq!(gate.count(Point::Step), 1);
        // Released and asked again: the second request is acknowledged on
        // its own.
        job.release();
        gate.reached(Point::Step, 2);
        job.request_pause();
        assert!(!job.wait_paused(Duration::from_millis(50)));
        gate.allow(Point::Step, 1);
        assert!(job.wait_paused(WATCHDOG));
        // Released to its last block (two seconds at 8 kHz: four blocks),
        // held there a while, and a pause asked: the block ends the
        // analysis, and only its publication answers the pause.
        job.release();
        gate.allow(Point::Step, 1);
        gate.reached(Point::Step, 4);
        let held = Instant::now();
        job.request_pause();
        std::thread::sleep(Duration::from_millis(200));
        let held = held.elapsed();
        gate.allow(Point::Step, 1);
        gate.reached(Point::Publish, 1);
        assert!(!job.wait_paused(Duration::from_millis(50)));
        assert!(!job.finished());
        gate.allow(Point::Publish, 1);
        assert!(job.wait_paused(WATCHDOG));
        assert!(job.finished());
        let wall = begun.elapsed();
        assert_eq!(gate.count(Point::Step), 4);
        assert!(matches!(job.take_result(), Some(Ok(_))));
        assert!(job.take_result().is_none(), "taken twice");
        // The time it reports excludes its pauses and holds.
        assert!(
            job.busy() + held <= wall,
            "{:?} {held:?} {wall:?}",
            job.busy()
        );

        // A pause asked during a block that fails: answered by the end.
        let gate = Gate::holding(&[Point::Step]);
        gate.allow(Point::Step, 1);
        gate.panic_at(1);
        let job = Job::start(c, b, key, "a".to_owned(), false, gate.hook()).unwrap();
        gate.reached(Point::Step, 2);
        job.request_pause();
        gate.allow(Point::Step, 1);
        assert!(job.wait_paused(WATCHDOG));
        assert!(job.finished());
        assert!(matches!(job.take_result(), Some(Err(JobError::Panicked))));
    }

    /// Cancelling a running job stops it within its block, before a chart's
    /// sounds are decoded: it drops its bank, and keeps no measurement; a
    /// job stopped while paused ends at once.
    #[test]
    fn cancelling_stops_the_job_and_drops_its_bank() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 3_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();

        // Running, held in its third block.
        let gate = Gate::holding(&[Point::Step]);
        gate.allow(Point::Step, 2);
        let mut loudness = Loudness::new(gate.hook());
        loudness.load(&lib, Arc::clone(&c), Arc::clone(&b), "a".to_owned(), false);
        gate.reached(Point::Step, 3);
        assert_eq!(Arc::strong_count(&b), 2);
        std::thread::scope(|s| {
            s.spawn(|| {
                // The cancel waits for the block in hand.
                std::thread::sleep(Duration::from_millis(50));
                gate.open();
            });
            loudness.cancel();
        });
        assert!(loudness.job().is_none());
        assert_eq!(Arc::strong_count(&b), 1, "the job kept its bank");
        assert!(loudness.in_memory(&key).is_none());
        assert_eq!(gate.count(Point::Publish), 0);

        // Paused from the start, then stopped.
        let gate = Gate::holding(&[]);
        let mut loudness = Loudness::new(gate.hook());
        loudness.load(&lib, c, Arc::clone(&b), "a".to_owned(), true);
        assert!(loudness.job().unwrap().wait_paused(WATCHDOG));
        loudness.cancel();
        assert_eq!(Arc::strong_count(&b), 1);
        assert_eq!(gate.count(Point::Construct), 0);
    }

    /// A job stopped with `stop_within` ends within its bound while paused,
    /// publishing nothing.
    #[test]
    fn a_paused_job_stops_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let songs = dir.path().join("song");
        write_sound(&songs, &square(1.0, 3_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();
        let gate = Gate::holding(&[]);
        let job = Job::start(c, Arc::clone(&b), key, "a".to_owned(), true, gate.hook()).unwrap();
        assert!(job.wait_paused(WATCHDOG));
        assert!(job.stop_within(WATCHDOG));
        assert_eq!(Arc::strong_count(&b), 1);
        assert_eq!(gate.count(Point::Publish), 0);
    }
}
