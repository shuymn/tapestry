//! One wake of the audio thread (stage1.md D12): observe the buffer event,
//! drain sound commands, schedule BGM that falls in the block, render it.

use std::fmt;
use std::sync::Arc;

use bms_model::{BgmEvent, SoundId, TimeUs};
use rtrb::Consumer;

use crate::CHANNELS;
use crate::bank::SoundBank;
use crate::clock::MixClock;
use crate::mixer::{InvalidVolume, Mixer};
use crate::output::{AudioOutput, OutputError};

/// A key sound to play, from the logic thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoundCmd {
    /// The sound.
    pub sound: SoundId,
    /// The raw receive time of the input behind it (stage1.md D11).
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

/// A sound start the engine scheduled.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The output's rate or period is zero.
    ZeroRateOrPeriod,
    /// The bank was decoded at a rate other than the output's.
    RateMismatch {
        /// The bank's rate.
        bank: u32,
        /// The output's rate.
        output: u32,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRateOrPeriod => write!(f, "the output's rate or period is zero"),
            Self::RateMismatch { bank, output } => write!(
                f,
                "the sound bank is at {bank} Hz but the output runs at {output} Hz"
            ),
        }
    }
}

impl std::error::Error for EngineError {}

/// The audio engine: mixer, mix clock and BGM schedule. Everything is
/// allocated in [`AudioEngine::new`]; [`AudioEngine::step`] allocates
/// nothing.
#[derive(Debug)]
pub struct AudioEngine {
    bank: Arc<SoundBank>,
    mixer: Mixer,
    clock: MixClock,
    period_us: i64,
    bgm: Vec<BgmEvent>,
    next_bgm: usize,
    output_offset: TimeUs,
    /// Where chart time 0 lies on the output's timeline.
    play_origin: TimeUs,
    scheduled: Vec<Scheduled>,
    scheduled_dropped: u64,
    underruns: u64,
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
            bank,
            mixer: Mixer::new(voices, rate),
            clock: MixClock::new(rate, period),
            period_us: i64::from(period) * 1_000_000 / i64::from(rate),
            bgm: bgm.to_vec(),
            next_bgm: 0,
            output_offset: TimeUs(0),
            play_origin: TimeUs(0),
            scheduled: Vec::with_capacity(schedule_log),
            scheduled_dropped: 0,
            underruns: 0,
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
    /// timeline the output's wakes are in: every voice stops and the BGM
    /// schedule rewinds. The output stream and its clock continue.
    pub fn restart(&mut self, origin: TimeUs) {
        self.mixer.stop_all();
        self.next_bgm = 0;
        self.play_origin = origin;
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
    }

    /// Calls `f` with each scheduled start kept since the last drain, then
    /// forgets them.
    pub fn drain_scheduled(&mut self, mut f: impl FnMut(&Scheduled)) {
        self.scheduled.iter().for_each(&mut f);
        self.scheduled.clear();
    }

    /// Starts `sound` at output frame `target` (or the first unwritten
    /// frame) and logs where it started.
    fn start(&mut self, sound: SoundId, kind: ScheduledKind, target: u64) {
        let Some(frame) = self.mixer.trigger(&self.bank, sound, target) else {
            return;
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
        out.write(|buf| {
            let block_end = self.written() + (buf.len() / CHANNELS) as u64;
            while let Some(&event) = self.bgm.get(self.next_bgm) {
                let at = TimeUs(self.play_origin.0 + event.time.0 - self.output_offset.0);
                let target = self.clock.frame_at(at).unwrap_or(0);
                if target >= block_end || !self.mixer.can_start() {
                    break;
                }
                self.next_bgm += 1;
                report.bgm_started += 1;
                self.start(event.sound, ScheduledKind::Bgm { time: event.time }, target);
            }
            report.frames = (buf.len() / CHANNELS) as u32;
            self.mixer.render(&self.bank, buf);
        })?;
        report.active_voices = self.mixer.sounding();
        report.bgm_left = self.bgm.len() - self.next_bgm;
        Ok(report)
    }
}
