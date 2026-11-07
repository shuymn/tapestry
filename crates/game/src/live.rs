//! The play lifecycle every driver shares (design.md "Play Lifecycle"): prepare
//! → open recording (READY starts) → play → evaluation complete → audio
//! tail → save → the result's preparation → result, with retry, quit,
//! focus loss, removal of the last device, suspend, an audio, screen or
//! input-thread failure, and a session policy failure ending the play from
//! any state.
//!
//! The world around the play is a [`Rig`]: the game's threads in the
//! player, a virtual clock and scripted input in [`crate::Session`] and in
//! tests. The rig owns the clock and every wait; the transitions here only
//! ask it what happened and tell it what to do.
//!
//! - Before recording opens, in this order: the screen's device is made
//!   usable, the play preset is refreshed, then audio and the play's
//!   attempt are prepared on the screen with that preset, the loading
//!   state ends, and a quit or a failure is looked for again at each step
//!   ([`ready_to_open`]); READY's origin is chosen only once that
//!   work is done. A preparation the screen refuses goes back to where the
//!   chart was chosen with the reason ([`LiveError::Preparation`]), and
//!   one the player cancels, or a quit before the opening, opens nothing;
//!   either gives the play's number back ([`Loaded::withdraw`]).
//!   The play's setup follows the opening, at READY's start: the
//!   plan, the reservation of the whole recording window and the logic
//!   step's construction, bounded by the chart and the reservation, then the
//!   handoff to audio and the screen (design.md "Recording Capacity", "Play
//!   Lifecycle"). The reserved memory is not touched in advance.
//! - A quit is latched by the rig and checked before recording opens and in
//!   every wait, so it is never lost to a transition.
//! - The quit key in a play's READY state, before its chart starts (chart
//!   time 0, a round's section start), leaves the chart without a result;
//!   from there until the play is complete it aborts the play into
//!   Closing, whose end shows the result, ended midway (in autoplay it
//!   aborts the play, whose result shows at once).
//! - Closing (design.md "Play Lifecycle", Closing): the displayed survival
//!   gauge's failure under `on_failure = "close"`, once the attempt is
//!   committed, or the quit key during play, freezes evaluation at the
//!   play's last finalized watermark, cuts the chart audio and seals
//!   recording ([`Rig::abort_close`]); the play then waits on its clock
//!   for [`closing_time`] from Closing's start ([`run_closing`]) before it
//!   is saved and its result shows. Every command taken from the
//!   admission to the end of Closing is Closing's ([`Rig::take_closing`]):
//!   a quit does nothing, a retry ends it and retries with no result, a
//!   suspend or a thread's or the screen's failure cuts it to the result,
//!   and closing the window cuts it and ends the run. A displayed failure
//!   before a gated attempt is seen committed is retained: scoring freezes
//!   ([`Logic::hold`]) until the gate settles, and the failure acts only
//!   on the commit, at its watermark.
//! - An aborted play closes recording and only then drains what is left in the
//!   source queues into its record, unevaluated, so input that arrived after
//!   the abort is kept (design.md "Play Lifecycle").
//! - A completed play enters one tail stage ([`TailStage`]), a round's end
//!   stage of which a full play's audio tail is the special case: a
//!   practice round ([`crate::practice`]) rings out and its stage ends when
//!   the audio side reports it done and the play's time has reached the
//!   ring-out's end; a full play seals its key sounds and its stage ends
//!   when the audio is done. The controls and failures act on every stage
//!   alike: once the play is complete, the quit key skips the rest of the
//!   stage to the result instead of aborting the play ([`TailAction`]). A
//!   repeated round whose stage ran to its end is followed by the next
//!   round at the following step, with no result between.
//! - Each step publishes whether the play is playing (design.md "Play
//!   Lifecycle"), as the step entered it: from the chart's start until a
//!   terminal transition, so READY is not, the step that processes
//!   a gauge failure or the completion still is, and the stop, the tail and
//!   the ring-out are not. It is the lifecycle's phase, not `!complete`.
//! - A practice round opens recording [`RESTORE_AHEAD`](crate::RESTORE_AHEAD)
//!   before its lead-in, has its audio restored at the lead-in's start
//!   ([`crate::restore`]), and is evaluated in its section.
//! - Opening and closing recording wait a bounded time for the input
//!   thread. One that is gone or does not answer ends the run with
//!   [`LiveError::Input`], after the play's record is saved: a failed close
//!   keeps what the play already took, marked incomplete (input failed).

use ::audio::OutputError;
use bms_model::TimeUs;
use eval::{InputStart, JudgeEvent};
use input::{Arrival, DeviceKind};
use replay::{CancelReason, Incomplete};

use crate::app::IDLE_WAIT;
use crate::attempt::{Preparation, PreparationRefusal};
use crate::audio_loop::AudioWaitError;
use crate::logic::{AbortCause, Advance, Logic, LogicError, SourceRead};
use crate::policy::SessionEvent;
use crate::practice::{FADE, RING_OUT};
use crate::render::RenderSide;
use crate::restore::RestoreError;
use crate::result_screen::ResultScreen;
use crate::session::{
    ClosingReport, Command, Loaded, MenuCommand, PlayEnd, PlayOutcome, PlayReport,
    RECORDING_MEMORY, Settings,
};
use crate::sound::{AudioQueue, audio_link_at};
use crate::{GateStorage, HistoryRecorder};

/// What opening recording gave.
#[derive(Debug)]
pub struct Opening<R> {
    /// Where the play's chart time 0 lies on the rig's clock.
    pub origin: TimeUs,
    /// The controls held at the opening.
    pub start: InputStart,
    /// The logic step's ends of the source queues, by source id.
    pub readers: Vec<R>,
    /// The participants' kinds, by source id.
    pub kinds: Vec<DeviceKind>,
    /// The axes each participant declares, by source id.
    pub axes: Vec<Vec<input::hid::AxisCap>>,
}

/// What closing recording gave. Once the rig returns it, nothing more is
/// pushed into the play's queues.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Closing {
    /// Devices that arrived while recording was open.
    pub arrivals: Vec<Arrival>,
    /// Whether a queue overflowed or a device's held state was lost.
    pub overflowed: bool,
    /// Whether a device did not take part because of the source limit.
    pub source_limited: bool,
    /// How many axis values each source reported outside the range their
    /// conversion uses, by source id
    /// ([`input::SourceSet::axis_out_of_range`]).
    pub axis_out_of_range: Vec<u64>,
}

/// Why the input thread did not open or close recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InputWaitError {
    /// The input thread is gone.
    #[error("the input thread is gone")]
    Gone,
    /// It did not answer within the rig's bound.
    #[error("the input thread did not answer")]
    Timeout,
}

/// The world a play runs in.
pub trait Rig {
    /// The source queues' read side.
    type Reader: SourceRead;
    /// Why the screen cannot draw a play.
    type ScreenError: std::error::Error + Send + Sync + 'static;
    /// A result's preparation in progress ([`begin_result`](Self::begin_result)):
    /// dropping it unfinished withdraws it.
    type ResultPrep: Default;

    /// The clock every play's origin is on.
    fn now(&self) -> TimeUs;
    /// Waits for the next step: production parks until the input thread
    /// publishes or a short timeout; a virtual rig moves its clock.
    fn wait(&mut self);
    /// Waits idly on a screen (the select screen, a result) until something
    /// happens, or until `deadline` on the rig's clock at the latest. By
    /// default, one [`wait`](Self::wait).
    fn idle_wait(&mut self, deadline: TimeUs) {
        let _ = deadline;
        self.wait();
    }
    /// After a step's decisions (`aborted`: the step ended the play).
    /// A virtual rig steps its audio through the step's own time here;
    /// threads need nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn stepped(&mut self, aborted: bool) {
        let _ = aborted;
    }
    /// What each logic step of a play measured (design.md "Latency Figures"):
    /// the step itself (its finalization delay), the events it delivered, and
    /// `delivered`, the play time read once the step had handed them to its
    /// sinks, so input to judgment covers the step's processing. Called after
    /// every step, before [`stepped`](Self::stepped).
    fn measured(&mut self, advance: &Advance, events: &[JudgeEvent], delivered: TimeUs) {
        let _ = (advance, events, delivered);
    }
    /// The play is over (evaluation and the audio tail ended, or it was
    /// aborted), before it is saved and re-evaluated: what measures the
    /// play stops here.
    fn play_over(&mut self) {}
    /// A pending retry or suspend (or quit, for rigs without a latch),
    /// taken.
    fn take(&mut self) -> Option<Command>;
    /// A pending command taken during Closing (design.md "Play
    /// Lifecycle", Closing), where a quit does nothing: a rig whose
    /// controls let a pending quit drop a pending retry keeps the retry
    /// here. By default, [`take`](Self::take).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take_closing(&mut self) -> Option<Command> {
        self.take()
    }
    /// The play skin's declared Closing duration, if it declares one
    /// ([`closing_time`] applies the floor). `None` by default.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn declared_close(&self) -> Option<TimeUs> {
        None
    }
    /// A pending result-screen command, taken. By default the in-play
    /// controls stand in for it: retry retries, quit goes back, and a
    /// suspend is ignored.
    fn take_menu(&mut self) -> Option<MenuCommand> {
        loop {
            match self.take()? {
                Command::Retry => return Some(MenuCommand::Retry),
                Command::Quit => return Some(MenuCommand::Back),
                Command::Suspend => {}
            }
        }
    }
    /// Whether the player quit or the window is closing: latched, never
    /// cleared.
    fn quitting(&self) -> bool;
    /// Makes sure the screen and its device can draw (built again if they
    /// failed) and waits for it to acknowledge: the device's recovery, for
    /// a play and for the menus alike. It prepares no attempt, which
    /// depends on the play preset ([`prepare_attempt`](Self::prepare_attempt)).
    ///
    /// # Errors
    ///
    /// Why the screen cannot draw, which ends the run.
    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError>;
    /// Prepares the screen for the attempt [`Loaded::attempt`] describes,
    /// fresh for every play, after the play preset was refreshed for it and
    /// before recording opens, and waits for it while servicing the
    /// player: a quit key or a close during the wait cancels it
    /// ([`Preparation::Cancelled`]) without waiting for the screen. A
    /// refusal leaves the screen usable for the menus. By default, for rigs
    /// without a screen, it is ready at once.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all, which ends the run.
    fn prepare_attempt(&mut self, loaded: &Loaded) -> Result<Preparation, Self::ScreenError> {
        let _ = loaded;
        Ok(Preparation::Ready)
    }
    /// Finalizes the attempt [`prepare_attempt`](Self::prepare_attempt)
    /// readied (design.md "Play Lifecycle"): what it prepared is installed
    /// for the play, after the attempt's readiness, a round's restore
    /// check and the look at a quit or a failure, and before the loading
    /// state ends; on every play, a retry and a practice round without a
    /// loading state included. Nothing to install by default.
    ///
    /// # Errors
    ///
    /// Why the screen did not install it, which ends the run: recording
    /// does not open.
    fn commit_attempt(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }
    /// Drops what [`prepare_attempt`](Self::prepare_attempt) readied and
    /// was not committed: the play was cancelled, refused or failed before
    /// its commit, so a preparation answered late never installs. Nothing
    /// by default.
    fn discard_attempt(&mut self) {}
    /// Ends a chart's loading state (design.md "Play Lifecycle"), after
    /// the audio's and the attempt's preparation and its commit, and
    /// before recording opens: once it returns, the screen draws nothing of it. Nothing to end without
    /// one (a retry); the default has none.
    ///
    /// # Errors
    ///
    /// Why the screen did not confirm it, which ends the run, as a failed
    /// preparation does: recording does not open.
    fn end_loading(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }
    /// Waits until audio is ready for a play: the previous play's switch
    /// taken. Before recording opens.
    ///
    /// # Errors
    ///
    /// Why audio cannot play, which ends the run.
    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        Ok(())
    }
    /// Opens recording at the next input boundary, with chart time 0
    /// `lead_in` after it.
    ///
    /// # Errors
    ///
    /// Why the input thread did not open it, which ends the run.
    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<Self::Reader>, InputWaitError>;
    /// Hands the play to audio and to the screen.
    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide);
    /// Whether the focus gate has tripped since recording opened.
    fn focus_lost(&self) -> bool;
    /// Whether a device that took part is still connected.
    fn connected(&self) -> bool;
    /// The audio output's failure, if it failed.
    fn audio_failure(&self) -> Option<OutputError>;
    /// Whether the screen failed during the play.
    fn screen_failed(&self) -> bool;
    /// Stops the play's sounds.
    fn cancel_audio(&mut self);
    /// A full play completed: its audio is done once it has ended, the key
    /// sounds of the tail's feedback included ([`audio_done`](Self::audio_done)).
    fn seal_audio(&mut self);
    /// A practice round completed: sealed, and its audio rings out
    /// (design.md "Mid-Chart Start and Section Repeat"): the BGM plays on
    /// until `fade_at`, the output fades to silence by `end_at`,
    /// both in the play's chart time, and every sound stops there.
    /// [`audio_done`](Self::audio_done) then turns true as for a sealed
    /// play, possibly before the play's time reaches `end_at`.
    fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs);
    /// Before a practice round's recording opens: checks that its audio can
    /// be restored at chart time `at`, the lead-in's start ([`crate::restore`]).
    /// By default, for rigs without audio, it can.
    ///
    /// # Errors
    ///
    /// Why it cannot (no restorer, a schedule other than the engine's): the
    /// round is refused before anything opens, with no start and no record.
    fn restore_ready(&mut self, at: TimeUs) -> Result<(), RestoreError> {
        let _ = at;
        Ok(())
    }
    /// After a practice round's recording opened with chart time 0 at
    /// `origin`, and before [`start`](Self::start): plans the restore of its
    /// audio to chart time `at`, the lead-in's start, which then lands with
    /// the start: the voices that playing the engine's schedule from its
    /// beginning leaves sounding there, each partway into its sound, at full
    /// gain. By default, for rigs without audio, nothing is restored.
    ///
    /// # Errors
    ///
    /// Why it cannot be planned: recording closes again, and the round
    /// reports no start and saves no record.
    fn restore_audio(&mut self, origin: TimeUs, at: TimeUs) -> Result<(), RestoreError> {
        let _ = (origin, at);
        Ok(())
    }
    /// Whether the sealed play's audio has ended, with every key sound
    /// sent to it so far: a report of its end made before the last key
    /// sound was sent does not count.
    fn audio_done(&self) -> bool;
    /// Closes recording and waits until nothing more is pushed.
    ///
    /// # Errors
    ///
    /// Why the input thread did not close it: the play is saved without
    /// the closing, and the run ends.
    fn close(&mut self) -> Result<Closing, InputWaitError>;
    /// A completed play's close (design.md "Play Lifecycle", the audio
    /// tail): recording closes as with [`close`](Self::close), at the
    /// next input boundary, and its closing is returned, but the play's
    /// sources stay, so input goes on reaching the play's queues, with no
    /// gap, for the tail's feedback, until
    /// [`end_feedback`](Self::end_feedback). Allocates nothing. By
    /// default, for rigs without feedback, it closes.
    ///
    /// # Errors
    ///
    /// As [`close`](Self::close).
    fn hand_off(&mut self) -> Result<Closing, InputWaitError> {
        self.close()
    }
    /// An aborted play's close that allocates nothing: recording is sealed
    /// at the next input boundary and its closing returned, as with
    /// [`hand_off`](Self::hand_off), but nothing more is pushed into the
    /// play's queues; the sources stay, and the input side's console lines
    /// for the play wait, until [`end_feedback`](Self::end_feedback). By
    /// default, for rigs without an input thread, it closes.
    ///
    /// # Errors
    ///
    /// As [`close`](Self::close).
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn abort_close(&mut self) -> Result<Closing, InputWaitError> {
        self.close()
    }
    /// The end of either handoff ([`hand_off`](Self::hand_off) after the
    /// tail, [`abort_close`](Self::abort_close) after Closing): the
    /// sources close, nothing more is pushed into the play's queues, and
    /// the input side's console lines for the play are printed. By
    /// default, nothing.
    ///
    /// # Errors
    ///
    /// Why the input thread did not close them: the run ends.
    fn end_feedback(&mut self) -> Result<(), InputWaitError> {
        Ok(())
    }
    /// The input thread's failure, once known: it is gone, or a handshake
    /// with it failed. Sticks once set.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn input_failure(&self) -> Option<InputWaitError> {
        None
    }
    /// The play ended: its report, before any end screen.
    fn ended(&mut self, report: &PlayReport);
    /// Begins the result's preparation (design.md "State Model",
    /// ResultPreparing), once the play's save is over and the commands
    /// pending then were taken as the play's: what the rig gives from here
    /// on was pressed after the save. By default, ready at once.
    fn begin_result(&mut self, report: &PlayReport, screen: &ResultScreen) -> Self::ResultPrep {
        let _ = (report, screen);
        Self::ResultPrep::default()
    }
    /// Advances the result's preparation, waiting until `deadline` on the
    /// rig's clock at the latest while it is pending. Between polls the
    /// flow takes the player's commands; one that leaves or retries drops
    /// `prep` unfinished, the preparation's withdrawal. By default, ready.
    fn poll_result(&mut self, prep: &mut Self::ResultPrep, deadline: TimeUs) -> Polled {
        let _ = (prep, deadline);
        Polled::Ready
    }
    /// Shows the end screen of a play that was not retried or quit in its
    /// READY state, with its result screen's view; called again whenever the
    /// view changes or the screen was built again.
    fn show_end(&mut self, report: &PlayReport, screen: &ResultScreen);
    /// Before each play opens recording: settings changed since the previous
    /// play are written into `settings`, which the play is evaluated and
    /// recorded with. The input offset changes only here, so a change takes
    /// effect from the next play (design.md "Offset Changes and the
    /// Watermark").
    fn before_play(&mut self, settings: &mut Settings) {
        let _ = settings;
    }
    /// After the play preset was refreshed for a play and its practice
    /// fitted to it, before the attempt's preparation: the presentation
    /// takes the attempt (design.md "Play Adjustments", preparation): when
    /// nothing is attached for it yet, the attempt is entered and its chart
    /// attached, solving once. `Ok(Some(_))` refuses the attempt as a
    /// preparation does (a Floating hi-speed that cannot be solved for the
    /// chart). Nothing by default.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer the fence, which ends the run.
    fn begin_presentation(
        &mut self,
        loaded: &Loaded,
    ) -> Result<Option<PreparationRefusal>, InputWaitError> {
        let _ = loaded;
        Ok(None)
    }
    /// The play stopped before recording opened: what
    /// [`begin_presentation`](Self::begin_presentation) attached is no
    /// longer the coming attempt's. Nothing by default.
    fn end_presentation(&mut self) {}
    /// Whether this rig's plays run the start gate (design.md "Play
    /// Lifecycle", the start gate): each READY is a candidate that a hold
    /// of START or SELECT before its transport commits cancels, and the
    /// methods below take part. `false` by default: the first READY opened
    /// is the play.
    fn gated(&self) -> bool {
        false
    }
    /// Opens recording with READY candidate `generation` at the next
    /// input boundary where START and SELECT are verifiably released, the
    /// input side arming the start gate for it; `Ok(None)` when one is
    /// held there, or may be: the opening is deferred and nothing opens.
    /// Gated rigs only; by default, [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer, which ends the run.
    fn open_candidate(
        &mut self,
        lead_in: TimeUs,
        generation: u32,
    ) -> Result<Option<Opening<Self::Reader>>, InputWaitError> {
        let _ = generation;
        self.open(lead_in).map(Some)
    }
    /// The opening was deferred by a hold: an attempt with a loading
    /// lifetime resumes it with its clock (Preparing → Loading), one
    /// without waits in Preparing. Nothing by default.
    fn defer(&mut self) {}
    /// Whether START or SELECT is held as the input thread's latch last
    /// read: when to ask the input side again. Never what opens a
    /// candidate, which the input side decides at its boundary.
    fn holding(&self) -> bool {
        false
    }
    /// The start gate's state: whether the armed candidate committed or
    /// was cancelled. Gated rigs only.
    fn candidate(&self) -> input::GateState {
        input::GateState {
            generation: 0,
            phase: input::GatePhase::Idle,
        }
    }
    /// Settles candidate `generation` against an ending: cancels it on the
    /// start gate unless its transport already committed, in one atomic
    /// outcome; `true` when it committed, so the attempt was played.
    /// Gated rigs only; by default, whether the gate shows it committed.
    fn settle_candidate(&mut self, generation: u32) -> bool {
        self.candidate()
            == input::GateState {
                generation,
                phase: input::GatePhase::Committed,
            }
    }
    /// Candidate `generation` was cancelled: its key sounds stop, and its
    /// scheduled chart start and audio restore are dropped; returns once
    /// the audio side acknowledged it, so no authorization of it is left
    /// when the next candidate opens. Gated rigs only.
    fn cancel_candidate(&mut self, generation: u32) {
        let _ = generation;
        self.cancel_audio();
    }
    /// The cancelled candidate's queues, `readers`, drained of its last
    /// events into `history`, which seals it with its closing time on its
    /// own timeline and its arrivals; then the attempt's next candidate
    /// `generation` opens on the same source table with chart time 0
    /// `lead_in` after its opening and its held-input snapshot copied into
    /// `start`, without allocating ([`input::SourceSet::reopen_candidate`]).
    /// Gated rigs only; by default nothing opens.
    ///
    /// # Errors
    ///
    /// Why the input thread did not answer, which ends the run.
    fn rearm_candidate(
        &mut self,
        readers: &mut Vec<Self::Reader>,
        lead_in: TimeUs,
        generation: u32,
        history: &mut crate::HistoryRecorder,
        start: &mut InputStart,
    ) -> Result<Rearm, InputWaitError> {
        let _ = (readers, lead_in, generation, history, start);
        Ok(Rearm::Held)
    }
    /// Starts audio for the rearmed candidate `generation`, chart time 0
    /// at `origin`, with the queue and screen of the first candidate: its
    /// transport waits for the commit as the first's did. Gated rigs only.
    fn restart_candidate(&mut self, origin: TimeUs, generation: u32) {
        let _ = (origin, generation);
    }
    /// The held-input snapshot's reserved size on the input side: what a
    /// rearmed candidate's snapshot may hold. Gated rigs only.
    fn snapshot_capacity(&self) -> usize {
        0
    }
    /// The bytes the rig's exchange with its input side retains for the
    /// attempt's rearms, by their capacities, admitted with the recording
    /// ([`GateStorage::exchange`](crate::GateStorage)). Gated rigs only.
    fn gate_exchange(&self) -> usize {
        0
    }
}

/// Where a result's preparation stands ([`Rig::poll_result`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polled {
    /// It is ready: the result can show.
    Ready,
    /// It is still running.
    Pending,
}

/// What a request to rearm READY gave ([`Rig::rearm_candidate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rearm {
    /// The next candidate opened, its chart time 0 at `origin` on the
    /// rig's clock.
    Opened {
        /// Where its chart time 0 lies.
        origin: TimeUs,
    },
    /// START or SELECT is held at the input boundary: nothing changed.
    Held,
}

/// Why the run of plays stopped short.
#[derive(Debug, thiserror::Error)]
pub enum LiveError<S> {
    /// The logic step refused to start (for example a key config binding
    /// one control twice).
    #[error("the play cannot start")]
    Logic(#[source] LogicError),
    /// The screen cannot draw a play: the rig's [`Rig::ScreenError`].
    #[error("the screen cannot draw the play")]
    Screen(#[source] S),
    /// The screen refused to prepare the play's attempt; it can still
    /// draw the menus, and recording did not open.
    #[error("the play cannot be prepared")]
    Preparation(#[source] PreparationRefusal),
    /// Audio cannot play.
    #[error("audio cannot play")]
    Audio(#[source] AudioWaitError),
    /// The audio output failed before recording opened; recording did not
    /// open.
    #[error("the audio output failed")]
    Output(#[source] OutputError),
    /// The screen failed after the attempt was prepared and before
    /// recording opened; recording did not open.
    #[error("the screen failed before the play opened")]
    ScreenLost,
    /// The input thread is gone or did not answer.
    #[error("the input thread failed")]
    Input(#[source] InputWaitError),
    /// A practice round's audio cannot be restored; the round did not
    /// start.
    #[error("the round's audio cannot be restored")]
    Restore(#[source] RestoreError),
}

/// How the run ends when nothing else stopped it: with the input thread's
/// failure, if one is known.
pub(crate) fn run_over<R: Rig>(rig: &R) -> Result<(), LiveError<R::ScreenError>> {
    rig.input_failure()
        .map_or(Ok(()), |e| Err(LiveError::Input(e)))
}

/// The other threads' failures that end the run before recording opens:
/// the input thread's ([`run_over`]), then the audio output's.
pub(crate) fn threads_over<R: Rig>(rig: &R) -> Result<(), LiveError<R::ScreenError>> {
    run_over(rig)?;
    rig.audio_failure()
        .map_or(Ok(()), |e| Err(LiveError::Output(e)))
}

/// What a play's error reports: a refusal the player can go back from
/// (its preparation, its restore, its logic step) gives way to the input
/// thread's or the audio output's failure when one came meanwhile, so it
/// never hides the run's end; any other error is the run's end already.
fn refused<R: Rig>(rig: &R, error: LiveError<R::ScreenError>) -> LiveError<R::ScreenError> {
    match error {
        LiveError::Preparation(_) | LiveError::Restore(_) | LiveError::Logic(_) => {
            threads_over(rig).err().unwrap_or(error)
        }
        other => other,
    }
}

/// Whether recording may open as far as the threads go: none of them
/// failed ([`threads_over`]), and the screen prepared for the attempt is
/// still there. A screen that failed now has no prepared attempt, so the
/// run ends rather than opening a play nothing draws.
fn preflight<R: Rig>(rig: &R) -> Result<(), LiveError<R::ScreenError>> {
    threads_over(rig)?;
    if rig.screen_failed() {
        return Err(LiveError::ScreenLost);
    }
    Ok(())
}

/// How the plays of a chart were left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// The player left the chart: quit in a play's READY state, or back on
    /// its result.
    Back,
    /// The window is closing: the run ends.
    Quit,
}

/// What a command does once the play is complete, during its audio tail or
/// a round's ring-out (design.md "Audio tail"). Skipping to the result is
/// its own action, not a quit: the quit key is its only trigger for now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TailAction {
    /// The rest of the tail is skipped: its audio stops and the play's
    /// result shows.
    SkipToResult,
    /// The tail is cut with this cause, which decides what follows (a
    /// retry retries; anything else shows the result).
    Cut(AbortCause),
}

impl TailAction {
    /// What `c` does in the tail.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn of(c: Command) -> Self {
        match c {
            Command::Quit => Self::SkipToResult,
            Command::Retry | Command::Suspend => Self::Cut(c.cause()),
        }
    }
}

/// The shortest Closing (design.md "Play Lifecycle", Closing): the same
/// 1 s as the loading state's minimum.
pub const CLOSE_FLOOR: TimeUs = TimeUs(1_000_000);

/// How long Closing lasts for a skin that declares `declared`: at least
/// [`CLOSE_FLOOR`]; a missing, zero or negative declaration gives the
/// floor, and nothing caps it.
#[must_use]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub const fn closing_time(declared: Option<TimeUs>) -> TimeUs {
    match declared {
        Some(d) if d.0 > CLOSE_FLOOR.0 => d,
        _ => CLOSE_FLOOR,
    }
}

/// Where a Closing started at play time `start` with `declared` runs out:
/// [`closing_time`] after it, saturating.
#[must_use]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub const fn closing_deadline(start: TimeUs, declared: Option<TimeUs>) -> TimeUs {
    TimeUs(start.0.saturating_add(closing_time(declared).0))
}

/// How a play's Closing ended (design.md "Play Lifecycle", Closing). The
/// play's own cause never changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosingEnd {
    /// Its time ran out: the play is saved and its result shows.
    RanOut,
    /// A retry ended it: the play is saved and retried, with no result.
    Retry,
    /// This cut it short: closing the window (`Quit`), a suspend, or the
    /// input thread's, the audio output's or the screen's failure.
    Cut(AbortCause),
}

/// A play's Closing on its play clock: where it started and runs out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClosingStage {
    /// Where it started, the `failed` timer's start.
    start: TimeUs,
    /// Where it runs out.
    deadline: TimeUs,
}

impl ClosingStage {
    /// The Closing that starts at play time `start`, for a skin that
    /// declares `declared`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn begin(start: TimeUs, declared: Option<TimeUs>) -> Self {
        Self {
            start,
            deadline: closing_deadline(start, declared),
        }
    }

    /// Whether it has run out at play time `now`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn over(self, now: TimeUs) -> bool {
        now.0 >= self.deadline.0
    }
}

/// What Closing has latched: a retry or a suspend taken since its
/// admission, the play's last steps' suspend included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ClosingLatches {
    retry: bool,
    suspend: bool,
}

/// What admits a play to Closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admit {
    /// The displayed survival gauge failed under `on_failure = "close"`.
    Failure,
    /// The quit key during play.
    Quit,
}

/// How the plays of a chart are left: closing the window ends the run,
/// anything else leaves the chart; a known input-thread failure ends the
/// run with its cause.
fn leave<R: Rig>(rig: &R) -> Result<Leave, LiveError<R::ScreenError>> {
    run_over(rig)?;
    Ok(if rig.quitting() {
        Leave::Quit
    } else {
        Leave::Back
    })
}

/// Plays one chart until the player leaves it: every play's report goes
/// to [`Rig::ended`]; a play that was not retried, and not quit in its
/// READY state, has its result prepared ([`prepare_result`]), where a
/// fresh quit, back or retry withdraws it, then shows its result, which
/// waits for a [`MenuCommand`]. The
/// quit key before the chart starts ([`Loaded::starts_at`]) leaves the
/// chart ([`Leave::Back`]); from then until the play is complete it aborts
/// the play into Closing, whose end shows the result, ended midway (in
/// autoplay, at once); after it, it skips the tail to the result. Closing
/// the window ends the run ([`Leave::Quit`]).
///
/// # Errors
///
/// [`LiveError`] when a play cannot start; the plays before it are saved.
/// A run whose input thread failed ends with [`LiveError::Input`] at the
/// next quit, back or retry, after the play's report and result.
pub fn run_chart<R: Rig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Leave, LiveError<R::ScreenError>> {
    loop {
        let Some(Played {
            report,
            next,
            next_round,
            counted_down,
        }) = play_and_next(loaded, rig)?
        else {
            return leave(rig);
        };
        rig.ended(&report);
        if rig.quitting() || counted_down {
            return leave(rig);
        }
        if report.end == PlayEnd::Aborted(AbortCause::Retry)
            || report.tail_cut == Some(AbortCause::Retry)
            || report.closing.is_some_and(|c| c.end == ClosingEnd::Retry)
            || next == Some(Command::Retry)
        {
            continue;
        }
        // A tail stage that ran to its end and repeats (a repeated round's
        // ring-out) is followed by the next round, whose READY state begins at
        // the following step; any other end shows the result.
        if next_round {
            rig.wait();
            continue;
        }
        let mut screen =
            ResultScreen::new(&report, loaded.shelf(), loaded.chart(), RECORDING_MEMORY);
        match prepare_result(rig, &report, &screen)? {
            Prepared::Ready => {}
            Prepared::Retry => continue,
            Prepared::Leave => return leave(rig),
        }
        loaded.emit(&SessionEvent::Result {
            attempt: report.attempt,
        });
        rig.show_end(&report, &screen);
        loop {
            // The wait ends at the shelf's poll deadline at the latest, so
            // a wake just before it does not put the check off.
            let now = rig.now();
            let deadline = loaded.shelf().deadline().next(now);
            rig.idle_wait(TimeUs(now.0.saturating_add(IDLE_WAIT.0).min(deadline.0)));
            if rig.quitting() {
                return leave(rig);
            }
            // The user preset files are polled while the result shows, never
            // during a play; a reload is used by plays from the next one on,
            // and the view sees every change through the shelf's revision.
            let _ = loaded.poll_presets(rig.now());
            let mut dirty = screen.sync(&report, loaded.shelf(), loaded.chart());
            // Retry and back are taken before the screen is rebuilt, so a
            // known input failure ends the run without another handshake
            // with the screen; a retry's preparation rebuilds it anyway.
            // A preset switch changes only the result's view, never the
            // play preset (design.md "Play Lifecycle").
            // Commands queued behind the one that leaves the result belong
            // to that result and are dropped with it. Every switch queued
            // is taken in one wake.
            let mut retry = false;
            while let Some(c) = rig.take_menu() {
                match c {
                    MenuCommand::Retry => {
                        retry = true;
                        break;
                    }
                    MenuCommand::Back => {
                        while rig.take_menu().is_some() {}
                        return leave(rig);
                    }
                    MenuCommand::NextPreset => dirty |= screen.step(1, &report, loaded.chart()),
                    MenuCommand::PrevPreset => dirty |= screen.step(-1, &report, loaded.chart()),
                }
            }
            if retry {
                while rig.take_menu().is_some() {}
                break;
            }
            // The screen failed during the play, its tail or this wait: it
            // is built again, acknowledged, before the end screen shows
            // again; if it cannot be, the run ends with the reason.
            if rig.screen_failed() {
                rig.prepare_screen().map_err(LiveError::Screen)?;
                dirty = true;
            }
            if dirty {
                rig.show_end(&report, &screen);
            }
        }
    }
}

/// How a result's preparation ended ([`prepare_result`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prepared {
    /// It is ready: the result shows.
    Ready,
    /// A fresh retry withdrew it: the next attempt follows.
    Retry,
    /// A fresh quit or back, or a close, withdrew it: the chart is left.
    Leave,
}

/// ResultPreparing (design.md "State Model"): the play is saved and
/// counted, and the commands pending then were taken as the play's, so
/// every command taken here is fresh. The rig's preparation is polled in
/// short slices; between them a quit or back leaves and a retry retries,
/// either dropping the preparation unfinished, with the commands queued
/// behind it; a preset step is dropped, since the result opens on the
/// played preset; a close leaves. A command taken during the poll that
/// found it ready is still served before the result shows. A screen that
/// failed meanwhile is built again, acknowledged, between polls, as on the
/// result, so the result never shows on a failed screen.
///
/// # Errors
///
/// [`LiveError::Screen`] when the failed screen cannot be built again,
/// which ends the run.
fn prepare_result<R: Rig>(
    rig: &mut R,
    report: &PlayReport,
    screen: &ResultScreen,
) -> Result<Prepared, LiveError<R::ScreenError>> {
    let mut prep = rig.begin_result(report, screen);
    loop {
        let deadline = TimeUs(rig.now().0.saturating_add(IDLE_WAIT.0));
        let polled = rig.poll_result(&mut prep, deadline);
        if rig.quitting() {
            return Ok(Prepared::Leave);
        }
        while let Some(c) = rig.take_menu() {
            let withdrawn = match c {
                MenuCommand::Retry => Prepared::Retry,
                MenuCommand::Back => Prepared::Leave,
                MenuCommand::NextPreset | MenuCommand::PrevPreset => continue,
            };
            while rig.take_menu().is_some() {}
            return Ok(withdrawn);
        }
        if rig.screen_failed() {
            rig.prepare_screen().map_err(LiveError::Screen)?;
        }
        if polled == Polled::Ready {
            return Ok(Prepared::Ready);
        }
    }
}

/// One play, from preparing the screen to its report. `None` when the
/// player quit before recording opened.
///
/// # Errors
///
/// [`LiveError`] when the screen, audio, recording or the logic step cannot
/// start; recording is closed again.
pub fn play<R: Rig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Option<PlayReport>, LiveError<R::ScreenError>> {
    Ok(play_and_next(loaded, rig)?.map(|p| p.report))
}

/// The rest of a play's preparation, once its preset was refreshed
/// (design.md "Play Lifecycle"): audio, the attempt on the screen and a
/// practice round's restore check, then, after a look for a quit or a
/// failure, the attempt's commit and the end of the loading state, with a
/// quit or a failure looked for again after it. `false` when the player
/// cancelled or quit before recording opens: the commands that did are
/// taken, so they leave only this play. Whatever ends it before the
/// commit discards what the preparation readied.
///
/// # Errors
///
/// What [`play`] reports; a refused attempt is [`LiveError::Preparation`].
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn ready_to_open<R: Rig>(loaded: &Loaded, rig: &mut R) -> Result<bool, LiveError<R::ScreenError>> {
    let ready = ready_to_commit(loaded, rig);
    if !matches!(ready, Ok(true)) {
        rig.discard_attempt();
        return ready;
    }
    rig.commit_attempt().map_err(LiveError::Screen)?;
    rig.end_loading().map_err(LiveError::Screen)?;
    // And again once no loading frame can follow: the last look before
    // recording opens. A retry or suspend from before this play belongs to
    // no play.
    preflight(rig)?;
    Ok(!(rig.quitting() || rig.take() == Some(Command::Quit)))
}

/// [`ready_to_open`] up to the commit: `true` once the attempt is ready,
/// a round's restore checked and no quit or failure came meanwhile.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn ready_to_commit<R: Rig>(
    loaded: &Loaded,
    rig: &mut R,
) -> Result<bool, LiveError<R::ScreenError>> {
    rig.prepare_audio().map_err(LiveError::Audio)?;
    match rig.prepare_attempt(loaded).map_err(LiveError::Screen)? {
        Preparation::Ready => {}
        // The player left, or another thread failed: a failure is the
        // run's end, with its cause.
        Preparation::Cancelled => {
            threads_over(rig)?;
            while rig.take().is_some() {}
            return Ok(false);
        }
        Preparation::Refused(why) => return Err(LiveError::Preparation(why)),
    }
    // A round whose audio cannot be restored is refused before loading
    // ends, and before anything opens.
    if let Some(at) = loaded.restore_at() {
        rig.restore_ready(at).map_err(LiveError::Restore)?;
    }
    // What happened during the preparation, before its commit: a quit key
    // pressed while it ran (or before it, when it answered at once) leaves
    // the play here, before anything is installed. A retry or suspend from
    // before this play belongs to no play.
    preflight(rig)?;
    Ok(!(rig.quitting() || rig.take() == Some(Command::Quit)))
}

/// A play's report, and what follows it.
struct Played {
    report: PlayReport,
    /// The run's next step: the command that arrived as recording closed
    /// after an abort, or a retry still pending once a completed play was
    /// saved.
    next: Option<Command>,
    /// The play's tail stage ran to its end and is followed by the next
    /// round, with no result between.
    next_round: bool,
    /// The quit key ended the play before its chart started: the chart is
    /// left without a result.
    counted_down: bool,
}

/// [`play`], with the command that arrived as recording closed after an
/// abort: the run's next step (a completed play's tail takes it).
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn play_and_next<R: Rig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Option<Played>, LiveError<R::ScreenError>> {
    if rig.quitting() {
        return Ok(None);
    }
    // A failed input thread is known before another handshake could mask
    // or delay it.
    run_over(rig)?;
    // The device first, for the play and the menus alike; it prepares no
    // attempt.
    rig.prepare_screen().map_err(LiveError::Screen)?;
    rig.before_play(loaded.settings_mut());
    // The play preset is refreshed here, so the attempt is prepared with
    // it, whatever a result screen viewed.
    let start = loaded.begin_play();
    // A round's practice follows the refreshed preset, or the round is
    // refused before anything is prepared: one that would play a reset
    // value the player set does not open, the reset its reason.
    let ready = match loaded.refit_practice() {
        Ok(()) => match rig.begin_presentation(loaded) {
            Ok(None) => ready_to_open(loaded, rig),
            Ok(Some(refusal)) => Err(LiveError::Preparation(refusal)),
            Err(e) => Err(LiveError::Input(e)),
        },
        Err(unfit) => Err(LiveError::Preparation(PreparationRefusal {
            location: None,
            reason: unfit.to_string(),
        })),
    };
    match ready {
        Ok(true) => {}
        Ok(false) => {
            rig.end_presentation();
            loaded.withdraw(start);
            return Ok(None);
        }
        Err(e) => {
            rig.end_presentation();
            loaded.withdraw(start);
            return Err(refused(rig, e));
        }
    }
    // A full play's recording opens its READY state before chart time 0, a
    // practice round's `RESTORE_AHEAD` before its lead-in, whose start its
    // audio is restored at (design.md "Mid-Chart Start and Section
    // Repeat").
    let opens_at = loaded.opens_at();
    let restore_at = loaded.restore_at();
    let lead_in = TimeUs(opens_at.0.saturating_neg());
    // A gated rig opens READY candidates, numbered after the gate's last,
    // so no authorization or acknowledgement of an earlier attempt's
    // candidate matches one of this attempt; otherwise the first READY
    // opened is the play.
    let gated = rig.gated();
    let mut generation = rig.candidate().generation.wrapping_add(1).max(1);
    // A quit during the wait is latched and taken by the first step. A
    // failed opening opened nothing: its number goes back, and so does an
    // attempt left while its opening was deferred.
    let opened = if gated {
        open_gated(rig, lead_in, generation)
    } else {
        rig.open(lead_in).map(Some).map_err(LiveError::Input)
    };
    let opening = match opened {
        Ok(Some(opening)) => opening,
        Ok(None) => {
            rig.end_presentation();
            loaded.withdraw(start);
            return Ok(None);
        }
        Err(e) => {
            rig.end_presentation();
            loaded.withdraw(start);
            return Err(e);
        }
    };
    let mut origin = opening.origin;
    // The refusal is the error reported; a failed close adds nothing to it.
    // A gated play's own storage is admitted with its recording.
    let storage = gated.then(|| GateStorage {
        held: rig.snapshot_capacity(),
        exchange: rig.gate_exchange(),
    });
    let plan = match loaded.plan_gated(&opening.kinds, &opening.axes, storage) {
        Ok(plan) => plan,
        Err(e) => {
            let _ = rig.close();
            return Err(refused(rig, LiveError::Logic(e)));
        }
    };
    let (mut logic, screen) = match Logic::with_section(
        loaded.chart(),
        loaded.preset(),
        &plan.device,
        &opening.start,
        loaded.section(),
        opening.readers,
        plan.logic,
        Vec::new(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = rig.close();
            return Err(refused(rig, LiveError::Logic(e)));
        }
    };
    // A gated play's candidates: the history of cancelled openings and the
    // rearmed snapshot, reserved with the play's setup and reused by every
    // rearm.
    let mut gate = storage.map(|GateStorage { held, .. }| {
        logic.reserve_start(held);
        Candidates::new(held)
    });
    // A failure before the first step: a gated play's restore plan.
    let mut failed = None;
    // The restore is planned on the opening's origin, in the play's setup,
    // and lands with the start: reset → restore → lead-in → section
    // evaluation. A refusal closes recording as the logic step's does; a
    // gated play's ends without a played attempt.
    if let Some(at) = restore_at
        && let Err(e) = rig.restore_audio(origin, at)
    {
        if !gated {
            let _ = rig.close();
            return Err(refused(rig, LiveError::Restore(e)));
        }
        failed = Some(AbortCause::RestoreFailed);
    }
    // Admitted: only now does the play start, so a refused opening reports
    // no start.
    loaded.admitted(&start);
    let (mut sink, sounds) = audio_link_at(logic.max_events(), origin);
    // Autoplay's key sounds are on the engine's schedule, ahead of their
    // judgments.
    if loaded.config().autoplay == Some(crate::AutoKeySounds::Scheduled) {
        sink = sink.without_key_sounds();
    }
    logic.add_sink(Box::new(sink));
    let mut live = Vec::with_capacity(logic.max_events());
    rig.start(origin, sounds, screen);
    let round = restore_at.is_some();
    // The tail stage, once the play completed.
    let mut stage = None;
    let mut output_error = None;
    let mut input_error = None;
    // A suspend while recording is open marks the record incomplete, whichever
    // cause ends the play (design.md "Play Lifecycle").
    let mut suspended = false;
    // The play time of the last step: where an abort came.
    let mut last;
    // Whether the step's ending was a taken quit key, not the window's
    // close: only it admits Closing.
    let mut key_quit = false;
    // A displayed survival failure retained before a gated attempt is seen
    // committed: scoring freezes, and it acts on the commit.
    let mut pending = false;
    // The displayed survival failure stopped the committed play.
    let mut failed_closed = false;

    let starts_at = loaded.starts_at();
    let end = loop {
        rig.wait();
        let now = TimeUs(rig.now().0 - origin.0);
        // Anything seen in a step is handled before the step advances.
        // Every pending command is taken, so a suspend is never left behind
        // a quit or a retry.
        let mut command = None;
        while let Some(c) = rig.take() {
            suspended |= c == Command::Suspend;
            command = command.or(Some(c));
        }
        let ending = if rig.quitting() {
            Some(AbortCause::Quit)
        } else if let Some(c) = command {
            key_quit = c == Command::Quit;
            Some(c.cause())
        } else if let Some(e) = rig.input_failure() {
            // Before focus loss: the input thread publishes the focus gate
            // and the connections, which are stale once it is gone.
            input_error = Some(e);
            Some(AbortCause::InputFailed)
        } else if rig.focus_lost() {
            Some(AbortCause::FocusLoss)
        } else if let Some(e) = rig.audio_failure() {
            output_error = Some(e);
            Some(AbortCause::OutputFailed)
        } else if rig.screen_failed() {
            Some(AbortCause::ScreenFailed)
        } else {
            failed.take()
        };
        // Before its transport commits, a gated attempt's candidate is
        // cancelled by a hold and rearmed by a release; whatever ends the
        // attempt ends the loop here, and the gate settles whether it was
        // played once the loop is left (below).
        if let Some(g) = gate.as_ref().filter(|g| !g.committed)
            && let Some(cause) = ending
        {
            if !g.waiting {
                logic.abort(cause);
                rig.cancel_audio();
            }
            last = now;
            break PlayEnd::Aborted(cause);
        }
        if let Some(g) = gate.as_mut().filter(|g| !g.committed) {
            let step = if g.waiting {
                g.wait_release(rig, &mut logic, lead_in, &mut generation)
            } else {
                g.watch(rig, &mut logic, origin, generation)
            };
            match step {
                Step::Go => {}
                // A cancellation or a rearm discards a retained failure
                // with the candidate's evaluation.
                Step::Wait => {
                    pending = false;
                    continue;
                }
                Step::Rearmed(o) => {
                    pending = false;
                    origin = o;
                    // The cancelled candidate's events are discarded whole.
                    live.clear();
                    // A round's restore is planned again on the new origin;
                    // a plan that fails ends the attempt at the next step.
                    if let Some(at) = restore_at
                        && rig.restore_audio(origin, at).is_err()
                    {
                        failed = Some(AbortCause::RestoreFailed);
                    }
                    rig.restart_candidate(origin, generation);
                    continue;
                }
                Step::End(cause) => {
                    if let AbortCause::InputFailed = cause {
                        input_error = rig.input_failure().or(Some(InputWaitError::Gone));
                    }
                    last = now;
                    break PlayEnd::Aborted(cause);
                }
            }
        }
        // The step's phase as it enters it, before anything below ends the
        // play: playing from the chart's start on, until a terminal
        // transition. The step that processes one still enters playing. A
        // gated play plays only once its candidate committed.
        let committed = gate.as_ref().is_none_or(|g| g.committed);
        logic.set_playing(
            committed && now >= starts_at && !logic.is_complete() && logic.aborted().is_none(),
        );
        if let Some(cause) = ending {
            logic.abort(cause);
        }
        last = now;
        // A retained failure freezes scoring: the step takes the input
        // with its checks and advances nothing, and once the gate shows
        // the transport committed, the failure acts at its watermark,
        // after those checks and before any advance.
        if pending {
            let held = logic.hold();
            rig.stepped(held.aborted.is_some());
            if let Some(cause) = held.aborted {
                rig.cancel_audio();
                break PlayEnd::Aborted(cause);
            }
            if committed {
                logic.abort(AbortCause::GaugeFailed);
                rig.cancel_audio();
                failed_closed = true;
                break PlayEnd::Aborted(AbortCause::GaugeFailed);
            }
            continue;
        }
        let advance = logic.advance_to(now);
        live.extend_from_slice(logic.events());
        let delivered = TimeUs(rig.now().0 - origin.0);
        rig.measured(&advance, logic.events(), delivered);
        rig.stepped(advance.aborted.is_some());
        if let Some(cause) = advance.aborted {
            rig.cancel_audio();
            break PlayEnd::Aborted(cause);
        }
        if advance.complete {
            stage = Some(TailStage::begin(rig, loaded, round, now));
            break PlayEnd::Completed;
        }
        // An audio failure after a step that did not complete ends the play
        // there; a completed play stays complete and only loses its tail.
        if let Some(e) = rig.audio_failure() {
            output_error = Some(e);
            logic.abort(AbortCause::OutputFailed);
            rig.cancel_audio();
            break PlayEnd::Aborted(AbortCause::OutputFailed);
        }
        // The displayed survival failure closes a committed attempt here,
        // after the failing advance; before the commit it is retained.
        if loaded.closes_on(logic.gauges()) {
            if committed {
                logic.abort(AbortCause::GaugeFailed);
                rig.cancel_audio();
                failed_closed = true;
                break PlayEnd::Aborted(AbortCause::GaugeFailed);
            }
            pending = true;
        }
    };
    // Every way out of a gated attempt not yet seen committed, whatever
    // ended it (a command, a failure before or after the step's audio, the
    // evaluator, a completion), settles the start gate here, before
    // recording closes and disarms it, in one atomic outcome: a transport
    // that committed meanwhile makes the attempt played; otherwise the
    // candidate is cancelled and its audio acknowledged dropped.
    if let Some(g) = gate.as_mut().filter(|g| !g.committed) {
        if !g.waiting && rig.settle_candidate(generation) {
            g.committed = true;
        } else {
            rig.cancel_candidate(generation);
        }
    }
    // Admission to Closing, once the gate settled: a played attempt whose
    // displayed survival gauge failed, or the quit key during its play (at
    // or after the chart's start, outside autoplay). Closing the window
    // vetoes it for either cause.
    let played = gate.as_ref().is_none_or(|g| g.committed);
    let admit = if rig.quitting() || !played {
        None
    } else if failed_closed {
        Some(Admit::Failure)
    } else if end == PlayEnd::Aborted(AbortCause::Quit)
        && key_quit
        && loaded.config().autoplay.is_none()
        && last >= starts_at
    {
        Some(Admit::Quit)
    } else {
        None
    };
    // Stopped, completed or failed: no frame from here on is playing, the
    // tail's, the ring-out's and Closing's included. Closing starts at its
    // publication, on the play clock.
    let closing_at = admit.map(|_| TimeUs(rig.now().0 - origin.0));
    match closing_at {
        Some(at) => logic.enter_closing(at),
        None => logic.stop_playing(),
    }

    // Recording closes at the end of evaluation (design.md "Play Lifecycle");
    // an aborted play then takes what reached its queues after the abort,
    // and a completed one hands its sources to the tail's feedback. A play
    // in Closing seals it without allocating, its sources closed once
    // Closing ends.
    // A failed close leaves the closing unknown, and the record keeps what
    // the play already took: what reached the queues after its last step is
    // lost, and the record is marked incomplete (input failed).
    let handed_off = end == PlayEnd::Completed;
    let closed = if handed_off {
        rig.hand_off()
    } else if admit.is_some() {
        rig.abort_close()
    } else {
        rig.close()
    };
    let handed_off = handed_off && closed.is_ok();
    let sealed = admit.is_some() && closed.is_ok();
    // A command that arrived during the close handshake is kept for what
    // follows: it cuts the tail, or, after an abort, it is the run's next
    // step. A suspend there still happened while recording was open.
    // In Closing it is Closing's: a quit does nothing, a retry ends it.
    let mut carried = None;
    let mut latches = ClosingLatches::default();
    if admit.is_some() {
        while let Some(c) = rig.take_closing() {
            suspended |= c == Command::Suspend;
            latches.retry |= c == Command::Retry;
        }
    } else {
        while let Some(c) = rig.take() {
            suspended |= c == Command::Suspend;
            carried = carried.or(Some(c));
        }
    }
    // A suspend latched in the play's last steps or at the handshake cuts
    // Closing at once.
    latches.suspend = suspended;
    // A gated attempt still waiting after a cancellation keeps what reached
    // the queues since in its cancelled opening.
    let waiting = gate.as_ref().is_some_and(|g| g.waiting);
    let closing = match closed {
        Ok(closing) => {
            if let Some(g) = gate.as_mut().filter(|_| waiting) {
                let _ = g.history.drain(logic.readers_mut());
            } else {
                logic.drain_after_close();
            }
            closing
        }
        Err(e) => {
            input_error = input_error.or(Some(e));
            Closing::default()
        }
    };
    // A gated attempt's history: before its commit, the last opening ends
    // the capture, cancelled before playback (design.md "Play Lifecycle",
    // ending without a played attempt), at origin 0; after it, the
    // cancelled openings are relative to the winning one. It is copied out
    // once the play is over, after Closing, which copies nothing.
    if let Some(g) = gate.as_mut().filter(|g| !g.committed) {
        let closed_at = TimeUs(rig.now().0.saturating_sub(origin.0));
        let reason = if waiting {
            CancelReason::Held
        } else {
            let _ = g.history.begin(logic.start(), origin);
            let _ = g.history.take(logic.popped());
            cancel_reason(end, rig.quitting())
        };
        let _ = g.history.seal(closed_at, &closing.arrivals, reason);
    }
    let incomplete = Incomplete {
        overflow: logic.overflowed()
            || closing.overflowed
            || gate.as_ref().is_some_and(|g| g.history.exhausted()),
        source_limit: closing.source_limited,
        suspend: suspended,
        input_failed: input_error.is_some(),
    };
    let tail = match stage {
        Some(stage) => run_tail(
            rig,
            &mut logic,
            stage,
            origin,
            carried,
            &mut input_error,
            &mut output_error,
        ),
        None => TailEnd::NoTail,
    };
    // Closing, once its recording is sealed and its record's flags are
    // known: nothing it takes changes them.
    let closing_report = closing_at.map(|start| {
        let stage = ClosingStage::begin(start, rig.declared_close());
        let end = run_closing(
            rig,
            stage,
            origin,
            &mut latches,
            &mut input_error,
            &mut output_error,
        );
        ClosingReport {
            started_at: stage.start,
            ended_at: TimeUs(rig.now().0 - origin.0),
            end,
        }
    });
    // The feedback stopped with the tail, and Closing ended: the sources
    // close now, so no input reaches the result or the next attempt.
    if (handed_off || sealed)
        && let Err(e) = rig.end_feedback()
    {
        input_error = input_error.or(Some(e));
    }
    // As the completion froze them: the feedback recorded nothing and
    // changed nothing scored.
    let view = logic.view();
    let record = logic.into_record();
    // A capture's last kept opening is its origin 0, even when the
    // history had no room for the last one.
    let history = gate.as_ref().map(|g| {
        let last = if g.committed {
            origin
        } else {
            g.history.last_origin().unwrap_or(origin)
        };
        g.history.history(last)
    });

    rig.play_over();
    let report = loaded.finish_play(PlayOutcome {
        start,
        end,
        tail_cut: tail.cut(),
        tail_skipped: tail == TailEnd::Skipped,
        output_error,
        input_error,
        live,
        view,
        record,
        kinds: opening.kinds,
        arrivals: closing.arrivals,
        axis_out_of_range: closing.axis_out_of_range,
        incomplete,
        played,
        history,
        closing: closing_report,
    });
    // A completed play's tail took the carried command.
    let next = carried.filter(|_| end != PlayEnd::Completed);
    // The quit key before the chart started (in READY, or as
    // recording closed on a play aborted there) leaves the chart without a
    // result; from its start on, the aborted play's result shows, and a
    // quit that arrived as recording closed is taken with the play. A
    // gated attempt quit before its commit leaves it the same way.
    // A play admitted to Closing always shows its result.
    let quit = end == PlayEnd::Aborted(AbortCause::Quit) || next == Some(Command::Quit);
    let counted_down = admit.is_none() && quit && (!report.played() || last < starts_at);
    let mut next = next.filter(|&c| c != Command::Quit);
    let mut next_round = stage.is_some_and(|s| s.repeats) && tail == TailEnd::RanOut;
    // Commands still pending once the play is over, pressed as it ended or
    // during the save, belong to that play, completed or aborted: the quit
    // key is taken with it (for a completed play, the skip to its result:
    // a repeated round shows its result instead of starting the next),
    // never the result's back; a retry retries; a suspend means nothing
    // there.
    while let Some(c) = rig.take() {
        match TailAction::of(c) {
            TailAction::SkipToResult => next_round = false,
            TailAction::Cut(AbortCause::Retry) => next = next.or(Some(c)),
            TailAction::Cut(_) => {}
        }
    }
    Ok(Some(Played {
        report,
        next,
        next_round,
        counted_down,
    }))
}

/// Opens a gated attempt's first READY candidate (design.md "Play
/// Lifecycle", the start gate): the input side opens it only where START
/// and SELECT are verifiably released. A hold there defers it
/// ([`Rig::defer`]): the attempt serves the quit key, a close and the
/// threads' and the screen's failures until the latch reads released,
/// then quiesces the loading state again and asks again. `None` when the
/// player left before it opened: nothing opened, so nothing is saved.
///
/// # Errors
///
/// The threads' failures, and a screen that does not quiesce the loading
/// state, which end the run as before any opening.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn open_gated<R: Rig>(
    rig: &mut R,
    lead_in: TimeUs,
    generation: u32,
) -> Result<Option<Opening<R::Reader>>, LiveError<R::ScreenError>> {
    loop {
        if let Some(opening) = rig
            .open_candidate(lead_in, generation)
            .map_err(LiveError::Input)?
        {
            return Ok(Some(opening));
        }
        rig.defer();
        loop {
            rig.wait();
            // The threads and the screen prepared for the attempt, as
            // before any opening: a screen lost while held ends the run.
            preflight(rig)?;
            // Every command pending is taken before the opening is asked
            // again: a retry or suspend before recording opens belongs to
            // no play, and a quit among them leaves, whatever came first.
            let mut quit = rig.quitting();
            while let Some(c) = rig.take() {
                quit |= c == Command::Quit;
            }
            if quit {
                return Ok(None);
            }
            if !rig.holding() {
                break;
            }
        }
        rig.end_loading().map_err(LiveError::Screen)?;
        preflight(rig)?;
    }
}

/// A gated attempt's READY candidates (design.md "Play Lifecycle", the
/// start gate): whether the current one committed or waits after its
/// cancellation, the history of cancelled openings, and the rearmed
/// snapshot's storage, all reserved with the play's setup.
struct Candidates {
    committed: bool,
    waiting: bool,
    history: HistoryRecorder,
    next: InputStart,
}

/// What one step of a gated attempt before its commit does.
enum Step {
    /// The candidate is live: the step advances it.
    Go,
    /// The attempt waits for START and SELECT to be released: nothing
    /// advances.
    Wait,
    /// The next candidate opened, its chart time 0 here on the rig's
    /// clock.
    Rearmed(TimeUs),
    /// The attempt ends without a played attempt.
    End(AbortCause),
}

impl Candidates {
    fn new(held: usize) -> Self {
        Self {
            committed: false,
            waiting: false,
            history: HistoryRecorder::new(held),
            next: InputStart {
                opened_at: TimeUs(0),
                held: Vec::with_capacity(held),
            },
        }
    }

    /// A live candidate's step: its commit, or its cancellation by a hold,
    /// as the start gate tells. A cancelled candidate is acknowledged by
    /// the audio side and its opening and packets are kept: what its
    /// evaluator popped, then what is queued. Its evaluator advances no
    /// more.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn watch<R: Rig>(
        &mut self,
        rig: &mut R,
        logic: &mut Logic<'_, R::Reader>,
        origin: TimeUs,
        generation: u32,
    ) -> Step {
        let state = rig.candidate();
        if state.generation != generation {
            return Step::Go;
        }
        match state.phase {
            input::GatePhase::Committed => {
                self.committed = true;
                Step::Go
            }
            input::GatePhase::Cancelled => {
                rig.cancel_candidate(generation);
                logic.set_playing(false);
                self.waiting = true;
                let kept = self
                    .history
                    .begin(logic.start(), origin)
                    .and_then(|()| self.history.take(logic.popped()))
                    .and_then(|()| self.history.drain(logic.readers_mut()));
                if kept.is_err() {
                    Step::End(AbortCause::Overflow)
                } else {
                    Step::Wait
                }
            }
            input::GatePhase::Idle | input::GatePhase::Armed => Step::Go,
        }
    }

    /// A step waiting after a cancellation: the capture goes on into the
    /// cancelled opening, and once the latch reads both roles released
    /// the input side is asked to seal it and open a fresh, full READY,
    /// with a new origin and snapshot, at once. The evaluator is reset in
    /// place for it, never rewound.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait_release<R: Rig>(
        &mut self,
        rig: &mut R,
        logic: &mut Logic<'_, R::Reader>,
        lead_in: TimeUs,
        generation: &mut u32,
    ) -> Step {
        if self.history.drain(logic.readers_mut()).is_err() {
            return Step::End(AbortCause::Overflow);
        }
        if rig.holding() {
            return Step::Wait;
        }
        let next = generation.wrapping_add(1);
        let mut readers = logic.lend_readers();
        let rearmed = rig.rearm_candidate(
            &mut readers,
            lead_in,
            next,
            &mut self.history,
            &mut self.next,
        );
        logic.restore_readers(readers);
        match rearmed {
            Err(_) => Step::End(AbortCause::InputFailed),
            Ok(Rearm::Held) if self.history.exhausted() => Step::End(AbortCause::Overflow),
            Ok(Rearm::Held) => Step::Wait,
            Ok(Rearm::Opened { origin }) => {
                *generation = next;
                self.waiting = false;
                logic.rearm(&self.next, origin);
                Step::Rearmed(origin)
            }
        }
    }
}

/// Why the last opening of an attempt that ended before its commit was
/// cancelled: its ending cause, `closing` when the window closed.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
const fn cancel_reason(end: PlayEnd, closing: bool) -> CancelReason {
    if closing {
        return CancelReason::Close;
    }
    match end {
        PlayEnd::Aborted(AbortCause::Quit) => CancelReason::Quit,
        PlayEnd::Aborted(AbortCause::Retry) => CancelReason::Retry,
        PlayEnd::Aborted(AbortCause::FocusLoss) => CancelReason::FocusLost,
        PlayEnd::Aborted(AbortCause::Overflow) => CancelReason::Exhausted,
        PlayEnd::Completed
        | PlayEnd::Aborted(
            AbortCause::LastSourceRemoved
            | AbortCause::Suspend
            | AbortCause::OutputFailed
            | AbortCause::GaugeFailed
            | AbortCause::ScreenFailed
            | AbortCause::InputFailed
            | AbortCause::RestoreFailed,
        ) => CancelReason::Failed,
    }
}

/// The stage after a completed play: a round's end stage, of which a full
/// play's audio tail is the special case (design.md "Audio tail",
/// "Mid-Chart Start and Section Repeat"). Nothing is recorded, and the
/// controls and failures act on every stage alike ([`run_tail`]); a stage
/// differs only in its data: where it ends, and what follows it once it
/// ran to its end. A full play is the stage that ends when the audio is
/// done and does not repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TailStage {
    /// Where the stage's ring-out ends, in the play's time: it then ends
    /// once the audio is done and the play's time has reached it, since
    /// the audio side mixes ahead and its report alone can come early.
    /// `None`: it ends when the audio is done (a full play).
    ring_out_end: Option<TimeUs>,
    /// Whether a stage run to its end is followed by the next round, with
    /// no result between (a repeated round); otherwise the result shows.
    repeats: bool,
}

impl TailStage {
    /// The stage of a play completed at play time `now`: a round rings out
    /// from that step; a full play, with no ring-out, seals its key sounds
    /// and lets its audio end by itself.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn begin<R: Rig>(rig: &mut R, loaded: &Loaded, round: bool, now: TimeUs) -> Self {
        if round {
            let end_at = TimeUs(now.0.saturating_add(RING_OUT.0));
            let fade_at = TimeUs(end_at.0.saturating_sub(FADE.0));
            rig.ring_out(fade_at, end_at);
            Self {
                ring_out_end: Some(end_at),
                repeats: loaded.practice().is_some_and(|p| p.repeat),
            }
        } else {
            rig.seal_audio();
            Self {
                ring_out_end: None,
                repeats: false,
            }
        }
    }

    /// Whether the stage has run to its end, for a play whose chart time 0
    /// lies at `origin`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn over<R: Rig>(self, rig: &R, origin: TimeUs) -> bool {
        rig.audio_done()
            && self
                .ring_out_end
                .is_none_or(|end_at| rig.now().0 - origin.0 >= end_at.0)
    }
}

/// How a play's tail stage ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TailEnd {
    /// The play did not complete: it had no tail.
    NoTail,
    /// It ran to its end.
    RanOut,
    /// The player skipped it to the result.
    Skipped,
    /// It was cut short with this cause.
    Cut(AbortCause),
}

impl TailEnd {
    /// What `action` ends the tail with.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn of(action: TailAction) -> Self {
        match action {
            TailAction::SkipToResult => Self::Skipped,
            TailAction::Cut(cause) => Self::Cut(cause),
        }
    }

    /// The cause that cut the tail, if one did.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    const fn cut(self) -> Option<AbortCause> {
        match self {
            Self::Cut(cause) => Some(cause),
            Self::NoTail | Self::RanOut | Self::Skipped => None,
        }
    }
}

/// Runs a completed play's tail stage until it ends (design.md "Audio
/// tail"). The command `carried` from the close, or an input-thread or
/// audio failure already known, ends it at once. Then, at each wait,
/// skipping to the result ends it, as do a retry or a suspend, closing the
/// window, an input-thread failure, focus loss, removal of the last device,
/// an audio failure or a screen failure, which cut it with their cause;
/// otherwise the tail's feedback converts the input that reached the
/// play's queues ([`Logic::feedback_to`]): its presses sound and its lanes
/// light, and nothing is judged or recorded. Whatever ends the tail ends
/// the feedback first, so no press is taken after it; whatever ends it
/// early stops its audio. The play stays complete either way.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn run_tail<R: Rig>(
    rig: &mut R,
    logic: &mut Logic<'_, R::Reader>,
    stage: TailStage,
    origin: TimeUs,
    carried: Option<Command>,
    input_error: &mut Option<InputWaitError>,
    output_error: &mut Option<OutputError>,
) -> TailEnd {
    let early = if let Some(c) = carried {
        Some(TailEnd::of(TailAction::of(c)))
    } else if let Some(e) = rig.input_failure() {
        *input_error = Some(e);
        Some(TailEnd::Cut(AbortCause::InputFailed))
    } else if let Some(e) = rig.audio_failure() {
        *output_error = Some(e);
        Some(TailEnd::Cut(AbortCause::OutputFailed))
    } else {
        None
    };
    if let Some(end) = early {
        rig.cancel_audio();
        return end;
    }
    while !stage.over(rig, origin) {
        rig.wait();
        // Closing the window ends the run: no command is taken with it.
        let ended = if rig.quitting() {
            Some(TailEnd::Cut(AbortCause::Quit))
        } else if let Some(c) = rig.take() {
            Some(TailEnd::of(TailAction::of(c)))
        } else if let Some(e) = rig.input_failure() {
            *input_error = Some(e);
            Some(TailEnd::Cut(AbortCause::InputFailed))
        } else if rig.focus_lost() {
            Some(TailEnd::Cut(AbortCause::FocusLoss))
        } else if !rig.connected() {
            Some(TailEnd::Cut(AbortCause::LastSourceRemoved))
        } else if let Some(e) = rig.audio_failure() {
            *output_error = Some(e);
            Some(TailEnd::Cut(AbortCause::OutputFailed))
        } else if rig.screen_failed() {
            Some(TailEnd::Cut(AbortCause::ScreenFailed))
        } else {
            let _ = logic.feedback_to();
            rig.stepped(false);
            rig.audio_failure().map(|e| {
                *output_error = Some(e);
                TailEnd::Cut(AbortCause::OutputFailed)
            })
        };
        if let Some(end) = ended {
            rig.cancel_audio();
            return end;
        }
    }
    TailEnd::RanOut
}

/// Runs a play's Closing until it ends (design.md "Play Lifecycle",
/// Closing): on entry and after every wait, every pending command is taken
/// as Closing's ([`Rig::take_closing`]: a quit does nothing, a retry and a
/// suspend are latched), the input thread's and the audio output's
/// failures are latched into `input_error` and `output_error` whichever
/// way it ends, and then, in this order: closing the window, a retry, an
/// input failure, an audio failure, a screen failure, a suspend, and its
/// time running out on the play clock end it. Focus loss and removal of
/// the last device change nothing. Nothing is evaluated, recorded or
/// allocated.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn run_closing<R: Rig>(
    rig: &mut R,
    stage: ClosingStage,
    origin: TimeUs,
    latches: &mut ClosingLatches,
    input_error: &mut Option<InputWaitError>,
    output_error: &mut Option<OutputError>,
) -> ClosingEnd {
    loop {
        while let Some(c) = rig.take_closing() {
            match c {
                Command::Quit => {}
                Command::Retry => latches.retry = true,
                Command::Suspend => latches.suspend = true,
            }
        }
        let input = rig.input_failure();
        if let Some(e) = input {
            *input_error = input_error.or(Some(e));
        }
        let audio = rig.audio_failure();
        if let Some(e) = audio {
            *output_error = output_error.or(Some(e));
        }
        let screen = rig.screen_failed();
        let now = TimeUs(rig.now().0 - origin.0);
        let end = if rig.quitting() {
            Some(ClosingEnd::Cut(AbortCause::Quit))
        } else if latches.retry {
            Some(ClosingEnd::Retry)
        } else if input.is_some() {
            Some(ClosingEnd::Cut(AbortCause::InputFailed))
        } else if audio.is_some() {
            Some(ClosingEnd::Cut(AbortCause::OutputFailed))
        } else if screen {
            Some(ClosingEnd::Cut(AbortCause::ScreenFailed))
        } else if latches.suspend {
            Some(ClosingEnd::Cut(AbortCause::Suspend))
        } else if stage.over(now) {
            Some(ClosingEnd::RanOut)
        } else {
            None
        };
        if let Some(end) = end {
            return end;
        }
        rig.wait();
    }
}
