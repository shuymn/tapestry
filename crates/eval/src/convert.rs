//! Input conversion: physical control changes to lane press and release
//! edges.

use bms_model::{LANE_COUNT, Lane};

use crate::EvalError;
use crate::input::{ControlId, DeviceProfile, InputKind, InputStart, PhysicalInputEvent};

/// A lane's state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneEdge {
    /// The lane went from released to pressed.
    Press(Lane),
    /// The lane went from pressed to released.
    Release(Lane),
}

/// The edges one input produced, in order: at most one per lane.
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
        // A lane changes at most once per input, so this never overflows.
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

/// One source's bound controls, sorted by control, and which are pressed.
#[derive(Clone, Debug)]
struct SourceState {
    bindings: Vec<(ControlId, Lane)>,
    pressed: Vec<bool>,
}

/// Which bound controls are pressed, and so which lanes are held.
///
/// A lane is held while any control bound to it is pressed (several
/// controls, of one source or of several, may play one lane). A press of a
/// control already pressed (such as a key's auto-repeat) and a release of a
/// control not pressed change nothing. Unbound controls are ignored.
#[derive(Clone, Debug)]
pub struct ConversionState {
    sources: Vec<SourceState>,
    held: [u32; LANE_COUNT],
}

impl ConversionState {
    /// The state at the start of a play: every control in `start` that is
    /// bound starts pressed, without a press edge.
    ///
    /// # Errors
    ///
    /// [`EvalError::DuplicateBinding`] when a source binds one control twice.
    pub fn init(device: &DeviceProfile, start: &InputStart) -> Result<Self, EvalError> {
        let mut sources = Vec::with_capacity(device.sources.len());
        for (index, profile) in device.sources.iter().enumerate() {
            let mut bindings: Vec<(ControlId, Lane)> = profile
                .bindings
                .iter()
                .map(|b| (b.control, b.lane))
                .collect();
            bindings.sort_unstable_by_key(|&(control, _)| control);
            if let Some(w) = bindings.windows(2).find(|w| w[0].0 == w[1].0) {
                return Err(EvalError::DuplicateBinding {
                    source: crate::SourceId(u16::try_from(index).unwrap_or(u16::MAX)),
                    control: w[0].0,
                });
            }
            let pressed = vec![false; bindings.len()];
            sources.push(SourceState { bindings, pressed });
        }
        let mut state = Self {
            sources,
            held: [0; LANE_COUNT],
        };
        for &(source, control, value) in &start.held {
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

    /// Applies one input and returns the lane edges it caused.
    pub fn apply(&mut self, ev: &PhysicalInputEvent) -> Edges {
        let mut edges = Edges::new();
        let source = ev.source.index();
        match ev.kind {
            InputKind::Control { control, value } => {
                if let Some(edge) = self.set(source, control, value != 0) {
                    edges.push(edge);
                }
            }
            InputKind::Disconnected => {
                let Some(state) = self.sources.get_mut(source) else {
                    return edges;
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
        }
        edges
    }

    /// Sets one control and returns the lane edge it caused, if any.
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
        let held = &mut self.held[lane.index()];
        if pressed {
            *held += 1;
            (*held == 1).then_some(LaneEdge::Press(lane))
        } else {
            *held -= 1;
            (*held == 0).then_some(LaneEdge::Release(lane))
        }
    }
}
