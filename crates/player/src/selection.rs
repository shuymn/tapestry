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
//!   into selection and every result releases it.
//! - Loading runs on the game thread: the window thread keeps the window
//!   answering meanwhile, and a close is latched and taken once the load
//!   is over (the load's waits are all bounded).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use bms_model::{TimeSource, TimeUs};
use game::{AppRig, Chain, LoadRefusal, Loaded, Refused, ResultScreen, SessionError};

#[cfg(feature = "dev-tools")]
use crate::controls::Controls;
use crate::end_screen::{LoadTimes, preset_error_text, shelf_error_text, view_log};
use crate::log_file;
use crate::play::{PlayerRig, since};
use crate::render_side::{RenderLink, RenderMsg, ScreenError};
use crate::result_input::Pad;
use crate::select::{Acted, MenuInput, MenuKey, MenuQueue, SelectScreen};
use crate::shelf::{Shelf, file_name};
use crate::window::ImeSwitch;

/// Arrow keys a scripted pick queues at a time: fewer than the queue holds
/// (dev-tools).
#[cfg(feature = "dev-tools")]
const PICK_BATCH: usize = crate::select::QUEUED_KEYS / 2;

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
    /// When selection was last entered.
    pub entered_at: TimeUs,
    /// The page changed since it was last sent.
    pub dirty: bool,
    /// The search or order changed and is not searched yet.
    pub search_pending: bool,
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
    fn act(&mut self, input: MenuInput) -> Option<PathBuf> {
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
            Acted::Choose(path) => Some(path),
        }
    }

    /// Takes the queued input; the chart it chose, if it did. Consecutive
    /// edits of the search or the order are searched once: at the end, or
    /// before any other input, which acts on the list they make.
    fn drain(&mut self, pad: &mut Pad) -> Option<PathBuf> {
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
            if let Some(path) = self.act(input) {
                return Some(path);
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

    /// Sends the page if it changed, with `status` in its title (the
    /// library's when `None`).
    fn show(&mut self, render: &RenderLink, status: Option<&str>) {
        if self.dirty {
            self.dirty = false;
            let shelf = self.shelf.status();
            let page = self.screen.page(status.unwrap_or(&shelf));
            render.send(RenderMsg::Menu(Box::new(page)));
        }
    }

    /// Runs the scripted steps due on the select screen. A step that types
    /// puts its input on the window's queue, as the window does, and the
    /// steps stop there until the queue is taken: `true` then.
    #[cfg(feature = "dev-tools")]
    fn script(&mut self, controls: &Controls, wakes: u64) -> bool {
        use crate::dev::SelectStep;
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
            match script.front().cloned() {
                None => return false,
                Some(SelectStep::Pick(name)) => {
                    let Some(at) = listed(&self.screen, &name) else {
                        if !script.waiting() {
                            controls.close();
                        }
                        return false;
                    };
                    // Toward the chart, a batch of keys the queue holds at a
                    // time; the step is done once the chart is selected.
                    let cursor = self.screen.cursor();
                    if cursor != at {
                        let key = if at > cursor {
                            MenuKey::Down
                        } else {
                            MenuKey::Up
                        };
                        let n = at.abs_diff(cursor).min(PICK_BATCH);
                        for _ in 0..n {
                            self.menu.push(MenuInput::Key(key));
                        }
                        return true;
                    }
                }
                Some(SelectStep::Wait(name)) => {
                    if listed(&self.screen, &name).is_none() {
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
                Some(SelectStep::Close) => {
                    script.done();
                    controls.close();
                    return false;
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
    fn script_result(&mut self, controls: &Controls) {
        if let Some(script) = &mut self.script {
            script_result(script, controls, &self.menu);
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

/// Runs the scripted step due on a result: `back` and `retry` as their
/// keys, `next` and `prev` as the right and left arrows on the window's
/// queue `menu`.
#[cfg(feature = "dev-tools")]
pub fn script_result(script: &mut crate::dev::SelectScript, controls: &Controls, menu: &MenuQueue) {
    use crate::dev::SelectStep;
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
        Some(step) => {
            let why = format!("{step:?} on a result");
            script.fail(why);
            controls.close();
        }
        None => {}
    }
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

impl PlayerRig<'_> {
    /// Looks at the library and the window's input on the select screen,
    /// and sends the page if it changed; the chart chosen, if one was.
    fn look(&mut self) -> Option<PathBuf> {
        let sel = self.selection.as_mut()?;
        if sel.shelf.poll(Instant::now()) {
            sel.refresh();
        }
        #[cfg_attr(
            not(feature = "dev-tools"),
            expect(unused_mut, reason = "scripts choose too")
        )]
        let mut chosen = sel.drain(&mut self.result_input.pad);
        #[cfg(feature = "dev-tools")]
        if chosen.is_none() && sel.script(self.controls, self.input.wakes()) {
            chosen = sel.drain(&mut self.result_input.pad);
        }
        if let Some(path) = &chosen {
            sel.shelf.pause();
            sel.chosen_at = self.clock.now();
            let name = file_name(path);
            let after = since(sel.entered_at, sel.chosen_at).as_millis();
            println!("select: chose {name} {after} ms after entering selection");
            log::info!("select: chose {}", path.display());
            sel.dirty = true;
            sel.show(&self.render, Some(&format!("読み込み中: {name}")));
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
        let Selection { shelf, screen, .. } = sel;
        let began = Instant::now();
        let paused = shelf.wait_paused(
            crate::shelf::PAUSE_SLICE,
            // Closing the window or the quit key ends the wait, and the
            // play: the countdown never starts unacknowledged.
            || controls.quitting() || controls.leaving(),
            crate::shelf::PAUSE_NOTICE,
            || {
                println!("library: waiting for the scan to pause");
                let page = screen.page("ライブラリのスキャンの一時停止を待っています");
                render.send(RenderMsg::Menu(Box::new(page)));
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
            }
        }
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
                script_result(script, self.controls, &self.result_input.menu);
            }
            return;
        };
        sel.shelf.release();
        #[cfg(feature = "dev-tools")]
        sel.script_result(self.controls);
    }
}

impl AppRig for PlayerRig<'_> {
    type BankError = ChartLoadError;

    fn choose(&mut self) -> Option<PathBuf> {
        self.look()
    }

    fn load_chart(&mut self, loaded: &Loaded) -> Result<(), ChartLoadError> {
        let sel = self.selection.as_ref().ok_or(ChartLoadError::NoSelection)?;
        let (rate, chosen_at) = (sel.rate, sel.chosen_at);
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
            (chart.bgm.clone(), None)
        };
        let bank = audio::load_bank(chart, rate, &audio::BankLimits::default())
            .map_err(ChartLoadError::Sounds)?;
        log_file::warnings("sounds", bank.warnings().iter().map(|w| format!("{w:?}")));
        let decoded = self.clock.now();
        self.audio
            .control
            .load(Arc::new(bank), audio::BgmSchedule::new(schedule))
            .map_err(ChartLoadError::Audio)?;
        self.render
            .load(Arc::new(chart.clone()))
            .map_err(ChartLoadError::Screen)?;
        let ready = self.clock.now();
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
            "load: the chart is ready {} ms after its choice (chart {} ms, sounds {} ms, audio and screen {} ms)",
            ms(chosen_at, ready),
            ms(chosen_at, read),
            ms(read, decoded),
            ms(decoded, ready),
        );
        Ok(())
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
            sel.refused(&refusal.chart, format!("{name}: {shown}"), preset);
            sel.show(&self.render, None);
        }
    }

    fn presets_changed(&mut self, shelf: Result<&game::Shelf, &game::ShelfError>) {
        let (name, error) = shelf_shown(shelf);
        let error = error.map(|(shown, chain)| {
            let line = format!("presets: {chain}");
            eprintln!("{line}");
            log::warn!("{line}");
            shown
        });
        if let Some(sel) = &mut self.selection {
            sel.presets(name, error, shelf.is_ok());
            // The list shows the bests of the chosen preset.
            sel.screen
                .set_bests_preset(shelf.ok().map(|s| s.chosen().clone()));
            sel.refresh_bests();
            sel.show(&self.render, None);
        }
    }

    fn entered_selection(&mut self) {
        self.end_lines = None;
        self.result_input.showing = false;
        // Nothing plays: the input thread drops its heartbeat, after a
        // completed play, an opening the logic step refused (which closes
        // recording without the end of a play) and at the first entry.
        self.in_play = false;
        self.input.idle();
        let Some(sel) = &mut self.selection else {
            return;
        };
        // What was typed during the chart belongs to it.
        self.result_input.clear();
        sel.entered(self.clock.now());
        println!("select: {} charts listed", sel.screen.listed());
        sel.show(&self.render, None);
    }
}

impl Selection {
    /// Selection is entered at `now`: the composition left from the chart
    /// goes, the scan goes on, and the list and the bests of the chosen
    /// preset are read anew, with what the chart's plays counted.
    fn entered(&mut self, now: TimeUs) {
        self.entered_at = now;
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

    /// Titles A, B and C by artists Z, Y and X, scanned and listed in title
    /// order, A under the cursor.
    fn selection(dir: &std::path::Path) -> Selection {
        let charts = dir.join("charts");
        std::fs::create_dir_all(&charts).unwrap();
        for (file, title, artist) in [("a", "A", "Z"), ("b", "B", "Y"), ("c", "C", "X")] {
            let text = format!("#TITLE {title}\n#ARTIST {artist}\n#BPM 120\n#00111:01\n");
            std::fs::write(charts.join(format!("{file}.bms")), text).unwrap();
        }
        let (library, _) = open(&DataRoot::new(dir.join("data"))).unwrap();
        let shelf = Shelf::new(library, vec![charts.clone()], Duration::from_secs(60), None);
        let mut sel = Selection {
            shelf,
            screen: SelectScreen::new(vec![charts]),
            menu: Arc::new(MenuQueue::default()),
            dropped: 0,
            preset_refusal: false,
            ime: None,
            rate: 48_000,
            chosen_at: TimeUs(0),
            entered_at: TimeUs(0),
            dirty: false,
            search_pending: false,
            #[cfg(feature = "dev-tools")]
            script: None,
        };
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
        assert_eq!(chosen.file_name().unwrap(), "a.bms");
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
        assert_eq!(chosen.file_name().unwrap(), "c.bms");
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
        assert_eq!(chosen.file_name().unwrap(), "c.bms");
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
}
