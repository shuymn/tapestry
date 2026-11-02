//! Song selection on the real threads (design.md "Play Lifecycle"): the
//! rig's select screen, and the loading of each chosen chart onto the
//! threads that stay up across charts.
//!
//! - **Loading** swaps the audio engine's bank and schedule
//!   ([`game::AudioControl::load`]), the render thread's chart
//!   ([`RenderLink::load`]) and, with autoplay, the input thread's generated
//!   input; a chart refused on the way returns to selection with why.
//! - **The scan** pauses from the choice to each countdown, which waits for
//!   its acknowledgment ([`PlayerRig`]'s audio preparation); every entry
//!   into selection and every result releases it. A chart whose loudness
//!   the library misses is measured during its load, so its first play has
//!   its gain; the measurement is stored once that pause is acknowledged.
//! - **The play screen's loading state** shows from a chart chosen to
//!   play, or a practice Start, to the countdown: the play screen without
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
use crate::loading_text::{ChosenChart, LoadingText, Stage};
use crate::log_file;
use crate::play::{PlayerRig, since};
use crate::render_side::{LoadingSink, RenderLink, RenderMsg, ScreenError, SkinState};
use crate::result_input::{Owner, Pad, ResultInput};
use crate::select::{Acted, MenuInput, MenuKey, MenuQueue, SelectScreen};
#[cfg(feature = "dev-tools")]
use crate::{dev::SelectStep, dev::scenario::InputRef, input_side::Mode};

/// Autoplay's input for a chart as made: the input, its queue's size and
/// the controls it records (dev-tools).
#[cfg(feature = "dev-tools")]
type Feed = (input::AutoplayInput, usize, Vec<eval::ControlId>);
use crate::shelf::{Shelf, file_name};
use crate::window::ImeSwitch;

/// Arrow keys a scripted pick queues at a time: fewer than the queue holds
/// (dev-tools).
#[cfg(feature = "dev-tools")]
const PICK_BATCH: usize = crate::select::QUEUED_KEYS / 2;

/// Backspaces a scripted search queues at a time (dev-tools).
#[cfg(feature = "dev-tools")]
const QUERY_BATCH: usize = crate::select::QUEUED_KEYS / 2;

/// What a chart's load lends its loudness measurement: the 1× chart and
/// bank of a practice at another speed, the load's stage on the play
/// screen, and the
/// window's close or the quit key as its cancel.
struct LoadHost<'a> {
    loaded: &'a Loaded,
    rate: u32,
    controls: &'a crate::controls::Controls,
    render: &'a RenderLink,
    /// The load's lines, which say the chart is being measured.
    page: Option<&'a LoadingText>,
    /// The measurement closes the window (a script's `close-measuring`,
    /// dev-tools).
    #[cfg(feature = "dev-tools")]
    close: bool,
    /// The measurement presses the quit key (a script's `quit-measuring`,
    /// dev-tools).
    #[cfg(feature = "dev-tools")]
    quit: bool,
}

impl crate::loudness::Host for LoadHost<'_> {
    fn at_normal(&mut self) -> anyhow::Result<(bms_model::Chart, Arc<audio::SoundBank>)> {
        let chart = self.loaded.chart_at(bms_model::Speed::NORMAL)?;
        let controls = self.controls;
        let bank = audio::load_bank_reporting(
            &chart,
            self.rate,
            bms_model::Speed::NORMAL,
            &audio::BankLimits::default(),
            &|_, _| {},
            &|| controls.quitting() || controls.leaving(),
        )?;
        Ok((chart, Arc::new(bank)))
    }

    fn measuring(&mut self, _name: &str) {
        show_stage(self.render, self.page, Stage::Loudness);
        #[cfg(feature = "dev-tools")]
        if self.close {
            self.controls.close();
        }
        #[cfg(feature = "dev-tools")]
        if self.quit {
            self.controls.command(game::Command::Quit);
        }
    }

    fn cancelled(&mut self) -> bool {
        self.controls.quitting() || self.controls.leaving()
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
    /// practice Start to its countdown or the way back.
    pub loading: Option<LoadingText>,
    /// Whether the chart chosen last was chosen to practice.
    pub practicing: bool,
    /// When selection was last entered.
    pub entered_at: TimeUs,
    /// The page changed since it was last sent.
    pub dirty: bool,
    /// The search or order changed and is not searched yet.
    pub search_pending: bool,
    /// Steps of the play preset taken and not yet applied: the flow takes
    /// them after each look ([`AppRig::take_preset_step`]).
    pub preset_steps: std::collections::VecDeque<game::PresetStep>,
    /// The scripted input (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub script: Option<crate::dev::SelectScript>,
}

impl Selection {
    /// Searches the library again for the screen's list.
    fn refresh(&mut self) {
        self.search_pending = false;
        match self.shelf.search(&self.screen.search()) {
            Ok(found) => self.screen.set_entries(found),
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
    fn drain(&mut self, pad: &mut Pad) -> Option<Pick> {
        // The controller's events join the window's, as the keys they
        // stand for.
        while let Some(event) = pad.pop() {
            self.menu
                .push(MenuInput::Key(crate::select::event_key(event)));
        }
        pad.report_dropped("select");
        while let Some(input) = self.menu.pop() {
            let edit = matches!(
                input,
                MenuInput::Text(_)
                    | MenuInput::Preedit(_)
                    | MenuInput::Key(MenuKey::Left | MenuKey::Right | MenuKey::Backspace)
            );
            if self.search_pending && !edit {
                self.refresh();
            }
            if let Some(pick) = self.act(input) {
                return Some(pick);
            }
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
        None
    }

    /// [`drain`](Self::drain) while selection owns the menus' queues
    /// (`menus`); otherwise nothing is taken, so input queued for another
    /// screen stays its own.
    fn drain_owned(&mut self, menus: &mut ResultInput) -> Option<Pick> {
        if menus.owner() != Owner::Select {
            return None;
        }
        self.drain(&mut menus.pad)
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
            .set_save_error(error.map(|e| format!("player.toml を保存できません: {e}")));
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
    ) -> Option<Pick> {
        let pick = chosen?;
        let Err(notice) = skin.ready() else {
            return Some(pick);
        };
        let (Pick::Play(path) | Pick::Practice(path)) = &pick;
        println!("select: not admitted {}: {notice}", file_name(path));
        self.refused(path, notice, false);
        self.show(render, None);
        None
    }

    /// Sends the page if it changed, with `status` in its title (the
    /// library's when `None`).
    fn show(&mut self, render: &RenderLink, status: Option<&str>) {
        if self.dirty {
            self.dirty = false;
            let shelf = self.shelf.status();
            let page = self.screen.page(status.unwrap_or(&shelf));
            // Where the cursor shows, for a script's checks (dev-tools).
            #[cfg(feature = "dev-tools")]
            if self.script.is_some()
                && let (Some(line), Some(chart)) =
                    (self.screen.cursor_line(), self.screen.selected())
            {
                println!(
                    "dev: select cursor: line {line}: {}",
                    file_name(&chart.path)
                );
            }
            render.send(RenderMsg::Menu(Box::new(page)));
        }
    }

    /// Runs the scripted steps due on the select screen. A step that types
    /// puts its input on the window's queue, as the window does, and the
    /// steps stop there until the queue is taken: `true` then. A scenario's
    /// captures are armed from `captures`.
    #[cfg(feature = "dev-tools")]
    fn script(
        &mut self,
        controls: &Controls,
        wakes: u64,
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
                    if move_to(&self.screen, &self.menu, at) {
                        return true;
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
                    if move_to(&self.screen, &self.menu, at) {
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
                    if move_to(&self.screen, &self.menu, 0) {
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
                Some(SelectStep::CloseLoading) => {
                    script.slow_load.get_or_insert_default().close = true;
                }
                Some(SelectStep::SlowMeasuring(ms)) => crate::dev::slow_measuring(ms),
                Some(SelectStep::Background(running)) => {
                    let (holds, idle) = self.shelf.background();
                    if (running && !holds) || (!running && !idle) {
                        if script.watchdog_expired() {
                            let what = if running { "held no job" } else { "kept a job" };
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
                Some(
                    step @ (SelectStep::Drawn
                    | SelectStep::Back
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

/// Moves the cursor toward entry `at` with the arrow keys, a batch the
/// queue holds at a time: `true` while it queued keys, `false` once the
/// entry is selected (dev-tools).
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
    /// The run has no select screen (never: only selection loads charts).
    #[error("no song selection")]
    NoSelection,
}

/// Whether the quit key or a window close cancels the load.
fn cancelled(controls: &crate::controls::Controls) -> bool {
    controls.quitting() || controls.leaving()
}

/// Shows the load's lines of `page` at `stage` on the play screen, and
/// says so on the console; nothing without them.
fn show_stage(render: &RenderLink, page: Option<&LoadingText>, stage: Stage) {
    let Some(page) = page else {
        return;
    };
    println!("load: {}", stage.label());
    render
        .loading()
        .show_typed(page.lines(stage), page.update(stage));
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
            .show_typed(page.lines(stage), page.update(stage));
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
        if controls.quitting() || controls.leaving() {
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
        if !acted && held >= total / 2 {
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
    /// Looks at the library and the window's input on the select screen,
    /// and sends the page if it changed; the chart chosen, if one was.
    fn look(&mut self) -> Option<Pick> {
        let sel = self.selection.as_mut()?;
        if sel.shelf.poll(Instant::now()) {
            sel.refresh();
        }
        #[cfg_attr(
            not(feature = "dev-tools"),
            expect(unused_mut, reason = "scripts choose too")
        )]
        let mut chosen = sel.drain_owned(&mut self.result_input);
        #[cfg(feature = "dev-tools")]
        if chosen.is_none()
            && sel.script(
                self.controls,
                self.input.wakes(),
                self.scenario.as_ref().map(|s| &s.captures),
            )
        {
            chosen = sel.drain_owned(&mut self.result_input);
        }
        // Before anything of the choice: no chart without its skin.
        let chosen = sel.admit(chosen, &mut self.skin, &self.render);
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
            // A chart to play enters the play screen's loading state at once
            // (`entered_loading`); the practice page reads it first.
            if sel.practicing {
                sel.show(&self.render, Some(&format!("読み込み中: {name}")));
            }
        } else {
            sel.show(&self.render, None);
        }
        chosen
    }

    /// Before each countdown, with song selection: the scan's pause is
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
            // play: the countdown never starts unacknowledged.
            || controls.quitting() || controls.leaving(),
            crate::shelf::PAUSE_NOTICE,
            || {
                println!("library: waiting for the scan to pause");
                // On the play screen before a chart's first countdown; a
                // retry's shows on the select page, as the scan's state.
                if loading.is_some() {
                    show_stage(render, loading.as_ref(), Stage::Scan);
                } else {
                    let page = screen.page("ライブラリのスキャンの一時停止を待っています");
                    render.send(RenderMsg::Menu(Box::new(page)));
                }
            },
        );
        match paused {
            crate::shelf::Paused::Acknowledged => {
                let line = format!(
                    "library: the scan paused before the countdown in {} ms",
                    began.elapsed().as_millis()
                );
                println!("{line}");
                log::info!("{line}");
            }
            crate::shelf::Paused::Idle => println!("library: no scan before the countdown"),
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
    #[cfg(feature = "dev-tools")]
    pub(crate) fn scenario_pending(&mut self) {
        let (Some(sc), Some(script)) = (
            &mut self.scenario,
            self.selection.as_mut().and_then(|s| s.script.as_mut()),
        ) else {
            return;
        };
        script.announce();
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
            // A practice's round: the one after the last is left at its
            // start by the quit key, which returns to the practice page.
            Some(&SelectStep::Rounds(r)) => {
                sc.until = None;
                sc.captures.bind(self.generation);
                sc.rounds = sc.rounds.saturating_add(1);
                if sc.rounds > r {
                    sc.rounds = 0;
                    script.done();
                    self.controls.command(game::Command::Quit);
                }
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
        let Some(sent) = sc.sent.take() else {
            return;
        };
        if let (Some(why), Some(o)) = (sent.missed(report), script.last_done()) {
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
    /// stage.
    fn entered_loading(&mut self, chart: &std::path::Path) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        let entry = sel.screen.selected().filter(|e| e.path == chart);
        let header = entry.map(|e| &e.header);
        let mut title = header
            .and_then(|h| h.title.clone())
            .unwrap_or_else(|| file_name(chart));
        if let Some(sub) = header.and_then(|h| h.subtitle.as_deref()) {
            title.push(' ');
            title.push_str(sub);
        }
        let page = LoadingText {
            title,
            artist: header.and_then(|h| h.artist.clone()).unwrap_or_default(),
            preset: sel.screen.preset().to_owned(),
            practice: sel.practicing,
            chart: header.map_or_else(ChosenChart::default, |h| ChosenChart {
                title: h.title.clone(),
                subtitle: h.subtitle.clone(),
                artist: h.artist.clone(),
                subartist: h.subartist.clone(),
                genre: h.genre.clone(),
            }),
        };
        println!("load: the play screen loads {}", file_name(chart));
        show_stage(&self.render, Some(&page), Stage::Chart);
        sel.loading = Some(page);
    }

    fn load_chart(&mut self, loaded: &Loaded) -> Result<Loading, ChartLoadError> {
        // A practice's Start chooses again: its load times run from it.
        let now = self.clock.now();
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        if loaded.section().is_some() {
            sel.chosen_at = now;
        }
        // Requested again after the practice page released it, as a
        // choice does.
        sel.shelf.choose(loaded.path());
        let (rate, chosen_at) = (sel.rate, sel.chosen_at);
        let page = sel.loading.clone();
        #[cfg(feature = "dev-tools")]
        let slow = sel.script.as_mut().and_then(|s| s.slow_load.take());
        // The previous chart's restorer never restores this one's rounds.
        self.restorer = None;
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
        let progress = Progress::new(self.render.loading(), page.clone());
        let controls = self.controls;
        // A script's `quit-decoding` presses the quit key once the first
        // file is decoded (dev-tools).
        #[cfg(feature = "dev-tools")]
        let quit_decoding = self
            .selection
            .as_mut()
            .and_then(|s| s.script.as_mut())
            .is_some_and(|s| std::mem::take(&mut s.quit_decoding));
        // The quit key or a close stops the decode at the next file or
        // packet, and no bank is made.
        let decoded_bank = audio::load_bank_reporting(
            chart,
            rate,
            speed,
            &audio::BankLimits::default(),
            &|done, files| {
                progress.decoded(done, files);
                #[cfg(feature = "dev-tools")]
                if quit_decoding && done == 1 {
                    controls.command(game::Command::Quit);
                }
            },
            &|| controls.quitting() || controls.leaving(),
        );
        let bank = match decoded_bank {
            Ok(bank) => Arc::new(bank),
            Err(audio::BankError::Cancelled) => {
                println!("load: cancelled while loading the sounds");
                println!("load: cancelled");
                return Ok(Loading::Cancelled);
            }
            Err(e) => return Err(ChartLoadError::Sounds(e)),
        };
        log_file::warnings("sounds", bank.warnings().iter().map(|w| format!("{w:?}")));
        let decoded = self.clock.now();
        // A quit or a close since the decode's last check: the bank is not
        // installed.
        if cancelled(self.controls) {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        // The engine frees the chart before's bank here, so a measurement's
        // 1× bank is never held beside both.
        self.audio
            .control
            .load(Arc::clone(&bank), audio::BgmSchedule::new(schedule))
            .map_err(ChartLoadError::Audio)?;
        // The rounds restore against the engine this load answered with.
        self.restorer = rounds.map(|s| game::Restorer::new(self.audio.control.seek_source(), s));
        let engine = self.clock.now();
        // Its gain, from the cache, or measured now, before its first play.
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        let name = sel
            .screen
            .selected()
            .map_or_else(|| "the chart".to_owned(), |e| file_name(&e.path));
        #[cfg(feature = "dev-tools")]
        let close = sel
            .script
            .as_mut()
            .is_some_and(|s| std::mem::take(&mut s.close_measuring));
        #[cfg(feature = "dev-tools")]
        let quit = sel
            .script
            .as_mut()
            .is_some_and(|s| std::mem::take(&mut s.quit_measuring));
        let mut host = LoadHost {
            loaded,
            rate,
            controls: self.controls,
            render: &self.render,
            page: page.as_ref(),
            #[cfg(feature = "dev-tools")]
            close,
            #[cfg(feature = "dev-tools")]
            quit,
        };
        if sel
            .shelf
            .loaded(chart, &bank, &name, loaded.path(), &mut host)
            == Loading::Cancelled
        {
            // The quit key leaves the chart's measurement to the
            // background; a close ends the run.
            if !self.controls.quitting() {
                sel.shelf.leave_to_background(crate::background::Job {
                    source: loaded.source(),
                    rate,
                    name,
                });
            }
            // Closing or the quit key: nothing more is prepared, and the
            // flow leaves the chart.
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        let measured = self.clock.now();
        // A cached measurement asks nothing: the screen is not waited for
        // after a quit or a close.
        if cancelled(self.controls) {
            println!("load: cancelled");
            return Ok(Loading::Cancelled);
        }
        show_stage(&self.render, page.as_ref(), Stage::Screen);
        screen.wait(&self.render).map_err(ChartLoadError::Screen)?;
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
            sel.show(&self.render, None);
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
            // The list shows the bests of the chosen preset.
            sel.screen
                .set_bests_preset(shelf.ok().map(|s| s.chosen().clone()));
            sel.refresh_bests();
            sel.show(&self.render, None);
        }
    }

    fn entered_selection(&mut self) {
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
        let Some(sel) = &mut self.selection else {
            return;
        };
        sel.entered(self.clock.now());
        println!("select: {} charts listed", sel.screen.listed());
        sel.show(&self.render, None);
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
        self.render.send(RenderMsg::Menu(Box::new(page)));
        #[cfg(feature = "dev-tools")]
        if let Some(sc) = &mut self.scenario {
            sc.practice = Some(screen.clone());
            sc.shown = true;
        }
    }

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
        self.loading = None;
        self.screen.end_composition();
        self.shelf.release();
        if let Some(ime) = &self.ime {
            ime.set(self.screen.searching());
        }
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use game::DataRoot;

    use super::Selection;
    use crate::result_input::{Pad, ResultInput};
    use crate::select::{MenuInput, MenuKey, MenuQueue, SelectScreen};
    use crate::shelf::{Shelf, open};

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
            let admitted = sel.admit(chosen, skin, &render);
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
            Shelf::new(library, charts, Duration::from_secs(60), None)
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
            let text = format!("#TITLE {title}\n#ARTIST {artist}\n#BPM 120\n#00111:01\n");
            std::fs::write(charts.join(format!("{file}.bms")), text).unwrap();
        }
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        let shelf = shelf(library, vec![charts.clone()]);
        Selection {
            shelf,
            screen: SelectScreen::new(vec![charts]),
            menu: Arc::new(MenuQueue::default()),
            dropped: 0,
            preset_refusal: false,
            ime: None,
            rate: 48_000,
            chosen_at: TimeUs(0),
            loading: None,
            practicing: false,
            entered_at: TimeUs(0),
            dirty: false,
            search_pending: false,
            preset_steps: std::collections::VecDeque::new(),
            #[cfg(feature = "dev-tools")]
            script: None,
        }
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
        assert_eq!(sel.drain_owned(&mut menus), None);
        assert_eq!(menus.take_practice(), Some(game::PracticeInput::Down));
        assert_eq!(menus.take_practice(), Some(game::PracticeInput::Confirm));
        // Selection's own, it chooses.
        menus.take_over(Owner::Select);
        sel.menu.push(MenuInput::Key(MenuKey::Enter));
        assert!(sel.drain_owned(&mut menus).is_some());
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
        assert_eq!(sel.drain_owned(&mut menus), None);
        assert!(sel.preset_steps.is_empty());
        // A black key goes back, which means nothing here.
        press(&mut set, &mut hook, key2, 4_000);
        assert_eq!(sel.drain_owned(&mut menus), None);
        assert!(sel.preset_steps.is_empty());
        // Key 7 steps.
        press(&mut set, &mut hook, key7, 5_000);
        assert_eq!(sel.drain_owned(&mut menus), None);
        assert_eq!(
            Vec::from(std::mem::take(&mut sel.preset_steps)),
            [game::PresetStep::Next]
        );
        press(&mut set, &mut hook, key1, 6_000);
        assert!(sel.drain_owned(&mut menus).is_some());
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
            if sel.script(&controls, 0, None) {
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
        assert!(!sel.script(&controls, 0, None));
        // The step's timeout passes while the pass runs, listing nothing.
        sel.script.as_mut().unwrap().age(crate::dev::STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, None));
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
            if sel.script(&controls, 0, None) {
                assert!(sel.drain(&mut Pad::default()).is_none());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        let chosen = sel.screen.selected().unwrap().path.clone();
        assert_eq!(chosen.file_name().unwrap(), "c.bms");
    }

    /// A pass that never ends fails the pick once the watchdog has run out,
    /// saying so.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_pick_fails_at_the_watchdog_of_a_running_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (mut sel, gate) = gated(dir.path(), "pick=c.bms");
        let controls = crate::controls::Controls::new(input::QpcClock::start());
        assert!(!sel.script(&controls, 0, None));
        sel.script.as_mut().unwrap().age(crate::dev::STEP_WATCHDOG);
        assert!(!sel.script(&controls, 0, None));
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
        assert!(!sel.script(&controls, 0, None));
        assert_eq!(sel.script.as_ref().unwrap().failure(), None);
        sel.script.as_mut().unwrap().age(crate::dev::STEP_TIMEOUT);
        assert!(!sel.script(&controls, 0, None));
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
        for (running, what) in [(true, "held no job"), (false, "kept a job")] {
            sel.script = Some(SelectScript::compiled(vec![(
                SelectStep::Background(running),
                o,
            )]));
            sel.script.as_mut().unwrap().age(STEP_TIMEOUT);
            assert!(!sel.script(&controls, 0, None));
            let script = sel.script.as_mut().unwrap();
            assert_eq!(script.failure(), None);
            assert!(script.front().is_some());
            script.age(STEP_WATCHDOG);
            assert!(!sel.script(&controls, 0, None));
            let failure = sel.script.as_ref().unwrap().failure().unwrap();
            assert!(failure.contains(what), "{failure}");
        }
    }
}
