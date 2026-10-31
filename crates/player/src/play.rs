//! The game thread's rig: the play lifecycle of `game::live` on the real
//! threads (design.md "Overall Architecture", "Play Lifecycle").
//!
//! Each play has its own timeline: the input thread opens recording at a
//! boundary and puts chart time 0 `lead_in` after it on the QPC clock. The
//! source set moves every recorded time onto the play's timeline, the logic
//! step advances to the clock minus that origin, the audio engine and the
//! key sounds' origins sit on the clock with the play's origin, and the
//! render thread draws at the clock minus the origin. This thread runs the
//! logic step, woken by the input thread after each publication.

use std::thread;
use std::time::Duration;

use audio::OutputError;
use bms_model::{TimeSource, TimeUs};
use eval::JudgeEvent;
use game::{
    Advance, AudioQueue, AudioWaitError, Chain, Closing, Command, InputWaitError, MenuCommand,
    Opening, PlayReport, RenderSide, ResultScreen, Rig, Settings, Tuning,
};
use input::{QpcClock, SourceReader};

use crate::audio_side::AudioLink;
use crate::controls::Controls;
use crate::end_screen::{self, Hints, LoadTimes, QueueLine};
use crate::input_side::{InputLink, Recorded, Request};
use crate::measure::{
    AudioBase, Figures, PlayMeasure, Step, StepSample, judgment_samples, limits_line, measure_path,
    process_page_faults, to_toml,
};
use crate::render_side::{RenderEnd, RenderLink, RenderMsg, ScreenError};
use crate::result_input::ResultInput;
use crate::tuning::{Delivered, SettingsFiles};

/// How long the logic step parks at most without a wake from the input
/// thread; also the pace of the tail.
const PARK: Duration = Duration::from_millis(2);

/// How long the game thread parks at most outside a play and its tail
/// (the end screen): until the window or the system unparks it
/// ([`Controls`]), or the next look at what has no wake of its own (the
/// screen's failure, a timed quit).
const IDLE_PARK: Duration = Duration::from_micros(game::IDLE_WAIT.0.unsigned_abs());

/// Each saved play's record and figures, in order (dev-tools).
#[cfg(feature = "dev-tools")]
pub type MeasuredPlays = std::sync::Arc<std::sync::Mutex<Vec<(std::path::PathBuf, Figures)>>>;

/// The game's threads as a rig.
pub struct PlayerRig<'a> {
    /// The input thread.
    pub input: InputLink,
    /// The audio thread.
    pub audio: AudioLink,
    /// The render thread.
    pub render: RenderLink,
    /// The window's controls.
    pub controls: &'a Controls,
    /// The clock, started with the process.
    pub clock: QpcClock,
    /// What each play records.
    pub recorded: Recorded,
    /// The gauge to show.
    pub gauge: usize,
    /// Whether the session plays generated input (design.md "Autoplay"):
    /// no recommended input offset, and "not saved" when nothing was.
    pub autoplay: bool,
    /// The countdown's length.
    pub lead_in: TimeUs,
    /// From recording's open to the end of the play's audio tail: the
    /// game thread parks [`PARK`] at most, else [`IDLE_PARK`].
    pub in_play: bool,
    /// The input offset the current play uses (a device play; autoplay
    /// plays with 0 whatever it is).
    pub input_offset_us: i64,
    /// The settings files.
    pub files: SettingsFiles,
    /// The settings keys' values last acted on.
    pub seen: Delivered,
    /// The end screen's summary, while it shows; the settings lines are
    /// added each time it is drawn.
    pub end_lines: Option<Vec<String>>,
    /// The end screen's key names.
    pub hints: Hints,
    /// The result screen's input.
    pub result_input: ResultInput,
    /// The view last printed: its entry and line ([`crate::end_screen::view_log`]).
    pub viewed: Option<(game::EntryId, String)>,
    /// The evaluations the shown result had made when last printed.
    pub evaluated: u64,
    /// The result screen's input, scripted, without selection (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub result_script: Option<crate::dev::SelectScript>,
    /// What the first play's load time runs from: the process start (the
    /// clock's 0), or the end of what the command set up first (a
    /// measurement's PresentMon capture).
    pub load_start: TimeUs,
    /// The latest retry taken, by its receipt time: the next play starts
    /// from it.
    pub pending_retry: Option<TimeUs>,
    /// The retry the current play started from.
    pub retry_from: Option<TimeUs>,
    /// Load times shown on the end screen.
    pub times: LoadTimes,
    /// When the current play's countdown started, on the clock.
    pub countdown_at: Option<TimeUs>,
    /// Where each logic step's measurement goes (the render thread).
    pub steps: rtrb::Producer<StepSample>,
    /// Step samples of the current play the full queue refused.
    pub steps_dropped: u64,
    /// The figures of the play just over, until they are written.
    pub figures: Option<Figures>,
    /// The current play's number, as the audio thread knows it.
    pub generation: u64,
    /// The periods the next play queues on the audio output.
    pub queue: game::QueueChoice,
    /// The most periods the output's queue holds, when it says.
    pub queue_most: Option<u32>,
    /// The periods the output keeps for the current play, when it says.
    pub queued_now: Option<u32>,
    /// The last ended play's audio queue, for its end screen.
    pub queue_line: Option<QueueLine>,
    /// Each saved play's record and figures, kept for the display
    /// capture of `measure play` (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub measured_plays: Option<MeasuredPlays>,
    /// The scenario the run plays (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub scenario: Option<crate::dev::scenario::State>,
    /// Quit once the end screen has shown this long (scripted runs).
    #[cfg(feature = "dev-tools")]
    pub quit_after_end: Option<Duration>,
    #[cfg(feature = "dev-tools")]
    pub end_shown: Option<std::time::Instant>,
    /// Retries still to send by itself at an end screen (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub auto_retries: u32,
    /// Settings keys pressed by themselves when the first play reaches
    /// chart time `adjust_at` (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub adjust_in_play: Vec<game::Adjust>,
    #[cfg(feature = "dev-tools")]
    pub adjust_at: TimeUs,
    /// The select screen, with song selection.
    pub selection: Option<crate::selection::Selection>,
    /// Settings keys pressed by themselves at the first end screen
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub adjust_on_end: Vec<game::Adjust>,
}

impl PlayerRig<'_> {
    /// Writes changed settings, then ends the render and audio threads;
    /// how the render thread ended: when [`RenderEnd::Stuck`], the window
    /// must stay open until the process exits.
    #[must_use]
    pub fn shut_down(mut self) -> RenderEnd {
        self.persist();
        let ended = self.render.exit();
        self.audio.exit();
        ended
    }

    /// Writes the settings keys' values to `device.toml` and
    /// `player.toml`, each atomically and only if it changed. Called when a
    /// play ends and when the run ends, never during a play.
    fn persist(&mut self) {
        self.files
            .write(self.controls.tuning().get(), self.queue.periods());
    }

    /// Counts the ended play's underruns after its countdown, raises the
    /// next play's queue on any, and keeps the end screen's line.
    fn judge_queue(&mut self) {
        let steady = self
            .audio
            .control
            .counters()
            .steady_underruns(self.generation);
        self.queue.play_ended(steady);
        // What the next play will keep: never more than the output holds.
        let next = end_screen::next_queue(self.queue.periods(), self.queue_most);
        let line = QueueLine {
            played: self.queued_now,
            steady_underruns: steady,
            next,
        };
        println!(
            "audio: queued {} periods, {steady} underruns after the countdown{}",
            self.queued_now
                .map_or_else(|| "an unknown number of".to_owned(), |q| q.to_string()),
            if self.queued_now == Some(next) {
                String::new()
            } else {
                format!("; the next play queues {next}")
            }
        );
        self.queue_line = Some(line);
    }

    /// Acts on changed settings keys' values: the volume goes to the
    /// audio thread, and a shown end screen is drawn again.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn sync(&mut self) {
        let Some(t) = self.seen.take_new(self.controls.tuning()) else {
            return;
        };
        // Always within 0 to unity: loading and `Tuning` keep it there.
        let _ = self
            .audio
            .control
            .set_volume(self.controls.tuning().volume());
        self.send_end(t);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn send_end(&self, t: Tuning) {
        if let Some(lines) = &self.end_lines {
            let mut all = lines.clone();
            all.extend(end_screen::settings_lines(t));
            self.render.send(RenderMsg::Text(all));
        }
    }

    /// Takes the ended play's figures from the render thread and writes
    /// them beside its saved record.
    /// Writes the figures frozen when the play was over beside its saved
    /// record.
    fn write_measure(&mut self, report: &PlayReport) {
        let Some(mut fig) = self.figures.take() else {
            eprintln!("measure: no figures for play {}", report.attempt);
            return;
        };
        fig.steps_dropped = std::mem::take(&mut self.steps_dropped);
        println!("{}", limits_line(&fig));
        let Some(Ok(saved)) = &report.saved else {
            return;
        };
        let path = measure_path(&saved.path);
        let ms = |d: Option<Duration>| d.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let text = to_toml(&fig, (ms(self.times.first), ms(self.times.retry)));
        match game::write_atomic(&path, text.as_bytes()) {
            Ok(()) => println!("measure: {}", path.display()),
            Err(e) => eprintln!("measure: {}: {e}", path.display()),
        }
        #[cfg(feature = "dev-tools")]
        if let Some(plays) = &self.measured_plays
            && let Ok(mut plays) = plays.lock()
        {
            plays.push((saved.path.clone(), fig));
        }
        #[cfg(not(feature = "dev-tools"))]
        let _: Figures = fig;
    }

    fn print(&self, report: &PlayReport) {
        let result = report.result.as_ref();
        let counts = result
            .and_then(|r| r.evaluation.as_ref().ok())
            .map(|e| e.tally.counts);
        match counts {
            Some(c) => println!(
                "play {}: {:?}; perfect {} great {} good {} bad {} poor {}; re-evaluation matches live: {}",
                report.attempt,
                report.end,
                c.perfect,
                c.great,
                c.good,
                c.bad,
                c.poor,
                result.is_some_and(|r| r.matches_live)
            ),
            None => println!(
                "play {}: {:?}; nothing recorded",
                report.attempt, report.end
            ),
        }
        match &report.saved {
            Some(Ok(r)) => println!("saved: {}", r.path.display()),
            Some(Err(e)) => eprintln!("not saved: {}", Chain(e)),
            None if self.autoplay => println!("autoplay: not saved"),
            None => {}
        }
        let ms = |d: Duration| format!("{} ms", d.as_millis());
        println!(
            "load: first {}, retry {}",
            self.times.first.map_or_else(|| "-".to_owned(), ms),
            self.times.retry.map_or_else(|| "-".to_owned(), ms)
        );
    }
}

pub(crate) fn since(from: TimeUs, to: TimeUs) -> Duration {
    Duration::from_micros(u64::try_from(to.0 - from.0).unwrap_or(0))
}

/// The command of a scenario's `until`, once it is due at `now` for the
/// play whose countdown started at `countdown_at`: taken, so it is sent
/// once.
#[cfg(feature = "dev-tools")]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) fn until_fired(
    until: &mut Option<(TimeUs, Command)>,
    countdown_at: Option<TimeUs>,
    lead_in: TimeUs,
    now: TimeUs,
) -> Option<Command> {
    let (Some(c), Some((at, command))) = (countdown_at, *until) else {
        return None;
    };
    if !until_due(c, lead_in, at, now) {
        return None;
    }
    *until = None;
    Some(command)
}

/// Whether chart time `at` has come at `now` for a play whose countdown
/// started at `countdown_at` and lasts `lead_in`, all on the clock.
#[cfg(feature = "dev-tools")]
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
pub(crate) const fn until_due(
    countdown_at: TimeUs,
    lead_in: TimeUs,
    at: TimeUs,
    now: TimeUs,
) -> bool {
    now.0 - countdown_at.0 - lead_in.0 >= at.0
}

impl Rig for PlayerRig<'_> {
    type Reader = SourceReader;
    type ScreenError = ScreenError;

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait(&mut self) {
        thread::park_timeout(if self.in_play { PARK } else { IDLE_PARK });
        // A scenario's `until`: the command, sent once, as its key would.
        #[cfg(feature = "dev-tools")]
        if let Some(sc) = &mut self.scenario
            && let Some(command) = until_fired(
                &mut sc.until,
                self.countdown_at,
                self.lead_in,
                self.clock.now(),
            )
        {
            self.controls.command(command);
        }
        #[cfg(feature = "dev-tools")]
        if let Some(c) = self.countdown_at
            && !self.adjust_in_play.is_empty()
            && until_due(c, self.lead_in, self.adjust_at, self.clock.now())
        {
            for a in self.adjust_in_play.drain(..) {
                self.controls.tuning().adjust(a);
            }
        }
        self.sync();
    }

    fn idle_wait(&mut self, deadline: TimeUs) {
        let left = deadline.0 - self.clock.now().0;
        if left > 0 {
            thread::park_timeout(Duration::from_micros(left.unsigned_abs()));
        }
        self.sync();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn take(&mut self) -> Option<Command> {
        let c = self.controls.take();
        if c == Some(Command::Retry) {
            self.pending_retry = self.controls.retry_at();
        }
        c
    }

    /// The retry and quit keys first (a suspend means nothing on a result),
    /// then, while a result shows, its own keys and controller events.
    fn take_menu(&mut self) -> Option<MenuCommand> {
        loop {
            match self.take() {
                Some(Command::Retry) => return Some(MenuCommand::Retry),
                Some(Command::Quit) => return Some(MenuCommand::Back),
                Some(Command::Suspend) => {}
                None => break,
            }
        }
        self.result_input.take()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn quitting(&self) -> bool {
        #[cfg(feature = "dev-tools")]
        if let (Some(after), Some(shown)) = (self.quit_after_end, self.end_shown)
            && shown.elapsed() >= after
        {
            return true;
        }
        self.controls.quitting()
    }

    fn prepare_audio(&mut self) -> Result<(), AudioWaitError> {
        // With selection, the scan has acknowledged its pause before
        // recording opens and the countdown starts.
        self.pause_scan();
        self.audio.control.ready()?;
        // Between plays, before recording opens: the change never lands
        // inside a play or its countdown.
        self.queued_now = self
            .audio
            .control
            .set_queued_periods(self.queue.periods())?;
        // A volume changed before the first play (during startup) or on
        // the end screen reaches the engine before the play starts.
        self.sync();
        Ok(())
    }

    fn prepare_screen(&mut self) -> Result<(), ScreenError> {
        self.retry_from = self.pending_retry.take();
        #[cfg(feature = "dev-tools")]
        self.scenario_pending();
        self.render.prepare()
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        self.in_play = true;
        self.input.open(Request::Open {
            lead_in,
            logic: thread::current(),
            recorded: self.recorded.clone(),
        })
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        // The play's setup ends here, before anything is handed over; the
        // page faults count from here too.
        let started = self.clock.now();
        let page_faults = process_page_faults();
        let countdown_at = TimeUs(origin.0 - self.lead_in.0);
        self.countdown_at = Some(countdown_at);
        println!("play started");
        // A chart's first play: a retry runs from its own receipt.
        if let Some(sel) = &self.selection
            && self.retry_from.is_none()
        {
            let ms = since(sel.chosen_at, started).as_millis();
            println!("load: the countdown started {ms} ms after the choice");
        }
        // Before the play is sent: the audio thread may mix its first wake
        // before this thread runs again.
        let audio = AudioBase::new(self.audio.control.counters());
        // The play's gain, fixed for the whole play: its chart's
        // normalization gain once measured, else the headroom.
        let gain = self
            .selection
            .as_ref()
            .map_or(audio::Gain::HEADROOM, |s| s.shelf.gain());
        // Audio was readied before recording opened, so this does not
        // wait; a worker gone since shows as the output's failure.
        let _ = self.audio.control.play_at(origin, sounds, gain);
        self.generation = self.audio.control.generation();
        self.steps_dropped = 0;
        let measure = PlayMeasure {
            generation: self.generation,
            origin,
            countdown_at,
            started,
            page_faults,
            load_from: self.retry_from.unwrap_or(self.load_start),
            retry: self.retry_from.is_some(),
            audio: Some(audio),
            queued_periods: self.queued_now,
        };
        #[cfg(feature = "dev-tools")]
        self.scenario_started();
        self.render.send(RenderMsg::Play {
            side: screen,
            gauge: self.gauge,
            measure: Box::new(measure),
        });
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn measured(&mut self, advance: &Advance, events: &[JudgeEvent], delivered: TimeUs) {
        let generation = self.generation;
        let finalization = advance.finalization_delay.map(|d| Step::Finalization(d.0));
        let judgments = judgment_samples(events, delivered).map(Step::Judgment);
        for kind in finalization.into_iter().chain(judgments) {
            if self.steps.push(StepSample { generation, kind }).is_err() {
                self.steps_dropped += 1;
            }
        }
    }

    fn play_over(&mut self) {
        // Measurement stops at the end of the play, before the save and the
        // re-evaluation, however long they take.
        self.figures = self.render.finish(self.clock.now());
        #[cfg(feature = "dev-tools")]
        self.scenario_over();
        // The tail is over: nothing reads the input thread's publications
        // or its focus checks until the next play opens.
        self.in_play = false;
        self.input.idle();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn focus_lost(&self) -> bool {
        self.input.tripped()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn connected(&self) -> bool {
        self.input.connected()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn audio_failure(&self) -> Option<OutputError> {
        self.audio.control.failure()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn screen_failed(&self) -> bool {
        self.render.failed()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn cancel_audio(&mut self) {
        self.audio.control.cancel();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn seal_audio(&mut self) {
        self.audio.control.seal();
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        self.audio.control.ring_out(fade_at, end_at);
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn audio_done(&self) -> bool {
        self.audio.control.done()
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        self.input.close()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn input_failure(&self) -> Option<InputWaitError> {
        self.input.failure()
    }

    fn before_play(&mut self, settings: &mut Settings) {
        // The input offset keys stop working here, so the play is evaluated
        // with the input offset it starts with (design.md "Offset Changes and
        // the Watermark"); the other values are recorded as they are at the
        // start.
        self.end_lines = None;
        self.result_input.showing = false;
        let t = self.controls.tuning().close_end_screen();
        t.apply_to(settings);
        self.input_offset_us = t.input_offset_us;
    }

    fn ended(&mut self, report: &PlayReport) {
        if let Some(c) = self.figures.as_ref().and_then(|f| f.countdown) {
            println!(
                "countdown: setup {} us, first frame {} after it started",
                c.setup_us,
                c.first_frame_us
                    .map_or_else(|| "N/A".to_owned(), |us| format!("{us} us"))
            );
        }
        // Load times run to the play's first frame: from the load's start
        // for the first play, from the retry's receipt for a retry.
        if let Some(frame) = self.render.first_frame() {
            match self.retry_from.take() {
                Some(from) => self.times.retry = Some(since(from, frame)),
                None if self.times.first.is_none() => {
                    self.times.first = Some(since(self.load_start, frame));
                }
                None => {}
            }
        }
        self.print(report);
        self.judge_queue();
        self.write_measure(report);
        self.persist();
    }

    fn show_end(&mut self, report: &PlayReport, screen: &ResultScreen) {
        // On entry: what was queued during the play is no result's.
        if self.result_input.enter() {
            self.viewed = None;
            self.evaluated = 0;
        }
        self.print_view(screen);
        let tuning = self.controls.tuning();
        tuning.open_end_screen();
        let queue = self.queue_line.unwrap_or(QueueLine {
            played: self.queued_now,
            steady_underruns: 0,
            next: end_screen::next_queue(self.queue.periods(), self.queue_most),
        });
        self.end_lines = Some(end_screen::lines(
            report,
            screen,
            self.input_offset_us,
            self.times,
            queue,
            &self.hints,
            self.autoplay,
        ));
        self.send_end(tuning.get());
        self.result_shown();
        #[cfg(feature = "dev-tools")]
        for a in self.adjust_on_end.drain(..) {
            tuning.adjust(a);
        }
        #[cfg(feature = "dev-tools")]
        if self.auto_retries > 0 {
            self.auto_retries -= 1;
            self.controls.command(Command::Retry);
        } else {
            self.end_shown = Some(std::time::Instant::now());
        }
    }
}

#[cfg(all(test, feature = "dev-tools"))]
mod tests {
    use bms_model::TimeUs;

    use game::Command;

    use super::{until_due, until_fired};

    /// A scenario's `until` is due at its chart time exactly: the countdown
    /// started at 5 s on the clock and lasts 1 s, so chart time 2.25 s is
    /// clock time 8.25 s.
    #[test]
    fn until_is_due_exactly_at_its_time() {
        let (countdown, lead_in) = (TimeUs(5_000_000), TimeUs(1_000_000));
        let at = TimeUs(2_250_000);
        assert!(!until_due(countdown, lead_in, at, TimeUs(8_249_999)));
        assert!(until_due(countdown, lead_in, at, TimeUs(8_250_000)));
        assert!(!until_due(countdown, lead_in, TimeUs(0), TimeUs(5_999_999)));
        assert!(until_due(countdown, lead_in, TimeUs(0), TimeUs(6_000_000)));
    }

    /// The `until` command is sent once, at its time on the play's clock:
    /// nothing before the countdown is known or before the time, then the
    /// command, then nothing again.
    #[test]
    fn until_fires_once_at_its_time() {
        let (countdown, lead_in) = (TimeUs(5_000_000), TimeUs(1_000_000));
        let mut until = Some((TimeUs(2_250_000), Command::Retry));
        assert_eq!(
            until_fired(&mut until, None, lead_in, TimeUs(9_000_000)),
            None
        );
        for now in [5_000_000, 7_000_000, 8_249_999] {
            assert_eq!(
                until_fired(&mut until, Some(countdown), lead_in, TimeUs(now)),
                None
            );
        }
        assert!(until.is_some());
        assert_eq!(
            until_fired(&mut until, Some(countdown), lead_in, TimeUs(8_250_000)),
            Some(Command::Retry)
        );
        assert_eq!(until, None);
        assert_eq!(
            until_fired(&mut until, Some(countdown), lead_in, TimeUs(8_250_001)),
            None
        );
    }
}
