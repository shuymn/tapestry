//! The loudness analysis of a chart (design.md "Algorithms", per-chart
//! volume normalization): its 1× autoplay schedule mixed offline through
//! [`Mixer::mix_raw`], at unity gain, before the volume, the play's gain and
//! the soft clipper, in [`BLOCK_FRAMES`]-frame blocks fed to an EBU R128
//! meter in mode `I | TRUE_PEAK | HISTOGRAM`.
//!
//! - **Memory independent of duration.** Everything is allocated in
//!   [`Analysis::new`]: the mix block, the mixer's voices and the meter,
//!   whose histogram mode bins gating-block energies in a fixed table
//!   instead of keeping them, so the integrated loudness covers the whole
//!   chart (to bin resolution) in fixed memory. The history is never
//!   truncated.
//! - **Steps.** [`Analysis::step`] mixes and meters one block, so the job
//!   running it can pause between blocks.
//! - **The end.** The analysis is done once the schedule is exhausted, no
//!   start waits and no voice sounds. The integrated loudness is read then,
//!   from the completed gating blocks only: a program shorter than one
//!   400 ms block has none and plays at [`HEADROOM`]. Zeros for the
//!   true-peak interpolator's delay are fed after it, for the true peak
//!   only.
//! - **Over range.** The histogram's top bin ends at +30 LUFS, and louder
//!   gating blocks saturate it. Each gating block's loudness is read where
//!   the meter completes it, so a block at or above +30 LUFS marks the
//!   measurement over range; its loudness is then taken as +30 LUFS, a
//!   lower bound.
//!
//! This module is not in-play: it allocates in [`Analysis::new`] only.

use std::sync::Arc;

use bms_model::{BgmEvent, TimeUs};
use ebur128::{EbuR128, Mode};

use crate::CHANNELS;
use crate::bank::SoundBank;
use crate::mixer::{HEADROOM, MAX_GAIN, MAX_VOICES, Mixer};

/// Frames mixed per [`Analysis::step`].
pub const BLOCK_FRAMES: usize = 4096;
/// The version of what [`Mixer::mix_raw`] makes of a chart's files and
/// schedule: decoding, rate conversion, the voice rules and the steal fade.
/// A change bumps it, and every cached measurement is analyzed again.
pub const MIXER_VERSION: u32 = 1;
/// The version of the analysis itself: the block size, the meter's mode
/// and version, the time-to-frame mapping and the autoplay schedule
/// (`game::schedule_iter`). A change bumps it.
pub const ANALYSIS_VERSION: u32 = 1;
/// The resolved crates that shape a measurement (the decoder, the
/// resampler and the meter, not those that only derive code or log) and
/// their dependency entries with features in the workspace manifest and
/// `audio`'s, as a SHA-256 the `analysis_dependencies_are_pinned` test
/// computes. A change of any of them fails that test until
/// [`MIXER_VERSION`] or [`ANALYSIS_VERSION`] is bumped and this updated.
pub const MEASUREMENT_DEPENDENCIES: &str =
    "5b22868cf844730d038fc2c853f86258c8b51ab5d244898911a7dca6d6e74b52";

/// The integrated loudness the normalization aims at, LUFS.
pub const TARGET_LUFS: f64 = -18.0;
/// The ceiling of the meter's histogram, LUFS: a gating block at or above
/// it saturates the top bin.
pub const CEILING_LUFS: f64 = 30.0;
/// The true peak the normalization keeps below, dBTP.
pub const PEAK_CEILING_DBTP: f64 = -1.0;

/// What an analysis measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    /// The integrated loudness, LUFS; `None` when no gating block passed
    /// the gates (silence, or a program shorter than 400 ms).
    pub integrated: Option<f64>,
    /// The true peak, linear, the larger of the two channels'; it may
    /// exceed 1.
    pub true_peak: f64,
    /// Whether a gating block was at or above [`CEILING_LUFS`]: the
    /// integrated loudness is then taken as the ceiling.
    pub over_range: bool,
    /// The program's frames: up to the last sample of the last voice.
    pub frames: u64,
}

/// The play's gain for a chart measured `m`: `min(10^((−18 − L)/20),
/// 10^(6/20), 10^(−1/20) / P)`, with L taken as +30 LUFS when the
/// measurement is over range, and [`HEADROOM`] when it has no integrated
/// loudness.
#[must_use]
pub fn normalization_gain(m: &Measurement) -> f32 {
    let loudness = if m.over_range {
        CEILING_LUFS
    } else {
        match m.integrated {
            Some(l) if l.is_finite() => l,
            _ => return HEADROOM,
        }
    };
    let by_loudness = 10_f64.powf((TARGET_LUFS - loudness) / 20.0);
    let by_peak = if m.true_peak > 0.0 {
        10_f64.powf(PEAK_CEILING_DBTP / 20.0) / m.true_peak
    } else {
        f64::INFINITY
    };
    let g = by_loudness.min(f64::from(MAX_GAIN)).min(by_peak);
    if g.is_nan() {
        return HEADROOM;
    }
    (g as f32).clamp(f32::MIN_POSITIVE, MAX_GAIN)
}

/// Why an analysis stopped.
#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    /// The meter refused.
    #[error("the loudness meter failed: {0}")]
    Meter(#[from] ebur128::Error),
}

/// What one [`Analysis::step`] came to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// A block was mixed and metered; more follow.
    More,
    /// The analysis is over.
    Done(Measurement),
}

/// Frames the true-peak interpolator lags at `rate`: its taps per phase
/// (4× oversampling below 96 kHz, 2× below 192 kHz, none above).
const fn true_peak_delay(rate: u32) -> usize {
    if rate < 96_000 {
        12
    } else if rate < 192_000 {
        24
    } else {
        0
    }
}

/// A chart's loudness analysis in progress.
pub struct Analysis {
    bank: Arc<SoundBank>,
    mixer: Mixer,
    meter: EbuR128,
    /// One block of interleaved stereo.
    block: Box<[f32]>,
    /// The schedule's next event, taken and not yet started.
    next: Option<BgmEvent>,
    /// The schedule has no more events.
    exhausted: bool,
    /// Frames fed to the meter.
    fed: u64,
    /// The meter's frame count at which its next gating block completes.
    next_gate: u64,
    /// Frames between gating blocks (100 ms).
    hop: u64,
    over_range: bool,
}

impl std::fmt::Debug for Analysis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analysis")
            .field("rate", &self.bank.rate())
            .field("mixed", &self.mixer.mixed_to())
            .field("over_range", &self.over_range)
            .finish_non_exhaustive()
    }
}

impl Analysis {
    /// An analysis of `bank`'s sounds at its rate, with every buffer it
    /// uses allocated: [`MAX_VOICES`] voices and the meter.
    ///
    /// # Errors
    ///
    /// [`AnalysisError::Meter`] when the meter refuses the rate.
    pub fn new(bank: Arc<SoundBank>) -> Result<Self, AnalysisError> {
        let rate = bank.rate();
        let meter = EbuR128::new(
            CHANNELS as u32,
            rate,
            Mode::I | Mode::TRUE_PEAK | Mode::HISTOGRAM,
        )?;
        // The meter's 100 ms, rounded as it rounds it.
        let hop = ((u64::from(rate) + 5) / 10).max(1);
        Ok(Self {
            mixer: Mixer::new(MAX_VOICES, rate),
            meter,
            block: vec![0.0; BLOCK_FRAMES * CHANNELS].into_boxed_slice(),
            next: None,
            exhausted: false,
            fed: 0,
            next_gate: 4 * hop,
            hop,
            over_range: false,
            bank,
        })
    }

    /// The output frame chart time `t` is due at: `floor(max(t, 0) × rate /
    /// 1 000 000)`.
    fn frame_of(&self, t: TimeUs) -> u64 {
        let us = u128::try_from(t.0.max(0)).unwrap_or(0);
        u64::try_from(us * u128::from(self.bank.rate()) / 1_000_000).unwrap_or(u64::MAX)
    }

    /// Mixes and meters the next block of `events`, the schedule in time
    /// order, the same iterator at every step: each event due before the
    /// block's end starts while a start can wait ([`Mixer::can_start`]);
    /// one that cannot waits for the next block, where it starts at the
    /// block's first frame, as the engine defers it. Done once the schedule
    /// is exhausted and nothing sounds.
    ///
    /// # Errors
    ///
    /// [`AnalysisError::Meter`] when the meter refuses.
    pub fn step(
        &mut self,
        events: &mut impl Iterator<Item = BgmEvent>,
    ) -> Result<Step, AnalysisError> {
        if self.bank.stored_count() == 0 {
            // Nothing can sound: no block is metered.
            return Ok(Step::Done(Measurement {
                integrated: None,
                true_peak: 0.0,
                over_range: false,
                frames: 0,
            }));
        }
        let block_end = self.mixer.mixed_to() + BLOCK_FRAMES as u64;
        while self.mixer.can_start() {
            let event = match self.next.take() {
                Some(e) => e,
                None if self.exhausted => break,
                None => match events.next() {
                    Some(e) => e,
                    None => {
                        self.exhausted = true;
                        break;
                    }
                },
            };
            let at = self.frame_of(event.time);
            if at >= block_end {
                self.next = Some(event);
                break;
            }
            // A sound the bank lacks starts nothing, as in play.
            let _ = self.mixer.trigger(&self.bank, event.sound, at);
        }
        // A full pending queue stops the admission before the schedule's
        // end is seen: look ahead, so the block that ends the program is
        // known as such.
        if self.next.is_none() && !self.exhausted {
            self.next = events.next();
            self.exhausted = self.next.is_none();
        }
        if self.ended() {
            // Nothing was started: no frame to meter.
            return self.finish(self.mixer.mixed_to()).map(Step::Done);
        }
        let block_start = self.mixer.mixed_to();
        let mut block = std::mem::take(&mut self.block);
        self.mixer.mix_raw(&self.bank, &mut block);
        // The block in which the last voice ends is metered only up to its
        // last sample: the silence after the program is not part of it.
        let end = if self.ended() {
            self.mixer
                .last_end()
                .clamp(block_start, block_start + BLOCK_FRAMES as u64)
        } else {
            block_start + BLOCK_FRAMES as u64
        };
        let len = usize::try_from(end - block_start).unwrap_or(BLOCK_FRAMES) * CHANNELS;
        let fed = self.feed(block.get(..len).unwrap_or(&block));
        self.block = block;
        fed?;
        if self.ended() {
            return self.finish(end).map(Step::Done);
        }
        Ok(Step::More)
    }

    /// Whether the schedule is exhausted and nothing sounds or waits.
    fn ended(&self) -> bool {
        self.exhausted && self.next.is_none() && self.mixer.sounding() == 0
    }

    /// Feeds `frames` to the meter, split where it completes a gating
    /// block, whose loudness is read there for the over-range check.
    fn feed(&mut self, frames: &[f32]) -> Result<(), AnalysisError> {
        let mut rest = frames;
        while !rest.is_empty() {
            let left = (rest.len() / CHANNELS) as u64;
            let to_gate = self.next_gate - self.fed;
            let take = left.min(to_gate);
            let (now, later) = rest.split_at(take as usize * CHANNELS);
            self.meter.add_frames_f32(now)?;
            self.fed += take;
            if self.fed == self.next_gate {
                if self.meter.loudness_momentary()? >= CEILING_LUFS {
                    self.over_range = true;
                }
                self.next_gate += self.hop;
            }
            rest = later;
        }
        Ok(())
    }

    /// The measurement of the program's `frames` frames: the integrated
    /// loudness of the completed gating blocks, then the true peak once the
    /// interpolator's delay is flushed with zeros.
    fn finish(&mut self, frames: u64) -> Result<Measurement, AnalysisError> {
        let integrated = Some(self.meter.loudness_global()?).filter(|l| l.is_finite());
        let delay = true_peak_delay(self.bank.rate());
        let mut zeros = std::mem::take(&mut self.block);
        zeros.fill(0.0);
        let fed = self.feed(zeros.get(..delay * CHANNELS).unwrap_or_default());
        self.block = zeros;
        fed?;
        let true_peak = self.meter.true_peak(0)?.max(self.meter.true_peak(1)?);
        Ok(Measurement {
            integrated,
            true_peak,
            over_range: self.over_range,
            frames,
        })
    }
}
