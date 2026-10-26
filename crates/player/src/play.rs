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
use game::{AudioQueue, Closing, Command, Opening, PlayReport, RenderSide, Rig, Settings, Tuning};
use input::{QpcClock, SourceReader};

use crate::audio_side::AudioLink;
use crate::controls::Controls;
use crate::end_screen::{self, Hints, LoadTimes};
use crate::input_side::{InputLink, Request};
use crate::render_side::{RenderLink, RenderMsg};
use crate::tuning::{Delivered, SettingsFiles};

/// How long the logic step parks at most without a wake from the input
/// thread; also the pace of the tail and the end screen.
const PARK: Duration = Duration::from_millis(2);

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
        self.files.write(self.controls.tuning().get());
    }

    /// Acts on changed settings keys' values: the volume goes to the
    /// audio thread, and a shown end screen is drawn again.
    fn sync(&mut self) {
        let Some(t) = self.seen.take_new(self.controls.tuning()) else {
            return;
        };
        // Always within 0 to unity: `Tuning` keeps it there.
        let _ = self.audio.control.set_volume(t.volume());
        if self.end_lines.is_some() {
            self.send_end(t);
        }
    }

    fn send_end(&self, t: Tuning) {
        if let Some(lines) = &self.end_lines {
            let mut all = lines.clone();
            all.extend(end_screen::settings_lines(t));
            self.render.send(RenderMsg::Text(all));
        }
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

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

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

    fn take(&mut self) -> Option<Command> {
        let c = self.controls.take();
        if c == Some(Command::Retry) {
            self.pending_retry = self.controls.retry_at();
        }
        c
    }

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
        self.countdown_at = Some(TimeUs(origin.0 - self.lead_in.0));
        println!("play started");
        // Audio was readied before recording opened, so this does not
        // wait; a worker gone since shows as the output's failure.
        let _ = self.audio.control.play(origin, sounds);
        self.render.send(RenderMsg::Play {
            origin,
            side: screen,
            gauge: self.gauge,
        });
    }

    fn focus_lost(&self) -> bool {
        self.input.tripped()
    }

    fn connected(&self) -> bool {
        self.input.connected()
    }

    fn audio_failure(&self) -> Option<OutputError> {
        self.audio.control.failure()
    }

    fn screen_failed(&self) -> bool {
        self.render.failed()
    }

    fn cancel_audio(&mut self) {
        self.audio.control.cancel();
    }

    fn seal_audio(&mut self) {
        self.audio.control.seal();
    }

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
        self.persist();
    }

    fn show_end(&mut self, report: &PlayReport) {
        let tuning = self.controls.tuning();
        tuning.open_end_screen();
        self.end_lines = Some(end_screen::lines(
            report,
            self.input_offset_us,
            self.times,
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
