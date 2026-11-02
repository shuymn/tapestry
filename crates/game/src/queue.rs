//! How many periods the audio output keeps queued ahead of playback (design.md
//! "Path from Press to Sound"). Two by default; a play with underruns after its
//! READY state makes the next play queue three, a choice kept in `device.toml`
//! that only the player lowers again, by editing the file.
//!
//! Some drivers deliver blocks whose size beats against the engine period,
//! and a stream can lock into a phase where two queued periods underrun
//! every few seconds; a third removes that at the cost of one period of key
//! sound latency, so it is taken only where it is needed.

use std::ops::RangeInclusive;

/// Periods queued until a play underruns.
pub const DEFAULT_QUEUED_PERIODS: u32 = 2;
/// Periods queued after a play underran.
pub const RAISED_QUEUED_PERIODS: u32 = 3;
/// The values `device.toml` may hold.
pub const QUEUED_PERIODS: RangeInclusive<u32> = DEFAULT_QUEUED_PERIODS..=RAISED_QUEUED_PERIODS;

/// The periods the next play queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueChoice(u32);

impl QueueChoice {
    /// The choice `device.toml` holds.
    #[must_use]
    pub const fn new(periods: u32) -> Self {
        Self(periods)
    }

    /// The periods the next play queues.
    #[must_use]
    pub const fn periods(self) -> u32 {
        self.0
    }

    /// A play ended, however it ended (completed, retried, quit or its
    /// device lost), after `steady_underruns` underruns from its chart time
    /// 0 on: READY before it, its first second, is the stream
    /// settling and does not count. Any raises the choice to
    /// [`RAISED_QUEUED_PERIODS`]; nothing lowers it. Returns whether it
    /// rose.
    pub fn play_ended(&mut self, steady_underruns: u64) -> bool {
        if steady_underruns == 0 || self.0 >= RAISED_QUEUED_PERIODS {
            return false;
        }
        self.0 = RAISED_QUEUED_PERIODS;
        true
    }
}
