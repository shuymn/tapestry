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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
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
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
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

/// One revolution in the clock's units: 2 160 sixths of a degree, each
/// counted in thousandths (the rate per ms times µs).
const REVOLUTION: i64 = 2_160_000;

/// The turntable's angle on a play state's clock (docs/stage3.md, offset
/// 1), as the source's turn accumulates it (`KeyInputProccessor.java:
/// 92-108`): each tick after the first adds the time since the previous
/// one times the rate of the direction the frame reads, in sixths of a
/// degree per ms: −1 idle, +1 with Negative, −3 with Positive. Integers
/// only: µs times the rate, modulo a revolution.
///
/// One clock lives for the play state it was given
/// ([`adopt`](Self::adopt)): its first presented frame sets the baseline
/// and adds 0; a pause (a practice round's result) stops the ticks and
/// keeps the angle; a resume ticks again from a fresh baseline; another
/// play state starts it over. Allocation-free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurntableClock {
    /// The play state it belongs to.
    number: Option<u64>,
    /// The accumulated angle in the source's sense, in thousandths of a
    /// sixth of a degree, within one revolution.
    scratch: i64,
    /// The previous tick's time; `None` until the first, and after a
    /// pause or a resume.
    prev: Option<TimeUs>,
    paused: bool,
}

impl TurntableClock {
    /// Takes play state `number`: another number than its own starts it
    /// over (angle 0, no previous tick, running); its own changes nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn adopt(&mut self, number: u64) {
        if !matches!(self.number, Some(n) if n == number) {
            *self = Self {
                number: Some(number),
                scratch: 0,
                prev: None,
                paused: false,
            };
        }
    }

    /// The play state it belongs to.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn number(&self) -> Option<u64> {
        self.number
    }

    /// Play state `number`'s pause: the ticks stop and the previous one
    /// is forgotten; the angle stays. Another number's is ignored.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn pause(&mut self, number: u64) {
        if matches!(self.number, Some(n) if n == number) {
            self.paused = true;
            self.prev = None;
        }
    }

    /// Play state `number`'s resume: a paused clock ticks again, its next
    /// tick a fresh baseline adding 0; a running one runs on. Another
    /// number's is ignored.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn resume(&mut self, number: u64) {
        if matches!(self.number, Some(n) if n == number) && self.paused {
            self.paused = false;
            self.prev = None;
        }
    }

    /// The frame's tick at `now` on the frame clock with the direction
    /// `turn`: the first records the time and adds 0, each later one adds
    /// the time since the previous at `turn`'s rate. Nothing while paused
    /// or without a play state.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn tick(&mut self, now: TimeUs, turn: Turn) {
        if self.paused || self.number.is_none() {
            return;
        }
        if let Some(prev) = self.prev {
            let dt = now.0.saturating_sub(prev.0).max(0);
            let rate = match turn {
                Turn::Idle => -1,
                Turn::Negative => 1,
                Turn::Positive => -3,
            };
            let turned = i128::from(dt) * rate % i128::from(REVOLUTION);
            let turned = i64::try_from(turned).unwrap_or(0);
            self.scratch = (self.scratch + turned).rem_euclid(REVOLUTION);
        }
        self.prev = Some(now);
    }

    /// The angle in the source's sense, in sixths of a degree, whole
    /// ones (what the source's `scratch` holds): for comparisons.
    #[must_use]
    pub const fn source_sixths(&self) -> i64 {
        self.scratch / 1_000
    }

    /// The angle a turntable follower adds, clockwise on y-down, in
    /// sixths of a millidegree (`skin_model::TURNTABLE_UNITS_PER_DEGREE`),
    /// within one revolution: the source's angle negated, as every source
    /// angle is.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn angle(&self) -> i32 {
        // Within one revolution, so it fits.
        i32::try_from((REVOLUTION - self.scratch) % REVOLUTION).unwrap_or(0)
    }
}

/// How long autoplay's presentation keeps a press, ms
/// (`JudgeManager.java:107`, `auto_minduration`).
const AUTO_MIN_MS: i64 = 80;

/// Autoplay's scratch presses as the source's judge keeps them
/// (`JudgeManager.java:212-339, 562-576`), for the turntable: a press time
/// per direction, in whole ms of the play state, and the long note being
/// processed. Updated once per presented frame from the play's start with
/// the frame's chart time and state time; fixed size, allocation-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoplayScratch {
    /// The next note not yet crossed.
    cursor: usize,
    /// The previous update's chart time.
    previous: TimeUs,
    /// Positive's and Negative's press, in state ms.
    press: [Option<i64>; 2],
    /// The long note processing: its end, and whether it is a charge
    /// note.
    processing: Option<(TimeUs, bool)>,
}

impl Default for AutoplayScratch {
    fn default() -> Self {
        Self::NEW
    }
}

impl AutoplayScratch {
    /// Nothing crossed or pressed: the state at a play's start or a
    /// round's (`JudgeManager.init`).
    pub const NEW: Self = Self {
        cursor: 0,
        previous: TimeUs(0),
        press: [None; 2],
        processing: None,
    };

    /// Back to [`NEW`](Self::NEW), in place.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn reset(&mut self) {
        *self = Self::NEW;
    }

    /// One update at chart time `t` and state time `ms` over `notes`, in
    /// the source's order: every note crossed (after the previous update's
    /// chart time, at or before `t`), then each press older than 80 ms
    /// cleared while nothing processes, then a plain long note whose end
    /// is before `t` completed.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn update(&mut self, notes: &[ScratchNote], t: TimeUs, ms: i64) {
        let [positive, negative] = &mut self.press;
        while let Some(note) = notes.get(self.cursor) {
            if note.time > t {
                break;
            }
            self.cursor += 1;
            if note.time <= self.previous {
                continue;
            }
            match note.kind {
                ScratchNoteKind::Tap => *positive = Some(ms),
                ScratchNoteKind::LongStart { end, charge } => {
                    if self.processing.is_none() {
                        *positive = Some(ms);
                        self.processing = Some((end, charge));
                    }
                }
                ScratchNoteKind::LongEnd { charge: true } => {
                    *positive = None;
                    *negative = Some(ms);
                    self.processing = None;
                }
                ScratchNoteKind::LongEnd { charge: false } => {}
            }
        }
        if self.processing.is_none() {
            for p in &mut self.press {
                if p.is_some_and(|at| ms.saturating_sub(at) > AUTO_MIN_MS) {
                    *p = None;
                }
            }
        }
        if let Some((end, false)) = self.processing
            && end < t
        {
            self.processing = None;
        }
        self.previous = t;
    }

    /// Whether Positive and Negative are pressed.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn pressed(&self) -> [bool; 2] {
        let [p, n] = self.press;
        [p.is_some(), n.is_some()]
    }
}

/// A frame's state time: whole ms since the play state's origin, floored,
/// as the source's state timer counts them (`TimerManager.java:33-35`).
#[must_use]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub const fn state_ms(now: TimeUs, origin: TimeUs) -> i64 {
    now.0.saturating_sub(origin.0).div_euclid(1_000)
}

#[cfg(test)]
mod tests;
