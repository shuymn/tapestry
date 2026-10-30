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
use std::sync::mpsc::Receiver;
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
use crate::select::{Acted, MenuInput, SelectScreen};
use crate::shelf::{Shelf, file_name};
use crate::window::ImeSwitch;

/// The select screen's side of the rig.
pub struct Selection {
    /// The library and its scan.
    pub shelf: Shelf,
    /// The screen's state.
    pub screen: SelectScreen,
    /// What the window hands the screen.
    pub menu: Receiver<MenuInput>,
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
    /// The scripted input (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub script: Option<crate::dev::SelectScript>,
}

impl Selection {
    /// Searches the library again for the screen's list.
    fn refresh(&mut self) {
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

    /// Acts on one input; the chart it chose, if it did.
    fn act(&mut self, input: MenuInput) -> Option<PathBuf> {
        match self.screen.input(input) {
            Acted::Nothing => None,
            Acted::Changed => {
                self.dirty = true;
                None
            }
            Acted::Search => {
                self.refresh();
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

    /// Sends the page if it changed, with `status` in its title (the
    /// library's when `None`).
    fn show(&mut self, render: &RenderLink, status: Option<&str>) {
        if self.dirty {
            self.dirty = false;
            let page = self.screen.page(status.unwrap_or(self.shelf.status()));
            render.send(RenderMsg::Menu(Box::new(page)));
        }
    }

    /// Runs the scripted steps due on the select screen; the chart chosen,
    /// if one was.
    #[cfg(feature = "dev-tools")]
    fn script(&mut self, controls: &Controls) -> Option<PathBuf> {
        use crate::dev::SelectStep;
        use crate::select::MenuKey;
        let listed = |screen: &SelectScreen, name: &str| {
            screen
                .entries()
                .iter()
                .position(|e| e.path.file_name().is_some_and(|n| n == name))
        };
        loop {
            let mut script = self.script.take()?;
            let chosen = match script.front().cloned() {
                None => {
                    self.script = Some(script);
                    return None;
                }
                Some(SelectStep::Pick(name)) => {
                    let Some(at) = listed(&self.screen, &name) else {
                        if !script.waiting() {
                            controls.close();
                        }
                        self.script = Some(script);
                        return None;
                    };
                    let _ = self.act(MenuInput::Key(MenuKey::Home));
                    for _ in 0..at {
                        let _ = self.act(MenuInput::Key(MenuKey::Down));
                    }
                    script.done();
                    None
                }
                Some(SelectStep::Wait(name)) => {
                    if listed(&self.screen, &name).is_none() {
                        if !script.waiting() {
                            controls.close();
                        }
                        self.script = Some(script);
                        return None;
                    }
                    script.done();
                    None
                }
                Some(SelectStep::Search(text)) => {
                    let _ = self.act(MenuInput::Key(MenuKey::Tab));
                    let _ = self.act(MenuInput::Text(text));
                    let _ = self.act(MenuInput::Key(MenuKey::Enter));
                    script.done();
                    None
                }
                Some(SelectStep::Sleep(ms)) => {
                    if !script.slept(ms) {
                        self.script = Some(script);
                        return None;
                    }
                    script.done();
                    None
                }
                Some(SelectStep::Enter) => {
                    script.done();
                    self.act(MenuInput::Key(MenuKey::Enter))
                }
                Some(SelectStep::Close) => {
                    script.done();
                    controls.close();
                    None
                }
                Some(step @ (SelectStep::Back | SelectStep::Retry)) => {
                    script.fail(format!("{step:?} on the select screen"));
                    controls.close();
                    None
                }
            };
            self.script = Some(script);
            if chosen.is_some() {
                return chosen;
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
        let mut chosen = None;
        while let Ok(input) = sel.menu.try_recv() {
            chosen = sel.act(input);
            if chosen.is_some() {
                break;
            }
        }
        #[cfg(feature = "dev-tools")]
        if chosen.is_none() {
            chosen = sel.script(self.controls);
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
    /// requested if it is not, and acknowledged before recording opens.
    pub(crate) fn pause_scan(&mut self) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        match sel.shelf.wait_paused() {
            crate::shelf::Paused::Acknowledged => {
                println!("library: the scan paused before the countdown");
            }
            crate::shelf::Paused::Idle => println!("library: no scan before the countdown"),
            crate::shelf::Paused::TimedOut => {
                let line = format!(
                    "library: the scan did not pause within {} s; the play starts anyway",
                    crate::shelf::PAUSE_TIMEOUT.as_secs()
                );
                eprintln!("{line}");
                log::warn!("{line}");
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
        let Some(sel) = &mut self.selection else {
            return;
        };
        sel.entered_at = self.clock.now();
        // What was typed during the chart belongs to it.
        while sel.menu.try_recv().is_ok() {}
        sel.shelf.release();
        if let Some(ime) = &sel.ime {
            ime.set(sel.screen.searching());
        }
        sel.refresh();
        println!("select: {} charts listed", sel.screen.listed());
        sel.show(&self.render, None);
    }
}
