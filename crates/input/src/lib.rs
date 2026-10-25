//! Input sources.
//!
//! One physical device is one source with its own queue, watermark and
//! input offset. The input thread pushes a batch of events into each
//! source's queue, then publishes `fresh − 1 µs` from a clock read taken
//! after the push ([`SourceSet::end_batch`]); the logic thread reads the
//! publication with `Acquire` before popping, so every event at or below it
//! is already poppable. The participants are frozen when recording opens,
//! and the controls held at that boundary go into [`eval::InputStart`].
//! The [`FocusGate`] keeps input received after focus loss out of the log.
//!
//! [`ScriptedInput`] feeds a scenario or a recorded log through the same
//! path, and [`autoplay`] makes the inputs that hit every note of a chart.
//!
//! After construction (and after [`SourceSet::open_recording`]), pushing
//! and publishing allocate nothing. The Raw Input adapter for Windows is
//! added under `cfg(windows)`.

#![forbid(unsafe_code)]

mod gate;
mod queue;
mod scripted;
mod sources;

use bms_model::TimeUs;

pub use gate::{Admit, DeviceId, FocusGate, FocusLatch, Packet};
pub use queue::{Overflow, SourceReader, SourceWriter, source_queue};
pub use scripted::{Action, ScriptItem, ScriptedInput, TimeOverflow, autoplay};
pub use sources::{Arrival, ConfigError, DeviceKind, SOURCE_LIMIT, SourceConfig, SourceSet};

/// Something that delivers input to a [`SourceSet`]: Raw Input on the input
/// thread, or a script in tests.
pub trait InputAdapter: Send {
    /// Delivers everything received up to `now`, in batches, and ends with
    /// a publication.
    fn poll(&mut self, now: TimeUs, sources: &mut SourceSet);
}
