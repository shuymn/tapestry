//! The play lifecycle every driver shares (D13): prepare → open recording
//! (the countdown starts) → play → evaluation complete → audio tail → save
//! → result, with retry, quit, focus loss, removal of the last device,
//! suspend, an audio or screen failure, and a session policy failure
//! ending the play from any state.
//!
//! The world around the play is a [`Rig`]: the game's threads in the
//! player, a virtual clock and scripted input in [`crate::Session`] and in
//! tests. The rig owns the clock and every wait; the transitions here only
//! ask it what happened and tell it what to do.
//!
//! - The screen is prepared, and acknowledges it, before recording opens,
//!   so the countdown origin is chosen only once the work before play is
//!   done; what follows the opening (the logic step's construction and the
//!   handoff to audio and the screen) is bounded by the chart's size.
//! - A quit is latched by the rig and checked before recording opens and in
//!   every wait, so it is never lost to a transition.
//! - An aborted play closes recording and only then drains what is left in
//!   the source queues into its record, unevaluated, so input that arrived
//!   after the abort is kept (D13).
//! - A completed play seals its key sounds; the tail ends when the audio
//!   side reports the sealed audio done.

use ::audio::OutputError;
use bms_model::TimeUs;
use eval::{InputStart, JudgeEvent};
use input::{Arrival, DeviceKind};
use replay::Incomplete;

use crate::audio_loop::AudioWaitError;
use crate::logic::{AbortCause, Advance, Logic, LogicError, SourceRead};
use crate::render::RenderSide;
use crate::session::{Command, Loaded, PlayEnd, PlayOutcome, PlayReport, Settings};
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
    /// After a step's decisions (`aborted`: the step ended the play).
    /// A virtual rig steps its audio through the step's own time here;
    /// threads need nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn stepped(&mut self, aborted: bool) {
        let _ = aborted;
    }
    /// What each logic step of a play measured (D11): the step itself
    /// (its finalization delay), the events it delivered, and `delivered`,
    /// the play time read once the step had handed them to its sinks, so
    /// input to judgment covers the step's processing. Called after every
    /// step, before [`stepped`](Self::stepped).
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
    fn open(&mut self, lead_in: TimeUs) -> Opening<Self::Reader>;
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
    /// Whether the sealed play's audio has ended.
    fn audio_done(&self) -> bool;
    /// Closes recording and waits until nothing more is pushed.
    fn close(&mut self) -> Closing;
    /// The play ended: its report, before any end screen.
    fn ended(&mut self, report: &PlayReport);
    /// Shows the end screen of a play that neither quit nor retried.
    fn show_end(&mut self, report: &PlayReport);
    /// Before each play opens recording: settings changed since the
    /// previous play are written into `settings`, which the play is
    /// evaluated and recorded with. The input offset changes only here, so
    /// a change takes effect from the next play (D13).
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
}

/// Plays until the player quits: every play's report goes to
/// [`Rig::ended`]; a play that neither quit nor retried shows its end
/// screen, which waits for retry or quit.
///
/// # Errors
///
/// [`LiveError`] when a play cannot start; the plays before it are saved.
pub fn run<R: Rig>(loaded: &mut Loaded, rig: &mut R) -> Result<(), LiveError<R::ScreenError>> {
    loop {
        let Some((report, next)) = play_and_next(loaded, rig)? else {
            return Ok(());
        };
        rig.ended(&report);
        let by = |c: AbortCause| {
            report.end == PlayEnd::Aborted(c)
                || report.tail_cut == Some(c)
                || next.map(Command::cause) == Some(c)
        };
        if rig.quitting() || by(AbortCause::Quit) {
            return Ok(());
        }
        if by(AbortCause::Retry) {
            continue;
        }
        rig.show_end(&report);
        loop {
            rig.wait();
            if rig.quitting() {
                return Ok(());
            }
            // The screen failed during the play, its tail or this wait: it
            // is built again, acknowledged, before the end screen shows
            // again; if it cannot be, the run ends with the reason.
            if rig.screen_failed() {
                rig.prepare_screen().map_err(LiveError::Screen)?;
                rig.show_end(&report);
            }
            match rig.take() {
                Some(Command::Retry) => break,
                Some(Command::Quit) => return Ok(()),
                _ => {}
            }
        }
    }
}

/// One play, from preparing the screen to its report. `None` when the
/// player quit before recording opened.
///
/// # Errors
///
/// [`LiveError`] when the screen or the logic step cannot start; recording
/// is closed again.
pub fn play<R: Rig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Option<PlayReport>, LiveError<R::ScreenError>> {
    Ok(play_and_next(loaded, rig)?.map(|(report, _)| report))
}

/// A play's report and the command that arrived as recording closed.
type Played = (PlayReport, Option<Command>);

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
    rig.prepare_screen().map_err(LiveError::Screen)?;
    rig.prepare_audio().map_err(LiveError::Audio)?;
    // A retry or suspend from before this play belongs to no play.
    if rig.quitting() || rig.take() == Some(Command::Quit) {
        return Ok(None);
    }
    rig.before_play(loaded.settings_mut());
    let start = loaded.begin_play();
    let opening = rig.open(loaded.config().lead_in);
    let origin = opening.origin;
    let plan = match loaded.plan(&opening.kinds, &opening.axes) {
        Ok(plan) => plan,
        Err(e) => {
            rig.close();
            return Err(LiveError::Logic(e));
        }
    };
    let (mut logic, screen) = match Logic::new(
        loaded.chart(),
        loaded.preset(),
        &plan.device,
        &opening.start,
        opening.readers,
        plan.logic,
        Vec::new(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            rig.close();
            return Err(LiveError::Logic(e));
        }
    };
    let (mut sink, sounds) = audio_link_at(logic.max_events(), origin);
    // Autoplay's key sounds are on the engine's schedule, ahead of their
    // judgments.
    if loaded.config().autoplay {
        sink = sink.without_key_sounds();
    }
    logic.add_sink(Box::new(sink));
    let mut live = Vec::with_capacity(logic.max_events());
    rig.start(origin, sounds, screen);
    let mut output_error = None;
    // A suspend while recording is open marks the record incomplete,
    // whichever cause ends the play (D13).
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
        } else if rig.focus_lost() {
            logic.abort(AbortCause::FocusLoss);
        } else if let Some(e) = rig.audio_failure() {
            output_error = Some(e);
            logic.abort(AbortCause::OutputFailed);
        } else if rig.screen_failed() {
            logic.abort(AbortCause::ScreenFailed);
        }
        let advance = logic.advance_to(TimeUs(rig.now().0 - origin.0));
        live.extend_from_slice(logic.events());
        let delivered = TimeUs(rig.now().0 - origin.0);
        rig.measured(&advance, logic.events(), delivered);
        rig.stepped(advance.aborted.is_some());
        if let Some(cause) = advance.aborted {
            rig.cancel_audio();
            break PlayEnd::Aborted(cause);
        }
        if advance.complete {
            rig.seal_audio();
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

    // Recording closes at the end of evaluation (D13); an aborted play
    // then takes what reached its queues after the abort.
    let closing = rig.close();
    // A command that arrived during the close handshake is kept for what
    // follows: it cuts the tail, or, after an abort, it is the run's next
    // step. A suspend there still happened while recording was open.
    let mut carried = None;
    while let Some(c) = rig.take() {
        suspended |= c == Command::Suspend;
        carried = carried.or(Some(c));
    }
    logic.drain_after_close();
    let incomplete = Incomplete {
        overflow: logic.overflowed() || closing.overflowed,
        source_limit: closing.source_limited,
        suspend: suspended,
    };
    let view = logic.view();
    let record = logic.into_record();

    // The audio tail of a completed play: nothing is recorded, and a
    // control, focus loss, removal of the last device, an audio failure or
    // a screen failure cuts it short; the record stays complete.
    let mut tail_cut = None;
    if end == PlayEnd::Completed
        && let Some(c) = carried
    {
        tail_cut = Some(c.cause());
        rig.cancel_audio();
    } else if end == PlayEnd::Completed
        && let Some(e) = rig.audio_failure()
    {
        output_error = Some(e);
        tail_cut = Some(AbortCause::OutputFailed);
        rig.cancel_audio();
    }
    while end == PlayEnd::Completed && tail_cut.is_none() && !rig.audio_done() {
        rig.wait();
        tail_cut = if rig.quitting() {
            Some(AbortCause::Quit)
        } else if let Some(c) = rig.take() {
            Some(c.cause())
        } else if rig.focus_lost() {
            Some(AbortCause::FocusLoss)
        } else if !rig.connected() {
            Some(AbortCause::LastSourceRemoved)
        } else if let Some(e) = rig.audio_failure() {
            output_error = Some(e);
            Some(AbortCause::OutputFailed)
        } else if rig.screen_failed() {
            Some(AbortCause::ScreenFailed)
        } else {
            None
        };
        if tail_cut.is_none() {
            rig.stepped(false);
            if let Some(e) = rig.audio_failure() {
                output_error = Some(e);
                tail_cut = Some(AbortCause::OutputFailed);
            }
        }
        if tail_cut.is_some() {
            rig.cancel_audio();
        }
    }

    rig.play_over();
    let report = loaded.finish_play(PlayOutcome {
        start,
        end,
        tail_cut,
        output_error,
        live,
        view,
        record,
        kinds: opening.kinds,
        arrivals: closing.arrivals,
        axis_out_of_range: closing.axis_out_of_range,
        incomplete,
    });
    // A completed play's tail took the carried command.
    let next = carried.filter(|_| end != PlayEnd::Completed);
    Ok(Some((report, next)))
}
