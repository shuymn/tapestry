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
//! Output buffers are two interleaved `f32` channels (left, right). Nothing
//! on the step path allocates once the engine is built.

mod bank;

pub use bank::{
    BankError, BankLimits, BankWarning, Cap, Found, LoadStats, Sound, SoundBank, load_bank,
};

/// Channels of every output buffer (interleaved left, right).
pub const CHANNELS: usize = 2;
