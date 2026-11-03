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
//! held control while any of them holds it.
//!
//! Besides the observations, the tap sends markers of what the input
//! thread decides at its boundaries, in the order it decides them: a READY
//! candidate opened ([`TapEvent::Opened`]) or cancelled by a hold
//! ([`TapEvent::Cancelled`]), and the answer to a fence the game thread
//! asked for ([`TapEvent::Fence`]). A loss (a full queue, an observation
//! the tap could not send, focus going away or a focus loss counted
//! since) is a recovery: [`TapEvent::Lost`], then [`TapEvent::Resync`]
//! with the controls held when it is sent. Markers and recoveries wait in
//! a small FIFO (one recovery and one marker of each kind) that is flushed
//! in order, stamped as it is sent, before any later observation; while
//! anything waits there an observation is dropped, which is a loss, so
//! nothing is ever sent out of order and no release is made up. The queue
//! keeps [`RESERVED`] slots that only the FIFO takes. While the FIFO waits
//! for room the tap publishes that it is owed a wake
//! ([`AdjustmentReceiver::owed`]), checking for room again after
//! publishing, so a consumer that drains after the check wakes it.
//!
//! Every observation and marker is stamped on the presentation clock, the
//! process clock of the reducer's head; the caller maps each batch's time
//! onto it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bms_model::TimeUs;
use eval::ControlId;

use crate::hid::{HidProduct, wrapped};
use crate::sources::DeviceKind;

/// Observations the queue holds.
pub const ADJUST_QUEUE: usize = 1024;
/// The most controls a tap observes, numbered below it.
pub const MAX_TAPPED: usize = 128;
/// The queue slots only markers and recoveries take: a recovery's two
/// and one each for an opening, a cancellation and a fence.
pub const RESERVED: usize = 5;
/// The window's F1 and F2 events the window-key queue holds.
pub const WINDOW_KEYS: usize = 64;

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
    /// The numbers that repeat while held (the scratch's digital
    /// directions), a bit each: the tap is due a tick at each repeat.
    pub repeating: u128,
    /// The repeat period of `repeating`, µs; 0 for none.
    pub repeat_us: i64,
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
        /// The values in its revolution as the movement was counted; 0
        /// for a relative or non-wrapping axis.
        revolution: u32,
    },
    /// The presentation clock passed: a tapped control is held, a repeat
    /// is due, or the last requests' group closes.
    Tick,
    /// Observations were lost, or focus was.
    Lost,
    /// The controls held, a bit each: after a loss, as the baseline when
    /// the tap becomes active, or when the devices reaching it change.
    /// `verified` is false when a device's held state is unknown or focus
    /// is away: then the held set may miss a control.
    Resync {
        /// The controls held.
        held: u128,
        /// Whether `held` is known to be complete.
        verified: bool,
    },
    /// READY candidate `generation` opened, its chart time 0 at `origin`
    /// on the presentation clock.
    Opened {
        /// The candidate's generation.
        generation: u32,
        /// Its chart time 0.
        origin: TimeUs,
    },
    /// A hold cancelled READY candidate `generation`: sent after the batch
    /// whose observation caused it, so the requests at that batch's time
    /// are made under the candidate's clock.
    Cancelled {
        /// The candidate's generation.
        generation: u32,
    },
    /// The answer to fence `n`: everything observed before it was sent
    /// before it.
    Fence(u64),
}

/// An observation at a batch's receive time, on the presentation clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapObservation {
    /// The receive time, or for a marker the time it was sent.
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

    /// No candidate: the attempt is over. The last generation stays, so
    /// the next attempt numbers its candidates after it and no
    /// authorization or acknowledgement of an earlier one matches them.
    pub fn disarm(&self) {
        let generation = self.state().generation;
        self.0
            .store(Self::word(generation, GatePhase::Idle), Ordering::Release);
    }
}

/// An event of a window key: F1 or F2 as the window sees them in an
/// autoplay session, where no keyboard reaches the tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKeyEvent {
    /// The key went down.
    Press,
    /// The keyboard repeated it.
    Repeat,
    /// The key went up.
    Release,
}

#[derive(Debug, Default)]
struct WindowKeyShared {
    /// One word, so the input thread acknowledges an overflow against the
    /// very held keys it takes as the baseline: the keys held as the
    /// window last saw them (bits 0 and 1), whether the queue was full so
    /// the window enqueues nothing until the input thread acknowledges it
    /// ([`OVERFLOW`]), and a version counting every change (from bit 8).
    word: AtomicU64,
}

/// [`WindowKeyShared::word`]'s overflow bit.
const OVERFLOW: u64 = 1 << 2;

/// The most tries one acknowledgement makes before it is left for the next
/// wake: the input thread never spins on the window's word.
const ACK_TRIES: u32 = 4;

/// The held keys of a [`WindowKeyShared::word`].
const fn held_keys(word: u64) -> u8 {
    (word & 3) as u8
}

impl WindowKeyShared {
    /// The held keys now.
    fn held(&self) -> u8 {
        held_keys(self.word.load(Ordering::Acquire))
    }

    /// Acknowledges an overflow: clears it in one step with reading the
    /// held keys it returns as the baseline, so a change the window made
    /// meanwhile (and could not queue) is never lost: the step fails and
    /// is taken again with the newer keys. `between` runs between the read
    /// and the step (a test forces an interleaving there).
    fn acknowledge(&self, mut between: impl FnMut()) -> Option<u8> {
        for _ in 0..ACK_TRIES {
            let w = self.word.load(Ordering::Acquire);
            between();
            if self
                .word
                .compare_exchange(w, w & !OVERFLOW, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(held_keys(w));
            }
        }
        // The window keeps changing its keys: the overflow stays, and the
        // input thread tries again at its next wake.
        None
    }
}

/// The window's end of the window-key queue ([`window_keys`]): an ordered
/// bounded queue of F1 and F2 events for the input thread.
#[derive(Debug)]
pub struct WindowKeys {
    queue: rtrb::Producer<(u8, WindowKeyEvent)>,
    shared: Arc<WindowKeyShared>,
}

/// The input thread's end of the window-key queue, given to the tap
/// ([`AdjustmentTap::set_window_keys`]).
#[derive(Debug)]
pub struct WindowKeyReceiver {
    queue: rtrb::Consumer<(u8, WindowKeyEvent)>,
    shared: Arc<WindowKeyShared>,
}

/// A window-key queue of [`WINDOW_KEYS`] events. It allocates; nothing
/// does after it.
#[must_use]
pub fn window_keys() -> (WindowKeys, WindowKeyReceiver) {
    let (producer, consumer) = rtrb::RingBuffer::new(WINDOW_KEYS);
    let shared = Arc::new(WindowKeyShared::default());
    (
        WindowKeys {
            queue: producer,
            shared: Arc::clone(&shared),
        },
        WindowKeyReceiver {
            queue: consumer,
            shared,
        },
    )
}

impl WindowKeys {
    /// Key `key` (0 for F1, 1 for F2) had `event`. Its held state follows
    /// every event; only a real one (`synthetic` false: not one the window
    /// system makes up as focus changes) is queued, unless the queue
    /// overflowed and the input thread has not caught up yet, which then
    /// takes the held state as a baseline instead. `false` when it was not
    /// queued.
    pub fn key(&mut self, key: u8, event: WindowKeyEvent, synthetic: bool) -> bool {
        let bit = 1_u64 << (key & 1);
        let changed = |w: u64| {
            let held = match event {
                WindowKeyEvent::Press | WindowKeyEvent::Repeat => (w & 3) | bit,
                WindowKeyEvent::Release => (w & 3) & !bit,
            };
            Some(((w >> 8).wrapping_add(1) << 8) | (w & OVERFLOW) | held)
        };
        // Every change moves the version, even one the window cannot queue.
        let before = self
            .shared
            .word
            .try_update(Ordering::AcqRel, Ordering::Acquire, changed)
            .unwrap_or(0);
        if synthetic || before & OVERFLOW != 0 {
            return false;
        }
        if self.queue.push((key & 1, event)).is_ok() {
            return true;
        }
        self.shared.word.fetch_or(OVERFLOW, Ordering::AcqRel);
        false
    }
}

/// What the tap owes before any later observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// [`TapEvent::Lost`] when `lost`, then [`TapEvent::Resync`] with the
    /// held set when it is sent.
    Recovery {
        lost: bool,
    },
    Opened {
        generation: u32,
        origin: TimeUs,
    },
    Cancelled {
        generation: u32,
    },
    Fence(u64),
}

impl Marker {
    const fn slots(self) -> usize {
        match self {
            Self::Recovery { lost: true } => 2,
            _ => 1,
        }
    }

    const fn same_kind(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Recovery { .. }, Self::Recovery { .. })
                | (Self::Opened { .. }, Self::Opened { .. })
                | (Self::Cancelled { .. }, Self::Cancelled { .. })
                | (Self::Fence(_), Self::Fence(_))
        )
    }
}

/// The FIFO of what the tap owes: at most one recovery and one marker of
/// each kind, in the order they were decided.
#[derive(Clone, Copy, Debug, Default)]
struct Owed {
    entries: [Option<Marker>; 4],
}

impl Owed {
    fn is_empty(&self) -> bool {
        self.entries[0].is_none()
    }

    fn front(&self) -> Option<Marker> {
        self.entries[0]
    }

    fn pop(&mut self) {
        self.entries.rotate_left(1);
        if let Some(last) = self.entries.last_mut() {
            *last = None;
        }
    }

    /// Adds `m` behind what is owed. A recovery still unsent takes a later
    /// loss in; any other kind still unsent is replaced in its place (the
    /// game thread's credits keep that from happening).
    fn push(&mut self, m: Marker) {
        for slot in &mut self.entries {
            match slot {
                Some(Marker::Recovery { lost })
                    if m.same_kind(Marker::Recovery { lost: false }) =>
                {
                    if let Marker::Recovery { lost: more } = m {
                        *lost |= more;
                    }
                    return;
                }
                Some(e) if e.same_kind(m) => {
                    *e = m;
                    return;
                }
                Some(_) => {}
                None => {
                    *slot = Some(m);
                    return;
                }
            }
        }
    }
}

/// The input thread's end of the tap. [`SourceSet`](crate::SourceSet)
/// feeds it ([`SourceSet::set_adjustment`](crate::SourceSet::set_adjustment)).
#[derive(Debug)]
pub struct AdjustmentTap {
    controls: Vec<(TapControl, u8)>,
    latched: u128,
    wrap: Option<u32>,
    repeating: u128,
    repeat_us: i64,
    /// Devices holding each number.
    holders: [u16; MAX_TAPPED],
    /// When each number was last pressed, for its repeats.
    pressed_at: [TimeUs; MAX_TAPPED],
    presses: u64,
    unverified: bool,
    active: bool,
    /// The presentation focus as last sampled, and the focus losses
    /// counted then.
    focused: bool,
    losses: u64,
    owed: Owed,
    seq: u64,
    /// The latest stamp sent: markers sent later never go before it.
    last_at: TimeUs,
    last_tick: TimeUs,
    /// When the last request (a press, release, repeat or axis movement)
    /// was sent, while no later observation has closed the reducer's group
    /// at that time: the tap owes a tick until one does.
    unclosed: Option<TimeUs>,
    queue: rtrb::Producer<TapObservation>,
    latch: Arc<AtomicU64>,
    /// The READY candidate's gate, cancelled as a hold is accepted.
    gate: StartGate,
    dropped: Arc<AtomicU64>,
    owed_flag: Arc<AtomicBool>,
    /// F1 and F2 from the window, with their numbers, and the keys the
    /// tap counts held from them.
    window: Option<(WindowKeyReceiver, [Option<u8>; 2])>,
    window_held: u8,
    /// The window's keys acknowledged with an overflow, the baseline of
    /// the recovery it owes.
    window_baseline: Option<u8>,
    /// The candidate a hold of this batch cancelled: its marker follows the
    /// batch.
    cancelled: Option<u32>,
}

/// The logic thread's end of the tap.
#[derive(Debug)]
pub struct AdjustmentReceiver {
    queue: rtrb::Consumer<TapObservation>,
    dropped: Arc<AtomicU64>,
    owed: Arc<AtomicBool>,
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
    let owed = Arc::new(AtomicBool::new(false));
    Ok((
        AdjustmentTap {
            controls,
            latched: bindings.latched,
            wrap: bindings.wrap,
            repeating: bindings.repeating,
            repeat_us: bindings.repeat_us,
            holders: [0; MAX_TAPPED],
            pressed_at: [TimeUs(0); MAX_TAPPED],
            presses: 0,
            unverified: false,
            active: false,
            focused: true,
            losses: 0,
            owed: Owed::default(),
            seq: 0,
            last_at: TimeUs(i64::MIN),
            last_tick: TimeUs(i64::MIN),
            unclosed: None,
            queue: producer,
            latch: Arc::new(AtomicU64::new(0)),
            gate: StartGate::new(),
            dropped: Arc::clone(&dropped),
            owed_flag: Arc::clone(&owed),
            window: None,
            window_held: 0,
            window_baseline: None,
            cancelled: None,
        },
        AdjustmentReceiver {
            queue: consumer,
            dropped,
            owed,
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

    /// Whether observations are sent. The held controls are counted
    /// either way; turning it on sends them as the baseline
    /// ([`TapEvent::Resync`]) before anything later.
    pub fn set_active(&mut self, active: bool) {
        if active && !self.active {
            self.owed.push(Marker::Recovery { lost: false });
        }
        self.active = active;
    }

    /// Takes F1 and F2 from the window ([`window_keys`]) as the controls
    /// numbered `numbers` (F1's, F2's): an autoplay session's speed keys.
    pub fn set_window_keys(&mut self, keys: WindowKeyReceiver, numbers: [Option<u8>; 2]) {
        self.window = Some((keys, numbers));
    }

    /// The presentation focus sampled for the next observations:
    /// `focused`, and `losses` focus losses counted so far. Focus going
    /// away, or a loss counted since the last sample, is a loss: the
    /// recovery's held set is unverified while focus is away. Focus coming
    /// back sends a verified baseline; until then observations are not
    /// sent.
    pub fn set_focus(&mut self, focused: bool, losses: u64) {
        // Focus going away sends Lost and an unverified Resync at once, not
        // a Resync held back until focus returns (design B3), so a marker
        // behind the recovery never waits on focus; the return sends a
        // verified one. Under review.
        let lost = losses != self.losses || (self.focused && !focused);
        let back = focused && !self.focused;
        self.losses = losses;
        self.focused = focused;
        if lost || back {
            // What was derived from the focus before is resolved now: the
            // recovery's Lost or Resync closes the reducer's open group, so
            // no closing tick is owed, and its baseline is read from the
            // window's keys as they are when it is sent, not from an
            // overflow acknowledged before the change.
            self.unclosed = None;
            self.window_baseline = None;
        }
        if self.active && (lost || back) {
            self.owed.push(Marker::Recovery { lost });
        }
    }

    /// Whether focus was in at the last sample.
    #[must_use]
    pub const fn focused(&self) -> bool {
        self.focused
    }

    /// The focus losses counted at the last sample.
    #[must_use]
    pub const fn focus_losses(&self) -> u64 {
        self.losses
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
        if self.active && (owners_changed || self.held_set() != before) {
            self.owed.push(Marker::Recovery { lost: false });
        }
        self.set_unverified(unverified);
    }

    /// A button or key `n` of one device went from `was` held to `now`
    /// held, in a batch received at `at`; `foreground` unless it is
    /// background input. A hold this cancels a READY candidate with is
    /// followed by [`TapEvent::Cancelled`].
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
        if let (Some(TapEvent::Press(_)), Some(p)) =
            (event, self.pressed_at.get_mut(usize::from(n)))
        {
            *p = at;
        }
        if let Some(generation) = self.store_latch() {
            self.cancelled = Some(generation);
        }
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
        let revolution = declared.map_or(0, |d| self.wrap.unwrap_or(d));
        let delta = wrapped(raw, revolution);
        if delta != 0 {
            let event = TapEvent::Axis {
                control: n,
                delta,
                revolution,
            };
            self.observe(at, event, foreground);
        }
    }

    /// A device's held state did not fit: the latch is unverified while
    /// `unverified` holds.
    pub(crate) fn set_unverified(&mut self, unverified: bool) {
        self.unverified = unverified;
        let cancelled = self.store_latch();
        if let Some(generation) = cancelled
            && self.active
        {
            self.owed.push(Marker::Cancelled { generation });
        }
    }

    /// READY candidate `generation` opened at this boundary, its chart
    /// time 0 at `origin` on the presentation clock: sent behind
    /// everything observed before it, stamped `at` or later.
    pub fn opened(&mut self, generation: u32, origin: TimeUs, at: TimeUs) {
        if self.active {
            self.owed.push(Marker::Opened { generation, origin });
            self.flush(at);
        }
    }

    /// The answer to fence `n`, at this boundary, stamped `at` or later.
    pub fn fence(&mut self, n: u64, at: TimeUs) {
        if self.active {
            self.owed.push(Marker::Fence(n));
            self.flush(at);
        }
    }

    /// Sends what is owed, stamped `at` or later, as far as the queue has
    /// room.
    pub(crate) fn send_owed(&mut self, at: TimeUs) {
        if self.active {
            self.flush(at);
        }
    }

    /// The presentation clock reached `at`, whether or not a batch ran:
    /// the window's keys are taken, what is owed is sent, and a tick is
    /// sent while a tapped control is held, and once more at a later time
    /// once nothing is, so the reducer's last group closes.
    pub fn tick(&mut self, at: TimeUs) {
        if !self.active {
            return;
        }
        // A cancellation follows the batch whose hold caused it, not the
        // causing press itself (design B2 says after the press): SELECT is
        // a hold too, so a request made with it in one batch is still made
        // under the candidate's clock; the reducer closes the group at the
        // cancel without ending a held gesture's context, so a scratch held
        // with START goes on repeating.
        if let Some(generation) = self.cancelled.take() {
            self.owed.push(Marker::Cancelled { generation });
        }
        self.flush(at);
        self.window_keys(at);
        if !self.owed.is_empty() {
            return;
        }
        let closing = self.unclosed.is_some_and(|t| at > t);
        if at > self.last_tick && (closing || self.held_set() != 0) {
            // A tick at a repeat's deadline makes a request: like a press,
            // it owes the tick that closes its group.
            let repeats = self.next_repeat().is_some_and(|due| due <= at);
            self.last_tick = at;
            // Only a tick the reducer takes owes a closing one: a tick
            // dropped (focus away, a loss) made no request.
            if self.observe(at, TapEvent::Tick, true) && repeats {
                self.unclosed = Some(at);
            }
        }
    }

    /// When the tap is next due a tick on the presentation clock: a held
    /// repeating control's next repeat, or the tick closing the last
    /// requests' group. `None` while it owes something the queue has no
    /// room for: the consumer's drain wakes the thread then
    /// ([`AdjustmentReceiver::owed`]).
    #[must_use]
    pub fn next_due(&self) -> Option<TimeUs> {
        // Nothing is due while focus is away (no tick is taken then) or
        // while the tap owes a send the queue has no room for.
        if !self.active || !self.focused || !self.owed.is_empty() {
            return None;
        }
        let closing = self.unclosed.map(|t| TimeUs(t.0.saturating_add(1)));
        match (closing, self.next_repeat()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The first repeat of a held repeating control after the last tick.
    fn next_repeat(&self) -> Option<TimeUs> {
        if self.repeat_us <= 0 {
            return None;
        }
        let held = self.held_set() & self.repeating;
        let mut due: Option<TimeUs> = None;
        for (n, &pressed) in self.pressed_at.iter().enumerate() {
            if held & (1_u128 << (n & 127)) == 0 {
                continue;
            }
            let since = self.last_tick.0.saturating_sub(pressed.0).max(0);
            let k = since / self.repeat_us + 1;
            let at = TimeUs(pressed.0.saturating_add(k.saturating_mul(self.repeat_us)));
            due = Some(due.map_or(at, |d| d.min(at)));
        }
        due
    }

    /// The window's F1 and F2 events, taken in order once nothing is owed:
    /// after an overflow, the events queued before it, then a recovery
    /// whose baseline is the window's held keys, all suppressed.
    fn window_keys(&mut self, at: TimeUs) {
        if !self.owed.is_empty() {
            return;
        }
        let Some((keys, numbers)) = &mut self.window else {
            return;
        };
        let numbers = *numbers;
        let overflow = keys.shared.word.load(Ordering::Acquire) & OVERFLOW != 0;
        let mut events = [None; WINDOW_KEYS];
        let mut taken = 0;
        while let Some(slot) = events.get_mut(taken)
            && let Ok(e) = keys.queue.pop()
        {
            *slot = Some(e);
            taken += 1;
        }
        for (key, event) in events.into_iter().flatten() {
            let Some(n) = numbers.get(usize::from(key)).copied().flatten() else {
                continue;
            };
            let bit = 1_u8 << key;
            let tap = match event {
                WindowKeyEvent::Press if self.window_held & bit == 0 => {
                    self.window_held |= bit;
                    TapEvent::Press(n)
                }
                WindowKeyEvent::Release if self.window_held & bit != 0 => {
                    self.window_held &= !bit;
                    TapEvent::Release(n)
                }
                WindowKeyEvent::Repeat if self.window_held & bit != 0 => TapEvent::Repeat(n),
                _ => continue,
            };
            self.observe(at, tap, true);
        }
        if overflow
            && let Some((keys, _)) = &self.window
            && let Some(held) = keys.shared.acknowledge(|| {})
        {
            // What the window could not queue shows only in its held
            // keys: they are the baseline, read in one step with the
            // acknowledgement, and none of them acts.
            self.window_baseline = Some(held);
            self.owed.push(Marker::Recovery { lost: true });
            self.flush(at);
        }
    }

    /// Sends what is owed in order as far as the queue has room, then
    /// publishes whether anything is still owed, checking for room again
    /// after publishing it.
    fn flush(&mut self, at: TimeUs) {
        self.flush_racing(at, || {});
    }

    /// [`flush`](Self::flush), with `before_publish` run between the
    /// first attempt that found no room and the publication of `owed`: a
    /// test forces a consumer's drain there.
    fn flush_racing(&mut self, at: TimeUs, before_publish: impl FnOnce()) {
        self.send_owed_entries(at);
        if self.owed.is_empty() {
            self.owed_flag.store(false, Ordering::Release);
            return;
        }
        before_publish();
        self.owed_flag.store(true, Ordering::Release);
        self.send_owed_entries(at);
        if self.owed.is_empty() {
            self.owed_flag.store(false, Ordering::Release);
        }
    }

    fn send_owed_entries(&mut self, at: TimeUs) {
        while let Some(m) = self.owed.front() {
            if self.queue.slots() < m.slots() {
                return;
            }
            match m {
                Marker::Recovery { lost } => {
                    if lost {
                        self.send(at, TapEvent::Lost);
                    }
                    let verified = self.focused && !self.unverified;
                    // The window's keys: an overflow's acknowledged baseline,
                    // else as the window holds them now (a made-up release
                    // while focus was away shows only there), its queued
                    // events before it dropped, as the loss drops them.
                    if let Some((keys, _)) = &mut self.window {
                        self.window_held = match self.window_baseline.take() {
                            Some(held) => held,
                            None => {
                                while keys.queue.pop().is_ok() {}
                                keys.shared.held()
                            }
                        };
                    }
                    let held = self.held_set();
                    self.send(at, TapEvent::Resync { held, verified });
                }
                Marker::Opened { generation, origin } => {
                    self.send(at, TapEvent::Opened { generation, origin });
                }
                Marker::Cancelled { generation } => {
                    self.send(at, TapEvent::Cancelled { generation });
                }
                Marker::Fence(n) => {
                    self.send(at, TapEvent::Fence(n));
                }
            }
            self.owed.pop();
        }
    }

    fn held_set(&self) -> u128 {
        let devices = self
            .holders
            .iter()
            .zip(0_u32..)
            .filter(|&(h, _)| *h > 0)
            .fold(0, |set, (_, n)| set | (1_u128 << n));
        let window = self.window.as_ref().map_or(0, |(_, numbers)| {
            numbers
                .iter()
                .zip(0_u8..)
                .filter(|&(n, key)| n.is_some() && self.window_held & (1 << key) != 0)
                .fold(0, |set, (n, _)| set | n.map_or(0, |n| 1_u128 << (n & 127)))
        });
        devices | window
    }

    /// Sends `event` once nothing is owed; anything else is a loss: a
    /// background input, an observation behind something owed, or one the
    /// queue (short of its reserved slots) has no room for. Nothing is sent
    /// while focus is away; its return sends a baseline.
    fn observe(&mut self, at: TimeUs, event: TapEvent, foreground: bool) -> bool {
        if !self.active || !self.focused {
            return false;
        }
        self.flush(at);
        if !foreground || !self.owed.is_empty() || self.queue.slots() <= RESERVED {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.owed.push(Marker::Recovery { lost: true });
            self.flush(at);
            return false;
        }
        self.send(at, event);
        true
    }

    fn send(&mut self, at: TimeUs, event: TapEvent) {
        let at = at.max(self.last_at);
        let seq = self.seq;
        if self.queue.push(TapObservation { at, seq, event }).is_err() {
            // The reserved slots make this unreachable for what is owed,
            // and an observation is pushed only with room.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.last_at = at;
        self.seq = seq.wrapping_add(1);
        if matches!(
            event,
            TapEvent::Press(_) | TapEvent::Release(_) | TapEvent::Repeat(_) | TapEvent::Axis { .. }
        ) {
            self.unclosed = Some(at);
        } else if self.unclosed.is_some_and(|t| at > t) {
            self.unclosed = None;
        }
    }

    /// Stores the latch; the generation of the candidate a hold cancelled
    /// in doing so.
    fn store_latch(&self) -> Option<u32> {
        let held = self.held_set() & self.latched != 0;
        // Cancelled before the latch shows the hold: whoever reads the
        // hold finds the candidate cancelled already.
        let cancelled = if held || self.unverified {
            self.gate.cancel()
        } else {
            None
        };
        let v = (self.presses << 2) | (u64::from(self.unverified) << 1) | u64::from(held);
        self.latch.store(v, Ordering::Release);
        cancelled
    }
}

impl AdjustmentReceiver {
    /// Takes the oldest observation.
    pub fn pop(&mut self) -> Option<TapObservation> {
        self.queue.pop().ok()
    }

    /// Observations dropped so far: lost to a full queue or behind
    /// something owed.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Whether the tap waits for room to send what it owes: the consumer
    /// wakes the input thread after a drain that finds it set.
    #[must_use]
    pub fn owed(&self) -> bool {
        self.owed.load(Ordering::Acquire)
    }
}

/// The window's presentation focus (design.md "Play Adjustments"), read by
/// the input thread at every batch and boundary: whether the window has
/// focus now, and how many times it has lost it. Not the recording's focus
/// gate, which stays tripped until the next opening.
#[derive(Clone, Debug)]
pub struct PresentationFocus(Arc<FocusWords>);

#[derive(Debug)]
struct FocusWords {
    focused: AtomicBool,
    losses: AtomicU64,
}

impl Default for PresentationFocus {
    fn default() -> Self {
        Self::new()
    }
}

impl PresentationFocus {
    /// Focused, with no loss counted.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(FocusWords {
            focused: AtomicBool::new(true),
            losses: AtomicU64::new(0),
        }))
    }

    /// The window gained (`true`) or lost focus: a loss is counted, so one
    /// that came back before the next read is still seen.
    pub fn set(&self, focused: bool) {
        if !focused {
            self.0.losses.fetch_add(1, Ordering::AcqRel);
        }
        self.0.focused.store(focused, Ordering::Release);
    }

    /// Whether the window has focus, and the losses counted so far.
    #[must_use]
    pub fn read(&self) -> (bool, u64) {
        let losses = self.0.losses.load(Ordering::Acquire);
        (self.0.focused.load(Ordering::Acquire), losses)
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowKeyEvent, window_keys};

    /// An overflow's acknowledgement takes the held keys in one step with
    /// clearing the overflow: F2 released after the input thread read the
    /// keys, and dropped by the window since the overflow still stood, is
    /// in the baseline all the same.
    #[test]
    fn an_acknowledgement_keeps_a_release_made_while_it_reads() {
        let (mut keys, receiver) = window_keys();
        assert!(keys.key(1, WindowKeyEvent::Press, false));
        for _ in 1..super::WINDOW_KEYS {
            assert!(keys.key(1, WindowKeyEvent::Repeat, false));
        }
        assert!(!keys.key(1, WindowKeyEvent::Repeat, false), "overflowed");
        let mut released = false;
        let held = receiver.shared.acknowledge(|| {
            if !std::mem::replace(&mut released, true) {
                // Dropped: the overflow still stands.
                assert!(!keys.key(1, WindowKeyEvent::Release, false));
            }
        });
        assert_eq!(held, Some(0), "the release is in the baseline");
    }

    /// The acknowledgement is bounded: a window changing its keys between
    /// every read and step leaves the overflow standing, for the next wake,
    /// instead of holding the input thread; once the window is quiet it
    /// goes through.
    #[test]
    fn a_busy_window_leaves_the_acknowledgement_for_the_next_wake() {
        let (mut keys, receiver) = window_keys();
        for _ in 0..=super::WINDOW_KEYS {
            keys.key(1, WindowKeyEvent::Press, false);
        }
        let mut tries = 0;
        let held = receiver.shared.acknowledge(|| {
            tries += 1;
            // A dropped repeat moves the version every time.
            assert!(!keys.key(1, WindowKeyEvent::Repeat, false));
        });
        assert_eq!(held, None);
        assert_eq!(tries, super::ACK_TRIES);
        assert!(
            receiver
                .shared
                .word
                .load(std::sync::atomic::Ordering::Acquire)
                & super::OVERFLOW
                != 0
        );
        assert_eq!(receiver.shared.acknowledge(|| {}), Some(2));
    }
}

#[cfg(test)]
mod owed_tests {
    use bms_model::TimeUs;

    use super::{ADJUST_QUEUE, AdjustmentBindings, Marker, TapEvent, adjustment_queue};

    /// E2's interleaving: the tap finds no room, the consumer drains
    /// everything while `owed` still reads false, then the tap publishes
    /// `owed`. Its recheck finds the room the drain made and sends what it
    /// owes, so nothing waits on a wake no one will send.
    #[test]
    fn a_drain_before_owed_is_published_is_caught_by_the_recheck() {
        let (mut tap, mut rx) = adjustment_queue(AdjustmentBindings::default()).unwrap();
        tap.set_active(true);
        tap.tick(TimeUs(0));
        while rx.pop().is_some() {}
        // Fill the queue to the last slot.
        for k in 0..ADJUST_QUEUE as i64 {
            tap.send(TimeUs(k + 1), TapEvent::Tick);
        }
        tap.owed.push(Marker::Fence(1));
        let rx = std::cell::RefCell::new(rx);
        tap.flush_racing(TimeUs(5_000), || {
            let mut rx = rx.borrow_mut();
            assert!(!rx.owed(), "not published yet");
            while rx.pop().is_some() {}
        });
        let mut rx = rx.into_inner();
        assert!(!rx.owed(), "the recheck sent it");
        assert_eq!(rx.pop().map(|o| o.event), Some(TapEvent::Fence(1)));
    }
}
