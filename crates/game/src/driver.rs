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
//!   millisecond at most ([`InputPump::park`]), until the pump's awake
//!   deadline ends it. Every wait goes through that one loop, and its
//!   first failure is latched: the driver asks nothing more of the input
//!   thread and answers every later wait with that failure
//!   ([`AdjustDriver::failure`]).
//! - **Attempts**: [`enter_attempt`](AdjustDriver::enter_attempt) at an
//!   attempt's entry fences, closes the open group and forgets the chart;
//!   [`attach`](AdjustDriver::attach) gives it the attempt's tempo where
//!   the chart, speed and section are final, solves once and names a new
//!   presentation epoch, pending until the attempt's preparation makes it
//!   active. [`refresh_presentation`](AdjustDriver::refresh_presentation)
//!   names a new epoch for the same attempt without entering it again.
//! - **Acceptance**: [`close_acceptance`](AdjustDriver::close_acceptance)
//!   and [`reopen_acceptance`](AdjustDriver::reopen_acceptance) fence the
//!   input thread and put [`Event::Acceptance`] in the stream where the
//!   fence's answer is, so everything observed before the request is taken
//!   under the old acceptance and everything after under the new.
//!
//! Draining allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Speed, TimeUs};
use input::{AdjustmentReceiver, AdjustmentTap, Ingress, TapError, TapEvent, adjustment_queue};

use crate::adjust_keys::AdjustmentPlan;
use crate::adjustment::{
    Acceptance, Adjustment, AttachError, Augmented, Diagnostics, Event, Head, NoTempo, Observation,
    PresentationReader, PresentationSnapshot, PresentationWriter, presentation_link,
};
use crate::attempt::PreparationRefusal;
use crate::hispeed::PlayOptions;
use crate::live::InputWaitError;
use crate::session::Loaded;
use crate::tempo::AttemptTempo;
use crate::turntable::ScratchDevice;

/// What a drain runs under [`InputPump::with_ingress`]: given the ingress
/// word (none: no gate) and the meeting point between each validation and
/// its action.
pub type IngressDrain<'a> = dyn FnMut(Option<Ingress>, &dyn Fn()) + 'a;

/// The input thread, as the driver's waits need it.
pub trait InputPump {
    /// Asks the input thread to answer fence `n` at its next boundary.
    fn request_fence(&mut self, n: u64);
    /// Wakes the input thread: the tap waits for room the driver made.
    fn wake(&mut self);
    /// The display offset now, µs: what the drain cuts in when it changed.
    fn display_offset_us(&self) -> i64;
    /// Runs `f` with the ingress word now and the step lock its writers
    /// take held (design.md "Play Lifecycle", the decision screen), so a
    /// drain inside validates each observation's stamp and acts on it
    /// before the word can change; `f`'s second argument is called between
    /// each validation and its action (a test's meeting point). `None`
    /// (by default): no ingress gate, every observation acts.
    fn with_ingress(&mut self, f: &mut IngressDrain<'_>) {
        f(None, &|| {});
    }
    /// Waits a millisecond at most for the input thread to make progress:
    /// the only end of a driver's wait other than its answer.
    ///
    /// # Errors
    ///
    /// [`InputWaitError::Gone`] when the input thread is gone, and
    /// [`InputWaitError::Timeout`] once this pump's waits have lasted the
    /// input thread's bound in awake time.
    fn park(&mut self) -> Result<(), InputWaitError>;
}

/// The adjustment controls' game side: the tap's receiver, the plan, the
/// reducer, its publication and the attempt's tempo.
#[derive(Debug)]
pub struct AdjustDriver {
    receiver: AdjustmentReceiver,
    plan: AdjustmentPlan,
    reducer: Adjustment,
    /// The scratch's device state for the turntable, kept for the run.
    scratch: ScratchDevice,
    writer: PresentationWriter,
    tempo: Option<AttemptTempo>,
    /// Where the attempt opens, in chart time: the frozen head's base.
    opens_at: TimeUs,
    /// The display offset the stream has, µs.
    offset: i64,
    /// The last fence asked for, and the last answered.
    fence_asked: u64,
    fence_answered: u64,
    /// The acceptance to put in the stream at fence `n`'s answer.
    at_fence: Option<(u64, Acceptance)>,
    /// A provisional close's fence: a drain stops right after its answer.
    hold_after: Option<u64>,
    /// The observations after a provisional close's answer wait in the
    /// queue until [`commit_close`](Self::commit_close) or
    /// [`reverse_close`](Self::reverse_close).
    holding: bool,
    /// The speed and opening of the last attachment, while it stands and
    /// succeeded: set by a successful attachment, cleared by any other
    /// and by [`enter_attempt`](Self::enter_attempt).
    attached: Option<(Speed, TimeUs)>,
    /// An opening or a rearm was asked for and its marker not yet taken.
    opening: bool,
    /// The generation of the last cancellation taken.
    cancelled: Option<u32>,
    /// Whether the last observation validated was stamped with the
    /// current ingress word: the reducer is told when it flips.
    admitted: bool,
    /// The next presentation epoch, counting the run's attempts from 1.
    next_epoch: u64,
    /// The epoch attached for the coming attempt, not yet prepared.
    pending: Option<u64>,
    /// The epoch of the attempt prepared.
    active: Option<u64>,
    /// The first wait on the input thread that failed: terminal, readable
    /// from any thread ([`failure_cell`](Self::failure_cell)).
    failed: InputFailureCell,
}

/// The first wait of a driver on the input thread that failed, latched
/// once, shared by the driver and whoever watches it from another thread
/// or while the driver is lent out ([`AdjustDriver::failure_cell`]).
#[derive(Clone, Debug, Default)]
pub struct InputFailureCell(std::sync::Arc<std::sync::OnceLock<InputWaitError>>);

impl InputFailureCell {
    /// The failure latched, if one was.
    #[must_use]
    pub fn get(&self) -> Option<InputWaitError> {
        self.0.get().copied()
    }

    /// Latches `e` unless a failure was latched before.
    fn latch(&self, e: InputWaitError) {
        let _ = self.0.set(e);
    }
}

impl AdjustDriver {
    /// The tap for `plan`, to install on the input side, which publishes
    /// START and SELECT for the select screen's panels by `plan`'s roles
    /// ([`AdjustmentPlan::panel_roles`]), the driver with
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
        let [start, select] = plan.panel_roles();
        tap.set_panel_roles(start, select);
        tap.set_tuning_controls(plan.tuning_controls());
        let reducer = Adjustment::new(options, plan.roles, TimeUs(0), display_offset_us);
        let (writer, reader) = presentation_link(&reducer.snapshot());
        Ok((
            Self {
                receiver,
                plan: plan.clone(),
                reducer,
                scratch: ScratchDevice::new(plan),
                writer,
                tempo: None,
                opens_at: TimeUs(0),
                offset: display_offset_us,
                fence_asked: 0,
                fence_answered: 0,
                at_fence: None,
                hold_after: None,
                holding: false,
                attached: None,
                opening: false,
                cancelled: None,
                admitted: true,
                next_epoch: 1,
                pending: None,
                active: None,
                failed: InputFailureCell::default(),
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
        self.drain_gated(display_offset_us, None, &|| {})
    }

    /// [`drain`](Self::drain), each control's observation validated
    /// against the ingress `word` (none: no gate): one stamped with another
    /// generation, or under a closed word, is taken with presses discarded
    /// ([`Event::Ingress`]); a release always acts on the held set.
    /// `between` runs after each validation, before its action.
    fn drain_gated(
        &mut self,
        display_offset_us: i64,
        word: Option<Ingress>,
        between: &dyn Fn(),
    ) -> bool {
        if self.holding {
            return self.receiver.owed();
        }
        let stopped = std::cell::Cell::new(false);
        let Self {
            receiver,
            plan,
            reducer,
            scratch,
            writer,
            tempo,
            opens_at,
            offset,
            fence_answered,
            at_fence,
            hold_after,
            opening,
            cancelled,
            admitted,
            ..
        } = self;
        let hold_after = *hold_after;
        let cut = (display_offset_us != *offset).then(|| {
            *offset = display_offset_us;
            Augmented::Cut(display_offset_us)
        });
        let opens_at = *opens_at;
        let popped = std::iter::from_fn(|| if stopped.get() { None } else { receiver.pop() });
        let mut one = |o: input::TapObservation| -> Option<Augmented> {
            // Every movement of the scratch axis turns the turntable,
            // before the plan drops one without an adjustment quantum.
            scratch.observe(&o);
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
                    if hold_after == Some(n) {
                        stopped.set(true);
                    }
                    let (_, acceptance) = at_fence.take_if(|(m, _)| *m == n)?;
                    Some(Augmented::Obs(Observation {
                        time: o.at,
                        seq: o.seq,
                        event: Event::Acceptance(acceptance),
                    }))
                }
                _ => plan.observation(&o).map(Augmented::Obs),
            }
        };
        let observed = popped.flat_map(|o| {
            // A control's observation, except a release, is validated
            // against its stamp; the reducer hears when that flips.
            let gated = matches!(
                o.event,
                TapEvent::Press(_) | TapEvent::Repeat(_) | TapEvent::Axis { .. } | TapEvent::Tick
            );
            let flip = word.filter(|_| gated).and_then(|w| {
                let valid = w.admits(o.ingress);
                between();
                (valid != *admitted).then(|| {
                    *admitted = valid;
                    Augmented::Obs(Observation {
                        time: o.at,
                        seq: o.seq,
                        event: Event::Ingress(valid),
                    })
                })
            });
            [flip, one(o)].into_iter().flatten()
        });
        let items = cut.into_iter().chain(observed);
        match tempo {
            Some(t) => reducer.take(items, t),
            None => reducer.take(items, &NoTempo),
        }
        reducer.set_scratch(scratch.input());
        reducer.publish(writer);
        if stopped.get() {
            self.hold_after = None;
            self.holding = true;
        }
        self.receiver.owed()
    }

    /// One drain from `input`, waking it when the tap is owed room.
    pub fn pump(&mut self, input: &mut impl InputPump) {
        // Read before the step lock is taken: the pump's own read takes
        // nothing, and nothing inside may call back into it.
        let offset = input.display_offset_us();
        let mut owed = false;
        input.with_ingress(&mut |word, between| {
            owed = self.drain_gated(offset, word, between);
        });
        if owed {
            input.wake();
        }
    }

    /// Drains until `done` holds, parking between drains.
    fn wait_until(
        &mut self,
        input: &mut impl InputPump,
        done: impl Fn(&Self) -> bool,
    ) -> Result<(), InputWaitError> {
        if let Some(e) = self.failed.get() {
            return Err(e);
        }
        loop {
            self.pump(input);
            if done(self) {
                return Ok(());
            }
            // The pump's awake deadline is the wait's only end.
            if let Err(e) = input.park() {
                self.failed.latch(e);
                return Err(e);
            }
        }
    }

    /// The first wait on the input thread that failed, if one did: the run
    /// ends with it, and the driver asks nothing more of the input thread.
    #[must_use]
    pub fn failure(&self) -> Option<InputWaitError> {
        self.failed.get()
    }

    /// The driver's latched failure, as a handle read from anywhere.
    #[must_use]
    pub fn failure_cell(&self) -> InputFailureCell {
        self.failed.clone()
    }

    /// Fences the input thread: returns once everything it observed before
    /// the request was taken.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer: the run ends with it.
    pub fn fence(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        self.fence_with(None, input)
    }

    /// [`fence`](Self::fence), with `acceptance` put in the stream where
    /// the fence's answer is.
    fn fence_with(
        &mut self,
        acceptance: Option<Acceptance>,
        input: &mut impl InputPump,
    ) -> Result<(), InputWaitError> {
        if let Some(e) = self.failed.get() {
            return Err(e);
        }
        let n = self.fence_asked.wrapping_add(1);
        self.fence_asked = n;
        self.at_fence = acceptance.map(|a| (n, a));
        input.request_fence(n);
        let waited = self.wait_until(input, |d| d.fence_answered == n);
        self.at_fence = None;
        waited
    }

    /// Closes the play adjustments' acceptance where the fence's answer
    /// is: everything observed before it acts as before, and from it until
    /// a reopening a press of a tuning control acts on nothing and its
    /// hold stays suppressed, so the control acts again only on a fresh
    /// press after the reopening ([`Acceptance::Closed`]). Publishes the
    /// result.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence: the run ends with it.
    pub fn close_acceptance(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        self.fence_with(Some(Acceptance::Closed), input)
    }

    /// Closes the play adjustments' acceptance where the fence's answer is,
    /// provisionally: what was observed after the answer waits in the
    /// queue, taken under no acceptance yet, until the caller decides.
    /// [`commit_close`](Self::commit_close) keeps the close, and the wait
    /// is taken under it; [`reverse_close`](Self::reverse_close) reopens at
    /// the answer's stamp, and the wait is taken open, so nothing observed
    /// between the close and the decision is lost. Nothing drains until
    /// then. Publishes the result.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence: the run ends with it.
    pub fn close_provisionally(
        &mut self,
        input: &mut impl InputPump,
    ) -> Result<(), InputWaitError> {
        self.hold_after = Some(self.fence_asked.wrapping_add(1));
        let closed = self.fence_with(Some(Acceptance::Closed), input);
        if closed.is_err() {
            self.hold_after = None;
            self.holding = false;
        }
        closed
    }

    /// Keeps a provisional close ([`close_provisionally`](Self::close_provisionally)):
    /// what waited is taken closed. Publishes the result.
    pub fn commit_close(&mut self, input: &mut impl InputPump) {
        self.holding = false;
        self.pump(input);
    }

    /// Reverses a provisional close ([`close_provisionally`](Self::close_provisionally))
    /// at its answer's stamp: acceptance is open again before anything
    /// observed after the answer, which is then taken open. Publishes the
    /// result.
    pub fn reverse_close(&mut self, input: &mut impl InputPump) {
        self.holding = false;
        let (time, seq) = self.reducer.last().unwrap_or((TimeUs(i64::MIN), 0));
        let reopen = Observation {
            time,
            seq,
            event: Event::Acceptance(Acceptance::Open),
        };
        match &self.tempo {
            Some(t) => self.reducer.take([Augmented::Obs(reopen)], t),
            None => self.reducer.take([Augmented::Obs(reopen)], &NoTempo),
        }
        self.pump(input);
    }

    /// Reopens the play adjustments' acceptance where the fence's answer
    /// is. Closing and reopening at once drops nothing: the fence took
    /// everything observed before either. An opening's phase reopens it
    /// too. Publishes the result.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence: the run ends with it.
    pub fn reopen_acceptance(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        self.fence_with(Some(Acceptance::Open), input)
    }

    /// Whether the play adjustments take presses.
    #[must_use]
    pub const fn accepting(&self) -> bool {
        self.reducer.accepting()
    }

    /// How many adjustments the reducer applied so far
    /// ([`Adjustment::accepted`]): read after a fence, it moves with every
    /// change accepted before it.
    #[must_use]
    pub const fn accepted(&self) -> u64 {
        self.reducer.accepted()
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
        self.attached = None;
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
        let speed = tempo.speed();
        let references = tempo.references().ok();
        let attached = self.reducer.attach(opens_at, references, speed, &tempo);
        self.attached_as(attached, tempo, opens_at, input)
    }

    /// After the reducer attached `tempo` for `opens_at` with the result
    /// `attached`: keeps them, names a new epoch, publishes, and makes the
    /// epoch pending when the attachment succeeded.
    fn attached_as(
        &mut self,
        attached: Result<(), AttachError>,
        tempo: AttemptTempo,
        opens_at: TimeUs,
        input: &mut impl InputPump,
    ) -> Result<u64, Result<AttachError, InputWaitError>> {
        self.opens_at = opens_at;
        self.attached = attached.is_ok().then(|| (tempo.speed(), opens_at));
        self.tempo = Some(tempo);
        let epoch = self.next_epoch;
        self.next_epoch = epoch.wrapping_add(1);
        self.reducer.set_epoch(epoch);
        self.pump(input);
        attached.map_err(Ok)?;
        self.pending = Some(epoch);
        Ok(epoch)
    }

    /// The scene changed (a play's result, selection or the practice
    /// page shows): the input thread is fenced first, so everything it
    /// observed before the change is taken in the old scene's context;
    /// then an ownership change at the fence's stamp, so a control held
    /// across it (F1 or F2 repeating, a scratch held with START) takes no
    /// further action until released and pressed afresh. Publishes the
    /// result. An attempt's entry ([`enter_attempt`](Self::enter_attempt))
    /// is the other scene change, and fences and ends the context too.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence: the run ends with it.
    pub fn scene_changed(&mut self, input: &mut impl InputPump) -> Result<(), InputWaitError> {
        self.fence(input)?;
        let (time, seq) = self.reducer.last().unwrap_or((TimeUs(i64::MIN), 0));
        let ownership = Observation {
            time,
            seq,
            event: Event::Ownership,
        };
        match &self.tempo {
            Some(t) => self
                .reducer
                .feed([Augmented::Obs(ownership)], t, &mut self.writer),
            None => self
                .reducer
                .feed([Augmented::Obs(ownership)], &NoTempo, &mut self.writer),
        }
        Ok(())
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

    /// The starting tuning of READY candidate `generation`, once its
    /// opening's phase was reduced ([`Adjustment::opened`]); `None` before
    /// that, or for another candidate.
    #[must_use]
    pub fn opened(&self, generation: u32) -> Option<replay::StartTuning> {
        self.reducer
            .opened()
            .filter(|s| s.generation == generation)
            .map(PresentationSnapshot::start_tuning)
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
        refusal(
            loaded,
            self.attach(loaded.attempt_tempo(), loaded.opens_at(), input),
        )
    }

    /// The presentation takes the attempt `loaded` describes again, as it
    /// stands now (its speed and opening possibly refitted), without
    /// entering it: the holds, their repeat deadlines, the axes'
    /// remainders, the selected cover and the double-press history stay,
    /// and nothing ends the dispatch context.
    ///
    /// While an epoch is pending this does nothing. Otherwise it names a
    /// new pending epoch: on the standing attachment when the last one
    /// succeeded for the same speed and opening, else by solving again
    /// ([`Adjustment::resolve`]). `Ok(Some(_))` refuses the attempt as
    /// [`attach_loaded`](Self::attach_loaded) does, so a load-time
    /// attachment that failed is refused here.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence.
    pub fn refresh_presentation(
        &mut self,
        loaded: &Loaded,
        input: &mut impl InputPump,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        if self.has_pending() {
            return Ok(None);
        }
        self.fence(input)?;
        let tempo = loaded.attempt_tempo();
        let opens_at = loaded.opens_at();
        let speed = tempo.speed();
        let attached = if self.attached == Some((speed, opens_at)) {
            Ok(())
        } else {
            let references = tempo.references().ok();
            self.reducer.resolve(opens_at, references, speed, &tempo)
        };
        refusal(loaded, self.attached_as(attached, tempo, opens_at, input))
    }
}

/// What an attachment of the attempt `loaded` describes gave: `Ok(Some(_))`
/// refuses it, located at the chart, when Floating cannot solve for it.
fn refusal(
    loaded: &Loaded,
    attached: Result<u64, Result<AttachError, InputWaitError>>,
) -> Result<Option<PreparationRefusal>, InputWaitError> {
    match attached {
        Ok(_) => Ok(None),
        // Located at the chart's path: the reason is the chart's tempo.
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
