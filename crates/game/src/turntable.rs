//! The turntable's input for the presentation (docs/stage3.md, offset 1):
//! which scratch directions the devices hold, published with the
//! adjustments' snapshot; the play states its clock belongs to; and the
//! scratch lane's notes autoplay's presentation presses.
//!
//! - **Device input** ([`ScratchDevice`]): the adjustment driver counts
//!   the holders of each digital scratch direction and steps the
//!   evaluator's analog rule ([`eval::AxisTurn`]) with every movement of
//!   the scratch axis, its reversal applied, before the cover
//!   adjustment's quantum filters it. Priority is the frame's
//!   ([`ScratchInput`]).
//! - **Play states** ([`PlayStates`]): created by a chart chosen to play,
//!   P, or a retry, each numbered and stamped with its origin on the
//!   shared clock.
//! - **Autoplay notes** ([`scratch_notes`]): the scratch lane's taps and
//!   long notes as events, built before READY.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, Lane, LnMode, NoteKind, TimeUs};
use eval::{AxisDirection, AxisTurn};
use input::{TapEvent, TapObservation};

use crate::adjust_keys::AdjustmentPlan;
use crate::adjustment::{ControlId, Role};

/// The analog scratch's press as the presentation reads it: its direction
/// and when its release is due on the input clock, unless it moves again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalogTurn {
    /// Whether it presses the negative direction.
    pub negative: bool,
    /// Its release deadline: from then on it no longer presses.
    pub until: TimeUs,
}

/// The devices' scratch directions in one snapshot: the digital
/// directions held, and the analog press with its deadline. A frame
/// expires the analog press at its time, then Negative wins over Positive
/// ([`direction`](Self::direction)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScratchInput {
    /// A Positive holder is down.
    pub positive: bool,
    /// A Negative holder is down.
    pub negative: bool,
    /// The analog scratch's press, while it presses.
    pub analog: Option<AnalogTurn>,
}

/// The direction the turntable turns in on a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// Neither direction: the idle turn.
    Idle,
    /// The positive direction (`ScratchPositive`).
    Positive,
    /// The negative direction (`ScratchNegative`).
    Negative,
}

impl ScratchInput {
    /// Nothing held.
    pub const NONE: Self = Self {
        positive: false,
        negative: false,
        analog: None,
    };

    /// The direction at `now` on the input clock, with autoplay's
    /// presses (`auto`: Positive, Negative): the analog press expires
    /// first (its deadline at or before `now`), then Negative wins if any
    /// Negative is live, else Positive, else neither. Allocates nothing.
    #[must_use]
    pub fn direction(&self, now: TimeUs, auto: [bool; 2]) -> Turn {
        let [auto_positive, auto_negative] = auto;
        let analog = self.analog.filter(|a| a.until > now);
        let analog_negative = analog.is_some_and(|a| a.negative);
        let analog_positive = analog.is_some_and(|a| !a.negative);
        if self.negative || analog_negative || auto_negative {
            Turn::Negative
        } else if self.positive || analog_positive || auto_positive {
            Turn::Positive
        } else {
            Turn::Idle
        }
    }
}

/// The scratch's device state the adjustment driver keeps beside its
/// reducer: the controls holding each digital direction, a loss, and the
/// analog rule stepped with the scratch axis's movements. It lives with
/// the driver: an attempt's entry, a context or evaluation reset and the
/// adjustment's gesture suppression leave it alone.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScratchDevice {
    /// The controls of each direction, a bit each.
    positive: u128,
    negative: u128,
    /// The controls held, a bit each.
    held: u128,
    /// A loss not yet resynchronized with a verified held set.
    lost: bool,
    /// The scratch axis's number, and whether its movement is reversed.
    axis: Option<u8>,
    reversed: bool,
    turn: AxisTurn,
}

impl ScratchDevice {
    /// Nothing held, for `plan`'s controls and scratch axis.
    pub(crate) fn new(plan: &AdjustmentPlan) -> Self {
        let mut positive = 0;
        let mut negative = 0;
        for n in 0..u8::try_from(crate::adjustment::MAX_CONTROLS).unwrap_or(u8::MAX) {
            match plan.roles.get(ControlId(n)) {
                Some(Role::ScratchPositive) => positive |= 1 << n,
                Some(Role::ScratchNegative) => negative |= 1 << n,
                _ => {}
            }
        }
        Self {
            positive,
            negative,
            held: 0,
            lost: false,
            axis: plan.axis(),
            reversed: plan.reversed(),
            turn: AxisTurn::new(plan.scratch()),
        }
    }

    /// Takes one of the tap's observations, before the plan filters it.
    pub(crate) fn observe(&mut self, o: &TapObservation) {
        let bit = |n: u8| 1u128.checked_shl(u32::from(n)).unwrap_or(0);
        match o.event {
            TapEvent::Press(n) => self.held |= bit(n),
            TapEvent::Release(n) => self.held &= !bit(n),
            TapEvent::Axis { control, delta, .. } if Some(control) == self.axis => {
                let d = if self.reversed {
                    delta.saturating_neg()
                } else {
                    delta
                };
                let _ = self.turn.step(d, o.at);
            }
            TapEvent::Lost => self.lost = true,
            TapEvent::Resync { held, verified } => {
                self.held = held;
                self.lost = !verified;
            }
            TapEvent::Axis { .. }
            | TapEvent::Repeat(_)
            | TapEvent::Tick
            | TapEvent::Opened { .. }
            | TapEvent::Cancelled { .. }
            | TapEvent::Fence(_) => {}
        }
    }

    /// What the snapshot publishes: nothing during a loss.
    pub(crate) fn input(&self) -> ScratchInput {
        if self.lost {
            return ScratchInput::NONE;
        }
        ScratchInput {
            positive: self.held & self.positive != 0,
            negative: self.held & self.negative != 0,
            analog: self.turn.pressed().map(|(d, until)| AnalogTurn {
                negative: d == AxisDirection::Backward,
                until,
            }),
        }
    }
}

/// A play state: its number, counting the run's from 1, and its origin on
/// the shared clock, stamped when the game thread created it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayState {
    /// Its number.
    pub number: u64,
    /// Its origin on the shared clock (QPC µs).
    pub origin: TimeUs,
}

/// The run's play states (docs/design.md "Play Lifecycle"): one per chart
/// chosen to play, P, and retry; a practice Start, a round, an automatic
/// round and a practice result keep it.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlayStates {
    current: Option<PlayState>,
}

impl PlayStates {
    /// A new play state, created at `origin` on the shared clock.
    pub const fn begin(&mut self, origin: TimeUs) -> PlayState {
        let number = match self.current {
            Some(s) => s.number.wrapping_add(1),
            None => 1,
        };
        let s = PlayState { number, origin };
        self.current = Some(s);
        s
    }

    /// The play state created last, if any.
    #[must_use]
    pub const fn current(&self) -> Option<PlayState> {
        self.current
    }
}

/// What a scratch note is to autoplay's presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScratchNoteKind {
    /// A tap.
    Tap,
    /// A long note's start, ending at `end` (chart µs), a charge note's
    /// (CN or HCN) when `charge`.
    LongStart {
        /// Its end.
        end: TimeUs,
        /// Whether it is a CN or HCN rather than a plain LN.
        charge: bool,
    },
    /// A long note's end, a charge note's (CN or HCN) when `charge`.
    LongEnd {
        /// Whether it is a CN or HCN rather than a plain LN.
        charge: bool,
    },
}

/// One scratch note event, at its chart time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScratchNote {
    /// Its chart time, µs.
    pub time: TimeUs,
    /// What it is.
    pub kind: ScratchNoteKind,
}

/// The scratch lane's taps and long notes of `chart` as autoplay's
/// presentation crosses them, in time order (a long note's start and end
/// apart, mines left out): a long note is a charge note (CN or HCN) when
/// the chart's mode `ln_mode` says so, a plain LN otherwise. Built before
/// READY: it allocates.
#[must_use]
pub fn scratch_notes(chart: &Chart, ln_mode: Option<LnMode>) -> Vec<ScratchNote> {
    let charge = matches!(ln_mode, Some(LnMode::Cn | LnMode::Hcn));
    let mut out = Vec::new();
    for note in chart.lanes.get(Lane::SCRATCH.index()).into_iter().flatten() {
        match note.kind {
            NoteKind::Tap => out.push(ScratchNote {
                time: note.time,
                kind: ScratchNoteKind::Tap,
            }),
            NoteKind::Long { end, .. } => {
                out.push(ScratchNote {
                    time: note.time,
                    kind: ScratchNoteKind::LongStart { end, charge },
                });
                out.push(ScratchNote {
                    time: end,
                    kind: ScratchNoteKind::LongEnd { charge },
                });
            }
            NoteKind::Mine { .. } => {}
        }
    }
    // Ends join the starts that follow them; a stable sort keeps a lane's
    // order at equal times (an end before the next start at its time).
    out.sort_by_key(|n| n.time);
    out
}

#[cfg(test)]
mod tests;
