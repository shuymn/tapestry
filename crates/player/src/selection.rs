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
use std::thread;
use std::time::{Duration, Instant};

use bms_model::{TimeSource, TimeUs};
use game::{AppRig, Chain, LoadRefusal, Loaded};

#[cfg(feature = "dev-tools")]
use crate::controls::Controls;
use crate::end_screen::LoadTimes;
use crate::log_file;
use crate::play::{PlayerRig, since};
use crate::render_side::{RenderLink, RenderMsg, ScreenError};
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
    /// What the window hands the screen.
    pub menu: Arc<MenuQueue>,
    /// The window's keys the queue dropped, as last reported.
    pub dropped: u64,
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
    fn drain(&mut self) -> Option<PathBuf> {
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

    /// Sends the page if it changed, with `status` in its title (the
    /// library's when `None`).
    fn show(&mut self, render: &RenderLink, status: Option<&str>) {
        if self.dirty {
            self.dirty = false;
            let page = self.screen.page(status.unwrap_or(self.shelf.status()));
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
                Some(step @ (SelectStep::Back | SelectStep::Retry)) => {
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
        use crate::dev::SelectStep;
        let Some(script) = &mut self.script else {
            return;
        };
        match script.front() {
            Some(SelectStep::Back) => {
                script.done();
                controls.command(game::Command::Quit);
            }
            Some(SelectStep::Retry) => {
                script.done();
                controls.command(game::Command::Retry);
            }
            Some(step) => {
                let why = format!("{step:?} on a result");
                script.fail(why);
                controls.close();
            }
            None => {}
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
        let mut chosen = sel.drain();
        #[cfg(feature = "dev-tools")]
        if chosen.is_none() && sel.script(self.controls, self.input.wakes()) {
            chosen = sel.drain();
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
                println!("library: the scan paused before the countdown");
            }
            crate::shelf::Paused::Idle => println!("library: no scan before the countdown"),
            crate::shelf::Paused::Cancelled => {
                println!("library: the wait for the scan was cancelled");
            }
        }
    }

    /// A result shows: the scan goes on, and a scripted step for it runs.
    pub(crate) fn result_shown(&mut self) {
        let Some(sel) = &mut self.selection else {
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
            .load(Arc::new(bank), schedule)
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

    fn idle_wait(&mut self, deadline: TimeUs) {
        let left = deadline.0 - self.clock.now().0;
        if left > 0 {
            thread::park_timeout(Duration::from_micros(left.unsigned_abs()));
        }
        self.sync();
    }

    fn show_refusal(&mut self, refusal: &LoadRefusal<ChartLoadError>) {
        let name = file_name(&refusal.chart);
        let why = Chain(&refusal.reason).to_string();
        println!("select: refused {name}: {why}");
        log::warn!("select: refused {}: {why}", refusal.chart.display());
        if let Some(sel) = &mut self.selection {
            sel.screen.select_path(&refusal.chart);
            sel.screen.set_notice(Some(format!("{name}: {why}")));
            sel.dirty = true;
            sel.show(&self.render, None);
        }
    }

    fn entered_selection(&mut self) {
        self.end_lines = None;
        // Nothing plays: the input thread drops its heartbeat, after a
        // completed play, an opening the logic step refused (which closes
        // recording without the end of a play) and at the first entry.
        self.in_play = false;
        self.input.idle();
        let Some(sel) = &mut self.selection else {
            return;
        };
        sel.entered_at = self.clock.now();
        // What was typed during the chart belongs to it.
        while sel.menu.pop().is_some() {}
        sel.screen.end_composition();
        sel.shelf.release();
        if let Some(ime) = &sel.ime {
            ime.set(sel.screen.searching());
        }
        sel.refresh();
        println!("select: {} charts listed", sel.screen.listed());
        sel.show(&self.render, None);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use game::DataRoot;

    use super::Selection;
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
        let shelf = Shelf::new(library, vec![charts.clone()], Duration::from_secs(60));
        let mut sel = Selection {
            shelf,
            screen: SelectScreen::new(vec![charts]),
            menu: Arc::new(MenuQueue::default()),
            dropped: 0,
            ime: None,
            rate: 48_000,
            chosen_at: TimeUs(0),
            entered_at: TimeUs(0),
            dirty: false,
            search_pending: false,
            #[cfg(feature = "dev-tools")]
            script: None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while sel.screen.listed() < 3 {
            assert!(Instant::now() < deadline, "the charts were not listed");
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
        let chosen = sel.drain().expect("Enter chooses");
        assert_eq!(chosen.file_name().unwrap(), "a.bms");
    }

    /// Consecutive edits of the search are searched once, at the end.
    #[test]
    fn consecutive_search_edits_are_searched_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut sel = selection(dir.path());
        sel.menu.push(MenuInput::Key(MenuKey::Tab));
        let _ = sel.drain();
        for c in ["c", "x"] {
            sel.menu.push(MenuInput::Text(c.to_owned()));
        }
        sel.menu.push(MenuInput::Key(MenuKey::Backspace));
        assert!(sel.drain().is_none());
        assert!(!sel.search_pending);
        assert_eq!(sel.screen.listed(), 1);
    }
}
