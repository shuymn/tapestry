//! Loudness normalization on the game thread (design.md "Algorithms"): the
//! gain of the loaded chart's plays, the cache it is read from, and the
//! analysis a load runs when the cache misses.
//!
//! - **Lookup at load.** A chart's key is its resolved hash, its bank's
//!   sound-content digest, the mixer and analysis versions and the device
//!   rate. A measurement kept in memory, then the library's `loudness`
//!   table, gives the gain of the chart's plays.
//! - **Measured at load.** On a miss the load measures the chart before its
//!   first play ([`measure`]): its 1× autoplay schedule mixed offline and
//!   metered ([`audio::Analysis`]) on the loading thread, block by block,
//!   the load's cancel checked before each. A bank at another speed is
//!   measured through the chart and its bank read again at 1×, under the
//!   same 1× key. An analysis that fails (a panic included), is cancelled,
//!   or cannot get its 1× bank leaves the 0.5 headroom and keeps nothing,
//!   so the next load measures again; a sound that cannot be read is
//!   measured as it plays, silent. A load cancelled during its measurement
//!   leaves the chart to the background measurement ([`crate::background`]),
//!   whose result is kept here as a load's own ([`Loudness::adopt`]).
//! - **Results.** A measurement is kept in memory by its whole key and
//!   stored in the library only before a countdown, once the scan's pause
//!   is acknowledged (or no scan runs), or at a graceful exit whose scan
//!   stopped, so the store never races a scan's commit; one the database
//!   refuses stays in memory for the run. Without a library (`tapestry
//!   play`) each load measures and nothing is stored.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audio::{Analysis, AnalysisError, Gain, Measurement, SoundBank, Speed, Step};
use bms_model::Chart;
use game::{Chain, Loading};
use library::{Library, LoudnessKey, LoudnessRow};

/// What a load lends its loudness measurement.
pub trait Host {
    /// The chart and its bank read again at 1×, for a bank at another
    /// speed: the 1× key's measurement.
    ///
    /// # Errors
    ///
    /// Why they cannot be read.
    fn at_normal(&mut self) -> anyhow::Result<(Chart, Arc<SoundBank>)>;

    /// The measurement starts: the load says so.
    fn measuring(&mut self, name: &str);

    /// Asked before each block: whether the load was cancelled (the window
    /// closing, the quit key).
    fn cancelled(&mut self) -> bool;
}

/// How an analysis ended.
#[derive(Debug)]
pub(crate) enum Analyzed {
    /// Measured, in this much time.
    Done(Measurement, Duration),
    /// The load was cancelled, or the background measurement stopped.
    Cancelled,
    /// Why it gave no measurement.
    Failed(String),
}

/// Mixes and meters `chart`'s 1× autoplay schedule over `bank`, asking
/// `cancelled` before each block. A panic of the analysis is a failure.
pub(crate) fn analyze(
    chart: &Chart,
    bank: Arc<SoundBank>,
    cancelled: &mut dyn FnMut() -> bool,
) -> Analyzed {
    let began = Instant::now();
    let run = panic::catch_unwind(AssertUnwindSafe(
        || -> Result<Option<Measurement>, AnalysisError> {
            let mut analysis = Analysis::new(bank)?;
            let mut events = game::schedule_iter(chart);
            // A script's slow measurement (dev-tools).
            #[cfg(feature = "dev-tools")]
            if !crate::dev::hold_measuring(&mut *cancelled) {
                return Ok(None);
            }
            loop {
                if cancelled() {
                    return Ok(None);
                }
                match analysis.step(&mut events)? {
                    Step::More => {}
                    Step::Done(m) => return Ok(Some(m)),
                }
            }
        },
    ));
    match run {
        Ok(Ok(Some(m))) => Analyzed::Done(m, began.elapsed()),
        Ok(Ok(None)) => Analyzed::Cancelled,
        Ok(Err(e)) => Analyzed::Failed(format!("the loudness analysis failed: {}", Chain(&e))),
        Err(_) => Analyzed::Failed("the loudness analysis stopped unexpectedly".to_owned()),
    }
}

/// Measures `chart` loaded with `bank` (`name` names it in the log): at a
/// bank's other speed, the chart and bank `host` reads again at 1×, whose
/// key must be `key` when one is given. `Ok(None)`, logged, when it cannot
/// be measured.
///
/// # Errors
///
/// [`Cancelled`] when the load was cancelled during the measurement.
pub fn measure(
    chart: &Chart,
    bank: &Arc<SoundBank>,
    key: Option<&LoudnessKey>,
    name: &str,
    host: &mut dyn Host,
) -> Result<Option<Measurement>, Cancelled> {
    host.measuring(name);
    log_line(&format!("loudness: measuring {name}"), false);
    let analyzed = if bank.speed() == Speed::NORMAL {
        analyze(chart, Arc::clone(bank), &mut || host.cancelled())
    } else {
        match host.at_normal() {
            Ok((c, b)) if key.is_none_or(|k| self::key(&c, &b).as_ref() == Some(k)) => {
                analyze(&c, b, &mut || host.cancelled())
            }
            Ok(_) => Analyzed::Failed("the sounds changed while they were read again".to_owned()),
            // The quit key or a close stopped the 1× sounds' decode.
            Err(_) if host.cancelled() => Analyzed::Cancelled,
            Err(e) => Analyzed::Failed(format!("the 1× sounds cannot be read: {e:#}")),
        }
    };
    match analyzed {
        Analyzed::Done(m, took) => {
            log_measured(name, &m, took, "");
            Ok(Some(m))
        }
        Analyzed::Cancelled => {
            log_line(
                &format!("loudness: {name}: the measurement was cancelled"),
                false,
            );
            Err(Cancelled)
        }
        Analyzed::Failed(why) => {
            log_line(
                &format!("loudness: {name}: {why}; it plays at the headroom"),
                true,
            );
            Ok(None)
        }
    }
}

/// Logs `name`'s measurement `m`, taken in `took` (`place` says where:
/// empty for a load's own).
pub(crate) fn log_measured(name: &str, m: &Measurement, took: Duration, place: &str) {
    let g = audio::normalization_gain(m);
    log_line(
        &format!(
            "loudness: {name} measured{place} in {} ms: {} LUFS{}, {:.2} dBTP; gain {:.2} dB",
            took.as_millis(),
            m.integrated
                .map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
            if m.over_range { " (over range)" } else { "" },
            20.0 * m.true_peak.max(1e-10).log10(),
            20.0 * f64::from(g).log10(),
        ),
        false,
    );
}

/// The load was cancelled during the measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled;

/// The gain of a play of `chart` loaded with `bank` without a library
/// (`tapestry play`): measured at each load, the headroom when it cannot
/// be or the measurement was cancelled (the run then ends before a play).
pub fn unstored_gain(
    chart: &Chart,
    bank: &Arc<SoundBank>,
    name: &str,
    host: &mut dyn Host,
) -> Gain {
    match measure(chart, bank, None, name, host) {
        Ok(Some(m)) => gain(&row(&m)),
        Ok(None) | Err(Cancelled) => Gain::HEADROOM,
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

pub(crate) fn log_line(line: &str, warn: bool) {
    if warn {
        eprintln!("{line}");
        log::warn!("{line}");
    } else {
        println!("{line}");
        log::info!("{line}");
    }
}

/// The gain of the loaded chart's plays and the measurements not yet
/// stored.
#[derive(Debug)]
pub struct Loudness {
    /// Measurements not yet stored, by their whole key.
    measured: HashMap<LoudnessKey, LoudnessRow>,
    /// The gain of the loaded chart's plays.
    gain: Gain,
}

impl Default for Loudness {
    fn default() -> Self {
        Self {
            measured: HashMap::new(),
            gain: Gain::HEADROOM,
        }
    }
}

impl Loudness {
    /// The gain of the loaded chart's plays.
    pub const fn gain(&self) -> Gain {
        self.gain
    }

    /// A measurement kept in memory under `key`.
    #[cfg(test)]
    pub fn in_memory(&self, key: &LoudnessKey) -> Option<LoudnessRow> {
        self.measured.get(key).copied()
    }

    /// The chart `name` was loaded with `bank`: its gain from a measurement
    /// of its key in memory, else in `library`; on a miss it is measured
    /// now ([`measure`]) and kept in memory, or plays at the headroom when
    /// it cannot be. A bank without a digest is never cached and plays at
    /// the headroom. [`Loading::Cancelled`] when the load was cancelled
    /// during the measurement: the load stops there.
    pub fn load(
        &mut self,
        library: &Library,
        chart: &Chart,
        bank: &Arc<SoundBank>,
        name: &str,
        host: &mut dyn Host,
    ) -> Loading {
        self.gain = Gain::HEADROOM;
        let Some(key) = key(chart, bank) else {
            return Loading::Ready;
        };
        if let Some(row) = self.lookup(library, &key) {
            self.gain = gain(&row);
            log_line(
                &format!(
                    "loudness: {name}: gain {:.2} dB, measured before",
                    20.0 * f64::from(self.gain.get()).log10()
                ),
                false,
            );
            return Loading::Ready;
        }
        match measure(chart, bank, Some(&key), name, host) {
            Ok(Some(m)) => {
                let row = row(&m);
                self.measured.insert(key, row);
                self.gain = gain(&row);
                Loading::Ready
            }
            Ok(None) => Loading::Ready,
            Err(Cancelled) => Loading::Cancelled,
        }
    }

    /// A measurement taken in the background ([`crate::background`]): kept
    /// in memory unless its key already has one, in memory or in
    /// `library`. Whether it was kept.
    pub fn adopt(&mut self, library: &Library, key: LoudnessKey, row: LoudnessRow) -> bool {
        if self.lookup(library, &key).is_some() {
            return false;
        }
        self.measured.insert(key, row);
        true
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
    /// none running): every measurement in memory is stored; one the
    /// database refuses is logged and kept for this run.
    pub fn store(&mut self, library: &mut Library) {
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

    /// At the end of a run: the measurements in memory are stored if the
    /// scan stopped (`quiet`).
    pub fn close(&mut self, library: &mut Library, quiet: bool) {
        if quiet {
            self.store(library);
        } else if !self.measured.is_empty() {
            log_line(
                "loudness: the scan did not stop, so the measurements are not stored",
                true,
            );
        }
    }
}

/// Charts, banks and a host for the measurement's tests, shared with the
/// shelf's.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::sync::Arc;

    use audio::{SoundBank, Speed};
    use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};

    /// The rate the test banks are decoded at: low, so a debug build
    /// analyzes seconds of sound quickly.
    pub const RATE: u32 = 8_000;

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
    pub fn chart(dir: &Path, id: u8) -> Chart {
        let mut chart = ChartBuilder::new(120_000)
            .sound("key.wav")
            .bgm(TimeUs(0), SoundId(0))
            .build()
            .unwrap();
        chart.sounds[0].path = Some(dir.join("key.wav"));
        chart.identity.resolved_hash = [id; 32];
        chart
    }

    /// `chart`'s sounds, decoded at [`RATE`] and `speed`.
    pub fn bank_at(chart: &Chart, speed: Speed) -> Arc<SoundBank> {
        Arc::new(audio::load_bank_at(chart, RATE, speed, &audio::BankLimits::default()).unwrap())
    }

    /// `chart`'s sounds, decoded at [`RATE`].
    pub fn bank(chart: &Chart) -> Arc<SoundBank> {
        bank_at(chart, Speed::NORMAL)
    }

    /// A load's host: counts the blocks it is asked about, cancels at
    /// block `cancel_at`, panics at block `panic_at`, and reads the 1×
    /// chart and bank from `normal`.
    #[derive(Default)]
    pub struct Host {
        pub blocks: usize,
        pub measuring: usize,
        pub cancel_at: Option<usize>,
        pub panic_at: Option<usize>,
        pub normal: Option<(Chart, Arc<SoundBank>)>,
        pub normal_asked: usize,
    }

    impl super::Host for Host {
        fn at_normal(&mut self) -> anyhow::Result<(Chart, Arc<SoundBank>)> {
            self.normal_asked += 1;
            self.normal
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no 1× chart (test)"))
        }

        fn measuring(&mut self, _name: &str) {
            self.measuring += 1;
        }

        fn cancelled(&mut self) -> bool {
            let n = self.blocks;
            self.blocks += 1;
            assert_ne!(self.panic_at, Some(n), "a block failed (test)");
            self.cancel_at == Some(n)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use audio::{Gain, Speed};
    use game::DataRoot;
    use library::Library;

    use super::fixture::{Host, bank, bank_at, chart, square, write_sound};
    use super::{Loading, Loudness};

    fn library(dir: &Path) -> Library {
        crate::shelf::open(&DataRoot::new(dir.join("data")))
            .unwrap()
            .0
    }

    /// A miss is measured during the load: the first play's gain is the
    /// measured one, kept in memory and stored before the countdown; a
    /// second load (in memory, then from the library in another run)
    /// measures nothing.
    #[test]
    fn a_missing_key_is_measured_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 1_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();
        let mut loudness = Loudness::default();
        let mut host = Host::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut host);
        assert_eq!(host.measuring, 1);
        assert!(host.blocks > 0);
        let row = loudness.in_memory(&key).unwrap();
        assert!(row.integrated.is_some(), "{row:?}");
        assert_eq!(loudness.gain(), super::gain(&row));
        assert_ne!(loudness.gain(), Gain::HEADROOM);
        assert_eq!(lib.loudness(&key).unwrap(), None);
        // Before the countdown: stored.
        loudness.store(&mut lib);
        assert_eq!(lib.loudness(&key).unwrap(), Some(row));
        assert!(loudness.in_memory(&key).is_none());
        // Loaded again: from the library, nothing measured.
        let mut again = Host::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut again);
        assert_eq!((again.measuring, again.blocks), (0, 0));
        assert_eq!(loudness.gain(), super::gain(&row));
        // In another run's memory-less state, too.
        let mut fresh = Loudness::default();
        fresh.load(&lib, &c, &b, "a.bms", &mut again);
        assert_eq!((again.measuring, again.blocks), (0, 0));
        assert_eq!(fresh.gain(), super::gain(&row));
    }

    /// An analysis that fails (it panics at its second block), and one
    /// cancelled at its second block, leave the headroom and keep nothing:
    /// nothing is stored, and the next load measures again. A failed
    /// load still plays; a cancelled one stops the load.
    #[test]
    fn a_failed_or_cancelled_measurement_leaves_the_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 1_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();
        for (host, outcome) in [
            (
                Host {
                    panic_at: Some(1),
                    ..Host::default()
                },
                Loading::Ready,
            ),
            (
                Host {
                    cancel_at: Some(1),
                    ..Host::default()
                },
                Loading::Cancelled,
            ),
        ] {
            let mut host = host;
            let mut loudness = Loudness::default();
            assert_eq!(loudness.load(&lib, &c, &b, "a.bms", &mut host), outcome);
            // It stopped at the block it was told to.
            assert_eq!(host.blocks, 2);
            assert_eq!(loudness.gain(), Gain::HEADROOM);
            assert!(loudness.in_memory(&key).is_none());
            loudness.store(&mut lib);
            assert_eq!(lib.loudness(&key).unwrap(), None);
            let mut next = Host::default();
            loudness.load(&lib, &c, &b, "a.bms", &mut next);
            assert_eq!(next.measuring, 1);
            assert_ne!(loudness.gain(), Gain::HEADROOM);
        }
    }

    /// A store the database refuses keeps the measurement in memory for
    /// the run: the gain is the measured one, and a reload measures
    /// nothing.
    #[test]
    fn a_refused_store_keeps_the_measurement_for_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(1.0, 3_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let key = super::key(&c, &b).unwrap();
        rusqlite::Connection::open(dir.path().join("data").join("library.db"))
            .unwrap()
            .execute_batch("DROP TABLE loudness;")
            .unwrap();
        let mut loudness = Loudness::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut Host::default());
        let measured = loudness.gain();
        assert_ne!(measured, Gain::HEADROOM);
        loudness.store(&mut lib);
        assert!(loudness.in_memory(&key).is_some());
        let mut again = Host::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut again);
        assert_eq!(again.measuring, 0);
        assert_eq!(loudness.gain(), measured);
    }

    /// A bank at another speed is measured through the chart and bank read
    /// again at 1×, under the 1× key: the gain a 1× load measures. When
    /// the 1× ones cannot be read, or their key is another, it plays at
    /// the headroom and keeps nothing.
    #[test]
    fn another_speed_measures_the_1x_chart() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 1_000));
        let c = chart(&songs, 1);
        let normal = bank(&c);
        let slow = bank_at(&c, Speed::SLOWEST);
        let key = super::key(&c, &normal).unwrap();
        assert_eq!(super::key(&c, &slow), Some(key));
        let mut at_1x = Loudness::default();
        at_1x.load(&lib, &c, &normal, "a.bms", &mut Host::default());

        let mut host = Host {
            normal: Some((c.clone(), Arc::clone(&normal))),
            ..Host::default()
        };
        let mut loudness = Loudness::default();
        loudness.load(&lib, &c, &slow, "a.bms", &mut host);
        assert_eq!(host.normal_asked, 1);
        assert_eq!(loudness.gain(), at_1x.gain());
        assert_eq!(loudness.in_memory(&key), at_1x.in_memory(&key));

        // No 1× chart.
        let mut none = Host::default();
        let mut loudness = Loudness::default();
        loudness.load(&lib, &c, &slow, "a.bms", &mut none);
        assert_eq!(none.normal_asked, 1);
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        assert!(loudness.in_memory(&key).is_none());
        // Another chart's 1× bank.
        let other_dir = dir.path().join("other");
        write_sound(&other_dir, &square(2.0, 500));
        let other = chart(&other_dir, 1);
        let mut wrong = Host {
            normal: Some((other.clone(), bank(&other))),
            ..Host::default()
        };
        loudness.load(&lib, &c, &slow, "a.bms", &mut wrong);
        assert_eq!(wrong.blocks, 0);
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        assert!(loudness.in_memory(&key).is_none());
    }

    /// A sound that cannot be read is measured as it plays, silent: the
    /// chart plays at the headroom (no integrated loudness), measured once
    /// and kept.
    #[test]
    fn an_unreadable_sound_measures_silent() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path());
        let songs = dir.path().join("song");
        std::fs::create_dir_all(&songs).unwrap();
        std::fs::write(songs.join("key.wav"), b"not a sound").unwrap();
        let c = chart(&songs, 1);
        let b = bank(&c);
        assert!(!b.warnings().is_empty());
        let key = super::key(&c, &b).unwrap();
        let mut loudness = Loudness::default();
        let mut host = Host::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut host);
        assert_eq!(host.measuring, 1);
        assert_eq!(loudness.gain(), Gain::HEADROOM);
        assert_eq!(loudness.in_memory(&key).unwrap().integrated, None);
    }

    /// Without a library each load measures, and nothing is kept: a
    /// failure plays at the headroom.
    #[test]
    fn without_a_library_each_load_measures() {
        let dir = tempfile::tempdir().unwrap();
        let songs = dir.path().join("song");
        write_sound(&songs, &square(1.0, 1_000));
        let c = chart(&songs, 1);
        let b = bank(&c);
        let lib = library(dir.path());
        let mut stored = Loudness::default();
        stored.load(&lib, &c, &b, "a.bms", &mut Host::default());
        for _ in 0..2 {
            let mut host = Host::default();
            let g = super::unstored_gain(&c, &b, "a.bms", &mut host);
            assert_eq!(host.measuring, 1);
            assert_eq!(g, stored.gain());
        }
        let mut failing = Host {
            panic_at: Some(0),
            ..Host::default()
        };
        assert_eq!(
            super::unstored_gain(&c, &b, "a.bms", &mut failing),
            Gain::HEADROOM
        );
    }

    /// A chart and its `key.wav` in a folder, measured and stored; the file
    /// is then overwritten with other samples of the same size and its old
    /// modification time, which the chart's resolved hash (sound names
    /// only) cannot see: the reload's key differs by its sound digest, the
    /// stored row misses, and the load measures the new file.
    #[test]
    fn replacing_a_sound_file_invalidates_the_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let mut lib = library(dir.path());
        let songs = dir.path().join("song");
        write_sound(&songs, &square(2.0, 4_000));
        let wav = songs.join("key.wav");
        let mtime = std::fs::metadata(&wav).unwrap().modified().unwrap();
        let mut loudness = Loudness::default();

        let c = chart(&songs, 1);
        let b = bank(&c);
        let first = super::key(&c, &b).unwrap();
        loudness.load(&lib, &c, &b, "a.bms", &mut Host::default());
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
        let mut host = Host::default();
        loudness.load(&lib, &c, &b, "a.bms", &mut host);
        assert_eq!(host.measuring, 1, "the stale row was taken");
        loudness.store(&mut lib);
        let again = lib.loudness(&second).unwrap().unwrap();
        // 20 dB quieter.
        let l = |r: &library::LoudnessRow| r.integrated.unwrap();
        assert!(
            (l(&stored) - l(&again) - 20.0).abs() < 0.5,
            "{stored:?} {again:?}"
        );
    }
}
