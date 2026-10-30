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
//! While recording is closed, the configured controller's bound controls
//! can drive the menus instead ([`MenuTap`], [`SourceSet::set_menu`]),
//! through a bounded queue; nothing of it is recorded.
//!
//! [`ScriptedInput`] feeds a scenario or a recorded log through the same
//! path. [`autoplay`] makes the inputs that hit every note of a chart, and
//! [`AutoplayInput`] feeds them on the input thread, submitting every due
//! event before its source publishes past it (the queue is sized for the
//! whole stream; an overflow marks the recording incomplete).
//!
//! On Windows, the input thread (`InputThread`) receives keyboards and
//! HID game controllers through Raw Input and stamps each batch with the
//! QPC read at its wake (`QpcClock`); when it wakes is [`cadence`]'s
//! portable state machine. Decoding the Raw Input packets
//! ([`raw`]), turning controller reports into packets ([`hid`]) and
//! converting QPC ticks ([`qpc_to_us`]) are portable.
//!
//! After construction (and after [`SourceSet::open_recording`]), pushing
//! and publishing allocate nothing.

// Only the Windows OS calls use `unsafe`, in the `win` module.
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]

mod autoplay;
pub mod cadence;
mod capture;
mod gate;
pub mod hid;
mod menu;
mod queue;
pub mod raw;
mod scripted;
mod sources;
mod time;
#[cfg(windows)]
#[allow(unsafe_code)]
mod win;

use bms_model::TimeUs;

pub use autoplay::{AUTOPLAY_DEVICE, AutoplayInput, TimeOverflow, autoplay, autoplay_count};
pub use cadence::{Phase, WakeReport};
pub use capture::{Capture, Watched};
pub use gate::{Admit, DeviceId, FocusGate, FocusLatch, Packet};
pub use menu::{
    MENU_QUEUE, MenuAction, MenuBindings, MenuEvent, MenuReceiver, MenuTap, menu_queue,
};
pub use queue::{Overflow, SourceReader, SourceWriter, source_queue};
pub use scripted::{Action, ScriptItem, ScriptedInput, declared_axes};
pub use sources::{Arrival, ConfigError, DeviceKind, SOURCE_LIMIT, SourceConfig, SourceSet};
pub use time::{Percentile, QpcFrequency, WAKE_HISTOGRAM_LIMIT, WakeHistogram, qpc_to_us};
#[cfg(windows)]
pub use win::{
    ClockedThread, CollectionInfo, InputError, InputThread, QpcClock, Waker, controller_layouts,
    hid_usages, keyboard_target,
};

/// Something that delivers input to a [`SourceSet`], such as a script in
/// tests. The Raw Input receiver polls the same way but is not `Send`: it
/// stays on the input thread that owns its window.
pub trait InputAdapter: Send {
    /// Delivers everything received up to `now`, in batches, and ends with
    /// a publication.
    fn poll(&mut self, now: TimeUs, sources: &mut SourceSet);
}
