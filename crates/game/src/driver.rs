//! The play adjustments' driver (design.md "Play Adjustments"): the game
//! thread's one writer of the presentation, shared by the scripted rig and
//! the player's. It drains the adjustment tap into the reducer, turns the
//! input thread's lifecycle markers into the reducer's phases, cuts each
//! display-offset change it reads into the stream at the drain that first
//! reads it, and publishes one snapshot per drain.
//!
//! - **Markers**: [`TapEvent::Opened`] is the head following the opened
//!   READY candidate's clock, [`TapEvent::Cancelled`] the head frozen at
//!   the attempt's opening again, and [`TapEvent::Fence`] the answer to a
//!   fence the driver asked for: everything the input thread observed
//!   before the request was taken before it.
//! - **Credits**: the game thread asks the input thread for an opening or
//!   a rearm only once it has taken the previous one's
//!   [`TapEvent::Opened`], a rearm only once it has taken the cancelled
//!   candidate's [`TapEvent::Cancelled`], and one fence at a time, so the
//!   tap never owes more than one marker of each kind.
//! - **Waits**: a fence or a credit is waited for in one loop that drains,
//!   wakes the input thread when the tap is owed room, and parks for a
//!   millisecond at most, up to [`WAIT_PARKS`] times.
//! - **Attempts**: [`enter_attempt`](AdjustDriver::enter_attempt) at an
//!   attempt's entry fences, closes the open group and forgets the chart;
//!   [`attach`](AdjustDriver::attach) gives it the attempt's tempo where
//!   the chart, speed and section are final, solves once and names a new
//!   presentation epoch, pending until the attempt's preparation makes it
//!   active.
//!
//! Draining allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::TimeUs;
use input::{AdjustmentReceiver, AdjustmentTap, TapError, TapEvent, adjustment_queue};

use crate::adjust_keys::AdjustmentPlan;
use crate::adjustment::{
    Adjustment, AttachError, Augmented, Diagnostics, Event, Head, NoTempo, Observation,
    PresentationReader, PresentationSnapshot, PresentationWriter, presentation_link,
};
use crate::attempt::PreparationRefusal;
use crate::hispeed::PlayOptions;
use crate::live::InputWaitError;
use crate::session::Loaded;
use crate::tempo::AttemptTempo;

/// The most parks a wait for a marker takes, a millisecond each at most:
/// the input thread's bound for an answer.
pub const WAIT_PARKS: u32 = 2_000;

/// The input thread, as the driver's waits need it.
pub trait InputPump {
    /// Asks the input thread to answer fence `n` at its next boundary.
    fn request_fence(&mut self, n: u64);
    /// Wakes the input thread: the tap waits for room the driver made.
    fn wake(&mut self);
    /// The display offset now, µs: what the drain cuts in when it changed.
    fn display_offset_us(&self) -> i64;
    /// Waits a millisecond at most for the input thread to make progress.
    ///
    /// # Errors
    ///
    /// [`InputWaitError::Gone`] when the input thread is gone.
    fn park(&mut self) -> Result<(), InputWaitError>;
}

/// The adjustment controls' game side: the tap's receiver, the plan, the
/// reducer, its publication and the attempt's tempo.
#[derive(Debug)]
pub struct AdjustDriver {
    receiver: AdjustmentReceiver,
    plan: AdjustmentPlan,
    reducer: Adjustment,
    writer: PresentationWriter,
    tempo: Option<AttemptTempo>,
    /// Where the attempt opens, in chart time: the frozen head's base.
    opens_at: TimeUs,
    /// The display offset the stream has, µs.
    offset: i64,
    /// The last fence asked for, and the last answered.
    fence_asked: u64,
    fence_answered: u64,
    /// An opening or a rearm was asked for and its marker not yet taken.
    opening: bool,
    /// The generation of the last cancellation taken.
    cancelled: Option<u32>,
    /// The next presentation epoch, counting the run's attempts from 1.
    next_epoch: u64,
    /// The epoch attached for the coming attempt, not yet prepared.
    pending: Option<u64>,
    /// The epoch of the attempt prepared.
    active: Option<u64>,
}

impl AdjustDriver {
    /// The tap for `plan`, to install on the input side, the driver with
    /// `options` and the display offset `display_offset_us`, and the
    /// reader of its snapshots, for the frames.
    ///
    /// # Errors
    ///
    /// What the tap refuses of `plan`'s bindings.
    pub fn new(
        plan: &AdjustmentPlan,
        options: PlayOptions,
        display_offset_us: i64,
    ) -> Result<(Self, AdjustmentTap, PresentationReader), TapError> {
        let (mut tap, receiver) = adjustment_queue(plan.bindings.clone())?;
        tap.set_active(true);
        let reducer = Adjustment::new(options, plan.roles, TimeUs(0), display_offset_us);
        let (writer, reader) = presentation_link(&reducer.snapshot());
        Ok((
            Self {
                receiver,
                plan: plan.clone(),
                reducer,
                writer,
                tempo: None,
                opens_at: TimeUs(0),
                offset: display_offset_us,
                fence_asked: 0,
                fence_answered: 0,
                opening: false,
                cancelled: None,
                next_epoch: 1,
                pending: None,
                active: None,
            },
            tap,
            reader,
        ))
    }

    /// Takes what the tap observed, the display offset `display_offset_us`
    /// cut in first when it changed, and publishes one snapshot. Returns
    /// whether the tap waits for room: the caller wakes the input thread
    /// then.
    #[must_use]
    pub fn drain(&mut self, display_offset_us: i64) -> bool {
        let Self {
            receiver,
            plan,
            reducer,
            writer,
            tempo,
            opens_at,
            offset,
            fence_answered,
            opening,
            cancelled,
            ..
        } = self;
        let cut = (display_offset_us != *offset).then(|| {
            *offset = display_offset_us;
            Augmented::Cut(display_offset_us)
        });
        let opens_at = *opens_at;
        let observed = std::iter::from_fn(|| receiver.pop()).filter_map(|o| {
            let phase = |head, generation| {
                Some(Augmented::Obs(Observation {
                    time: o.at,
                    seq: o.seq,
                    event: Event::Phase { head, generation },
                }))
            };
            match o.event {
                TapEvent::Opened { generation, origin } => {
                    *opening = false;
                    phase(
                        Head::Clock {
                            at: origin,
                            chart: TimeUs(0),
                        },
                        generation,
                    )
                }
                TapEvent::Cancelled { generation } => {
                    *cancelled = Some(generation);
                    phase(Head::Frozen { opens_at }, generation)
                }
                TapEvent::Fence(n) => {
                    *fence_answered = n;
                    None
                }
                _ => plan.observation(&o).map(Augmented::Obs),
            }
        });
        let items = cut.into_iter().chain(observed);
        match tempo {
            Some(t) => reducer.feed(items, t, writer),
            None => reducer.feed(items, &NoTempo, writer),
        }
        receiver.owed()
    }

    /// One drain from `input`, waking it when the tap is owed room.
    pub fn pump(&mut self, input: &mut impl InputPump) {
        if self.drain(input.display_offset_us()) {
            input.wake();
        }
    }

    /// Drains until `done` holds, parking between drains.
    fn wait_until(
        &mut self,
        input: &mut impl InputPump,
        done: impl Fn(&Self) -> bool,
    ) -> Result<(), InputWaitError> {
        for _ in 0..WAIT_PARKS {
            self.pump(input);
            if done(self) {
                return Ok(());
            }
            input.park()?;
        }
        Err(InputWaitError::Timeout)
    }

    /// Fences the input thread: returns once everything it observed before
    /// the request was taken.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer: the run ends with it.
    pub fn fence(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        let n = self.fence_asked.wrapping_add(1);
        self.fence_asked = n;
        input.request_fence(n);
        self.wait_until(input, |d| d.fence_answered == n)
    }

    /// Before asking for an opening or a rearm: the previous one's marker
    /// is taken first, so the tap owes at most one.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer.
    pub fn before_opening(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        self.wait_until(input, |d| !d.opening)?;
        self.opening = true;
        Ok(())
    }

    /// The opening asked for was not made (deferred, held, refused): no
    /// marker is coming for it.
    pub const fn no_opening(&mut self) {
        self.opening = false;
    }

    /// Before rearming after candidate `generation` was cancelled: its
    /// cancellation's marker is taken first.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer.
    pub fn after_cancel(
        &mut self,
        generation: u32,
        input: &mut impl InputPump,
    ) -> Result<(), InputWaitError> {
        self.wait_until(input, |d| d.cancelled == Some(generation))
    }

    /// An attempt's entry (design.md "Play Adjustments", preparation):
    /// fences the input thread, closes the open group, carries the holds
    /// over suppressed, and forgets the chart until
    /// [`attach`](Self::attach), the head frozen at `opens_at` when known.
    /// Every epoch is invalidated.
    ///
    /// # Errors
    ///
    /// Why the fence failed: the run ends with it.
    pub fn enter_attempt(
        &mut self,
        opens_at: Option<TimeUs>,
        input: &mut impl InputPump,
    ) -> Result<(), InputWaitError> {
        self.fence(input)?;
        match &self.tempo {
            Some(t) => self.reducer.enter_attempt(opens_at, t),
            None => self.reducer.enter_attempt(opens_at, &NoTempo),
        }
        self.tempo = None;
        if let Some(opens_at) = opens_at {
            self.opens_at = opens_at;
        }
        self.invalidate();
        self.reducer.set_epoch(0);
        self.pump(input);
        Ok(())
    }

    /// The attempt's chart is final: fences the input thread, then gives
    /// the reducer `tempo` (its speed included) with the head frozen at
    /// `opens_at`, solves once, and names a new presentation epoch,
    /// pending until [`prepared`](Self::prepared). Publishes the result.
    ///
    /// # Errors
    ///
    /// `Err(Ok(refusal))` when Floating cannot solve for the chart (its
    /// references unavailable, or the solve does not fit): the attempt is
    /// refused. `Err(Err(_))` when the fence failed.
    pub fn attach(
        &mut self,
        tempo: AttemptTempo,
        opens_at: TimeUs,
        input: &mut impl InputPump,
    ) -> Result<u64, Result<AttachError, InputWaitError>> {
        self.fence(input).map_err(Err)?;
        self.opens_at = opens_at;
        let speed = tempo.speed();
        let references = tempo.references().ok();
        let attached = self.reducer.attach(opens_at, references, speed, &tempo);
        self.tempo = Some(tempo);
        let epoch = self.next_epoch;
        self.next_epoch = epoch.wrapping_add(1);
        self.reducer.set_epoch(epoch);
        self.pump(input);
        attached.map_err(Ok)?;
        self.pending = Some(epoch);
        Ok(epoch)
    }

    /// Whether an attempt is attached and not yet prepared.
    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending attempt is prepared: its epoch is the active one.
    pub const fn prepared(&mut self) -> Option<u64> {
        if let Some(e) = self.pending.take() {
            self.active = Some(e);
        }
        self.active
    }

    /// The epoch of the attempt prepared, if any.
    #[must_use]
    pub const fn active(&self) -> Option<u64> {
        self.active
    }

    /// Every exit before recording opens: no epoch is pending or active,
    /// so the next attempt attaches anew.
    pub const fn invalidate(&mut self) {
        self.pending = None;
        self.active = None;
    }

    /// The attempt's tempo, once attached.
    #[must_use]
    pub const fn tempo(&self) -> Option<&AttemptTempo> {
        self.tempo.as_ref()
    }

    /// The settings with every accepted change: what is saved.
    #[must_use]
    pub const fn options(&self) -> PlayOptions {
        self.reducer.options()
    }

    /// The latest state, as the next snapshot will publish it.
    #[must_use]
    pub fn snapshot(&self) -> PresentationSnapshot {
        self.reducer.snapshot()
    }

    /// What the reducer refused so far.
    #[must_use]
    pub const fn diagnostics(&self) -> Diagnostics {
        self.reducer.diagnostics()
    }

    /// Observations the tap dropped so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.receiver.dropped()
    }
}

impl AdjustDriver {
    /// The presentation takes the attempt `loaded` describes, unless one
    /// is attached for it already: entered, then attached
    /// ([`attach_loaded`](Self::attach_loaded)).
    ///
    /// # Errors
    ///
    /// As [`attach_loaded`](Self::attach_loaded).
    pub fn begin_presentation(
        &mut self,
        loaded: &Loaded,
        input: &mut impl InputPump,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        if self.has_pending() {
            return Ok(None);
        }
        self.enter_attempt(Some(loaded.opens_at()), input)?;
        self.attach_loaded(loaded, input)
    }

    /// Attaches the attempt `loaded` describes, whose chart, speed and
    /// section are final ([`attach`](Self::attach)): `Ok(Some(_))` refuses
    /// it, located at the chart, when Floating cannot solve for it.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    pub fn attach_loaded(
        &mut self,
        loaded: &Loaded,
        input: &mut impl InputPump,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        match self.attach(loaded.attempt_tempo(), loaded.opens_at(), input) {
            Ok(_) => Ok(None),
            // Located at the chart's path: the reason is the chart's
            // tempo. Under review.
            Err(Ok(why)) => Ok(Some(PreparationRefusal {
                location: Some(loaded.path().display().to_string()),
                reason: format!(
                    "Floating hi-speed cannot be solved for this chart ({why}); \
                     play it in Classic (player.toml [hispeed] mode = \"classic\")"
                ),
            })),
            Err(Err(e)) => Err(e),
        }
    }
}
