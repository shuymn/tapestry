//! The evaluator's input contract: physical input events, the device profile
//! that maps them to lanes, and the snapshot of controls held when recording
//! opened.

use bms_model::{Lane, TimeUs};

/// A source: one physical device, with its own queue and input offset.
/// Ids are anonymous and session-local, given in the order devices are found
/// when recording opens; the id is the index into
/// [`DeviceProfile::sources`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub u16);

impl SourceId {
    /// The index into [`DeviceProfile::sources`].
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A control of a source: a key's scan code, a button or an axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControlId(pub u32);

/// What happened at a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// A control changed. For a button, a nonzero value is pressed and zero
    /// is released.
    Control {
        /// The control.
        control: ControlId,
        /// Its new value.
        value: i32,
    },
    /// The device was removed: input conversion releases every control of
    /// the source it holds.
    Disconnected,
}

/// One recorded physical input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalInputEvent {
    /// The true receive time: the floor of the QPC read taken when the batch
    /// was received, the same for every event of the batch.
    pub received: TimeUs,
    /// The source.
    pub source: SourceId,
    /// What happened.
    pub kind: InputKind,
}

/// The controls held when play recording opened, copied at a boundary
/// between two input batches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputStart {
    /// The QPC read at the boundary.
    pub opened_at: TimeUs,
    /// Each held control with its value; a control not listed is released.
    pub held: Vec<(SourceId, ControlId, i32)>,
}

/// A control bound to a lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The control.
    pub control: ControlId,
    /// The lane it plays.
    pub lane: Lane,
}

/// How one source is evaluated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceProfile {
    /// The input latency offset, subtracted from the receive time.
    pub input_offset: TimeUs,
    /// The source's lane bindings; each control appears at most once.
    pub bindings: Vec<Binding>,
}

/// How every source of a play is evaluated, indexed by [`SourceId`].
///
/// A source without a profile has no bindings and a zero offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceProfile {
    /// The sources' profiles.
    pub sources: Vec<SourceProfile>,
}

impl DeviceProfile {
    /// The input latency offset of `source`.
    #[must_use]
    pub fn offset(&self, source: SourceId) -> TimeUs {
        self.sources
            .get(source.index())
            .map_or(TimeUs(0), |s| s.input_offset)
    }
}

/// The time an input counts at for judgment: its receive time minus its
/// source's input offset, saturating at the `i64` range.
#[must_use]
pub fn evaluation_time(ev: &PhysicalInputEvent, device: &DeviceProfile) -> TimeUs {
    TimeUs(ev.received.0.saturating_sub(device.offset(ev.source).0))
}
