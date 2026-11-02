//! The play adjustments' tap (design.md "Play Adjustments", order and
//! ownership): the adjustment controls' presses, releases, key repeats and
//! signed axis movement, observed beside the physical-state tracking and
//! sent through a bounded queue, while recording is open too. The physical
//! hold latch ([`HoldLatch`]) says whether a latched control (START or
//! SELECT) is held, from any thread, and takes part in the start gate
//! ([`StartGate`]): the input thread cancels an armed READY candidate as it
//! accepts a hold, so an observation ordered before the commit blocks it.
//!
//! Each tapped control has a number below [`MAX_TAPPED`]; one number may
//! stand for the same control on several devices, which then count as one
//! held control while any of them holds it. An overflow, or input taken
//! while focus was away, sends [`TapEvent::Lost`] then
//! [`TapEvent::Resync`] with the controls held, once the queue has room
//! for both: nothing in between is sent, and no release is made up.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bms_model::TimeUs;
use eval::ControlId;

use crate::hid::{HidProduct, wrapped};
use crate::sources::DeviceKind;

/// Observations the queue holds.
pub const ADJUST_QUEUE: usize = 1024;
/// The most controls a tap observes, numbered below it.
pub const MAX_TAPPED: usize = 128;

/// A control the tap observes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapControl {
    /// A key, on every keyboard.
    Key(ControlId),
    /// A button or an axis of every controller of `product`.
    Controller {
        /// The controller model.
        product: HidProduct,
        /// The control.
        control: ControlId,
    },
}

/// What the tap observes: each control with its number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdjustmentBindings {
    /// The controls and their numbers, below [`MAX_TAPPED`].
    pub controls: Vec<(TapControl, u8)>,
    /// The numbers whose holding the latch reports (START and SELECT), a
    /// bit each.
    pub latched: u128,
    /// The values in a revolution of an absolute axis, for its movement
    /// the short way round; the declared one when `None`.
    pub wrap: Option<u32>,
}

/// Why [`adjustment_queue`] refused bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TapError {
    /// More controls than [`MAX_TAPPED`].
    #[error("{0} tapped controls exceed the limit of {MAX_TAPPED}")]
    TooMany(usize),
    /// A number at or above [`MAX_TAPPED`].
    #[error("tapped control number {0} is not below {MAX_TAPPED}")]
    Number(u8),
    /// One control listed twice.
    #[error("a tapped control is listed twice")]
    Duplicate,
}

/// One observation's content; controls are the tap's numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapEvent {
    /// The first holder pressed it.
    Press(u8),
    /// The last holder released it.
    Release(u8),
    /// A holding keyboard repeated it.
    Repeat(u8),
    /// An axis moved `delta` counts since its last report, signed as it
    /// moved, the short way round its revolution.
    Axis {
        /// The axis.
        control: u8,
        /// The signed movement.
        delta: i64,
    },
    /// A batch ended while a tapped control was held, or once after the
    /// last request so its group closes.
    Tick,
    /// Observations were lost, or focus was.
    Lost,
    /// The controls held, a bit each: after a loss, or as the baseline
    /// when the tap becomes active.
    Resync(u128),
}

/// An observation at a batch's receive time, on the input thread's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapObservation {
    /// The batch's receive time.
    pub at: TimeUs,
    /// The tap's running sequence, ordering observations at one time.
    pub seq: u64,
    /// What happened.
    pub event: TapEvent,
}

/// What the latch reads ([`HoldLatch::read`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hold {
    /// A latched control is held now.
    pub held: bool,
    /// Presses of a latched control so far, wrapping: a press the reader
    /// has not seen changes it even when released again since.
    pub presses: u64,
    /// A device's held state did not fit its table, so `held` may miss a
    /// control until that device is removed.
    pub unverified: bool,
}

/// The physical hold latch: whether START or SELECT is held, as the input
/// thread last saw it, readable from any thread.
#[derive(Clone, Debug)]
pub struct HoldLatch(Arc<AtomicU64>);

impl HoldLatch {
    /// The latch's state now.
    #[must_use]
    pub fn read(&self) -> Hold {
        let v = self.0.load(Ordering::Acquire);
        Hold {
            held: v & 1 != 0,
            unverified: v & 2 != 0,
            presses: v >> 2,
        }
    }
}

/// Where a READY candidate's start gate stands ([`StartGate::state`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatePhase {
    /// No candidate is armed.
    Idle,
    /// The candidate waits for its commit.
    Armed,
    /// The candidate committed: its transport started.
    Committed,
    /// A hold cancelled the candidate before its commit.
    Cancelled,
}

/// A start gate's generation and phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateState {
    /// The READY candidate's generation.
    pub generation: u32,
    /// Where it stands.
    pub phase: GatePhase,
}

/// The start gate's word (design.md "Play Lifecycle", the start gate),
/// shared by the input thread, the game thread and whatever starts the
/// transport: a candidate is armed by generation, and exactly one of its
/// commit and its cancellation wins, each a compare-and-swap from armed.
/// The input thread cancels as it accepts a hold
/// ([`AdjustmentTap::start_gate`]); the transport commits only its own
/// generation, so a stale authorization starts nothing. Nothing allocates
/// after [`new`](Self::new).
#[derive(Clone, Debug)]
pub struct StartGate(Arc<AtomicU64>);

impl Default for StartGate {
    fn default() -> Self {
        Self::new()
    }
}

impl StartGate {
    /// An idle gate.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    const fn word(generation: u32, phase: GatePhase) -> u64 {
        let code = match phase {
            GatePhase::Idle => 0,
            GatePhase::Armed => 1,
            GatePhase::Committed => 2,
            GatePhase::Cancelled => 3,
        };
        ((generation as u64) << 2) | code
    }

    fn decode(v: u64) -> GateState {
        GateState {
            generation: u32::try_from(v >> 2).unwrap_or(u32::MAX),
            phase: match v & 3 {
                0 => GatePhase::Idle,
                1 => GatePhase::Armed,
                2 => GatePhase::Committed,
                _ => GatePhase::Cancelled,
            },
        }
    }

    /// The gate now.
    #[must_use]
    pub fn state(&self) -> GateState {
        Self::decode(self.0.load(Ordering::Acquire))
    }

    /// Arms candidate `generation`, whatever stood before: the opening
    /// that verified both roles released.
    pub fn arm(&self, generation: u32) {
        self.0
            .store(Self::word(generation, GatePhase::Armed), Ordering::Release);
    }

    /// Commits candidate `generation`: `true` when it was armed and
    /// nothing cancelled it first. Another generation, a cancelled or an
    /// idle gate commits nothing.
    pub fn commit(&self, generation: u32) -> bool {
        self.0
            .compare_exchange(
                Self::word(generation, GatePhase::Armed),
                Self::word(generation, GatePhase::Committed),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Cancels the armed candidate, whichever it is: its generation when
    /// one was armed and not committed first.
    pub fn cancel(&self) -> Option<u32> {
        let v = self.0.load(Ordering::Acquire);
        let state = Self::decode(v);
        if state.phase != GatePhase::Armed {
            return None;
        }
        self.0
            .compare_exchange(
                v,
                Self::word(state.generation, GatePhase::Cancelled),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
            .then_some(state.generation)
    }

    /// No candidate: the attempt is over.
    pub fn disarm(&self) {
        self.0.store(0, Ordering::Release);
    }
}

/// What the tap must send before anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    Resync,
    LostResync,
}

/// The input thread's end of the tap. [`SourceSet`](crate::SourceSet)
/// feeds it ([`SourceSet::set_adjustment`](crate::SourceSet::set_adjustment)).
#[derive(Debug)]
pub struct AdjustmentTap {
    controls: Vec<(TapControl, u8)>,
    latched: u128,
    wrap: Option<u32>,
    /// Devices holding each number.
    holders: [u16; MAX_TAPPED],
    presses: u64,
    unverified: bool,
    active: bool,
    pending: Pending,
    seq: u64,
    last_tick: TimeUs,
    /// When the last request (a press, release, repeat or axis movement)
    /// was sent, while no later observation has closed the reducer's group
    /// at that time: `end_batch` owes a tick until one does.
    unclosed: Option<TimeUs>,
    queue: rtrb::Producer<TapObservation>,
    latch: Arc<AtomicU64>,
    /// The READY candidate's gate, cancelled as a hold is accepted.
    gate: StartGate,
    dropped: Arc<AtomicU64>,
}

/// The logic thread's end of the tap.
#[derive(Debug)]
pub struct AdjustmentReceiver {
    queue: rtrb::Consumer<TapObservation>,
    dropped: Arc<AtomicU64>,
}

/// A tap for `bindings` and its receiver; the queue holds
/// [`ADJUST_QUEUE`] observations. It allocates; nothing does after it.
///
/// # Errors
///
/// [`TapError`] for bindings beyond the limits or listing a control twice.
pub fn adjustment_queue(
    bindings: AdjustmentBindings,
) -> Result<(AdjustmentTap, AdjustmentReceiver), TapError> {
    let controls = bindings.controls;
    if controls.len() > MAX_TAPPED {
        return Err(TapError::TooMany(controls.len()));
    }
    if let Some(&(_, n)) = controls.iter().find(|(_, n)| usize::from(*n) >= MAX_TAPPED) {
        return Err(TapError::Number(n));
    }
    for (i, (c, _)) in controls.iter().enumerate() {
        if controls.iter().skip(i + 1).any(|(d, _)| d == c) {
            return Err(TapError::Duplicate);
        }
    }
    let (producer, consumer) = rtrb::RingBuffer::new(ADJUST_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    Ok((
        AdjustmentTap {
            controls,
            latched: bindings.latched,
            wrap: bindings.wrap,
            holders: [0; MAX_TAPPED],
            presses: 0,
            unverified: false,
            active: false,
            pending: Pending::None,
            seq: 0,
            last_tick: TimeUs(i64::MIN),
            unclosed: None,
            queue: producer,
            latch: Arc::new(AtomicU64::new(0)),
            gate: StartGate::new(),
            dropped: Arc::clone(&dropped),
        },
        AdjustmentReceiver {
            queue: consumer,
            dropped,
        },
    ))
}

impl AdjustmentTap {
    /// A handle on the physical hold latch.
    #[must_use]
    pub fn latch(&self) -> HoldLatch {
        HoldLatch(Arc::clone(&self.latch))
    }

    /// A handle on the start gate this tap cancels: the latch's hold, or
    /// its state turning unverified, cancels the armed candidate as the
    /// input thread accepts it.
    #[must_use]
    pub fn start_gate(&self) -> StartGate {
        self.gate.clone()
    }

    /// The start gate, for the source set to arm at an opening.
    pub(crate) const fn gate(&self) -> &StartGate {
        &self.gate
    }

    /// Whether START or SELECT is held, or may be: what defers an opening.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.unverified || self.held_set() & self.latched != 0
    }

    /// Whether observations are sent: set at each boundary. The held
    /// controls are counted either way; turning it on sends them as the
    /// baseline ([`TapEvent::Resync`]) at the start of the next batch,
    /// before its packets.
    pub fn set_active(&mut self, active: bool) {
        if active && !self.active {
            self.pending = Pending::Resync;
        }
        self.active = active;
    }

    /// The number of `control` of a device of `kind` and `product`.
    pub(crate) fn number(
        &self,
        kind: DeviceKind,
        product: Option<HidProduct>,
        control: ControlId,
    ) -> Option<u8> {
        let wanted = match (kind, product) {
            (DeviceKind::Keyboard, _) => TapControl::Key(control),
            (DeviceKind::Controller, Some(product)) => TapControl::Controller { product, control },
            (DeviceKind::Controller, None) => return None,
        };
        self.controls
            .iter()
            .find(|(c, _)| *c == wanted)
            .map(|&(_, n)| n)
    }

    /// Counts the holders again from `held`, each control a device whose
    /// input reaches the tap holds, without sending them as presses or
    /// releases; the latch is unverified while `unverified` holds. While
    /// active, a held set that changes, or `owners_changed` (the devices
    /// that reach the tap or their mappings changed), is sent as a new
    /// baseline.
    pub(crate) fn recount(
        &mut self,
        held: impl Iterator<Item = (DeviceKind, Option<HidProduct>, ControlId)>,
        unverified: bool,
        owners_changed: bool,
    ) {
        let before = self.held_set();
        self.holders = [0; MAX_TAPPED];
        for (kind, product, control) in held {
            if let Some(h) = self
                .number(kind, product, control)
                .and_then(|n| self.holders.get_mut(usize::from(n)))
            {
                *h = h.saturating_add(1);
            }
        }
        if self.active
            && self.pending == Pending::None
            && (owners_changed || self.held_set() != before)
        {
            self.pending = Pending::Resync;
        }
        self.set_unverified(unverified);
    }

    /// A button or key `n` of one device went from `was` held to `now`
    /// held, in a batch received at `at`; `foreground` when it was
    /// admitted for recording.
    pub(crate) fn button(&mut self, n: u8, was: bool, now: bool, at: TimeUs, foreground: bool) {
        let latched = self.latched & (1_u128 << (n & 127)) != 0;
        let Some(h) = self.holders.get_mut(usize::from(n)) else {
            return;
        };
        let event = match (was, now) {
            (false, true) => {
                *h = h.saturating_add(1);
                if latched && *h == 1 {
                    self.presses = self.presses.wrapping_add(1);
                }
                (*h == 1).then_some(TapEvent::Press(n))
            }
            (true, false) => {
                *h = h.saturating_sub(1);
                (*h == 0).then_some(TapEvent::Release(n))
            }
            (true, true) => Some(TapEvent::Repeat(n)),
            (false, false) => None,
        };
        self.store_latch();
        if let Some(event) = event {
            self.observe(at, event, foreground);
        }
    }

    /// Axis `n` moved `raw` counts: a relative report (`declared` is
    /// `None`), or a position's difference, taken the short way round the
    /// configured revolution, else the declared one (`Some(0)`: none), as
    /// play and the menus count it.
    pub(crate) fn axis(
        &mut self,
        n: u8,
        raw: i64,
        declared: Option<u32>,
        at: TimeUs,
        foreground: bool,
    ) {
        let wrap = declared.map_or(0, |d| self.wrap.unwrap_or(d));
        let delta = wrapped(raw, wrap);
        if delta != 0 {
            self.observe(at, TapEvent::Axis { control: n, delta }, foreground);
        }
    }

    /// A device's held state did not fit: the latch is unverified while
    /// `unverified` holds.
    pub(crate) fn set_unverified(&mut self, unverified: bool) {
        self.unverified = unverified;
        self.store_latch();
    }

    /// Sends what is pending, stamped at `at`, once the queue has room;
    /// `open` while the focus gate admits input. Packets observed
    /// afterwards follow it.
    pub(crate) fn send_pending(&mut self, at: TimeUs, open: bool) {
        if self.active && open {
            self.flush(at);
        }
    }

    /// The end of a batch received at `at`; `open` while the focus gate
    /// admits input. Sends what is pending once there is room, else a tick
    /// while a tapped control is held, and one more at a later time once
    /// nothing is, so the reducer's last group closes.
    pub(crate) fn end_batch(&mut self, at: TimeUs, open: bool) {
        if !self.active {
            return;
        }
        if !open {
            self.pending = Pending::LostResync;
            return;
        }
        if self.pending != Pending::None {
            self.flush(at);
            return;
        }
        let owed = self.unclosed.is_some_and(|t| at > t);
        if at > self.last_tick && (owed || self.held_set() != 0) {
            self.last_tick = at;
            self.observe(at, TapEvent::Tick, true);
        }
    }

    /// Sends the pending loss or baseline once the queue has room for it.
    fn flush(&mut self, at: TimeUs) {
        match self.pending {
            Pending::LostResync if self.queue.slots() >= 2 => {
                self.send(at, TapEvent::Lost);
                self.send(at, TapEvent::Resync(self.held_set()));
                self.pending = Pending::None;
            }
            Pending::Resync if self.queue.slots() >= 1 => {
                self.send(at, TapEvent::Resync(self.held_set()));
                self.pending = Pending::None;
            }
            _ => {}
        }
    }

    fn held_set(&self) -> u128 {
        self.holders
            .iter()
            .zip(0_u32..)
            .filter(|&(h, _)| *h > 0)
            .fold(0, |set, (_, n)| set | (1_u128 << n))
    }

    /// Sends `event` unless something is pending; a background or
    /// rejected input, or a full queue, makes the loss pending.
    fn observe(&mut self, at: TimeUs, event: TapEvent, foreground: bool) {
        if !self.active {
            return;
        }
        if !foreground {
            self.pending = Pending::LostResync;
            return;
        }
        if self.pending != Pending::None {
            return;
        }
        if !self.send(at, event) {
            self.pending = Pending::LostResync;
        }
    }

    fn send(&mut self, at: TimeUs, event: TapEvent) -> bool {
        let seq = self.seq;
        let sent = self.queue.push(TapObservation { at, seq, event }).is_ok();
        if sent {
            self.seq = seq.wrapping_add(1);
            if matches!(
                event,
                TapEvent::Press(_)
                    | TapEvent::Release(_)
                    | TapEvent::Repeat(_)
                    | TapEvent::Axis { .. }
            ) {
                self.unclosed = Some(at);
            } else if self.unclosed.is_some_and(|t| at > t) {
                self.unclosed = None;
            }
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    fn store_latch(&self) {
        let held = self.held_set() & self.latched != 0;
        // Cancelled before the latch shows the hold: whoever reads the
        // hold finds the candidate cancelled already.
        if held || self.unverified {
            let _ = self.gate.cancel();
        }
        let v = (self.presses << 2) | (u64::from(self.unverified) << 1) | u64::from(held);
        self.latch.store(v, Ordering::Release);
    }
}

impl AdjustmentReceiver {
    /// Takes the oldest observation.
    pub fn pop(&mut self) -> Option<TapObservation> {
        self.queue.pop().ok()
    }

    /// Observations dropped so far because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
