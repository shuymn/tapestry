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
    /// is released; for an axis, the value is its report ([`AxisMode`]).
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
    /// An absolute axis listed here starts at its value; one not listed
    /// starts unknown, and its first report only sets where it is.
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

/// How an axis reports where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisMode {
    /// Each report is the position. `wrap` is the number of positions in
    /// one revolution: the axis goes from its highest value back to its
    /// lowest, so a change by more than half a revolution is taken the
    /// short way round (exactly half counts forward). 0: it does not wrap.
    Absolute {
        /// Positions per revolution, or 0.
        wrap: u32,
    },
    /// Each report is the movement since the previous report, used as is.
    Relative,
}

/// How an axis's movement becomes presses and releases of its lane
/// (design.md "Analog Scratch"). Movement is counted in the axis's own
/// units, after wraparound. Both directions press alike, so the direction
/// the axis counts in does not matter.
///
/// Released, the axis presses its lane once it has moved
/// `press_threshold` in one direction, each movement no more than
/// `release_after_us` after the previous one. Pressed, movement of
/// `hold_threshold` in the same direction keeps it pressed (the
/// hysteresis: less movement holds than presses), and movement of
/// `press_threshold` the other way is a new press in that direction.
/// Every press and every hold sets the release deadline `release_after_us`
/// later; when the watermark passes it, the lane is released. A threshold
/// of 0 counts as 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScratchSettings {
    /// Absolute or relative, and the wraparound.
    pub mode: AxisMode,
    /// Movement in one direction that presses.
    pub press_threshold: u32,
    /// Movement in the pressed direction that keeps the lane pressed.
    pub hold_threshold: u32,
    /// How long after the last press or hold the lane is released, µs.
    pub release_after_us: u32,
}

impl ScratchSettings {
    /// Defaults for the thresholds and the release time: a press after 2
    /// positions (a single position's flicker at rest never presses), held
    /// by any further movement, and released 100 ms after the movement
    /// stops; starting points, tuned by playing. The range is the device's
    /// to declare, so the default mode is absolute without wraparound,
    /// which the device's declared range replaces.
    pub const DEFAULT: Self = Self {
        mode: AxisMode::Absolute { wrap: 0 },
        press_threshold: 2,
        hold_threshold: 1,
        release_after_us: 100_000,
    };
}

impl Default for ScratchSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The most axes one source binds.
pub const MAX_AXES_PER_SOURCE: usize = 8;
/// The most axes a play binds, over every source.
pub const MAX_AXES: usize = 16;
/// The most buttons and keys one source binds.
pub const MAX_BINDINGS_PER_SOURCE: usize = 256;
/// The most buttons and keys a play binds, over every source.
pub const MAX_BINDINGS: usize = 1024;

/// An axis bound to a lane, with its scratch conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisBinding {
    /// The axis.
    pub control: ControlId,
    /// The lane it plays, usually the scratch.
    pub lane: Lane,
    /// How its movement becomes presses and releases.
    pub scratch: ScratchSettings,
}

/// How one source is evaluated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceProfile {
    /// The input latency offset, subtracted from the receive time.
    pub input_offset: TimeUs,
    /// The source's button and key bindings, at most
    /// [`MAX_BINDINGS_PER_SOURCE`].
    pub bindings: Vec<Binding>,
    /// The source's axis bindings, at most [`MAX_AXES_PER_SOURCE`]. A
    /// control appears at most once across `bindings` and `axes`.
    pub axes: Vec<AxisBinding>,
}

/// How every source of a play is evaluated, indexed by [`SourceId`].
///
/// A source without a profile has no bindings and a zero offset. Over
/// every source there are at most [`MAX_BINDINGS`] bindings and
/// [`MAX_AXES`] axes, so converting an input stays cheap whatever a log
/// declares.
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

/// Edges each recorded button gets per second of the recording window: a
/// press and a release 30 times a second (stage1.md D8).
pub const BUTTON_EDGES_PER_SEC: usize = 60;

/// The most button edges one source gets per second of the recording
/// window, whatever its number of buttons: 120 presses and releases a
/// second, beyond what hands play (stage1.md D8). Input beyond it overflows
/// and marks the log incomplete.
pub const SOURCE_BUTTON_EDGES_PER_SEC: usize = 240;

/// Reports each recorded axis gets per second of the recording window:
/// one per millisecond, a 1 kHz controller reporting without pause
/// (stage1.md D8).
pub const AXIS_REPORTS_PER_SEC: usize = 1000;

/// The inputs a recording window of `span` holds (stage1.md D8), for
/// sources recording `(buttons, axes)` each: per source, its recorded
/// buttons and keys [`BUTTON_EDGES_PER_SEC`] each, at most
/// [`SOURCE_BUTTON_EDGES_PER_SEC`] together, and its recorded axes
/// [`AXIS_REPORTS_PER_SEC`] each, over the span in whole seconds, rounded
/// up. A span that is not positive holds nothing; the sums saturate.
#[must_use]
pub fn window_inputs(span: TimeUs, sources: &[(usize, usize)]) -> usize {
    let secs = u64::try_from(span.0).unwrap_or(0).div_ceil(1_000_000);
    let secs = usize::try_from(secs).unwrap_or(usize::MAX);
    sources
        .iter()
        .map(|&(buttons, axes)| {
            buttons
                .saturating_mul(BUTTON_EDGES_PER_SEC)
                .min(SOURCE_BUTTON_EDGES_PER_SEC)
                .saturating_add(axes.saturating_mul(AXIS_REPORTS_PER_SEC))
        })
        .fold(0_usize, usize::saturating_add)
        .saturating_mul(secs)
}
