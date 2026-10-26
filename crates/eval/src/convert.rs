//! Input conversion: physical control changes to lane press and release
//! edges, including the analog scratch conversion of axes.

use bms_model::{LANE_COUNT, Lane, TimeUs};

use crate::EvalError;
use crate::input::{
    AxisMode, ControlId, DeviceProfile, InputKind, InputStart, PhysicalInputEvent, ScratchSettings,
    SourceId,
};

/// A lane's state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneEdge {
    /// The lane went from released to pressed.
    Press(Lane),
    /// The lane went from pressed to released.
    Release(Lane),
}

/// The edges one input produced, in order: a disconnect releases each lane
/// at most once, and a control change touches one lane, which an axis
/// reversing direction releases and presses again.
#[derive(Clone, Copy, Debug)]
pub struct Edges {
    items: [LaneEdge; LANE_COUNT],
    len: usize,
}

impl Edges {
    const fn new() -> Self {
        Self {
            items: [LaneEdge::Release(Lane::SCRATCH); LANE_COUNT],
            len: 0,
        }
    }

    fn push(&mut self, edge: LaneEdge) {
        // At most one edge per lane, or two on one lane: never more than
        // there are lanes.
        if let Some(slot) = self.items.get_mut(self.len) {
            *slot = edge;
            self.len += 1;
        }
    }

    /// The edges as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[LaneEdge] {
        &self.items[..self.len]
    }
}

/// One source's bound buttons, sorted by control, and which are pressed.
#[derive(Clone, Debug)]
struct SourceState {
    bindings: Vec<(ControlId, Lane)>,
    pressed: Vec<bool>,
}

/// Which way an axis moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// One bound axis's scratch conversion.
#[derive(Clone, Debug)]
struct AxisState {
    source: SourceId,
    control: ControlId,
    lane: Lane,
    settings: ScratchSettings,
    /// The last absolute report; `None` until the first.
    last: Option<i32>,
    /// Movement since the last press, hold or change of direction, signed
    /// forward.
    moved: i64,
    /// When the axis last moved.
    last_move: Option<TimeUs>,
    /// The direction it presses in, while it presses its lane.
    pressed: Option<Direction>,
    /// The pending release deadline, while it presses its lane.
    release_at: Option<TimeUs>,
}

impl AxisState {
    /// The movement a report means, forward positive.
    fn movement(&mut self, value: i32) -> i64 {
        let d = match self.settings.mode {
            AxisMode::Absolute { wrap } => match self.last.replace(value) {
                // The first report only says where the axis is.
                None => 0,
                Some(prev) => wrapped(i64::from(value) - i64::from(prev), wrap),
            },
            AxisMode::Relative => i64::from(value),
        };
        if self.settings.invert { -d } else { d }
    }

    /// Back to released and unknown, as after a disconnect.
    fn clear(&mut self) {
        self.last = None;
        self.moved = 0;
        self.last_move = None;
        self.pressed = None;
        self.release_at = None;
    }
}

/// `d` taken the short way round a revolution of `wrap` positions:
/// within `(−wrap/2, wrap/2]`; unchanged when `wrap` is 0.
fn wrapped(d: i64, wrap: u32) -> i64 {
    if wrap == 0 {
        return d;
    }
    let wrap = i64::from(wrap);
    let d = d.rem_euclid(wrap);
    if d > wrap / 2 { d - wrap } else { d }
}

/// Which bound controls are pressed, and so which lanes are held.
///
/// A lane is held while any control bound to it is pressed (several
/// controls, of one source or of several, may play one lane; a button and
/// an axis on one lane combine the same way). A press of a control already
/// pressed (such as a key's auto-repeat) and a release of a control not
/// pressed change nothing. Unbound controls are ignored.
///
/// A bound axis presses and releases its lane by the scratch conversion of
/// [`ScratchSettings`]; its pending release deadlines take part in the
/// evaluator's scan ([`next_release`](Self::next_release)).
#[derive(Clone, Debug)]
pub struct ConversionState {
    sources: Vec<SourceState>,
    axes: Vec<AxisState>,
    held: [u32; LANE_COUNT],
}

impl ConversionState {
    /// The state at the start of a play: every button in `start` that is
    /// bound starts pressed, without a press edge, and every absolute axis
    /// in `start` starts at its value, released.
    ///
    /// # Errors
    ///
    /// [`EvalError::DuplicateBinding`] when a source binds one control
    /// twice, as buttons, axes, or both.
    pub fn init(device: &DeviceProfile, start: &InputStart) -> Result<Self, EvalError> {
        let mut sources = Vec::with_capacity(device.sources.len());
        let mut axes = Vec::new();
        for (index, profile) in device.sources.iter().enumerate() {
            let source = SourceId(u16::try_from(index).unwrap_or(u16::MAX));
            let mut controls: Vec<ControlId> = profile
                .bindings
                .iter()
                .map(|b| b.control)
                .chain(profile.axes.iter().map(|a| a.control))
                .collect();
            controls.sort_unstable();
            if let Some(w) = controls.windows(2).find(|w| w[0] == w[1]) {
                return Err(EvalError::DuplicateBinding {
                    source,
                    control: w[0],
                });
            }
            let mut bindings: Vec<(ControlId, Lane)> = profile
                .bindings
                .iter()
                .map(|b| (b.control, b.lane))
                .collect();
            bindings.sort_unstable_by_key(|&(control, _)| control);
            let pressed = vec![false; bindings.len()];
            sources.push(SourceState { bindings, pressed });
            axes.extend(profile.axes.iter().map(|a| AxisState {
                source,
                control: a.control,
                lane: a.lane,
                settings: a.scratch,
                last: None,
                moved: 0,
                last_move: None,
                pressed: None,
                release_at: None,
            }));
        }
        let mut state = Self {
            sources,
            axes,
            held: [0; LANE_COUNT],
        };
        for &(source, control, value) in &start.held {
            if let Some(axis) = state
                .axes
                .iter_mut()
                .find(|a| a.source == source && a.control == control)
            {
                // Only an absolute axis reads it.
                axis.last = Some(value);
                continue;
            }
            // Held controls start pressed; no edge is reported for them.
            state.set(source.index(), control, value != 0);
        }
        Ok(state)
    }

    /// Whether `lane` is held.
    #[must_use]
    pub fn is_held(&self, lane: Lane) -> bool {
        self.held[lane.index()] > 0
    }

    /// The earliest pending scratch release: its deadline, its lane, and
    /// the axis to pass to [`release_due`](Self::release_due). Ties go to
    /// the lower lane, then to the axis bound first (by source, then in
    /// the order of its bindings).
    #[must_use]
    pub fn next_release(&self) -> Option<(TimeUs, Lane, usize)> {
        self.axes
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.release_at.map(|t| (t, a.lane, i)))
            .min()
    }

    /// Releases axis `axis` at its pending deadline, which the evaluator
    /// reached ([`next_release`](Self::next_release)), and returns the lane
    /// edge it caused, if any. An axis with no pending release changes
    /// nothing.
    pub fn release_due(&mut self, axis: usize) -> Option<LaneEdge> {
        let a = self.axes.get_mut(axis)?;
        a.release_at.take()?;
        a.pressed = None;
        a.moved = 0;
        let lane = a.lane;
        self.unhold(lane)
    }

    /// Applies one input, at evaluation time `at`, and returns the lane
    /// edges it caused.
    pub fn apply(&mut self, ev: &PhysicalInputEvent, at: TimeUs) -> Edges {
        let mut edges = Edges::new();
        match ev.kind {
            InputKind::Control { control, value } => {
                if let Some(i) = self
                    .axes
                    .iter()
                    .position(|a| a.source == ev.source && a.control == control)
                {
                    self.axis_report(i, value, at, &mut edges);
                } else if let Some(edge) = self.set(ev.source.index(), control, value != 0) {
                    edges.push(edge);
                }
            }
            InputKind::Disconnected => self.disconnect(ev.source, &mut edges),
        }
        edges
    }

    /// Releases every control of `source` and clears its axes.
    fn disconnect(&mut self, source: SourceId, edges: &mut Edges) {
        for i in 0..self.axes.len() {
            let a = &mut self.axes[i];
            if a.source != source {
                continue;
            }
            let was_pressed = a.pressed.is_some();
            let lane = a.lane;
            a.clear();
            if was_pressed && let Some(edge) = self.unhold(lane) {
                edges.push(edge);
            }
        }
        let Some(state) = self.sources.get_mut(source.index()) else {
            return;
        };
        for (i, pressed) in state.pressed.iter_mut().enumerate() {
            if std::mem::replace(pressed, false) {
                let lane = state.bindings[i].1;
                let held = &mut self.held[lane.index()];
                *held -= 1;
                if *held == 0 {
                    edges.push(LaneEdge::Release(lane));
                }
            }
        }
    }

    /// One report of axis `i` at `at`.
    fn axis_report(&mut self, i: usize, value: i32, at: TimeUs, edges: &mut Edges) {
        let a = &mut self.axes[i];
        let d = a.movement(value);
        if d == 0 {
            return;
        }
        let s = a.settings;
        let release_after = i64::from(s.release_after_us);
        // Movement adds up only in one direction and without a pause
        // longer than the release time.
        let paused = a
            .last_move
            .is_none_or(|m| at.0.saturating_sub(m.0) > release_after);
        if paused || a.moved.signum() != d.signum() {
            a.moved = 0;
        }
        a.moved = a.moved.saturating_add(d);
        a.last_move = Some(at);
        let direction = if a.moved > 0 {
            Direction::Forward
        } else {
            Direction::Backward
        };
        let moved = a.moved.unsigned_abs();
        // `moved` is at least 1 here, so a threshold of 0 acts as 1.
        let press = u64::from(s.press_threshold);
        let hold = u64::from(s.hold_threshold);
        let reversal = match a.pressed {
            None if moved >= press => false,
            Some(p) if p != direction && moved >= press => true,
            Some(p) if p == direction && moved >= hold => {
                // Held: the release moves later.
                a.release_at = Some(TimeUs(at.0.saturating_add(release_after)));
                a.moved = 0;
                return;
            }
            _ => return,
        };
        a.pressed = Some(direction);
        a.release_at = Some(TimeUs(at.0.saturating_add(release_after)));
        a.moved = 0;
        let lane = a.lane;
        // A reversal is a new press.
        if reversal && let Some(edge) = self.unhold(lane) {
            edges.push(edge);
        }
        if let Some(edge) = self.hold(lane) {
            edges.push(edge);
        }
    }

    /// One more control holds `lane`: a press edge if it was released.
    fn hold(&mut self, lane: Lane) -> Option<LaneEdge> {
        let held = &mut self.held[lane.index()];
        *held += 1;
        (*held == 1).then_some(LaneEdge::Press(lane))
    }

    /// One control fewer holds `lane`: a release edge if none is left.
    fn unhold(&mut self, lane: Lane) -> Option<LaneEdge> {
        let held = &mut self.held[lane.index()];
        *held -= 1;
        (*held == 0).then_some(LaneEdge::Release(lane))
    }

    /// Sets one button and returns the lane edge it caused, if any.
    fn set(&mut self, source: usize, control: ControlId, pressed: bool) -> Option<LaneEdge> {
        let state = self.sources.get_mut(source)?;
        let i = state
            .bindings
            .binary_search_by_key(&control, |&(c, _)| c)
            .ok()?;
        if state.pressed[i] == pressed {
            return None;
        }
        state.pressed[i] = pressed;
        let lane = state.bindings[i].1;
        if pressed {
            self.hold(lane)
        } else {
            self.unhold(lane)
        }
    }
}
