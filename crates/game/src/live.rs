//! The play lifecycle every driver shares (design.md "Play Lifecycle"): prepare
//! → open recording (the countdown starts) → play → evaluation complete → audio
//! tail → save → result, with retry, quit, focus loss, removal of the last
//! device, suspend, an audio, screen or input-thread failure, and a session
//! policy failure ending the play from any state.
//!
//! The world around the play is a [`Rig`]: the game's threads in the
//! player, a virtual clock and scripted input in [`crate::Session`] and in
//! tests. The rig owns the clock and every wait; the transitions here only
//! ask it what happened and tell it what to do.
//!
//! - The screen and audio are prepared, and acknowledge it, before recording
//!   opens, so the countdown origin is chosen only once that work is done.
//!   The play's setup follows the opening, at the countdown's start: the
//!   plan, the reservation of the whole recording window and the logic
//!   step's construction, bounded by the chart and the reservation, then the
//!   handoff to audio and the screen (design.md "Recording Capacity", "Play
//!   Lifecycle"). The reserved memory is not touched in advance.
//! - A quit is latched by the rig and checked before recording opens and in
//!   every wait, so it is never lost to a transition.
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
//!   stage to the result instead of leaving the chart ([`TailAction`]). A
//!   repeated round whose stage ran to its end is followed by the next
//!   round at the following step, with no result between.
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
use replay::Incomplete;

use crate::app::IDLE_WAIT;
use crate::audio_loop::AudioWaitError;
use crate::logic::{AbortCause, Advance, Logic, LogicError, SourceRead};
use crate::policy::SessionEvent;
use crate::practice::{FADE, RING_OUT};
use crate::render::RenderSide;
use crate::restore::RestoreError;
use crate::result_screen::ResultScreen;
use crate::session::{
    Command, Loaded, MenuCommand, PlayEnd, PlayOutcome, PlayReport, RECORDING_MEMORY, Settings,
};
use crate::sound::{AudioQueue, audio_link_at};

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
    /// Readies the screen for a play (its play state reset) and waits for
    /// it to acknowledge.
    ///
    /// # Errors
    ///
    /// Why the screen cannot draw a play, which ends the run.
    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError>;
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
    /// No key sound follows.
    fn seal_audio(&mut self);
    /// A practice round completed: no key sound follows, and its audio
    /// rings out (design.md "Mid-Chart Start and Section Repeat"): the BGM
    /// plays on until `fade_at`, the output fades to silence by `end_at`,
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
    /// Whether the sealed play's audio has ended.
    fn audio_done(&self) -> bool;
    /// Closes recording and waits until nothing more is pushed.
    ///
    /// # Errors
    ///
    /// Why the input thread did not close it: the play is saved without
    /// the closing, and the run ends.
    fn close(&mut self) -> Result<Closing, InputWaitError>;
    /// The input thread's failure, once known: it is gone, or a handshake
    /// with it failed. Sticks once set.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn input_failure(&self) -> Option<InputWaitError> {
        None
    }
    /// The play ended: its report, before any end screen.
    fn ended(&mut self, report: &PlayReport);
    /// Shows the end screen of a play that neither quit nor retried, with
    /// its result screen's view; called again whenever the view changes or
    /// the screen was built again.
    fn show_end(&mut self, report: &PlayReport, screen: &ResultScreen);
    /// Before each play opens recording: settings changed since the previous
    /// play are written into `settings`, which the play is evaluated and
    /// recorded with. The input offset changes only here, so a change takes
    /// effect from the next play (design.md "Offset Changes and the
    /// Watermark").
    fn before_play(&mut self, settings: &mut Settings) {
        let _ = settings;
    }
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
    /// Audio cannot play.
    #[error("audio cannot play")]
    Audio(#[source] AudioWaitError),
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

/// How the plays of a chart were left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// The player left the chart: quit during it, or back on its result.
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
/// to [`Rig::ended`]; a play that neither quit nor retried shows its
/// result, which waits for a [`MenuCommand`]. A quit before the play is
/// complete leaves the chart ([`Leave::Back`]); after it, it skips the
/// tail to the result. Closing the window ends the run ([`Leave::Quit`]).
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
        }) = play_and_next(loaded, rig)?
        else {
            return leave(rig);
        };
        rig.ended(&report);
        let by = |c: AbortCause| {
            report.end == PlayEnd::Aborted(c)
                || report.tail_cut == Some(c)
                || next.map(Command::cause) == Some(c)
        };
        if rig.quitting() || by(AbortCause::Quit) {
            return leave(rig);
        }
        if by(AbortCause::Retry) {
            continue;
        }
        // A tail stage that ran to its end and repeats (a repeated round's
        // ring-out) is followed by the next round, whose countdown begins at
        // the following step; any other end shows the result.
        if next_round {
            rig.wait();
            continue;
        }
        loaded.emit(&SessionEvent::Result {
            attempt: report.attempt,
        });
        let mut screen =
            ResultScreen::new(&report, loaded.shelf(), loaded.chart(), RECORDING_MEMORY);
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
    rig.prepare_screen().map_err(LiveError::Screen)?;
    rig.prepare_audio().map_err(LiveError::Audio)?;
    // A retry or suspend from before this play belongs to no play.
    if rig.quitting() || rig.take() == Some(Command::Quit) {
        return Ok(None);
    }
    rig.before_play(loaded.settings_mut());
    let start = loaded.begin_play();
    // A full play's recording opens its countdown before chart time 0, a
    // practice round's `RESTORE_AHEAD` before its lead-in, whose start its
    // audio is restored at (design.md "Mid-Chart Start and Section
    // Repeat"): checked before anything opens.
    let opens_at = loaded.opens_at();
    let restore_at = loaded.restore_at();
    if let Some(at) = restore_at {
        rig.restore_ready(at).map_err(LiveError::Restore)?;
    }
    // A quit during the wait is latched and taken by the first step.
    let opening = rig
        .open(TimeUs(opens_at.0.saturating_neg()))
        .map_err(LiveError::Input)?;
    let origin = opening.origin;
    // The refusal is the error reported; a failed close adds nothing to it.
    let plan = match loaded.plan(&opening.kinds, &opening.axes) {
        Ok(plan) => plan,
        Err(e) => {
            let _ = rig.close();
            return Err(LiveError::Logic(e));
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
            return Err(LiveError::Logic(e));
        }
    };
    // The restore is planned on the opening's origin, in the play's setup,
    // and lands with the start: reset → restore → lead-in → section
    // evaluation. A refusal closes recording as the logic step's does.
    if let Some(at) = restore_at
        && let Err(e) = rig.restore_audio(origin, at)
    {
        let _ = rig.close();
        return Err(LiveError::Restore(e));
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

    let end = loop {
        rig.wait();
        // Anything seen in a step is handled before the step advances.
        // Every pending command is taken, so a suspend is never left behind
        // a quit or a retry.
        let mut command = None;
        while let Some(c) = rig.take() {
            suspended |= c == Command::Suspend;
            command = command.or(Some(c));
        }
        if rig.quitting() {
            logic.abort(AbortCause::Quit);
        } else if let Some(c) = command {
            logic.abort(c.cause());
        } else if let Some(e) = rig.input_failure() {
            // Before focus loss: the input thread publishes the focus gate
            // and the connections, which are stale once it is gone.
            input_error = Some(e);
            logic.abort(AbortCause::InputFailed);
        } else if rig.focus_lost() {
            logic.abort(AbortCause::FocusLoss);
        } else if let Some(e) = rig.audio_failure() {
            output_error = Some(e);
            logic.abort(AbortCause::OutputFailed);
        } else if rig.screen_failed() {
            logic.abort(AbortCause::ScreenFailed);
        }
        let now = TimeUs(rig.now().0 - origin.0);
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
        if loaded.ends_play(logic.gauges()) {
            logic.abort(AbortCause::GaugeFailed);
            rig.cancel_audio();
            break PlayEnd::Aborted(AbortCause::GaugeFailed);
        }
    };

    // Recording closes at the end of evaluation (design.md "Play Lifecycle");
    // an aborted play then takes what reached its queues after the abort.
    // A failed close leaves the closing unknown, and the record keeps what
    // the play already took: what reached the queues after its last step is
    // lost, and the record is marked incomplete (input failed).
    let closed = rig.close();
    // A command that arrived during the close handshake is kept for what
    // follows: it cuts the tail, or, after an abort, it is the run's next
    // step. A suspend there still happened while recording was open.
    let mut carried = None;
    while let Some(c) = rig.take() {
        suspended |= c == Command::Suspend;
        carried = carried.or(Some(c));
    }
    let closing = match closed {
        Ok(closing) => {
            logic.drain_after_close();
            closing
        }
        Err(e) => {
            input_error = input_error.or(Some(e));
            Closing::default()
        }
    };
    let incomplete = Incomplete {
        overflow: logic.overflowed() || closing.overflowed,
        source_limit: closing.source_limited,
        suspend: suspended,
        input_failed: input_error.is_some(),
    };
    let view = logic.view();
    let record = logic.into_record();

    let tail = match stage {
        Some(stage) => run_tail(
            rig,
            stage,
            origin,
            carried,
            &mut input_error,
            &mut output_error,
        ),
        None => TailEnd::NoTail,
    };

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
    });
    // A completed play's tail took the carried command.
    let mut next = carried.filter(|_| end != PlayEnd::Completed);
    let mut next_round = stage.is_some_and(|s| s.repeats) && tail == TailEnd::RanOut;
    // Commands still pending once a completed play's stage is over, pressed
    // as it ended or during the save, belong to that play: the quit key is
    // the skip to its result (a repeated round shows its result instead of
    // starting the next), never the result's back; a retry retries; a
    // suspend means nothing there.
    if end == PlayEnd::Completed {
        while let Some(c) = rig.take() {
            match TailAction::of(c) {
                TailAction::SkipToResult => next_round = false,
                TailAction::Cut(AbortCause::Retry) => next = next.or(Some(c)),
                TailAction::Cut(_) => {}
            }
        }
    }
    Ok(Some(Played {
        report,
        next,
        next_round,
    }))
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
/// an audio failure or a screen failure, which cut it with their cause.
/// Whatever ends it early stops its audio; the play stays complete either
/// way.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn run_tail<R: Rig>(
    rig: &mut R,
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
