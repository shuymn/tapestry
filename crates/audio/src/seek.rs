//! Preparing a mid-chart start off the audio thread (design.md "Audio"):
//! [`SeekPlan::new`] decides every start, stop, cut and steal of a schedule
//! before the seek's frame as the engine's passes would, block admission
//! included, without summing samples, and keeps the voices and releases
//! left sounding there. [`AudioEngine::seek`](crate::AudioEngine::seek)
//! only copies them in, so the audio thread's share is bounded by the voice
//! count whatever the chart.

use bms_model::{BgmEvent, TimeUs};

use crate::bank::SoundBank;
use crate::mixer::{Counts, MAX_PENDING_STARTS, Slots, Voice, fade_frames};

/// Where a play's chart times fall on the output, as the engine maps them:
/// chart time `x` is due at frame `floor((x + base_us) × rate / 1,000,000)`,
/// and each engine pass writes a block of `period` frames, one of which
/// starts at `block_start`. [`AudioEngine::frame_line`](crate::AudioEngine::frame_line)
/// gives the engine's current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLine {
    /// The output rate.
    pub rate: u32,
    /// Frames per engine pass.
    pub period: u32,
    /// A frame where a pass's block starts.
    pub block_start: u64,
    /// The play's origin minus the output offset and the mix clock's
    /// anchor (the time frame 0 is mixed at), µs.
    pub base_us: i64,
}

impl FrameLine {
    /// The frame chart time `t` is due at, saturating at the `i64` range.
    #[must_use]
    pub fn frame(&self, t: TimeUs) -> i64 {
        let us = i128::from(t.0) + i128::from(self.base_us);
        let frame = (us * i128::from(self.rate)).div_euclid(1_000_000);
        frame.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }

    /// The first frame of the block holding `frame`.
    fn block_of(&self, frame: i64) -> i64 {
        let period = i64::from(self.period.max(1));
        let grid = i128::from(self.block_start).rem_euclid(i128::from(period)) as i64;
        frame - (frame - grid).rem_euclid(period)
    }
}

/// A mid-chart start worked out ahead: the voices and releases a schedule
/// leaves sounding at the seek's frame, each with its position in its sound
/// or fade, and where the schedule goes on. It holds two 32-byte slots per
/// voice, 16 KiB with 256 voices, whatever the chart.
#[derive(Clone, Debug)]
pub struct SeekPlan {
    t: TimeUs,
    line: FrameLine,
    /// The frame of `t` on `line`.
    at: i64,
    /// The schedule's length, and its starts the plan covers.
    events: usize,
    split: usize,
    /// Starts the block holding `at` can still admit.
    budget: usize,
    /// The last trigger order used.
    seq: u64,
    slots: Slots,
}

impl SeekPlan {
    /// Plans a seek to chart time `t` of `schedule` (sorted by time, as the
    /// engine plays it) on `bank`, for an engine with `voices` voices whose
    /// play falls on `line`. Each engine pass admits at most
    /// [`MAX_PENDING_STARTS`] starts, as playing does, and the rest wait
    /// for the next pass's first frame. Allocates the plan's slots; the work
    /// grows with the starts before `t` times the voices.
    #[must_use]
    pub fn new(
        bank: &SoundBank,
        schedule: &[BgmEvent],
        voices: usize,
        line: FrameLine,
        t: TimeUs,
    ) -> Self {
        let fade = fade_frames(line.rate);
        let at = line.frame(t);
        let block_at = line.block_of(at);
        let period = i64::from(line.period.max(1));
        let mut slots = Slots::new(voices);
        let mut unheard = Counts::default();
        let mut seq = 0;
        // The block admitting starts, and how many it has admitted.
        let mut block = i64::MIN;
        let mut admitted = 0;
        let mut admitted_at = 0;
        let mut retired_to = i64::MIN;
        let mut split = 0;
        for e in schedule {
            let frame = line.frame(e.time);
            let own = line.block_of(frame);
            if own > block {
                block = own;
                admitted = 0;
            } else if admitted == MAX_PENDING_STARTS {
                // The pass was full: the start waits for the next one.
                block += period;
                admitted = 0;
            }
            let start = frame.max(block);
            if start >= at {
                break;
            }
            split += 1;
            let Some(sound) = bank.slot(e.sound) else {
                continue;
            };
            admitted += 1;
            if block == block_at {
                admitted_at += 1;
            }
            if start > retired_to {
                slots.retire(bank, fade, start);
                retired_to = start;
            }
            seq += 1;
            let voice = Voice {
                active: true,
                id: e.sound,
                sound,
                start,
                seq,
                cut: i64::MAX,
            };
            slots.apply(voice, &mut unheard);
        }
        slots.retire(bank, fade, at);
        Self {
            t,
            line,
            at,
            events: schedule.len(),
            split,
            budget: MAX_PENDING_STARTS - admitted_at,
            seq,
            slots,
        }
    }

    /// The chart time it starts at.
    #[must_use]
    pub fn time(&self) -> TimeUs {
        self.t
    }

    /// The frame line it was planned on.
    #[must_use]
    pub fn line(&self) -> FrameLine {
        self.line
    }

    /// The voices it was planned for.
    #[must_use]
    pub fn voices(&self) -> usize {
        self.slots.voices.len()
    }

    /// Voices and fading releases sounding at its frame.
    #[must_use]
    pub fn sounding(&self) -> usize {
        self.slots.all().filter(|v| v.active).count()
    }

    pub(crate) fn at(&self) -> i64 {
        self.at
    }

    pub(crate) fn events(&self) -> usize {
        self.events
    }

    pub(crate) fn split(&self) -> usize {
        self.split
    }

    pub(crate) fn budget(&self) -> usize {
        self.budget
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn slots(&self) -> &Slots {
        &self.slots
    }
}
