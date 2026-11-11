//! The select screen's state on the game thread (design.md "Play
//! Lifecycle", "Loading and Song Selection"): the place in the library's
//! folders, or the all-chart list, or a search's results, what it lists, the
//! cursor, the search box and the order, and the page the render thread draws
//! from them ([`MenuPage`]).
//!
//! The keyboard's input comes through winit: the navigation keys move the
//! cursor, Enter enters a folder or chooses a chart, Escape and Backspace go
//! back one place, F11 and F12 step the play preset, which the game
//! thread's flow applies, Tab gives the search box focus, and text typed
//! or committed through the IME goes into the search box only while it
//! has focus. The keyboard's lane keys type nothing into the list. The
//! controller's bound lane controls come from the input thread as menu
//! events ([`menu_bindings`]) and act as the keys they stand for
//! ([`event_key`]). Everything here runs on menus, where allocation is
//! allowed.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use eval::ControlId;
use input::hid::{HidProduct, is_axis};
use input::{MenuAction, MenuBindings, MenuEvent};
use library::{ChartEntry, FolderKey, FolderRow, Level, Score, Search, Sort};
use rules::RulePreset;
use skin_model::{
    ChartLabels, FolderId, MAX_VISIBLE_ROWS, Ratio, RowFeatures, RowKey, RowKind, RowScore,
    RowView, SelectedChart, SelectionLabels, SelectionPlace, SelectionView,
};

use crate::menu_screen::{Caret, LineStyle, MenuLine, MenuPage, PAGE_LINES};
use crate::ui_text::{Label, Message};

/// The most charts one search lists; a search that finds more says so.
pub const LIMIT: usize = 5_000;
/// The list's lines on a page: the rest are the title, the search box, the
/// notice and the keys.
const LIST_LINES: usize = PAGE_LINES - 4;
/// Lines Page Up and Page Down move.
const PAGE_STEP: usize = LIST_LINES - 2;
/// The list's line a moved cursor shows on, where the list allows.
const CURSOR_LINE: usize = LIST_LINES / 3;

/// A key the select screen acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// The previous order.
    Left,
    /// The next order.
    Right,
    Enter,
    Tab,
    /// The first press of Backspace.
    Backspace,
    /// Backspace repeating while held: only the search box reads it, so a
    /// held key never climbs the folders.
    BackspaceHeld,
    Escape,
    /// P: the practice screen of the chart under the cursor.
    Practice,
    /// F11: the previous play preset.
    PrevPreset,
    /// F12: the next play preset.
    NextPreset,
}

/// What the window hands the select screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuInput {
    /// A key press.
    Key(MenuKey),
    /// Text typed, or committed through the IME.
    Text(String),
    /// The IME's composition so far; empty when it ends.
    Preedit(String),
    /// A controller's menu `event`, sent while `panel` was open: the key
    /// it stands for ([`event_key`]), queued with the window's keys.
    Pad {
        /// The event.
        event: MenuEvent,
        /// The panel open as its packet came.
        panel: Option<usize>,
    },
}

/// What a controller's bound lane control does on the select screen
/// (design.md "Input"), the one table of it: the white keys 1, 3 and 5
/// confirm, the white key 7 sends its own event (the next play preset
/// there), the black keys 2, 4 and 6 go back, and the scratch scrolls: an
/// axis one line per press threshold of movement, toward higher values
/// down and lower values up (the other way round when `reversed`), and a
/// scratch of two buttons up for the first bound and down for the second,
/// a line per press.
fn lane_menu(lane: u8, axis: bool, nth_scratch: usize, reversed: bool) -> Option<MenuAction> {
    use input::MenuEvent::{Back, Confirm, Down, Extra, Up};
    Some(match (lane, axis) {
        (0, true) => {
            let (forward, backward) = if reversed { (Up, Down) } else { (Down, Up) };
            MenuAction::Turn { forward, backward }
        }
        (0, false) if nth_scratch == 0 => MenuAction::Press(Up),
        (0, false) if nth_scratch == 1 => MenuAction::Press(Down),
        (1 | 3 | 5, _) => MenuAction::Press(Confirm),
        (7, _) => MenuAction::Press(Extra),
        (2 | 4 | 6, _) => MenuAction::Press(Back),
        _ => return None,
    })
}

/// The controller's menu controls from its key config, or `None` when no
/// controller is configured: each bound lane control as [`lane_menu`]
/// says, of the configured controller, an axis stepping every press
/// threshold of `scratch`'s movement, measured in the revolution play
/// converts it with (`scratch`'s mode and revolution when set), scrolling
/// the way `scratch`'s menu direction says.
pub fn menu_bindings(
    config: &game::KeyConfig,
    device: Option<HidProduct>,
    scratch: &game::ScratchConfig,
) -> Option<MenuBindings> {
    let product = device?;
    let mut scratch_buttons = 0;
    let controls = config
        .lanes
        .iter()
        .filter_map(|k| {
            let control = ControlId(k.control);
            let axis = is_axis(control);
            let nth = scratch_buttons;
            if k.lane == 0 && !axis {
                scratch_buttons += 1;
            }
            lane_menu(k.lane, axis, nth, scratch.menu_reversed).map(|a| (control, a))
        })
        .collect();
    Some(MenuBindings {
        product,
        controls,
        step: scratch.press_threshold,
        relative: scratch.relative,
        wrap: scratch.wrap,
    })
}

/// The select screen's key for a controller's menu event: key 7 chooses
/// the next play preset, as F12 does.
pub const fn event_key(event: MenuEvent) -> MenuKey {
    match event {
        MenuEvent::Confirm => MenuKey::Enter,
        MenuEvent::Extra => MenuKey::NextPreset,
        MenuEvent::Back => MenuKey::Escape,
        MenuEvent::Up => MenuKey::Up,
        MenuEvent::Down => MenuKey::Down,
    }
}

/// Whether window key `key` acts on the select screen while `panel` is
/// open (design.md "Input", Menu input), as the reference's option panels
/// hold the song list (`MusicSelectInputProcessor.java` at 0.8.9): the
/// list's own input runs only with no panel open (`bar.input()`, `:290`),
/// so under any panel nothing moves the list, chooses, practises, opens
/// or leaves a folder or pages (`:115`, `:205`, `:242`; under panel 1 the
/// reference's Up and Down change an option's target, `:163-202`, which
/// tapestry's display-only panels leave out); the order, the search box
/// and the presets act as without a panel (the order is the reference's
/// sort key, taken before the panels, `:90`; the presets are tapestry's
/// own).
#[must_use]
pub const fn acts_under_panel(key: MenuKey, panel: Option<usize>) -> bool {
    match (panel, key) {
        (None, _) => true,
        (
            Some(_),
            MenuKey::Up
            | MenuKey::Down
            | MenuKey::PageUp
            | MenuKey::PageDown
            | MenuKey::Home
            | MenuKey::End
            | MenuKey::Enter
            | MenuKey::Backspace
            | MenuKey::BackspaceHeld
            | MenuKey::Escape
            | MenuKey::Practice,
        ) => false,
        (
            Some(_),
            MenuKey::Left
            | MenuKey::Right
            | MenuKey::Tab
            | MenuKey::PrevPreset
            | MenuKey::NextPreset,
        ) => true,
    }
}

/// The result screen's command for a window key: the arrow keys switch
/// presets, and every other key does nothing there (the retry and quit
/// keys arrive as controls, which the window takes from the same press).
pub const fn result_key(key: MenuKey) -> Option<game::MenuCommand> {
    match key {
        MenuKey::Left | MenuKey::Up => Some(game::MenuCommand::PrevPreset),
        MenuKey::Right | MenuKey::Down => Some(game::MenuCommand::NextPreset),
        _ => None,
    }
}

/// The result screen's command for a controller's menu event: the scratch
/// switches presets, the white keys (7 included) retry and the black keys
/// go back.
pub const fn result_event(event: MenuEvent) -> game::MenuCommand {
    match event {
        MenuEvent::Up => game::MenuCommand::PrevPreset,
        MenuEvent::Down => game::MenuCommand::NextPreset,
        MenuEvent::Confirm | MenuEvent::Extra => game::MenuCommand::Retry,
        MenuEvent::Back => game::MenuCommand::Back,
    }
}

/// Keys the queue holds at most; a key past them is dropped and counted.
pub const QUEUED_KEYS: usize = 64;

/// What the window hands the select screen, until the game thread takes it
/// at its next look. Nothing that carries state is ever dropped: committed
/// text joins the text before it, and an IME composition replaces the one
/// before it, so its last state, the end of a composition included, always
/// arrives. Only keys are bounded ([`QUEUED_KEYS`]); a key past the bound
/// is dropped and counted.
///
/// What the window publishes carries the ingress word it was observed
/// under (design.md "Play Lifecycle", the decision screen), and is
/// validated where it is taken: under the tuning step lock, an input
/// stamped with another generation, or under a closed word, is dropped
/// there. Text and compositions join only those with the same stamp.
#[derive(Debug, Default)]
pub struct MenuQueue {
    inputs: Mutex<VecDeque<(MenuInput, Option<input::Ingress>)>>,
    dropped: AtomicU64,
    /// The tuning whose ingress word validates stamped inputs; none: every
    /// input is taken.
    gate: Option<Arc<crate::tuning::LiveTuning>>,
}

impl MenuQueue {
    /// A queue whose stamped inputs `tuning`'s ingress word validates.
    pub fn gated(tuning: Arc<crate::tuning::LiveTuning>) -> Self {
        Self {
            gate: Some(tuning),
            ..Self::default()
        }
    }

    /// Queues `input`, unstamped: the game thread's own, always taken.
    pub fn push(&self, input: MenuInput) {
        self.push_stamped(input, None);
    }

    /// Queues `input` observed under the ingress word `stamp`.
    pub fn push_stamped(&self, input: MenuInput, stamp: Option<input::Ingress>) {
        let mut q = self.inputs.lock().unwrap_or_else(PoisonError::into_inner);
        let last = q.back_mut().filter(|(_, s)| *s == stamp).map(|(i, _)| i);
        match (input, last) {
            (MenuInput::Text(text), Some(MenuInput::Text(last))) => last.push_str(&text),
            (MenuInput::Preedit(text), Some(MenuInput::Preedit(last))) => *last = text,
            (key @ (MenuInput::Key(_) | MenuInput::Pad { .. }), _) => {
                let keys = q
                    .iter()
                    .filter(|(i, _)| matches!(i, MenuInput::Key(_) | MenuInput::Pad { .. }))
                    .count();
                if keys < QUEUED_KEYS {
                    q.push_back((key, stamp));
                } else {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            (input, _) => q.push_back((input, stamp)),
        }
    }

    /// Takes the oldest input whose stamp is valid now, dropping the
    /// stale ones before it.
    pub fn pop(&self) -> Option<MenuInput> {
        loop {
            let (input, stamp) = self
                .inputs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()?;
            let valid = match (stamp, &self.gate) {
                (Some(stamp), Some(gate)) => gate.with_ingress(|word| word.admits(stamp)),
                _ => true,
            };
            if valid {
                return Some(input);
            }
        }
    }

    /// Takes every input queued, valid or not, and whether one was the
    /// Escape key: what the decision screen reads past its time
    /// (design.md "Play Lifecycle", the practice page).
    pub fn take_escape(&self) -> bool {
        let mut q = self.inputs.lock().unwrap_or_else(PoisonError::into_inner);
        let escape = q
            .iter()
            .any(|(i, _)| matches!(i, MenuInput::Key(MenuKey::Escape)));
        q.clear();
        escape
    }

    /// Keys dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// What an input did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acted {
    /// Nothing changed.
    Nothing,
    /// What the page shows changed.
    Changed,
    /// The place, the search or the order changed: the list is read again.
    Search,
    /// The search box gained or lost focus: the IME is allowed only while
    /// it has it.
    Focus(bool),
    /// The chart under the cursor was chosen.
    Choose(PathBuf),
    /// The chart under the cursor is to be practiced.
    Practice(PathBuf),
    /// The play preset is to step: the game thread's flow, which holds
    /// the presets, takes it.
    Preset(game::PresetStep),
}

/// The orders, in the order Left and Right cycle them, with their labels.
const SORTS: [(Sort, Label); 4] = [
    (Sort::Title, Label::SortTitle),
    (Sort::Artist, Label::SortArtist),
    (Sort::Level, Label::SortLevel),
    (Sort::Modified, Label::SortModified),
];

/// One line of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// The "all charts" row.
    All,
    /// Folder row `.0`.
    Folder(usize),
    /// The heading of the folder of chart `.0`, the first in it.
    Heading(usize),
    /// Chart `.0`.
    Chart(usize),
}

/// A selectable row's identity, which outlives the list it was read in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RowId {
    All,
    Folder(FolderKey),
    Chart(PathBuf),
}

/// A list as the cursor left it: its selectable rows in order, the cursor's
/// index among them and the line it showed on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Saved {
    order: Vec<RowId>,
    at: usize,
    line: usize,
}

impl Saved {
    /// The one rule of restoration: the first row of `order` from `at` onward
    /// that the fresh list holds, else the nearest before `at`, else the
    /// fresh list's first row. A new row is never picked by it.
    fn resolve(&self, fresh: &HashMap<RowId, usize>) -> usize {
        let find = |id: &RowId| fresh.get(id).copied();
        let after = self.order.get(self.at..).unwrap_or_default();
        let before = self.order.get(..self.at).unwrap_or_default();
        after
            .iter()
            .find_map(find)
            .or_else(|| before.iter().rev().find_map(find))
            .unwrap_or(0)
    }
}

/// A place in the library, which a query's results cover without leaving.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Place {
    /// The "all charts" row, then the one root's level or the roots' rows.
    #[default]
    Top,
    /// A folder's level. A configured root stays a level while it has rows,
    /// however it was reached; any other folder also stops being one when it
    /// becomes a song folder.
    Folder { key: FolderKey, name: String },
    /// The all-chart list.
    All,
}

/// Where the cursor goes when the next list arrives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Arrival {
    /// To its row by the rule, over the list it is on.
    #[default]
    Keep,
    /// To the first row: a place just entered. The place left is saved only
    /// for the return to it.
    First,
    /// To this saved list's row by the rule, at its line.
    Restore(Saved),
}

/// How the current list is read: the caller reads it from the library and
/// hands it to the method named.
#[derive(Clone, Debug)]
pub enum ListQuery<'a> {
    /// A search: [`SelectScreen::set_entries`].
    Search(Search<'a>),
    /// Top: [`SelectScreen::set_top`], of the one root's level when `one_root`
    /// and the library has rows of it, else of the rows of the roots.
    Top { one_root: bool, sort: Sort },
    /// A folder's level: [`SelectScreen::set_level`].
    Level { key: FolderKey, sort: Sort },
}

/// The select screen.
#[derive(Debug, Default)]
pub struct SelectScreen {
    /// The place, and those it was entered from, each as the cursor left it.
    place: Place,
    stack: Vec<(Place, Saved)>,
    /// The place as it was when the query became non-empty.
    under_query: Option<Saved>,
    /// Where the cursor goes when the next list arrives.
    arrival: Arrival,
    /// Whether the list starts with the "all charts" row, and its count.
    all_row: Option<u64>,
    /// The folder rows, after it.
    folders: Vec<FolderRow>,
    /// The charts listed, after them; with the folder rows at most [`LIMIT`].
    entries: Vec<ChartEntry>,
    /// Whether there were more rows than [`LIMIT`].
    more: bool,
    /// The list's lines.
    rows: Vec<Row>,
    /// The selectable row under the cursor: the "all charts" row, the folder
    /// rows and the charts, in that order.
    cursor: usize,
    /// The first of `rows` the list shows.
    top: usize,
    /// The search box's text.
    query: String,
    /// The IME's composition, shown after the text.
    preedit: String,
    /// Whether the search box has focus.
    searching: bool,
    /// The order, by index into [`SORTS`].
    sort: usize,
    /// Why the chart last chosen was refused, until the next choice.
    notice: Option<String>,
    /// The chosen preset's name, for the title.
    preset: String,
    /// The chosen preset file's error, or why the shelf cannot open.
    preset_error: Option<String>,
    /// Why `player.toml` could not be written, until it is.
    save_error: Option<String>,
    /// The library roots as configured: for folder names, how many, and which
    /// folders are roots, never song folders.
    roots: Vec<PathBuf>,
    /// The chosen preset, whose bests the list shows.
    bests_preset: Option<RulePreset>,
    /// Its scores, by chart SHA-256.
    bests: HashMap<[u8; 32], Score>,
}

impl SelectScreen {
    /// An empty screen naming folders relative to `roots`.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            ..Self::default()
        }
    }

    /// How the current list is read.
    pub fn list_query(&self) -> ListQuery<'_> {
        let sort = self.sort_key();
        match &self.place {
            _ if !self.query.is_empty() => ListQuery::Search(self.search()),
            Place::All => ListQuery::Search(self.search()),
            Place::Top => ListQuery::Top {
                one_root: self.roots.len() == 1,
                sort,
            },
            Place::Folder { key, .. } => ListQuery::Level {
                key: key.clone(),
                sort,
            },
        }
    }

    /// The search of the all-chart list or of a query's results: one more
    /// than [`LIMIT`], to tell when there are more.
    pub fn search(&self) -> Search<'_> {
        Search {
            text: &self.query,
            sort: self.sort_key(),
            by_folder: true,
            limit: LIMIT + 1,
        }
    }

    fn sort_key(&self) -> Sort {
        SORTS.get(self.sort).map_or(Sort::Title, |&(s, _)| s)
    }

    /// The order the list is in, by its place among those Left and Right
    /// cycle.
    pub const fn order(&self) -> usize {
        self.sort
    }

    /// The order the list is in, as a selection skin shows it.
    pub fn order_view(&self) -> skin_model::SelectionOrder {
        match self.sort_key() {
            Sort::Title => skin_model::SelectionOrder::Title,
            Sort::Artist => skin_model::SelectionOrder::Artist,
            Sort::Level => skin_model::SelectionOrder::Level,
            Sort::Modified => skin_model::SelectionOrder::Modified,
        }
    }

    /// Whether the search box has focus.
    pub const fn searching(&self) -> bool {
        self.searching
    }

    /// The search text (dev-tools: a scenario types toward its own).
    #[cfg(feature = "dev-tools")]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The charts a search or a scan's pass found, in its order, under their
    /// folders' headings. See [`arrive`](Self::arrive) for the cursor.
    pub fn set_entries(&mut self, entries: Vec<ChartEntry>) {
        self.arrive(None, Vec::new(), entries, false, true);
    }

    /// Top's list: the "all charts" row of the library's `charts` listed, then
    /// `level`, which is the one root's or the rows of the roots.
    pub fn set_top(&mut self, level: Level, charts: u64) {
        self.arrive(Some(charts), level.folders, level.charts, level.more, false);
    }

    /// A folder's level, read for [`ListQuery::Level`]: `true` when it is
    /// listed. `false` when that folder is no longer a level: nothing listed
    /// remains under it, or it has become a song folder, its own charts and
    /// nothing listed below them. Selection is then one place up, restored by
    /// that place's saved list, and the list is to be read again.
    pub fn set_level(&mut self, level: Level) -> bool {
        let navigable = match &self.place {
            Place::Folder { key, .. } => {
                let folder = key.path();
                let own = |c: &ChartEntry| c.path.parent() == Some(folder.as_path());
                // A configured root, as its path's components compare: the
                // stored key is that root's one spelling.
                let root = self.roots.contains(&folder);
                let song_folder = !root
                    && level.folders.is_empty()
                    && !level.more
                    && level.charts.iter().all(own);
                !song_folder && !(level.folders.is_empty() && level.charts.is_empty())
            }
            _ => true,
        };
        if !navigable {
            self.go_back_a_place();
            return false;
        }
        self.arrive(None, level.folders, level.charts, level.more, false);
        true
    }

    /// Puts a new list in place. The cursor goes where [`Arrival`] says, by
    /// [`Saved::resolve`] unless the place was just entered, and the list
    /// scrolls so it stays on its line where the list allows.
    fn arrive(
        &mut self,
        all_row: Option<u64>,
        mut folders: Vec<FolderRow>,
        mut entries: Vec<ChartEntry>,
        more: bool,
        headings: bool,
    ) {
        let saved = match std::mem::take(&mut self.arrival) {
            Arrival::Keep => Some(self.saved()),
            Arrival::First => None,
            Arrival::Restore(saved) => Some(saved),
        };
        let rows = folders.len() + entries.len();
        self.more = more || rows > LIMIT;
        folders.truncate(LIMIT);
        entries.truncate(LIMIT - folders.len());
        self.all_row = all_row;
        self.folders = folders;
        self.entries = entries;
        self.rows.clear();
        if self.all_row.is_some() {
            self.rows.push(Row::All);
        }
        self.rows.extend((0..self.folders.len()).map(Row::Folder));
        let mut folder: Option<&Path> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let parent = e.path.parent();
            if headings && folder != parent {
                folder = parent;
                self.rows.push(Row::Heading(i));
            }
            self.rows.push(Row::Chart(i));
        }
        match saved {
            Some(saved) => {
                let fresh = self.row_ids().enumerate().map(|(i, id)| (id, i)).collect();
                self.cursor = saved.resolve(&fresh);
                self.scroll(saved.line);
            }
            None => {
                self.cursor = 0;
                self.scroll(CURSOR_LINE);
            }
        }
    }

    /// How many selectable rows come before the charts.
    fn lead(&self) -> usize {
        usize::from(self.all_row.is_some()) + self.folders.len()
    }

    /// The selectable rows' identities, in order.
    fn row_ids(&self) -> impl Iterator<Item = RowId> + '_ {
        self.all_row
            .iter()
            .map(|_| RowId::All)
            .chain(self.folders.iter().map(|f| RowId::Folder(f.key.clone())))
            .chain(self.entries.iter().map(|e| RowId::Chart(e.path.clone())))
    }

    /// The list as the cursor leaves it.
    fn saved(&self) -> Saved {
        Saved {
            order: self.row_ids().collect(),
            at: self.cursor,
            line: self.cursor_line().unwrap_or(CURSOR_LINE),
        }
    }

    /// Goes one place up, restored by that place's saved list: `false` at Top.
    fn go_back_a_place(&mut self) -> bool {
        let Some((place, saved)) = self.stack.pop() else {
            return false;
        };
        self.place = place;
        self.arrival = Arrival::Restore(saved);
        true
    }

    /// Enters `place` from the current one, the cursor on its first row.
    fn enter(&mut self, place: Place) -> Acted {
        let left = std::mem::replace(&mut self.place, place);
        self.stack.push((left, self.saved()));
        self.arrival = Arrival::First;
        Acted::Search
    }

    /// Opens on the all-chart list, as "all charts" entered from Top does
    /// (dev-tools scripts and the tests that run in that list).
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn show_all(&mut self) {
        self.place = Place::All;
        self.stack.clear();
        self.stack.push((
            Place::Top,
            Saved {
                order: vec![RowId::All],
                ..Saved::default()
            },
        ));
        self.arrival = Arrival::First;
    }

    /// Puts the cursor on the chart at `path`; `false` when it is not
    /// listed. The list stays as it is when the chart is already under the
    /// cursor.
    pub fn select_path(&mut self, path: &Path) -> bool {
        match self.entries.iter().position(|e| e.path == path) {
            Some(i) => {
                let at = self.lead() + i;
                if at != self.cursor {
                    self.cursor = at;
                    self.scroll(CURSOR_LINE);
                }
                true
            }
            None => false,
        }
    }

    /// The list's row of the selectable row under the cursor; none while
    /// nothing is listed.
    fn cursor_row(&self) -> Option<usize> {
        let row = self.row_at_cursor()?;
        self.rows.iter().position(|r| *r == row)
    }

    /// The selectable row under the cursor; none while nothing is listed.
    fn row_at_cursor(&self) -> Option<Row> {
        let all = usize::from(self.all_row.is_some());
        if self.cursor < all {
            Some(Row::All)
        } else if self.cursor < self.lead() {
            Some(Row::Folder(self.cursor - all))
        } else {
            let i = self.cursor - self.lead();
            (i < self.entries.len()).then_some(Row::Chart(i))
        }
    }

    /// The line of the list the cursor shows on; none while nothing is
    /// listed.
    pub fn cursor_line(&self) -> Option<usize> {
        self.cursor_row().map(|at| at.saturating_sub(self.top))
    }

    /// Scrolls the list so the cursor shows on its `line`, or as near it
    /// as the list allows: the list neither starts above its first row
    /// nor ends short of a page while it has more rows.
    fn scroll(&mut self, line: usize) {
        let at = self.cursor_row().unwrap_or(0);
        let last = self.rows.len().saturating_sub(LIST_LINES);
        self.top = at.saturating_sub(line).min(last);
    }

    /// The chart under the cursor; none on another row.
    pub fn selected(&self) -> Option<&ChartEntry> {
        self.cursor
            .checked_sub(self.lead())
            .and_then(|i| self.entries.get(i))
    }

    /// How many charts are under the current place: its folder rows' counts
    /// and its chart rows.
    pub fn listed(&self) -> usize {
        let below: u64 = self.folders.iter().map(|f| f.charts).sum();
        usize::try_from(below).unwrap_or(usize::MAX) + self.entries.len()
    }

    /// The selectable row under the cursor (dev-tools scripts): charts and the
    /// rows before them count together, as [`chart_row`](Self::chart_row) and
    /// [`folder_row`](Self::folder_row) do.
    #[cfg(feature = "dev-tools")]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selectable row of chart `i` of [`entries`](Self::entries)
    /// (dev-tools scripts).
    #[cfg(feature = "dev-tools")]
    pub fn chart_row(&self, i: usize) -> usize {
        self.lead() + i
    }

    /// The selectable row of the folder row of this name (dev-tools scripts).
    #[cfg(feature = "dev-tools")]
    pub fn folder_row(&self, name: &str) -> Option<usize> {
        let at = self.folders.iter().position(|f| f.name == name)?;
        Some(usize::from(self.all_row.is_some()) + at)
    }

    /// What the row under the cursor names (dev-tools scripts): `all`, a
    /// folder's name or a chart's file name.
    #[cfg(feature = "dev-tools")]
    pub fn cursor_name(&self) -> Option<String> {
        Some(match self.row_at_cursor()? {
            Row::All => "all".to_owned(),
            Row::Folder(i) => self.folders.get(i)?.name.clone(),
            Row::Chart(i) | Row::Heading(i) => {
                let path = &self.entries.get(i)?.path;
                path.file_name()?.to_string_lossy().into_owned()
            }
        })
    }

    /// The charts listed, in order (dev-tools scripts).
    #[cfg(feature = "dev-tools")]
    pub fn entries(&self) -> &[ChartEntry] {
        &self.entries
    }

    /// Forgets an IME composition left from before a chart: its end may
    /// have arrived while the chart played, with what it typed.
    pub fn end_composition(&mut self) {
        self.preedit.clear();
    }

    /// Shows why the chosen chart was refused, or nothing.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// Shows the chosen preset in the title and, while no refusal shows,
    /// the error of its file, or why the shelf cannot open.
    pub fn set_preset(&mut self, name: String, error: Option<String>) {
        self.preset = name;
        self.preset_error = error;
    }

    /// The chosen preset's name, as the title shows it.
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Shows why `player.toml` could not be written, while no refusal
    /// shows; `None` once it was.
    pub fn set_save_error(&mut self, error: Option<String>) {
        self.save_error = error;
    }

    /// The preset whose bests the list shows; `None` shows none. Another
    /// preset drops the scores read under the one before, which
    /// [`set_bests`](Self::set_bests) replaces with its own.
    pub fn set_bests_preset(&mut self, preset: Option<RulePreset>) {
        if preset != self.bests_preset {
            self.bests.clear();
        }
        self.bests_preset = preset;
    }

    /// The preset whose bests the list shows.
    pub const fn bests_preset(&self) -> Option<&RulePreset> {
        self.bests_preset.as_ref()
    }

    /// The scores of [`bests_preset`](Self::bests_preset), by chart.
    pub fn set_bests(&mut self, bests: HashMap<[u8; 32], Score>) {
        self.bests = bests;
    }

    /// Acts on one input.
    pub fn input(&mut self, input: MenuInput) -> Acted {
        if let MenuInput::Pad { event, .. } = input {
            return self.input(MenuInput::Key(event_key(event)));
        }
        if self.searching {
            return self.search_input(input);
        }
        let MenuInput::Key(key) = input else {
            // Typing goes to the search box only while it has focus: the
            // lane keys type letters.
            return Acted::Nothing;
        };
        let last = (self.lead() + self.entries.len()).saturating_sub(1);
        let to = match key {
            MenuKey::Up => self.cursor.saturating_sub(1),
            MenuKey::Down => (self.cursor + 1).min(last),
            MenuKey::PageUp => self.cursor.saturating_sub(PAGE_STEP),
            MenuKey::PageDown => (self.cursor + PAGE_STEP).min(last),
            MenuKey::Home => 0,
            MenuKey::End => last,
            MenuKey::Left | MenuKey::Right => {
                let n = SORTS.len();
                self.sort = if key == MenuKey::Right {
                    (self.sort + 1) % n
                } else {
                    (self.sort + n - 1) % n
                };
                return Acted::Search;
            }
            MenuKey::Tab => {
                self.searching = true;
                return Acted::Focus(true);
            }
            MenuKey::Enter | MenuKey::Practice => return self.act_on_row(key),
            MenuKey::PrevPreset => return Acted::Preset(game::PresetStep::Prev),
            MenuKey::NextPreset => return Acted::Preset(game::PresetStep::Next),
            MenuKey::Backspace | MenuKey::Escape => return self.back(),
            MenuKey::BackspaceHeld => return Acted::Nothing,
        };
        if to == self.cursor {
            return Acted::Nothing;
        }
        self.cursor = to;
        self.scroll(CURSOR_LINE);
        Acted::Changed
    }

    /// Enter or P on the row under the cursor: a chart is chosen or practiced;
    /// Enter enters a folder or the all-chart list.
    fn act_on_row(&mut self, key: MenuKey) -> Acted {
        match (self.row_at_cursor(), key) {
            (Some(Row::Chart(i)), _) => {
                let Some(path) = self.entries.get(i).map(|e| e.path.clone()) else {
                    return Acted::Nothing;
                };
                self.notice = None;
                if key == MenuKey::Practice {
                    Acted::Practice(path)
                } else {
                    Acted::Choose(path)
                }
            }
            (Some(Row::All), MenuKey::Enter) => self.enter(Place::All),
            (Some(Row::Folder(i)), MenuKey::Enter) => {
                let Some(row) = self.folders.get(i) else {
                    return Acted::Nothing;
                };
                let place = Place::Folder {
                    key: row.key.clone(),
                    name: row.name.clone(),
                };
                self.enter(place)
            }
            _ => Acted::Nothing,
        }
    }

    /// Back, outside the search box: results clear their query, a place
    /// returns to the one it was entered from, and Top stays.
    fn back(&mut self) -> Acted {
        if !self.query.is_empty() {
            self.query.clear();
            self.return_from_query();
            return Acted::Search;
        }
        if self.go_back_a_place() {
            Acted::Search
        } else {
            Acted::Nothing
        }
    }

    /// The query is empty: the place returns as it was saved.
    fn return_from_query(&mut self) {
        if let Some(saved) = self.under_query.take() {
            self.arrival = Arrival::Restore(saved);
        }
    }

    fn search_input(&mut self, input: MenuInput) -> Acted {
        match input {
            MenuInput::Text(text) => {
                if self.query.is_empty() && !text.is_empty() {
                    // A query retyped before the emptied one's list was read
                    // keeps the place saved then.
                    self.under_query = Some(match std::mem::take(&mut self.arrival) {
                        Arrival::Restore(saved) => saved,
                        Arrival::Keep | Arrival::First => self.saved(),
                    });
                }
                self.query.push_str(&text);
                Acted::Search
            }
            MenuInput::Preedit(text) => {
                self.preedit = text;
                Acted::Changed
            }
            // While the IME composes, its keys are its own.
            MenuInput::Key(_) if !self.preedit.is_empty() => Acted::Nothing,
            MenuInput::Key(MenuKey::Backspace | MenuKey::BackspaceHeld) => {
                if self.query.pop().is_some() {
                    if self.query.is_empty() {
                        self.return_from_query();
                    }
                    Acted::Search
                } else {
                    Acted::Nothing
                }
            }
            MenuInput::Key(MenuKey::Enter | MenuKey::Tab | MenuKey::Escape) => {
                self.searching = false;
                Acted::Focus(false)
            }
            // A controller's event came as its key ([`Self::input`]).
            MenuInput::Key(_) | MenuInput::Pad { .. } => Acted::Nothing,
        }
    }

    /// The page as it stands, with `status` (the library's state) in the
    /// title.
    pub fn page(&self, status: &str) -> MenuPage {
        let line = |text: String, style| MenuLine { text, style };
        let sort = SORTS
            .get(self.sort)
            .map_or(Label::SortTitle, |&(_, label)| label);
        let place = if self.query.is_empty() {
            match &self.place {
                Place::Top => None,
                Place::Folder { name, .. } => Some(name.as_str()),
                Place::All => Some(Label::AllCharts.text()),
            }
        } else {
            Some(Label::SearchResults.text())
        };
        let title = Message::SelectTitle {
            place,
            listed: self.listed(),
            more: self.more,
            sort,
            preset: &self.preset,
            status,
        };
        let mut lines = vec![line(title.text(), LineStyle::Title)];
        let mut caret = None;
        lines.push(if self.searching {
            let text = Message::SearchEditing { query: &self.query }.text();
            let start = text.len();
            caret = Some(Caret {
                line: lines.len(),
                start,
                end: start + self.preedit.len(),
            });
            line(format!("{text}{}_", self.preedit), LineStyle::Search)
        } else {
            line(
                Message::SearchIdle { query: &self.query }.text(),
                LineStyle::Search,
            )
        });
        lines.push(line(
            self.notice
                .clone()
                .or_else(|| self.save_error.clone())
                .or_else(|| self.preset_error.clone())
                .unwrap_or_default(),
            LineStyle::Notice,
        ));
        // The list's page of rows, from the cursor's line to the page's end.
        let view = self
            .cursor_line()
            .map_or_else(SelectionView::default, |at| {
                self.view(at, LIST_LINES.saturating_sub(at + 1), true)
            });
        for (i, row) in view.rows.iter().enumerate() {
            let style = |other| {
                if Some(i) == view.cursor {
                    LineStyle::Cursor
                } else {
                    other
                }
            };
            lines.push(match row.kind {
                RowKind::All { charts } => line(
                    Message::AllChartsRow {
                        charts: charts.into(),
                    }
                    .text(),
                    style(LineStyle::Folder),
                ),
                RowKind::Folder { charts } => line(
                    format!("  ▶ {}  ({charts})", row.title),
                    style(LineStyle::Folder),
                ),
                RowKind::Heading => line(format!("▼ {}", row.title), LineStyle::Folder),
                RowKind::Chart => line(self.chart_line(row), style(LineStyle::Chart)),
            });
        }
        if self.entries.is_empty() && self.folders.is_empty() {
            lines.push(line(
                if self.query.is_empty() {
                    Label::NoCharts
                } else {
                    Label::NotFound
                }
                .text()
                .to_owned(),
                LineStyle::Chart,
            ));
        }
        while lines.len() < PAGE_LINES - 1 {
            lines.push(line(String::new(), LineStyle::Chart));
        }
        lines.push(line(Label::SelectHelp.text().to_owned(), LineStyle::Help));
        MenuPage { lines, caret }
    }

    /// The snapshot of the list: the cursor's row with up to `before` rows
    /// above it and `after` below it that the list has, at most
    /// [`MAX_VISIBLE_ROWS`] together, and what is known of the place, the
    /// query and the chart under the cursor. Without `headings` the list's
    /// heading rows are left out, so `before` and `after` count selectable
    /// rows only: a skin's song list shows no headings, which group the
    /// built-in page's lines.
    pub fn view(&self, before: usize, after: usize, headings: bool) -> SelectionView {
        let lines = || {
            self.rows
                .iter()
                .copied()
                .filter(move |r| headings || !matches!(r, Row::Heading(_)))
        };
        let at = self
            .row_at_cursor()
            .and_then(|row| lines().position(|r| r == row));
        let (start, end) = at.map_or((0, 0), |at| {
            let before = before.min(MAX_VISIBLE_ROWS - 1).min(at);
            let after = after.min(MAX_VISIBLE_ROWS - 1 - before);
            (at - before, at + after + 1)
        });
        let rows = lines()
            .skip(start)
            .take(end - start)
            .map(|row| self.row_view(row))
            .collect();
        let selectable = self.lead() + self.entries.len();
        let position = (selectable > 1)
            .then(|| Ratio::new(self.cursor as u64, selectable as u64 - 1))
            .flatten();
        let place = match &self.place {
            _ if !self.query.is_empty() => SelectionPlace::Results,
            Place::Top => SelectionPlace::Top,
            Place::Folder { .. } => SelectionPlace::Folder(self.breadcrumb()),
            Place::All => SelectionPlace::All,
        };
        SelectionView {
            rows,
            cursor: at.map(|at| at - start),
            position,
            place,
            query: self.query.clone(),
            selected: self.selected().map(|e| self.selected_chart(e)),
        }
    }

    /// The names of the folders entered from Top's level to the current
    /// place, outermost first: with several roots, the root's row first.
    fn breadcrumb(&self) -> Vec<String> {
        self.stack
            .iter()
            .map(|(place, _)| place)
            .chain(std::iter::once(&self.place))
            .filter_map(|place| match place {
                Place::Folder { name, .. } => Some(name.clone()),
                Place::Top | Place::All => None,
            })
            .collect()
    }

    /// A row of the list as the snapshot shows it.
    fn row_view(&self, row: Row) -> RowView {
        let count = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        let blank = |key, kind, title| RowView {
            key,
            kind,
            title,
            subtitle: String::new(),
            artist: String::new(),
            level: String::new(),
            level_value: 0,
            difficulty: None,
            score: RowScore::Unplayed,
            features: None,
            random: false,
        };
        match row {
            Row::All => blank(
                RowKey::All,
                RowKind::All {
                    charts: count(self.all_row.unwrap_or(0)),
                },
                String::new(),
            ),
            Row::Folder(i) => self.folders.get(i).map_or_else(
                || blank(RowKey::All, RowKind::Heading, String::new()),
                |f| {
                    blank(
                        RowKey::Folder(FolderId::new(f.key.path())),
                        RowKind::Folder {
                            charts: count(f.charts),
                        },
                        f.name.clone(),
                    )
                },
            ),
            Row::Heading(i) => {
                let folder = self
                    .entries
                    .get(i)
                    .and_then(|e| e.path.parent())
                    .unwrap_or(Path::new(""));
                blank(
                    RowKey::Heading(FolderId::new(folder.to_owned())),
                    RowKind::Heading,
                    self.folder_name(folder),
                )
            }
            Row::Chart(i) => self.entries.get(i).map_or_else(
                || blank(RowKey::All, RowKind::Heading, String::new()),
                |e| {
                    let h = &e.header;
                    let text = |f: &Option<String>| f.clone().unwrap_or_default();
                    RowView {
                        key: RowKey::Chart(e.path.clone()),
                        kind: RowKind::Chart,
                        title: text(&h.title),
                        subtitle: text(&h.subtitle),
                        artist: text(&h.artist),
                        level: text(&h.playlevel),
                        level_value: level_value(h.playlevel.as_deref()),
                        difficulty: difficulty(h.difficulty.as_deref()),
                        score: self.score_of(e),
                        features: e.summary.as_ref().map(RowFeatures::of),
                        random: e.random,
                    }
                },
            ),
        }
    }

    /// The chart's score under the chosen preset; unplayed without one.
    fn score_of(&self, e: &ChartEntry) -> RowScore {
        self.bests_preset.as_ref().map_or(RowScore::Unplayed, |p| {
            crate::bests::row_score(self.bests.get(&e.sha256), p)
        })
    }

    /// What is known of the chart under the cursor.
    fn selected_chart(&self, e: &ChartEntry) -> SelectedChart {
        let h = &e.header;
        let text = |f: &Option<String>| f.clone().unwrap_or_default();
        SelectedChart {
            labels: ChartLabels {
                title: text(&h.title),
                subtitle: text(&h.subtitle),
                artist: text(&h.artist),
                subartist: text(&h.subartist),
                genre: text(&h.genre),
                level: text(&h.playlevel),
                level_value: level_value(h.playlevel.as_deref()),
                difficulty: difficulty(h.difficulty.as_deref()),
            },
            summary: e.summary,
            random: e.random,
            max_ex: max_ex(e, self.bests_preset.as_ref()),
            score: self.score_of(e),
        }
    }

    /// A heading's text: its folder under its root.
    fn folder_name(&self, folder: &Path) -> String {
        let under = self
            .roots
            .iter()
            .find_map(|r| folder.strip_prefix(r).ok())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(folder);
        under.display().to_string()
    }

    /// A chart row's line: its lamp and best EX under the chosen preset
    /// ([`crate::bests::chart_cell`]), level, title, subtitle and artist, or
    /// its file name when it has no title.
    fn chart_line(&self, row: &RowView) -> String {
        let title = match &row.key {
            RowKey::Chart(path) if row.title.is_empty() => path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            _ => row.title.clone(),
        };
        let cell = crate::bests::chart_cell(&row.score, self.bests_preset.as_ref());
        let level = if row.level.is_empty() {
            "-"
        } else {
            &row.level
        };
        let mut text = format!("    {cell} ☆{level:<3} {title}");
        if !row.subtitle.is_empty() {
            text.push(' ');
            text.push_str(&row.subtitle);
        }
        if !row.artist.is_empty() {
            text.push_str("  / ");
            text.push_str(&row.artist);
        }
        text
    }
}

/// The player's strings a selection skin draws, in the chosen language
/// (docs/stage3.md S10-09d, #176 C10): the results' name holds no query,
/// which a skin reads as data.
pub fn labels() -> SelectionLabels {
    SelectionLabels {
        all_charts: Label::AllCharts.text().to_owned(),
        search_results: Label::SearchResults.text().to_owned(),
    }
}

/// `#PLAYLEVEL` as a number, as a skin draws it: what Java's
/// `Integer.parseInt` reads ([`crate::imports::facts::level_of`]), 0 when
/// it is absent or reads none.
fn level_value(text: Option<&str>) -> i32 {
    text.map_or(0, crate::imports::facts::level_of)
}

/// `#DIFFICULTY` when it is 1 to 5.
fn difficulty(text: Option<&str>) -> Option<u8> {
    text.and_then(|d| d.trim().parse().ok())
        .filter(|d| (1..=5).contains(d))
}

/// The largest EX score a chart's summary allows under `preset`: its judged
/// notes (taps and long notes) at the preset's largest EX weight, at most
/// `u32::MAX` as the evaluator's EX score. `None` for a
/// chart that draws, since its best may come from a draw with another note
/// count, and without a summary or a preset.
fn max_ex(e: &ChartEntry, preset: Option<&RulePreset>) -> Option<u64> {
    if e.random {
        return None;
    }
    let (summary, preset) = (e.summary?, preset?);
    let ex = preset.score.ex;
    let weight = [ex.perfect, ex.great, ex.good, ex.bad, ex.poor]
        .into_iter()
        .max()
        .unwrap_or(0);
    let notes = u64::from(summary.taps) + u64::from(summary.longs);
    // The evaluator's EX score saturates at `u32::MAX`.
    Some((notes * u64::from(weight)).min(u64::from(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use library::{ChartEntry, Header, Sort};

    use super::{Acted, MenuInput, MenuKey, MenuQueue, QUEUED_KEYS, SelectScreen};
    use crate::menu_screen::LineStyle;

    fn entry(path: &str, title: &str) -> ChartEntry {
        ChartEntry {
            path: PathBuf::from(path),
            size: 0,
            mtime_ns: 0,
            sha256: [0; 32],
            md5: [0; 16],
            header: Header {
                title: Some(title.to_owned()),
                ..Header::default()
            },
            random: false,
            bga: false,
            summary: None,
        }
    }

    fn screen() -> SelectScreen {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(vec![
            entry(r"C:\charts\a\1.bms", "あ"),
            entry(r"C:\charts\a\2.bms", "い"),
            entry(r"C:\charts\b\3.bms", "う"),
        ]);
        s
    }

    fn key(k: MenuKey) -> MenuInput {
        MenuInput::Key(k)
    }

    /// The controller's menu table: the white keys 1, 3 and 5 confirm, the
    /// white key 7 sends its own event, which steps to the next preset on
    /// the select screen, the black keys 2, 4 and 6 go back; a scratch axis scrolls down
    /// toward higher values and up toward lower ones, a scratch of two
    /// buttons up with the first bound and down with the second, at the
    /// press threshold; without a configured controller there is no menu.
    /// On a result: the arrows switch presets (left and up back, right and
    /// down on), and Enter, Escape, Backspace, F11, F12 and the rest do
    /// nothing; the scratch switches presets, the white keys (7 included)
    /// retry and the black keys go back.
    #[test]
    fn result_keys_and_controller_events_map_to_menu_commands() {
        use game::MenuCommand::{Back, NextPreset, PrevPreset, Retry};
        use input::MenuEvent;
        for (key, command) in [
            (MenuKey::Left, Some(PrevPreset)),
            (MenuKey::Up, Some(PrevPreset)),
            (MenuKey::Right, Some(NextPreset)),
            (MenuKey::Down, Some(NextPreset)),
            (MenuKey::Enter, None),
            (MenuKey::Escape, None),
            (MenuKey::Backspace, None),
            (MenuKey::Tab, None),
            (MenuKey::PageUp, None),
            (MenuKey::PageDown, None),
            (MenuKey::Home, None),
            (MenuKey::End, None),
            (MenuKey::PrevPreset, None),
            (MenuKey::NextPreset, None),
        ] {
            assert_eq!(super::result_key(key), command, "{key:?}");
        }
        for (event, command) in [
            (MenuEvent::Up, PrevPreset),
            (MenuEvent::Down, NextPreset),
            (MenuEvent::Confirm, Retry),
            (MenuEvent::Extra, Retry),
            (MenuEvent::Back, Back),
        ] {
            assert_eq!(super::result_event(event), command, "{event:?}");
        }
    }

    /// The title names the chosen preset; its file's error shows on the
    /// notice line unless a refusal shows there.
    #[test]
    fn the_title_names_the_chosen_preset_and_its_error() {
        let mut s = screen();
        s.set_preset("mine".to_owned(), Some("mine.toml: broken".to_owned()));
        let page = s.page("");
        assert!(
            page.lines[0].text.contains("プリセット: mine"),
            "{}",
            page.lines[0].text
        );
        assert_eq!(page.lines[2].text, "mine.toml: broken");
        s.set_notice(Some("a.bms: refused".to_owned()));
        assert_eq!(s.page("").lines[2].text, "a.bms: refused");
        s.set_notice(None);
        s.set_preset("mine".to_owned(), None);
        assert_eq!(s.page("").lines[2].text, "");
    }

    #[test]
    fn controller_lanes_map_to_menu_events() {
        use eval::ControlId;
        use game::{KeyConfig, LaneKey};
        use input::hid::{HidProduct, axis_control};
        use input::{MenuAction, MenuEvent};

        use super::{event_key, menu_bindings};

        let product = HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        let x = axis_control(0x01, 0x30).0;
        let lane = |control, lane| LaneKey { control, lane };
        let mut lanes: Vec<LaneKey> = (1..=7).map(|n| lane(u32::from(n) + 10, n)).collect();
        lanes.push(lane(x, 0));
        let config = KeyConfig {
            lanes,
            retry: None,
            quit: None,
            ..KeyConfig::default()
        };
        let scratch = game::ScratchConfig {
            relative: Some(false),
            wrap: Some(256),
            press_threshold: 3,
            ..game::ScratchConfig::default()
        };
        let b = menu_bindings(&config, Some(product), &scratch).unwrap();
        assert_eq!(
            (b.product, b.step, b.relative, b.wrap),
            (product, 3, Some(false), Some(256))
        );
        let scratch = game::ScratchConfig::default();
        let press = |e| MenuAction::Press(e);
        let (c, k) = (MenuEvent::Confirm, MenuEvent::Back);
        let mut want: Vec<(ControlId, MenuAction)> = [c, k, c, k, c, k, MenuEvent::Extra]
            .into_iter()
            .zip(11..)
            .map(|(e, n)| (ControlId(n), press(e)))
            .collect();
        want.push((
            ControlId(x),
            MenuAction::Turn {
                forward: MenuEvent::Down,
                backward: MenuEvent::Up,
            },
        ));
        assert_eq!(b.controls, want);
        // The menu direction reversed: the axis scrolls up toward higher
        // values, and nothing else changes.
        let reversed = game::ScratchConfig {
            menu_reversed: true,
            ..scratch
        };
        if let Some((_, turn)) = want.last_mut() {
            *turn = MenuAction::Turn {
                forward: MenuEvent::Up,
                backward: MenuEvent::Down,
            };
        }
        let b = menu_bindings(&config, Some(product), &reversed).unwrap();
        assert_eq!(b.controls, want);
        // A scratch of two buttons, bound after key 1; a third plays the
        // lane but sends nothing.
        let config = KeyConfig {
            lanes: vec![lane(11, 1), lane(20, 0), lane(21, 0), lane(22, 0)],
            retry: None,
            quit: None,
            ..KeyConfig::default()
        };
        let b = menu_bindings(&config, Some(product), &scratch).unwrap();
        assert_eq!(
            b.controls,
            [
                (ControlId(11), press(c)),
                (ControlId(20), press(MenuEvent::Up)),
                (ControlId(21), press(MenuEvent::Down)),
            ]
        );
        // The menu direction is the axis's alone.
        assert_eq!(
            menu_bindings(&config, Some(product), &reversed)
                .unwrap()
                .controls,
            b.controls
        );
        assert_eq!(menu_bindings(&config, None, &scratch), None);
        assert_eq!(event_key(MenuEvent::Confirm), MenuKey::Enter);
        assert_eq!(event_key(MenuEvent::Back), MenuKey::Escape);
        assert_eq!(event_key(MenuEvent::Up), MenuKey::Up);
        assert_eq!(event_key(MenuEvent::Down), MenuKey::Down);
        assert_eq!(event_key(MenuEvent::Extra), MenuKey::NextPreset);
    }

    /// The navigation keys move the cursor within the list, Enter chooses
    /// the chart under it, and the order keys cycle the four orders.
    #[test]
    fn menu_keys_move_order_and_choose() {
        let mut s = screen();
        assert_eq!(s.input(key(MenuKey::Up)), Acted::Nothing);
        assert_eq!(s.input(key(MenuKey::Down)), Acted::Changed);
        assert_eq!(s.input(key(MenuKey::End)), Acted::Changed);
        assert_eq!(s.input(key(MenuKey::Down)), Acted::Nothing);
        assert_eq!(
            s.input(key(MenuKey::Enter)),
            Acted::Choose(PathBuf::from(r"C:\charts\b\3.bms"))
        );
        assert_eq!(s.input(key(MenuKey::Home)), Acted::Changed);
        assert_eq!(s.search().sort, Sort::Title);
        assert_eq!(s.input(key(MenuKey::Right)), Acted::Search);
        assert_eq!(s.search().sort, Sort::Artist);
        assert_eq!(s.input(key(MenuKey::Left)), Acted::Search);
        assert_eq!(s.input(key(MenuKey::Left)), Acted::Search);
        assert_eq!(s.search().sort, Sort::Modified);
        assert!(s.search().by_folder);
        // At Top, Back has nowhere to go.
        assert_eq!(s.input(key(MenuKey::Backspace)), Acted::Nothing);
        assert_eq!(s.input(key(MenuKey::Escape)), Acted::Nothing);
    }

    /// P picks the chart under the cursor for practice; while the search
    /// box has focus it picks nothing, its letter arriving as text; with
    /// nothing listed it does nothing.
    #[test]
    fn practice_key_picks_the_chart_under_the_cursor() {
        let mut s = screen();
        s.input(key(MenuKey::Down));
        s.set_notice(Some("refused".to_owned()));
        assert_eq!(
            s.input(key(MenuKey::Practice)),
            Acted::Practice(PathBuf::from(r"C:\charts\a\2.bms"))
        );
        assert_eq!(s.page("").lines[2].text, "");
        assert_eq!(s.input(key(MenuKey::Tab)), Acted::Focus(true));
        assert_eq!(s.input(key(MenuKey::Practice)), Acted::Nothing);
        assert_eq!(s.input(MenuInput::Text("p".to_owned())), Acted::Search);
        assert_eq!(s.search().text, "p");
        assert_eq!(super::result_key(MenuKey::Practice), None);
        let mut empty = SelectScreen::new(Vec::new());
        assert_eq!(empty.input(key(MenuKey::Practice)), Acted::Nothing);
    }

    /// Text reaches the search box only while it has focus; the IME's
    /// composition shows until it is committed, and its keys are its own.
    #[test]
    fn text_goes_to_the_search_box_only_while_it_has_focus() {
        let mut s = screen();
        assert_eq!(s.input(MenuInput::Text("z".to_owned())), Acted::Nothing);
        assert_eq!(s.search().text, "");
        assert_eq!(s.input(key(MenuKey::Tab)), Acted::Focus(true));
        assert_eq!(s.input(MenuInput::Text("a".to_owned())), Acted::Search);
        assert_eq!(
            s.input(MenuInput::Preedit("にほ".to_owned())),
            Acted::Changed
        );
        assert_eq!(s.input(key(MenuKey::Enter)), Acted::Nothing);
        assert!(s.page("").lines[1].text.contains("aにほ"));
        assert_eq!(s.input(MenuInput::Preedit(String::new())), Acted::Changed);
        assert_eq!(s.input(MenuInput::Text("日本".to_owned())), Acted::Search);
        assert_eq!(s.search().text, "a日本");
        assert_eq!(s.input(key(MenuKey::Backspace)), Acted::Search);
        assert_eq!(s.search().text, "a日");
        // Enter leaves the box; it chooses nothing.
        assert_eq!(s.input(key(MenuKey::Enter)), Acted::Focus(false));
        assert!(!s.searching());
        assert_eq!(s.input(MenuInput::Text("x".to_owned())), Acted::Nothing);
    }

    /// A full queue drops and counts keys only: committed text and the
    /// IME's last composition, its end included, always arrive, joined or
    /// replaced rather than queued one by one.
    #[test]
    fn the_menu_queue_keeps_text_and_the_end_of_a_composition() {
        let q = MenuQueue::default();
        for _ in 0..QUEUED_KEYS + 3 {
            q.push(key(MenuKey::Down));
        }
        q.push(MenuInput::Preedit("に".to_owned()));
        q.push(MenuInput::Preedit("にほ".to_owned()));
        q.push(MenuInput::Preedit(String::new()));
        q.push(MenuInput::Text("日".to_owned()));
        q.push(MenuInput::Text("本".to_owned()));
        q.push(key(MenuKey::Escape));
        assert_eq!(q.dropped(), 4);
        let all: Vec<MenuInput> = std::iter::from_fn(|| q.pop()).collect();
        assert_eq!(all.len(), QUEUED_KEYS + 2);
        assert_eq!(
            all[QUEUED_KEYS..],
            [
                MenuInput::Preedit(String::new()),
                MenuInput::Text("日本".to_owned())
            ]
        );
    }

    /// The list is grouped under folder headings, the cursor's chart is
    /// marked, and a new search keeps the cursor on its chart.
    #[test]
    fn the_page_groups_by_folder_and_keeps_the_cursor() {
        let mut s = screen();
        s.input(key(MenuKey::Down));
        let page = s.page("");
        let list: Vec<(&str, LineStyle)> = page.lines[3..7]
            .iter()
            .map(|l| (l.text.as_str(), l.style))
            .collect();
        assert_eq!(list[0], ("▼ a", LineStyle::Folder));
        assert_eq!(list[1].1, LineStyle::Chart);
        assert!(list[2].0.ends_with("い") && list[2].1 == LineStyle::Cursor);
        assert_eq!(list[3], ("▼ b", LineStyle::Folder));
        s.set_entries(vec![
            entry(r"C:\charts\a\0.bms", "ん"),
            entry(r"C:\charts\a\2.bms", "い"),
        ]);
        assert_eq!(s.selected().unwrap().path, Path::new(r"C:\charts\a\2.bms"));
        // Its chart gone, the cursor stays within the list.
        s.set_entries(vec![entry(r"C:\charts\a\0.bms", "ん")]);
        assert_eq!(s.selected().unwrap().path, Path::new(r"C:\charts\a\0.bms"));
        assert!(s.select_path(Path::new(r"C:\charts\a\0.bms")));
        assert!(!s.select_path(Path::new(r"C:\charts\a\9.bms")));
    }

    /// `n` charts in `folder`, in the order a search lists them.
    fn folder(folder: &str, n: usize) -> Vec<ChartEntry> {
        (0..n)
            .map(|i| {
                let name = format!("{folder}{i:02}");
                entry(&format!(r"C:\charts\{folder}\{name}.bms"), &name)
            })
            .collect()
    }

    /// The listing of `folders`' charts, each `(folder, n)`, without the
    /// charts titled in `gone`.
    fn listing(folders: &[(&str, usize)], gone: &[&str]) -> Vec<ChartEntry> {
        folders
            .iter()
            .flat_map(|&(f, n)| folder(f, n))
            .filter(|e| !gone.contains(&e.header.title.as_deref().unwrap()))
            .collect()
    }

    /// The chart under the cursor and the list's line it is drawn on.
    fn drawn(s: &SelectScreen) -> (String, usize) {
        let page = s.page("");
        let at = page
            .lines
            .iter()
            .position(|l| l.style == LineStyle::Cursor)
            .unwrap();
        let title = s.selected().unwrap().header.title.clone().unwrap();
        assert!(
            page.lines[at].text.ends_with(&title),
            "{:?}",
            page.lines[at]
        );
        // The title, the search box and the notice come first.
        (title, at - 3)
    }

    /// Charts listed above the cursor keep it on its chart and its line:
    /// the list scrolls down by what was inserted.
    #[test]
    fn charts_listed_above_keep_the_cursor_on_its_chart_and_line() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(listing(&[("m", 30)], &[]));
        assert_eq!(drawn(&s), ("m00".to_owned(), 1));
        s.input(key(MenuKey::Down));
        assert_eq!(drawn(&s), ("m01".to_owned(), 2));
        s.set_entries(listing(&[("a", 5), ("m", 30)], &[]));
        assert_eq!(drawn(&s), ("m01".to_owned(), 2));
        s.set_entries(listing(&[("a", 9), ("b", 1), ("m", 30)], &[]));
        assert_eq!(drawn(&s), ("m01".to_owned(), 2));
    }

    /// Charts listed below the cursor keep it on its line, even where the
    /// list's end held it there.
    #[test]
    fn charts_listed_below_keep_the_cursor_on_its_chart_and_line() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(listing(&[("m", 30)], &[]));
        s.input(key(MenuKey::End));
        assert_eq!(drawn(&s), ("m29".to_owned(), 19));
        s.set_entries(listing(&[("m", 30), ("z", 10)], &[]));
        assert_eq!(drawn(&s), ("m29".to_owned(), 19));
        s.input(key(MenuKey::Home));
        s.set_entries(listing(&[("m", 30), ("z", 20)], &[]));
        assert_eq!(drawn(&s), ("m00".to_owned(), 1));
    }

    /// Charts listed above and below at once keep the cursor on its chart
    /// and line.
    #[test]
    fn charts_listed_above_and_below_keep_the_cursor_on_its_chart_and_line() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(listing(&[("m", 30)], &[]));
        s.input(key(MenuKey::End));
        s.set_entries(listing(&[("a", 4), ("m", 30), ("z", 3)], &[]));
        assert_eq!(drawn(&s), ("m29".to_owned(), 19));
        s.input(key(MenuKey::Home));
        for _ in 0..12 {
            s.input(key(MenuKey::Down));
        }
        let at = drawn(&s);
        s.set_entries(listing(&[("a", 6), ("b", 2), ("m", 30), ("z", 9)], &[]));
        assert_eq!(drawn(&s), at);
    }

    /// When the chart under the cursor is no longer listed, the cursor
    /// moves to the nearest chart after it that still is, else the nearest
    /// before it, on the same line.
    #[test]
    fn a_gone_chart_moves_the_cursor_to_its_nearest_neighbour() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(listing(&[("m", 30)], &[]));
        for _ in 0..5 {
            s.input(key(MenuKey::Down));
        }
        assert_eq!(drawn(&s), ("m05".to_owned(), 6));
        s.set_entries(listing(&[("a", 3), ("m", 30)], &["m05", "m06"]));
        assert_eq!(drawn(&s), ("m07".to_owned(), 6));
        s.input(key(MenuKey::End));
        let (_, line) = drawn(&s);
        s.set_entries(listing(
            &[("a", 5), ("m", 30)],
            &["m05", "m06", "m28", "m29"],
        ));
        assert_eq!(drawn(&s), ("m27".to_owned(), line));
    }

    /// An empty list that gains charts puts the cursor on the first.
    #[test]
    fn an_empty_list_gaining_charts_selects_the_first() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(Vec::new());
        assert!(s.selected().is_none());
        s.set_entries(listing(&[("m", 30)], &[]));
        assert_eq!(drawn(&s), ("m00".to_owned(), 1));
    }

    /// The rule of restoration over rows of any kind: with `[a, b, c]` saved on
    /// `b`, `b` gone and `bb` new, `c`; a new row is never picked; before the
    /// saved row when nothing after it remains; else the first.
    #[test]
    fn a_saved_list_finds_its_row_by_the_one_rule() {
        use std::collections::HashMap;

        use super::{RowId, Saved};
        let chart = |n: &str| RowId::Chart(PathBuf::from(n));
        let saved = Saved {
            order: vec![RowId::All, chart("a"), chart("b"), chart("c")],
            at: 2,
            line: 0,
        };
        let fresh = |ids: Vec<RowId>| -> HashMap<RowId, usize> {
            ids.into_iter().enumerate().map(|(i, id)| (id, i)).collect()
        };
        for (ids, at) in [
            (vec![RowId::All, chart("a"), chart("b"), chart("c")], 2),
            (vec![RowId::All, chart("a"), chart("bb"), chart("c")], 3),
            (vec![RowId::All, chart("a"), chart("bb")], 1),
            (vec![chart("z"), RowId::All], 1),
            (vec![chart("z")], 0),
            (Vec::new(), 0),
        ] {
            assert_eq!(saved.resolve(&fresh(ids.clone())), at, "{ids:?}");
        }
    }

    /// Past [`LIMIT`](super::LIMIT) rows a level lists the first and says so,
    /// its count naming what is under it.
    #[test]
    fn a_top_past_the_limit_says_so() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        let charts = (0..=super::LIMIT)
            .map(|i| entry(&format!(r"C:\charts\{i}.bms"), "t"))
            .collect();
        s.set_top(
            library::Level {
                charts,
                ..library::Level::default()
            },
            5_001,
        );
        assert_eq!(s.listed(), super::LIMIT);
        assert_eq!(
            s.page("").lines[0].text,
            "曲選択  5000以上 譜面  並び順: タイトル  プリセット:   "
        );
    }

    /// The page's fixed wording in full: the title with each order and
    /// the library's status, the idle and the editing search line with a
    /// non-ASCII query, the results' title, both empty lists and the keys.
    #[test]
    fn page_wording() {
        let mut s = SelectScreen::new(Vec::new());
        s.set_preset("自作".to_owned(), None);
        let page = s.page("スキャン中");
        assert_eq!(
            page.lines[0].text,
            "曲選択  0 譜面  並び順: タイトル  プリセット: 自作  スキャン中"
        );
        assert_eq!(page.lines[1].text, "検索 (Tab): ");
        assert_eq!(
            page.lines[3].text,
            "譜面がありません (device.toml の [library] roots)"
        );
        assert_eq!(
            page.lines[super::PAGE_LINES - 1].text,
            "↑↓ 選択  Enter 決定  Esc 戻る  P 練習  Tab 検索  ←→ 並び順  F11/F12 プリセット  F1-F8 設定  ウィンドウを閉じて終了"
        );
        for order in ["アーティスト", "レベル", "更新日", "タイトル"] {
            s.input(key(MenuKey::Right));
            assert_eq!(
                s.page("").lines[0].text,
                format!("曲選択  0 譜面  並び順: {order}  プリセット: 自作  ")
            );
        }
        s.input(key(MenuKey::Tab));
        s.input(MenuInput::Text("日本".to_owned()));
        let page = s.page("");
        assert_eq!(page.lines[1].text, "検索: 日本_");
        assert_eq!(
            page.lines[0].text,
            "曲選択  検索結果  0 譜面  並び順: タイトル  プリセット: 自作  "
        );
        assert_eq!(page.lines[3].text, "見つかりません");
        s.input(key(MenuKey::Enter));
        assert_eq!(s.page("").lines[1].text, "検索 (Tab): 日本");
    }

    /// Each chart line starts with the chart's lamp and best EX under the
    /// chosen preset: blank for a chart never played under it, `-` for one
    /// whose plays set no best.
    #[test]
    fn chart_line_shows_lamp_and_ex_of_the_chosen_preset() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        let mut scored = entry(r"C:\charts\a\1.bms", "あ");
        scored.sha256 = [1; 32];
        scored.header.playlevel = Some("12".to_owned());
        let mut no_best = entry(r"C:\charts\a\2.bms", "い");
        no_best.sha256 = [2; 32];
        let unplayed = entry(r"C:\charts\a\3.bms", "う");
        s.set_entries(vec![scored, no_best, unplayed]);
        let preset = rules::tapestry();
        s.set_bests_preset(Some(preset.clone()));
        let hard = preset
            .gauges
            .iter()
            .position(|g| g.id == "survival")
            .unwrap();
        s.set_bests(std::collections::HashMap::from([
            (
                [1; 32],
                library::Score {
                    plays: 3,
                    best: Some(library::Best {
                        ex: 1234,
                        bp: 5,
                        cleared: vec![1 << hard],
                        full_combo: false,
                    }),
                },
            ),
            (
                [2; 32],
                library::Score {
                    plays: 2,
                    best: None,
                },
            ),
        ]));
        let page = s.page("");
        let lines: Vec<&str> = page.lines[4..7].iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            lines,
            [
                "    SURVIVAL     1234 ☆12  あ",
                "    -               - ☆-   い",
                "                      ☆-   う",
            ]
        );
    }

    fn view_keys(view: &skin_model::SelectionView) -> Vec<String> {
        use skin_model::RowKey;
        view.rows
            .iter()
            .map(|r| match &r.key {
                RowKey::All => "all".to_owned(),
                RowKey::Folder(f) => format!("folder {}", f.path().display()),
                RowKey::Heading(f) => format!("heading {}", f.path().display()),
                RowKey::Chart(p) => p.file_name().unwrap().to_string_lossy().into_owned(),
            })
            .collect()
    }

    /// The window holds at most `MAX_VISIBLE_ROWS` rows with the cursor's
    /// inside, as many above and below as asked and the list has, headings
    /// included; the position is exact over the selectable rows, none under
    /// two.
    #[test]
    fn the_view_is_a_bounded_window_around_the_cursor() {
        use skin_model::{MAX_VISIBLE_ROWS, Ratio, RowKey, RowKind};
        let empty = SelectScreen::new(Vec::new()).view(10, 10, true);
        assert_eq!(
            (empty.rows.len(), empty.cursor, empty.position),
            (0, None, None)
        );
        let mut one = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        one.set_entries(folder("a", 1));
        let v = one.view(10, 10, true);
        assert_eq!(view_keys(&v), [r"heading C:\charts\a", "a00.bms"]);
        assert_eq!(v.rows[0].kind, RowKind::Heading);
        assert_eq!(v.rows[0].title, "a");
        assert_eq!((v.cursor, v.position), (Some(1), None));
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        s.set_entries(names.iter().flat_map(|f| folder(f, 10)).collect());
        // At the start: the first heading, then the cursor's chart.
        let v = s.view(100, 100, true);
        assert_eq!(v.rows.len(), MAX_VISIBLE_ROWS);
        assert_eq!(v.cursor, Some(1));
        assert_eq!(v.position, Ratio::new(0, 99));
        // In the middle, as asked.
        for _ in 0..45 {
            s.input(key(MenuKey::Down));
        }
        let v = s.view(2, 3, true);
        assert_eq!(
            view_keys(&v),
            [
                "e03.bms", "e04.bms", "e05.bms", "e06.bms", "e07.bms", "e08.bms"
            ]
        );
        assert_eq!((v.cursor, v.position), (Some(2), Ratio::new(45, 99)));
        // At the end.
        s.input(key(MenuKey::End));
        let v = s.view(100, 100, true);
        assert_eq!(v.rows.len(), MAX_VISIBLE_ROWS);
        assert_eq!(v.cursor, Some(MAX_VISIBLE_ROWS - 1));
        assert_eq!(
            v.rows[MAX_VISIBLE_ROWS - 1].key,
            RowKey::Chart(PathBuf::from(r"C:\charts\j\j09.bms"))
        );
        assert_eq!(v.position, Ratio::new(99, 99));
    }

    /// The page drawn from the view is the page the list draws: headings, the
    /// cursor's row, and each chart's lamp, best EX, level, title, subtitle
    /// and artist under the chosen preset. A field present but empty shows as
    /// an absent one: `-` for the level, no subtitle or artist, and the file
    /// name for the title.
    #[test]
    fn the_page_is_drawn_from_the_view() {
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        let mut a = entry(r"C:\charts\x\1.bms", "あ");
        a.sha256 = [1; 32];
        a.header.playlevel = Some("12".to_owned());
        a.header.subtitle = Some("[A]".to_owned());
        a.header.artist = Some("art".to_owned());
        let mut b = entry(r"C:\charts\x\2.bms", "い");
        b.sha256 = [2; 32];
        let mut c = entry(r"C:\charts\y\3.bms", "う");
        c.sha256 = [3; 32];
        c.header.title = None;
        // Present but empty: no level, no subtitle, no artist, the file name.
        let mut d = entry(r"C:\charts\y\4.bms", "");
        d.header.playlevel = Some(String::new());
        d.header.subtitle = Some(String::new());
        d.header.artist = Some(String::new());
        s.set_entries(vec![a, b, c, d]);
        let mut preset = rules::tapestry();
        preset.gauges[0].id = "easy".to_owned();
        let easy = 0;
        let best = |ex, bp, cleared: Vec<u8>, full_combo| library::Score {
            plays: 4,
            best: Some(library::Best {
                ex,
                bp,
                cleared,
                full_combo,
            }),
        };
        s.set_bests_preset(Some(preset));
        s.set_bests(std::collections::HashMap::from([
            ([1; 32], best(1234, 5, vec![1 << easy], false)),
            ([2; 32], best(99, 0, Vec::new(), true)),
        ]));
        s.input(key(MenuKey::Down));
        let page = s.page("");
        let lines: Vec<(&str, LineStyle)> = page.lines[3..9]
            .iter()
            .map(|l| (l.text.as_str(), l.style))
            .collect();
        assert_eq!(
            lines,
            [
                ("▼ x", LineStyle::Folder),
                ("    EASY         1234 ☆12  あ [A]  / art", LineStyle::Chart),
                ("    FC             99 ☆-   い", LineStyle::Cursor),
                ("▼ y", LineStyle::Folder),
                ("                      ☆-   3.bms", LineStyle::Chart),
                ("                      ☆-   4.bms", LineStyle::Chart),
            ]
        );
    }

    /// A preset switch drops the scores read under the one before, so none
    /// shows under the new preset until its own are read: a read that then
    /// fails leaves the list unplayed, never the old preset's lamps.
    #[test]
    fn a_preset_switch_drops_the_old_presets_scores() {
        use skin_model::RowScore;
        let mut s = SelectScreen::new(Vec::new());
        let mut e = entry(r"C:\charts\x\1.bms", "a");
        e.sha256 = [1; 32];
        s.set_entries(vec![e]);
        s.set_bests_preset(Some(rules::tapestry()));
        s.set_bests(std::collections::HashMap::from([(
            [1; 32],
            library::Score {
                plays: 2,
                best: None,
            },
        )]));
        assert_eq!(
            s.view(1, 1, true).rows[1].score,
            RowScore::NoBest { plays: 2 }
        );
        s.set_bests_preset(Some(rules::lr2_style()));
        assert_eq!(s.view(1, 1, true).rows[1].score, RowScore::Unplayed);
    }

    /// A row's difficulty is `#DIFFICULTY` when it is 1 to 5, spaces aside.
    #[test]
    fn a_difficulty_is_one_to_five() {
        for (text, want) in [
            ("1", Some(1)),
            (" 5 ", Some(5)),
            ("0", None),
            ("6", None),
            ("x", None),
        ] {
            let mut e = entry(r"C:\charts\x\1.bms", "a");
            e.header.difficulty = Some(text.to_owned());
            let mut s = SelectScreen::new(Vec::new());
            s.set_entries(vec![e]);
            let v = s.view(1, 1, true);
            assert_eq!(v.rows[1].difficulty, want, "{text:?}");
            assert_eq!(v.selected.unwrap().labels.difficulty, want, "{text:?}");
        }
    }

    /// A chart that does not draw has a largest EX score: its judged notes at
    /// the preset's largest EX weight. One that draws has none, even when its
    /// best, from another draw, is past what its summary allows; so has one
    /// without a summary or a preset.
    #[test]
    fn the_largest_ex_needs_one_draw_a_summary_and_a_preset() {
        use bms_model::{ChartSummary, Tempo, TimeUs};
        let tempo = Tempo::from_units(120 * Tempo::UNITS_PER_BPM).unwrap();
        let summary = ChartSummary {
            taps: 10,
            longs: 5,
            mines: 3,
            length: TimeUs(1),
            tempo_start: tempo,
            tempo_min: tempo,
            tempo_max: tempo,
        };
        let mut e = entry(r"C:\charts\x\1.bms", "a");
        e.summary = Some(summary);
        e.sha256 = [1; 32];
        let max = |e: &ChartEntry, preset: Option<rules::RulePreset>| {
            let mut s = SelectScreen::new(Vec::new());
            s.set_entries(vec![e.clone()]);
            s.set_bests_preset(preset);
            s.set_bests(std::collections::HashMap::from([(
                [1; 32],
                library::Score {
                    plays: 1,
                    best: Some(library::Best {
                        ex: 40,
                        bp: 0,
                        cleared: Vec::new(),
                        full_combo: false,
                    }),
                },
            )]));
            s.view(0, 0, true).selected.unwrap().max_ex
        };
        assert_eq!(max(&e, Some(rules::tapestry())), Some(30));
        let mut heavier = rules::tapestry();
        heavier.score.ex.great = 4;
        assert_eq!(max(&e, Some(heavier)), Some(60));
        // The evaluator's EX score saturates at `u32::MAX`, and so does this.
        let mut huge = rules::tapestry();
        huge.score.ex.perfect = 3_000_000_000;
        let mut two = e.clone();
        two.summary = Some(ChartSummary {
            taps: 2,
            longs: 0,
            ..summary
        });
        assert_eq!(max(&two, Some(huge)), Some(u64::from(u32::MAX)));
        assert_eq!(max(&e, None), None);
        let mut random = e.clone();
        random.random = true;
        assert_eq!(max(&random, Some(rules::tapestry())), None);
        let mut refused = e;
        refused.summary = None;
        assert_eq!(max(&refused, Some(rules::tapestry())), None);
    }

    /// Without headings the window holds the selectable rows alone, and
    /// counts the rows asked for above and below the cursor among them;
    /// with headings it is the built-in page's window.
    #[test]
    fn a_view_without_headings_counts_selectable_rows() {
        use skin_model::{MAX_VISIBLE_ROWS, Ratio, RowKind};
        // A hundred charts in ten folders: at most `MAX_VISIBLE_ROWS` rows,
        // none a heading, the position exact over the hundred.
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        s.set_entries(names.iter().flat_map(|f| folder(f, 10)).collect());
        let window = |s: &SelectScreen| {
            let v = s.view(100, 100, false);
            assert_eq!(v.rows.len(), MAX_VISIBLE_ROWS);
            assert!(v.rows.iter().all(|r| r.kind == RowKind::Chart));
            (v.cursor, v.position, view_keys(&v))
        };
        let (cursor, position, keys) = window(&s);
        assert_eq!((cursor, position), (Some(0), Ratio::new(0, 99)));
        assert_eq!(keys.first().map(String::as_str), Some("a00.bms"));
        assert_eq!(keys.last().map(String::as_str), Some("g03.bms"));
        for _ in 0..45 {
            s.input(key(MenuKey::Down));
        }
        let (cursor, position, keys) = window(&s);
        assert_eq!((cursor, position), (Some(45), Ratio::new(45, 99)));
        assert_eq!(keys[45], "e05.bms");
        s.input(key(MenuKey::End));
        let (cursor, position, keys) = window(&s);
        assert_eq!(
            (cursor, position),
            (Some(MAX_VISIBLE_ROWS - 1), Ratio::new(99, 99))
        );
        assert_eq!(keys.last().map(String::as_str), Some("j09.bms"));
        assert_eq!(keys.first().map(String::as_str), Some("d06.bms"));

        let mut s = screen();
        assert_eq!(
            view_keys(&s.view(10, 10, false)),
            ["1.bms", "2.bms", "3.bms"]
        );
        s.input(key(MenuKey::Down));
        s.input(key(MenuKey::Down));
        let v = s.view(1, 0, false);
        assert_eq!(view_keys(&v), ["2.bms", "3.bms"]);
        assert_eq!(v.cursor, Some(1));
        let v = s.view(1, 0, true);
        assert_eq!(view_keys(&v), [r"heading C:\charts\b", "3.bms"]);
        assert_eq!(v.cursor, Some(1));
        let v = s.view(10, 10, false);
        assert_eq!(
            (view_keys(&v), v.cursor),
            (
                vec!["1.bms".to_owned(), "2.bms".to_owned(), "3.bms".to_owned()],
                Some(2)
            )
        );
    }

    /// A chart row carries its level as a skin draws it, as the source's
    /// `Integer.parseInt` reads `#PLAYLEVEL` (0 when it reads none), its
    /// summary's long notes and mines, unknown without a summary, and
    /// whether it draws; the chart under the cursor carries the same level.
    #[test]
    fn a_chart_row_carries_its_level_value_features_and_random() {
        use bms_model::{ChartSummary, Tempo, TimeUs};
        use skin_model::RowFeatures;
        let tempo = Tempo::from_units(120 * Tempo::UNITS_PER_BPM).unwrap();
        let summary = |longs, mines| ChartSummary {
            taps: 1,
            longs,
            mines,
            length: TimeUs(1),
            tempo_start: tempo,
            tempo_min: tempo,
            tempo_max: tempo,
        };
        let chart = |i: usize, level: Option<&str>, sum: Option<ChartSummary>, random| {
            let mut e = entry(&format!(r"C:\charts\a\{i}.bms"), "t");
            e.header.playlevel = level.map(str::to_owned);
            e.summary = sum;
            e.random = random;
            e
        };
        let mut s = SelectScreen::new(vec![PathBuf::from(r"C:\charts")]);
        s.set_entries(vec![
            chart(0, Some("12"), Some(summary(1, 0)), false),
            chart(1, Some("+7"), Some(summary(0, 2)), true),
            chart(2, Some(" 7"), Some(summary(3, 4)), false),
            chart(3, Some("12+"), None, true),
            chart(4, None, Some(summary(0, 0)), false),
            chart(5, Some("-3"), None, false),
            chart(6, Some("2147483648"), None, false),
        ]);
        let v = s.view(0, 10, false);
        let read: Vec<_> = v
            .rows
            .iter()
            .map(|r| (r.level_value, r.features, r.random))
            .collect();
        let f = |long_notes, mines| Some(RowFeatures { long_notes, mines });
        assert_eq!(
            read,
            [
                (12, f(true, false), false),
                (7, f(false, true), true),
                (0, f(true, true), false),
                (0, None, true),
                (0, f(false, false), false),
                (-3, None, false),
                (0, None, false),
            ]
        );
        assert_eq!(v.selected.unwrap().labels.level_value, 12);
        s.input(key(MenuKey::Down));
        let v = s.view(0, 0, false);
        assert_eq!(v.selected.unwrap().labels.level_value, 7);
    }

    /// The labels a skin draws are the player's own strings in its
    /// language: the "all charts" name and the results' name, without the
    /// query.
    #[test]
    fn the_skin_labels_are_the_players_strings() {
        assert_eq!(
            super::labels(),
            skin_model::SelectionLabels {
                all_charts: "全譜面".to_owned(),
                search_results: "検索結果".to_owned(),
            }
        );
    }
}
