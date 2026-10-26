//! A simulated device for tests and headless runs (design.md "Mechanisms
//! for Testing"): the engine passes of a shared-mode device on a virtual
//! timeline, with clock drift, wake jitter, resume discontinuities and
//! missed wakes, and the true time each written frame is played at.
//!
//! The device starts at [`SimConfig::start`] with a wake that asks for the
//! queue to be filled. Engine pass `k` then takes one period from the
//! buffer, `k + 1` device periods after the start (a device period is
//! `period / rate` seconds on the device's own clock, which runs
//! `drift_ppm` fast against the judgment timeline), and its buffer event
//! wakes the engine up to `jitter_us` later. A pass that finds less than a
//! period queued plays silence for the rest (an underrun), so later frames
//! play later.

use bms_model::TimeUs;

use crate::CHANNELS;
use crate::output::{AudioOutput, MAX_QUEUE_PERIODS, OutputError, Wake};

/// From pass `at_pass` on, every pass comes `gap_us` later (a resume).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resume {
    /// The first delayed pass.
    pub at_pass: u64,
    /// The delay, µs.
    pub gap_us: i64,
}

/// The settings of a [`SimDevice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// Frames per second (nominal).
    pub rate: u32,
    /// Frames per engine pass.
    pub period: u32,
    /// Periods the engine keeps queued, 1 to [`MAX_QUEUE_PERIODS`];
    /// [`AudioOutput::set_queue_periods`] changes it.
    pub queue_periods: u32,
    /// Time of the first wake.
    pub start: TimeUs,
    /// The device clock's speed against the judgment timeline, ppm
    /// (positive: fast).
    pub drift_ppm: i64,
    /// Largest wake lateness, µs; each wake is late by a pseudo-random
    /// amount in `0..=jitter_us`. Keep it below a period.
    pub jitter_us: i64,
    /// Seed of the jitter.
    pub seed: u64,
    /// Discontinuities, sorted by pass.
    pub resumes: Vec<Resume>,
    /// Passes whose buffer event never wakes the engine, sorted.
    pub missed_wakes: Vec<u64>,
}

impl SimConfig {
    /// A steady device: two queued periods, no drift, no jitter.
    #[must_use]
    pub fn steady(rate: u32, period: u32, start: TimeUs) -> Self {
        Self {
            rate,
            period,
            queue_periods: 2,
            start,
            drift_ppm: 0,
            jitter_us: 0,
            seed: 0,
            resumes: Vec::new(),
            missed_wakes: Vec::new(),
        }
    }
}

/// Underruns whose timing shift [`SimDevice::true_time`] remembers.
pub const SHIFT_HISTORY: usize = 64;

/// The simulated device. Waiting and writing allocate nothing.
/// [`SimDevice::true_time`] is exact for frames written after the
/// [`SHIFT_HISTORY`]th most recent underrun.
#[derive(Clone, Debug)]
pub struct SimDevice {
    cfg: SimConfig,
    started: bool,
    /// Passes done.
    pass: u64,
    written: u64,
    consumed: u64,
    /// Silent device frames played so far.
    silence: u64,
    /// `(first app frame, silence before it)`, one per underrun.
    shifts: Vec<(u64, u64)>,
    buf: Vec<f32>,
    rng: u64,
    next_jitter: i64,
    underruns: u64,
}

impl SimDevice {
    /// A device with `cfg`.
    ///
    /// # Panics
    ///
    /// If `cfg` queues no frames (`queue_periods` or `period` is zero) or
    /// more than [`MAX_QUEUE_PERIODS`], or its rate is zero: such a device
    /// could never be written to.
    #[must_use]
    pub fn new(cfg: SimConfig) -> Self {
        assert!(
            (1..=MAX_QUEUE_PERIODS).contains(&cfg.queue_periods) && cfg.period > 0 && cfg.rate > 0,
            "a simulated device needs a nonzero rate and period and a queue of 1 to {MAX_QUEUE_PERIODS} periods"
        );
        let buf = vec![0.0; (MAX_QUEUE_PERIODS * cfg.period) as usize * CHANNELS];
        let mut dev = Self {
            rng: cfg.seed,
            cfg,
            started: false,
            pass: 0,
            written: 0,
            consumed: 0,
            silence: 0,
            shifts: Vec::with_capacity(SHIFT_HISTORY),
            buf,
            next_jitter: 0,
            underruns: 0,
        };
        dev.next_jitter = dev.draw_jitter();
        dev
    }

    fn draw_jitter(&mut self) -> i64 {
        // SplitMix64.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let span = self.cfg.jitter_us.max(0) as u64 + 1;
        (z % span) as i64
    }

    /// The settings.
    #[must_use]
    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    /// Passes that found less than a period queued.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    fn gap_before(&self, pass: u64) -> i64 {
        self.cfg
            .resumes
            .iter()
            .filter(|r| r.at_pass <= pass)
            .map(|r| r.gap_us)
            .sum()
    }

    /// When device frame `n` is played: `n + period` device frames after the
    /// start, plus the resume gaps before its pass.
    fn device_time(&self, n: u64) -> TimeUs {
        let c = &self.cfg;
        let num = i128::from(n + u64::from(c.period)) * 1_000_000_000_000;
        let den = i128::from(c.rate) * (1_000_000 + i128::from(c.drift_ppm));
        let pass = n / u64::from(c.period);
        TimeUs(c.start.0 + (num / den) as i64 + self.gap_before(pass))
    }

    /// The true time written frame `frame` is played at, given the
    /// underruns so far.
    #[must_use]
    pub fn true_time(&self, frame: u64) -> TimeUs {
        let silence = self
            .shifts
            .iter()
            .rev()
            .find(|&&(from, _)| from <= frame)
            .map_or(0, |&(_, s)| s);
        self.device_time(frame + silence)
    }

    /// When the next wake happens.
    #[must_use]
    pub fn next_wake(&self) -> TimeUs {
        if !self.started {
            return self.cfg.start;
        }
        let mut k = self.pass;
        while self.cfg.missed_wakes.binary_search(&k).is_ok() {
            k += 1;
        }
        let p = u64::from(self.cfg.period);
        TimeUs(self.device_time(k * p).0 + self.next_jitter)
    }
}

impl AudioOutput for SimDevice {
    fn rate(&self) -> u32 {
        self.cfg.rate
    }

    fn period(&self) -> u32 {
        self.cfg.period
    }

    fn wait(&mut self) -> Wake {
        if !self.started {
            self.started = true;
            return Wake {
                queued_from: 0,
                at: self.cfg.start,
                padding: 0,
                priming: false,
            };
        }
        let p = u64::from(self.cfg.period);
        loop {
            let k = self.pass;
            self.pass += 1;
            let take = p.min(self.written - self.consumed);
            if take < p {
                self.underruns += 1;
                self.silence += p - take;
                if self.shifts.len() == SHIFT_HISTORY {
                    // Forget the oldest; frames written before it are long played.
                    self.shifts.remove(0);
                }
                self.shifts.push((self.consumed + take, self.silence));
            }
            self.consumed += take;
            if self.cfg.missed_wakes.binary_search(&k).is_ok() {
                continue;
            }
            let at = TimeUs(self.device_time(k * p).0 + self.next_jitter);
            self.next_jitter = self.draw_jitter();
            return Wake {
                queued_from: self.consumed,
                at,
                padding: (self.written - self.consumed) as u32,
                priming: false,
            };
        }
    }

    fn queue_periods(&self) -> Option<u32> {
        Some(self.cfg.queue_periods)
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.cfg.queue_periods = periods.clamp(1, MAX_QUEUE_PERIODS);
        Some(self.cfg.queue_periods)
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        let queue = u64::from(self.cfg.queue_periods * self.cfg.period);
        let p = u64::from(self.cfg.period);
        let padding = self.written - self.consumed;
        let frames = queue.saturating_sub(padding) / p * p;
        f(&mut self.buf[..frames as usize * CHANNELS]);
        self.written += frames;
        Ok(())
    }
}
