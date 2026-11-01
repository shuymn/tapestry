//! Sound bank, mixer and audio clock (design.md "Audio", "Aligning Audio to
//! the Judgment Timeline").
//!
//! - [`load_bank`] decodes the sounds a chart references into i16 at the
//!   device rate, stores files with identical contents once, and refuses a
//!   bank over the caps of [`BankLimits`] before decoding when the files
//!   declare their length, and while decoding otherwise. [`load_bank_at`]
//!   converts them for a practice [`Speed`] in a second [`Stage`], with each
//!   stage's workspace bound charged before it is built.
//! - [`Mixer`] sums preallocated voices in `f32`, steals the oldest voice
//!   with a short fade when all are busy, applies the volume and the play's
//!   fixed gain (0.5, or the chart's normalization gain), and a static soft
//!   clipper that keeps the sum within full scale without riding its gain
//!   or adding latency.
//! - [`MixClock`] maps output frames to the judgment timeline from the
//!   buffer events, re-anchored on every period.
//! - [`AudioEngine::step`] is one wake of the audio thread: it drains sound
//!   commands, schedules BGM, and renders one block into an
//!   [`AudioOutput`]. [`SimDevice`] and [`RecordingOutput`] stand in for the
//!   device in tests and headless runs. [`SeekPlan`] works out, off the audio
//!   thread, the sounds still playing partway into a schedule, and
//!   [`AudioEngine::seek`] starts a play there.
//!
//! [`run_audio`] is the audio thread loop: one step per buffer event until
//! asked to stop. On Windows, `WasapiShared` is the device: WASAPI shared
//! mode, event-driven, at the shortest engine period the endpoint offers.
//!
//! Output buffers are two interleaved `f32` channels (left, right). Nothing
//! on the step path allocates once the engine is built.

// Only the Windows OS calls use `unsafe`, in the `wasapi` module.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

mod bank;
mod clock;
mod engine;
mod loudness;
mod mixer;
mod output;
mod run;
mod seek;
mod sim;
#[cfg(windows)]
#[allow(unsafe_code)]
mod wasapi;

pub use bank::{
    BankError, BankLimits, BankWarning, Cap, Conversion, Found, LoadStats, MAX_RATE, MIN_RATE,
    Sound, SoundBank, Speed, Stage, load_bank, load_bank_at, load_bank_reporting, plan_conversion,
};
pub use clock::{MIX_CLOCK_REBASE_AFTER, MIX_CLOCK_WINDOW, MixClock};
pub use engine::{
    AudioEngine, ChartSounds, EngineError, Scheduled, ScheduledKind, SeekError, SoundCmd,
    StepReport,
};
pub use loudness::{
    ANALYSIS_VERSION, Analysis, AnalysisError, BLOCK_FRAMES, CEILING_LUFS,
    MEASUREMENT_DEPENDENCIES, MIXER_VERSION, Measurement, PEAK_CEILING_DBTP, Step, TARGET_LUFS,
    normalization_gain,
};
pub use mixer::{
    Gain, HEADROOM, InvalidGain, InvalidVolume, MAX_GAIN, MAX_PENDING_STARTS, MAX_VOICES,
    MAX_VOLUME, Mixer, SOFT_CLIP_KNEE, STEAL_FADE_MS, soft_clip,
};
pub use output::{AudioOutput, MAX_QUEUE_PERIODS, OutputError, RecordingOutput, Wake};
pub use run::{RunReport, run_audio};
pub use seek::{BgmSchedule, FrameLine, SeekPlan, SeekSource};
pub use sim::{Resume, SHIFT_HISTORY, SimConfig, SimDevice};
#[cfg(windows)]
pub use wasapi::{
    EVENT_TIMEOUT_MS, EnginePeriods, Fallback, Grant, QUEUE_PERIODS, WakeTrace, WasapiError,
    WasapiShared, WasapiStats,
};

/// Channels of every output buffer (interleaved left, right).
pub const CHANNELS: usize = 2;
