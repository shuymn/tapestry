//! The scene flow (design.md "Play Lifecycle"): song selection → loading →
//! the plays of the chosen chart → song selection, on the game thread.
//!
//! The world around it is an [`AppRig`]: a [`Rig`] that also holds the
//! select screen and the library, builds what a loaded chart needs beside
//! it, and waits idly on menus. The flow only asks it what happened and
//! tells it what to do, as [`crate::live`] does for a play.
//!
//! - A chart chosen to play, and each practice Start, switches to the play
//!   screen in its loading state at once ([`AppRig::entered_loading`]),
//!   before anything is read; it lasts until the countdown or the way back.
//! - A refused load returns to selection with the reason, and the chart
//!   stays selected: a chart or preset file that cannot be used, a bank
//!   that does not load, a play whose attempt the screen refuses to
//!   prepare, and a play the logic step refuses at the opening. A practice
//!   Start shows the same refusals on its page.
//! - Only the screen, audio and input-thread failures [`LiveError`] carries
//!   end the run; closing the window ends it from any scene.
//! - One shelf of presets serves the run (design.md "Tuning Without
//!   Restart"): opened once, polled while selection shows, lent to each
//!   chart and handed back however the chart ends. A shelf that cannot
//!   open refuses every chart and is opened again at each poll.

use std::fmt;
use std::path::{Path, PathBuf};

use bms_model::TimeUs;

use bms_model::Speed;

use crate::attempt::PreparationRefusal;
use crate::error::Chain;
use crate::live::{Leave, LiveError, Rig, run_chart, run_over, threads_over};
use crate::logic::LogicError;
use crate::policy::{SessionEvent, SessionEventSink, SharedEvents};
use crate::practice::PracticeError;
use crate::practice_screen::{PracticeActed, PracticeInput, PracticeScreen};
use crate::presets::{PollDeadline, PresetStep, Shelf, ShelfError, UnknownPreset};
use crate::session::Command;
use crate::session::{Loaded, Parts, SessionConfig, SessionError, Settings};
use crate::settings::{DataRoot, PlayerSettings};

/// The longest idle wait on the select screen and a result, on the rig's
/// clock: the flow looks again at least this often.
pub const IDLE_WAIT: TimeUs = TimeUs(250_000);

/// The world the scene flow runs in.
pub trait AppRig: Rig {
    /// Why the sound bank of a chart cannot be built.
    type BankError: std::error::Error + Send + Sync + 'static;

    /// The chart the player chose on the select screen, taken, and
    /// whether to play or practice it.
    fn choose(&mut self) -> Option<Pick>;
    /// Builds what the plays of `loaded` need beside it: its sound bank
    /// and the screen's chart, replacing the previous chart's; or stops at
    /// a cancel ([`Loading::Cancelled`]), and the chart is left.
    ///
    /// # Errors
    ///
    /// Why the bank cannot be built: the chart is refused.
    fn load_chart(&mut self, loaded: &Loaded) -> Result<Loading, Self::BankError>;
    /// A chart's load for play began: the play screen shows in its loading
    /// state from here, before anything of `chart` is read, until the
    /// play's countdown or a return to where the chart was chosen
    /// (selection with a refusal or after a cancel, the practice page), or
    /// the run's end. Called at every chosen chart to play and at every
    /// practice Start, never at a retry, which loads nothing.
    fn entered_loading(&mut self, chart: &Path);
    /// Shows on the select screen why the chosen chart was refused.
    fn show_refusal(&mut self, refusal: &LoadRefusal<Self::BankError>);
    /// Song selection was entered: called on every entry, before anything
    /// is shown there.
    fn entered_selection(&mut self);
    /// The shelf, or why it cannot open: called on every entry into
    /// selection, right after [`entered_selection`](Self::entered_selection),
    /// and after every poll or batch of preset steps there that changed
    /// it.
    fn presets_changed(&mut self, shelf: Result<&Shelf, &ShelfError>) {
        let _ = shelf;
    }
    /// The chosen chart's session choices and parts, before it loads
    /// (dev-tools): a development scenario chooses each chart's input. The
    /// default keeps them.
    #[cfg(feature = "dev-tools")]
    fn configure(&mut self, config: &mut SessionConfig, parts: &mut Parts) {
        let _ = (config, parts);
    }
    /// The practice screen was entered, or returned to: called when it
    /// opens and after every return to it (a refused Start, the rounds
    /// left, a rebuilt screen), before anything is read there. The model
    /// is the one built when it opened.
    fn entered_practice(&mut self, screen: &PracticeScreen) {
        let _ = screen;
    }
    /// What the practice screen shows changed.
    fn show_practice(&mut self, screen: &PracticeScreen) {
        let _ = screen;
    }
    /// The next input on the practice screen, taken.
    fn take_practice(&mut self) -> Option<PracticeInput> {
        None
    }
    /// The next step of the play preset taken on the select screen, in
    /// the order taken: those before a chosen chart, taken by the same
    /// [`choose`](Self::choose), apply to it.
    fn take_preset_step(&mut self) -> Option<PresetStep> {
        None
    }
}

/// A chart chosen on the select screen, and what to do with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Play it.
    Play(PathBuf),
    /// Open its practice screen.
    Practice(PathBuf),
}

/// How a chart's load ([`AppRig::load_chart`]) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loading {
    /// The chart is ready to play.
    Ready,
    /// The load stopped before it was ready: the window is closing, or
    /// the quit key left the chart. Nothing further is prepared, and the
    /// chart is left.
    Cancelled,
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
    /// The chart cannot be practiced (an autoplay session).
    #[error("the chart cannot be practiced")]
    Practice(#[source] PracticeError),
    /// The screen refused to prepare a play of the chart, and stays
    /// usable for selection.
    #[error("the play cannot be prepared")]
    Preparation(#[source] PreparationRefusal),
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

/// How a chosen chart went on rig `R`, or the failure that ends the run.
type Went<R> = Result<Chosen<<R as AppRig>::BankError>, LiveError<<R as Rig>::ScreenError>>;

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

    /// Applies the preset steps the rig took (design.md "Play Lifecycle");
    /// whether what the rig shows of the shelf changed. Each step checks
    /// the shelf and chooses the previous or next entry that loads; a step
    /// while the shelf cannot open opens it on the default preset. The
    /// chosen name is the one the next chart loads with, and the one a
    /// shelf that cannot open is opened with again.
    fn step_presets<R: AppRig>(&mut self, rig: &mut R) -> bool {
        let mut changed = false;
        while let Some(step) = rig.take_preset_step() {
            changed |= match &mut self.shelf {
                Ok(shelf) => {
                    let before = shelf.revision();
                    shelf.step(step);
                    shelf.revision() != before
                }
                Err(_) => {
                    let default = PlayerSettings::default().preset;
                    match Shelf::open(self.root.presets(), &default) {
                        Ok(mut shelf) => {
                            // The cadence carries over, as at a reopen.
                            shelf.set_deadline(self.reopen);
                            self.shelf = Ok(shelf);
                            true
                        }
                        // A built-in opens whatever the folder holds.
                        Err(_) => false,
                    }
                }
            };
        }
        if let Ok(shelf) = &self.shelf
            && shelf.chosen_name() != self.settings.player.preset
        {
            self.settings.player.preset = shelf.chosen_name().to_owned();
        }
        changed
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
            let pick = loop {
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
                // Steps taken before a choice apply to the chart chosen.
                let pick = rig.choose();
                let stepped = self.step_presets(rig);
                if let Some(pick) = pick {
                    if stepped {
                        rig.presets_changed(self.shelf.as_ref());
                    }
                    break pick;
                }
                // The user preset files are polled while selection shows,
                // and the wait ends at the poll deadline at the latest, so a
                // wake just before it does not put the check off.
                let now = rig.now();
                if self.poll(now) | stepped {
                    // A refusal for the shelf that would not open is over
                    // once it opens: it is not shown again.
                    if self.shelf.is_ok()
                        && refusal.as_ref().is_some_and(|r| {
                            matches!(r.reason, Refused::Chart(SessionError::Preset(_)))
                        })
                    {
                        refusal = None;
                    }
                    rig.presets_changed(self.shelf.as_ref());
                }
                let deadline = self.next_poll(now);
                rig.idle_wait(TimeUs(now.0.saturating_add(IDLE_WAIT.0).min(deadline.0)));
            };
            refusal = None;
            let (chart, practice) = match pick {
                Pick::Play(chart) => (chart, false),
                Pick::Practice(chart) => (chart, true),
            };
            match self.play_chart(&chart, practice, rig)? {
                Chosen::Left(Leave::Back) => {}
                Chosen::Left(Leave::Quit) => return Ok(()),
                // A refusal never hides a thread's failure: the run ends
                // with it instead.
                Chosen::Refused(reason) => {
                    threads_over(rig)?;
                    refusal = Some(LoadRefusal { chart, reason });
                }
            }
        }
    }

    /// Loads `chart` and plays it, or opens its practice screen with
    /// `practice`, until the player leaves it, or refuses it. The shelf is
    /// lent to the chart and comes back on every outcome.
    fn play_chart<R: AppRig>(
        &mut self,
        chart: &Path,
        practice: bool,
        rig: &mut R,
    ) -> Result<Chosen<R::BankError>, LiveError<R::ScreenError>> {
        let shelf = match std::mem::replace(&mut self.shelf, Err(lent())) {
            Ok(shelf) => shelf,
            Err(e) => {
                self.shelf = Err(e.clone());
                return Ok(Chosen::Refused(Refused::Chart(SessionError::Preset(e))));
            }
        };
        // The one way back for the shelf, whatever the chart did.
        let (shelf, chosen) = self.play_on(shelf, chart, practice, rig);
        self.shelf = Ok(shelf);
        chosen
    }

    /// [`play_chart`](Self::play_chart) on the lent `shelf`, which comes
    /// back with how the chart went.
    fn play_on<R: AppRig>(
        &mut self,
        shelf: Shelf,
        chart: &Path,
        practice: bool,
        rig: &mut R,
    ) -> (Shelf, Went<R>) {
        let mut config = self.config.clone();
        #[cfg_attr(
            not(feature = "dev-tools"),
            expect(unused_mut, reason = "the hook changes them")
        )]
        let mut parts = (self.parts)(&self.settings);
        #[cfg(feature = "dev-tools")]
        rig.configure(&mut config, &mut parts);
        if practice {
            // An autoplay session plays whole charts only.
            if config.autoplay.is_some() {
                return (
                    shelf,
                    Ok(Chosen::Refused(Refused::Practice(PracticeError::Autoplay))),
                );
            }
            // The screen's measures are the chart's 1× times.
            config.speed = Speed::NORMAL;
        } else {
            // The loading state shows before the chart is read; a practice
            // shows it at each Start instead.
            rig.entered_loading(chart);
        }
        // Every chart reports to the run's one sink, never the parts' own.
        let mut loaded = match Loaded::load_reporting_to(
            self.root.clone(),
            self.settings.clone(),
            chart,
            config,
            parts,
            self.events.clone(),
            shelf,
        ) {
            Ok(loaded) => loaded,
            Err(refused) => {
                let (e, shelf) = *refused;
                return (shelf, Ok(Chosen::Refused(Refused::Chart(e))));
            }
        };
        let chosen = if practice {
            practice_chart(&mut loaded, rig).map(Chosen::Left)
        } else {
            match rig.load_chart(&loaded) {
                Err(e) => Ok(Chosen::Refused(Refused::Bank(e))),
                Ok(Loading::Cancelled) => cancelled(rig).map(Chosen::Left),
                Ok(Loading::Ready) => match run_chart(&mut loaded, rig) {
                    Ok(leave) => Ok(Chosen::Left(leave)),
                    Err(LiveError::Logic(e)) => Ok(Chosen::Refused(Refused::Play(e))),
                    Err(LiveError::Preparation(e)) => Ok(Chosen::Refused(Refused::Preparation(e))),
                    Err(e) => Err(e),
                },
            }
        };
        self.settings = loaded.settings().clone();
        (loaded.into_shelf(), chosen)
    }
}

/// The practice screen of `loaded`, loaded at 1×, until the player leaves
/// it (design.md "Play Lifecycle"): it waits idly on the page, and each
/// Start reads the chart again at the chosen speed, sets the practice up,
/// loads the bank and the screen's chart, and plays its rounds. A refusal
/// on the way, or at a round's opening, shows on the page, which stays;
/// leaving the rounds returns to it. The model is built once, here, and
/// every return to the page enters it again ([`AppRig::entered_practice`]).
///
/// # Errors
///
/// The screen, audio or input-thread failure that ends the run.
fn practice_chart<R: AppRig>(
    loaded: &mut Loaded,
    rig: &mut R,
) -> Result<Leave, LiveError<R::ScreenError>> {
    let mut screen = PracticeScreen::new(
        loaded.chart(),
        loaded.preset(),
        loaded.settings().player.displayed_gauge,
    );
    rig.entered_practice(&screen);
    loop {
        run_over(rig)?;
        if rig.quitting() {
            return Ok(Leave::Quit);
        }
        // The quit key goes back; the other controls and a result's
        // commands mean nothing here.
        let mut back = false;
        while let Some(c) = rig.take() {
            back |= c == Command::Quit;
        }
        while rig.take_menu().is_some() {}
        if back {
            return Ok(Leave::Back);
        }
        if rig.screen_failed() {
            rig.prepare_screen().map_err(LiveError::Screen)?;
            rig.entered_practice(&screen);
            continue;
        }
        let mut dirty = false;
        let mut start = false;
        while let Some(input) = rig.take_practice() {
            match screen.input(input) {
                PracticeActed::Nothing => {}
                PracticeActed::Changed => dirty = true,
                PracticeActed::Start => {
                    start = true;
                    break;
                }
                PracticeActed::Back => return Ok(Leave::Back),
            }
        }
        if start {
            // The play preset changes only as a round begins: the page
            // follows it by gauge ID once the rounds return, so what it
            // shows is what the next round plays.
            let ran = start_rounds(loaded, &screen, rig);
            let fitted = screen.fit(loaded.preset());
            let notice = match ran {
                Ok(Leave::Quit) => return Ok(Leave::Quit),
                Ok(Leave::Back) => None,
                // A refusal never hides a thread's failure.
                Err(Stopped::Notice(n)) => {
                    threads_over(rig)?;
                    Some(n)
                }
                Err(Stopped::Run(e)) => return Err(e),
            };
            // What the fit reset follows the Start's own notice.
            let notice = match (notice, fitted) {
                (Some(n), Some(f)) => Some(format!("{n}; {f}")),
                (n, f) => n.or(f),
            };
            screen.set_notice(notice);
            rig.entered_practice(&screen);
            continue;
        }
        if dirty {
            rig.show_practice(&screen);
        }
        let now = rig.now();
        rig.idle_wait(TimeUs(now.0.saturating_add(IDLE_WAIT.0)));
    }
}

/// How a Start that did not end in leaving the rounds went.
enum Stopped<S> {
    /// It was refused, and why, for the page.
    Notice(String),
    /// The run ends.
    Run(LiveError<S>),
}

/// One Start of the practice screen: the chart read again at the chosen
/// speed, the practice set up, the bank and the screen's chart loaded
/// (every time, so no bank of another chart or speed is reused), and the
/// rounds played until they are left.
fn start_rounds<R: AppRig>(
    loaded: &mut Loaded,
    screen: &PracticeScreen,
    rig: &mut R,
) -> Result<Leave, Stopped<R::ScreenError>> {
    let notice = |e: &(dyn std::error::Error + 'static)| Stopped::Notice(Chain(e).to_string());
    rig.entered_loading(loaded.path());
    if loaded.config().speed != screen.speed() {
        loaded.set_speed(screen.speed()).map_err(|e| notice(&e))?;
    }
    let practice = screen.practice(loaded);
    loaded
        .set_practice(Some(practice))
        .map_err(|e| notice(&e))?;
    if rig.load_chart(loaded).map_err(|e| notice(&e))? == Loading::Cancelled {
        return cancelled(rig).map_err(Stopped::Run);
    }
    match run_chart(loaded, rig) {
        Ok(leave) => Ok(leave),
        Err(LiveError::Logic(e)) => Err(notice(&e)),
        Err(LiveError::Restore(e)) => Err(notice(&e)),
        Err(LiveError::Preparation(e)) => Err(notice(&e)),
        Err(e) => Err(Stopped::Run(e)),
    }
}

/// A load the rig cancelled: the quit that cancelled it is taken with it,
/// so it leaves only the load, and nothing more is prepared (no screen or
/// audio handshake). A close ends the run; the quit key returns to where
/// the chart was chosen.
fn cancelled<R: AppRig>(rig: &mut R) -> Result<Leave, LiveError<R::ScreenError>> {
    run_over(rig)?;
    while rig.take().is_some() {}
    Ok(if rig.quitting() {
        Leave::Quit
    } else {
        Leave::Back
    })
}

/// What the run's shelf holds while a chart has it: never seen, since
/// every way out of [`App::play_chart`] puts the shelf back first.
fn lent() -> ShelfError {
    ShelfError::Unknown(UnknownPreset(String::new()))
}
