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
//!   before anything is read; it lasts until READY or the way back.
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
use crate::coord::{Caps, Coordinator, JobKind, Joined, LoadFailure, Spawner};
use crate::decision::{Decision, Origin, Reply, Verdict};
use crate::error::Chain;
use crate::live::{Leave, LiveError, Rig, run_over, threads_over};
use crate::logic::LogicError;
use crate::opening::{LoadingRig, load_and_open};
use crate::policy::{SessionEvent, SessionEventSink, SharedEvents};
use crate::practice::PracticeError;
use crate::practice_screen::{PracticeActed, PracticeInput, PracticeScreen};
use crate::presets::{PollDeadline, PresetStep, Shelf, ShelfError, UnknownPreset};
use crate::session::Command;
use crate::session::{
    Loaded, Parts, PendingLoad, SessionConfig, SessionError, Settings, read_for_load,
};
use crate::settings::{DataRoot, PlayerSettings};

/// The longest idle wait on the select screen and a result, on the rig's
/// clock: the flow looks again at least this often.
pub const IDLE_WAIT: TimeUs = TimeUs(250_000);

/// How often a game thread waiting for a load's job asks whether to stop
/// it.
pub const LOAD_SLICE: std::time::Duration = std::time::Duration::from_millis(1);

/// The world the scene flow runs in. A chart's first attempt after its
/// load opens out of the loading state ([`load_and_open`]), so the rig is
/// a [`LoadingRig`].
pub trait AppRig: LoadingRig {
    /// Why the sound bank of a chart cannot be built.
    type BankError: std::error::Error + Send + Sync + 'static;

    /// The chart the player chose on the select screen, taken, and
    /// whether to play or practice it.
    fn choose(&mut self) -> Option<Pick>;
    /// Builds what the plays of `loaded` need beside it: its sound bank
    /// and the screen's chart, replacing the previous chart's; or stops at
    /// a cancel ([`Loading::Cancelled`]), and the chart is left. Its waits
    /// serve `decision` ([`Decision::classify`] from [`Origin::Load`]): a
    /// deadline met inside one steps there, and a wait it ends returns
    /// [`Loading::Cancelled`] with [`Decision::leave`] set.
    ///
    /// # Errors
    ///
    /// Why the bank cannot be built: the chart is refused.
    fn load_chart(
        &mut self,
        loaded: &Loaded,
        decision: &mut Decision,
    ) -> Result<Loading, Self::BankError>;
    /// Whether a load in progress is to stop: the window closing, or a
    /// quit key press waiting to be taken. Asked while the game thread waits
    /// for a load's job, never taking the press; by default the latched
    /// [`quitting`](Rig::quitting).
    fn load_cancelled(&self) -> bool {
        self.quitting()
    }
    /// A chart's load for play began: the play screen shows in its loading
    /// state from here, before anything of `chart` is read, until the
    /// play's READY state or a return to where the chart was chosen
    /// (selection with a refusal or after a cancel, the practice page), or
    /// the run's end. Called at every chosen chart to play and at every
    /// practice Start, never at a retry, which loads nothing.
    fn entered_loading(&mut self, chart: &Path);
    /// Shows on the select screen why the chosen chart was refused.
    fn show_refusal(&mut self, refusal: &LoadRefusal<Self::BankError>);
    /// Song selection was entered: called on every entry, before anything
    /// is shown there.
    fn entered_selection(&mut self);
    /// The screen failed while selection showed and was built again: the
    /// same entry goes on, so what it started (its timers, a skin's state)
    /// is kept and only what the new screen draws is sent again. Called
    /// instead of [`entered_selection`](Self::entered_selection); the
    /// default enters selection anew.
    fn recovered_selection(&mut self) {
        self.entered_selection();
    }
    /// The shelf, or why it cannot open: called on every entry into
    /// selection, right after [`entered_selection`](Self::entered_selection)
    /// or [`recovered_selection`](Self::recovered_selection),
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
    /// The chart chosen to practice was read at 1× (`loaded`), before its
    /// practice page opens: the rig may begin its page jobs (design.md
    /// "State Model", the practice page). Nothing by default.
    ///
    /// # Errors
    ///
    /// Why the chart cannot be practised: a refused load, which returns to
    /// selection with its reason. [`Loading::Failed`] ends the run.
    fn practice_read(
        &mut self,
        loaded: &Loaded,
        decision: &mut Decision,
    ) -> Result<Loading, Self::BankError> {
        let _ = (loaded, decision);
        Ok(Loading::Ready)
    }
    /// The decision screen's time for the choice about to be made; none
    /// (by default) shows no decision screen.
    fn decision_time(&self) -> Option<TimeUs> {
        None
    }
    /// The decision screen begins for `pick` (design.md "Play Lifecycle",
    /// the decision screen): its presentation and tone, its input epoch,
    /// and the attempt's reset at the choice, whose wait serves `d`: a
    /// leaving verdict a look returned there is left in `d`
    /// ([`Decision::leave`]) for App to route before the chart is read.
    /// Nothing by default.
    fn entered_decision(&mut self, pick: &Pick, d: &mut Decision) {
        let _ = (pick, d);
    }
    /// Each look of the decision screen from `origin`, before its
    /// classification: what needs no wait (the page's jobs in
    /// [`Origin::Jobs`]). Nothing by default.
    ///
    /// # Errors
    ///
    /// A failure the run ends with.
    fn decision_tick(&mut self, origin: Origin) -> Result<(), LiveError<Self::ScreenError>> {
        let _ = origin;
        Ok(())
    }
    /// The play's deadline: its loading state shown, its input epoch over.
    /// Waits for no acknowledgement. Nothing by default.
    fn deadline_step(&mut self) {}
    /// The decision screen is left for selection or the practice page: its
    /// input epoch ends. Nothing by default.
    fn closed_epoch(&mut self) {}
    /// Past the deadline while practice waits for its read: whether Back
    /// or the quit key was pressed at or after `deadline`, as its press
    /// was observed, taken; one observed before it, whenever it is taken,
    /// is not. Never by default.
    fn decision_escape(&mut self, deadline: TimeUs) -> bool {
        let _ = deadline;
        false
    }
    /// The screen lost during the decision screen: its rebuild begins,
    /// one at a time, answered through
    /// [`poll_recovery`](Self::poll_recovery). The rig's render side
    /// restores the presentation of the state it is in once the screen is
    /// built again. Nothing by default: the default poll rebuilds the
    /// screen there.
    fn begin_recovery(&mut self) {}
    /// After [`load_chart`](crate::LoadingRig::load_chart) returned an
    /// error: the screen's failure it hid, if the chart's screen load lost
    /// the device or the surface twice ([`LiveError::Screen`], not a
    /// refusal), taken. None by default.
    fn screen_load_failed(&mut self) -> Option<Self::ScreenError> {
        None
    }
    /// The rebuild [`begin_recovery`](Self::begin_recovery) began, waited
    /// for until `until` at most: `None` while it runs, its answer once it
    /// came, or the error of its bound. By default the screen is built
    /// again here, waiting for it ([`Rig::prepare_screen`]).
    ///
    /// [`Rig::prepare_screen`]: crate::Rig::prepare_screen
    fn poll_recovery(&mut self, until: TimeUs) -> Option<Result<(), Self::ScreenError>> {
        let _ = until;
        Some(self.prepare_screen())
    }
    /// Each pass of the practice page, whatever the input: the rig
    /// advances its page jobs. Nothing by default.
    ///
    /// # Errors
    ///
    /// A failure of a page job the run ends with.
    fn poll_page(&mut self) -> Result<(), LiveError<Self::ScreenError>> {
        Ok(())
    }
    /// The chart chosen is left (Back, a refusal), before selection is
    /// entered again: the rig settles what it kept of it. Nothing by
    /// default.
    ///
    /// # Errors
    ///
    /// A failure found in what it settled that the run ends with.
    fn left_chart(&mut self) -> Result<(), LiveError<Self::ScreenError>> {
        Ok(())
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
    /// A Start was taken on the practice page, before anything of it
    /// begins: whether the play screen can show the chart's loading state
    /// now. A notice instead keeps the page, with it, and admits nothing;
    /// the page's other controls go on as before. Asked at every Start,
    /// never blocking. The default is always ready.
    ///
    /// # Errors
    ///
    /// The notice to show on the practice page.
    fn start_ready(&mut self) -> Result<(), String> {
        Ok(())
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loading {
    /// The chart is ready to play.
    Ready,
    /// The load stopped before it was ready: the window is closing, or
    /// the quit key left the chart. Nothing further is prepared, and the
    /// chart is left.
    Cancelled,
    /// A job of the load could not start its thread or panicked: the run
    /// ends with it, never as a refusal (design.md "Play Lifecycle", who
    /// loads).
    Failed(LoadFailure),
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
    /// The run's coordinator, whose foreground permits a chart's read
    /// takes.
    coord: Coordinator,
    /// Starts the load's job threads.
    spawner: Spawner,
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
            coord: Coordinator::new(Caps::for_machine(0, 0)),
            spawner: Spawner::SYSTEM,
        }
    }

    /// The flow on the run's coordinator `coord`, in place of one of its
    /// own with no speculation.
    #[must_use]
    pub fn with_coordinator(mut self, coord: Coordinator) -> Self {
        self.coord = coord;
        self
    }

    /// The flow starting the load's threads through `spawner` (tests stand
    /// in a refusal).
    #[doc(hidden)]
    #[must_use]
    pub const fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
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
        let mut recovered = false;
        'select: loop {
            if std::mem::take(&mut recovered) {
                rig.recovered_selection();
            } else {
                rig.entered_selection();
            }
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
                // shown on it again within the same entry; if it cannot
                // be, the run ends.
                if rig.screen_failed() {
                    rig.prepare_screen().map_err(LiveError::Screen)?;
                    recovered = true;
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
            let (chart, practice) = match &pick {
                Pick::Play(chart) => (chart.clone(), false),
                Pick::Practice(chart) => (chart.clone(), true),
            };
            // What the chart loads with, and autoplay's P refused on the
            // select screen, before any decision screen (design.md "Play
            // Lifecycle", the decision screen).
            let mut decision = Decision::off();
            let went = match self.session_for(practice, rig) {
                Err(reason) => Chosen::Refused(reason),
                Ok(session) => {
                    decision = Decision::begin(rig.now(), rig.decision_time(), practice);
                    if decision.on() {
                        rig.entered_decision(&pick, &mut decision);
                    }
                    // A look inside the choice's own wait left the screen.
                    match decision.take_left() {
                        Some(left) => left_by(Ok(next_of(left)), rig)?,
                        None => self.play_chart(&chart, practice, session, &mut decision, rig)?,
                    }
                }
            };
            match went {
                Chosen::Left(Leave::Back) => {
                    if decision.on() {
                        rig.closed_epoch();
                    }
                    rig.left_chart()?;
                }
                Chosen::Left(Leave::Quit) => return Ok(()),
                // A refusal never hides a thread's failure: the run ends
                // with it instead. The decision screen holds it to its
                // deadline.
                Chosen::Refused(reason) => {
                    if decision.on()
                        && !decision.past(rig.now())
                        && hold_refusal(&mut decision, rig)? == Next::End
                        && ended(rig)? == Leave::Quit
                    {
                        return Ok(());
                    }
                    if decision.on() {
                        rig.closed_epoch();
                    }
                    rig.left_chart()?;
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
        session: (SessionConfig, Parts),
        decision: &mut Decision,
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
        let (shelf, chosen) = self.play_on(shelf, chart, practice, session, decision, rig);
        self.shelf = Ok(shelf);
        chosen
    }

    /// What a chart chosen to play (or practice, with `practice`) is
    /// loaded with: the session's configuration and parts.
    ///
    /// # Errors
    ///
    /// Autoplay's P, refused on the select screen.
    fn session_for<R: AppRig>(
        &mut self,
        practice: bool,
        rig: &mut R,
    ) -> Result<(SessionConfig, Parts), Refused<R::BankError>> {
        let mut config = self.config.clone();
        #[cfg_attr(
            not(feature = "dev-tools"),
            expect(unused_mut, reason = "the hook changes them")
        )]
        let mut parts = (self.parts)(&self.settings);
        #[cfg(feature = "dev-tools")]
        rig.configure(&mut config, &mut parts);
        #[cfg(not(feature = "dev-tools"))]
        let _ = rig;
        if practice {
            // An autoplay session plays whole charts only.
            if config.autoplay.is_some() {
                return Err(Refused::Practice(PracticeError::Autoplay));
            }
            // The screen's measures are the chart's 1× times.
            config.speed = Speed::NORMAL;
        }
        Ok((config, parts))
    }

    /// [`play_chart`](Self::play_chart) on the lent `shelf`, which comes
    /// back with how the chart went.
    fn play_on<R: AppRig>(
        &mut self,
        shelf: Shelf,
        chart: &Path,
        practice: bool,
        (config, parts): (SessionConfig, Parts),
        d: &mut Decision,
        rig: &mut R,
    ) -> (Shelf, Went<R>) {
        rig.play_state_began();
        if !practice && !d.on() {
            // The loading state shows before the chart is read; a practice
            // shows it at each Start instead, and with a decision screen it
            // shows at its deadline.
            rig.entered_loading(chart);
        }
        // Every chart reports to the run's one sink, never the parts' own.
        let pending = match PendingLoad::new(
            self.root.clone(),
            self.settings.clone(),
            config,
            parts,
            self.events.clone(),
            shelf,
        ) {
            Ok(pending) => pending,
            Err(refused) => {
                let (e, shelf) = *refused;
                return (shelf, Ok(Chosen::Refused(Refused::Chart(e))));
            }
        };
        // Read on its own thread while this one looks for a quit or a
        // close (design.md "Play Lifecycle", who loads), or serves the
        // decision screen, which records why it stopped the read.
        let mut why = None;
        let read = {
            let (ingest, transform) = pending.reader();
            let config = pending.config();
            let job = (JobKind::Ingest, chart.to_owned(), config.speed);
            std::thread::scope(|s| {
                match self.coord.run_scoped(s, self.spawner, job, |_| {
                    read_for_load(chart, config, ingest, transform)
                }) {
                    Err(failure) => Joined::Failed(failure),
                    Ok(job) if d.on() => {
                        job.wait(LOAD_SLICE, || match serve(d, rig, Origin::Read, None) {
                            Ok(Next::Go | Next::Stepped) => false,
                            other => {
                                why = Some(other);
                                true
                            }
                        })
                    }
                    Ok(job) => job.wait(LOAD_SLICE, || rig.load_cancelled()),
                }
            })
        };
        let read = match read {
            Joined::Done(read) => read,
            Joined::Cancelled(_) => {
                let left = match why {
                    None => cancelled(rig).map(Chosen::Left),
                    Some(next) => left_by(next, rig),
                };
                return (pending.into_shelf(), left);
            }
            Joined::Failed(failure) => {
                return (pending.into_shelf(), Err(LiveError::Load(failure)));
            }
        };
        let finished = pending.finish(chart, read);
        // The read's reply in hand, a refusal included, is looked at in one
        // classification before anything acts on it: a refusal with the
        // deadline due returns without the loading state.
        if d.on() {
            let reply = finished.is_err().then_some(Reply::Refusal);
            match serve(d, rig, Origin::Read, reply) {
                Ok(Next::Go | Next::Stepped | Next::Return) => {}
                next => {
                    let shelf = match finished {
                        Ok(loaded) => loaded.into_shelf(),
                        Err(refused) => refused.1,
                    };
                    return (shelf, left_by(next, rig));
                }
            }
        }
        let mut loaded = match finished {
            Ok(loaded) => loaded,
            Err(refused) => {
                let (e, shelf) = *refused;
                return (shelf, Ok(Chosen::Refused(Refused::Chart(e))));
            }
        };
        let chosen = if practice {
            match rig.practice_read(&loaded, d) {
                Err(e) => Ok(Chosen::Refused(Refused::Bank(e))),
                Ok(Loading::Failed(failure)) => Err(LiveError::Load(failure)),
                Ok(Loading::Cancelled) => match d.take_left() {
                    Some(left) => left_by(Ok(next_of(left)), rig),
                    None => cancelled(rig).map(Chosen::Left),
                },
                Ok(Loading::Ready) => match d.take_left() {
                    Some(left) => left_by(Ok(next_of(left)), rig),
                    // The page opens at the deadline, its epoch over.
                    None => match page_entry(d, rig) {
                        Ok(true) => practice_chart(&mut loaded, rig, &self.coord, self.spawner)
                            .map(Chosen::Left),
                        Ok(false) => ended(rig).map(Chosen::Left),
                        Err(e) => Err(e),
                    },
                },
            }
        } else {
            match rig.load_chart(&loaded, d) {
                // A screen lost in the chart's load is the screen's
                // failure, which ends the run; anything else is refused.
                Err(e) => match rig.screen_load_failed() {
                    Some(lost) => Err(LiveError::Screen(lost)),
                    None => Ok(Chosen::Refused(Refused::Bank(e))),
                },
                Ok(Loading::Cancelled) => match d.take_left() {
                    Some(left) => left_by(Ok(next_of(left)), rig),
                    None => cancelled(rig).map(Chosen::Left),
                },
                Ok(Loading::Failed(failure)) => Err(LiveError::Load(failure)),
                // The loading state opens only after the deadline's step.
                Ok(Loading::Ready) => match ready_at_deadline(d, rig) {
                    Ok(false) => ended(rig).map(Chosen::Left),
                    Err(e) => Err(e),
                    Ok(true) => match load_and_open(&mut loaded, rig) {
                        Ok(leave) => Ok(Chosen::Left(leave)),
                        Err(LiveError::Logic(e)) => Ok(Chosen::Refused(Refused::Play(e))),
                        Err(LiveError::Preparation(e)) => {
                            Ok(Chosen::Refused(Refused::Preparation(e)))
                        }
                        Err(e) => Err(e),
                    },
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
    coord: &Coordinator,
    spawner: Spawner,
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
        // The page's jobs go on with no input.
        rig.poll_page()?;
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
        if start && let Err(n) = rig.start_ready() {
            screen.set_notice(Some(n));
            rig.entered_practice(&screen);
            continue;
        }
        if start {
            // The play preset changes only at a Start and as a round
            // begins: once this Start's practice was fitted to it, the page
            // shows what the last fit gave, so what it shows is what the
            // next round plays.
            let mut set_up = false;
            let ran = start_rounds(loaded, &screen, rig, &mut set_up, (coord, spawner));
            // Back on the page: a turntable paused by a round's result
            // resumes.
            rig.turntable_resumed();
            if set_up {
                screen.sync(loaded);
            }
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

/// One Start of the practice screen: the chart read again when the chosen
/// speed is another (on a job's thread, as a choice's read), the practice
/// set up, the bank and the screen's chart loaded (every time, so no bank
/// of another chart or speed is reused), and the rounds played until they
/// are left. `set_up` turns true once the screen's practice was fitted to
/// the refreshed play preset ([`Loaded::start_practice`]).
fn start_rounds<R: AppRig>(
    loaded: &mut Loaded,
    screen: &PracticeScreen,
    rig: &mut R,
    set_up: &mut bool,
    (coord, spawner): (&Coordinator, Spawner),
) -> Result<Leave, Stopped<R::ScreenError>> {
    let notice = |e: &(dyn std::error::Error + 'static)| Stopped::Notice(Chain(e).to_string());
    rig.entered_loading(loaded.path());
    let speed = screen.speed();
    if loaded.config().speed != speed {
        let read = {
            let reader = loaded.reader();
            let job = (JobKind::Ingest, loaded.path().to_owned(), speed);
            std::thread::scope(|s| {
                match coord.run_scoped(s, spawner, job, |_| reader.read_with(speed)) {
                    Err(failure) => Joined::Failed(failure),
                    Ok(job) => job.wait(LOAD_SLICE, || rig.load_cancelled()),
                }
            })
        };
        match read {
            Joined::Done(read) => loaded.apply_speed(speed, read).map_err(|e| notice(&e))?,
            Joined::Cancelled(_) => return cancelled(rig).map_err(Stopped::Run),
            Joined::Failed(failure) => return Err(Stopped::Run(LiveError::Load(failure))),
        }
    }
    let practice = screen.practice(loaded);
    let started = loaded.start_practice(practice);
    *set_up = true;
    started.map_err(|e| Stopped::Notice(e.to_string()))?;
    let load = rig.load_chart(loaded, &mut Decision::off());
    // A Start's failed load is the page's notice, the screen's failure it
    // hid included, with its cause: taken here, so no later load reports
    // it.
    let lost = rig.screen_load_failed();
    match load.map_err(|e| match &lost {
        Some(lost) => Stopped::Notice(format!("{}: {}", Chain(&e), Chain(lost))),
        None => notice(&e),
    })? {
        Loading::Ready => {}
        Loading::Cancelled => return cancelled(rig).map_err(Stopped::Run),
        Loading::Failed(failure) => return Err(Stopped::Run(LiveError::Load(failure))),
    }
    match load_and_open(loaded, rig) {
        Ok(leave) => Ok(leave),
        Err(LiveError::Logic(e)) => Err(notice(&e)),
        Err(LiveError::Restore(e)) => Err(notice(&e)),
        Err(LiveError::Preparation(e)) => Err(notice(&e)),
        Err(e) => Err(Stopped::Run(e)),
    }
}

/// What App does after a decision screen's look ([`serve`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    /// Go on.
    Go,
    /// The play's deadline step ran: its loading state shows.
    Stepped,
    /// Practice's deadline: the page opens now.
    Page,
    /// Back to selection with the latched refusal.
    Return,
    /// Back to selection by the player.
    Back,
    /// The run ends, or the window closes ([`ended`]).
    End,
}

/// The decision screen's one look from `origin`, with `reply` in hand:
/// the rig's tick, then the terminal causes and the escape read now, then
/// [`Decision::classify`], whose verdict it returns. A play's step runs
/// its effect here ([`AppRig::deadline_step`]), once, as the decision
/// latches it.
fn look<R: AppRig>(
    d: &mut Decision,
    rig: &mut R,
    origin: Origin,
    reply: Option<Reply>,
) -> Result<Verdict, LiveError<R::ScreenError>> {
    rig.decision_tick(origin)?;
    let terminal = rig.quitting() || threads_over(rig).is_err();
    let escape = d.waiting() && d.deadline().is_some_and(|t| rig.decision_escape(t));
    let seen = d.classify(origin, rig.now(), terminal, escape, reply);
    if seen == Verdict::Step && !d.practice() {
        rig.deadline_step();
    }
    Ok(seen)
}

/// What App does on a look's verdict.
const fn next_for(d: &Decision, v: Verdict) -> Next {
    match v {
        Verdict::Continue => Next::Go,
        Verdict::Step if d.practice() => Next::Page,
        Verdict::Step => Next::Stepped,
        Verdict::End => Next::End,
        Verdict::Return => Next::Return,
        Verdict::Back => Next::Back,
    }
}

/// The decision screen's look from `origin` ([`look`]), first, so `reply`
/// is consumed once, in [`Decision::classify`]'s order, before anything
/// else. A screen lost meanwhile is then built again, unless the look left
/// the screen (the next screen's own entry builds it): the rebuild begins
/// ([`AppRig::begin_recovery`]) and is polled with the screen looked at
/// between polls. A leaving verdict returns at once, the rebuild left
/// running; the deadline's step runs inside it, and is returned when the
/// rebuild ends, as the transition the look latched. Every look of the
/// screen comes here, the chart's read's included.
///
/// # Errors
///
/// The rig's tick's; a thread's failure seen when the rebuild fails, else
/// the screen's that cannot be built again. A close then ends the run
/// ([`Next::End`]).
fn serve<R: AppRig>(
    d: &mut Decision,
    rig: &mut R,
    origin: Origin,
    reply: Option<Reply>,
) -> Result<Next, LiveError<R::ScreenError>> {
    let seen = look(d, rig, origin, reply)?;
    let mut next = next_for(d, seen);
    if seen.leaves() || !rig.screen_failed() {
        return Ok(next);
    }
    rig.begin_recovery();
    loop {
        let now = rig.now();
        let poll_until = now.0.saturating_add(IDLE_WAIT.0);
        let until = d
            .deadline()
            .filter(|t| t.0 > now.0)
            .map_or(poll_until, |t| t.0.min(poll_until));
        match rig.poll_recovery(TimeUs(until)) {
            Some(Ok(())) => return Ok(next),
            Some(Err(e)) => {
                // The rebuild answers no terminal cause: a failure that
                // came meanwhile ends the run with its own cause, a close
                // as a close.
                threads_over(rig)?;
                if rig.quitting() {
                    return Ok(Next::End);
                }
                return Err(LiveError::Screen(e));
            }
            None => {
                let seen = look(d, rig, origin, None)?;
                if seen.leaves() {
                    return Ok(next_for(d, seen));
                }
                if seen == Verdict::Step {
                    next = next_for(d, seen);
                }
            }
        }
    }
}

/// How a wait the rig owns left the decision screen, as App routes it.
const fn next_of(v: Verdict) -> Next {
    match v {
        Verdict::Continue => Next::Go,
        Verdict::Step => Next::Stepped,
        Verdict::End => Next::End,
        Verdict::Return => Next::Return,
        Verdict::Back => Next::Back,
    }
}

/// The decision screen, nothing in its waits but its looks, until a look
/// gives more than [`Next::Go`]: what it gave. A screen lost meanwhile is
/// recovered by the next look ([`serve`]). A step a wait before the hold
/// latched (the decision stepped, nothing left or refused) ends it at
/// once: a later look classifies only [`Verdict::Continue`].
fn hold<R: AppRig>(
    d: &mut Decision,
    rig: &mut R,
    origin: Origin,
) -> Result<Next, LiveError<R::ScreenError>> {
    loop {
        if d.stepped() && d.left().is_none() && !d.latched() {
            return Ok(if d.practice() {
                Next::Page
            } else {
                Next::Stepped
            });
        }
        let next = serve(d, rig, origin, None)?;
        if next != Next::Go {
            return Ok(next);
        }
        let now = rig.now();
        let until = d.deadline().map_or(now.0, |at| at.0).max(now.0 + 1);
        rig.idle_wait(TimeUs(now.0.saturating_add(IDLE_WAIT.0).min(until)));
    }
}

/// A refusal held to the decision screen's deadline: latched, then held
/// until it returns, or the run ends ([`Next::End`]).
fn hold_refusal<R: AppRig>(
    d: &mut Decision,
    rig: &mut R,
) -> Result<Next, LiveError<R::ScreenError>> {
    match serve(d, rig, Origin::Ready, Some(Reply::Refusal))? {
        Next::Go => hold(d, rig, Origin::Ready),
        next => Ok(next),
    }
}

/// A loaded play held to its deadline's step: `false` when the run ends
/// or the window closes first.
fn ready_at_deadline<R: AppRig>(
    d: &mut Decision,
    rig: &mut R,
) -> Result<bool, LiveError<R::ScreenError>> {
    if !d.on() || d.stepped() {
        return Ok(true);
    }
    Ok(match hold(d, rig, Origin::Ready)? {
        Next::Stepped | Next::Go | Next::Page => true,
        Next::End | Next::Return | Next::Back => false,
    })
}

/// A practice chart read: its page's jobs advance until the deadline, then
/// the page opens with the decision screen's epoch over. `false` when the
/// run ends or the window closes first.
fn page_entry<R: AppRig>(d: &mut Decision, rig: &mut R) -> Result<bool, LiveError<R::ScreenError>> {
    if !d.on() {
        return Ok(true);
    }
    let opens = match hold(d, rig, Origin::Jobs)? {
        Next::Page | Next::Go | Next::Stepped => true,
        Next::End | Next::Return | Next::Back => false,
    };
    if opens {
        rig.closed_epoch();
    }
    Ok(opens)
}

/// The decision screen left for `next`, as a chart's outcome.
fn left_by<R: AppRig, B>(
    next: Result<Next, LiveError<R::ScreenError>>,
    rig: &mut R,
) -> Result<Chosen<B>, LiveError<R::ScreenError>> {
    match next? {
        Next::Back | Next::Return | Next::Go | Next::Stepped | Next::Page => {
            Ok(Chosen::Left(Leave::Back))
        }
        Next::End => ended(rig).map(Chosen::Left),
    }
}

/// The run ends on a decision screen's look: a thread's failure ends it
/// with its cause; a close quits.
fn ended<R: AppRig>(rig: &mut R) -> Result<Leave, LiveError<R::ScreenError>> {
    threads_over(rig)?;
    Ok(if rig.quitting() {
        Leave::Quit
    } else {
        Leave::Back
    })
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
