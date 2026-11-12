//! Song selection on the real threads (design.md "Play Lifecycle"): the
//! rig's select screen, and the loading of each chosen chart onto the
//! threads that stay up across charts.
//!
//! - **Loading** swaps the audio engine's bank and schedule
//!   ([`game::AudioControl::load`]), the render thread's chart
//!   ([`RenderLink::load`]) and, with autoplay, the input thread's generated
//!   input; a chart refused on the way returns to selection with why.
//! - **The scan** pauses from the choice to each READY state, which waits for
//!   its acknowledgment ([`PlayerRig`]'s audio preparation); every entry
//!   into selection and every result releases it. A chart whose loudness
//!   the library misses is measured during its load, so its first play has
//!   its gain; the measurement is stored once that pause is acknowledged.
//! - **The play screen's loading state** shows from a chart chosen to
//!   play, or a practice Start, to READY: the play screen without
//!   notes, with the chart, the preset and the load's stage over it, the
//!   sounds decoded so far among them.
//! - Loading runs on the game thread, the screen's preparation of the
//!   chart on the render thread meanwhile: the render thread keeps drawing
//!   the loading state around it, the window thread keeps the window
//!   answering, and a close or the quit key cancels the load at the
//!   measurement's checks, or is taken once the load is over (the load's
//!   waits are all bounded).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bms_model::{TimeSource, TimeUs};
use game::{
    AppRig, Chain, LoadRefusal, Loaded, Loading, Pick, PracticeInput, PracticeScreen, Refused,
    ResultScreen, SessionError,
};

#[cfg(feature = "dev-tools")]
use crate::controls::Controls;
use crate::end_screen::{LoadTimes, preset_error_text, shelf_error_text, view_log};
use crate::loading_state::LoadInput;
use crate::loading_text::{ChosenChart, LoadingText, Stage};
use crate::log_file;
use crate::play::{PlayerRig, since};
use crate::render_side::{LoadingSink, RenderLink, RenderMsg, ScreenError, SkinState};
use crate::result_input::{Owner, Pad, ResultInput};
use crate::select::{Acted, ListQuery, MenuInput, MenuKey, MenuQueue, SelectScreen};
#[cfg(feature = "dev-tools")]
use crate::{dev::SelectStep, dev::scenario::InputRef, input_side::Mode};

/// Autoplay's input for a chart as made: the input, its queue's size and
/// the controls it records (dev-tools).
#[cfg(feature = "dev-tools")]
type Feed = (input::AutoplayInput, usize, Vec<eval::ControlId>);
use crate::selection_skin::SkinFeed;
use crate::shelf::{Shelf, file_name};
use crate::ui_text::{Label, Message};
use crate::window::ImeSwitch;

/// The terminal causes a page job's settle looks at, read afresh at every
/// ask: a close, the input thread's failure and the audio output's.
pub(crate) struct Looks<'r> {
    controls: &'r crate::controls::Controls,
    failures: &'r crate::failures::Failures,
}

impl Looks<'_> {
    /// Whether the run is ending or a thread failed, now.
    pub(crate) fn terminal(&self) -> Option<crate::page_jobs::Terminal> {
        (self.controls.quitting() || self.failures.any()).then_some(crate::page_jobs::Terminal)
    }
}

/// The rig's [`Looks`], borrowing only the fields it reads, so the
/// selection can be borrowed beside it.
macro_rules! looks {
    ($rig:ident) => {
        crate::selection::Looks {
            controls: $rig.controls,
            failures: &$rig.failures,
        }
    };
}
pub(crate) use looks;

/// Arrow keys a scripted pick queues at a time: fewer than the queue holds
/// (dev-tools).
#[cfg(feature = "dev-tools")]
const PICK_BATCH: usize = crate::select::QUEUED_KEYS / 2;

/// Backspaces a scripted search queues at a time (dev-tools).
#[cfg(feature = "dev-tools")]
const QUERY_BATCH: usize = crate::select::QUEUED_KEYS / 2;

/// What a load's job threads report of their decodes (tests): each
/// decode's speed and the foreground permits held as it begins; each
/// decode that made a bank, with its most workers at once and the permits
/// its other workers still held as its pool returned; the decodes that
/// returned; and the permits held at a measurement's first check.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Decodes {
    /// `(speed, foreground permits held)` as each decode begins.
    pub begun: Mutex<Vec<(bms_model::Speed, usize)>>,
    /// `(speed, decode_threads, permits left)` per decode that made a bank.
    pub decodes: Mutex<Vec<(bms_model::Speed, usize, usize)>>,
    /// Decodes that returned, whatever they gave.
    pub ended: std::sync::atomic::AtomicUsize,
    /// Foreground permits held at the analysis's first check.
    pub analysis: Mutex<Option<usize>>,
    /// Called at each [`Probe`] of the load.
    pub hook: Option<Hook>,
    /// Called by each decode worker, with its index, as it asks before a
    /// file.
    pub at_file: Option<FileHook>,
    /// The measurement's analysis is given a bank whose rate its meter
    /// refuses, so it fails without a panic.
    pub refuse_meter: std::sync::atomic::AtomicBool,
    /// The decode returns a bank without a sound digest.
    pub no_digest: std::sync::atomic::AtomicBool,
}

/// Where a load calls its test hook (tests).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// On a job's thread as a decode begins, its permits taken: the Bank
    /// job's at the load's speed, the Measure job's at 1×.
    Decode(bms_model::Speed),
    /// On the game thread at each slice of its wait for a job, before it
    /// looks for a cancel.
    Wait(game::coord::JobKind),
    /// On the game thread, the bank installed, before the Measure job
    /// begins.
    BeforeMeasure,
    /// On the Measure job's thread, at the analysis's first check.
    Analysis,
    /// On the game thread at each slice of its wait for the screen's
    /// chart.
    Screen,
    /// On the game thread in the release barrier, before the unload.
    Barrier,
    /// On the game thread once the Bank job is joined, before its bank is
    /// wrapped and registered.
    Joined,
}

/// A load's test hook: the probe, the run's controls and whether the job
/// calling it was asked to stop (`false` on the game thread).
#[cfg(test)]
#[derive(Clone)]
pub struct Hook(pub Arc<HookFn>);

/// A decode worker's test hook, called with its index (tests).
#[cfg(test)]
#[derive(Clone)]
pub struct FileHook(pub Arc<dyn Fn(usize) + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for FileHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileHook")
    }
}

/// What a [`Hook`] calls (tests).
#[cfg(test)]
pub type HookFn = dyn Fn(Probe, &crate::controls::Controls, &dyn Fn() -> bool) + Send + Sync;

#[cfg(test)]
impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// Calls `decodes`' hook, if any, at `at`.
#[cfg(test)]
fn probe(
    decodes: Option<&Decodes>,
    at: Probe,
    controls: &crate::controls::Controls,
    stop: &dyn Fn() -> bool,
) {
    if let Some(Hook(hook)) = decodes.and_then(|d| d.hook.as_ref()) {
        hook(at, controls, stop);
    }
}

/// Where a decode calls its test hooks (tests): the load's record and
/// probe, and a call at each worker's file boundary.
#[cfg(test)]
#[derive(Clone, Copy, Default)]
pub struct DecodeProbe<'a> {
    /// The load's record and its run's controls.
    pub load: Option<(&'a Decodes, &'a crate::controls::Controls)>,
    /// Called with the worker's index as it asks before a file.
    pub at_file: Option<&'a (dyn Fn(usize) + Sync)>,
}

/// A decode's width on its job's permits (design.md "Load Pipeline": the
/// decode pool draws the foreground permits). Worker 0 is the job's own
/// thread, whose checkpoint before each file moves its permit to the class
/// the job wants and calls its class hook. Each other worker is started on
/// a foreground permit reserved for it alone ([`game::coord::WorkerLeases`],
/// within the pool's cap), only while the job's own permit is a foreground
/// one, so no permit is taken for a worker that does not run; once the job
/// does not want the foreground, a worker retires at its next file and its
/// permit goes back.
struct PermitWidth<'a, 'c> {
    ctx: &'a game::coord::Ctx<'c>,
    leases: game::coord::WorkerLeases<'a>,
    /// Each other worker's permit, by its index, while it runs or starts.
    held: Mutex<Vec<(usize, game::coord::WorkerLease)>>,
    /// The decode's cancel, which stops worker 0 too.
    cancel: &'a (dyn Fn() -> bool + Sync),
    #[cfg(test)]
    at_file: Option<&'a (dyn Fn(usize) + Sync)>,
}

impl audio::Width for PermitWidth<'_, '_> {
    fn at_file(&self, worker: usize) -> bool {
        #[cfg(test)]
        if let Some(at_file) = self.at_file {
            at_file(worker);
        }
        if worker == 0 {
            !self.ctx.checkpoint() && !(self.cancel)()
        } else {
            self.ctx.wanted() == game::coord::Class::Foreground
        }
    }

    fn start(&self, worker: usize, cap: usize) -> bool {
        // Worker 0's permit moves at its own checkpoint: a promotion since
        // then widens the decode at its next file.
        if self.ctx.class() != Some(game::coord::Class::Foreground) {
            return false;
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(lease) = self.leases.reserve(self.ctx, cap, held.len()) else {
            return false;
        };
        held.push((worker, lease));
        true
    }

    fn retire(&self, worker: usize) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let lease = held
            .iter()
            .position(|(w, _)| *w == worker)
            .map(|i| held.swap_remove(i).1);
        drop(held);
        if let Some(lease) = lease {
            lease.retire();
        }
    }
}

/// Decodes `chart`'s bank at `speed` on `ctx`'s job: on its own thread and
/// permit, and on other workers on the foreground permits free as it goes
/// ([`PermitWidth`]), at most [`audio::BankLimits`]' default thread count,
/// fewer on fewer cores or files. Every permit taken for those workers is
/// given back as each one ends. `cancel` stops it at the next file or
/// packet.
pub(crate) fn decode_on_permits(
    ctx: &game::coord::Ctx<'_>,
    coord: &game::coord::Coordinator,
    (chart, rate, speed): (&bms_model::Chart, u32, bms_model::Speed),
    progress: &(dyn Fn(usize, usize) + Sync),
    cancel: &(dyn Fn() -> bool + Sync),
    #[cfg(test)] hooks: DecodeProbe<'_>,
) -> Result<audio::SoundBank, audio::BankError> {
    #[cfg(test)]
    if let Some((d, controls)) = hooks.load {
        d.begun
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((speed, coord.held(game::coord::Class::Foreground)));
        probe(Some(d), Probe::Decode(speed), controls, &|| ctx.cancelled());
        if d.no_digest.load(std::sync::atomic::Ordering::SeqCst) {
            d.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(audio::SoundBank::from_sounds(rate, Vec::new()));
        }
    }
    let width = PermitWidth {
        ctx,
        leases: game::coord::WorkerLeases::new(coord),
        held: Mutex::new(Vec::new()),
        cancel,
        #[cfg(test)]
        at_file: hooks.at_file,
    };
    let limits = audio::BankLimits::default();
    let bank = audio::load_bank_width(chart, rate, speed, &limits, progress, cancel, &width);
    #[cfg(test)]
    if let Some((d, _)) = hooks.load {
        if let Ok(bank) = &bank {
            let left = width
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len();
            d.decodes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((speed, bank.stats().decode_threads, left));
        }
        d.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    bank
}

/// What a chart's measurement job lends its measurement, on the job's
/// thread: the load's cancel. It measures the 1× bank it is given only: a
/// practice at another speed measures on its 1× bank before its own
/// ([`page_start`]). The stage line was shown before the job began.
struct JobHost<'a, 'c> {
    controls: &'a crate::controls::Controls,
    #[cfg(test)]
    coord: &'a game::coord::Coordinator,
    ctx: &'a game::coord::Ctx<'c>,
    #[cfg(test)]
    decodes: Option<&'a Decodes>,
}

impl crate::loudness::Host for JobHost<'_, '_> {
    fn at_normal(&mut self) -> anyhow::Result<(bms_model::Chart, Arc<audio::SoundBank>)> {
        anyhow::bail!("a load measures its 1× bank only")
    }

    fn measuring(&mut self, _name: &str) {}

    fn cancelled(&mut self) -> bool {
        #[cfg(test)]
        if let Some(d) = self.decodes {
            let mut first = d.analysis.lock().unwrap_or_else(PoisonError::into_inner);
            if first.is_none() {
                *first = Some(self.coord.held(game::coord::Class::Foreground));
                drop(first);
                let ctx = self.ctx;
                probe(Some(d), Probe::Analysis, self.controls, &|| ctx.cancelled());
            }
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::press_due(self.controls, crate::dev::LoadPhase::Measuring);
        self.ctx.cancelled() || cancelled(self.controls)
    }
}

/// The select screen's side of the rig.
pub struct Selection {
    /// The library and its scan.
    pub shelf: Shelf,
    /// The screen's state.
    pub screen: SelectScreen,
    /// What the window hands the screen: the result screen's input
    /// ([`crate::play::ResultInput`]) reads it too, while a result shows.
    pub menu: Arc<MenuQueue>,
    /// The window's keys the queue dropped, as last reported.
    pub dropped: u64,
    /// The pointer's side on the game thread: its shared state and the
    /// batch in flight.
    pub pointer: crate::pointer::PointerPass,
    /// The pointer's presses and notches the queue refused, as last
    /// reported.
    pub dropped_pointer: u64,
    /// Whether the notice shown is a refusal because the shelf of presets
    /// would not open: it goes once the shelf opens.
    pub preset_refusal: bool,
    /// The window's IME, allowed while the search box has focus; none
    /// headless.
    pub ime: Option<ImeSwitch>,
    /// The output's rate, which every bank is decoded at.
    pub rate: u32,
    /// When the chart being loaded was chosen.
    pub chosen_at: TimeUs,
    /// The load's lines on the play screen, from a chart's choice or a
    /// practice Start to its READY state or the way back.
    pub loading: Option<LoadingText>,
    /// Whether the chart chosen last was chosen to practice.
    pub practicing: bool,
    /// The decision screen's presentation while it shows, sent again when
    /// the screen is built again, and the chosen chart's file name.
    pub decision: Option<(crate::menu_screen::MenuPage, String)>,
    /// When selection was last entered.
    pub entered_at: TimeUs,
    /// The page changed since it was last sent.
    pub dirty: bool,
    /// The search or order changed and is not searched yet.
    pub search_pending: bool,
    /// Steps of the play preset taken and not yet applied: the flow takes
    /// them after each look ([`AppRig::take_preset_step`]).
    pub preset_steps: std::collections::VecDeque<game::PresetStep>,
    /// What the render thread is given of each change: the lifetime, the
    /// timers' starts and the skin's window.
    pub feed: SkinFeed,
    /// The scripted input (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub script: Option<crate::dev::SelectScript>,
    /// The run's coordinator, whose foreground permits a chart's bank and
    /// measurement take.
    pub coord: game::coord::Coordinator,
    /// Starts the load's job threads (tests stand in a refusal).
    pub spawner: game::coord::Spawner,
    /// The practice page's own jobs for the chosen chart.
    pub page: crate::page_jobs::PageJobs,
    /// What the load's decodes report (tests).
    #[cfg(test)]
    pub decodes: Option<Arc<Decodes>>,
    /// Called after the Bank job made its bank and before the load's look
    /// for a cancel (tests).
    #[cfg(test)]
    pub after_bank: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The release barrier's unload fails with this, once, before it is
    /// sent (tests: the rig's audio thread cannot be held from answering).
    #[cfg(test)]
    pub unload_fault: Option<game::AudioLoadError>,
    /// Every bank the chosen chart's load decodes and every decode in
    /// flight, checked by the release barrier (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub witness: Arc<crate::bank_witness::BankWitness>,
}

impl Selection {
    /// Reads the screen's list from the library, climbing past a place that
    /// is no longer a level. Each climb leaves one place fewer on the screen's
    /// stack, and Top and the searches always list.
    fn read_list(&mut self) -> Result<(), library::LibraryError> {
        let limit = crate::select::LIMIT + 1;
        loop {
            match self.screen.list_query() {
                ListQuery::Search(search) => {
                    let found = self.shelf.search(&search)?;
                    self.screen.set_entries(found);
                    return Ok(());
                }
                ListQuery::Top { one_root, sort } => {
                    let roots = self.shelf.top()?;
                    let charts = roots.iter().map(|r| r.charts).sum();
                    let level = match roots.as_slice() {
                        [root] if one_root => self.shelf.level(&root.key, sort, limit)?,
                        _ => library::Level {
                            folders: roots,
                            ..library::Level::default()
                        },
                    };
                    self.screen.set_top(level, charts);
                    return Ok(());
                }
                ListQuery::Level { key, sort } => {
                    let level = self.shelf.level(&key, sort, limit)?;
                    if self.screen.set_level(level) {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Reads the screen's list from the library again.
    fn refresh(&mut self) {
        self.search_pending = false;
        self.feed.list_revision = self.feed.list_revision.saturating_add(1);
        match self.read_list() {
            Ok(()) => {}
            Err(e) => {
                let line = format!("library: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
            }
        }
        self.refresh_bests();
        self.dirty = true;
    }

    /// Reads the bests of the chosen preset again, for the list.
    fn refresh_bests(&mut self) {
        let Some(preset) = self.screen.bests_preset() else {
            return;
        };
        match self.shelf.bests(&replay::preset_hash(preset)) {
            Ok(bests) => self.screen.set_bests(bests),
            Err(e) => {
                let line = format!("library: the bests cannot be read: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
            }
        }
        self.dirty = true;
    }

    /// Acts on one input; the chart it chose, if it did. A changed search
    /// or order is searched once all queued input is taken.
    fn act(&mut self, input: MenuInput) -> Option<Pick> {
        match self.screen.input(input) {
            Acted::Nothing => None,
            Acted::Changed => {
                self.dirty = true;
                None
            }
            Acted::Search => {
                self.search_pending = true;
                self.dirty = true;
                None
            }
            Acted::Focus(on) => {
                if let Some(ime) = &self.ime {
                    ime.set(on);
                }
                self.dirty = true;
                None
            }
            Acted::Choose(path) => Some(Pick::Play(path)),
            Acted::Practice(path) => Some(Pick::Practice(path)),
            Acted::Preset(step) => {
                self.preset_steps.push_back(step);
                None
            }
        }
    }

    /// Takes the queued input; the chart it chose, if it did. Consecutive
    /// edits of the search or the order are searched once: at the end, or
    /// before any other input, which acts on the list they make.
    ///
    /// While a panel is open (START, SELECT or both held), the keys it
    /// holds back do nothing: the window's by the panel the screen shows
    /// ([`crate::select::acts_under_panel`]); the controller's all, under
    /// that panel or the one open as each was sent: in the reference they
    /// set options or, the scratch under panel 1, an option's target
    /// (`MusicSelectInputProcessor.java` at 0.8.9, `:123-152`, `:158-202`,
    /// `:213-240`, `:250-288`), never the list, and the panels are display
    /// only. No panel counts while the search box has focus. An input that gives or takes the box's focus reads
    /// the panels' `level` again at `now` ([`panels`](Self::panels)), so
    /// the input after it finds the panel it leaves.
    fn drain_at(
        &mut self,
        pad: &mut Pad,
        level: Option<&input::PanelLevel>,
        now: TimeUs,
    ) -> Option<Pick> {
        // The controller's events join the window's, within their bound,
        // with the panel open as each was sent.
        while let Some((event, panel, stamp, at)) = pad.pop_stamped() {
            self.menu.push_stamped(
                MenuInput::Pad { event, panel },
                Some(crate::select::Observed { stamp, at }),
            );
        }
        pad.report_dropped("select");
        if let Some(pick) = self.pass(level, now) {
            return Some(pick);
        }
        if self.search_pending {
            self.refresh();
        }
        let dropped = self.menu.dropped();
        if dropped > self.dropped {
            let line = format!(
                "select: {} keys dropped, the window's queue was full",
                dropped - self.dropped
            );
            eprintln!("{line}");
            log::warn!("{line}");
            self.dropped = dropped;
        }
        let refused = self.pointer.shared.dropped();
        if refused > self.dropped_pointer {
            let line = format!(
                "select: {} pointer presses or notches refused, the window's queue was full",
                refused - self.dropped_pointer
            );
            eprintln!("{line}");
            log::warn!("{line}");
            self.dropped_pointer = refused;
        }
        None
    }

    /// One pass over the queue (design.md "Input", **Pointer input**): the
    /// answers of the batch in flight first, once the render thread has
    /// answered it, then at most [`MENU_PASS`](crate::select::MENU_PASS)
    /// entries in order, a run of pointer entries handed to the render
    /// thread as one batch, after which nothing more is taken until it is
    /// answered (the barrier); then a drag, once no batch is in flight and
    /// the press that began its hold was dispatched.
    fn pass(&mut self, level: Option<&input::PanelLevel>, now: TimeUs) -> Option<Pick> {
        if self.pointer.take() == crate::pointer::Taken::Waiting {
            return None;
        }
        // The answers in order; a pick leaves the rest for the next pass,
        // which dispatches them if the pick was refused and selection
        // stays.
        while let Some((slot, built)) = self.pointer.next_answer() {
            if let Some(pick) = self.dispatch_pointer(slot, built, level, now) {
                return Some(pick);
            }
        }
        let mut taken = 0;
        while taken < crate::select::MENU_PASS {
            if self.menu.front_is_pointer() {
                let menu = &self.menu;
                let mut left = crate::select::MENU_PASS - taken;
                self.pointer.flight = self.pointer.shared.submit(self.feed.lifetime, || {
                    while left > 0 && menu.front_is_pointer() {
                        left -= 1;
                        match menu.pop_observed() {
                            Some(Some((input, _))) => {
                                return crate::pointer::PointerRequest::of(&input);
                            }
                            Some(None) => {}
                            None => return None,
                        }
                    }
                    None
                });
                taken = crate::select::MENU_PASS - left;
                if self.pointer.flight.is_some() {
                    return None;
                }
                continue;
            }
            let Some(entry) = self.menu.pop_observed() else {
                break;
            };
            taken += 1;
            let Some((input, _)) = entry else {
                continue;
            };
            let shown = self.shown_panel();
            let acts = match input {
                MenuInput::Key(key) => crate::select::acts_under_panel(key, shown),
                MenuInput::Pad { panel, .. } => shown
                    .or(panel.filter(|_| !self.screen.searching()))
                    .is_none(),
                MenuInput::Text(_) | MenuInput::Preedit(_) => true,
                // The pointer's are taken as batches above.
                MenuInput::Pointer { .. } | MenuInput::Wheel { .. } => false,
            };
            if !acts {
                continue;
            }
            let pick = self.take_reading(input, level, now);
            #[cfg(test)]
            self.dispatched(crate::pointer::Dispatched::Input);
            if pick.is_some() {
                return pick;
            }
        }
        if let Some(drag) = self.pointer.shared.take_drag(self.menu.presses_taken()) {
            self.refresh_pending();
            let facts = self.pointer_facts();
            let act = crate::pointer::resolve_drag(&drag, &facts);
            let pick = self.pointer_act(act, level, now);
            #[cfg(test)]
            self.dispatched(crate::pointer::Dispatched::Drag(act));
            return pick;
        }
        None
    }

    /// Records a dispatched input and the cursor after it, while the log
    /// is kept (tests).
    #[cfg(test)]
    fn dispatched(&mut self, what: crate::pointer::Dispatched) {
        let cursor = self.screen.cursor();
        if let Some(log) = &mut self.pointer.log {
            log.push((what, cursor));
        }
    }

    /// What the select screen is to a pointer input now.
    fn pointer_facts(&self) -> crate::pointer::Facts {
        // The panel the screen shows: none while the search box has
        // focus, where the pointer acts as without a panel.
        let held = self.shown_panel();
        crate::pointer::Facts {
            lifetime: self.feed.lifetime,
            word: self.pointer.shared.word(),
            list_revision: self.feed.list_revision,
            count: self.screen.selectable(),
            cursor: self.screen.cursor(),
            searching: self.screen.searching(),
            composing: self.screen.composing(),
            panel: held.is_some(),
        }
    }

    /// A changed search or order is searched before a pointer input, which
    /// acts on the list it makes (a row drawn before it is then stale).
    fn refresh_pending(&mut self) {
        if self.search_pending {
            self.refresh();
        }
    }

    /// Acts on one answered press or notch.
    fn dispatch_pointer(
        &mut self,
        slot: crate::pointer::PointerSlot,
        built: Option<crate::pointer::Built>,
        level: Option<&input::PanelLevel>,
        now: TimeUs,
    ) -> Option<Pick> {
        self.refresh_pending();
        let facts = self.pointer_facts();
        let screen = &self.screen;
        let resolved = crate::pointer::resolve(slot.request, slot.hit, built, &facts, |i| {
            screen.row_kind_at(i)
        });
        if resolved.unfocus {
            // As Tab in the box: the focus goes, and the IME with it.
            let _ = self.take_reading(MenuInput::Key(MenuKey::Tab), level, now);
        }
        let pick = self.pointer_act(resolved.act, level, now);
        #[cfg(test)]
        self.dispatched(match slot.request {
            crate::pointer::PointerRequest::Press { .. } => {
                crate::pointer::Dispatched::Press(resolved.act)
            }
            crate::pointer::PointerRequest::Wheel { .. } => {
                crate::pointer::Dispatched::Notch(resolved.act)
            }
        });
        pick
    }

    /// Carries out what a pointer input does.
    fn pointer_act(
        &mut self,
        act: crate::pointer::PointerAct,
        level: Option<&input::PanelLevel>,
        now: TimeUs,
    ) -> Option<Pick> {
        let to = match act {
            crate::pointer::PointerAct::Nothing => return None,
            crate::pointer::PointerAct::Key(key) => {
                return self.take_reading(MenuInput::Key(key), level, now);
            }
            crate::pointer::PointerAct::MoveTo(index) => index,
            crate::pointer::PointerAct::Rows(rows) => {
                let last = self.screen.selectable().saturating_sub(1);
                let rows_abs = usize::try_from(rows.unsigned_abs()).unwrap_or(usize::MAX);
                if rows < 0 {
                    self.screen.cursor().saturating_sub(rows_abs)
                } else {
                    self.screen.cursor().saturating_add(rows_abs).min(last)
                }
            }
        };
        if self.screen.move_to(to) {
            self.dirty = true;
        }
        None
    }

    /// [`drain_at`](Self::drain_at) with no panels' level to read: the
    /// panel the screen shows stays.
    #[cfg(test)]
    fn drain(&mut self, pad: &mut Pad) -> Option<Pick> {
        self.drain_at(pad, None, TimeUs(0))
    }

    /// The panel the screen shows: none while the search box has focus.
    fn shown_panel(&self) -> Option<usize> {
        self.feed.panel().filter(|_| !self.screen.searching())
    }

    /// [`take`](Self::take), then the panels' `level` read again at `now`
    /// when `input` gave or took the search box's focus.
    fn take_reading(
        &mut self,
        input: MenuInput,
        level: Option<&input::PanelLevel>,
        now: TimeUs,
    ) -> Option<Pick> {
        let searching = self.screen.searching();
        let pick = self.take(input);
        if self.screen.searching() != searching {
            self.panels(level, now);
        }
        pick
    }

    /// Acts on `input` taken from a queue, searching a changed search or
    /// order first unless `input` edits it too.
    fn take(&mut self, input: MenuInput) -> Option<Pick> {
        let edit = matches!(
            input,
            MenuInput::Text(_)
                | MenuInput::Preedit(_)
                | MenuInput::Key(
                    MenuKey::Left | MenuKey::Right | MenuKey::Backspace | MenuKey::BackspaceHeld
                )
        );
        if self.search_pending && !edit {
            self.refresh();
        }
        self.act(input)
    }

    /// The panels' `level` read at `now`: an update is due when the panel
    /// the screen shows changed.
    fn panels(&mut self, level: Option<&input::PanelLevel>, now: TimeUs) {
        let Some(level) = level else {
            return;
        };
        if self.feed.panels(level.read(), self.screen.searching(), now) {
            self.dirty = true;
        }
    }

    /// [`drain_at`](Self::drain_at) while selection owns the menus' queues
    /// (`menus`); otherwise nothing is taken, so input queued for another
    /// screen stays its own.
    fn drain_owned(
        &mut self,
        menus: &mut ResultInput,
        level: Option<&input::PanelLevel>,
        now: TimeUs,
    ) -> Option<Pick> {
        if menus.owner() != Owner::Select {
            return None;
        }
        self.drain_at(&mut menus.pad, level, now)
    }

    /// What startup left the select screen: a skin's refusal `notice`, and
    /// the installed selection skin's song list (`window`, its center slot
    /// and its placements), which the screen's view is made for.
    pub fn started(&mut self, notice: Option<String>, window: Option<(usize, usize)>) {
        if let Some(notice) = notice {
            self.screen.set_notice(Some(notice));
        }
        self.feed.window = window;
    }

    /// Shows on the page that `chart` was refused, and why (`text`);
    /// `preset` when the refusal was the shelf's, which cannot open.
    fn refused(&mut self, chart: &std::path::Path, text: String, preset: bool) {
        self.screen.select_path(chart);
        self.screen.set_notice(Some(text));
        self.preset_refusal = preset;
        self.dirty = true;
    }

    /// Shows the chosen preset `name` and its file's `error`; once the
    /// shelf is `opened`, a refusal because it would not open is over, and
    /// its notice goes.
    fn presets(&mut self, name: String, error: Option<String>, opened: bool) {
        self.screen.set_preset(name, error);
        if opened && self.preset_refusal {
            self.screen.set_notice(None);
            self.preset_refusal = false;
        }
        self.dirty = true;
    }

    /// Shows why `player.toml` could not be written (`error`), or nothing
    /// once it was; a write that failed `now` replaces the refusal shown,
    /// which is older than it.
    fn save_error(&mut self, error: Option<&str>, now: bool) {
        if now {
            self.screen.set_notice(None);
            self.preset_refusal = false;
        }
        self.screen
            .set_save_error(error.map(|why| Message::SaveFailed { why }.text()));
        self.dirty = true;
    }

    /// `chosen`, if the play skin is ready for it ([`SkinState::ready`]);
    /// else nothing is admitted, and the chart shows under the cursor with
    /// the reason as the notice. The page stays usable.
    fn admit(
        &mut self,
        chosen: Option<Pick>,
        skin: &mut SkinState,
        render: &RenderLink,
        now: TimeUs,
    ) -> Option<Pick> {
        let pick = chosen?;
        let Err(notice) = skin.ready() else {
            return Some(pick);
        };
        let (Pick::Play(path) | Pick::Practice(path)) = &pick;
        println!("select: not admitted {}: {notice}", file_name(path));
        self.refused(path, notice, false);
        self.show(render, None, now);
        None
    }

    /// Sends the screen's update if it changed at `now`, its page with
    /// `status` in its title (the library's when `None`): the page, and
    /// what a selection skin draws ([`SkinFeed::update`]).
    fn show(&mut self, render: &RenderLink, status: Option<&str>, now: TimeUs) {
        if self.dirty {
            self.dirty = false;
            let shelf = self.shelf.status();
            let page = self.screen.page(status.unwrap_or(&shelf));
            // Where the cursor shows, for a script's checks (dev-tools).
            #[cfg(feature = "dev-tools")]
            if self.script.is_some()
                && let (Some(line), Some(row)) =
                    (self.screen.cursor_line(), self.screen.cursor_name())
            {
                println!("dev: select cursor: line {line}: {row}");
            }
            // The notice line the page shows, for a script's checks
            // (dev-tools).
            #[cfg(feature = "dev-tools")]
            if self.script.is_some()
                && let Some(notice) = page.lines.iter().find(|l| {
                    l.style == crate::menu_screen::LineStyle::Notice && !l.text.is_empty()
                })
            {
                println!("dev: select notice: {}", notice.text);
            }
            // The title and the keys, the page's own words, for a
            // script's checks (dev-tools).
            #[cfg(feature = "dev-tools")]
            if self.script.is_some() {
                for line in &page.lines {
                    if matches!(
                        line.style,
                        crate::menu_screen::LineStyle::Title | crate::menu_screen::LineStyle::Help
                    ) {
                        println!("dev: select page: {}", line.text);
                    }
                }
            }
            render.show_selection(self.feed.update(&self.screen, page, now));
        }
    }

    /// Runs the scripted steps due on the select screen. A step that types
    /// puts its input on the window's queue, as the window does, and the
    /// steps stop there until the queue is taken: `true` then. `drawn` is
    /// how many select screen frames were presented so far. A scenario's
    /// captures are armed from `captures`.
    #[cfg(feature = "dev-tools")]
    fn script(
        &mut self,
        controls: &Controls,
        wakes: u64,
        drawn: u64,
        captures: Option<&crate::dev::scenario::Captures>,
    ) -> bool {
        let listed = |screen: &SelectScreen, name: &str| {
            screen
                .entries()
                .iter()
                .position(|e| e.path.file_name().is_some_and(|n| n == name))
        };
        let Some(script) = &mut self.script else {
            return false;
        };
        let mut typed = Vec::new();
        loop {
            script.announce();
            match script.front().cloned() {
                None => return false,
                Some(SelectStep::Pick(name)) => {
                    let Some(at) = listed(&self.screen, &name) else {
                        script.scanning(self.shelf.has_scan());
                        if !script.waiting() {
                            controls.close();
                        }
                        return false;
                    };
                    if move_to(&self.screen, &self.menu, self.screen.chart_row(at)) {
                        return true;
                    }
                }
                Some(SelectStep::Folder(name)) => {
                    let Some(at) = self.screen.folder_row(&name) else {
                        script.scanning(self.shelf.has_scan());
                        if !script.waiting() {
                            controls.close();
                        }
                        return false;
                    };
                    if move_to(&self.screen, &self.menu, at) {
                        return true;
                    }
                    typed.push(MenuInput::Key(MenuKey::Enter));
                }
                Some(SelectStep::AllCharts) => {
                    if move_to(&self.screen, &self.menu, 0) {
                        return true;
                    }
                    typed.push(MenuInput::Key(MenuKey::Enter));
                }
                Some(SelectStep::Up) => typed.push(MenuInput::Key(MenuKey::Escape)),
                Some(SelectStep::Row(name)) => {
                    let row = self.screen.cursor_name();
                    if row.as_deref() != Some(name.as_str()) {
                        script.fail(format!("the cursor is on {row:?}, not on {name}"));
                        controls.close();
                        return false;
                    }
                }
                Some(SelectStep::PickPath(target)) => {
                    script.scanning(self.shelf.has_scan());
                    if script.expired() {
                        let why = format!("{} was not listed", target.path.display());
                        script.fail(why);
                        controls.close();
                        return false;
                    }
                    // A search left from an earlier step is cleared first.
                    if type_query(&self.screen, &self.menu, "") {
                        return true;
                    }
                    let paths = self.screen.entries().iter().map(|e| e.path.as_path());
                    let Some(at) =
                        crate::dev::scenario::find(paths, &target, &mut script.identities)
                    else {
                        return false;
                    };
                    if move_to(&self.screen, &self.menu, self.screen.chart_row(at)) {
                        return true;
                    }
                }
                Some(SelectStep::Wait(name)) => {
                    if listed(&self.screen, &name).is_none() {
                        script.scanning(self.shelf.has_scan());
                        if !script.waiting() {
                            controls.close();
                        }
                        return false;
                    }
                }
                Some(SelectStep::Search(text)) => typed.extend([
                    MenuInput::Key(MenuKey::Tab),
                    MenuInput::Text(text),
                    MenuInput::Key(MenuKey::Enter),
                ]),
                Some(SelectStep::Query(text)) => {
                    if script.expired() {
                        script.fail(format!("the search \"{text}\" was not typed"));
                        controls.close();
                        return false;
                    }
                    if type_query(&self.screen, &self.menu, &text) {
                        return true;
                    }
                }
                Some(SelectStep::PickSearched(text)) => {
                    // Only a completed pass lists every chart the search
                    // finds.
                    let n = self.screen.entries().len();
                    if self.shelf.passes() == 0 || n == 0 {
                        script.scanning(self.shelf.has_scan());
                        if script.expired() {
                            script.fail(format!("search \"{text}\" listed no chart"));
                            controls.close();
                        }
                        return false;
                    }
                    if n > 1 {
                        script.fail(format!("search \"{text}\" listed {n} charts; give a path"));
                        controls.close();
                        return false;
                    }
                    if move_to(&self.screen, &self.menu, self.screen.chart_row(0)) {
                        return true;
                    }
                }
                Some(SelectStep::Idle(ms)) => {
                    let from = *script.idle_from.get_or_insert(wakes);
                    if !script.slept(ms) {
                        return false;
                    }
                    println!(
                        "dev: the input thread woke {} times in {ms} ms on the select screen",
                        wakes - from
                    );
                }
                Some(SelectStep::Sleep(ms)) => {
                    if !script.slept(ms) {
                        return false;
                    }
                }
                Some(SelectStep::File(path)) => {
                    if !path.exists() {
                        if script.watchdog_expired() {
                            script.fail(format!("{} never appeared", path.display()));
                            controls.close();
                        }
                        return false;
                    }
                }
                Some(SelectStep::Enter) => typed.push(MenuInput::Key(MenuKey::Enter)),
                Some(SelectStep::PrevPreset) => typed.push(MenuInput::Key(MenuKey::PrevPreset)),
                Some(SelectStep::NextPreset) => typed.push(MenuInput::Key(MenuKey::NextPreset)),
                Some(SelectStep::PracticeKey(_)) => {
                    typed.push(MenuInput::Key(MenuKey::Practice));
                }
                // Due here only when the practice page did not open.
                Some(
                    SelectStep::PracticeSet(_)
                    | SelectStep::PracticeStart
                    | SelectStep::Rounds(_)
                    | SelectStep::PracticeBack,
                ) => {
                    if let Some(why) = script.refusal() {
                        let chosen = script.last_done().map_or(0, |o| o.index);
                        let why = format!("the chart chosen at step {chosen} was refused: {why}");
                        script.fail(why);
                        controls.close();
                    } else if script.expired() {
                        script.fail("the practice screen did not open".to_owned());
                        controls.close();
                    }
                    return false;
                }
                Some(SelectStep::Capture(k)) => {
                    if let Some(c) = captures {
                        c.arm(k);
                    }
                }
                Some(SelectStep::CloseMeasuring) => script.close_measuring = true,
                Some(SelectStep::QuitMeasuring) => script.quit_measuring = true,
                Some(SelectStep::QuitDecoding) => script.quit_decoding = true,
                Some(SelectStep::SlowLoad(ms)) => script.slow_load.get_or_insert_default().ms = ms,
                Some(SelectStep::QuitLoading) => {
                    script.slow_load.get_or_insert_default().quit = true
                }
                Some(SelectStep::Specializing(at, s)) => crate::dev::specializing(at, s),
                Some(SelectStep::CloseLoading) => {
                    script.slow_load.get_or_insert_default().close = true;
                }
                Some(SelectStep::SlowMeasuring(ms)) => crate::dev::slow_measuring(ms),
                Some(SelectStep::SlowDecoding(ms)) => crate::dev::slow_decoding(ms),
                Some(SelectStep::SlowPreparing(ms)) => crate::dev::slow_preparing(ms),
                Some(SelectStep::SlowSpecializing(ms)) => crate::dev::slow_specializing(ms),
                Some(SelectStep::SlowChartLoad(ms)) => crate::dev::slow_chart_load(ms),
                Some(SelectStep::PressLoading(ms)) => crate::dev::press_loading(ms),
                Some(SelectStep::HoldBackground) => crate::dev::hold_background(),
                Some(SelectStep::Background(running)) => {
                    let done = if running {
                        crate::dev::background_held()
                    } else {
                        self.shelf.background().1
                    };
                    if !done {
                        if script.watchdog_expired() {
                            let what = if running {
                                "held nothing at its gate"
                            } else {
                                "kept a job"
                            };
                            script.fail(format!("the background measurement {what}"));
                            controls.close();
                        }
                        return false;
                    }
                }
                // Due here only when the chosen chart did not play.
                Some(SelectStep::Play(_)) => {
                    if let Some(why) = script.refusal() {
                        let chosen = script.last_done().map_or(0, |o| o.index);
                        let why = format!("the chart chosen at step {chosen} was refused: {why}");
                        script.fail(why);
                        controls.close();
                    } else if script.expired() {
                        script.fail("no chart was chosen".to_owned());
                        controls.close();
                    }
                    return false;
                }
                Some(SelectStep::Close) => {
                    script.done();
                    controls.close();
                    return false;
                }
                // The result left on its own.
                Some(SelectStep::Stay) => {}
                Some(SelectStep::Drawn) => {
                    let from = *script.drawn_from.get_or_insert(drawn);
                    if drawn <= from {
                        if script.expired() {
                            script.fail("the select screen was not drawn".to_owned());
                            controls.close();
                        }
                        return false;
                    }
                }
                Some(
                    step @ (SelectStep::Back
                    | SelectStep::Retry
                    | SelectStep::Next
                    | SelectStep::Prev),
                ) => {
                    script.fail(format!("{step:?} on the select screen"));
                    controls.close();
                    return false;
                }
            }
            script.done();
            if !typed.is_empty() {
                for input in typed {
                    self.menu.push(input);
                }
                return true;
            }
        }
    }

    /// Runs the scripted step due on a result.
    #[cfg(feature = "dev-tools")]
    fn script_result(
        &mut self,
        controls: &Controls,
        render: &RenderLink,
        captures: Option<&crate::dev::scenario::Captures>,
    ) {
        if let Some(script) = &mut self.script {
            script_result(script, controls, render, &self.menu, captures);
        }
    }

    /// Why the script failed, if it did (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn script_failure(&self) -> Option<String> {
        self.script
            .as_ref()
            .and_then(|s| s.failure().map(str::to_owned))
    }
}

/// What the select screen shows of the shelf: the chosen preset's name, and
/// the error to show, if any, as the screen draws it (by file name) and
/// with its full chain for the console and the log. The error is the
/// chosen file's, else that of a folder that cannot be listed, which a
/// chosen built-in shows too; a shelf that cannot open shows why.
fn shelf_shown(
    shelf: Result<&game::Shelf, &game::ShelfError>,
) -> (String, Option<(String, String)>) {
    match shelf {
        Ok(shelf) => (
            shelf.chosen_name().to_owned(),
            shelf
                .chosen_error()
                .or(shelf.listing_error())
                .map(|e| (preset_error_text(e), Chain(e).to_string())),
        ),
        Err(e) => (
            "-".to_owned(),
            Some((shelf_error_text(e), Chain(e).to_string())),
        ),
    }
}

/// Runs the scripted step due on a result: `drawn` by waiting on `render`
/// until the result's page has been drawn, then the step after it; `back`
/// and `retry` as their keys, `next` and `prev` as the right and left
/// arrows on the window's queue `menu`, `close` as closing the window; a
/// scenario's captures before them are armed from `captures`.
#[cfg(feature = "dev-tools")]
pub fn script_result(
    script: &mut crate::dev::SelectScript,
    controls: &Controls,
    render: &RenderLink,
    menu: &MenuQueue,
    captures: Option<&crate::dev::scenario::Captures>,
) {
    script.announce();
    loop {
        match script.front() {
            Some(&SelectStep::Capture(k)) => {
                if let Some(c) = captures {
                    c.arm(k);
                }
            }
            // Holds the retry's specialization.
            Some(&SelectStep::SlowSpecializing(ms)) => crate::dev::slow_specializing(ms),
            Some(SelectStep::Drawn) => {
                if !render.wait_end_drawn(crate::dev::STEP_TIMEOUT) {
                    script.fail("the result's page was not drawn".to_owned());
                    controls.close();
                    return;
                }
            }
            _ => break,
        }
        script.done();
        script.announce();
    }
    // A command acts on the result once it is drawn: a skin's view
    // committed, or its built-in page, each frame of it (the S10-09i
    // design's D1). A result that never draws still gets its command.
    if matches!(
        script.front(),
        Some(SelectStep::Back | SelectStep::Retry | SelectStep::Next | SelectStep::Prev)
    ) {
        let _ = render.wait_end_drawn(crate::dev::STEP_TIMEOUT);
    }
    match script.front() {
        Some(SelectStep::Back) => {
            script.done();
            controls.command(game::Command::Quit);
        }
        Some(SelectStep::Retry) => {
            script.done();
            controls.command(game::Command::Retry);
        }
        Some(SelectStep::Next) => {
            script.done();
            menu.push(MenuInput::Key(MenuKey::Right));
        }
        Some(SelectStep::Prev) => {
            script.done();
            menu.push(MenuInput::Key(MenuKey::Left));
        }
        // Closing the window ends the run from any scene.
        Some(SelectStep::Close) => {
            script.done();
            controls.close();
        }
        // The result is left on its own; the select screen takes it.
        Some(SelectStep::Stay) => {}
        Some(step) => {
            let why = format!("{step:?} on a result");
            script.fail(why);
            controls.close();
        }
        None => {}
    }
}

/// The routine of every entry into a menu screen, selection's or the
/// practice page's: `owner` takes the menus' queues and what was queued
/// is dropped, whatever owned them; the end screen is gone; nothing plays,
/// so the input thread drops its heartbeat (Tail to Idle: after a play, an
/// opening the logic step or the restore refused, which closes recording
/// without the end of a play, and at the first entry) and its menu events
/// flow again; and the scan goes on.
fn recover(
    menus: &mut ResultInput,
    owner: Owner,
    input: &crate::input_side::InputLink,
    in_play: &mut bool,
    end_page: &mut Option<crate::render_side::EndPage>,
    shelf: Option<&mut Shelf>,
) {
    *end_page = None;
    menus.take_over(owner);
    *in_play = false;
    input.idle();
    if let Some(shelf) = shelf {
        shelf.release();
    }
}

/// What the practice page's script does at a look (dev-tools).
#[cfg(feature = "dev-tools")]
#[derive(Debug, PartialEq, Eq)]
enum Driven {
    /// Press this key.
    Key(MenuKey),
    /// Nothing yet.
    Wait,
    /// The script failed: the run ends.
    Failed,
}

/// The scripted step due on the practice page (a scenario's `practice`):
/// a key toward the page its step sets, Start, or Back, one key per showing
/// of the page, so each acts on what the page shows. Setting the page
/// times out only once it stalls: each change it shows starts the wait
/// over, however many keys the step takes. A step that cannot be done
/// there fails the script (dev-tools).
#[cfg(feature = "dev-tools")]
fn drive_practice(
    script: &mut crate::dev::SelectScript,
    sc: &mut crate::dev::scenario::State,
) -> Driven {
    let Some(screen) = &sc.practice else {
        return Driven::Wait;
    };
    let fail = |script: &mut crate::dev::SelectScript, why: String| {
        script.fail(why);
        Driven::Failed
    };
    script.announce();
    let key = match script.front().cloned() {
        Some(SelectStep::PracticeSet(goal)) => {
            if goal.to > screen.measures().len() {
                let why = format!(
                    "to {} is past the chart's {} measures",
                    goal.to,
                    screen.measures().len()
                );
                return fail(script, why);
            }
            // The page moves in 1 % steps from the gauge's start: a goal
            // off them is never shown.
            if (screen.value().0 - goal.value.0) % rules::GaugeValue::PERCENT != 0 {
                let why = format!(
                    "gauge {} is off the page's 1 % steps from {}",
                    crate::practice_page::percent(goal.value),
                    crate::practice_page::percent(screen.value())
                );
                return fail(script, why);
            }
            // Only a page nearer the goal is progress: one that only moves
            // about stalls.
            let distance = distance(screen, &goal);
            if sc.shown && sc.nearest.is_none_or(|n| distance < n) {
                sc.nearest = Some(distance);
                script.progressed();
            }
            if script.expired() {
                return fail(script, format!("the practice page did not reach {goal:?}"));
            }
            match toward(screen, &goal) {
                Some(key) => key,
                None => {
                    script.done();
                    script.announce();
                    if !matches!(script.front(), Some(SelectStep::PracticeStart)) {
                        return Driven::Wait;
                    }
                    start_key(screen)
                }
            }
        }
        Some(SelectStep::PracticeStart) => start_key(screen),
        Some(SelectStep::PracticeBack) => MenuKey::Escape,
        // The rounds were due: their Start was refused, or they ended
        // before their last.
        Some(SelectStep::Rounds(_)) => {
            let why = match screen.notice() {
                Some(why) => format!("the practice was refused: {why}"),
                // The armed quit cancelled the Start's specialization, and
                // the page shows again: the rounds' expected end.
                None if sc.rounds == 0 && crate::dev::take_specialization_quit() => {
                    println!("dev: the rounds ended at the Start's cancelled specialization");
                    script.done();
                    return Driven::Wait;
                }
                None if sc.rounds > 0 => "the rounds ended before their last".to_owned(),
                None if script.expired() => "no round started".to_owned(),
                None => return Driven::Wait,
            };
            return fail(script, why);
        }
        None => return Driven::Wait,
        Some(step) => return fail(script, format!("{step:?} on the practice page")),
    };
    // One key on what the page shows: the next waits for it to show that
    // key's change.
    if !sc.shown {
        return Driven::Wait;
    }
    let last = match script.front() {
        Some(SelectStep::PracticeBack) => true,
        Some(SelectStep::PracticeStart) => {
            key == MenuKey::Enter && screen.row() == game::PracticeRow::Start && !screen.editing()
        }
        _ => false,
    };
    if last {
        script.done();
    }
    sc.shown = false;
    Driven::Key(key)
}

/// How many value steps the practice page is from `goal` (dev-tools).
#[cfg(feature = "dev-tools")]
fn distance(screen: &PracticeScreen, goal: &crate::dev::scenario::PracticeGoal) -> u64 {
    let steps = |a: i64, b: i64, step: i64| (a - b).unsigned_abs() / step.unsigned_abs();
    let index = |a: usize, b: usize| a.abs_diff(b) as u64;
    index(screen.from(), goal.from)
        + index(screen.to(), goal.to)
        + u64::from(
            screen
                .speed()
                .twentieths()
                .abs_diff(goal.speed.twentieths()),
        )
        + steps(screen.lead_in().0, goal.lead_in.0, game::LEAD_IN_STEP.0)
        + steps(screen.value().0, goal.value.0, rules::GaugeValue::PERCENT)
        + u64::from(screen.repeat() != goal.repeat)
}

/// The key that moves the practice page toward `goal`: out of an edit
/// first, then, row by row from the top, to the first row whose value
/// differs, and Left or Right on it; `to` first when `from` cannot pass
/// it. `None` once the page shows the goal (dev-tools).
#[cfg(feature = "dev-tools")]
fn toward(screen: &PracticeScreen, goal: &crate::dev::scenario::PracticeGoal) -> Option<MenuKey> {
    use game::PracticeRow;
    if screen.editing() {
        return Some(MenuKey::Escape);
    }
    let step = |up: bool| if up { MenuKey::Right } else { MenuKey::Left };
    let rows = [
        (PracticeRow::To, (goal.to > screen.to()).then(|| step(true))),
        (
            PracticeRow::From,
            (goal.from != screen.from()).then(|| step(goal.from > screen.from())),
        ),
        (
            PracticeRow::To,
            (goal.to != screen.to()).then(|| step(goal.to > screen.to())),
        ),
        (
            PracticeRow::Speed,
            (goal.speed != screen.speed())
                .then(|| step(goal.speed.twentieths() > screen.speed().twentieths())),
        ),
        (
            PracticeRow::LeadIn,
            (goal.lead_in != screen.lead_in()).then(|| step(goal.lead_in > screen.lead_in())),
        ),
        (
            PracticeRow::Gauge,
            (goal.value != screen.value()).then(|| step(goal.value > screen.value())),
        ),
        (
            PracticeRow::Repeat,
            (goal.repeat != screen.repeat()).then(|| step(true)),
        ),
    ];
    let (row, key) = rows.into_iter().find_map(|(r, k)| k.map(|k| (r, k)))?;
    Some(row_key(screen, row).unwrap_or(key))
}

/// The arrow that moves the page's cursor toward `row`, if it is not
/// there (dev-tools).
#[cfg(feature = "dev-tools")]
fn row_key(screen: &PracticeScreen, row: game::PracticeRow) -> Option<MenuKey> {
    let at = |r| game::PracticeRow::ALL.iter().position(|&x| x == r);
    let (now, to) = (at(screen.row())?, at(row)?);
    match now.cmp(&to) {
        std::cmp::Ordering::Less => Some(MenuKey::Down),
        std::cmp::Ordering::Greater => Some(MenuKey::Up),
        std::cmp::Ordering::Equal => None,
    }
}

/// The key toward Start on the practice page, and Enter there
/// (dev-tools).
#[cfg(feature = "dev-tools")]
fn start_key(screen: &PracticeScreen) -> MenuKey {
    if screen.editing() {
        return MenuKey::Escape;
    }
    row_key(screen, game::PracticeRow::Start).unwrap_or(MenuKey::Enter)
}

/// Moves the cursor toward selectable row `at` with the arrow keys, a batch the
/// queue holds at a time: `true` while it queued keys, `false` once the
/// row is selected (dev-tools).
#[cfg(feature = "dev-tools")]
fn move_to(screen: &SelectScreen, menu: &MenuQueue, at: usize) -> bool {
    let cursor = screen.cursor();
    if cursor == at {
        return false;
    }
    let key = if at > cursor {
        MenuKey::Down
    } else {
        MenuKey::Up
    };
    for _ in 0..at.abs_diff(cursor).min(PICK_BATCH) {
        menu.push(MenuInput::Key(key));
    }
    true
}

/// Types toward the search text `text` as the keyboard does, a step per
/// call: the box's focus, then the old text erased a bounded batch of
/// Backspaces at a time, then the new text and Enter. `true` while it
/// queued input; `false` once the box has left focus with `text` in it
/// (drained, so it is searched) (dev-tools).
#[cfg(feature = "dev-tools")]
fn type_query(screen: &SelectScreen, menu: &MenuQueue, text: &str) -> bool {
    if !screen.searching() {
        if screen.query() == text {
            return false;
        }
        menu.push(MenuInput::Key(MenuKey::Tab));
        return true;
    }
    let left = screen.query().chars().count();
    if left > 0 {
        for _ in 0..left.min(QUERY_BATCH) {
            menu.push(MenuInput::Key(MenuKey::Backspace));
        }
        return true;
    }
    menu.push(MenuInput::Text(text.to_owned()));
    menu.push(MenuInput::Key(MenuKey::Enter));
    true
}

/// Why a chosen chart's sounds or screen could not be readied: the chart is
/// refused, and selection shows why.
#[derive(Debug, thiserror::Error)]
pub enum ChartLoadError {
    /// Autoplay cannot play the chart.
    #[error("autoplay cannot play the chart")]
    Autoplay(#[source] game::AutoplayError),
    /// The sounds cannot be decoded.
    #[error("the sounds cannot be loaded")]
    Sounds(#[source] audio::BankError),
    /// The audio thread did not take them.
    #[error("audio")]
    Audio(#[source] game::AudioLoadError),
    /// The screen cannot draw the chart.
    #[error("the screen cannot draw the chart")]
    Screen(#[source] ScreenError),
    /// The screen failed while it loaded the chart: the device or the
    /// surface was lost twice, or the render thread did not answer or
    /// ended. Its error is the rig's, for the flow
    /// ([`game::AppRig::screen_load_failed`]), which ends the run.
    #[error("the screen failed while it loaded the chart")]
    ScreenFailed,
    /// The run has no select screen (never: only selection loads charts).
    #[error("no song selection")]
    NoSelection,
}

/// Whether the quit key or a window close cancels the load, or the
/// decision screen its waits serve ([`Controls::look`]) was left.
///
/// [`Controls::look`]: crate::controls::Controls::look
fn cancelled(controls: &crate::controls::Controls) -> bool {
    // The look first, whatever else holds: a terminal cause is routed by
    // the verdict it leaves, with its cause; then the ordinary
    // cancellation, a close or the quit key pressed in the loading state.
    let left = controls.look().is_some_and(game::Verdict::leaves);
    left || controls.quitting() || controls.leaving()
}

/// Past the decision screen's `deadline` while practice waits for its
/// read: whether the quit key, an Escape from the window's `menu` queue or
/// a Back from the controller's `pad` was observed at or after it, each by
/// the time its producer observed the press, taken once here; everything
/// else queued is dropped. A press observed before the deadline does not
/// leave, however late it is taken; one observed after it does, however
/// late the screen looks.
pub(crate) fn take_escape(
    controls: &crate::controls::Controls,
    menu: Option<&MenuQueue>,
    pad: &mut crate::result_input::Pad,
    deadline: TimeUs,
) -> bool {
    let quit = controls.take_quit_since(deadline);
    let escape = menu.is_some_and(|m| m.take_escape(deadline));
    let mut back = false;
    while let Some((event, _, _, at)) = pad.pop_stamped() {
        back |= event == input::MenuEvent::Back && at.0 >= deadline.0;
    }
    quit || escape || back
}

/// The chart chosen, `chart`, as the loading state's and the decision
/// screen's lines name it: from its library entry, the chosen preset and
/// the session.
fn loading_text(sel: &Selection, chart: &std::path::Path, autoplay: bool) -> LoadingText {
    let entry = sel.screen.selected().filter(|e| e.path == chart);
    let header = entry.map(|e| &e.header);
    let mut title = header
        .and_then(|h| h.title.clone())
        .unwrap_or_else(|| file_name(chart));
    if let Some(sub) = header.and_then(|h| h.subtitle.as_deref()) {
        title.push(' ');
        title.push_str(sub);
    }
    LoadingText {
        title,
        artist: header.and_then(|h| h.artist.clone()).unwrap_or_default(),
        preset: sel.screen.preset().to_owned(),
        practice: sel.practicing,
        autoplay,
        chart: entry.map_or_else(ChosenChart::default, chosen_chart),
    }
}

/// What a loading frame may show of a chart, from its library entry.
fn chosen_chart(e: &library::ChartEntry) -> ChosenChart {
    let h = &e.header;
    ChosenChart {
        title: h.title.clone(),
        subtitle: h.subtitle.clone(),
        artist: h.artist.clone(),
        subartist: h.subartist.clone(),
        genre: h.genre.clone(),
        play_level: Some(crate::imports::facts::level_of(
            h.playlevel.as_deref().unwrap_or_default(),
        )),
        difficulty: Some(crate::imports::facts::difficulty_of(
            h.difficulty.as_deref().unwrap_or_default(),
        )),
        bga: Some(e.bga),
        length_us: e.summary.as_ref().map(|s| s.length),
    }
}

/// Shows the load's lines of `page` at `stage` on the play screen, and
/// says so on the console; nothing without them.
pub(crate) fn show_stage(render: &RenderLink, page: Option<&LoadingText>, stage: Stage) {
    let Some(page) = page else {
        return;
    };
    println!("load: {}", stage.label());
    render
        .loading()
        .report(LoadInput::Stage(stage), Some(page.lines(stage)));
}

/// How often the sounds' progress is shown at most: more often would only
/// lay out pages a frame never draws, the slot keeping the newest.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// The sounds' progress on the play screen, sent from the decode pool.
struct Progress {
    render: LoadingSink,
    /// When progress was last shown.
    shown: Mutex<Option<Instant>>,
    page: Option<LoadingText>,
}

impl Progress {
    const fn new(render: LoadingSink, page: Option<LoadingText>) -> Self {
        Self {
            render,
            shown: Mutex::new(None),
            page,
        }
    }

    /// `done` of `files` decoded: shown at most every [`PROGRESS_EVERY`],
    /// and when the last is done.
    fn decoded(&self, done: usize, files: usize) {
        let Some(page) = &self.page else {
            return;
        };
        let mut shown = self.shown.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if done < files && shown.is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY) {
            return;
        }
        *shown = Some(now);
        let stage = Stage::Sounds { done, files };
        self.render
            .report(LoadInput::Stage(stage), Some(page.lines(stage)));
    }

    /// The bank is complete, an empty one included: the lines stay.
    fn completed(&self) {
        if self.page.is_some() {
            self.render.report(LoadInput::BankComplete, None);
        }
    }
}

/// How often a slow load prints the loading frames presented (dev-tools).
#[cfg(feature = "dev-tools")]
const HOLD_CHECKPOINT: Duration = Duration::from_millis(200);

/// The longest a slow load holds waiting for its loading frames
/// (dev-tools).
#[cfg(feature = "dev-tools")]
const HOLD_MOST: Duration = Duration::from_secs(20);

/// Holds a load for a script's slow load (dev-tools), pressing the quit
/// key or closing the window halfway if it says; whether the quit key or a
/// close cancelled it, checked as the measurement does. Every
/// [`HOLD_CHECKPOINT`] it reports the loading frames presented so far
/// (`drawn`, to `report`), and it holds its time, and on until those printed
/// checkpoints show the frames following one another
/// ([`crate::dev::checkpoints::frames_follow`], the rule its test checks),
/// at most [`HOLD_MOST`], so even a slow machine shows them.
#[cfg(feature = "dev-tools")]
fn hold(
    controls: &Controls,
    slow: crate::dev::SlowLoad,
    drawn: &dyn Fn() -> u64,
    report: &mut dyn FnMut(u64),
) -> bool {
    println!("dev: the load holds {} ms", slow.ms);
    let began = Instant::now();
    let total = Duration::from_millis(slow.ms);
    let mut acted = false;
    let mut checkpoint = began;
    // Every checkpoint printed.
    let mut printed = Vec::new();
    loop {
        // The hold's look serves the decision screen too.
        if cancelled(controls) {
            return true;
        }
        let held = began.elapsed();
        if checkpoint.elapsed() >= HOLD_CHECKPOINT {
            checkpoint = Instant::now();
            let frames = drawn();
            report(frames);
            printed.push(frames);
        }
        if held >= total && (crate::dev::checkpoints::frames_follow(&printed) || held >= HOLD_MOST)
        {
            return false;
        }
        // Its quit or close in the loading state, once the decision screen
        // that discards every press has passed.
        if !acted && held >= total / 2 && controls.ingress().is_open() {
            acted = true;
            if slow.quit {
                controls.command(game::Command::Quit);
            }
            if slow.close {
                controls.close();
            }
            continue;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl PlayerRig<'_> {
    /// Selection is left (the decision screen, loading or the practice
    /// page): nothing of it is drawn again, and the window queues no
    /// pointer input until the next entry, nothing in flight dispatched.
    fn left_selection(&mut self) {
        self.render.leave_selection();
        if let Some(sel) = &mut self.selection {
            sel.pointer.leave();
        }
    }

    /// The release barrier (design.md "Load Pipeline"), on the game thread
    /// before a bank decode begins: every earlier bank's owners go first.
    /// The scoped jobs of the previous load were joined when it returned;
    /// the previous chart's restorer is dropped (it holds the engine's
    /// bank, and would restore no round of this chart); the engine's bank
    /// is unloaded, freed here once the audio thread took the empty one.
    /// The practice page's jobs are settled first and its kept bank
    /// dropped, unless `kept` is that bank, which a Start at 1× installs.
    /// The witness then finds no decode in flight and no bank alive but
    /// `kept` (dev-tools).
    ///
    /// The unload and its settle serve the decision screen when it is
    /// watched: a leaving verdict a look returns stops them
    /// ([`game::Served::Left`]), the unload left to the audio thread, its
    /// answer taken by the next switch.
    ///
    /// # Errors
    ///
    /// The unload's ([`game::AudioControl::unload`]): nothing is decoded.
    fn release_banks(
        &mut self,
        kept: Option<&Arc<audio::SoundBank>>,
    ) -> Result<game::Served<crate::page_jobs::Settlement>, game::AudioLoadError> {
        let settled = self.release_owners(kept);
        let unload = self.unload_switch()?;
        if let game::Served::Left(left) = self.switch_served(unload)? {
            return Ok(game::Served::Left(left));
        }
        #[cfg(feature = "dev-tools")]
        if let Some(sel) = &self.selection {
            sel.witness.check(kept);
        }
        Ok(game::Served::Done(settled))
    }

    /// The release barrier's first half: the previous chart's restorer
    /// dropped and the page's jobs settled, its kept bank dropped unless it
    /// is `kept`. The settle's bound starts at a terminal cause, or at a
    /// leaving verdict of the watched decision screen's look.
    fn release_owners(
        &mut self,
        kept: Option<&Arc<audio::SoundBank>>,
    ) -> crate::page_jobs::Settlement {
        self.restorer = None;
        let looks = looks!(self);
        let controls = self.controls;
        let settled = self.selection.as_mut().map(|sel| {
            sel.page
                .settle(kept.is_some(), &mut || {
                    let left = controls.look().is_some_and(game::Verdict::leaves);
                    looks
                        .terminal()
                        .or_else(|| left.then_some(crate::page_jobs::Terminal))
                })
                .failure()
        });
        #[cfg(test)]
        {
            let decodes = self.selection.as_ref().and_then(|s| s.decodes.clone());
            probe(decodes.as_deref(), Probe::Barrier, self.controls, &|| false);
        }
        crate::page_jobs::Settlement::of(settled.flatten())
    }

    /// The release barrier's unload, made and not sent.
    fn unload_switch(&mut self) -> Result<game::Switch, game::AudioLoadError> {
        #[cfg(test)]
        if let Some(fault) = self.selection.as_mut().and_then(|s| s.unload_fault.take()) {
            return Err(fault);
        }
        Ok(self.audio.control.unload_switch())
    }

    /// Polls `switch` to its answer or its bound ([`game::SWITCH_TIMEOUT`]),
    /// the watched decision screen looked at between polls: a leaving
    /// verdict stops the wait ([`game::Served::Left`]); the play's step
    /// runs inside the look and the wait goes on. Unwatched, the wait is
    /// [`game::AudioControl::load`]'s, a full control queue retried
    /// within the bound.
    ///
    /// # Errors
    ///
    /// The switch's answer, or its bound's.
    fn switch_served(
        &mut self,
        mut switch: game::Switch,
    ) -> Result<game::Served<()>, game::AudioLoadError> {
        loop {
            if let game::SwitchPoll::Done(done) = self.audio.control.poll_switch(&mut switch) {
                return done.map(game::Served::Done);
            }
            if let Some(left) = self.controls.look().filter(|v| v.leaves()) {
                return Ok(game::Served::Left(left));
            }
            if let Some(expired) = self.audio.control.expired(&switch) {
                return Err(expired);
            }
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Gives the select screen's updates the settings a skin shows
    /// (docs/stage3.md §3, Selection): the play options in force, the
    /// chosen preset's displayed gauge and the settings keys' values.
    fn feed_settings(&mut self) {
        let options = self
            .adjust
            .as_ref()
            .map(game::AdjustDriver::options)
            .or_else(|| self.files.play_options());
        let tuning = self.controls.tuning();
        let gauge = self.gauge;
        let on_failure = self.files.on_failure();
        if let Some(sel) = &mut self.selection {
            sel.feed.settings = crate::selection_skin::settings_of(
                options,
                sel.screen.bests_preset(),
                gauge,
                on_failure,
                sel.screen.order_view(),
                tuning,
            );
        }
    }

    /// Whether a loading lifetime runs: from the choice or a practice's
    /// Start until loading ends.
    pub(crate) fn loading_active(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| s.loading.is_some())
    }

    /// Looks at the library and the window's input on the select screen,
    /// and sends the page if it changed; the chart chosen, if one was.
    fn look(&mut self) -> Option<Pick> {
        let sel = self.selection.as_mut()?;
        if sel.shelf.poll(Instant::now()) {
            sel.refresh();
        }
        // The selection skin refused at runtime: the page draws from now
        // on, and says why.
        if let Some(why) = self.render.take_selection_refusal() {
            log::warn!("select: the selection skin was refused, the built-in page draws: {why}");
            let notice = crate::imports::notice(skin_model::ScreenKind::Selection, &why);
            sel.screen.set_notice(Some(notice));
            sel.dirty = true;
        }
        // The panel the keys find open, then the one they leave (a search
        // box taking focus closes it).
        sel.panels(self.panels.as_ref(), self.clock.now());
        #[cfg_attr(
            not(feature = "dev-tools"),
            expect(unused_mut, reason = "scripts choose too")
        )]
        let mut chosen = sel.drain_owned(
            &mut self.result_input,
            self.panels.as_ref(),
            self.clock.now(),
        );
        sel.panels(self.panels.as_ref(), self.clock.now());
        #[cfg(feature = "dev-tools")]
        if chosen.is_none()
            && sel.script(
                self.controls,
                self.input.wakes(),
                self.render.selection_drawn(),
                self.scenario.as_ref().map(|s| &s.captures),
            )
        {
            chosen = sel.drain_owned(
                &mut self.result_input,
                self.panels.as_ref(),
                self.clock.now(),
            );
        }
        // Before anything of the choice: no chart without its skin.
        let now = self.clock.now();
        let chosen = sel.admit(chosen, &mut self.skin, &self.render, now);
        if let Some(pick) = &chosen {
            let (Pick::Play(path) | Pick::Practice(path)) = pick;
            let what = if matches!(pick, Pick::Practice(_)) {
                " to practice"
            } else {
                ""
            };
            #[cfg(feature = "dev-tools")]
            if let Some(script) = &mut sel.script {
                script.chose();
            }
            sel.shelf.choose(path);
            sel.chosen_at = self.clock.now();
            let name = file_name(path);
            let after = since(sel.entered_at, sel.chosen_at).as_millis();
            println!("select: chose {name}{what} {after} ms after entering selection");
            log::info!("select: chose {}{what}", path.display());
            sel.dirty = true;
            sel.practicing = matches!(pick, Pick::Practice(_));
            // The decision screen follows at once (`entered_decision`).
        } else {
            sel.show(&self.render, None, now);
        }
        chosen
    }

    /// Before each READY state, with song selection: the scan's pause is
    /// requested if it is not, and acknowledged before recording opens. A
    /// close or the quit key cancels the wait; the play then does not start
    /// (`game::live` takes the quit, or the close, before it opens).
    pub(crate) fn pause_scan(&mut self) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        let controls = self.controls;
        let render = &self.render;
        let Selection {
            shelf,
            screen,
            loading,
            ..
        } = sel;
        let began = Instant::now();
        let paused = shelf.wait_paused(
            crate::shelf::PAUSE_SLICE,
            // Closing the window or the quit key ends the wait, and the
            // play: READY never starts unacknowledged.
            || controls.quitting() || controls.leaving(),
            crate::shelf::PAUSE_NOTICE,
            || {
                println!("library: waiting for the scan to pause");
                // On the play screen before a chart's first READY state; a
                // retry's shows on the select page, as the scan's state.
                if loading.is_some() {
                    show_stage(render, loading.as_ref(), Stage::Scan);
                } else {
                    let page = screen.page(Label::ScanPauseWait.text());
                    render.send(RenderMsg::Menu(Box::new(page)));
                }
            },
        );
        match paused {
            crate::shelf::Paused::Acknowledged => {
                let line = format!(
                    "library: the scan paused before READY in {} ms",
                    began.elapsed().as_millis()
                );
                println!("{line}");
                log::info!("{line}");
            }
            crate::shelf::Paused::Idle => println!("library: no scan before READY"),
            crate::shelf::Paused::Cancelled => {
                println!("library: the wait for the scan was cancelled");
                return;
            }
        }
        // The scan is quiet: a measurement may be stored now.
        shelf.store_loudness();
    }

    /// Prints which preset the result views when that changed, and how
    /// long a new evaluation took, with any error's full chain.
    pub(crate) fn print_view(&mut self, screen: &ResultScreen) {
        if let (Some(t), Some(entry)) = (screen.last_evaluation(), screen.viewed())
            && screen.evaluations() > self.evaluated
        {
            println!("result: evaluated {} in {} ms", entry.name, t.as_millis());
            self.evaluated = screen.evaluations();
        }
        let view = view_log(screen);
        if view.is_some() && view != self.viewed {
            if let Some((_, line)) = &view {
                println!("{line}");
                log::info!("{line}");
            }
            self.viewed = view;
        }
    }

    /// A result shows: the scan goes on, and a scripted step for it runs.
    pub(crate) fn result_shown(&mut self) {
        let Some(sel) = &mut self.selection else {
            #[cfg(feature = "dev-tools")]
            if let Some(script) = &mut self.result_script {
                script_result(
                    script,
                    self.controls,
                    &self.render,
                    &self.result_input.menu,
                    None,
                );
            }
            return;
        };
        sel.shelf.release();
        #[cfg(feature = "dev-tools")]
        sel.script_result(
            self.controls,
            &self.render,
            self.scenario.as_ref().map(|s| &s.captures),
        );
    }

    /// Before a play's screen is readied (a scenario): the `play` step due
    /// is the one a failure from here to the play's start is named by.
    /// Once a practice's rounds have all started, the next round is left
    /// here, at the round boundary, by the quit key, before its attempt is
    /// prepared or anything opens, so it never plays: the rounds return to
    /// the practice page however late the game thread takes the key.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn scenario_pending(&mut self) {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return;
        };
        script.announce();
        if let Some(&SelectStep::Rounds(r)) = script.front()
            && sc.rounds >= r
        {
            sc.rounds = 0;
            sc.pending = None;
            script.done();
            self.controls.command(game::Command::Quit);
            return;
        }
        sc.pending = matches!(
            script.front(),
            Some(SelectStep::Play(_) | SelectStep::Rounds(_))
        )
        .then(|| script.origin())
        .flatten();
    }

    /// A play started (a scenario): its `play` step is done, its `until`
    /// armed and the captures armed before it bound to it. A play where no
    /// `play` step is due fails the run.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn scenario_started(&mut self) {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return;
        };
        sc.pending = None;
        match script.front() {
            Some(&SelectStep::Play(p)) => {
                script.done();
                sc.until = p.until;
                sc.captures.bind(self.generation);
            }
            // A practice's round, counted; the one after the last is left
            // before it opens ([`Self::scenario_pending`]).
            Some(&SelectStep::Rounds(_)) => {
                sc.until = None;
                sc.captures.bind(self.generation);
                sc.rounds = sc.rounds.saturating_add(1);
            }
            _ => {
                script.fail("a play started where this step was due".to_owned());
                self.controls.close();
            }
        }
    }

    /// Runs the scripted step due on the practice page (a scenario): a key
    /// toward the page its `practice` step sets, Start, or Back, on the
    /// window's queue as the keyboard does, one key per showing of the
    /// page. `true` when it queued one. A step that cannot be done there
    /// fails the run.
    #[cfg(feature = "dev-tools")]
    fn script_practice(&mut self) -> bool {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return false;
        };
        match drive_practice(script, sc) {
            Driven::Key(key) => {
                self.result_input.menu.push(MenuInput::Key(key));
                true
            }
            Driven::Wait => false,
            Driven::Failed => {
                self.controls.close();
                false
            }
        }
    }

    /// A play ended (a scenario): an `until` sent during it fails the run
    /// when the play did not land where its command says (a quit after
    /// completion, a skip before it).
    #[cfg(feature = "dev-tools")]
    pub(crate) fn scenario_ended(&mut self, report: &game::PlayReport) {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return;
        };
        let Some((sent, at)) = sc.sent.take() else {
            return;
        };
        if let (Some(why), Some(o)) = (sent.missed(report, self.autoplay, at), script.last_done()) {
            script.fail_at(o, why);
            self.controls.close();
        }
    }

    /// A play is over and its frames drawn (a scenario): an `until` it
    /// never reached, and each capture bound to it that failed, fail the
    /// run; each written is printed.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn scenario_over(&mut self) {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return;
        };
        let play = script.last_done();
        let mut failed = false;
        if sc.until.take().is_some()
            && let Some(o) = play
        {
            script.fail_at(o, "the play ended before at_ms");
            failed = true;
        }
        match sc.captures.settle(self.generation, self.figures.is_some()) {
            Ok(crate::dev::scenario::Settled {
                written,
                failed: refused,
            }) => {
                for (o, path) in written {
                    println!("dev: scenario: {o}: wrote {}", path.display());
                }
                for (o, why) in refused {
                    script.fail_at(o, &why);
                    failed = true;
                }
            }
            Err(()) => {
                if let Some(o) = play {
                    script.fail_at(o, "the render thread did not answer");
                }
                failed = true;
            }
        }
        if failed {
            self.controls.close();
        }
    }

    /// The input the loaded chart plays (a scenario): autoplay's generated
    /// input, or its log replayed from its start at every play, each on a
    /// source set started over, and what a log's plays record.
    /// Returns `feed`, autoplay's input as made, when no scenario takes it.
    #[cfg(feature = "dev-tools")]
    fn scenario_input(&mut self, feed: Option<Feed>) -> Option<Feed> {
        let Some(sc) = &self.scenario else {
            return feed;
        };
        match (sc.chart_input, feed) {
            (Some(InputRef::Auto), Some((input, capacity, recorded))) => {
                let sources = input::SourceConfig {
                    queue_capacity: capacity,
                    ..crate::source_config()
                };
                self.input.switch(Mode::Autoplay(input), sources);
                self.recorded.keyboard = recorded;
                None
            }
            (Some(InputRef::Log(k)), feed) => {
                if let Some(log) = sc.logs.get(usize::from(k)) {
                    let script = crate::input_side::Script {
                        log: log.clone(),
                        current: log.clone(),
                        sources: crate::source_config(),
                        loading: None,
                        rearm_delay: TimeUs(0),
                    };
                    self.input
                        .switch(Mode::Script(Box::new(script)), crate::source_config());
                }
                self.recorded = sc.recorded.clone();
                feed
            }
            (_, feed) => feed,
        }
    }
}

/// The waiting page bank's look at `chart`'s background measurement, with
/// no wait ([`Shelf::background_ready`]). The look requests the scan's
/// pause; one it requests after the page released it is the page's own,
/// held until the bank is admitted.
///
/// [`Shelf::background_ready`]: crate::shelf::Shelf::background_ready
fn page_background_ready(sel: &mut Selection, chart: &std::path::Path, name: &str) -> bool {
    let was = sel.shelf.paused();
    let ready = sel.shelf.background_ready(chart, name);
    if !was && sel.shelf.paused() {
        sel.page.hold_pause();
    }
    ready
}

/// Why the page's waiting bank did not begin.
enum PageBegin {
    /// The release barrier found a failure the run ends with.
    Failed(game::coord::LoadFailure),
    /// The audio thread refused the barrier's unload.
    Refused(game::AudioLoadError),
}

impl PlayerRig<'_> {
    /// The page's waiting 1× bank ([`PageJobs::defer`]) begins once its
    /// prerequisites hold, each looked at with no wait, so the decision
    /// screen and the page are served meanwhile and the page opens at its
    /// deadline whichever step this is at: waiting for the chart's
    /// background measurement (its result kept); then the release barrier,
    /// its owners released at once and its unload kept in the page and
    /// polled at each look; then, the unload answered, the background
    /// looked at again (the page's entry may have let it resume meanwhile)
    /// and only then the bank begun, or back to waiting for it.
    ///
    /// [`PageJobs::defer`]: crate::page_jobs::PageJobs::defer
    fn begin_page_when_ready(&mut self) -> Result<(), PageBegin> {
        let Some(sel) = &mut self.selection else {
            return Ok(());
        };
        let Some(path) = sel.page.pending_path().map(std::path::Path::to_path_buf) else {
            return Ok(());
        };
        let name = file_name(&path);
        if sel.page.unloading().is_none() {
            if !page_background_ready(sel, &path, &name) {
                return Ok(());
            }
            if let Some(failure) = self.release_owners(None).failure() {
                return Err(PageBegin::Failed(failure));
            }
            let unload = self.unload_switch().map_err(PageBegin::Refused)?;
            if let Some(sel) = &mut self.selection {
                sel.page.set_unloading(Some(unload));
            }
        }
        let Some(sel) = &mut self.selection else {
            return Ok(());
        };
        let Some(unload) = sel.page.unloading() else {
            return Ok(());
        };
        let answered = match self.audio.control.poll_switch(unload) {
            game::SwitchPoll::Done(answered) => answered,
            game::SwitchPoll::Pending => match self.audio.control.expired(unload) {
                Some(expired) => Err(expired),
                None => return Ok(()),
            },
        };
        sel.page.set_unloading(None);
        answered.map_err(PageBegin::Refused)?;
        // Admitted only beside no background bank, looked at again.
        if !page_background_ready(sel, &path, &name) {
            return Ok(());
        }
        #[cfg(feature = "dev-tools")]
        sel.witness.check(None);
        // The page stays: the scan goes on, as the page's entry let it.
        sel.page
            .begin_pending(
                &sel.coord,
                sel.spawner,
                #[cfg(feature = "dev-tools")]
                &sel.witness,
            )
            .release(&mut sel.shelf);
        Ok(())
    }

    /// `f` with `decision` moved to the waits it owns
    /// ([`Controls::look`], reached from every wait's stop), when
    /// the screen shows; `decision` comes back with `f`'s answer, as they
    /// left it.
    ///
    /// [`Controls::look`]: crate::controls::Controls::look
    fn watching<T>(
        &mut self,
        chart: &std::path::Path,
        decision: game::Decision,
        f: impl FnOnce(&mut Self) -> T,
    ) -> (T, game::Decision) {
        if !decision.on() {
            return (f(self), decision);
        }
        let reveal = self.render.reveal_handle();
        let name = file_name(chart);
        // Practice is watched before its chart is read: its deadline sets
        // the screen waiting for the read; it never steps in a rig's wait.
        let origin = if decision.practice() {
            game::Origin::Read
        } else {
            game::Origin::Load
        };
        self.controls
            .watch_decision(crate::controls::DecisionWatch {
                decision,
                origin,
                reveal: Box::new(move || {
                    println!("load: the play screen loads {name}");
                    reveal();
                }),
                failures: self.failures.clone(),
                screen: Some(self.render.recovery_handle()),
            });
        let out = f(self);
        // Only this wait unwatches what it watched.
        let decision = self
            .controls
            .unwatch_decision()
            .unwrap_or_else(game::Decision::off);
        (out, decision)
    }

    /// [`AppRig::load_chart`]'s load, its waits' stops serving the
    /// decision screen when one is watched.
    fn load_watched(&mut self, loaded: &Loaded) -> Result<Loading, ChartLoadError> {
        // A cause another load left is not this load's.
        self.screen_load_failure = None;
        // The chart, speed and section are final: the presentation solves
        // for them once, so what the player adjusts while loading applies
        // to this attempt. A refusal is reported by the play's preparation
        // ([`game::Rig::begin_presentation`]), which attaches again: a
        // refused attach leaves nothing pending. A failed
        // fence is latched in the driver and ends the run.
        if let Some((driver, mut pump)) = self.adjustments_watched()
            && let Ok(game::Served::Left(_)) = driver.attach_loaded_served(loaded, &mut pump)
        {
            // A look inside the fence left the decision screen.
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        // A practice's Start chooses again: its load times run from it.
        let now = self.clock.now();
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        if loaded.section().is_some() {
            sel.chosen_at = now;
            // A Start decodes its own: a page bank still waiting for the
            // background is given up, and the Start waits for it itself.
            if let Some((_, pause)) = sel.page.take_pending() {
                // The Start's load keeps the scan paused (`choose` below).
                pause.hand_over();
                sel.shelf.stop_awaiting();
            }
        }
        // Requested again after the practice page released it, as a
        // choice does.
        sel.shelf.choose(loaded.path());
        let chosen_at = sel.chosen_at;
        let page = sel.loading.clone();
        #[cfg(feature = "dev-tools")]
        let slow = sel.script.as_mut().and_then(|s| s.slow_load.take());
        let chart = loaded.chart();
        let warnings = loaded.warnings();
        log::info!("loaded the chart ({} warnings)", warnings.len());
        log_file::warnings("chart", warnings.iter());
        log_file::missing_sounds(
            chart
                .sounds
                .iter()
                .filter(|s| s.path.is_none())
                .map(|s| s.name.as_str()),
        );
        let read = self.clock.now();
        // Autoplay's input and schedule first: a chart it cannot play is
        // refused before its sounds are decoded.
        let (schedule, feed) = if self.autoplay {
            let made = loaded.autoplay().map_err(ChartLoadError::Autoplay)?;
            let capacity = made.queue_capacity();
            let feed = (
                input::AutoplayInput::new(made.events),
                capacity,
                made.recorded,
            );
            (made.schedule, Some(feed))
        } else {
            // A practice's rounds play the BGM with the key sounds of the
            // notes that are not targets.
            (loaded.audio_schedule(), None)
        };
        // The screen prepares the chart on the render thread while the
        // sounds decode and the loudness is measured: it needs neither. A
        // load that stops before its answer leaves the screen the chart,
        // which the next load replaces.
        let screen = self
            .render
            .begin_load(Arc::new(chart.clone()), loaded.section().cloned());
        let rounds = loaded.section().map(|_| schedule.clone());
        let speed = loaded.config().speed;
        // A measurement running in the background is waited for before the
        // sounds decode, so its 1× bank is gone before this load's: this
        // chart's to its end, whose result the load then finds, another's
        // until it has stopped for the pause.
        let waiting = self.clock.now();
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        let controls = self.controls;
        let render = &self.render;
        let waited = sel.shelf.wait_background(
            loaded.path(),
            &file_name(loaded.path()),
            || cancelled(controls),
            || show_stage(render, page.as_ref(), Stage::Loudness),
        );
        if waited == Loading::Cancelled {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        let background = since(waiting, self.clock.now());
        show_stage(
            &self.render,
            page.as_ref(),
            Stage::Sounds { done: 0, files: 0 },
        );
        #[cfg(feature = "dev-tools")]
        if let Some(slow) = slow
            && hold(
                self.controls,
                slow,
                &|| self.render.loading_drawn(),
                &mut |n| println!("dev: presented loading frames: {n}"),
            )
        {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        let progress = Arc::new(Progress::new(self.render.loading(), page.clone()));
        // A practice Start of the page's chart takes the page's 1× bank and
        // knows its gain before any bank of its own: at 1× it installs that
        // bank, at another speed the barrier releases it first (design.md
        // "Load Pipeline").
        let practised = loaded
            .section()
            .and(self.selection.as_ref())
            .and_then(|s| s.page.chart_of(loaded.path()).cloned());
        let mut retried = false;
        let bank = loop {
            // At another speed with the gain known, the 1× bank is not
            // needed. This only spares the decode: the speed's bank is
            // checked against the gain's key below whatever it says.
            let known = speed != bms_model::Speed::NORMAL
                && self.selection.as_ref().is_some_and(|s| s.page.gain_known());
            let mut reused = None;
            if let Some(practised) = practised.as_ref().filter(|_| !known) {
                let bank = match self.page_bank(practised, &progress)? {
                    Got::Ready(bank) => bank,
                    Got::Cancelled => {
                        println!("load: cancelled");
                        return Ok(Loading::Cancelled);
                    }
                    Got::Failed(failure) => return Ok(Loading::Failed(failure)),
                };
                show_stage(&self.render, page.as_ref(), Stage::Loudness);
                match self.page_gain(practised, &bank, loaded.source())? {
                    Got::Ready(()) => {}
                    Got::Cancelled => {
                        println!("load: cancelled");
                        return Ok(Loading::Cancelled);
                    }
                    Got::Failed(failure) => return Ok(Loading::Failed(failure)),
                }
                if speed == bms_model::Speed::NORMAL {
                    reused = Some(bank);
                }
            }
            // Every earlier bank's owners go before this decode begins (the
            // page's kept bank stays when this Start installs it); an unload
            // the audio thread does not take refuses the load here.
            match self
                .release_banks(reused.as_ref())
                .map_err(ChartLoadError::Audio)?
            {
                game::Served::Left(_) => {
                    println!("load: cancelled");
                    return Ok(Loading::Cancelled);
                }
                game::Served::Done(settled) => {
                    if let Some(failure) = settled.failure() {
                        return Ok(Loading::Failed(failure));
                    }
                }
            }
            // A quit or a close during the unload, whose wait reads neither:
            // nothing is decoded.
            if cancelled(self.controls) {
                println!("load: cancelled");
                return Ok(Loading::Cancelled);
            }
            let bank = match reused {
                Some(bank) => bank,
                None => match self.decode_bank((loaded.path(), chart), speed, &progress)? {
                    Got::Ready(bank) => bank,
                    Got::Cancelled => {
                        println!("load: cancelled");
                        return Ok(Loading::Cancelled);
                    }
                    Got::Failed(failure) => return Ok(Loading::Failed(failure)),
                },
            };
            if practised.is_some()
                && speed != bms_model::Speed::NORMAL
                && !self.speed_gain(chart, &bank, &mut retried)?
            {
                drop(bank);
                continue;
            }
            break bank;
        };
        log_file::warnings("sounds", bank.warnings().iter().map(|w| format!("{w:?}")));
        let decoded = self.clock.now();
        #[cfg(test)]
        if let Some(gate) = self.selection.as_ref().and_then(|s| s.after_bank.as_ref()) {
            gate();
        }
        // A quit or a close since the decode's last check: the bank is not
        // installed.
        if cancelled(self.controls) {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        // The engine takes the new bank in place of the empty one the
        // release barrier left: the previous chart's bank was freed before
        // the decode.
        let install = self
            .audio
            .control
            .switch(Arc::clone(&bank), audio::BgmSchedule::new(schedule))
            .map_err(ChartLoadError::Audio)?;
        if let game::Served::Left(_) = self.switch_served(install).map_err(ChartLoadError::Audio)? {
            // The install left to the audio thread: the next switch
            // replaces it before any play.
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        // The rounds restore against the engine this load answered with.
        self.restorer = rounds.map(|s| game::Restorer::new(self.audio.control.seek_source(), s));
        let engine = self.clock.now();
        // Its gain, from the cache, or measured now on the Measure job's
        // thread, before its first play; a practice Start knew it before.
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        let name = sel
            .screen
            .selected()
            .map_or_else(|| "the chart".to_owned(), |e| file_name(&e.path));
        if let Some(key) = sel.shelf.look_up(chart, &bank, &name, loaded.path())
            && practised.is_none()
        {
            show_stage(&self.render, page.as_ref(), Stage::Loudness);
            match self.measure_scoped(chart, &bank, key, &name, loaded.source())? {
                Got::Ready(m) => {
                    let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
                    sel.shelf.keep_measured(key, m);
                }
                Got::Cancelled => {
                    // Closing or the quit key: nothing more is prepared,
                    // and the flow leaves the chart.
                    println!("load: cancelled");
                    return Ok(Loading::Cancelled);
                }
                Got::Failed(failure) => return Ok(Loading::Failed(failure)),
            }
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::print_adjustments("measuring", self.adjust.as_ref());
        let measured = self.clock.now();
        // A cached measurement asks nothing: the screen is not waited for
        // after a quit or a close.
        if cancelled(self.controls) {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        show_stage(&self.render, page.as_ref(), Stage::Screen);
        #[cfg(test)]
        let (decodes, controls) = (
            self.selection.as_ref().and_then(|s| s.decodes.clone()),
            self.controls,
        );
        wait_chart_load(
            screen,
            &self.render,
            self.adjust.as_mut(),
            &self.input,
            self.controls,
            #[cfg(test)]
            &|| probe(decodes.as_deref(), Probe::Screen, controls, &|| false),
        )
        .map_err(|e| match e {
            // A skin's refusal refuses the chart.
            ScreenError::Render(r) if r.is_refusal() => {
                ChartLoadError::Screen(ScreenError::Render(r))
            }
            // Anything else is the screen's failure, which ends the run.
            lost => {
                self.screen_load_failure = Some(lost);
                ChartLoadError::ScreenFailed
            }
        })?;
        if self.controls.decision_left() {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        #[cfg(feature = "dev-tools")]
        crate::dev::print_adjustments("chart-load", self.adjust.as_ref());
        let ready = self.clock.now();
        #[cfg(feature = "dev-tools")]
        let feed = self.scenario_input(feed);
        if let Some((input, capacity, recorded)) = feed {
            self.input.feed(input, capacity);
            self.recorded.keyboard = recorded;
        }
        // This chart's load times run from its choice.
        self.times = LoadTimes::default();
        self.load_start = chosen_at;
        self.pending_retry = None;
        self.retry_from = None;
        let ms = |from: TimeUs, to: TimeUs| since(from, to).as_millis();
        println!(
            "load: the chart is ready {} ms after its choice (chart {} ms, sounds {} ms, loudness {} ms, audio {} ms, screen {} ms)",
            ms(chosen_at, ready),
            ms(chosen_at, read),
            // The wait for the background's measurement is the loudness's.
            ms(read, decoded).saturating_sub(background.as_millis()),
            ms(engine, measured) + background.as_millis(),
            ms(decoded, engine),
            ms(measured, ready),
        );
        Ok(Loading::Ready)
    }
}

impl AppRig for PlayerRig<'_> {
    type BankError = ChartLoadError;

    /// A scenario's chart plays the input of its `play` step: autoplay
    /// unsaved, or a log saved and counted.
    #[cfg(feature = "dev-tools")]
    fn configure(&mut self, config: &mut game::SessionConfig, parts: &mut game::Parts) {
        let Some(sc) = &mut self.scenario else {
            return;
        };
        let script = self.selection.as_ref().and_then(|s| s.script.as_ref());
        // A chart's input comes from its `play` step, or from its
        // `practice` step's page keys, P pressed or about to be.
        let input = match script.and_then(crate::dev::SelectScript::front) {
            Some(&SelectStep::Play(p)) => p.input,
            Some(&SelectStep::PracticeKey(input)) => input,
            Some(&SelectStep::PracticeSet(goal)) => goal.input,
            _ => {
                sc.chart_input = None;
                return;
            }
        };
        sc.chart_input = Some(input);
        sc.practice = None;
        sc.rounds = 0;
        sc.nearest = None;
        match input {
            InputRef::Auto => {
                config.autoplay = Some(game::AutoKeySounds::Scheduled);
                parts.records = None;
                self.autoplay = true;
            }
            InputRef::Log(_) => {
                config.autoplay = None;
                self.autoplay = false;
            }
        }
    }

    fn choose(&mut self) -> Option<Pick> {
        self.look()
    }

    /// The play screen's loading state, its lines from the chart's library
    /// entry (its title and artist) and the chosen preset, at the chart's
    /// stage: a loading lifetime begins, with the shown gauge's descriptor
    /// from the validated preset selection has chosen.
    fn entered_loading(&mut self, chart: &std::path::Path) {
        // A new generation of the chart's images before the loading
        // lifetime begins: none of the last one's draws in it.
        self.chart_images.renew();
        // The coming attempt's entry: its loading-time adjustments are its
        // own, and solve nothing until its chart is read. A failed fence is
        // the input thread's failure, which ends the run.
        self.enter_attempt();
        // Tuning's loading epoch: a settings key down now holds the
        // loading state until released, and steps nothing.
        self.begin_loading_epoch();
        // Selection is left: nothing of it is drawn again.
        self.left_selection();
        let Some(sel) = &mut self.selection else {
            return;
        };
        let page = loading_text(sel, chart, self.autoplay);
        println!("load: the play screen loads {}", file_name(chart));
        let gauge = sel
            .screen
            .bests_preset()
            .and_then(|p| crate::play::loading_gauge(p, self.gauge));
        self.render.begin_loading(&page, gauge);
        show_stage(&self.render, Some(&page), Stage::Chart);
        sel.loading = Some(page);
    }

    fn load_cancelled(&self) -> bool {
        cancelled(self.controls)
    }

    /// The built-in presentation, for every choice.
    fn decision_shown(
        &mut self,
        _pick: &Pick,
        _lifetime: game::DecisionLifetime,
    ) -> Option<game::DecisionShown> {
        Some(game::DecisionShown::BuiltIn)
    }

    /// The decision screen begins: the input epoch first, so nothing
    /// observed before the choice acts after it, then the built-in
    /// presentation and its tone, and a play's loading lifetime, made now
    /// and held until the deadline reveals it; then the attempt's entry
    /// (its loading-time adjustments are its own), whose fence waits on the
    /// input thread with the screen already showing.
    fn entered_decision(&mut self, pick: &Pick, d: game::Decision) -> game::Decision {
        // A new generation of the chart's images before the held loading
        // lifetime begins: none of the last choice's draws when the
        // deadline reveals it.
        self.chart_images.renew();
        self.controls.close_epoch();
        if let Some(ime) = self.selection.as_ref().and_then(|s| s.ime.as_ref()) {
            ime.set(false);
        }
        self.left_selection();
        let (Pick::Play(chart) | Pick::Practice(chart)) = pick;
        let Some(sel) = &mut self.selection else {
            return d;
        };
        let page = loading_text(sel, chart, self.autoplay);
        println!("decision: {} chosen", file_name(chart));
        let shown = page.decision();
        self.render.show_decision(shown.clone());
        sel.decision = Some((shown, file_name(chart)));
        self.audio.control.play_tone();
        if matches!(pick, Pick::Play(_)) {
            let gauge = sel
                .screen
                .bests_preset()
                .and_then(|p| crate::play::loading_gauge(p, self.gauge));
            self.render.begin_loading_held(&page, gauge);
            show_stage(&self.render, Some(&page), Stage::Chart);
            sel.loading = Some(page);
        }
        // The attempt's entry last: its fence waits on the input thread with
        // the screen and its tone already out, serving the decision screen
        // (a leaving verdict is left in `d` for App to route).
        let ((), d) = self.watching(chart, d, |rig| {
            rig.next_window_scene();
            if let Some((driver, mut pump)) = rig.adjustments_watched() {
                let _ = driver.enter_attempt_served(None, &mut pump);
            }
        });
        d
    }

    fn screen_load_failed(&mut self) -> Option<ScreenError> {
        self.screen_load_failure.take()
    }

    fn begin_recovery(&mut self) {
        self.before_rebuild();
        // Through the rebuild's one owner: one under way (a look's) is
        // adopted, a lost screen with none gets one; a failure saved there
        // is reported by the first poll.
        let _ = self.render.serve_recovery(None, false);
    }

    fn poll_recovery(&mut self, until: TimeUs) -> Option<Result<(), ScreenError>> {
        let left = until.0.saturating_sub(self.clock.now().0).max(0);
        let until = Instant::now() + Duration::from_micros(left.unsigned_abs());
        match self.render.serve_recovery(Some(until), true) {
            crate::render_side::Recovered::Nothing => Some(Ok(())),
            crate::render_side::Recovered::Running => None,
            crate::render_side::Recovered::Answered(answered) => Some(answered),
        }
    }

    fn decision_tick(&mut self, origin: game::Origin) -> Result<(), game::LiveError<ScreenError>> {
        // The page's jobs go on through the decision screen.
        if origin == game::Origin::Jobs {
            self.poll_page()?;
        }
        Ok(())
    }

    /// The play's deadline: the loading state's tuning epoch, the input
    /// epoch's end, the held lifetime shown, and a new scene. Waits for
    /// nothing.
    fn deadline_step(&mut self) {
        if let Some((_, name)) = self.selection.as_ref().and_then(|s| s.decision.as_ref()) {
            println!("load: the play screen loads {name}");
        }
        self.render.reveal_loading();
        self.begin_loading_epoch();
        self.controls.open_epoch();
        self.controls.next_scene();
    }

    /// The decision screen is left for the practice page or selection: the
    /// input epoch ends, and the page's jobs are the page's from here.
    fn closed_epoch(&mut self) {
        self.controls.open_epoch();
        if let Some(sel) = &mut self.selection {
            sel.decision = None;
            sel.page.hand_to_page();
        }
    }

    /// Past the decision screen's time while practice waits for its read:
    /// the quit key, Escape or the controller's Back, taken; everything
    /// else queued is dropped.
    fn decision_escape(&mut self, deadline: TimeUs) -> bool {
        let menu = self.selection.as_ref().map(|s| &s.menu);
        take_escape(
            self.controls,
            menu.map(AsRef::as_ref),
            &mut self.result_input.pad,
            deadline,
        )
    }

    fn load_chart(
        &mut self,
        loaded: &Loaded,
        decision: &mut game::Decision,
    ) -> Result<Loading, ChartLoadError> {
        self.chart_images_at_choice(loaded);
        let stepped = decision.stepped();
        // Moved to the load's waits and back, one owner at a time.
        let lent = std::mem::replace(decision, game::Decision::off());
        let (loading, served) = self.watching(loaded.path(), lent, |rig| rig.load_watched(loaded));
        *decision = served;
        if decision.stepped() && !stepped {
            // The deadline's step ran inside a wait: the opening's
            // reducer count is forgotten, as at its own step.
            self.accepted_at_o1 = None;
        }
        loading
    }

    fn show_refusal(&mut self, refusal: &LoadRefusal<ChartLoadError>) {
        let name = file_name(&refusal.chart);
        let why = Chain(&refusal.reason).to_string();
        println!("select: refused {name}: {why}");
        log::warn!("select: refused {}: {why}", refusal.chart.display());
        // A preset's error is shown by its file's name, never its path.
        let (shown, preset) = match &refusal.reason {
            Refused::Chart(SessionError::Preset(e)) => {
                (format!("{}: {}", refusal.reason, shelf_error_text(e)), true)
            }
            _ => (why, false),
        };
        if let Some(sel) = &mut self.selection {
            #[cfg(feature = "dev-tools")]
            if let Some(script) = &mut sel.script {
                script.refused(Chain(&refusal.reason).to_string());
            }
            sel.refused(&refusal.chart, format!("{name}: {shown}"), preset);
            sel.show(&self.render, None, self.clock.now());
        }
    }

    /// Shows the shelf, and writes a chosen preset that changed to
    /// `player.toml` at once (design.md "Settings Files"): the flow chose
    /// it on a step, the only way its name changes.
    fn presets_changed(&mut self, shelf: Result<&game::Shelf, &game::ShelfError>) {
        let (name, error) = shelf_shown(shelf);
        let error = error.map(|(shown, chain)| {
            let line = format!("presets: {chain}");
            eprintln!("{line}");
            log::warn!("{line}");
            shown
        });
        let chosen = shelf
            .ok()
            .map(game::Shelf::chosen_name)
            .filter(|n| *n != self.files.preset());
        // Said once written, or once the write failed.
        let failed = chosen.is_some_and(|n| {
            let t = self.controls.tuning().get();
            let written = self.files.choose_preset(n, t, self.queue.periods());
            println!("select: preset {n}");
            log::info!("select: preset {n}");
            written.is_err_and(|e| {
                log::warn!("select: player.toml cannot be written: {e}");
                true
            })
        });
        if let Some(sel) = &mut self.selection {
            sel.presets(name, error, shelf.is_ok());
            sel.save_error(self.files.player_error(), failed);
            // The list shows the bests of the chosen preset, and a skin
            // its gauge.
            sel.screen
                .set_bests_preset(shelf.ok().map(|s| s.chosen().clone()));
            sel.refresh_bests();
        }
        self.feed_settings();
        if let Some(sel) = &mut self.selection {
            sel.show(&self.render, None, self.clock.now());
        }
    }

    /// The chart is left: the page's jobs of it end and their results go
    /// nowhere (design.md "Load Pipeline"), but a panic one published
    /// meanwhile that a Start was responsible for, which ends the run.
    fn left_chart(&mut self) -> Result<(), game::LiveError<ScreenError>> {
        // Its images' job was cancelled with the choice's scope below; its
        // set is never taken, nor drawn.
        self.chart_images.forget();
        let looks = looks!(self);
        let Some(sel) = &mut self.selection else {
            return Ok(());
        };
        sel.coord.advance(game::coord::Scope::Choice);
        sel.page
            .leave(Some(&mut sel.shelf), &mut || looks.terminal())
            .failure()
            .map_or(Ok(()), |f| Err(game::LiveError::Load(f)))
    }

    fn entered_selection(&mut self) {
        // Idle first, so the scene's fence below is answered after it and
        // the panels' level read at the entry is the one Idle publishes.
        self.input.idle();
        self.leave_result();
        // Another scene: held adjustment controls act no more until
        // pressed afresh.
        self.enter_scene();
        self.show_cursor(true);
        recover(
            &mut self.result_input,
            Owner::Select,
            &self.input,
            &mut self.in_play,
            &mut self.end_page,
            self.selection.as_mut().map(|s| &mut s.shelf),
        );
        self.render.clear_end();
        self.render.clear_loading();
        // The play options as the play before left them.
        self.feed_settings();
        let Some(sel) = &mut self.selection else {
            return;
        };
        let now = self.clock.now();
        sel.entered(now);
        // A panel held at the entry opens with it.
        sel.panels(self.panels.as_ref(), now);
        self.render.enter_selection(sel.feed.lifetime);
        // The window takes the pointer for this lifetime from now on.
        sel.pointer.enter(sel.feed.lifetime);
        println!("select: {} charts listed", sel.screen.listed());
        sel.show(&self.render, None, now);
    }

    /// The same entry on a screen built again: its lifetime, its timers'
    /// starts and the skin's state stay (design.md "Rendering and Skins",
    /// Screen skins); its update is sent again, so the new screen prepares
    /// what it draws.
    fn recovered_selection(&mut self) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        sel.dirty = true;
        sel.show(&self.render, None, self.clock.now());
    }

    /// The played bank goes first (the release barrier), then the page's
    /// 1× bank begins at background priority (design.md "State Model",
    /// the practice page).
    fn practice_read(
        &mut self,
        loaded: &Loaded,
        decision: &mut game::Decision,
    ) -> Result<Loading, ChartLoadError> {
        // The page's 1× bank is decoded as a play's is (design.md "Load
        // Pipeline"): once the chart's background measurement is done, the
        // release barrier, then the decode, never beside the background's
        // bank. Its prerequisites are looked at with no wait, here and at
        // every look of the decision screen and pass of the page, so both
        // are served meanwhile and the page may open before its bank.
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        let chart = crate::page_jobs::PageChart {
            path: loaded.path().to_owned(),
            chart: Arc::new(loaded.chart().clone()),
            name: file_name(loaded.path()),
            rate: sel.rate,
        };
        // Through the decision screen the 1× bank is the choice's,
        // promoted; the page takes it over when it opens.
        sel.page.defer(chart, decision.on());
        match self.begin_page_when_ready() {
            Ok(()) => Ok(Loading::Ready),
            Err(PageBegin::Failed(failure)) => Ok(Loading::Failed(failure)),
            Err(PageBegin::Refused(e)) => Err(ChartLoadError::Audio(e)),
        }
    }

    fn poll_page(&mut self) -> Result<(), game::LiveError<ScreenError>> {
        match self.begin_page_when_ready() {
            Ok(()) => {}
            Err(PageBegin::Failed(failure)) => return Err(game::LiveError::Load(failure)),
            Err(PageBegin::Refused(e)) => {
                // Left to a Start, which releases again before its decode.
                crate::loudness::log_line(
                    &format!("practice: the page's bank cannot begin ({e}); a Start decodes it"),
                    true,
                );
                // The page stays: a pause the bank took is released.
                if let Some(sel) = &mut self.selection
                    && let Some((_, pause)) = sel.page.take_pending()
                {
                    pause.release(&mut sel.shelf);
                }
            }
        }
        let Some(sel) = &mut self.selection else {
            return Ok(());
        };
        sel.page
            .poll(&sel.coord, sel.spawner, &mut sel.shelf)
            .map_err(game::LiveError::Load)
    }

    /// The one routine of every entry into the practice page: on opening
    /// and after every return (a refused Start, a refused opening or
    /// restore, the rounds left by the quit key or a result's back).
    fn start_ready(&mut self) -> Result<(), String> {
        self.skin.ready().inspect_err(|notice| {
            println!("practice: not started: {notice}");
        })
    }

    fn entered_practice(&mut self, screen: &PracticeScreen) {
        self.leave_result();
        self.enter_scene();
        self.left_selection();
        self.show_cursor(true);
        recover(
            &mut self.result_input,
            Owner::Practice,
            &self.input,
            &mut self.in_play,
            &mut self.end_page,
            self.selection.as_mut().map(|s| &mut s.shelf),
        );
        self.render.clear_end();
        self.render.clear_loading();
        if let Some(sel) = &mut self.selection {
            sel.loading = None;
        }
        if let Some(ime) = self.selection.as_ref().and_then(|s| s.ime.as_ref()) {
            ime.set(false);
        }
        match screen.notice() {
            Some(why) => println!("practice: refused: {why}"),
            None => println!("practice: on the practice screen"),
        }
        self.show_practice(screen);
    }

    fn show_practice(&mut self, screen: &PracticeScreen) {
        let name = self
            .selection
            .as_ref()
            .and_then(|s| s.screen.selected())
            .map_or_else(String::new, |e| {
                e.header.title.clone().unwrap_or_else(|| file_name(&e.path))
            });
        let page = crate::practice_page::page(screen, &name);
        self.render.send(RenderMsg::PracticePage(Box::new(page)));
        #[cfg(feature = "dev-tools")]
        if let Some(sc) = &mut self.scenario {
            sc.practice = Some(screen.clone());
            sc.shown = true;
        }
    }

    /// The flow applies the step taken now; a step that changes the
    /// chosen preset gives the next update its settings through
    /// [`presets_changed`](game::AppRig::presets_changed).
    fn take_preset_step(&mut self) -> Option<game::PresetStep> {
        self.selection.as_mut()?.preset_steps.pop_front()
    }

    fn take_practice(&mut self) -> Option<PracticeInput> {
        if let Some(input) = self.result_input.take_practice() {
            return Some(input);
        }
        #[cfg(feature = "dev-tools")]
        if self.script_practice() {
            return self.result_input.take_practice();
        }
        None
    }
}

impl Selection {
    /// Selection is entered at `now`: the composition left from the chart
    /// goes, the scan goes on, and the list and the bests of the chosen
    /// preset are read anew, with what the chart's plays counted.
    fn entered(&mut self, now: TimeUs) {
        self.entered_at = now;
        self.feed.enter(now);
        self.loading = None;
        self.screen.end_composition();
        self.shelf.release();
        if let Some(ime) = &self.ime {
            ime.set(self.screen.searching());
        }
        self.refresh();
    }
}

/// Waits for the chart's load on the render thread, `screen`, serving the
/// adjustments through the wait (design.md "Play Adjustments"), so one made
/// meanwhile is published before the load ends.
///
/// # Errors
///
/// As [`PendingLoad::wait_serving`](crate::render_side::PendingLoad::wait_serving).
fn wait_chart_load(
    screen: crate::render_side::PendingLoad,
    render: &RenderLink,
    mut adjust: Option<&mut game::AdjustDriver>,
    input: &crate::input_side::InputLink,
    controls: &crate::controls::Controls,
    #[cfg(test)] on_serve: &dyn Fn(),
) -> Result<(), ScreenError> {
    screen.wait_serving(render, || {
        #[cfg(test)]
        on_serve();
        #[cfg(feature = "dev-tools")]
        crate::dev::press_due(controls, crate::dev::LoadPhase::ChartLoad);
        crate::play::serve_adjustments(adjust.as_deref_mut(), input, controls.tuning());
        // The decision screen is served here too; a look that leaves it
        // stops the wait.
        controls.look().is_some_and(game::Verdict::leaves)
    })
}

#[cfg(all(test, feature = "dev-tools"))]
mod job_tests;

mod page_start;
use page_start::Got;

#[cfg(test)]
pub(crate) mod tests {
    #[cfg(feature = "dev-tools")]
    mod pointer;

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use game::DataRoot;

    use super::{Selection, chosen_chart};
    use crate::result_input::{Pad, ResultInput};
    use crate::select::{MenuInput, MenuKey, MenuQueue, SelectScreen};
    use crate::shelf::{Shelf, open};

    /// A library entry with `header` and BGA flag `bga`.
    fn entry(header: library::Header, bga: bool) -> library::ChartEntry {
        library::ChartEntry {
            path: std::path::PathBuf::from("a.bms"),
            size: 0,
            mtime_ns: 0,
            sha256: [0; 32],
            md5: [0; 16],
            header,
            random: false,
            bga,
            summary: None,
            rank: None,
        }
    }

    /// The chart's load waits on the render thread serving the adjustments
    /// (design.md "Play Adjustments"): F2 pressed while the load is pending
    /// steps a Classic hi-speed of 3.20 to 3.45, published before the load
    /// is answered, which the answer waits for.
    #[test]
    fn an_adjustment_during_the_charts_load_is_published_before_it_ends() {
        use eval::ControlId;
        use game::{
            AdjustDriver, AdjustmentConfig, Covers, DeviceSettings, HiSpeedSettings, KeyConfigs,
            PlayOptions, SpeedMode,
        };
        use input::{DeviceId, DeviceKind, Packet, PresentationFocus, SourceConfig, SourceSet};

        const F2: u32 = 0x3C;
        let device = DeviceSettings::default();
        let keys = KeyConfigs {
            keyboard: device.keyboard.clone(),
            controller: device.controller.clone(),
            controller_device: None,
        };
        let plan = AdjustmentConfig::default().plan(&keys, None, None).unwrap();
        let options = PlayOptions::new(
            HiSpeedSettings {
                mode: SpeedMode::Classic,
                classic_hispeed_x100: 320,
                green_target: 300,
                ..HiSpeedSettings::default()
            },
            Covers::default(),
        )
        .unwrap();
        let (mut driver, tap, mut reader) = AdjustDriver::new(&plan, options, 0).unwrap();
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        set.set_presentation_focus(PresentationFocus::new());
        set.begin_batch(TimeUs(0));
        set.connect(DeviceId(1), DeviceKind::Keyboard);
        set.end_batch(TimeUs(1));
        set.set_adjustment(tap);

        let controls = Arc::new(crate::controls::Controls::new(input::QpcClock::start()));
        let (_hook, input) = crate::input_side::link(
            Arc::clone(&controls),
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let mut render = crate::render_side::tests::Installers::default().spawn_bare();
        // The load's own bound past the input side's, which decides.
        render.set_timeouts(Duration::from_secs(120), Duration::from_secs(5));
        let (answer, rx) = std::sync::mpsc::sync_channel(1);
        let load = crate::render_side::PendingLoad::answered_on(rx);
        // The input side: F2 pressed and released while the load is
        // pending, its group closed a millisecond later; the load is
        // answered only once the step is published, or past a bound that
        // only catches a wait that never serves.
        let side = std::thread::spawn(move || {
            let packet = |value| Packet {
                device: DeviceId(1),
                control: ControlId(F2),
                value,
                background: false,
            };
            set.begin_batch(TimeUs(10_000));
            set.packet(&packet(1));
            set.packet(&packet(0));
            set.end_batch(TimeUs(10_001));
            set.tick_presentation(TimeUs(11_000));
            let deadline = Instant::now() + Duration::from_secs(60);
            let published = loop {
                if reader.read().options.hispeed().classic_hispeed_x100 == 345 {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            answer.send(Ok(())).unwrap();
            published
        });
        super::wait_chart_load(load, &render, Some(&mut driver), &input, &controls, &|| {})
            .unwrap();
        assert!(side.join().unwrap(), "not published while the load waited");
        assert_eq!(
            driver.snapshot().options.hispeed().classic_hispeed_x100,
            345
        );
        render.exit();
    }

    /// A chosen chart's level is its library entry's `#PLAYLEVEL`, parsed as
    /// an attempt's is: a missing or unreadable one a known 0.
    #[test]
    fn the_chosen_charts_level_comes_from_its_entry() {
        let level = |text: Option<&str>| {
            let header = library::Header {
                playlevel: text.map(str::to_owned),
                ..library::Header::default()
            };
            chosen_chart(&entry(header, false)).play_level
        };
        assert_eq!(level(Some("12")), Some(12));
        assert_eq!(level(Some("１２")), Some(12));
        assert_eq!(level(Some("12+")), Some(0));
        assert_eq!(level(None), Some(0));
    }

    /// A chosen chart's difficulty is its entry's `#DIFFICULTY` as an
    /// attempt reads it, 1 to 5 and 0 for any other or none, and its BGA
    /// flag the entry's: both known once the entry is.
    #[test]
    fn the_chosen_charts_difficulty_and_bga_come_from_its_entry() {
        let chosen = |text: Option<&str>, bga| {
            let header = library::Header {
                difficulty: text.map(str::to_owned),
                ..library::Header::default()
            };
            let c = chosen_chart(&entry(header, bga));
            (c.difficulty, c.bga)
        };
        assert_eq!(chosen(Some("4"), true), (Some(4), Some(true)));
        assert_eq!(chosen(Some("+5"), false), (Some(5), Some(false)));
        for text in [Some("6"), Some("0"), Some("-1"), Some("2a"), None] {
            assert_eq!(chosen(text, false), (Some(0), Some(false)), "{text:?}");
        }
    }

    /// A chosen chart's length is its entry's summary's, the loader's 1×
    /// length; none for an entry whose seed-0 branch the loader refused.
    #[test]
    fn the_chosen_charts_length_comes_from_its_summary() {
        let mut e = entry(library::Header::default(), false);
        assert_eq!(chosen_chart(&e).length_us, None);
        e.summary = Some(bms_model::ChartSummary {
            taps: 1,
            longs: 0,
            mines: 0,
            length: bms_model::TimeUs(125_400_000),
            tempo_start: bms_model::Tempo::from_milli(120_000).unwrap(),
            tempo_min: bms_model::Tempo::from_milli(120_000).unwrap(),
            tempo_max: bms_model::Tempo::from_milli(120_000).unwrap(),
        });
        assert_eq!(
            chosen_chart(&e).length_us,
            Some(bms_model::TimeUs(125_400_000))
        );
    }

    /// A healthy screen whose first loading frame comes after the hold's
    /// time, then one every 40 ms: the hold goes on until its reported
    /// checkpoints show the frames following one another, the rule its
    /// test checks, not only until enough frames came between two of them.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_slow_load_holds_until_its_checkpoints_show_frames_following() {
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let began = Instant::now();
        let drawn = || {
            let ms = u64::try_from(began.elapsed().as_millis()).unwrap();
            // The first frame at 300 ms, then one every 40 ms.
            if ms < 300 { 0 } else { 1 + (ms - 300) / 40 }
        };
        let slow = crate::dev::SlowLoad {
            ms: 0,
            quit: false,
            close: false,
        };
        let mut reported = Vec::new();
        assert!(!super::hold(&controls, slow, &drawn, &mut |n| reported.push(n)));
        assert!(
            crate::dev::checkpoints::frames_follow(&reported),
            "{reported:?}"
        );
        // Only catches a hold that never ends, however late a loaded
        // machine runs it.
        assert!(began.elapsed() < Duration::from_secs(600));
    }

    /// A chart chosen is admitted only once the selected skin is
    /// installed, before anything of the choice happens (`look`'s gate):
    /// - while its installation waits on the render thread, the choice is
    ///   dropped with a notice, the select page goes on taking keys and
    ///   each look returns at once;
    /// - installed, the next choice is admitted;
    /// - refused, the choice is dropped with the screen's located reason
    ///   as the notice, every later choice too, and the page stays usable.
    ///
    /// The practice page asks the same state at a Start
    /// ([`game::AppRig::start_ready`]).
    #[test]
    fn a_chart_is_admitted_only_once_its_skin_is_installed() {
        use std::sync::atomic::Ordering;

        use crate::menu_screen::LineStyle;
        use crate::render_side::SkinState;
        use crate::render_side::tests::{Installers, bundle, wait_until};

        let notice = |sel: &Selection| {
            let page = sel.screen.page("");
            page.lines
                .into_iter()
                .find(|l| l.style == LineStyle::Notice)
                .map(|l| l.text)
                .unwrap_or_default()
        };

        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let screens = Installers::default();
        let render = screens.spawn_bare();
        let mut skin = SkinState::Ready;
        let choose = |sel: &mut Selection, skin: &mut SkinState, keys: &[MenuKey]| {
            for key in keys {
                sel.menu.push(MenuInput::Key(*key));
            }
            let chosen = sel.drain(&mut Pad::default());
            let began = Instant::now();
            let admitted = sel.admit(chosen, skin, &render, TimeUs(0));
            assert!(began.elapsed() < Duration::from_secs(1), "a look waited");
            admitted
        };

        // Held on the render thread: dropped, and the page moves on.
        screens.hold_install.store(true, Ordering::Release);
        skin.select(&render, Some(bundle()));
        assert!(choose(&mut sel, &mut skin, &[MenuKey::Enter]).is_none());
        assert_eq!(notice(&sel), "スキンを準備しています");
        assert!(choose(&mut sel, &mut skin, &[MenuKey::Down, MenuKey::Enter]).is_none());
        screens.hold_install.store(false, Ordering::Release);
        assert!(wait_until(|| screens.log().len() == 2));
        let admitted = choose(&mut sel, &mut skin, &[MenuKey::Enter]).expect("installed");
        assert_eq!(played(admitted).file_name().unwrap(), "b.bms");

        // Refused: never admitted, with the reason.
        screens.refuse_install.store(true, Ordering::Release);
        skin.select(&render, Some(bundle()));
        assert!(wait_until(|| screens.log().len() == 4));
        assert!(choose(&mut sel, &mut skin, &[MenuKey::Enter]).is_none());
        let refused = notice(&sel);
        assert!(
            refused.contains("the skin cannot be installed"),
            "{refused}"
        );
        assert!(choose(&mut sel, &mut skin, &[MenuKey::Up, MenuKey::Enter]).is_none());
        assert_eq!(skin.ready(), Err(refused));
        render.exit();
        assert_eq!(
            screens.log(),
            ["1 install 1", "1 commit 1", "1 refuse 2", "1 discard"]
        );
    }

    /// The chart a pick plays.
    fn played(pick: game::Pick) -> std::path::PathBuf {
        match pick {
            game::Pick::Play(path) => path,
            other @ game::Pick::Practice(_) => panic!("{other:?}"),
        }
    }

    /// Titles A, B and C by artists Z, Y and X, scanned and listed in title
    /// order, A under the cursor.
    fn selection(dir: &std::path::Path) -> Selection {
        let mut sel = unlisted(dir, |library, charts| {
            Shelf::new(
                library,
                charts,
                Duration::from_secs(60),
                None,
                &crate::shelf::tests::coordinator(),
            )
        });
        // The scan pass runs in background mode, which a loaded machine
        // starves for seconds: the bound only catches a pass that never
        // lists them.
        let begun = Instant::now();
        while sel.screen.listed() < 3 {
            assert!(
                begun.elapsed() < Duration::from_secs(600),
                "the charts were not listed"
            );
            sel.shelf.poll(Instant::now());
            sel.refresh();
            std::thread::sleep(Duration::from_millis(10));
        }
        sel
    }

    /// The charts of [`selection`] on a shelf `shelf` makes, its first pass
    /// started and nothing listed yet.
    fn unlisted(
        dir: &std::path::Path,
        shelf: impl FnOnce(library::Library, Vec<std::path::PathBuf>) -> Shelf,
    ) -> Selection {
        let charts = dir.join("charts");
        std::fs::create_dir_all(&charts).unwrap();
        for (file, title, artist) in [("a", "A", "Z"), ("b", "B", "Y"), ("c", "C", "X")] {
            let text =
                format!("#TITLE {title}\n#ARTIST {artist}\n#BPM 120\n#00138:01\n#00111:01\n");
            std::fs::write(charts.join(format!("{file}.bms")), text).unwrap();
        }
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        let shelf = shelf(library, vec![charts.clone()]);
        let mut screen = SelectScreen::new(vec![charts]);
        screen.show_all();
        Selection {
            shelf,
            screen,
            menu: Arc::new(MenuQueue::default()),
            dropped: 0,
            pointer: crate::pointer::PointerPass::default(),
            dropped_pointer: 0,
            feed: crate::selection_skin::SkinFeed::default(),
            preset_refusal: false,
            ime: None,
            rate: 48_000,
            chosen_at: TimeUs(0),
            loading: None,
            decision: None,
            practicing: false,
            entered_at: TimeUs(0),
            dirty: false,
            search_pending: false,
            preset_steps: std::collections::VecDeque::new(),
            #[cfg(feature = "dev-tools")]
            script: None,
            coord: crate::shelf::tests::coordinator(),
            spawner: game::coord::Spawner::SYSTEM,
            page: crate::page_jobs::PageJobs::new(Arc::new(crate::priority::Unchanged)),
            decodes: None,
            after_bank: None,
            unload_fault: None,
            #[cfg(feature = "dev-tools")]
            witness: crate::bank_witness::BankWitness::new(),
        }
    }

    /// Charts of other modes reach no list: once a whole pass has indexed
    /// them, the screen still lists only the 7-key charts.
    #[test]
    fn the_screen_lists_no_chart_of_another_mode() {
        let dir = tempfile::tempdir().unwrap();
        let charts = dir.path().join("charts");
        std::fs::create_dir_all(&charts).unwrap();
        std::fs::write(charts.join("five.bms"), "#TITLE F\n#00111:01\n").unwrap();
        std::fs::write(charts.join("two.bms"), "#TITLE T\n#00118:01\n#00121:01\n").unwrap();
        let mut sel = selection(dir.path());
        let begun = Instant::now();
        while sel.shelf.status() == "スキャン中" {
            assert!(begun.elapsed() < Duration::from_secs(600), "no pass ended");
            sel.shelf.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        sel.refresh();
        assert_eq!(sel.screen.listed(), 3);
    }

    /// An order change queued before navigation is searched before the
    /// navigation acts: in artist order A is last, so Down stays there and
    /// Enter chooses A. Navigating the stale title order first would move
    /// to B.
    #[test]
    fn an_order_change_is_searched_before_the_keys_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        for key in [MenuKey::Right, MenuKey::Down, MenuKey::Enter] {
            sel.menu.push(MenuInput::Key(key));
        }
        let chosen = sel.drain(&mut Pad::default()).expect("Enter chooses");
        assert_eq!(played(chosen).file_name().unwrap(), "a.bms");
    }

    /// The controller's events act as the keys they stand for: Down twice
    /// and Confirm choose C.
    #[test]
    fn controller_events_move_and_choose() {
        use eval::ControlId;
        use input::hid::HidProduct;
        use input::{
            DeviceId, DeviceKind, MenuAction, MenuBindings, MenuEvent, Packet, SourceConfig,
            SourceSet,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let product = HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        let (tap, pad) = input::menu_queue(MenuBindings {
            product,
            controls: vec![
                (ControlId(1), MenuAction::Press(MenuEvent::Confirm)),
                (ControlId(2), MenuAction::Press(MenuEvent::Down)),
            ],
            step: 2,
            relative: None,
            wrap: None,
        });
        let mut pad = Pad {
            receiver: Some(pad),
            dropped: 0,
        };
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 4,
            queue_capacity: 4,
            max_arrivals: 1,
        })
        .unwrap();
        set.connect(DeviceId(7), DeviceKind::Controller);
        set.set_product(DeviceId(7), product);
        set.set_menu(tap);
        set.menu_mut().unwrap().set_active(true);
        for (control, value) in [(2, 1), (2, 0), (2, 1), (1, 1)] {
            set.packet(&Packet {
                device: DeviceId(7),
                control: ControlId(control),
                value,
                background: false,
            });
        }
        let chosen = sel.drain(&mut pad).expect("Confirm chooses");
        assert_eq!(played(chosen).file_name().unwrap(), "c.bms");
    }

    /// The controller's events share the window's 64-key bound: with a
    /// chart under the cursor and 64 window keys queued (a held
    /// Backspace, which does nothing outside the search box), a
    /// controller Confirm is dropped and counted, and nothing is chosen.
    #[test]
    fn controller_events_share_the_key_bound() {
        use eval::ControlId;
        use input::{MenuAction, MenuBindings, MenuEvent};
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let (mut tap, receiver) = input::menu_queue(MenuBindings {
            product: controller_product(),
            controls: vec![(ControlId(1), MenuAction::Press(MenuEvent::Confirm))],
            step: 1,
            relative: None,
            wrap: None,
        });
        tap.set_active(true);
        let mut pad = Pad {
            receiver: Some(receiver),
            dropped: 0,
        };
        for _ in 0..crate::select::QUEUED_KEYS {
            sel.menu.push(MenuInput::Key(MenuKey::BackspaceHeld));
        }
        tap.offer(
            &input::Packet {
                device: input::DeviceId(7),
                control: ControlId(1),
                value: 1,
                background: false,
            },
            None,
            bms_model::TimeUs(0),
        );
        assert_eq!(
            sel.drain(&mut pad),
            None,
            "the Confirm past the bound chose"
        );
        assert_eq!(sel.menu.dropped(), 1);
        assert_eq!(pad.dropped, 0, "the controller's own queue had room");
    }

    /// A keyboard with START on E and SELECT on W reaching an adjustment
    /// tap, and the panels' level it publishes.
    fn panel_keys() -> (input::SourceSet, input::PanelLevel) {
        use input::{
            AdjustmentBindings, DeviceId, DeviceKind, SourceConfig, SourceSet, TapControl,
        };
        let mut set = SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 4,
            queue_capacity: 4,
            max_arrivals: 1,
        })
        .unwrap();
        set.begin_batch(TimeUs(0));
        set.connect(DeviceId(1), DeviceKind::Keyboard);
        set.end_batch(TimeUs(0));
        let (mut tap, _rx) = input::adjustment_queue(AdjustmentBindings {
            controls: vec![
                (TapControl::Key(eval::ControlId(0x12)), 0),
                (TapControl::Key(eval::ControlId(0x11)), 1),
            ],
            latched: 0b11,
            ..AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_panel_roles(0b01, 0b10);
        let level = tap.panel_level();
        set.set_adjustment(tap);
        (set, level)
    }

    /// Key `code` of [`panel_keys`]'s keyboard to `value`, then the
    /// boundary's publication.
    fn panel_key(set: &mut input::SourceSet, code: u32, value: i32) {
        set.begin_batch(TimeUs(0));
        set.packet(&input::Packet {
            device: input::DeviceId(1),
            control: eval::ControlId(code),
            value,
            background: false,
        });
        set.end_batch(TimeUs(0));
        set.adjustment_mut().unwrap().publish_panels(true, 0);
    }

    /// Under a panel the select screen's list takes no input, as the
    /// reference's option panels hold it (`MusicSelectInputProcessor.java`
    /// at 0.8.9, `bar.input()` only with none open, `:290`): under every
    /// panel Up, Down, the scratch and every other controller key do
    /// nothing, and nothing chooses, practises, goes back or pages; the
    /// order, the search box and the presets act as without one.
    #[test]
    fn keys_under_a_panel_act_as_the_panels_let_them() {
        use eval::ControlId;
        use input::{MenuAction, MenuBindings, MenuEvent};
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let (mut set, level) = panel_keys();
        let (mut pad_tap, receiver) = input::menu_queue(MenuBindings {
            product: controller_product(),
            controls: vec![
                (ControlId(1), MenuAction::Press(MenuEvent::Confirm)),
                (ControlId(2), MenuAction::Press(MenuEvent::Down)),
                (ControlId(3), MenuAction::Press(MenuEvent::Back)),
                (ControlId(7), MenuAction::Press(MenuEvent::Extra)),
            ],
            step: 1,
            relative: None,
            wrap: None,
        });
        pad_tap.set_active(true);
        let mut pad = Pad {
            receiver: Some(receiver),
            dropped: 0,
        };
        // A controller press sent while `panel` is open.
        let mut send = |panel: Option<usize>, control: u32| {
            pad_tap.set_panel(panel);
            pad_tap.offer(
                &input::Packet {
                    device: input::DeviceId(7),
                    control: ControlId(control),
                    value: 1,
                    background: false,
                },
                None,
                bms_model::TimeUs(0),
            );
        };
        sel.feed.enter(TimeUs(0));
        let look = |sel: &mut Selection, set: &mut input::SourceSet, code, value| {
            panel_key(set, code, value);
            sel.panels(Some(&level), TimeUs(1));
        };
        // Panel 1: Down, Up, Enter, P, Escape, Backspace, Page Down and
        // End do nothing; the scratch, the white keys, the black keys and
        // key 7 do nothing.
        look(&mut sel, &mut set, 0x12, 1);
        assert_eq!(sel.feed.panel(), Some(0));
        let cursor = |sel: &Selection| sel.screen.view(0, 0, false);
        let before = cursor(&sel);
        for key in [
            MenuKey::Down,
            MenuKey::Down,
            MenuKey::Up,
            MenuKey::Enter,
            MenuKey::Practice,
            MenuKey::Escape,
            MenuKey::Backspace,
            MenuKey::PageDown,
            MenuKey::End,
        ] {
            sel.menu.push(MenuInput::Key(key));
        }
        for control in [1, 2, 3, 7] {
            send(Some(0), control);
        }
        assert_eq!(sel.drain(&mut pad), None);
        assert_eq!(cursor(&sel), before, "nothing moved the list");
        assert!(sel.preset_steps.is_empty(), "key 7 stepped no preset");
        // Tab and the presets act: into the search box and out, and F12.
        keys(&mut sel, &[MenuKey::Tab]);
        assert!(sel.screen.searching());
        keys(&mut sel, &[MenuKey::Tab, MenuKey::NextPreset]);
        assert!(!sel.screen.searching());
        assert_eq!(sel.preset_steps.len(), 1, "F12 stepped the preset");
        sel.preset_steps.clear();
        // Panel 3 (both) and panel 2 (SELECT): nothing moves; the order
        // still changes, and back.
        look(&mut sel, &mut set, 0x11, 1);
        assert_eq!(sel.feed.panel(), Some(2));
        keys(&mut sel, &[MenuKey::Up, MenuKey::Home, MenuKey::Right]);
        assert_eq!(sel.screen.order(), 1, "the next order");
        keys(&mut sel, &[MenuKey::Left, MenuKey::Enter]);
        assert_eq!(sel.screen.order(), 0);
        send(Some(2), 2);
        assert_eq!(sel.drain(&mut pad), None);
        assert_eq!(cursor(&sel), before, "nothing moved the list");
        look(&mut sel, &mut set, 0x12, 0);
        assert_eq!(sel.feed.panel(), Some(1));
        assert_eq!(keys(&mut sel, &[MenuKey::Up, MenuKey::Enter]), None);
        assert_eq!(cursor(&sel), before, "nothing moved the list");
        // Closed: Enter chooses A, where the cursor stayed.
        look(&mut sel, &mut set, 0x11, 0);
        assert_eq!(sel.feed.panel(), None);
        let chosen = keys(&mut sel, &[MenuKey::Enter]).expect("Enter chooses");
        assert_eq!(played(chosen).file_name().unwrap(), "a.bms");
        // A controller event sent under a panel stays held back once it
        // closed; one sent after acts.
        send(Some(1), 1);
        assert_eq!(sel.drain(&mut pad), None);
        send(None, 1);
        assert!(sel.drain(&mut pad).is_some());
    }

    /// While the search box has focus no panel opens, and a panel open
    /// closes; once the box loses focus with START still held, panel 1
    /// opens again with a fresh start.
    #[test]
    fn the_search_box_keeps_the_panels_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let (mut set, level) = panel_keys();
        sel.feed.window = Some((3, 7));
        sel.feed.enter(TimeUs(0));
        let panels = |sel: &mut Selection, t: i64| {
            sel.panels(Some(&level), TimeUs(t));
            let page = sel.screen.page("");
            sel.feed.update(&sel.screen, page, TimeUs(t)).events.panels
        };
        panel_key(&mut set, 0x12, 1);
        assert_eq!(panels(&mut sel, 10), [Some(TimeUs(10)), None, None]);
        keys(&mut sel, &[MenuKey::Tab]);
        assert!(sel.screen.searching());
        assert_eq!(panels(&mut sel, 20), [None; 3]);
        // Typing E into the box opens nothing.
        panel_key(&mut set, 0x12, 0);
        panel_key(&mut set, 0x12, 1);
        assert_eq!(panels(&mut sel, 30), [None; 3]);
        keys(&mut sel, &[MenuKey::Tab]);
        assert!(!sel.screen.searching());
        assert_eq!(panels(&mut sel, 40), [Some(TimeUs(40)), None, None]);
    }

    /// The search box's focus changing within one look moves the panel
    /// for the keys after it, through the rig's look: with START held and
    /// the box focused, Tab leaves it and the Enter after it is held back;
    /// Tab into the box lets the Backspace after it edit the query.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_focus_change_in_a_look_moves_the_panel_for_the_keys_after_it() {
        use eval::ControlId;
        use game::AppRig as _;
        use input::MenuEvent;
        let dir = tempfile::tempdir().unwrap();
        let sel = selection(dir.path());
        let (mut set, level) = panel_keys();
        crate::tests::on_rig(game::DeviceSettings::default(), |rig, _| {
            rig.adjust = None;
            rig.panels = Some(level);
            rig.selection = Some(sel);
            panel_key(&mut set, 0x12, 1);
            rig.entered_selection();
            let menu = Arc::clone(&rig.selection.as_ref().unwrap().menu);
            let look = |rig: &mut crate::play::PlayerRig<'_>, inputs: &[MenuInput]| {
                for input in inputs {
                    menu.push(input.clone());
                }
                let chosen = rig.look();
                let sel = rig.selection.as_ref().unwrap();
                (chosen, sel.screen.searching(), sel.feed.panel())
            };
            let (chosen, searching, panel) = look(rig, &[MenuInput::Key(MenuKey::Tab)]);
            assert!(chosen.is_none() && searching && panel.is_none());
            let (chosen, searching, panel) = look(
                rig,
                &[MenuInput::Key(MenuKey::Tab), MenuInput::Key(MenuKey::Enter)],
            );
            assert!(chosen.is_none(), "Enter under panel 1 chose");
            assert!(!searching);
            assert_eq!(panel, Some(0));
            // A query typed in the box, then Tab into it and Backspace.
            look(
                rig,
                &[
                    MenuInput::Key(MenuKey::Tab),
                    MenuInput::Text("ab".to_owned()),
                    MenuInput::Key(MenuKey::Tab),
                ],
            );
            assert_eq!(rig.selection.as_ref().unwrap().screen.query(), "ab");
            let (_, searching, panel) = look(
                rig,
                &[
                    MenuInput::Key(MenuKey::Tab),
                    MenuInput::Key(MenuKey::Backspace),
                ],
            );
            assert!(searching && panel.is_none());
            assert_eq!(rig.selection.as_ref().unwrap().screen.query(), "a");
            // The controller's keys follow the panel shown too: Back sent
            // under panel 1 leaves the focused box, and Confirm sent with
            // no panel is held back under the panel shown.
            let (mut pad_tap, receiver) = input::menu_queue(input::MenuBindings {
                product: controller_product(),
                controls: vec![
                    (ControlId(1), input::MenuAction::Press(MenuEvent::Confirm)),
                    (ControlId(2), input::MenuAction::Press(MenuEvent::Back)),
                ],
                step: 1,
                relative: None,
                wrap: None,
            });
            pad_tap.set_active(true);
            rig.result_input.pad = Pad {
                receiver: Some(receiver),
                dropped: 0,
            };
            let mut send = |panel: Option<usize>, control: u32| {
                pad_tap.set_panel(panel);
                pad_tap.offer(
                    &input::Packet {
                        device: input::DeviceId(7),
                        control: ControlId(control),
                        value: 1,
                        background: false,
                    },
                    None,
                    bms_model::TimeUs(0),
                );
            };
            send(Some(0), 2);
            let (_, searching, panel) = look(rig, &[]);
            assert!(!searching, "Back left the box");
            assert_eq!(panel, Some(0));
            send(None, 1);
            let (chosen, _, _) = look(rig, &[]);
            assert!(chosen.is_none(), "Confirm under panel 1 chose");
        });
    }

    /// Work for [`input_thread`] to run on its source set.
    #[cfg(feature = "dev-tools")]
    type Jobs = std::sync::mpsc::Sender<Box<dyn FnOnce(&mut input::SourceSet) + Send>>;

    /// The rig's input thread on a clocked thread with a keyboard
    /// connected; `jobs` run on its source set at its next wake, before
    /// the hook.
    #[cfg(feature = "dev-tools")]
    pub(super) fn input_thread(
        end: crate::tests::InputEnd,
    ) -> (input::ClockedThread, Jobs, input::Waker, input::QpcClock) {
        use input::{DeviceId, DeviceKind, SourceConfig, SourceSet};
        let crate::tests::InputEnd {
            mut hook,
            waker,
            clock,
        } = end;
        let mut sources = SourceSet::new(SourceConfig {
            max_devices: 2,
            max_sources: 2,
            max_held: 16,
            queue_capacity: 64,
            max_arrivals: 2,
        })
        .unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(1), DeviceKind::Keyboard);
        sources.end_batch(TimeUs(0));
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce(&mut input::SourceSet) + Send>>();
        let thread = input::ClockedThread::spawn(clock, sources, &waker, move |s, now| {
            while let Ok(job) = rx.try_recv() {
                job(s);
            }
            hook.wake(s, now)
        })
        .unwrap();
        (thread, tx, waker, clock)
    }

    /// A batch at `at` of [`input_thread`]'s keyboard: key `code` to
    /// `value`.
    #[cfg(feature = "dev-tools")]
    fn key_batch(s: &mut input::SourceSet, at: TimeUs, code: u32, value: i32) {
        s.begin_batch(at);
        s.packet(&input::Packet {
            device: input::DeviceId(1),
            control: eval::ControlId(code),
            value,
            background: false,
        });
        s.end_batch(at);
    }

    /// C16 at selection's entry, through the rig and its input thread
    /// (START on E as installed): the input thread stopped after it
    /// answers the entry's fence, before the rest of its boundary, the
    /// entry's update shows the level as of the fence. Back from the
    /// practice page with START released in the fence's boundary: closed.
    /// Back from a play's tail (the thread's start) with START held: open,
    /// from the entry.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_entry_reads_the_level_its_fence_published() {
        use bms_model::TimeSource as _;
        use game::AppRig as _;
        // The update's panel timers as the entry left them.
        let panels = |rig: &mut crate::play::PlayerRig<'_>| {
            let sel = rig.selection.as_mut().unwrap();
            let page = sel.screen.page("");
            let at = sel.feed.update(&sel.screen, page.clone(), TimeUs(0)).events;
            (at.panels, at.entered)
        };
        // Stops the input thread at its next fence, `before` run there;
        // `f` runs while it is stopped.
        let gated = |rig: &mut crate::play::PlayerRig<'_>,
                     before: Box<dyn FnOnce(&mut input::SourceSet) + Send>,
                     f: &mut dyn FnMut(&mut crate::play::PlayerRig<'_>)| {
            let (reached, reached_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume) = std::sync::mpsc::channel();
            rig.input.fence_gate(crate::input_side::FenceGate {
                before,
                reached,
                resume,
            });
            f(rig);
            assert!(
                reached_rx.recv_timeout(Duration::from_secs(60)).is_ok(),
                "the gate was reached"
            );
            resume_tx.send(()).unwrap();
        };
        for back_from_practice in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let sel = selection(dir.path());
            crate::tests::on_rig_hooked(
                game::Settings {
                    device: game::DeviceSettings::default(),
                    player: game::PlayerSettings::default(),
                },
                crate::render_side::tests::Installers::default().spawn_bare(),
                input::QpcClock::start(),
                |rig, _, end| {
                    let level = rig.panels.clone().expect("installed with the tap");
                    rig.selection = Some(sel);
                    let (thread, jobs, waker, clock) = input_thread(end);
                    let press = |code, value| {
                        jobs.send(Box::new(move |s: &mut input::SourceSet| {
                            key_batch(s, clock.now(), code, value);
                        }))
                        .unwrap();
                        waker.wake();
                    };
                    if back_from_practice {
                        // Selection, then START held on the practice page
                        // (the thread Idle), released at the boundary that
                        // answers the next entry's fence.
                        rig.entered_selection();
                        press(0x12, 1);
                        assert!(crate::render_side::tests::wait_until(|| level.read().start));
                        gated(
                            rig,
                            Box::new(move |s| key_batch(s, clock.now(), 0x12, 0)),
                            &mut |rig| rig.entered_selection(),
                        );
                        assert_eq!(panels(rig).0, [None; 3]);
                    } else {
                        // START held in the tail: nothing published, as
                        // a wake after the press's sees.
                        press(0x12, 1);
                        let (seen, seen_rx) = std::sync::mpsc::channel();
                        jobs.send(Box::new(move |_: &mut input::SourceSet| {
                            seen.send(()).unwrap();
                        }))
                        .unwrap();
                        waker.wake();
                        seen_rx.recv_timeout(Duration::from_secs(60)).unwrap();
                        assert!(!level.read().start);
                        gated(rig, Box::new(|_| {}), &mut |rig| rig.entered_selection());
                        let (open, entered) = panels(rig);
                        assert_eq!(open, [Some(entered), None, None]);
                    }
                    thread.stop().unwrap();
                },
            );
        }
    }

    /// Stops the input thread at its next fence, `before` run there on the
    /// input thread before the fence is answered; `f` runs on the game
    /// thread meanwhile, and `held` runs while the thread is stopped after
    /// its answer; the thread then goes on.
    #[cfg(feature = "dev-tools")]
    fn at_fence(
        rig: &mut crate::play::PlayerRig<'_>,
        before: Box<dyn FnOnce(&mut input::SourceSet) + Send>,
        f: &mut dyn FnMut(&mut crate::play::PlayerRig<'_>),
        held: &mut dyn FnMut(),
    ) {
        let (reached, reached_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel();
        rig.input.fence_gate(crate::input_side::FenceGate {
            before,
            reached,
            resume,
        });
        f(rig);
        assert!(
            reached_rx.recv_timeout(Duration::from_secs(60)).is_ok(),
            "the gate was reached"
        );
        held();
        resume_tx.send(()).unwrap();
    }

    /// The choice's decision screen is out before its input fence: at the
    /// fence, held before its answer, its presentation and its tone were
    /// already sent (a fence that takes the whole time still shows the
    /// screen). A second choice sounds its tone again (ND1); a screen built
    /// again shows the presentation from the render thread, with no tone
    /// (ND2, `the_render_thread_draws_the_latest_mode_after_a_rebuild`).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_decision_screen_is_out_before_its_fence() {
        use game::AppRig as _;
        let dir = tempfile::tempdir().unwrap();
        let sel = selection(dir.path());
        let chart = dir.path().join("x.bms");
        crate::tests::on_rig_hooked(
            game::Settings {
                device: game::DeviceSettings::default(),
                player: game::PlayerSettings::default(),
            },
            crate::render_side::tests::Installers::default().spawn_bare(),
            input::QpcClock::start(),
            |rig, _, end| {
                rig.selection = Some(sel);
                let (thread, _jobs, _waker, _clock) = input_thread(end);
                let sent = rig.render.decisions_sent();
                let tones = rig.audio.control.failure_probe();
                let tones_now = tones.clone();
                let (seen_tx, seen) = std::sync::mpsc::channel();
                let pick = game::Pick::Play(chart.clone());
                at_fence(
                    rig,
                    Box::new(move |_| {
                        let _ = seen_tx.send((sent(), tones_now.tone_requests()));
                    }),
                    &mut |rig| {
                        let _ = rig.entered_decision(&pick, game::Decision::off());
                    },
                    &mut || {},
                );
                assert_eq!(
                    seen.recv_timeout(Duration::from_secs(60)).unwrap(),
                    (1, 1),
                    "the presentation and the tone before the fence"
                );
                let sent = rig.render.decisions_sent();
                rig.closed_epoch();
                let _ = rig
                    .entered_decision(&game::Pick::Practice(chart.clone()), game::Decision::off());
                assert_eq!((sent(), tones.tone_requests()), (2, 2), "a tone a choice");
                thread.stop().unwrap();
            },
        );
    }

    /// An event at a held fence: what is done once the input thread is at
    /// it, and the condition that lets the fence be answered.
    #[cfg(feature = "dev-tools")]
    type AtFence<'e> = (
        &'e (dyn Fn(&crate::controls::Controls) + Sync),
        &'e (dyn Fn(&crate::controls::Controls) -> bool + Sync),
    );

    /// The choice `pick` entered with `decision` watched, its entry fence
    /// held on the input thread (its answer withheld) until `event`'s
    /// condition holds, `event`'s act run once the input thread is at the
    /// fence; with no event, until the entry returns. The decision comes
    /// back as the entry left it.
    #[cfg(feature = "dev-tools")]
    fn entry_held(
        rig: &mut crate::play::PlayerRig<'_>,
        pick: &game::Pick,
        decision: game::Decision,
        event: Option<AtFence<'_>>,
    ) -> game::Decision {
        use game::AppRig as _;
        let (at_tx, at_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (reached, reached_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume) = std::sync::mpsc::channel();
        rig.input.fence_gate(crate::input_side::FenceGate {
            before: Box::new(move |_| {
                at_tx.send(()).unwrap();
                let _ = go_rx.recv_timeout(Duration::from_secs(60));
            }),
            reached,
            resume,
        });
        let controls = rig.controls;
        let decision = std::thread::scope(|s| {
            let go = go_tx.clone();
            s.spawn(move || {
                at_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the input thread at the fence");
                if let Some((act, met)) = event {
                    act(controls);
                    let began = Instant::now();
                    while !met(controls) && began.elapsed() < Duration::from_secs(60) {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                    let _ = go.send(());
                }
            });
            let decision = rig.entered_decision(pick, decision);
            let _ = go_tx.send(());
            decision
        });
        assert!(
            reached_rx.recv_timeout(Duration::from_secs(60)).is_ok(),
            "the fence answered"
        );
        resume_tx.send(()).unwrap();
        decision
    }

    /// The decision screen of `pick` on a rig with its input thread, its
    /// deadline already due when `due`, its entry held as [`entry_held`];
    /// `check` sees the rig and the decision as they were left.
    #[cfg(feature = "dev-tools")]
    fn entered_held(
        pick: impl Fn(&std::path::Path) -> game::Pick,
        due: bool,
        event: Option<AtFence<'_>>,
        check: impl FnOnce(&crate::play::PlayerRig<'_>, game::Decision),
    ) {
        use bms_model::TimeSource as _;
        let dir = tempfile::tempdir().unwrap();
        let sel = selection(dir.path());
        let chart = dir.path().join("x.bms");
        crate::tests::on_rig_hooked(
            game::Settings {
                device: game::DeviceSettings::default(),
                player: game::PlayerSettings::default(),
            },
            crate::render_side::tests::Installers::default().spawn_bare(),
            input::QpcClock::start(),
            |rig, _, end| {
                rig.selection = Some(sel);
                let (thread, _jobs, _waker, _clock) = input_thread(end);
                let pick = pick(&chart);
                let practice = matches!(pick, game::Pick::Practice(_));
                let now = rig.clock.now();
                let begun = if due {
                    TimeUs(now.0 - game::DECISION_TIME.0)
                } else {
                    now
                };
                let decision = game::Decision::begin(
                    begun,
                    game::DecisionLifetime(1),
                    Some(game::DecisionShown::BuiltIn),
                    practice,
                );
                let decision = entry_held(rig, &pick, decision, event);
                check(rig, decision);
                thread.stop().unwrap();
            },
        );
    }

    /// A1: a close inside the choice's entry fence, held on the input
    /// thread, leaves the decision screen with `End` (the run's clean end,
    /// App routing it before the read) and latches nothing as the input
    /// thread's failure. The control: the same fence left unanswered with
    /// no close times out and latches the input failure.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_close_inside_the_entry_fence_ends_the_run_cleanly() {
        let play = |c: &std::path::Path| game::Pick::Play(c.to_owned());
        let left = |c: &crate::controls::Controls| {
            c.with_watched(|d| d.is_none_or(|d| d.left().is_some()))
        };
        let close = |c: &crate::controls::Controls| c.close();
        entered_held(play, false, Some((&close, &left)), |rig, d| {
            assert_eq!(d.left(), Some(game::Verdict::End));
            assert!(rig.controls.quitting());
            assert!(!rig.failures.any(), "the close latched as a failure");
        });
        entered_held(play, false, None, |rig, d| {
            assert_eq!(d.left(), None);
            assert!(rig.failures.input().is_some(), "the control: a timeout");
        });
    }

    /// A2: the play's deadline inside its entry fence: the step runs once
    /// inside the wait (the ingress opened), nothing leaves, and the late
    /// answer changes nothing.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_deadline_inside_the_entry_fence_steps_once() {
        let play = |c: &std::path::Path| game::Pick::Play(c.to_owned());
        let stepped = |c: &crate::controls::Controls| c.ingress().is_open();
        let nothing = |_: &crate::controls::Controls| {};
        entered_held(play, true, Some((&nothing, &stepped)), |rig, d| {
            assert!(d.stepped(), "no step inside the fence");
            assert_eq!(d.left(), None);
            assert!(rig.controls.ingress().is_open());
            assert!(!rig.failures.any());
        });
    }

    /// A3: practice's deadline inside its entry fence, before its chart is
    /// read: the screen goes waiting for the read and never steps there
    /// (no ingress opened), and nothing leaves.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_practice_deadline_inside_the_entry_fence_waits_for_the_read() {
        let practice = |c: &std::path::Path| game::Pick::Practice(c.to_owned());
        let waiting = |c: &crate::controls::Controls| {
            c.with_watched(|d| d.is_none_or(game::Decision::waiting))
        };
        let nothing = |_: &crate::controls::Controls| {};
        entered_held(practice, true, Some((&nothing, &waiting)), |rig, d| {
            assert!(d.waiting(), "not waiting for the read");
            assert!(!d.stepped(), "stepped in a rig's wait");
            assert_eq!(d.left(), None);
            assert!(!rig.controls.ingress().is_open());
        });
    }

    /// Finding 10 at the fence (START on E as installed): START pressed
    /// during the decision screen and held through a refusal's return
    /// opens no panel even as seen by the return's own fence, the input
    /// thread held right after answering it: the held panels are masked
    /// before any level or fence answer is published. A fresh press opens
    /// it (the positive control).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_panel_held_through_the_decision_screen_is_masked_at_the_fence() {
        use bms_model::TimeSource as _;
        use game::AppRig as _;
        let dir = tempfile::tempdir().unwrap();
        let sel = selection(dir.path());
        crate::tests::on_rig_hooked(
            game::Settings {
                device: game::DeviceSettings::default(),
                player: game::PlayerSettings::default(),
            },
            crate::render_side::tests::Installers::default().spawn_bare(),
            input::QpcClock::start(),
            |rig, _, end| {
                let level = rig.panels.clone().expect("installed with the tap");
                rig.selection = Some(sel);
                let (thread, jobs, waker, clock) = input_thread(end);
                let press = |code, value| {
                    jobs.send(Box::new(move |s: &mut input::SourceSet| {
                        key_batch(s, clock.now(), code, value);
                    }))
                    .unwrap();
                    waker.wake();
                };
                // A wake of the input thread after everything sent before.
                let boundary = || {
                    let (seen, seen_rx) = std::sync::mpsc::channel();
                    jobs.send(Box::new(move |_: &mut input::SourceSet| {
                        seen.send(()).unwrap();
                    }))
                    .unwrap();
                    waker.wake();
                    seen_rx.recv_timeout(Duration::from_secs(60)).unwrap();
                    // Its whole boundary, the menu's included.
                    waker.wake();
                };
                rig.entered_selection();
                // The choice closes the word and the input thread sees it;
                // START comes down during the decision screen, after that.
                rig.controls.close_epoch();
                boundary();
                boundary();
                press(0x12, 1);
                boundary();
                boundary();
                // A refusal's return: the word opens, selection is entered
                // and its fence answered; the thread stops after it.
                rig.closed_epoch();
                let mut shown = None;
                at_fence(
                    rig,
                    Box::new(|_| {}),
                    &mut |rig| rig.entered_selection(),
                    &mut || shown = Some(level.read().start),
                );
                assert_eq!(
                    shown,
                    Some(false),
                    "held through the screen: a panel at the fence"
                );
                press(0x12, 0);
                press(0x12, 1);
                assert!(crate::render_side::tests::wait_until(|| level.read().start));
                thread.stop().unwrap();
            },
        );
    }

    /// The controller's Back past the decision screen's time `T` is taken
    /// by the time its packet was received: one received before `T` does
    /// not leave, though taken after it; one received after it does.
    #[test]
    fn a_back_is_taken_by_its_observation_time() {
        use bms_model::TimeUs;
        use eval::ControlId;
        use input::{MenuAction, MenuBindings, MenuEvent};
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let (mut tap, receiver) = input::menu_queue(MenuBindings {
            product: controller_product(),
            controls: vec![(ControlId(3), MenuAction::Press(MenuEvent::Back))],
            step: 1,
            relative: None,
            wrap: None,
        });
        tap.set_activity(Arc::clone(controls.tuning().activity()));
        tap.set_active(true);
        let back = |tap: &mut input::MenuTap, at: i64| {
            for value in [1, 0] {
                tap.offer(
                    &input::Packet {
                        device: input::DeviceId(7),
                        control: ControlId(3),
                        value,
                        background: false,
                    },
                    None,
                    TimeUs(at),
                );
            }
        };
        let mut pad = Pad {
            receiver: Some(receiver),
            dropped: 0,
        };
        let queue = MenuQueue::gated(Arc::clone(controls.tuning()));
        let deadline = TimeUs(100);
        controls.close_epoch();
        back(&mut tap, 99);
        assert!(
            !super::take_escape(&controls, Some(&queue), &mut pad, deadline),
            "a Back from before T left"
        );
        back(&mut tap, 100);
        assert!(
            super::take_escape(&controls, Some(&queue), &mut pad, deadline),
            "a Back at T did not leave"
        );
    }

    /// What the panels fixture's last six elements showed in each
    /// selection frame: options 21, 22 and 23's images drawn, and timers
    /// 21, 22 and 23's images' sampled times.
    #[cfg(feature = "dev-tools")]
    #[derive(Clone, Default)]
    struct PanelFrames(Arc<std::sync::Mutex<Vec<PanelFrame>>>);

    /// One frame of [`PanelFrames`].
    #[cfg(feature = "dev-tools")]
    type PanelFrame = ([bool; 3], [Option<TimeUs>; 3]);

    #[cfg(feature = "dev-tools")]
    impl crate::render_side::FrameSink for PanelFrames {
        fn frame(
            &mut self,
            screen: &dyn crate::render_side::Screen,
            _drawn: &crate::measure::Drawn<'_>,
        ) {
            let Some(observed) = screen.selection_observations() else {
                return;
            };
            let [.., a, b, c, t1, t2, t3] = observed.as_slice() else {
                return;
            };
            let drawn =
                |o: &skin_model::ElementObservation| o.outcome == skin_model::Outcome::Sampled;
            let timer = |o: &skin_model::ElementObservation| drawn(o).then_some(o.t).flatten();
            self.0.lock().unwrap().push((
                [drawn(a), drawn(b), drawn(c)],
                [timer(t1), timer(t2), timer(t3)],
            ));
        }
    }

    /// C16 through the player's rig and the render thread on WARP: the
    /// game thread's look reads the level the input side published and
    /// sends the update; the frames after it draw option 21 under START,
    /// 23 under both, 22 under SELECT and none once released, and each
    /// panel's timer image only while it is open, sampled at the time since
    /// the look that opened it: each panel opens twice, its timer starting
    /// over each time.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_rig_draws_the_panel_open() {
        use bms_model::TimeSource as _;
        use game::AppRig as _;
        use game::Preparation;
        use skin_model::ScreenKind;

        let dir = crate::selection_skin::panels_fixture();
        let limits = crate::render_common::offscreen(1_920, 1_080);
        let skin = crate::selection_skin::fixture_bundle(&limits, dir.path(), "s.json");
        let frames = PanelFrames::default();
        let make: crate::render_side::MakeScreen = Box::new(|| {
            let r = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(crate::menu_screen::MenuScreen::new(
                r,
                Some(render::text::Fonts::runtime()?),
            )) as Box<dyn crate::render_side::Screen>)
        });
        let clock = input::QpcClock::start();
        let link = crate::render_side::spawn(
            make,
            None,
            None,
            clock,
            crate::render_side::tests::view(),
            crate::render_side::tests::measure(clock),
            Box::new(frames.clone()),
        )
        .unwrap();
        assert_eq!(
            link.select(ScreenKind::Selection, Some(skin))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let charts = tempfile::tempdir().unwrap();
        let (sel, _) = tree(charts.path(), &[&["a.bms"]]);
        let (mut set, level) = panel_keys();
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let last = || frames.0.lock().unwrap().last().copied();
        crate::tests::on_rig_hooked(settings, link, clock, |rig, _, _| {
            // The level comes with the adjustments' tap; this one's input
            // side is the test's.
            assert!(rig.panels.is_some(), "installed with the tap");
            rig.adjust = None;
            rig.panels = Some(level);
            rig.selection = Some(sel);
            // The last frame draws `want`, each panel open sampled at most
            // the time since `since`, when the look that opened it began.
            let opened_since = |since: TimeUs, want: [bool; 3]| {
                let (open, timers) = last().unwrap();
                let bound = clock.now().0 - since.0;
                assert_eq!(open, want);
                for (p, timer) in timers.into_iter().enumerate() {
                    assert_eq!(timer.is_some(), want[p], "{timers:?}");
                    if let Some(t) = timer {
                        assert!(
                            (0..=bound).contains(&t.0),
                            "panel {p}: {t:?} not in 0..={bound}"
                        );
                    }
                }
            };
            // START held at the entry: panel 1 from the entry's update.
            panel_key(&mut set, 0x12, 1);
            let since = clock.now();
            rig.entered_selection();
            assert!(
                crate::render_side::tests::wait_until(|| last().is_some_and(|(open, timers)| open
                    == [true, false, false]
                    && timers[0].is_some())),
                "{:?}",
                last()
            );
            opened_since(since, [true, false, false]);
            let menu = Arc::clone(&rig.selection.as_ref().unwrap().menu);
            // Each step: a key of the keyboard reaching the tap, if any,
            // and keys on the window's queue, taken by one look.
            for (key, keys, want) in [
                (Some((0x12, 0)), vec![], [false; 3]),
                // Enter in the look that sees START: held back.
                (Some((0x12, 1)), vec![MenuKey::Enter], [true, false, false]),
                // The search box taking focus in a look closes the panel
                // in that look's update; leaving it opens it again.
                (None, vec![MenuKey::Tab], [false; 3]),
                (None, vec![MenuKey::Tab], [true, false, false]),
                (Some((0x11, 1)), vec![], [false, false, true]),
                (Some((0x12, 0)), vec![], [false, true, false]),
                (Some((0x11, 0)), vec![], [false; 3]),
                // Panels 2 and 3 again, each timer from its new opening.
                (Some((0x11, 1)), vec![], [false, true, false]),
                (Some((0x12, 1)), vec![], [false, false, true]),
                (Some((0x12, 0)), vec![], [false, true, false]),
                (Some((0x11, 0)), vec![], [false; 3]),
            ] {
                if let Some((code, value)) = key {
                    panel_key(&mut set, code, value);
                }
                for k in &keys {
                    menu.push(MenuInput::Key(*k));
                }
                // No step moves the list: Enter on the folder row the
                // cursor starts on would enter it.
                let view = |rig: &crate::play::PlayerRig<'_>| {
                    rig.selection.as_ref().unwrap().screen.view(0, 0, false)
                };
                let before = view(rig);
                let since = clock.now();
                assert!(rig.look().is_none(), "{key:?} {keys:?}");
                assert_eq!(view(rig), before, "{key:?} {keys:?}");
                // The update taken: the frames from here draw it.
                assert!(rig.render.fence(Duration::from_secs(60)));
                let seen = frames.0.lock().unwrap().len();
                assert!(
                    crate::render_side::tests::wait_until(|| frames.0.lock().unwrap().len() > seen),
                    "no frame"
                );
                // Every step opens its panel afresh.
                opened_since(since, want);
            }
        });
    }

    /// The select loop drains result commands on every look; while no
    /// result shows, that takes nothing from the queue, so the select
    /// screen's keys survive it and still choose.
    #[test]
    fn select_input_survives_the_select_loop_drain() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let mut result = ResultInput::new(Arc::clone(&sel.menu), None);
        for key in [MenuKey::Down, MenuKey::Down, MenuKey::Enter] {
            sel.menu.push(MenuInput::Key(key));
        }
        assert_eq!(result.take(), None);
        let chosen = sel.drain(&mut result.pad).expect("Enter chooses");
        assert_eq!(played(chosen).file_name().unwrap(), "c.bms");
    }

    /// The test controller's model.
    const fn controller_product() -> input::hid::HidProduct {
        input::hid::HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        }
    }

    /// The menus with a controller whose control 2 moves down, its source
    /// set, and the input thread's hook and link, in its tail as at its
    /// start (or after a close).
    fn controller() -> (
        ResultInput,
        input::SourceSet,
        crate::input_side::Hook,
        crate::input_side::InputLink,
        std::sync::Arc<crate::controls::Controls>,
    ) {
        use eval::ControlId;
        use input::{MenuAction, MenuBindings, MenuEvent};
        controller_with(MenuBindings {
            product: controller_product(),
            controls: vec![(ControlId(2), MenuAction::Press(MenuEvent::Down))],
            step: 2,
            relative: None,
            wrap: None,
        })
    }

    /// [`controller`] with `bindings`, of [`controller_product`].
    fn controller_with(
        bindings: input::MenuBindings,
    ) -> (
        ResultInput,
        input::SourceSet,
        crate::input_side::Hook,
        crate::input_side::InputLink,
        std::sync::Arc<crate::controls::Controls>,
    ) {
        use input::{DeviceId, DeviceKind, SourceConfig};
        let product = bindings.product;
        let (tap, pad) = input::menu_queue(bindings);
        let mut set = input::SourceSet::new(SourceConfig {
            max_devices: 1,
            max_sources: 1,
            max_held: 4,
            queue_capacity: 4,
            max_arrivals: 1,
        })
        .unwrap();
        set.begin_batch(TimeUs(0));
        set.connect(DeviceId(7), DeviceKind::Controller);
        set.set_product(DeviceId(7), product);
        set.end_batch(TimeUs(0));
        set.set_menu(tap);
        let controls = Arc::new(crate::controls::Controls::new(input::QpcClock::start()));
        controls.set_latch(set.gate().handle());
        controls.set_game(std::thread::current());
        let (hook, link) = crate::input_side::link(
            Arc::clone(&controls),
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let menus = ResultInput::new(Arc::new(MenuQueue::default()), Some(pad));
        (menus, set, hook, link, controls)
    }

    /// The controller's Down pressed in a batch at `at`, then the boundary.
    fn press_down(set: &mut input::SourceSet, hook: &mut crate::input_side::Hook, at: i64) {
        press(set, hook, 2, at);
    }

    /// The controller's `control` pressed and released in a batch at `at`,
    /// then the boundary.
    fn press(
        set: &mut input::SourceSet,
        hook: &mut crate::input_side::Hook,
        control: u32,
        at: i64,
    ) {
        set.begin_batch(TimeUs(at));
        for value in [1, 0] {
            set.packet(&input::Packet {
                device: input::DeviceId(7),
                control: eval::ControlId(control),
                value,
                background: false,
            });
        }
        set.end_batch(TimeUs(at));
        hook.wake(set, TimeUs(at));
    }

    /// The routine of every return to the practice page, as the rig runs
    /// it, after a round's opening was refused (recording closed into the
    /// input thread's tail, with no play over) and after a result's back:
    /// at the input thread's next boundary it is Idle, nothing plays, the
    /// end screen is gone, the scan goes on and what both queues held is
    /// dropped; a controller Down pressed then moves the page's cursor and
    /// reaches no result.
    #[test]
    fn controller_navigates_the_practice_page_after_a_logic_refusal() {
        recovers_to_the_practice_page(false);
    }

    /// [`controller_navigates_the_practice_page_after_a_logic_refusal`]
    /// from a round's result, left with back.
    #[test]
    fn controller_navigates_after_result_back_to_practice() {
        recovers_to_the_practice_page(true);
    }

    fn recovers_to_the_practice_page(from_result: bool) {
        use crate::result_input::Owner;
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let (mut menus, mut set, mut hook, link, _controls) = controller();
        menus.menu = Arc::clone(&sel.menu);
        // The play: started, and its opening refused, or its result shown.
        menus.release();
        if from_result {
            assert!(menus.enter());
        }
        let mut in_play = true;
        let mut end_page = Some(crate::render_side::EndPage::default());
        sel.shelf.pause();
        // Input queued meanwhile: a window key and a controller press from
        // when the menu was last on.
        set.menu_mut().unwrap().set_active(true);
        press_down(&mut set, &mut hook, 1_000);
        sel.menu.push(MenuInput::Key(MenuKey::Down));
        assert_eq!(hook.wake(&mut set, TimeUs(2_000)), input::Phase::Tail);
        super::recover(
            &mut menus,
            Owner::Practice,
            &link,
            &mut in_play,
            &mut end_page,
            Some(&mut sel.shelf),
        );
        assert_eq!(hook.wake(&mut set, TimeUs(3_000)), input::Phase::Idle);
        assert!(!in_play);
        assert_eq!(end_page, None);
        assert!(!sel.shelf.paused());
        assert_eq!(menus.owner(), Owner::Practice);
        assert_eq!(menus.take_practice(), None);
        // A fresh press moves the cursor.
        let chart = bms_parser::load_bms(
            b"#BPM 120\n#00111:01\n",
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .chart;
        let mut screen = game::PracticeScreen::new(&chart, &rules::tapestry(), 0);
        press_down(&mut set, &mut hook, 4_000);
        assert_eq!(menus.take(), None);
        let input = menus.take_practice().expect("the press reaches the page");
        assert_eq!(input, game::PracticeInput::Down);
        screen.input(input);
        assert_eq!(screen.row(), game::PracticeRow::To);
        assert_eq!(menus.take_practice(), None);
    }

    /// While the practice page owns the queues, selection's look takes
    /// nothing: a Down queued there stays the page's.
    #[test]
    fn selection_takes_nothing_queued_for_the_practice_page() {
        use crate::result_input::Owner;
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let mut menus = ResultInput::new(Arc::clone(&sel.menu), None);
        menus.take_over(Owner::Practice);
        sel.menu.push(MenuInput::Key(MenuKey::Down));
        sel.menu.push(MenuInput::Key(MenuKey::Enter));
        assert_eq!(sel.drain_owned(&mut menus, None, TimeUs(0)), None);
        assert_eq!(menus.take_practice(), Some(game::PracticeInput::Down));
        assert_eq!(menus.take_practice(), Some(game::PracticeInput::Confirm));
        // Selection's own, it chooses.
        menus.take_over(Owner::Select);
        sel.menu.push(MenuInput::Key(MenuKey::Enter));
        assert!(sel.drain_owned(&mut menus, None, TimeUs(0)).is_some());
    }

    /// A scenario's state on a practice page of a chart of `measures`
    /// measures, and its script of `steps`.
    #[cfg(feature = "dev-tools")]
    fn practice_script(
        measures: usize,
        steps: Vec<crate::dev::SelectStep>,
        preset: &rules::RulePreset,
    ) -> (crate::dev::SelectScript, crate::dev::scenario::State) {
        use std::fmt::Write as _;
        let mut text = "#BPM 240\n".to_owned();
        for m in 1..measures {
            writeln!(text, "#{m:03}11:01").unwrap();
        }
        let chart = bms_parser::load_bms(
            text.as_bytes(),
            std::path::Path::new("."),
            &Default::default(),
        )
        .unwrap()
        .chart;
        let screen = game::PracticeScreen::new(&chart, preset, 0);
        assert_eq!(screen.measures().len(), measures);
        let o = crate::dev::scenario::Origin {
            index: 1,
            kind: crate::dev::scenario::StepKind::Practice,
        };
        let script =
            crate::dev::SelectScript::compiled(steps.into_iter().map(|s| (s, o)).collect());
        let state = crate::dev::scenario::State {
            logs: Vec::new(),
            captures: crate::dev::scenario::Captures::of(Vec::new()),
            recorded: crate::input_side::Recorded {
                keyboard: Vec::new(),
                controller: Vec::new(),
                controller_device: None,
                scratch: game::ScratchConfig::default(),
            },
            chart_input: None,
            pending: None,
            until: None,
            sent: None,
            practice: Some(screen),
            shown: true,
            rounds: 0,
            nearest: None,
        };
        (script, state)
    }

    /// Runs the page's script as the page does: each key it presses acts on
    /// the page, which is shown again when it changed, a look taking
    /// `per_key` of the script's time; at most `looks` looks. The keys
    /// pressed, and how it ended.
    #[cfg(feature = "dev-tools")]
    fn drive(
        script: &mut crate::dev::SelectScript,
        state: &mut crate::dev::scenario::State,
        per_key: Duration,
        looks: usize,
    ) -> (Vec<MenuKey>, super::Driven) {
        let mut keys = Vec::new();
        for _ in 0..looks {
            script.age(per_key);
            match super::drive_practice(script, state) {
                super::Driven::Key(key) => {
                    keys.push(key);
                    let screen = state.practice.as_mut().unwrap();
                    let input = crate::practice_page::practice_key(key).unwrap();
                    if screen.input(input) == game::PracticeActed::Changed {
                        state.shown = true;
                    }
                }
                other => return (keys, other),
            }
        }
        (keys, super::Driven::Wait)
    }

    /// Setting a page far from where it opens takes a key per showing of
    /// the page, far more than fit in one step's wait at a look's pace; it
    /// times out only once the page stops changing. Measure 100 of 101
    /// takes 99 steps right on the first row, 25 s at four looks a second.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_far_practice_page_is_set_one_key_per_showing() {
        use crate::dev::SelectStep;
        let goal = crate::dev::scenario::PracticeGoal {
            from: 99,
            to: 100,
            speed: bms_model::Speed::from_twentieths(15).unwrap(),
            lead_in: TimeUs(2_000_000),
            value: game::PracticeScreen::new(
                &bms_model::ChartBuilder::new(150_000).build().unwrap(),
                &rules::tapestry(),
                0,
            )
            .value(),
            repeat: true,
            input: crate::dev::scenario::InputRef::Log(0),
        };
        let (mut script, mut state) = practice_script(
            101,
            vec![SelectStep::PracticeSet(goal), SelectStep::PracticeStart],
            &rules::tapestry(),
        );
        let (keys, ended) = drive(&mut script, &mut state, Duration::from_millis(250), 400);
        assert_eq!(ended, super::Driven::Wait, "{:?}", script.failure());
        assert!(keys.len() > 80, "{}", keys.len());
        assert_eq!(keys.last(), Some(&MenuKey::Enter));
        assert_eq!(script.front(), None);
        let screen = state.practice.as_ref().unwrap();
        assert_eq!((screen.from(), screen.to()), (99, 100));
        assert_eq!(screen.speed(), goal.speed);
        assert_eq!(screen.row(), game::PracticeRow::Start);
    }

    /// A value the page cannot show stalls it: once nothing changes for
    /// the step's wait, the step fails, naming the goal.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn an_unreachable_practice_value_still_times_out() {
        use crate::dev::SelectStep;
        let goal = crate::dev::scenario::PracticeGoal {
            from: 0,
            to: 2,
            speed: bms_model::Speed::NORMAL,
            lead_in: TimeUs(2_000_000),
            // Past what the gauge accepts, on its 1 % steps.
            value: rules::GaugeValue(200 * rules::GaugeValue::PERCENT),
            repeat: true,
            input: crate::dev::scenario::InputRef::Log(0),
        };
        let (mut script, mut state) =
            practice_script(2, vec![SelectStep::PracticeSet(goal)], &rules::tapestry());
        let (keys, ended) = drive(&mut script, &mut state, Duration::from_millis(250), 400);
        assert_eq!(ended, super::Driven::Wait);
        assert!(!keys.is_empty());
        // Stalled: the page shows no change, so nothing is pressed, and the
        // wait runs out.
        script.age(crate::dev::STEP_TIMEOUT);
        assert_eq!(
            super::drive_practice(&mut script, &mut state),
            super::Driven::Failed
        );
        let failure = script.failure().unwrap();
        assert!(failure.contains("did not reach"), "{failure}");
    }

    /// A gauge goal off the page's 1 % steps from the gauge's start (20 %
    /// from 20.5 %) is never shown: the page would only step about it,
    /// 20.5 to 19.5 and back. The step fails, named, within a bounded time.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_gauge_goal_off_the_pages_steps_fails() {
        use crate::dev::SelectStep;
        let mut preset = rules::tapestry();
        preset.gauges[0].start = rules::GaugeValue(2_050_000);
        let goal = crate::dev::scenario::PracticeGoal {
            from: 0,
            to: 2,
            speed: bms_model::Speed::NORMAL,
            lead_in: TimeUs(2_000_000),
            value: rules::GaugeValue(20 * rules::GaugeValue::PERCENT),
            repeat: true,
            input: crate::dev::scenario::InputRef::Log(0),
        };
        let (mut script, mut state) =
            practice_script(2, vec![SelectStep::PracticeSet(goal)], &preset);
        // Within twice the step's wait at four looks a second.
        let (_, ended) = drive(&mut script, &mut state, Duration::from_millis(250), 160);
        assert_eq!(ended, super::Driven::Failed);
        let failure = script.failure().unwrap();
        assert!(failure.starts_with("step 1 (practice): "), "{failure}");
    }

    /// Keys pressed during the play wait in the window's queue; the result
    /// drops them when it shows, and takes only what is pressed after.
    #[test]
    fn menu_input_from_the_play_is_dropped_at_the_result() {
        let menu = Arc::new(MenuQueue::default());
        let mut result = ResultInput::new(Arc::clone(&menu), None);
        for key in [MenuKey::Right, MenuKey::Left, MenuKey::Down] {
            menu.push(MenuInput::Key(key));
        }
        assert!(result.enter());
        assert!(!result.enter());
        assert_eq!(result.take(), None);
        menu.push(MenuInput::Text("x".to_owned()));
        menu.push(MenuInput::Key(MenuKey::Enter));
        menu.push(MenuInput::Key(MenuKey::Left));
        assert_eq!(result.take(), Some(game::MenuCommand::PrevPreset));
        assert_eq!(result.take(), None);
    }

    /// A chart refused because the shelf would not open shows why on the
    /// notice line; once the shelf opens, that notice goes. Another refusal
    /// stays when the shelf changes, and a folder that cannot be listed
    /// shows with a built-in chosen.
    #[test]
    fn a_preset_refusal_clears_once_the_shelf_opens() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let notice = |sel: &Selection| sel.screen.page("").lines[2].text.clone();
        let chart = dir.path().join("charts").join("a.bms");
        sel.presets("-".to_owned(), Some("mine.toml: broken".to_owned()), false);
        sel.refused(&chart, "a.bms: the preset cannot open".to_owned(), true);
        assert_eq!(notice(&sel), "a.bms: the preset cannot open");
        sel.presets("-".to_owned(), Some("mine.toml: broken".to_owned()), false);
        assert_eq!(notice(&sel), "a.bms: the preset cannot open");
        sel.presets("mine".to_owned(), None, true);
        assert_eq!(notice(&sel), "");
        // A refusal of the chart itself is not the shelf's to clear.
        sel.refused(&chart, "a.bms: no adapter reads it".to_owned(), false);
        sel.presets(
            "tapestry".to_owned(),
            Some("presets: denied".to_owned()),
            true,
        );
        assert_eq!(notice(&sel), "a.bms: no adapter reads it");
        sel.screen.set_notice(None);
        assert_eq!(notice(&sel), "presets: denied");
    }

    /// The play preset keys step it while the list has the keys: F11 and
    /// F12 step back and forth, in order, and Enter after them chooses
    /// with the steps taken. With the search box focused, or the IME
    /// composing, they step nothing; leaving the search box, they step
    /// again.
    #[test]
    fn preset_keys_step_only_outside_the_search_box() {
        use game::PresetStep::{Next, Prev};
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        for key in [
            MenuKey::NextPreset,
            MenuKey::PrevPreset,
            MenuKey::NextPreset,
        ] {
            sel.menu.push(MenuInput::Key(key));
        }
        assert_eq!(sel.drain(&mut Pad::default()), None);
        assert_eq!(
            Vec::from(std::mem::take(&mut sel.preset_steps)),
            [Next, Prev, Next]
        );
        // Focused: typed text only.
        for input in [
            MenuInput::Key(MenuKey::Tab),
            MenuInput::Key(MenuKey::NextPreset),
            MenuInput::Preedit("か".to_owned()),
            MenuInput::Key(MenuKey::PrevPreset),
            MenuInput::Preedit(String::new()),
            MenuInput::Key(MenuKey::NextPreset),
        ] {
            sel.menu.push(input);
        }
        assert_eq!(sel.drain(&mut Pad::default()), None);
        assert!(sel.screen.searching());
        assert!(sel.preset_steps.is_empty());
        // Left: they step again, before the choice that follows.
        for key in [MenuKey::Enter, MenuKey::PrevPreset, MenuKey::Enter] {
            sel.menu.push(MenuInput::Key(key));
        }
        assert!(sel.drain(&mut Pad::default()).is_some());
        assert_eq!(Vec::from(std::mem::take(&mut sel.preset_steps)), [Prev]);
    }

    /// Controller key 7, through the production bindings and the input
    /// thread's hook: on a result it retries, and a press queued there is
    /// dropped when selection takes the queues; on the select screen it
    /// steps to the next preset, and a black key never does. Key 1 still
    /// chooses. On the practice page it confirms.
    #[test]
    fn controller_key_7_steps_the_preset_on_select_only() {
        use crate::result_input::Owner;
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let (key1, key2, key7) = (11, 12, 17);
        let config = game::KeyConfig {
            lanes: [(key1, 1), (key2, 2), (key7, 7)]
                .map(|(control, lane)| game::LaneKey { control, lane })
                .to_vec(),
            retry: None,
            quit: None,
            ..game::KeyConfig::default()
        };
        let bindings = crate::select::menu_bindings(
            &config,
            Some(controller_product()),
            &game::ScratchConfig::default(),
        )
        .unwrap();
        let (mut menus, mut set, mut hook, link, _controls) = controller_with(bindings);
        menus.menu = Arc::clone(&sel.menu);
        // A result shows: key 7 retries, and one more stays queued.
        menus.release();
        assert!(menus.enter());
        set.menu_mut().unwrap().set_active(true);
        press(&mut set, &mut hook, key7, 1_000);
        assert_eq!(menus.take(), Some(game::MenuCommand::Retry));
        press(&mut set, &mut hook, key7, 2_000);
        // Back to selection: what was queued is dropped.
        let (mut in_play, mut end_lines) = (false, None);
        super::recover(
            &mut menus,
            Owner::Select,
            &link,
            &mut in_play,
            &mut end_lines,
            Some(&mut sel.shelf),
        );
        assert_eq!(hook.wake(&mut set, TimeUs(3_000)), input::Phase::Idle);
        assert_eq!(sel.drain_owned(&mut menus, None, TimeUs(0)), None);
        assert!(sel.preset_steps.is_empty());
        // A black key goes back: from the all-chart list to Top.
        press(&mut set, &mut hook, key2, 4_000);
        assert_eq!(sel.drain_owned(&mut menus, None, TimeUs(0)), None);
        assert!(sel.preset_steps.is_empty());
        assert_eq!(sel.screen.selected().map(|c| &c.path), None);
        // Key 7 steps.
        press(&mut set, &mut hook, key7, 5_000);
        assert_eq!(sel.drain_owned(&mut menus, None, TimeUs(0)), None);
        assert_eq!(
            Vec::from(std::mem::take(&mut sel.preset_steps)),
            [game::PresetStep::Next]
        );
        // A white key confirms: the "all charts" row, then a chart.
        press(&mut set, &mut hook, key1, 6_000);
        assert_eq!(sel.drain_owned(&mut menus, None, TimeUs(0)), None);
        press(&mut set, &mut hook, key1, 6_500);
        assert!(sel.drain_owned(&mut menus, None, TimeUs(0)).is_some());
        assert!(sel.preset_steps.is_empty());
        // The practice page takes it as Confirm.
        menus.take_over(Owner::Practice);
        press(&mut set, &mut hook, key7, 7_000);
        assert_eq!(menus.take_practice(), Some(game::PracticeInput::Confirm));
    }

    /// A write of `player.toml` that failed shows on the select screen in
    /// place of the refusal before it, and goes once the file is written;
    /// a later refusal shows over it.
    #[test]
    fn a_save_error_replaces_the_refusal_until_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let notice = |sel: &Selection| sel.screen.page("").lines[2].text.clone();
        let chart = dir.path().join("charts").join("a.bms");
        // The shelf would not open; a step opened it, and the write failed.
        sel.refused(&chart, "a.bms: the preset cannot open".to_owned(), true);
        sel.presets("beatoraja-style".to_owned(), None, true);
        sel.save_error(Some("denied"), true);
        assert_eq!(notice(&sel), "player.toml を保存できません: denied");
        assert!(!sel.preset_refusal);
        // Shown again with no new failure, a chart's refusal stays over it.
        sel.refused(&chart, "a.bms: no adapter reads it".to_owned(), false);
        sel.save_error(Some("denied"), false);
        assert_eq!(notice(&sel), "a.bms: no adapter reads it");
        sel.screen.set_notice(None);
        assert_eq!(notice(&sel), "player.toml を保存できません: denied");
        sel.save_error(None, false);
        assert_eq!(notice(&sel), "");
        // A chart's refusal shows; a write that fails after it replaces it.
        sel.refused(&chart, "a.bms: no adapter reads it".to_owned(), false);
        sel.save_error(Some("denied"), true);
        assert_eq!(notice(&sel), "player.toml を保存できません: denied");
    }

    /// The select screen shows the chosen file's error, else the folder's
    /// that cannot be listed, with a built-in chosen too; each goes once the
    /// shelf recovers; a shelf that cannot open shows why.
    #[test]
    fn the_select_screen_picks_the_file_error_else_the_listing_error() {
        let dir = tempfile::tempdir().unwrap();
        let presets = dir.path().join("presets");
        std::fs::create_dir_all(&presets).unwrap();
        let mine = presets.join("mine.toml");
        let write = |text: &str| {
            std::fs::write(&mine, text).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&mine)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
                .unwrap();
        };
        let shown = |shelf: &game::Shelf| {
            let (name, error) = super::shelf_shown(Ok(shelf));
            (name, error.map(|(text, _)| text))
        };
        write(&game::preset_to_toml(&rules::tapestry()));
        let mut chosen = game::Shelf::open(presets.clone(), "mine").unwrap();
        assert_eq!(shown(&chosen), ("mine".to_owned(), None));
        // The chosen file breaks: its error, by file name.
        write("windows = [");
        chosen.check();
        let (name, error) = shown(&chosen);
        assert_eq!(name, "mine");
        let error = error.unwrap();
        assert!(error.starts_with("mine.toml: "), "{error}");
        assert!(!error.contains(&*presets.to_string_lossy()), "{error}");
        // It recovers: nothing.
        write(&game::preset_to_toml(&rules::tapestry()));
        chosen.check();
        assert_eq!(shown(&chosen), ("mine".to_owned(), None));
        // A built-in chosen, the folder not listable: the folder's error;
        // listable again: nothing.
        let mut built_in = game::Shelf::open(presets.clone(), "tapestry").unwrap();
        let kept = dir.path().join("kept");
        std::fs::rename(&presets, &kept).unwrap();
        std::fs::write(&presets, b"").unwrap();
        built_in.check();
        let (name, error) = shown(&built_in);
        assert_eq!(name, "tapestry");
        assert!(error.unwrap().starts_with("presets: "));
        std::fs::remove_file(&presets).unwrap();
        std::fs::rename(&kept, &presets).unwrap();
        built_in.check();
        assert_eq!(shown(&built_in), ("tapestry".to_owned(), None));
        // A shelf that cannot open: why, with no name.
        let refused = game::Shelf::open(presets.clone(), "nothing").unwrap_err();
        let (name, error) = super::shelf_shown(Err(&refused));
        assert_eq!(name, "-");
        let (text, chain) = error.unwrap();
        assert!(text.contains("`nothing`"), "{text}");
        assert!(!chain.is_empty());
    }

    /// Consecutive edits of the search are searched once, at the end.
    #[test]
    fn consecutive_search_edits_are_searched_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        sel.menu.push(MenuInput::Key(MenuKey::Tab));
        let _ = sel.drain(&mut Pad::default());
        for c in ["c", "x"] {
            sel.menu.push(MenuInput::Text(c.to_owned()));
        }
        sel.menu.push(MenuInput::Key(MenuKey::Backspace));
        assert!(sel.drain(&mut Pad::default()).is_none());
        assert!(!sel.search_pending);
        assert_eq!(sel.screen.listed(), 1);
    }

    /// A play counted while the chart played shows on the select screen
    /// once selection is entered again, which searches the list and reads
    /// the bests of the chosen preset anew.
    #[test]
    fn bests_refresh_on_entering_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let preset = rules::tapestry();
        sel.screen.set_bests_preset(Some(preset.clone()));
        sel.entered(TimeUs(1));
        let line = |sel: &Selection| {
            sel.screen
                .page("")
                .lines
                .into_iter()
                .find(|l| l.text.ends_with(" A  / Z"))
                .unwrap()
                .text
        };
        assert!(line(&sel).starts_with(&format!("{} ☆", " ".repeat(4 + 17))));
        let chart = sel.screen.selected().unwrap().sha256;
        let c = library::Contribution {
            key: library::Key {
                preset_sha256: replay::preset_hash(&preset),
                ln_mode: library::LN_MODE,
                chart_sha256: chart,
            },
            id: library::PlayId {
                path: "ab/1-1.tprl".to_owned(),
                body_sha256: [7; 32],
                size: 1,
                mtime_ns: 1,
            },
            best: Some(library::Best {
                ex: 2,
                bp: 0,
                cleared: vec![0b1],
                full_combo: false,
            }),
            ineligible: None,
        };
        sel.shelf.best_writer().unwrap().ingest(&c).unwrap();
        // Unchanged until selection is entered.
        assert!(line(&sel).starts_with(&format!("{} ☆", " ".repeat(4 + 17))));
        sel.entered(TimeUs(5));
        assert_eq!(sel.entered_at, TimeUs(5));
        let lamp = preset.gauges[0].id.to_uppercase();
        assert_eq!(line(&sel), format!("    {lamp:11}     2 ☆-   A  / Z"));
    }

    /// Types `text` into the search box through the window's queue.
    #[cfg(feature = "dev-tools")]
    fn search(sel: &mut Selection, text: &str) {
        for input in [
            MenuInput::Key(MenuKey::Tab),
            MenuInput::Text(text.to_owned()),
            MenuInput::Key(MenuKey::Enter),
        ] {
            sel.menu.push(input);
        }
        assert!(sel.drain(&mut Pad::default()).is_none());
        assert_eq!(sel.screen.query(), text);
    }

    /// Runs `steps` as a scenario's on the select screen, draining the
    /// window's queue after each batch they type, until they are done or
    /// wait; whether any key was dropped.
    #[cfg(feature = "dev-tools")]
    fn run_steps(sel: &mut Selection, steps: Vec<crate::dev::SelectStep>) {
        use crate::dev::scenario::{Origin, StepKind};
        let o = Origin {
            index: 1,
            kind: StepKind::Select,
        };
        sel.script = Some(crate::dev::SelectScript::compiled(
            steps.into_iter().map(|s| (s, o)).collect(),
        ));
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let begun = Instant::now();
        while sel.script.as_ref().unwrap().front().is_some() {
            assert!(
                begun.elapsed() < Duration::from_secs(600),
                "the steps never ended"
            );
            if sel.script(&controls, 0, 0, None) {
                // Each batch typed is at most half the queue's keys.
                let mut batch = Vec::new();
                while let Some(input) = sel.menu.pop() {
                    batch.push(input);
                }
                let keys = batch
                    .iter()
                    .filter(|i| matches!(i, MenuInput::Key(_)))
                    .count();
                assert!(
                    keys <= crate::select::QUEUED_KEYS / 2,
                    "{keys} keys in one batch"
                );
                for input in batch {
                    sel.menu.push(input);
                }
                assert!(sel.drain(&mut Pad::default()).is_none());
            } else {
                sel.shelf.poll(Instant::now());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        assert_eq!(sel.menu.dropped(), 0, "a key was dropped");
    }

    /// A scenario's search replaces the text in the box, however long, by
    /// keys the window's queue holds, and picks the one chart it lists.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_query_replaces_the_search_text() {
        use crate::dev::SelectStep;
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        // Below the queue's keys, and past them.
        for (n, text) in [(63, "B"), (crate::select::QUEUED_KEYS * 2 + 5, "C")] {
            search(&mut sel, &"x".repeat(n));
            assert_eq!(sel.screen.listed(), 0);
            run_steps(
                &mut sel,
                vec![
                    SelectStep::Query(text.to_owned()),
                    SelectStep::PickSearched(text.to_owned()),
                ],
            );
            assert_eq!(sel.screen.query(), text);
            assert!(!sel.screen.searching());
            assert_eq!(sel.screen.listed(), 1);
            let chosen = sel.screen.selected().unwrap().path.clone();
            assert_eq!(
                chosen.file_name().unwrap(),
                format!("{}.bms", text.to_lowercase()).as_str()
            );
            // Back to an empty box for the next.
            run_steps(&mut sel, vec![SelectStep::Query(String::new())]);
        }
    }

    /// A path chosen after a search clears the search first, so the chart
    /// is listed and picked.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_path_pick_after_a_search_clears_it() {
        use crate::dev::SelectStep;
        use crate::dev::scenario::Target;
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        search(&mut sel, "B");
        let path = std::path::absolute(dir.path().join("charts").join("c.bms")).unwrap();
        let target = Target {
            identity: std::fs::canonicalize(&path).unwrap(),
            path,
        };
        run_steps(&mut sel, vec![SelectStep::PickPath(target)]);
        assert_eq!(sel.screen.query(), "");
        assert_eq!(sel.screen.listed(), 3);
        let chosen = sel.screen.selected().unwrap().path.clone();
        assert_eq!(chosen.file_name().unwrap(), "c.bms");
    }

    /// A gate the scan thread waits at before any work: its pass runs,
    /// listing nothing, until the test opens it.
    #[cfg(feature = "dev-tools")]
    type Gate = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

    /// [`unlisted`] with its first pass held at `Gate` until opened, and
    /// the script `steps`.
    #[cfg(feature = "dev-tools")]
    fn gated(dir: &std::path::Path, steps: &str) -> (Selection, Gate) {
        let gate: Gate = Arc::default();
        let held = Arc::clone(&gate);
        let mut sel = unlisted(dir, move |library, charts| {
            let options = library::ScanOptions {
                on_thread_start: Some(Arc::new(move || {
                    let (open, changed) = &*held;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = changed.wait(open).unwrap();
                    }
                })),
                ..library::ScanOptions::default()
            };
            Shelf::with_options(library, charts, Duration::from_secs(60), options)
        });
        sel.script = Some(crate::dev::SelectScript::parse(steps).unwrap());
        (sel, gate)
    }

    #[cfg(feature = "dev-tools")]
    fn open_gate(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    /// A pick waits as long as the pass that lists its chart runs, past the
    /// step's timeout (a background pass a loaded machine starves), and
    /// picks the chart once that pass lists it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_pick_waits_for_a_running_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, gate) = gated(dir.path(), "pick=c.bms");
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        assert!(!sel.script(&controls, 0, 0, None));
        // The step's timeout passes while the pass runs, listing nothing.
        sel.script.as_mut().unwrap().age(crate::dev::STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, 0, None));
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        // The pass goes on and lists the chart: the step picks it.
        open_gate(&gate);
        let begun = Instant::now();
        while sel.script.as_ref().unwrap().front().is_some() {
            assert!(
                begun.elapsed() < Duration::from_secs(600),
                "the chart was not picked"
            );
            if sel.shelf.poll(Instant::now()) {
                sel.refresh();
            }
            if sel.script(&controls, 0, 0, None) {
                assert!(sel.drain(&mut Pad::default()).is_none());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        let chosen = sel.screen.selected().unwrap().path.clone();
        assert_eq!(chosen.file_name().unwrap(), "c.bms");
    }

    /// `drawn` on the select screen holds the steps after it until a
    /// select frame is presented after it began, however many were
    /// before, and fails once the step's timeout passes without one.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn drawn_on_the_select_screen_waits_for_a_presented_frame() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, gate) = gated(dir.path(), "drawn,close");
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        assert!(!sel.script(&controls, 0, 5, None));
        assert!(!sel.script(&controls, 0, 5, None));
        assert_eq!(
            sel.script.as_ref().unwrap().front(),
            Some(&crate::dev::SelectStep::Drawn)
        );
        assert!(!sel.script(&controls, 0, 6, None));
        assert_eq!(sel.script.as_ref().unwrap().front(), None);
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);

        open_gate(&gate);

        let dir = tempfile::tempdir().unwrap();
        let (mut sel, gate) = gated(dir.path(), "drawn,close");
        assert!(!sel.script(&controls, 0, 5, None));
        sel.script.as_mut().unwrap().age(crate::dev::STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, 5, None));
        assert_eq!(
            sel.script.as_ref().unwrap().failure(),
            Some("the select screen was not drawn")
        );
        open_gate(&gate);
    }

    /// A pass that never ends fails the pick once the watchdog has run out,
    /// saying so.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_pick_fails_at_the_watchdog_of_a_running_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, gate) = gated(dir.path(), "pick=c.bms");
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        assert!(!sel.script(&controls, 0, 0, None));
        sel.script.as_mut().unwrap().age(crate::dev::STEP_WATCHDOG);
        assert!(!sel.script(&controls, 0, 0, None));
        let failure = sel.script.as_ref().unwrap().failure().unwrap().to_owned();
        assert!(failure.contains("scan passes included"), "{failure}");
        open_gate(&gate);
    }

    /// With no pass running, a chart that is not listed fails the pick
    /// once the step's timeout has run out: a pass whose charts never
    /// reach the screen is caught.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_pick_times_out_with_no_pass_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let begun = Instant::now();
        while sel.shelf.has_scan() {
            assert!(
                begun.elapsed() < Duration::from_secs(600),
                "the pass never ended"
            );
            sel.shelf.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(10));
        }
        sel.script = Some(crate::dev::SelectScript::parse("pick=z.bms").unwrap());
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        assert!(!sel.script(&controls, 0, 0, None));
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        sel.script.as_mut().unwrap().age(crate::dev::STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, 0, None));
        let failure = sel.script.as_ref().unwrap().failure().unwrap().to_owned();
        assert!(failure.contains("waited 20 s"), "{failure}");
    }

    /// A `background` step waits past [`crate::dev::STEP_TIMEOUT`] for the
    /// background measurement, which a loaded machine starves, and fails
    /// only at [`crate::dev::STEP_WATCHDOG`]. A pause keeps the queued
    /// job waiting, so the worker neither holds it nor is idle.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_background_step_outlasts_the_step_timeout() {
        use crate::dev::scenario::{Origin, StepKind};
        use crate::dev::{STEP_TIMEOUT, STEP_WATCHDOG, SelectScript, SelectStep};
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        sel.shelf.pause();
        let path = dir.path().join("charts").join("a.bms");
        sel.shelf.leave_to_background(crate::background::Job {
            source: game::ChartSource::new(path, 0, [0; 32]),
            rate: 48_000,
            name: "a.bms".to_owned(),
        });
        assert_eq!(sel.shelf.background(), (false, false));
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let o = Origin {
            index: 1,
            kind: StepKind::Select,
        };
        for (running, what) in [(true, "held nothing at its gate"), (false, "kept a job")] {
            sel.script = Some(SelectScript::compiled(vec![(
                SelectStep::Background(running),
                o,
            )]));
            sel.script.as_mut().unwrap().age(STEP_TIMEOUT);
            assert!(!sel.script(&controls, 0, 0, None));
            let script = sel.script.as_mut().unwrap();
            assert_eq!(script.failure(), None);
            assert!(script.front().is_some());
            script.age(STEP_WATCHDOG);
            assert!(!sel.script(&controls, 0, 0, None));
            let failure = sel.script.as_ref().unwrap().failure().unwrap();
            assert!(failure.contains(what), "{failure}");
        }
    }

    /// A `file` step holds the script until its file exists, past
    /// [`crate::dev::STEP_TIMEOUT`], then moves on; one whose file never
    /// appears fails at [`crate::dev::STEP_WATCHDOG`].
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_file_step_waits_for_its_file() {
        use crate::dev::scenario::{Origin, StepKind};
        use crate::dev::{STEP_TIMEOUT, STEP_WATCHDOG, SelectScript, SelectStep};
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        let o = Origin {
            index: 1,
            kind: StepKind::Select,
        };
        let go = dir.path().join("go");
        sel.script = Some(SelectScript::compiled(vec![(
            SelectStep::File(go.clone()),
            o,
        )]));
        sel.script.as_mut().unwrap().age(STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, 0, None));
        assert!(sel.script.as_ref().unwrap().front().is_some());
        std::fs::write(&go, "").unwrap();
        sel.script(&controls, 0, 0, None);
        let script = sel.script.as_ref().unwrap();
        assert_eq!((script.front(), script.failure()), (None, None));
        sel.script = Some(SelectScript::compiled(vec![(
            SelectStep::File(dir.path().join("never")),
            o,
        )]));
        sel.script.as_mut().unwrap().age(STEP_WATCHDOG);
        assert!(!sel.script(&controls, 0, 0, None));
        let failure = sel.script.as_ref().unwrap().failure().unwrap();
        assert!(failure.contains("never appeared"), "{failure}");
    }

    /// Listed charts, one per relative path under one root, or several roots,
    /// on a shelf that scans again at once, opened as a run opens (on Top) and
    /// with every chart listed. A file named `hidden*` is 5-key-shaped.
    pub(crate) fn tree(
        dir: &std::path::Path,
        roots: &[&[&str]],
    ) -> (Selection, Vec<std::path::PathBuf>) {
        let roots: Vec<_> = roots
            .iter()
            .enumerate()
            .map(|(i, files)| {
                let root = dir.join(format!("root{i}"));
                for file in *files {
                    write_chart(&root, file);
                }
                root
            })
            .collect();
        over(dir, roots)
    }

    /// [`tree`] over charts already written under `roots`.
    fn over(
        dir: &std::path::Path,
        roots: Vec<std::path::PathBuf>,
    ) -> (Selection, Vec<std::path::PathBuf>) {
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        let shelf = Shelf::new(
            library,
            roots.clone(),
            Duration::ZERO,
            None,
            &crate::shelf::tests::coordinator(),
        );
        let mut sel = Selection {
            shelf,
            screen: SelectScreen::new(roots.clone()),
            menu: Arc::new(MenuQueue::default()),
            dropped: 0,
            pointer: crate::pointer::PointerPass::default(),
            dropped_pointer: 0,
            feed: crate::selection_skin::SkinFeed::default(),
            preset_refusal: false,
            ime: None,
            rate: 48_000,
            chosen_at: TimeUs(0),
            loading: None,
            decision: None,
            practicing: false,
            entered_at: TimeUs(0),
            dirty: false,
            search_pending: false,
            preset_steps: std::collections::VecDeque::new(),
            #[cfg(feature = "dev-tools")]
            script: None,
            coord: crate::shelf::tests::coordinator(),
            spawner: game::coord::Spawner::SYSTEM,
            page: crate::page_jobs::PageJobs::new(Arc::new(crate::priority::Unchanged)),
            decodes: None,
            after_bank: None,
            unload_fault: None,
            #[cfg(feature = "dev-tools")]
            witness: crate::bank_witness::BankWitness::new(),
        };
        let listed: usize = roots.iter().map(|r| walk_count(r)).sum();
        settle(&mut sel);
        if roots.len() > 1 {
            // Overlapping roots count their shared charts in each.
            return (sel, roots);
        }
        assert_eq!(sel.screen.listed(), listed, "{:?}", list(&sel));
        (sel, roots)
    }

    /// The listed charts under `dir`.
    fn walk_count(dir: &std::path::Path) -> usize {
        let mut n = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                n += walk_count(&path);
            } else if !path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("hidden")
            {
                n += 1;
            }
        }
        n
    }

    /// Writes the chart at `file` under `root`, titled by its file's stem.
    fn write_chart(root: &std::path::Path, file: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let lane = if stem.starts_with("hidden") {
            "11"
        } else {
            "38"
        };
        std::fs::write(&path, format!("#TITLE {stem}\n#BPM 120\n#001{lane}:01\n")).unwrap();
    }

    /// Reads the list once two scan passes have completed after now, so the
    /// second started after any change the test made. The bound only catches
    /// a scan that stops passing; what the list then shows is the test's to
    /// assert.
    fn settle(sel: &mut Selection) {
        let target = sel.shelf.passes() + 2;
        let begun = Instant::now();
        while sel.shelf.passes() < target {
            assert!(
                begun.elapsed() < Duration::from_secs(600),
                "the scan stopped passing"
            );
            sel.shelf.poll(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
        sel.refresh();
    }

    fn keys(sel: &mut Selection, pressed: &[MenuKey]) -> Option<game::Pick> {
        for key in pressed {
            sel.menu.push(MenuInput::Key(*key));
        }
        sel.drain(&mut Pad::default())
    }

    /// The list's lines as drawn, a chart's by its title's end of line, the
    /// cursor's with `>`.
    fn list(sel: &Selection) -> Vec<String> {
        let page = sel.screen.page("");
        page.lines[3..page.lines.len() - 1]
            .iter()
            .filter(|l| !l.text.is_empty())
            .map(|l| {
                let text = l.text.trim_start();
                let name = text.rsplit("☆-").next().unwrap().trim();
                if l.style == crate::menu_screen::LineStyle::Cursor {
                    format!(">{name}")
                } else {
                    name.to_owned()
                }
            })
            .collect()
    }

    /// The cursor's row as [`list`] names it, without its mark, and its line.
    fn cursor(sel: &Selection) -> (String, usize) {
        let lines = list(sel);
        let at = lines.iter().position(|l| l.starts_with('>')).unwrap();
        (lines[at][1..].to_owned(), at)
    }

    /// The skin's update: its view is the installed song list's window
    /// (three rows above the cursor, three below, no heading), with the
    /// player's labels; timer `change` starts at the entry and again when
    /// the cursor's row or the place changes, not on a refresh keeping the
    /// row; each entry is a new lifetime with its own starts; a preset step
    /// reaches the strip with the preset's name.
    #[test]
    fn the_skins_update_follows_the_cursor_and_the_place() {
        use crate::selection_skin::SkinFeed;
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(
            dir.path(),
            &[&["s1/song/a.bms", "s2/song/b.bms", "d.bms", "song/c.bms"]],
        );
        sel.feed = SkinFeed::default();
        sel.feed.window = Some((3, 7));
        let at = |sel: &mut Selection, t: i64| {
            let page = sel.screen.page("");
            sel.feed.update(&sel.screen, page, TimeUs(t))
        };
        sel.feed.enter(TimeUs(100));
        let u = at(&mut sel, 100);
        assert_eq!((u.lifetime, u.revision), (1, 1));
        assert_eq!(u.labels, crate::select::labels());
        // Top: the all-charts row, s1, s2, c, d; three below the cursor.
        assert_eq!((u.view.rows.len(), u.view.cursor), (4, Some(0)));
        assert_eq!(
            (u.events.entered, u.events.changed),
            (TimeUs(100), TimeUs(100))
        );
        keys(&mut sel, &[MenuKey::Down]);
        let u = at(&mut sel, 200);
        assert_eq!((u.view.rows.len(), u.view.cursor), (5, Some(1)));
        assert_eq!(u.events.changed, TimeUs(200));
        // A refresh keeping the cursor's row: no restart, a new list.
        let lists = u.list_revision;
        sel.refresh();
        let u = at(&mut sel, 300);
        assert_eq!(u.events.changed, TimeUs(200));
        assert!(u.list_revision > lists);
        // Into folder s1: the place changes.
        keys(&mut sel, &[MenuKey::Enter]);
        let u = at(&mut sel, 400);
        assert_eq!(
            u.view.place,
            skin_model::SelectionPlace::Folder(vec!["s1".to_owned()])
        );
        assert_eq!(u.events.changed, TimeUs(400));
        // Back, onto the all-charts row, into the all-chart list: rows by
        // folder, none of them a heading.
        keys(&mut sel, &[MenuKey::Escape, MenuKey::Home, MenuKey::Enter]);
        let u = at(&mut sel, 500);
        assert_eq!(u.view.place, skin_model::SelectionPlace::All);
        assert_eq!((u.view.rows.len(), u.view.cursor), (4, Some(0)));
        assert!(
            u.view
                .rows
                .iter()
                .all(|r| r.kind == skin_model::RowKind::Chart)
        );
        assert_eq!(u.events.changed, TimeUs(500));
        // The next entry: another lifetime, its own starts.
        sel.feed.enter(TimeUs(600));
        let u = at(&mut sel, 600);
        assert_eq!((u.lifetime, u.revision), (2, 1));
        assert_eq!(
            (u.events.entered, u.events.changed),
            (TimeUs(600), TimeUs(600))
        );
        // Without a skin the view is the cursor's row alone; the keys act
        // the same.
        sel.feed.window = None;
        assert_eq!(at(&mut sel, 700).view.rows.len(), 1);
    }

    /// What startup leaves the select screen reaches its updates: the
    /// installed skin's window makes the view, and a refusal is the
    /// notice.
    #[test]
    fn startup_gives_the_updates_the_skins_window_and_the_notice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["a.bms", "b.bms", "c.bms"]]);
        sel.started(Some("refused".to_owned()), Some((3, 7)));
        sel.feed.enter(TimeUs(0));
        let page = sel.screen.page("");
        let u = sel.feed.update(&sel.screen, page, TimeUs(0));
        // Top: the all-charts row and the three charts, the cursor first.
        assert_eq!((u.view.rows.len(), u.view.cursor), (4, Some(0)));
        assert_eq!(u.strip.notice.as_deref(), Some("refused"));
    }

    /// `f` on a rig of the game's threads whose render thread is `render`,
    /// on `screens`, with `sel` on its select screen, entered and its first
    /// update drawn (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub(crate) fn on_selection<R>(
        sel: Selection,
        screens: &crate::render_side::tests::Installers,
        render: crate::render_side::RenderLink,
        f: impl FnOnce(&mut crate::play::PlayerRig<'_>) -> R,
    ) -> R {
        use game::AppRig as _;
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let (out, _) = crate::tests::on_rig_with(settings, render, |rig, _| {
            // No adjustment waits on the input thread, which none serves.
            rig.adjust = None;
            rig.selection = Some(sel);
            rig.entered_selection();
            assert!(crate::render_side::tests::wait_until(|| {
                screens
                    .selections
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s.lifetime == 1)
            }));
            f(rig)
        });
        out
    }

    /// Closes the window when dropped, so a run waiting on it ends even
    /// when the check driving it fails.
    #[cfg(feature = "dev-tools")]
    struct CloseOnDrop<'a>(&'a crate::controls::Controls);

    #[cfg(feature = "dev-tools")]
    impl Drop for CloseOnDrop<'_> {
        fn drop(&mut self) {
            self.0.close();
        }
    }

    /// A device lost while selection shows is not an entry (design.md
    /// "Rendering and Skins", Screen skins): through the app's run, the
    /// screen built again draws the same lifetime with the skin's state and
    /// the timers' starts kept; a real entry again (after a refused chart)
    /// is a new lifetime with a fresh state and its own starts.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_device_recovery_keeps_the_entry_and_an_entry_again_starts_anew() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let (sel, _) = tree(dir.path(), &[&["a.bms"]]);
        let menu = Arc::clone(&sel.menu);
        let screens = crate::render_side::tests::Installers::default();
        let render = screens.spawn_bare();
        assert_eq!(
            render
                .select(
                    skin_model::ScreenKind::Selection,
                    Some(crate::render_side::tests::bundle())
                )
                .wait()
                .unwrap(),
            game::Preparation::Ready
        );
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        // A preset that does not exist: a chart chosen is refused, and
        // selection entered again.
        let mut app_settings = settings.clone();
        app_settings.player.preset = "missing".to_owned();
        let seen = || screens.selections.lock().unwrap().clone();
        let skinned = |lifetime: u64| {
            seen()
                .into_iter()
                .find(|s| s.lifetime == lifetime && s.state.is_some())
        };
        let (ran, _) = crate::tests::on_rig_with(settings, render, |rig, _| {
            rig.adjust = None;
            rig.selection = Some(sel);
            let controls = rig.controls;
            let mut app = game::App::new(
                DataRoot::new(dir.path().join("app")),
                app_settings,
                crate::session_config(replay::ReceiveMethod::Scripted, None),
                Box::new(|s: &game::Settings| game::Parts::unsaved(&s.player)),
                Box::new(game::NoSessionEvents),
            );
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _close = CloseOnDrop(controls);
                    assert!(crate::render_side::tests::wait_until(
                        || skinned(1).is_some()
                    ));
                    let first = skinned(1).unwrap();
                    // The device lost; once the flow built the screen
                    // again, the new one draws.
                    screens.fail.store(true, Ordering::Release);
                    assert!(crate::render_side::tests::wait_until(|| {
                        screens.builds.load(Ordering::Acquire) >= 2
                    }));
                    screens.fail.store(false, Ordering::Release);
                    assert!(crate::render_side::tests::wait_until(|| {
                        seen().iter().any(|s| s.device != first.device)
                    }));
                    let again = seen()
                        .into_iter()
                        .find(|s| s.device != first.device)
                        .unwrap();
                    assert_eq!(
                        (again.lifetime, again.state, again.events.entered),
                        (1, first.state, first.events.entered),
                        "{again:?}"
                    );
                    assert_eq!(again.events.changed, first.events.changed);
                    // The chart chosen and refused: selection entered again.
                    menu.push(MenuInput::Key(MenuKey::End));
                    menu.push(MenuInput::Key(MenuKey::Enter));
                    assert!(crate::render_side::tests::wait_until(
                        || skinned(2).is_some()
                    ));
                    let next = skinned(2).unwrap();
                    assert_ne!(next.state, first.state);
                    assert!(next.events.entered > first.events.entered, "{next:?}");
                    assert!(seen().iter().all(|s| s.lifetime <= 2), "{:?}", seen());
                });
                app.run(rig)
            })
        });
        assert!(ran.is_ok(), "{ran:?}");
    }

    /// Leaving selection for a chart's loading or for the practice page
    /// ends its lifetime: an update the select screen makes after it is
    /// never drawn (design.md "Rendering and Skins", Screen skins).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn leaving_for_a_chart_or_the_practice_page_draws_no_later_update() {
        use bms_model::TimeSource as _;
        use game::AppRig as _;
        for practice in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (sel, roots) = tree(dir.path(), &[&["a.bms"]]);
            let chart = roots[0].join("a.bms");
            let screens = crate::render_side::tests::Installers::default();
            let drawn = on_selection(sel, &screens, screens.spawn_bare(), |rig| {
                if practice {
                    let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
                    rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
                } else {
                    rig.entered_loading(&chart);
                }
                let sel = rig.selection.as_mut().unwrap();
                sel.dirty = true;
                sel.show(&rig.render, None, rig.clock.now());
                assert!(rig.render.fence(Duration::from_secs(5)));
                screens
                    .selections
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|s| s.revision)
                    .collect::<Vec<_>>()
            });
            assert!(
                drawn.contains(&1) && !drawn.contains(&2),
                "{practice}: {drawn:?}"
            );
        }
    }

    /// A controller whose key 7 retries and key 2 goes back on a result,
    /// with its result
    /// input taken over by a showing result (`controller_with`).
    #[cfg(feature = "dev-tools")]
    fn result_controller() -> (
        ResultInput,
        input::SourceSet,
        crate::input_side::Hook,
        crate::input_side::InputLink,
    ) {
        // Key 7 retries and key 2, a black key, goes back.
        let config = game::KeyConfig {
            lanes: [(12, 2), (17, 7)]
                .map(|(control, lane)| game::LaneKey { control, lane })
                .to_vec(),
            retry: None,
            quit: None,
            ..game::KeyConfig::default()
        };
        let bindings = crate::select::menu_bindings(
            &config,
            Some(controller_product()),
            &game::ScratchConfig::default(),
        )
        .unwrap();
        let (mut menus, mut set, hook, link, _controls) = controller_with(bindings);
        menus.release();
        assert!(menus.enter());
        set.menu_mut().unwrap().set_active(true);
        (menus, set, hook, link)
    }

    /// Each step a later instant on the rig's clock.
    #[cfg(feature = "dev-tools")]
    fn later(rig: &crate::play::PlayerRig<'_>) -> TimeUs {
        use bms_model::TimeSource as _;
        let t = rig.clock.now();
        while rig.clock.now() == t {}
        rig.clock.now()
    }

    /// The play state's clock starts where the game begins a play state
    /// (`Rig::play_state_began`; design.md "beatoraja Lua Skins", the
    /// state clock), at the instant the turntable's play state starts: a
    /// chart chosen to play, before its loading state, which keeps it. A
    /// practice session, begun at its choice, keeps its first origin
    /// through its page, Starts, rounds and retries, until selection is
    /// entered again, and so does its turntable's play state: the flow
    /// begins none for a retry within practice.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_state_clock_starts_where_a_play_state_is_entered() {
        use bms_model::TimeSource as _;
        use game::{AppRig as _, Rig as _};
        let dir = tempfile::tempdir().unwrap();
        let (sel, roots) = tree(dir.path(), &[&["a.bms"]]);
        let chart = roots[0].join("a.bms");
        let screens = crate::render_side::tests::Installers::default();
        on_selection(sel, &screens, screens.spawn_bare(), |rig| {
            let turntable =
                |rig: &crate::play::PlayerRig<'_>| rig.play_states.current().map(|s| s.origin);
            // A chart chosen to play: the play state begins, then its
            // loading state and preparation keep it.
            let before = later(rig);
            rig.play_state_began();
            let play = rig.state_clock.origin().unwrap();
            assert!(play >= before && play <= rig.clock.now(), "{play:?}");
            assert_eq!(Some(play), turntable(rig));
            later(rig);
            rig.entered_loading(&chart);
            rig.prepare_screen().unwrap();
            assert_eq!(rig.state_clock.origin(), Some(play));
            // Back to selection, then another chart: afresh.
            rig.entered_selection();
            let before = later(rig);
            rig.play_state_began();
            let next = rig.state_clock.origin().unwrap();
            assert!(next >= before && next > play);
            assert_eq!(Some(next), turntable(rig));
            // A practice session, begun at its choice.
            let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
            let page = game::PracticeScreen::new(&c, &rules::tapestry(), 0);
            rig.entered_selection();
            rig.selection.as_mut().unwrap().practicing = true;
            let before = later(rig);
            rig.play_state_began();
            let session = rig.state_clock.origin().unwrap();
            assert!(session >= before);
            // Its page, a Start's loading state, consecutive rounds (each
            // prepared again), a retry (which begins no play state within
            // practice, the flow resuming the turntable instead) and the
            // page again.
            later(rig);
            rig.entered_practice(&page);
            rig.entered_loading(&chart);
            rig.prepare_screen().unwrap();
            later(rig);
            rig.prepare_screen().unwrap();
            later(rig);
            rig.turntable_resumed();
            rig.prepare_screen().unwrap();
            rig.entered_practice(&page);
            rig.entered_loading(&chart);
            assert_eq!(rig.state_clock.origin(), Some(session));
            // The turntable's play state and the state clock agree: one
            // origin for the session.
            assert_eq!(turntable(rig), Some(session));
            // Selection ends it: the next session starts afresh.
            rig.entered_selection();
            let before = later(rig);
            rig.play_state_began();
            assert!(rig.state_clock.origin().unwrap() >= before);
        });
    }

    /// The controller's key 7 pressed at `at`, its menu active as a
    /// showing result keeps it.
    #[cfg(feature = "dev-tools")]
    fn pad_retry(set: &mut input::SourceSet, hook: &mut crate::input_side::Hook, at: TimeUs) {
        set.menu_mut().unwrap().set_active(true);
        press(set, hook, 17, at.0);
    }

    /// Every retry the game acts on outside practice starts the next
    /// play's state clock where the flow begins its play state
    /// (`Rig::play_state_began`, with the turntable's), and its load time
    /// at its receipt (design.md "beatoraja Lua Skins", the state clock).
    /// The flow begins the play state after the play's end for a retry
    /// taken in a play's states (`Rig::take`) or Closing (`take_closing`),
    /// after the withdrawal for the first retry taken while the result is
    /// prepared (`withdraw_result`), and right after the serve for the
    /// retry a result accepts (`accept_leaving`, `serve_leaving`, after
    /// its outro or at once): the calls here follow that order. On a
    /// result (`take_menu`), the retry key's or a controller's (received
    /// when taken), only the retry the flow acts on is the next play's: a
    /// retry gated, queued behind the accepted command or a back, taken
    /// while the outro plays or left from an earlier result is none.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn every_accepted_retry_starts_the_state_clock_afresh() {
        use bms_model::TimeSource as _;
        use game::{AppRig as _, Command, MenuCommand, Rig as _};
        let dir = tempfile::tempdir().unwrap();
        let (sel, roots) = tree(dir.path(), &[&["a.bms"]]);
        let chart = roots[0].join("a.bms");
        let screens = crate::render_side::tests::Installers::default();
        on_selection(sel, &screens, screens.spawn_bare(), |rig| {
            rig.play_state_began();
            rig.entered_loading(&chart);
            // The retry key, received now: its receipt.
            let key = |rig: &mut crate::play::PlayerRig<'_>| {
                later(rig);
                rig.controls.command(Command::Retry);
                rig.controls.retry_at().unwrap()
            };
            // The flow begins the play state later than `after`, then
            // prepares it: the clock is the turntable's, the load time
            // from `receipt`.
            let begun =
                |rig: &mut crate::play::PlayerRig<'_>, receipt: Option<TimeUs>, after: TimeUs| {
                    later(rig);
                    rig.play_state_began();
                    rig.prepare_screen().unwrap();
                    let origin = rig.state_clock.origin().unwrap();
                    assert_eq!(Some(origin), rig.play_states.current().map(|s| s.origin));
                    assert!(origin > after, "{origin:?} {after:?}");
                    assert_eq!(rig.retry_from, receipt);
                    origin
                };
            // The play's states: acted on as taken, begun after the end.
            type Takes = fn(&mut crate::play::PlayerRig<'_>) -> bool;
            let plays: [(&str, Takes); 2] = [
                ("take", |rig| rig.take() == Some(Command::Retry)),
                ("take_closing", |rig| {
                    rig.take_closing() == Some(Command::Retry)
                }),
            ];
            for (taken_by, take) in plays {
                let receipt = key(rig);
                assert!(take(rig), "{taken_by}");
                begun(rig, Some(receipt), receipt);
            }
            let (menus, mut set, mut hook, _link) = result_controller();
            rig.result_input = menus;
            // ResultPreparing withdrawn by the first retry taken; a
            // controller's queued behind it is dropped.
            let first = key(rig);
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            pad_retry(&mut set, &mut hook, later(rig));
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            rig.withdraw_result(Some(MenuCommand::Retry));
            begun(rig, Some(first), first);
            // ResultPreparing withdrawn by a controller's back: a retry
            // queued behind it is no play's, and no play state begins.
            rig.result_input.enter();
            set.menu_mut().unwrap().set_active(true);
            press(&mut set, &mut hook, 12, later(rig).0);
            pad_retry(&mut set, &mut hook, later(rig));
            assert_eq!(rig.take_menu(), Some(MenuCommand::Back));
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            rig.withdraw_result(Some(MenuCommand::Back));
            assert_eq!(rig.pending_retry, None);
            // Result: a gated retry dropped, then one accepted, served
            // after its outro (what is taken while it plays dropped) or at
            // once; the play state begins at the serve.
            for after_outro in [true, false] {
                // A result shows again (the preparation left the last).
                rig.result_input.enter();
                key(rig);
                assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
                let accepted = key(rig);
                assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
                rig.accept_leaving(MenuCommand::Retry);
                if after_outro {
                    key(rig);
                    assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
                    pad_retry(&mut set, &mut hook, later(rig));
                    assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
                    // The outro plays on after an early press.
                    let end = TimeUs(rig.clock.now().0 + 2_000);
                    while rig.clock.now() < end {}
                }
                let served = later(rig);
                rig.serve_leaving(MenuCommand::Retry);
                let origin = begun(rig, Some(accepted), served);
                let outro = if after_outro { 2_000 } else { 1 };
                assert!(origin.0 - accepted.0 >= outro, "{after_outro}");
            }
            // Result: a controller's retry accepted, received when taken.
            rig.result_input.enter();
            pad_retry(&mut set, &mut hook, later(rig));
            let before = later(rig);
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            let after = rig.clock.now();
            rig.accept_leaving(MenuCommand::Retry);
            rig.serve_leaving(MenuCommand::Retry);
            later(rig);
            rig.play_state_began();
            rig.prepare_screen().unwrap();
            let received = rig.retry_from.unwrap();
            assert!(received >= before && received <= after, "{received:?}");
            // A retry an earlier result offered and never acted on is no
            // later result's: a later preparation withdrawn by its own
            // retry loads from that one's receipt.
            key(rig);
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            rig.prepare_screen().unwrap();
            let withdrawing = key(rig);
            assert_eq!(rig.take_menu(), Some(MenuCommand::Retry));
            rig.withdraw_result(Some(MenuCommand::Retry));
            begun(rig, Some(withdrawing), withdrawing);
        });
    }

    /// A selection skin refused at runtime is said on the select screen:
    /// the game thread's next look puts the reason in the notice of the
    /// update it sends (design.md "Rendering and Skins", Screen skins).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_runtime_refusal_is_the_notice_of_the_next_update() {
        use game::AppRig as _;
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let (sel, _) = tree(dir.path(), &[&["a.bms"]]);
        let screens = crate::render_side::tests::Installers::default();
        let render = screens.spawn_bare();
        let skin = crate::render_side::tests::bundle();
        assert_eq!(
            render
                .select(skin_model::ScreenKind::Selection, Some(skin))
                .wait()
                .unwrap(),
            game::Preparation::Ready
        );
        let next = on_selection(sel, &screens, render, |rig| {
            screens.refuse_selection.store(true, Ordering::Release);
            assert!(crate::render_side::tests::wait_until(|| {
                screens
                    .selections
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|s| s.state.is_none())
            }));
            let before = screens.updates.lock().unwrap().len();
            assert!(rig.choose().is_none());
            assert!(crate::render_side::tests::wait_until(|| {
                screens.updates.lock().unwrap().len() > before
            }));
            screens.updates.lock().unwrap().last().unwrap().clone()
        });
        assert_eq!((next.lifetime, next.revision), (1, 2));
        let notice = next.strip.notice.unwrap_or_default();
        assert!(notice.contains("refused for the test"), "{notice}");
    }

    const ALL: &str = "▶ 全譜面";

    fn all_row(n: u64) -> String {
        format!("{ALL}  ({n})")
    }

    fn folder_row(name: &str, n: u64) -> String {
        format!("▶ {name}  ({n})")
    }

    /// Selection opens on Top with the "all charts" row first under the
    /// cursor, then the one root's level: its subfolders' rows, then its own
    /// charts and those of the song folders directly under it. It has no
    /// heading, and a hidden chart has no row.
    #[test]
    fn selection_opens_on_top_with_the_all_charts_row_first() {
        let dir = tempfile::tempdir().unwrap();
        let (sel, _) = tree(
            dir.path(),
            &[&[
                "s1/song/a.bms",
                "s2/song/b.bms",
                "d.bms",
                "song/c.bms",
                "hidden/hidden.bms",
            ]],
        );
        assert_eq!(
            list(&sel),
            [
                format!(">{}", all_row(4)),
                folder_row("s1", 1),
                folder_row("s2", 1),
                "c".to_owned(),
                "d".to_owned(),
            ]
        );
    }

    /// Each place's title in full up to the preset, a non-ASCII folder's
    /// name shown as stored on its row, its heading and its title, and the
    /// all-chart list's.
    #[test]
    fn each_place_is_named_in_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["曲集/x/a.bms", "曲集/y/b.bms"]]);
        let title = |sel: &Selection| sel.screen.page("").lines[0].text.clone();
        let order = "  並び順: タイトル  プリセット: ";
        assert!(title(&sel).starts_with(&format!("曲選択  2 譜面{order}")));
        assert_eq!(
            list(&sel),
            [format!(">{}", all_row(2)), folder_row("曲集", 2)]
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert!(
            title(&sel).starts_with(&format!("曲選択  曲集  2 譜面{order}")),
            "{}",
            title(&sel)
        );
        keys(&mut sel, &[MenuKey::Escape, MenuKey::Up, MenuKey::Enter]);
        assert!(
            title(&sel).starts_with(&format!("曲選択  全譜面  2 譜面{order}")),
            "{}",
            title(&sel)
        );
        let page = sel.screen.page("");
        assert!(
            page.lines.iter().any(|l| l.text == r"▼ 曲集\x"),
            "{:?}",
            page.lines
        );
    }

    /// With several roots, Top holds a row per root, and entering one lists
    /// it. A root holding only charts stays a level, though any other such
    /// folder would be a song folder, until it has no rows.
    #[test]
    fn several_roots_are_rows_and_a_root_of_charts_stays_a_level() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(dir.path(), &[&["a.bms", "b.bms"], &["s/song/c.bms"]]);
        assert_eq!(
            list(&sel),
            [
                format!(">{}", all_row(3)),
                folder_row("root0", 2),
                folder_row("root1", 1),
            ]
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(list(&sel), [">a", "b"]);
        std::fs::remove_file(roots[0].join("a.bms")).unwrap();
        std::fs::remove_file(roots[0].join("b.bms")).unwrap();
        settle(&mut sel);
        assert_eq!(
            list(&sel),
            [all_row(1), format!(">{}", folder_row("root1", 1))]
        );
    }

    /// Enter enters a folder with the cursor on its first row; Back returns
    /// to the row left, on its line, by Escape or Backspace; at Top it does
    /// nothing. A chart's row chooses, as it does in the all-chart list, and
    /// the controller's events act as those keys.
    #[test]
    fn a_folder_is_entered_and_returned_from_on_its_row_and_line() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(
            dir.path(),
            &[&[
                "s1/song/a.bms",
                "s2/song/b.bms",
                "s2/song2/c.bms",
                "s3/song/d.bms",
            ]],
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Down]);
        let left = cursor(&sel);
        assert_eq!(left, (folder_row("s2", 2), 2));
        for back in [
            MenuKey::Escape,
            MenuKey::Backspace,
            crate::select::event_key(input::MenuEvent::Back),
        ] {
            let enter = crate::select::event_key(input::MenuEvent::Confirm);
            keys(&mut sel, &[enter]);
            assert_eq!(list(&sel), [">b", "c"]);
            keys(&mut sel, &[MenuKey::Down]);
            assert_eq!(cursor(&sel), ("c".to_owned(), 1));
            keys(&mut sel, &[back]);
            assert_eq!(cursor(&sel), left, "{back:?}");
        }
        keys(&mut sel, &[MenuKey::Enter, MenuKey::Down]);
        let chosen = keys(&mut sel, &[MenuKey::Enter]).expect("a chart is chosen");
        assert_eq!(played(chosen).file_name().unwrap(), "c.bms");
        keys(&mut sel, &[MenuKey::Escape, MenuKey::Home]);
        let before = list(&sel);
        assert!(keys(&mut sel, &[MenuKey::Escape]).is_none());
        assert_eq!(list(&sel), before, "Top stays");
    }

    /// The "all charts" row enters today's list, with its headings, and Back
    /// returns to it.
    #[test]
    fn the_all_charts_row_enters_the_list_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["s1/song/a.bms", "s2/song/b.bms"]]);
        keys(&mut sel, &[MenuKey::Enter]);
        let lines = list(&sel);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[0].starts_with('▼'), "{lines:?}");
        assert_eq!(lines[1], ">a");
        assert_eq!(sel.screen.listed(), 2);
        keys(&mut sel, &[MenuKey::Backspace]);
        assert_eq!(cursor(&sel), (all_row(2), 0));
    }

    /// P does nothing on a folder row or the "all charts" row, and picks a
    /// chart's row.
    #[test]
    fn practice_acts_only_on_a_chart() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["s/song/a.bms", "b.bms"]]);
        assert!(keys(&mut sel, &[MenuKey::Practice]).is_none());
        assert_eq!(cursor(&sel).0, all_row(2));
        assert!(keys(&mut sel, &[MenuKey::Down, MenuKey::Practice]).is_none());
        assert_eq!(cursor(&sel).0, folder_row("s", 1));
        let picked = keys(&mut sel, &[MenuKey::Down, MenuKey::Practice]).expect("practiced");
        assert!(matches!(picked, game::Pick::Practice(_)), "{picked:?}");
    }

    /// A held Backspace never climbs; in the search box it erases, as its
    /// first press does, and the box keeps Escape and Backspace inside a
    /// folder.
    #[test]
    fn the_search_box_keeps_its_keys_and_a_held_backspace_does_not_climb() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["s/song/ab.bms", "s/song/cd.bms", "e.bms"]]);
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(list(&sel), [">ab", "cd"]);
        keys(&mut sel, &[MenuKey::BackspaceHeld]);
        assert_eq!(list(&sel), [">ab", "cd"], "held, it does not climb");
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("abc".to_owned()));
        keys(&mut sel, &[MenuKey::BackspaceHeld]);
        assert_eq!(sel.screen.search().text, "ab");
        keys(&mut sel, &[MenuKey::Backspace]);
        assert_eq!(sel.screen.search().text, "a");
        keys(&mut sel, &[MenuKey::Escape]);
        assert!(!sel.screen.searching(), "Escape left the box");
        assert_eq!(sel.screen.search().text, "a");
        assert_eq!(sel.screen.listed(), 1, "the results stay");
    }

    /// A query lists the whole library whatever the place under it, and when
    /// it empties, by Backspace in the box or Back in results, the place
    /// returns on its row and line.
    #[test]
    fn a_query_covers_the_place_and_returns_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(
            dir.path(),
            &[&["s/x/xa.bms", "s/x/xb.bms", "s/x/xc.bms", "t/u/xz.bms"]],
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter, MenuKey::Down]);
        let place = cursor(&sel);
        assert_eq!(place, ("xb".to_owned(), 1));
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("x".to_owned()));
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(sel.screen.listed(), 4, "the whole library");
        assert!(sel.screen.page("").lines[0].text.contains("検索結果"));
        // Back in results: the query clears, the place stays.
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(sel.screen.search().text, "");
        assert_eq!(cursor(&sel), place);
        // Emptied in the box.
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("x".to_owned()));
        keys(&mut sel, &[MenuKey::Backspace, MenuKey::Enter]);
        assert_eq!(cursor(&sel), place);
        // A query emptied and retyped before the place's list was read keeps
        // the place saved before the first, not the results then shown.
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("x".to_owned()));
        keys(&mut sel, &[MenuKey::Enter, MenuKey::Down, MenuKey::Tab]);
        assert_eq!(cursor(&sel).0, "xc");
        sel.menu.push(MenuInput::Key(MenuKey::Backspace));
        sel.menu.push(MenuInput::Text("x".to_owned()));
        keys(&mut sel, &[MenuKey::Enter, MenuKey::Escape]);
        assert_eq!(cursor(&sel), place);
    }

    /// The rule over a query's end: on `b` of `[a, b, c]`, with `b` removed
    /// and `bb` added while the query showed, the place returns on `c`.
    #[test]
    fn a_querys_end_finds_the_row_by_the_rule() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(
            dir.path(),
            &[&["s/x/a.bms", "s/x/b.bms", "s/x/c.bms", "t/u/z.bms"]],
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter, MenuKey::Down]);
        assert_eq!(cursor(&sel).0, "b");
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("b".to_owned()));
        keys(&mut sel, &[MenuKey::Enter]);
        std::fs::remove_file(roots[0].join("s/x/b.bms")).unwrap();
        write_chart(&roots[0], "s/x/bb.bms");
        settle(&mut sel);
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(cursor(&sel).0, "c");
        assert_eq!(list(&sel), ["a", "bb", ">c"]);
    }

    /// A scan's refresh keeps the cursor on its row and line: charts added
    /// above and below in a folder; its row removed, the nearest after, then
    /// before. The same for a parent restored by a return.
    #[test]
    fn a_refresh_keeps_a_folders_cursor_by_the_rule() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(
            dir.path(),
            &[&["s/x/m.bms", "s/x/n.bms", "s/x/o.bms", "t/u/z.bms"]],
        );
        let root = &roots[0];
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter, MenuKey::Down]);
        assert_eq!(cursor(&sel), ("n".to_owned(), 1));
        write_chart(root, "s/y/a.bms");
        write_chart(root, "s/y/z.bms");
        settle(&mut sel);
        assert_eq!(list(&sel), ["a", "m", ">n", "o", "z"]);
        std::fs::remove_file(root.join("s/x/n.bms")).unwrap();
        settle(&mut sel);
        assert_eq!(list(&sel), ["a", "m", ">o", "z"]);
        std::fs::remove_file(root.join("s/x/o.bms")).unwrap();
        std::fs::remove_file(root.join("s/y/z.bms")).unwrap();
        settle(&mut sel);
        assert_eq!(list(&sel), ["a", ">m"]);
        // The parent changes while it is saved, a folder above it added and
        // one below it removed: the return lands on its row.
        std::fs::remove_dir_all(root.join("t")).unwrap();
        write_chart(root, "r/q/k.bms");
        settle(&mut sel);
        assert_eq!(list(&sel), ["a", ">m"]);
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(
            list(&sel),
            [
                all_row(3),
                folder_row("r", 1),
                format!(">{}", folder_row("s", 2))
            ]
        );
    }

    /// Entering the all-chart list puts the cursor on its first row, even
    /// where Top's charts are listed there in another order: Top's saved
    /// list is only for the return.
    #[test]
    fn the_all_chart_list_is_entered_on_its_first_row() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["a/z.bms", "b/a.bms"]]);
        assert_eq!(
            list(&sel),
            [format!(">{}", all_row(2)), "a".to_owned(), "z".to_owned()]
        );
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(cursor(&sel).0, "z", "{:?}", list(&sel));
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(cursor(&sel).0, all_row(2));
    }

    /// A configured root stays a level however it is reached: through an
    /// overlapping root above it, it holds its own charts only once its
    /// subfolder empties, and stays open.
    #[test]
    fn an_overlapping_root_reached_through_another_stays_a_level() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("root0");
        write_chart(&outer, "A/x.bms");
        write_chart(&outer, "A/B/y.bms");
        write_chart(&outer, "Z/q/w.bms");
        let (mut sel, _) = over(dir.path(), vec![outer.clone(), outer.join("A")]);
        assert_eq!(
            list(&sel),
            [
                format!(">{}", all_row(5)),
                folder_row("root0", 3),
                folder_row("A", 2),
            ]
        );
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(
            list(&sel),
            [format!(">{}", folder_row("A", 2)), folder_row("Z", 1)]
        );
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(list(&sel), [">x", "y"]);
        std::fs::remove_file(outer.join("A/B/y.bms")).unwrap();
        settle(&mut sel);
        // Still in `A`: `root0`'s level would list `Z`'s row and `x`.
        assert_eq!(list(&sel), [">x"]);
    }

    /// A configured root that held nothing when Top was read, and fills while
    /// a scan runs, is a root too: reached through another root it stays a
    /// level when it holds only its own charts.
    #[test]
    fn a_newly_populated_root_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("root0");
        std::fs::create_dir_all(outer.join("A")).unwrap();
        write_chart(&outer, "Z/q/w.bms");
        let (mut sel, _) = over(dir.path(), vec![outer.clone(), outer.join("A")]);
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        write_chart(&outer, "A/x.bms");
        write_chart(&outer, "A/B/y.bms");
        settle(&mut sel);
        keys(&mut sel, &[MenuKey::Home, MenuKey::Enter]);
        assert_eq!(list(&sel), [">x", "y"]);
        std::fs::remove_file(outer.join("A/B/y.bms")).unwrap();
        settle(&mut sel);
        assert_eq!(list(&sel), [">x"]);
    }

    /// A place that stops being a level goes up to its nearest ancestor that
    /// still is: a mixed folder whose subfolder emptied has become a song
    /// folder, its chart now its parent's row. The same when a query covered
    /// it, and when a whole branch emptied.
    #[test]
    fn a_place_no_longer_a_level_goes_up() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(
            dir.path(),
            &[&[
                "a/x.bms",
                "a/b/y.bms",
                "p/q/r/deep.bms",
                "p/q/r2/deep2.bms",
                "p/t/u/w.bms",
            ]],
        );
        let root = &roots[0];
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(list(&sel), [">x", "y"]);
        std::fs::remove_file(root.join("a/b/y.bms")).unwrap();
        settle(&mut sel);
        assert_eq!(cursor(&sel).0, folder_row("p", 3));
        // Under a query: seen when it ends.
        write_chart(root, "a/b/y.bms");
        settle(&mut sel);
        keys(
            &mut sel,
            &[MenuKey::Home, MenuKey::Down, MenuKey::Enter, MenuKey::Tab],
        );
        assert_eq!(sel.screen.listed(), 2);
        sel.menu.push(MenuInput::Text("y".to_owned()));
        keys(&mut sel, &[MenuKey::Enter]);
        std::fs::remove_file(root.join("a/b/y.bms")).unwrap();
        settle(&mut sel);
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(cursor(&sel).0, folder_row("p", 3));
        // Two levels down: a branch emptied climbs to what is left of it.
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(cursor(&sel).0, folder_row("q", 2));
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(list(&sel), [">deep", "deep2"]);
        std::fs::remove_dir_all(root.join("p/q")).unwrap();
        settle(&mut sel);
        // `p` itself is left with `t`, so the climb stops there.
        assert_eq!(list(&sel), [format!(">{}", folder_row("t", 1))]);
    }

    /// A song folder that gains a listed subfolder becomes a folder row at
    /// the next refresh, and the rule moves the cursor off its charts' rows.
    #[test]
    fn a_song_folder_gaining_a_subfolder_becomes_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(dir.path(), &[&["s/a.bms", "z.bms"]]);
        keys(&mut sel, &[MenuKey::Down]);
        assert_eq!(cursor(&sel).0, "a");
        write_chart(&roots[0], "s/t/u/b.bms");
        settle(&mut sel);
        assert_eq!(
            list(&sel),
            [all_row(3), folder_row("s", 2), ">z".to_owned()]
        );
    }

    /// Nothing derives from a title: two folders' charts with equal titles
    /// stay two folders' rows, and an order applies to chart rows, folders
    /// staying by name.
    #[test]
    fn a_folder_is_its_directory_and_an_order_moves_only_charts() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(
            dir.path(),
            &[&["b/x/same.bms", "a/y/same.bms", "n.bms", "m.bms"]],
        );
        std::fs::write(roots[0].join("m.bms"), "#TITLE m\n#ARTIST z\n#00138:01\n").unwrap();
        std::fs::write(roots[0].join("n.bms"), "#TITLE n\n#ARTIST a\n#00138:01\n").unwrap();
        settle(&mut sel);
        let names = |sel: &Selection| -> Vec<String> {
            let page = sel.screen.page("");
            page.lines[4..8]
                .iter()
                .map(|l| l.text.trim().to_owned())
                .collect()
        };
        let by_title = names(&sel);
        assert_eq!(by_title[0], folder_row("a", 1));
        assert_eq!(by_title[1], folder_row("b", 1));
        assert!(by_title[2].contains(" m "), "{by_title:?}");
        keys(&mut sel, &[MenuKey::Right]);
        let by_artist = names(&sel);
        assert_eq!(by_artist[..2], by_title[..2]);
        assert!(by_artist[2].contains(" n "), "{by_artist:?}");
    }

    /// What a skin reads of selection, by the groups of docs/stage3.md's
    /// Selection table, taken from the view alone: a fact the view lacks
    /// fails to compile or reads wrong here.
    #[derive(Debug, PartialEq, Eq)]
    struct Read {
        // Rows, cursor and position.
        rows: Vec<RowRead>,
        cursor: Option<usize>,
        position: Option<skin_model::Ratio>,
        // Folders.
        place: skin_model::SelectionPlace,
        folder_charts: Vec<u32>,
        // Chart and query.
        query: String,
        labels: skin_model::ChartLabels,
        notes: Option<(u32, u32, u32)>,
        bpm: Option<(bms_model::Tempo, bms_model::Tempo, bms_model::Tempo)>,
        length: Option<TimeUs>,
        random: bool,
        // Selected score.
        score: skin_model::RowScore,
        max_ex: Option<u64>,
    }

    /// A row as a skin's list reads it.
    #[derive(Debug, PartialEq, Eq)]
    struct RowRead {
        key: skin_model::RowKey,
        kind: skin_model::RowKind,
        title: String,
        level: String,
        difficulty: Option<u8>,
        score: skin_model::RowScore,
    }

    fn read(view: &skin_model::SelectionView) -> Read {
        use skin_model::RowKind;
        let selected = view.selected.as_ref().expect("a chart is selected");
        let summary = selected.summary;
        Read {
            rows: view
                .rows
                .iter()
                .map(|r| RowRead {
                    key: r.key.clone(),
                    kind: r.kind,
                    title: r.title.clone(),
                    level: r.level.clone(),
                    difficulty: r.difficulty,
                    score: r.score,
                })
                .collect(),
            cursor: view.cursor,
            position: view.position,
            place: view.place.clone(),
            folder_charts: view
                .rows
                .iter()
                .filter_map(|r| match r.kind {
                    RowKind::Folder { charts } | RowKind::All { charts } => Some(charts),
                    RowKind::Chart | RowKind::Heading => None,
                })
                .collect(),
            query: view.query.clone(),
            labels: selected.labels.clone(),
            notes: summary.map(|s| (s.taps, s.longs, s.mines)),
            bpm: summary.map(|s| (s.tempo_start, s.tempo_min, s.tempo_max)),
            length: summary.map(|s| s.length),
            random: selected.random,
            score: selected.score,
            max_ex: selected.max_ex,
        }
    }

    /// The rows that are not charts, the "all charts" row, a folder row and
    /// a heading, carry no level, features or draws, beside a chart row that
    /// has all three.
    #[test]
    fn rows_that_are_not_charts_carry_no_chart_facts() {
        use skin_model::{RowKind, RowView};
        let facts = |r: &RowView| (r.level_value, r.features.is_some(), r.random);
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(dir.path(), &[&["s/x/a.bms", "s/y/b.bms"]]);
        std::fs::write(
            roots[0].join("c.bms"),
            "#TITLE c\n#PLAYLEVEL 9\n#BPM 120\n#00138:01\n\
             #RANDOM 2\n#IF 1\n#00118:01\n#ENDIF\n",
        )
        .unwrap();
        settle(&mut sel);
        // Top: the "all charts" row, the folder s, then c.
        let v = sel.screen.view(0, 10, true);
        let kinds: Vec<_> = v.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::All { charts: 3 },
                RowKind::Folder { charts: 2 },
                RowKind::Chart
            ]
        );
        assert_eq!(facts(&v.rows[0]), (0, false, false));
        assert_eq!(facts(&v.rows[1]), (0, false, false));
        assert_eq!(facts(&v.rows[2]), (9, true, true));
        // The all-chart list: headings among the charts.
        keys(&mut sel, &[MenuKey::Enter]);
        let v = sel.screen.view(10, 10, true);
        let headings: Vec<_> = v
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Heading)
            .collect();
        assert_eq!(headings.len(), 3, "{:?}", v.rows);
        for h in headings {
            assert_eq!(facts(h), (0, false, false));
        }
    }

    /// A nested place names every folder entered from Top's level,
    /// outermost first, and a return drops the last; with several roots the
    /// root's row is the first name.
    #[test]
    fn a_nested_place_names_every_folder_entered() {
        use skin_model::SelectionPlace;
        let place = |sel: &Selection| sel.screen.view(0, 0, false).place;
        let names = |n: &[&str]| SelectionPlace::Folder(n.iter().map(|&s| s.to_owned()).collect());
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["A/B/x/1.bms", "A/B/y/2.bms", "A/z/3.bms"]]);
        settle(&mut sel);
        assert_eq!(place(&sel), SelectionPlace::Top);
        // Top: the "all charts" row, then A.
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(place(&sel), names(&["A"]));
        // A: B first, then z's chart.
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(place(&sel), names(&["A", "B"]));
        keys(&mut sel, &[MenuKey::Escape]);
        assert_eq!(place(&sel), names(&["A"]));

        let dir = tempfile::tempdir().unwrap();
        let (mut sel, _) = tree(dir.path(), &[&["a/x/1.bms", "a/y/2.bms"], &["b/z/3.bms"]]);
        settle(&mut sel);
        // Top: the "all charts" row, then each root's row.
        keys(&mut sel, &[MenuKey::Down, MenuKey::Enter]);
        assert_eq!(place(&sel), names(&["root0"]));
        keys(&mut sel, &[MenuKey::Enter]);
        assert_eq!(place(&sel), names(&["root0", "a"]));
    }

    /// A stand-in consumer reads every needed fact of the Selection table's
    /// groups from a view the player filled from a scanned library: each
    /// row's identity, kind, text, level, difficulty and score, the cursor
    /// and its position, the folder counts, the chart's labels and summary,
    /// the query, and the selected score with the largest EX its rates divide
    /// by.
    #[test]
    fn a_consumer_reads_every_selection_fact_from_the_view() {
        use skin_model::{
            ChartLabels, FolderId, OwnLamp, Ratio, RowKey, RowKind, RowScore, SelectionPlace,
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, roots) = tree(dir.path(), &[&["s/x/a.bms"]]);
        // 150 BPM, then 300 at measure 1; two taps and a mine.
        std::fs::write(
            roots[0].join("c.bms"),
            "#TITLE c\n#SUBTITLE sub\n#ARTIST art\n#SUBARTIST sa\n#GENRE g\n#PLAYLEVEL 7\n\
             #DIFFICULTY 4\n#BPM 150\n#BPM01 300\n\
             #00111:01\n#00118:01\n#001D3:01\n#00108:01\n#00211:01\n",
        )
        .unwrap();
        settle(&mut sel);
        let mut preset = rules::tapestry();
        preset.gauges[0].id = "easy".to_owned();
        let c = sel
            .shelf
            .search(&library::Search {
                text: "c",
                sort: library::Sort::Title,
                by_folder: false,
                limit: 1,
            })
            .unwrap()
            .remove(0);
        sel.screen.set_bests_preset(Some(preset));
        sel.screen.set_bests(std::collections::HashMap::from([(
            c.sha256,
            library::Score {
                plays: 3,
                best: Some(library::Best {
                    ex: 3,
                    bp: 1,
                    cleared: vec![1],
                    full_combo: false,
                }),
            },
        )]));
        keys(&mut sel, &[MenuKey::Down, MenuKey::Down]);
        let bpm =
            |n: u128| bms_model::Tempo::from_units(n * bms_model::Tempo::UNITS_PER_BPM).unwrap();
        let other = |key, kind, title: &str| RowRead {
            key,
            kind,
            title: title.to_owned(),
            level: String::new(),
            difficulty: None,
            score: RowScore::Unplayed,
        };
        let best = RowScore::Best {
            plays: 3,
            ex: 3,
            bp: 1,
            lamp: OwnLamp::Cleared {
                gauge: 0,
                rank: rules::tapestry().gauges[0].lamp,
                look: skin_model::GaugeLook::Easy,
            },
        };
        let chart = |score| RowRead {
            key: RowKey::Chart(roots[0].join("c.bms")),
            kind: RowKind::Chart,
            title: "c".to_owned(),
            level: "7".to_owned(),
            difficulty: Some(4),
            score,
        };
        let mut want = Read {
            rows: vec![
                other(RowKey::All, RowKind::All { charts: 2 }, ""),
                other(
                    RowKey::Folder(FolderId::new(roots[0].join("s"))),
                    RowKind::Folder { charts: 1 },
                    "s",
                ),
                chart(best),
            ],
            cursor: Some(2),
            position: Ratio::new(2, 2),
            place: SelectionPlace::Top,
            folder_charts: vec![2, 1],
            query: String::new(),
            labels: ChartLabels {
                title: "c".to_owned(),
                subtitle: "sub".to_owned(),
                artist: "art".to_owned(),
                subartist: "sa".to_owned(),
                genre: "g".to_owned(),
                level: "7".to_owned(),
                level_value: 7,
                difficulty: Some(4),
            },
            notes: Some((3, 0, 1)),
            bpm: Some((bpm(150), bpm(150), bpm(300))),
            // Measure 1 at 1.6 s, then 0.8 s a measure: measure 2 at 2.4 s.
            length: Some(TimeUs(2_400_000)),
            random: false,
            score: best,
            max_ex: Some(6),
        };
        assert_eq!(read(&sel.screen.view(5, 5, true)), want);
        // A query: its text, and the place is its results.
        keys(&mut sel, &[MenuKey::Tab]);
        sel.menu.push(MenuInput::Text("c".to_owned()));
        keys(&mut sel, &[MenuKey::Enter]);
        let view = sel.screen.view(5, 5, true);
        let got = read(&view);
        // The root's own charts are headed by the root's path; the list read
        // again read the library's bests too, which hold no play.
        want.score = RowScore::Unplayed;
        want.rows = vec![
            other(
                RowKey::Heading(FolderId::new(roots[0].clone())),
                RowKind::Heading,
                &roots[0].display().to_string(),
            ),
            chart(RowScore::Unplayed),
        ];
        want.cursor = Some(1);
        want.position = None;
        want.place = SelectionPlace::Results;
        want.folder_charts = Vec::new();
        want.query = "c".to_owned();
        assert_eq!(got, want);
    }
}
