//! Sound bank, mixer and audio clock (design.md "Audio", "Aligning Audio to
//! the Judgment Timeline").
//!
//! - [`load_bank`] decodes the sounds a chart references into i16 at the
//!   device rate, stores files with identical contents once, and refuses a
//!   bank over the caps of [`BankLimits`] before decoding when the files
//!   declare their length, and while decoding otherwise.
//! - [`Mixer`] sums preallocated voices in `f32`, steals the oldest voice
//!   with a short fade when all are busy, applies the volume and a peak
//!   limiter without look-ahead, so it adds no latency.
//! - [`MixClock`] maps output frames to the judgment timeline from the
//!   buffer events, re-anchored on every period.
//! - [`AudioEngine::step`] is one wake of the audio thread: it drains sound
//!   commands, schedules BGM, and renders one block into an
//!   [`AudioOutput`]. [`SimDevice`] and [`RecordingOutput`] stand in for the
//!   device in tests and headless runs.
//!
//! [`run_audio`] is the audio thread loop: one step per buffer event until
//! asked to stop.
//!
//! Output buffers are two interleaved `f32` channels (left, right). Nothing
//! on the step path allocates once the engine is built.

mod bank;
mod clock;
mod engine;
mod mixer;
mod output;
mod run;
mod sim;

pub use bank::{
    BankError, BankLimits, BankWarning, Cap, Found, LoadStats, MAX_RATE, MIN_RATE, Sound,
    SoundBank, load_bank,
};
pub use clock::{MIX_CLOCK_REBASE_AFTER, MIX_CLOCK_WINDOW, MixClock};
pub use engine::{AudioEngine, EngineError, Scheduled, ScheduledKind, SoundCmd, StepReport};
pub use mixer::{
    InvalidVolume, LIMITER_CEILING, LIMITER_RELEASE_MS, Limiter, MAX_PENDING_STARTS, MAX_VOICES,
    MAX_VOLUME, Mixer, STEAL_FADE_MS,
};
pub use output::{AudioOutput, OutputError, RecordingOutput, Wake};
pub use run::{RunReport, run_audio};
pub use sim::{Resume, SHIFT_HISTORY, SimConfig, SimDevice};

/// Channels of every output buffer (interleaved left, right).
pub const CHANNELS: usize = 2;
