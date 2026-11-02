//! The play adjustments' tap (design.md "Play Adjustments", order and
//! ownership): the adjustment controls' presses, releases, key repeats and
//! signed axis movement, observed beside the physical-state tracking and
//! sent through a bounded queue, while recording is open too. The physical
//! hold latch ([`HoldLatch`]) says whether a latched control (START or
//! SELECT) is held, from any thread.
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
    /// A batch ended while a tapped control was held.
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
    queue: rtrb::Producer<TapObservation>,
    latch: Arc<AtomicU64>,
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
            queue: producer,
            latch: Arc::new(AtomicU64::new(0)),
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

    /// Whether observations are sent: set at each boundary. The held
    /// controls are counted either way; turning it on sends them as the
    /// baseline ([`TapEvent::Resync`]) first.
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

    /// Forgets every holder, for a recount ([`hold`](Self::hold) each).
    pub(crate) fn clear_holders(&mut self) {
        self.holders = [0; MAX_TAPPED];
        self.store_latch();
    }

    /// Counts a holder of `n` without sending anything: a recount.
    pub(crate) fn hold(&mut self, n: u8) {
        if let Some(h) = self.holders.get_mut(usize::from(n)) {
            *h = h.saturating_add(1);
        }
        self.store_latch();
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

    /// The end of a batch received at `at`; `open` while the focus gate
    /// admits input. Sends what is pending once there is room, else a tick
    /// while a tapped control is held.
    pub(crate) fn end_batch(&mut self, at: TimeUs, open: bool) {
        if !self.active {
            return;
        }
        if !open {
            self.pending = Pending::LostResync;
            return;
        }
        let held = self.held_set();
        match self.pending {
            Pending::LostResync if self.queue.slots() >= 2 => {
                self.send(at, TapEvent::Lost);
                self.send(at, TapEvent::Resync(held));
                self.pending = Pending::None;
            }
            Pending::Resync if self.queue.slots() >= 1 => {
                self.send(at, TapEvent::Resync(held));
                self.pending = Pending::None;
            }
            Pending::None if held != 0 && at > self.last_tick => {
                self.last_tick = at;
                self.observe(at, TapEvent::Tick, true);
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
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        sent
    }

    fn store_latch(&self) {
        let held = self.held_set() & self.latched != 0;
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
