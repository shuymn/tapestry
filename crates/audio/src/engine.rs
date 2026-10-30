//! One wake of the audio thread (design.md "Aligning Audio to the Judgment
//! Timeline"): observe the buffer event, drain sound commands, schedule BGM
//! that falls in the block, render it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::Consumer;

use crate::CHANNELS;
use crate::bank::SoundBank;
use crate::clock::MixClock;
use crate::mixer::{InvalidVolume, Mixer};
use crate::output::{AudioOutput, OutputError};
use crate::seek::{BgmSchedule, FrameLine, SeekPlan, SeekSource, schedule_hash};

/// A key sound to play, from the logic thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundCmd {
    /// The sound.
    pub sound: SoundId,
    /// The raw receive time of the input behind it (design.md "Latency
    /// Figures").
    pub origin: TimeUs,
}

/// What a scheduled start came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduledKind {
    /// A key sound, with its command's origin.
    Key {
        /// The input's receive time.
        origin: TimeUs,
    },
    /// A BGM event, with its chart time.
    Bgm {
        /// The event's time.
        time: TimeUs,
    },
}

/// A sound start the engine scheduled: one per start the mixer accepted.
/// A start of the same sound at the same frame is logged too, but sounds as
/// the one instance (design.md "Audio"), so the log can hold more starts
/// than were heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scheduled {
    /// The sound.
    pub sound: SoundId,
    /// Where it came from.
    pub kind: ScheduledKind,
    /// The output frame of its first sample.
    pub frame: u64,
    /// When that frame is mixed, by the mix clock at scheduling.
    pub at: Option<TimeUs>,
}

/// What one step did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Frames written.
    pub frames: u32,
    /// Sound commands drained.
    pub commands: u32,
    /// BGM events started.
    pub bgm_started: u32,
    /// Whether the wake found the buffer empty after audio had been
    /// written (an underrun).
    pub underrun: bool,
    /// When the wake happened; `None` for a priming wake, which has no
    /// time.
    pub at: Option<TimeUs>,
    /// Voices and fading releases still to sound after the block.
    pub active_voices: usize,
    /// BGM events not yet started.
    pub bgm_left: usize,
}

/// Why an engine could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The output's rate or period is zero.
    #[error("the output's rate or period is zero")]
    ZeroRateOrPeriod,
    /// The bank was decoded at a rate other than the output's.
    #[error("the sound bank is at {bank} Hz but the output runs at {output} Hz")]
    RateMismatch {
        /// The bank's rate.
        bank: u32,
        /// The output's rate.
        output: u32,
    },
}

/// A chart's sounds as the engine plays them: its bank and its BGM schedule.
pub type ChartSounds = (Arc<SoundBank>, BgmSchedule);

/// Why an engine did not take a [`SeekPlan`]; nothing changed, and the
/// caller plans again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SeekError {
    /// The plan was made for another engine, or from a schedule other than
    /// its engine's.
    #[error("the seek was planned for another engine or schedule")]
    Source,
    /// The plan's frame line has another rate or period than the output.
    #[error(
        "the seek was planned at {plan_rate} Hz in {plan_period}-frame passes but the output runs at {rate} Hz in {period}-frame passes"
    )]
    Line {
        /// The plan's rate.
        plan_rate: u32,
        /// The plan's period.
        plan_period: u32,
        /// The output's rate.
        rate: u32,
        /// The output's period.
        period: u32,
    },
    /// The seek's frame is already written.
    #[error("the seek's frame {frame} is already written (up to {written})")]
    Late {
        /// The frame the mix clock gives the seek's time.
        frame: i64,
        /// The first frame not yet written.
        written: u64,
    },
    /// The play is ringing out, or has rung out: a seek belongs to the
    /// start of a play, after [`AudioEngine::restart`] has taken the
    /// ring-out off.
    #[error("the play is ringing out; a seek starts the next play")]
    RingingOut,
}

/// Engine ids for [`SeekSource`]: one per engine built.
static NEXT_ENGINE: AtomicU64 = AtomicU64::new(0);

/// Where chart time `t` of a play whose chart time 0 lies at `origin` is due
/// on the output's timeline, `offset` early.
fn output_time(origin: TimeUs, offset: TimeUs, t: TimeUs) -> TimeUs {
    TimeUs(origin.0.saturating_add(t.0).saturating_sub(offset.0))
}

/// The audio engine: mixer, mix clock and BGM schedule. Everything is
/// allocated in [`AudioEngine::new`]; [`AudioEngine::step`] allocates
/// nothing.
#[derive(Debug)]
pub struct AudioEngine {
    /// Which engine it is, for its seek plans.
    id: u64,
    /// Which of its charts it plays, for its seek plans: one more at each
    /// [`replace`](Self::replace).
    chart: u64,
    bank: Arc<SoundBank>,
    mixer: Mixer,
    clock: MixClock,
    rate: u32,
    period: u32,
    period_us: i64,
    bgm: Vec<BgmEvent>,
    /// [`schedule_hash`] of `bgm`.
    bgm_hash: u64,
    next_bgm: usize,
    output_offset: TimeUs,
    /// Where chart time 0 lies on the output's timeline.
    play_origin: TimeUs,
    /// The chart time a seek starts at and the starts the pass reaching it
    /// may still admit, until its frame is placed. Set only at a play's
    /// start: a seek is refused while a ring-out is set ([`SeekError::RingingOut`]).
    seek_to: Option<(TimeUs, usize)>,
    /// A ring-out's fade and end in chart time, until the pass that reaches
    /// the fade puts them on the output.
    ring_out: Option<(TimeUs, TimeUs)>,
    /// The output frame a placed ring-out ends at: no BGM event starts
    /// from there.
    ring_end: Option<i64>,
    scheduled: Vec<Scheduled>,
    scheduled_dropped: u64,
    underruns: u64,
    deferred_wakes: u64,
}

impl AudioEngine {
    /// An engine for an output at `rate` with `period`-frame passes,
    /// playing `bgm` (sorted by time) and key sounds from `bank` with
    /// `voices` voices. Up to `schedule_log` scheduled starts are kept for
    /// [`AudioEngine::drain_scheduled`].
    ///
    /// # Errors
    ///
    /// [`EngineError`] for a zero rate or period, or a bank at another rate.
    pub fn new(
        bank: Arc<SoundBank>,
        bgm: &[BgmEvent],
        rate: u32,
        period: u32,
        voices: usize,
        schedule_log: usize,
    ) -> Result<Self, EngineError> {
        Self::with_schedule(bank, bgm.to_vec(), rate, period, voices, schedule_log)
    }

    /// [`new`](Self::new) with the schedule moved in rather than copied:
    /// for a schedule built for this engine alone, such as an autoplay
    /// session's BGM merged with its key sounds.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_schedule(
        bank: Arc<SoundBank>,
        bgm: Vec<BgmEvent>,
        rate: u32,
        period: u32,
        voices: usize,
        schedule_log: usize,
    ) -> Result<Self, EngineError> {
        if rate == 0 || period == 0 {
            return Err(EngineError::ZeroRateOrPeriod);
        }
        if bank.rate() != rate {
            return Err(EngineError::RateMismatch {
                bank: bank.rate(),
                output: rate,
            });
        }
        Ok(Self {
            id: NEXT_ENGINE.fetch_add(1, Ordering::Relaxed),
            chart: 0,
            bank,
            mixer: Mixer::new(voices, rate),
            clock: MixClock::new(rate, period),
            rate,
            period,
            period_us: i64::from(period) * 1_000_000 / i64::from(rate),
            bgm_hash: schedule_hash(&bgm),
            bgm,
            next_bgm: 0,
            output_offset: TimeUs(0),
            play_origin: TimeUs(0),
            seek_to: None,
            ring_out: None,
            ring_end: None,
            scheduled: Vec::with_capacity(schedule_log),
            scheduled_dropped: 0,
            underruns: 0,
            deferred_wakes: 0,
        })
    }

    /// The mix clock.
    #[must_use]
    pub fn clock(&self) -> &MixClock {
        &self.clock
    }

    /// The mixer.
    #[must_use]
    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// The first output frame not yet written.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.mixer.mixed_to()
    }

    /// Wakes that found the buffer empty after audio had been written.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Wakes that left a due key sound or BGM start waiting for a later
    /// wake because the mixer's pending starts were full.
    #[must_use]
    pub fn deferred_wakes(&self) -> u64 {
        self.deferred_wakes
    }

    /// Scheduled starts not kept because the log was full.
    #[must_use]
    pub fn scheduled_dropped(&self) -> u64 {
        self.scheduled_dropped
    }

    /// Sets the master volume.
    ///
    /// # Errors
    ///
    /// [`InvalidVolume`] for a volume outside `0.0..=MAX_VOLUME`, which is
    /// not applied.
    pub fn set_volume(&mut self, volume: f32) -> Result<(), InvalidVolume> {
        self.mixer.set_volume(volume)
    }

    /// Sets the audio output latency offset: BGM is scheduled this much
    /// before its chart time.
    pub fn set_output_offset(&mut self, offset: TimeUs) {
        self.output_offset = offset;
    }

    /// Starts the play over (a retry) with chart time 0 at `origin` on the
    /// timeline the output's wakes are in: every voice stops, the BGM
    /// schedule rewinds, and a ring-out, finished or not, is taken off, so
    /// the output is at full gain from the play's first frame. The output
    /// stream and its clock continue.
    pub fn restart(&mut self, origin: TimeUs) {
        self.mixer.stop_all();
        self.next_bgm = 0;
        self.seek_to = None;
        self.clear_ring_out();
        self.play_origin = origin;
    }

    fn clear_ring_out(&mut self) {
        self.ring_out = None;
        self.ring_end = None;
    }

    /// Rings the play out (design.md "Mid-Chart Start and Section Repeat"):
    /// chart times `fade_at` and `end_at` go on the output's frames as a BGM
    /// event's time does, by the mix clock of the pass that reaches
    /// `fade_at`'s frame. From that frame the output's gain falls linearly
    /// per frame to 0 at `end_at`'s frame (a `fade_at` after `end_at` counts
    /// as `end_at`), where every voice stops and a seek still waiting is
    /// dropped; no BGM event or key sound starts from there. The play's
    /// audio then ends as a sealed tail does
    /// (no voice, no BGM event left). Until then the BGM plays on its
    /// schedule. Until the next [`restart`](Self::restart) a seek is refused
    /// ([`SeekError::RingingOut`]), so nothing takes the envelope off before
    /// its end. Allocates nothing.
    pub fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        // A fade after the end is placed and rung as the end.
        self.ring_out = Some((fade_at.min(end_at), end_at));
        self.ring_end = None;
    }

    /// The BGM schedule the engine plays (sorted by time), for planning
    /// its seeks ([`SeekPlan::new`]).
    #[must_use]
    pub fn schedule(&self) -> &[BgmEvent] {
        &self.bgm
    }

    /// What this engine plays, for planning its seeks where the play is
    /// prepared ([`SeekPlan::new`], with the schedule it was built with).
    #[must_use]
    pub fn seek_source(&self) -> SeekSource {
        SeekSource {
            engine: self.id,
            bank: Arc::clone(&self.bank),
            schedule: self.bgm_hash,
            chart: self.chart,
            voices: self.mixer.voices(),
        }
    }

    /// Where the current play's chart times fall on the output now, for
    /// planning a seek ([`SeekPlan::new`]).
    #[must_use]
    pub fn frame_line(&self) -> FrameLine {
        let anchor = self.clock.anchor_us().unwrap_or(0);
        FrameLine {
            rate: self.rate,
            period: self.period,
            block_start: self.written(),
            base_us: self
                .play_origin
                .0
                .saturating_sub(self.output_offset.0)
                .saturating_sub(anchor),
        }
    }

    /// Starts the play at the chart time `plan` was made for instead of its
    /// beginning, for a mid-chart start (design.md "Audio"); call it after
    /// [`restart`](Self::restart). The plan's voices and releases are
    /// copied in, which costs at most the voice slots and allocates
    /// nothing, and enter at the frame the mix clock gives that time, each
    /// partway into its sound or fade; the plan's starts are neither logged
    /// nor counted as BGM started. Nothing sounds before that frame, no
    /// start of the schedule is put before it, and a key sound started
    /// before it stops there.
    ///
    /// The output from that frame on is what playing the schedule from
    /// chart time 0 gives there, sample for sample, when from planning to
    /// that frame the mix clock maps chart times as the plan's line does,
    /// every pass writes the line's period, and no key sound is started.
    /// Otherwise it is degraded but bounded. A moved clock moves the seek's
    /// voices with that frame, and a start lands where the moved clock
    /// puts it, never before that frame, head intact. A different pass
    /// size or key sounds change only which pass admits the starts beyond
    /// [`MAX_PENDING_STARTS`](crate::MAX_PENDING_STARTS) in one pass (and
    /// so the voices those start, cut or steal); each still starts at a
    /// pass's first frame, head intact.
    ///
    /// # Errors
    ///
    /// [`SeekError`] for a plan made from another engine's
    /// [`seek_source`](Self::seek_source) (a schedule other than this
    /// engine's is refused when planning), on a line of another rate or
    /// period, or whose frame is already written; and
    /// [`SeekError::RingingOut`] while a ring-out is set: a seek starts a
    /// play (a round's restore), so it comes after [`restart`](Self::restart),
    /// which takes the ring-out off, and never cuts into a fade. Nothing
    /// changes.
    pub fn seek(&mut self, plan: &SeekPlan) -> Result<(), SeekError> {
        // A plan of another engine, or of a chart the engine no longer plays
        // (its bank or schedule was replaced since), is refused.
        if plan.engine() != self.id || plan.chart() != self.chart {
            return Err(SeekError::Source);
        }
        // Parking the seek's voices would take the envelope off with them.
        if self.ring_out.is_some() || self.ring_end.is_some() {
            return Err(SeekError::RingingOut);
        }
        let line = plan.line();
        if (line.rate, line.period) != (self.rate, self.period) {
            return Err(SeekError::Line {
                plan_rate: line.rate,
                plan_period: line.period,
                rate: self.rate,
                period: self.period,
            });
        }
        let frame = self.frame_line().frame(plan.time());
        if frame < i64::try_from(self.written()).unwrap_or(i64::MAX) {
            return Err(SeekError::Late {
                frame,
                written: self.written(),
            });
        }
        self.mixer.park(plan.slots(), plan.seq(), plan.at());
        self.next_bgm = plan.split();
        self.seek_to = Some((plan.time(), plan.budget()));
        Ok(())
    }

    /// BGM events not yet started.
    #[must_use]
    pub fn bgm_left(&self) -> usize {
        self.bgm.len() - self.next_bgm
    }

    /// Stops the play (an abort): every voice and pending start stops, and
    /// no remaining BGM event starts. The output stream continues, silent.
    pub fn stop(&mut self) {
        self.mixer.stop_all();
        self.next_bgm = self.bgm.len();
        self.seek_to = None;
        self.clear_ring_out();
    }

    /// Plays another chart from here on: `bank` and `bgm` (sorted by time)
    /// take the place of the current ones, which are returned so the caller
    /// frees them where it may. Every voice stops and no BGM event starts
    /// until the next [`restart`](Self::restart); the output stream and its
    /// clock continue. Nothing is allocated or freed, and the schedule is
    /// not walked: its hash came with it ([`BgmSchedule`]). A seek planned
    /// for the old schedule no longer applies: a pending one is dropped,
    /// and a later one is refused ([`SeekError::Source`]).
    ///
    /// # Errors
    ///
    /// `bank` and `bgm` themselves, untouched, when the bank was decoded at
    /// another rate than the output's: the engine keeps what it had.
    pub fn replace(
        &mut self,
        bank: Arc<SoundBank>,
        bgm: BgmSchedule,
    ) -> Result<ChartSounds, ChartSounds> {
        if bank.rate() != self.bank.rate() {
            return Err((bank, bgm));
        }
        self.mixer.stop_all();
        let (events, hash) = bgm.into_parts();
        let old_bank = std::mem::replace(&mut self.bank, bank);
        let old_bgm = std::mem::replace(&mut self.bgm, events);
        let old_hash = std::mem::replace(&mut self.bgm_hash, hash);
        self.chart = self.chart.wrapping_add(1);
        self.next_bgm = self.bgm.len();
        self.seek_to = None;
        self.clear_ring_out();
        Ok((old_bank, BgmSchedule::from_parts(old_bgm, old_hash)))
    }

    /// The bank key sounds and BGM play from.
    #[must_use]
    pub fn bank(&self) -> &Arc<SoundBank> {
        &self.bank
    }

    /// Calls `f` with each scheduled start kept since the last drain, then
    /// forgets them.
    pub fn drain_scheduled(&mut self, mut f: impl FnMut(&Scheduled)) {
        self.scheduled.iter().for_each(&mut f);
        self.scheduled.clear();
    }

    /// Starts `sound` at output frame `target` (or the first unwritten
    /// frame) and logs where it started; `false` if the mixer took no
    /// start.
    fn start(&mut self, sound: SoundId, kind: ScheduledKind, target: u64) -> bool {
        let Some(frame) = self.mixer.trigger(&self.bank, sound, target) else {
            return false;
        };
        if self.scheduled.len() < self.scheduled.capacity() {
            self.scheduled.push(Scheduled {
                sound,
                kind,
                frame,
                at: self.clock.time_at(frame),
            });
        } else {
            self.scheduled_dropped += 1;
        }
        true
    }

    /// One wake: waits for the output's buffer event, re-anchors the mix
    /// clock on it, starts every queued key sound at the first frame not
    /// yet written, starts the BGM events that fall in the block, and
    /// renders the block. A [priming](crate::Wake::priming) wake only fills
    /// the queue: the clock, the commands and the BGM wait for the first
    /// timed wake.
    ///
    /// # Errors
    ///
    /// The output's failure.
    pub fn step(
        &mut self,
        out: &mut impl AudioOutput,
        cmds: &mut Consumer<SoundCmd>,
    ) -> Result<StepReport, OutputError> {
        let wake = out.wait();
        let mut report = StepReport::default();
        if wake.priming {
            // No engine pass to time a start against yet: silence.
            out.write(|buf| {
                report.frames = (buf.len() / CHANNELS) as u32;
                self.mixer.render(&self.bank, buf);
            })?;
            report.active_voices = self.mixer.sounding();
            report.bgm_left = self.bgm.len() - self.next_bgm;
            return Ok(report);
        }
        report.at = Some(wake.at);
        if wake.padding == 0 && self.written() > 0 {
            // The engine may have played silence: the frame/time relation
            // moved.
            self.underruns += 1;
            report.underrun = true;
            self.clock.reset();
        }
        self.clock
            .observe(wake.queued_from, TimeUs(wake.at.0 + self.period_us));
        // A command or BGM event that finds the mixer's pending starts full
        // stays queued for the next step, which starts it at the first frame
        // it can still write, head intact.
        while self.mixer.can_start() {
            let Ok(cmd) = cmds.pop() else { break };
            report.commands += 1;
            let target = self.clock.frame_at(cmd.origin).unwrap_or(0);
            let kind = ScheduledKind::Key { origin: cmd.origin };
            self.start(cmd.sound, kind, target);
        }
        let mut deferred = !self.mixer.can_start() && !cmds.is_empty();
        out.write(|buf| {
            let block_end = self.written() + (buf.len() / CHANNELS) as u64;
            // Starts the pass may still admit: fewer in the pass that
            // places a seek, by what its plan admitted there.
            let mut budget = usize::MAX;
            let mut held = false;
            if let Some((t, left)) = self.seek_to {
                // Placed by the clock of the pass that reaches it, as a BGM
                // event at `t` is. Until then the schedule after the plan
                // waits, so none of it starts before the seek's voices.
                let frame = self.frame_line().frame(t);
                if frame < i64::try_from(block_end).unwrap_or(i64::MAX) {
                    self.mixer.place(frame);
                    self.seek_to = None;
                    // A clock moved past the frame leaves the plan's pass
                    // behind, and its share with it.
                    if frame >= i64::try_from(self.written()).unwrap_or(i64::MAX) {
                        budget = left;
                    }
                } else {
                    held = true;
                }
            }
            // A ring-out goes on the output at the pass reaching its fade,
            // by that pass's clock.
            if let Some((fade_at, end_at)) = self.ring_out {
                let line = self.frame_line();
                let (fade, end) = (line.frame(fade_at), line.frame(end_at));
                if fade < i64::try_from(block_end).unwrap_or(i64::MAX) {
                    self.mixer.ring_out(fade, end);
                    self.ring_out = None;
                    self.ring_end = Some(end);
                }
            }
            while let Some(&event) = self.bgm.get(self.next_bgm).filter(|_| !held) {
                let at = output_time(self.play_origin, self.output_offset, event.time);
                let target = self.clock.frame_at(at).unwrap_or(0);
                // Nothing starts from a ring-out's end on.
                if self
                    .ring_end
                    .is_some_and(|end| i64::try_from(target).unwrap_or(i64::MAX) >= end)
                {
                    self.next_bgm = self.bgm.len();
                    break;
                }
                if target >= block_end {
                    break;
                }
                if !self.mixer.can_start() || budget == 0 {
                    deferred = true;
                    break;
                }
                self.next_bgm += 1;
                report.bgm_started += 1;
                if self.start(event.sound, ScheduledKind::Bgm { time: event.time }, target) {
                    budget -= 1;
                }
            }
            report.frames = (buf.len() / CHANNELS) as u32;
            self.mixer.render(&self.bank, buf);
            // Rendering reached a ring-out's end: everything stopped there,
            // a seek still waiting included, and nothing of the schedule
            // starts any more.
            if self.mixer.rung_out() {
                self.seek_to = None;
                self.next_bgm = self.bgm.len();
            }
        })?;
        if deferred {
            self.deferred_wakes += 1;
        }
        report.active_voices = self.mixer.sounding();
        report.bgm_left = self.bgm.len() - self.next_bgm;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bms_model::{BgmEvent, SoundId, TimeUs};

    use super::AudioEngine;
    use crate::bank::SoundBank;
    use crate::seek::{BgmSchedule, HASHED};

    /// The schedule is hashed where it is built, once; taking it in walks
    /// none of it, however long it is.
    #[test]
    fn replace_does_not_hash_the_schedule() {
        let bank = || Arc::new(SoundBank::from_sounds(48_000, Vec::new()));
        let mut e = AudioEngine::new(bank(), &[], 48_000, 480, 4, 4).unwrap();
        let events: Vec<BgmEvent> = (0..500_000)
            .map(|k| BgmEvent {
                time: TimeUs(k),
                sound: SoundId(0),
            })
            .collect();
        let before = HASHED.with(std::cell::Cell::get);
        let schedule = BgmSchedule::new(events);
        assert_eq!(HASHED.with(std::cell::Cell::get), before + 1);
        let (_, old) = e.replace(bank(), schedule).unwrap();
        assert!(old.events().is_empty());
        assert_eq!(HASHED.with(std::cell::Cell::get), before + 1);
        assert_eq!(e.bgm_left(), 0);
    }
}
