//! The scene flow (design.md "Play Lifecycle"): song selection → loading →
//! the plays of the chosen chart → song selection, on the game thread.
//!
//! The world around it is an [`AppRig`]: a [`Rig`] that also holds the
//! select screen and the library, builds what a loaded chart needs beside
//! it, and waits idly on menus. The flow only asks it what happened and
//! tells it what to do, as [`crate::live`] does for a play.
//!
//! - A refused load returns to selection with the reason, and the chart
//!   stays selected: a chart or preset file that cannot be used, a bank
//!   that does not load, and a play the logic step refuses at the opening.
//! - Only the screen, audio and input-thread failures [`LiveError`] carries
//!   end the run; closing the window ends it from any scene.
//! - One shelf of presets serves the run (design.md "Tuning Without
//!   Restart"): opened once, polled while selection shows, lent to each
//!   chart and handed back however the chart ends. A shelf that cannot
//!   open refuses every chart and is opened again at each poll.

use std::fmt;
use std::path::{Path, PathBuf};

use bms_model::TimeUs;

use crate::live::{Leave, LiveError, Rig, run_chart, run_over};
use crate::logic::LogicError;
use crate::policy::{SessionEvent, SessionEventSink, SharedEvents};
use crate::presets::{PollDeadline, Shelf, ShelfError, UnknownPreset};
use crate::session::{Loaded, Parts, SessionConfig, SessionError, Settings};
use crate::settings::DataRoot;

/// The longest idle wait on the select screen and a result, on the rig's
/// clock: the flow looks again at least this often.
pub const IDLE_WAIT: TimeUs = TimeUs(250_000);

/// The world the scene flow runs in.
pub trait AppRig: Rig {
    /// Why the sound bank of a chart cannot be built.
    type BankError: std::error::Error + Send + Sync + 'static;

    /// The chart the player chose on the select screen, taken.
    fn choose(&mut self) -> Option<PathBuf>;
    /// Builds what the plays of `loaded` need beside it: its sound bank
    /// and the screen's chart, replacing the previous chart's.
    ///
    /// # Errors
    ///
    /// Why the bank cannot be built: the chart is refused.
    fn load_chart(&mut self, loaded: &Loaded) -> Result<(), Self::BankError>;
    /// Shows on the select screen why the chosen chart was refused.
    fn show_refusal(&mut self, refusal: &LoadRefusal<Self::BankError>);
    /// Song selection was entered: called on every entry, before anything
    /// is shown there.
    fn entered_selection(&mut self);
    /// The shelf, or why it cannot open: called on every entry into
    /// selection, right after [`entered_selection`](Self::entered_selection),
    /// and after every poll there that changed it.
    fn presets_changed(&mut self, shelf: Result<&Shelf, &ShelfError>) {
        let _ = shelf;
    }
}

/// Why a chosen chart was refused.
#[derive(Debug, thiserror::Error)]
pub enum Refused<B> {
    /// The chart or the preset file cannot be used.
    #[error("the chart cannot be loaded")]
    Chart(#[source] SessionError),
    /// The chart's sound bank cannot be built.
    #[error("the chart's sounds cannot be loaded")]
    Bank(#[source] B),
    /// The logic step refused to play the chart, at a play's opening.
    #[error("the chart cannot be played")]
    Play(#[source] LogicError),
}

/// A chosen chart that was refused, and why; it stays selected.
#[derive(Debug)]
pub struct LoadRefusal<B> {
    /// The chart's file.
    pub chart: PathBuf,
    /// Why.
    pub reason: Refused<B>,
}

/// How a chosen chart went: played until the player left it, or refused.
enum Chosen<B> {
    Left(Leave),
    Refused(Refused<B>),
}

/// What the scene flow loads every chart with.
pub struct App {
    root: DataRoot,
    settings: Settings,
    config: SessionConfig,
    parts: Box<dyn FnMut(&Settings) -> Parts>,
    events: SharedEvents,
    /// The run's shelf, or why it cannot open.
    shelf: Result<Shelf, ShelfError>,
    /// When a shelf that cannot open is tried again; an opened shelf keeps
    /// its own deadline.
    reopen: PollDeadline,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("root", &self.root)
            .field("config", &self.config)
            .field("shelf", &self.shelf)
            .finish_non_exhaustive()
    }
}

impl App {
    /// The scene flow under `root`, starting from `settings`; a chart's
    /// plays may change them, and the next chart starts from those. `parts`
    /// makes the extension points of each chart loaded; their session
    /// events go to `events` whatever `parts` sets, as selection's do. The
    /// run's shelf opens here, with the preset `settings` names chosen.
    #[must_use]
    pub fn new(
        root: DataRoot,
        settings: Settings,
        config: SessionConfig,
        parts: Box<dyn FnMut(&Settings) -> Parts>,
        events: Box<dyn SessionEventSink>,
    ) -> Self {
        let shelf = Shelf::open(root.presets(), &settings.player.preset);
        Self {
            root,
            settings,
            config,
            parts,
            events: SharedEvents::new(events),
            shelf,
            reopen: PollDeadline::default(),
        }
    }

    /// The run's shelf, or why it cannot open.
    pub fn shelf(&self) -> Result<&Shelf, &ShelfError> {
        self.shelf.as_ref()
    }

    /// When selection next polls the shelf, or tries to open it again.
    fn next_poll(&self, now: TimeUs) -> TimeUs {
        match &self.shelf {
            Ok(shelf) => shelf.deadline().next(now),
            Err(_) => self.reopen.next(now),
        }
    }

    /// Polls the shelf at `now` if it is due, or opens it again if it
    /// cannot open yet; whether what the rig shows of it changed.
    fn poll(&mut self, now: TimeUs) -> bool {
        match &mut self.shelf {
            Ok(shelf) => {
                let before = shelf.revision();
                let _ = shelf.poll(now);
                shelf.revision() != before
            }
            Err(e) => {
                if !self.reopen.due(now) {
                    return false;
                }
                match Shelf::open(self.root.presets(), &self.settings.player.preset) {
                    Ok(mut shelf) => {
                        // The cadence carries over to the opened shelf.
                        shelf.set_deadline(self.reopen);
                        self.shelf = Ok(shelf);
                        true
                    }
                    Err(again) if again == *e => false,
                    Err(again) => {
                        *e = again;
                        true
                    }
                }
            }
        }
    }

    /// Runs the scene flow from song selection until the window closes.
    ///
    /// # Errors
    ///
    /// The screen, audio or input-thread failure that ended the run; the
    /// plays before it are saved.
    pub fn run<R: AppRig>(&mut self, rig: &mut R) -> Result<(), LiveError<R::ScreenError>> {
        let mut refusal = None;
        'select: loop {
            rig.entered_selection();
            rig.presets_changed(self.shelf.as_ref());
            self.events.on_event(&SessionEvent::Select);
            if let Some(r) = &refusal {
                rig.show_refusal(r);
            }
            let chart = loop {
                run_over(rig)?;
                if rig.quitting() {
                    return Ok(());
                }
                // A chart's controls and a result's commands mean nothing
                // here, and none is kept for the next chart.
                while rig.take().is_some() {}
                while rig.take_menu().is_some() {}
                // The screen is built again, acknowledged, and selection
                // entered anew; if it cannot be, the run ends.
                if rig.screen_failed() {
                    rig.prepare_screen().map_err(LiveError::Screen)?;
                    continue 'select;
                }
                if let Some(chart) = rig.choose() {
                    break chart;
                }
                // The user preset files are polled while selection shows,
                // and the wait ends at the poll deadline at the latest, so a
                // wake just before it does not put the check off.
                let now = rig.now();
                if self.poll(now) {
                    rig.presets_changed(self.shelf.as_ref());
                }
                let deadline = self.next_poll(now);
                rig.idle_wait(TimeUs(now.0.saturating_add(IDLE_WAIT.0).min(deadline.0)));
            };
            refusal = None;
            match self.play_chart(&chart, rig)? {
                Chosen::Left(Leave::Back) => {}
                Chosen::Left(Leave::Quit) => return Ok(()),
                Chosen::Refused(reason) => refusal = Some(LoadRefusal { chart, reason }),
            }
        }
    }

    /// Loads `chart` and plays it until the player leaves it, or refuses
    /// it. The shelf is lent to the chart and comes back on every outcome.
    fn play_chart<R: AppRig>(
        &mut self,
        chart: &Path,
        rig: &mut R,
    ) -> Result<Chosen<R::BankError>, LiveError<R::ScreenError>> {
        let shelf = match &self.shelf {
            Ok(_) => std::mem::replace(&mut self.shelf, Err(lent())),
            Err(e) => Err(e.clone()),
        };
        let shelf = match shelf {
            Ok(shelf) => shelf,
            Err(e) => return Ok(Chosen::Refused(Refused::Chart(SessionError::Preset(e)))),
        };
        // Every chart reports to the run's one sink, never the parts' own.
        let mut loaded = match Loaded::load_reporting_to(
            self.root.clone(),
            self.settings.clone(),
            chart,
            self.config.clone(),
            (self.parts)(&self.settings),
            self.events.clone(),
            shelf,
        ) {
            Ok(loaded) => loaded,
            Err(refused) => {
                let (e, shelf) = *refused;
                self.shelf = Ok(shelf);
                return Ok(Chosen::Refused(Refused::Chart(e)));
            }
        };
        let left = match rig.load_chart(&loaded) {
            Ok(()) => Ok(run_chart(&mut loaded, rig)),
            Err(e) => Err(e),
        };
        self.settings = loaded.settings().clone();
        self.shelf = Ok(loaded.into_shelf());
        let left = match left {
            Ok(left) => left,
            Err(e) => return Ok(Chosen::Refused(Refused::Bank(e))),
        };
        match left {
            Ok(leave) => Ok(Chosen::Left(leave)),
            Err(LiveError::Logic(e)) => Ok(Chosen::Refused(Refused::Play(e))),
            Err(e) => Err(e),
        }
    }
}

/// What the run's shelf holds while a chart has it: never seen, since
/// every way out of [`App::play_chart`] puts the shelf back first.
fn lent() -> ShelfError {
    ShelfError::Unknown(UnknownPreset(String::new()))
}
