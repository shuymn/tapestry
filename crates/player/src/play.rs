//! The game thread's rig: the play lifecycle of `game::live` on the real
//! threads (D8, D13).
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
    Advance, AudioQueue, Closing, Command, Opening, PlayReport, RenderSide, Rig, Settings, Tuning,
};
use input::{QpcClock, SourceReader};

use crate::audio_side::AudioLink;
use crate::controls::Controls;
use crate::end_screen::{self, Hints, LoadTimes, QueueLine};
use crate::input_side::{InputLink, Request};
use crate::measure::{
    Figures, PlayMeasure, Step, StepSample, judgment_samples, measure_path, to_toml,
};
use crate::render_side::{RenderLink, RenderMsg};
use crate::tuning::{Delivered, SettingsFiles};

/// How long the logic step parks at most without a wake from the input
/// thread; also the pace of the tail and the end screen.
const PARK: Duration = Duration::from_millis(2);

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
    /// The controls recorded from keyboards and from controllers.
    pub recorded: (Vec<eval::ControlId>, Vec<eval::ControlId>),
    /// The gauge to show.
    pub gauge: usize,
    /// The countdown's length.
    pub lead_in: TimeUs,
    /// The input offset the current play uses.
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
    /// Settings keys pressed by themselves at the first end screen
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub adjust_on_end: Vec<game::Adjust>,
}

impl PlayerRig<'_> {
    /// Writes changed settings, then ends the render and audio threads.
    pub fn shut_down(mut self) {
        self.persist();
        self.render.exit();
        self.audio.exit();
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
    fn sync(&mut self) {
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
            Some(Err(e)) => eprintln!("not saved: {e}"),
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

fn since(from: TimeUs, to: TimeUs) -> Duration {
    Duration::from_micros(u64::try_from(to.0 - from.0).unwrap_or(0))
}

impl Rig for PlayerRig<'_> {
    type Reader = SourceReader;

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn wait(&mut self) {
        thread::park_timeout(PARK);
        #[cfg(feature = "dev-tools")]
        if let Some(c) = self.countdown_at
            && !self.adjust_in_play.is_empty()
            && self.clock.now().0 - c.0 - self.lead_in.0 >= self.adjust_at.0
        {
            for a in self.adjust_in_play.drain(..) {
                self.controls.tuning().adjust(a);
            }
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

    fn prepare_audio(&mut self) -> Result<(), String> {
        self.audio.control.ready().map_err(|e| e.to_string())?;
        // Between plays, before recording opens: the change never lands
        // inside a play or its countdown.
        self.queued_now = self
            .audio
            .control
            .set_queued_periods(self.queue.periods())
            .map_err(|e| e.to_string())?;
        // A volume changed before the first play (during startup) or on
        // the end screen reaches the engine before the play starts.
        self.sync();
        Ok(())
    }

    fn prepare_screen(&mut self) -> Result<(), String> {
        self.retry_from = self.pending_retry.take();
        self.render.prepare()
    }

    fn open(&mut self, lead_in: TimeUs) -> Opening<SourceReader> {
        self.input.open(Request::Open {
            lead_in,
            logic: thread::current(),
            keyboard: self.recorded.0.clone(),
            controller: self.recorded.1.clone(),
        })
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        let countdown_at = TimeUs(origin.0 - self.lead_in.0);
        self.countdown_at = Some(countdown_at);
        println!("play started");
        // Audio was readied before recording opened, so this does not
        // wait; a worker gone since shows as the output's failure.
        let _ = self.audio.control.play(origin, sounds);
        self.generation = self.audio.control.generation();
        self.steps_dropped = 0;
        let counters = self.audio.control.counters();
        let measure = PlayMeasure {
            generation: self.generation,
            origin,
            load_from: self.retry_from.unwrap_or(TimeUs(0)),
            retry: self.retry_from.is_some(),
            audio: Some((
                counters.clone(),
                counters.underruns(),
                counters.probes_dropped(),
            )),
            queued_periods: self.queued_now,
        };
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
    fn audio_done(&self) -> bool {
        self.audio.control.done()
    }

    fn close(&mut self) -> Closing {
        self.input.close()
    }

    fn before_play(&mut self, settings: &mut Settings) {
        // The input offset keys stop working here, so the play is evaluated
        // with the input offset it starts with (D13); the other values
        // are recorded as they are at the start.
        self.end_lines = None;
        let t = self.controls.tuning().close_end_screen();
        t.apply_to(settings);
        self.input_offset_us = t.input_offset_us;
    }

    fn ended(&mut self, report: &PlayReport) {
        // Load times run to the play's first frame: from the process start
        // (the clock's 0) for the first play, from the retry's receipt for
        // a retry.
        if let Some(frame) = self.render.first_frame() {
            if let Some(c) = self.countdown_at {
                println!(
                    "countdown: first frame {} us after it started",
                    frame.0 - c.0
                );
            }
            match self.retry_from.take() {
                Some(from) => self.times.retry = Some(since(from, frame)),
                None if self.times.first.is_none() => {
                    self.times.first = Some(since(TimeUs(0), frame));
                }
                None => {}
            }
        }
        self.print(report);
        self.judge_queue();
        self.write_measure(report);
        self.persist();
    }

    fn show_end(&mut self, report: &PlayReport) {
        let tuning = self.controls.tuning();
        tuning.open_end_screen();
        let queue = self.queue_line.unwrap_or(QueueLine {
            played: self.queued_now,
            steady_underruns: 0,
            next: end_screen::next_queue(self.queue.periods(), self.queue_most),
        });
        self.end_lines = Some(end_screen::lines(
            report,
            self.input_offset_us,
            self.times,
            queue,
            &self.hints,
        ));
        self.send_end(tuning.get());
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
