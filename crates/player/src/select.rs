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
use std::sync::{Mutex, PoisonError};

use eval::ControlId;
use input::hid::{HidProduct, is_axis};
use input::{MenuAction, MenuBindings, MenuEvent};
use library::{ChartEntry, FolderKey, FolderRow, Level, Score, Search, Sort};
use rules::RulePreset;

use crate::menu_screen::{Caret, LineStyle, MenuLine, MenuPage, PAGE_LINES};

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
#[derive(Debug, Default)]
pub struct MenuQueue {
    inputs: Mutex<VecDeque<MenuInput>>,
    dropped: AtomicU64,
}

impl MenuQueue {
    /// Queues `input`.
    pub fn push(&self, input: MenuInput) {
        let mut q = self.inputs.lock().unwrap_or_else(PoisonError::into_inner);
        match (input, q.back_mut()) {
            (MenuInput::Text(text), Some(MenuInput::Text(last))) => last.push_str(&text),
            (MenuInput::Preedit(text), Some(MenuInput::Preedit(last))) => *last = text,
            (MenuInput::Key(key), _) => {
                let keys = q.iter().filter(|i| matches!(i, MenuInput::Key(_))).count();
                if keys < QUEUED_KEYS {
                    q.push_back(MenuInput::Key(key));
                } else {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            (input, _) => q.push_back(input),
        }
    }

    /// Takes the oldest input.
    pub fn pop(&self) -> Option<MenuInput> {
        self.inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
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
const SORTS: [(Sort, &str); 4] = [
    (Sort::Title, "タイトル"),
    (Sort::Artist, "アーティスト"),
    (Sort::Level, "レベル"),
    (Sort::Modified, "更新日"),
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
    /// The roots' keys, as Top last read them: a root is never a song folder.
    root_keys: Vec<FolderKey>,
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
    /// The library roots as configured, for folder names and how many.
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
    /// `level`, which is the one root's or the rows of the roots, whose keys
    /// are `root_keys`.
    pub fn set_top(&mut self, level: Level, root_keys: Vec<FolderKey>, charts: u64) {
        self.root_keys = root_keys;
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
                let song_folder = !self.root_keys.contains(key)
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

    /// The preset whose bests the list shows; `None` shows none.
    pub fn set_bests_preset(&mut self, preset: Option<RulePreset>) {
        if preset.is_none() {
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
            MenuInput::Key(_) => Acted::Nothing,
        }
    }

    /// The page as it stands, with `status` (the library's state) in the
    /// title.
    pub fn page(&self, status: &str) -> MenuPage {
        let line = |text: String, style| MenuLine { text, style };
        let sort = SORTS.get(self.sort).map_or("", |&(_, label)| label);
        let more = if self.more { "以上" } else { "" };
        let place = if self.query.is_empty() {
            match &self.place {
                Place::Top => String::new(),
                Place::Folder { name, .. } => format!("{name}  "),
                Place::All => "全譜面  ".to_owned(),
            }
        } else {
            "検索結果  ".to_owned()
        };
        let mut lines = vec![line(
            format!(
                "曲選択  {place}{}{more} 譜面  並び順: {sort}  プリセット: {}  {status}",
                self.listed(),
                self.preset
            ),
            LineStyle::Title,
        )];
        let mut caret = None;
        lines.push(if self.searching {
            let text = format!("検索: {}", self.query);
            let start = text.len();
            caret = Some(Caret {
                line: lines.len(),
                start,
                end: start + self.preedit.len(),
            });
            line(format!("{text}{}_", self.preedit), LineStyle::Search)
        } else {
            line(format!("検索 (Tab): {}", self.query), LineStyle::Search)
        });
        lines.push(line(
            self.notice
                .clone()
                .or_else(|| self.save_error.clone())
                .or_else(|| self.preset_error.clone())
                .unwrap_or_default(),
            LineStyle::Notice,
        ));
        let at = self.row_at_cursor();
        for row in self.rows.iter().skip(self.top).take(LIST_LINES) {
            let style = |other| {
                if Some(*row) == at {
                    LineStyle::Cursor
                } else {
                    other
                }
            };
            lines.push(match *row {
                Row::All => line(
                    format!("  ▶ 全譜面  ({})", self.all_row.unwrap_or(0)),
                    style(LineStyle::Folder),
                ),
                Row::Folder(i) => line(self.folder_line(i), style(LineStyle::Folder)),
                Row::Heading(i) => line(self.folder_name(i), LineStyle::Folder),
                Row::Chart(i) => line(self.chart_line(i), style(LineStyle::Chart)),
            });
        }
        if self.entries.is_empty() && self.folders.is_empty() {
            lines.push(line(
                if self.query.is_empty() {
                    "譜面がありません (device.toml の [library] roots)".to_owned()
                } else {
                    "見つかりません".to_owned()
                },
                LineStyle::Chart,
            ));
        }
        while lines.len() < PAGE_LINES - 1 {
            lines.push(line(String::new(), LineStyle::Chart));
        }
        lines.push(line(
            "↑↓ 選択  Enter 決定  Esc 戻る  P 練習  Tab 検索  ←→ 並び順  F11/F12 プリセット  F1-F8 設定  ウィンドウを閉じて終了"
                .to_owned(),
            LineStyle::Help,
        ));
        MenuPage { lines, caret }
    }

    /// Folder row `i`: its directory's name and the charts under it.
    fn folder_line(&self, i: usize) -> String {
        self.folders
            .get(i)
            .map_or_else(String::new, |f| format!("  ▶ {}  ({})", f.name, f.charts))
    }

    /// The heading of chart `i`'s folder: its path under its root.
    fn folder_name(&self, i: usize) -> String {
        let Some(folder) = self.entries.get(i).and_then(|e| e.path.parent()) else {
            return String::new();
        };
        let under = self
            .roots
            .iter()
            .find_map(|r| folder.strip_prefix(r).ok())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(folder);
        format!("▼ {}", under.display())
    }

    /// Chart `i`'s line: its lamp and best EX under the chosen preset
    /// ([`crate::bests::chart_cell`]), level, title, subtitle and artist,
    /// or its file name when it has no title.
    fn chart_line(&self, i: usize) -> String {
        let Some(e) = self.entries.get(i) else {
            return String::new();
        };
        let h = &e.header;
        let title = h.title.clone().unwrap_or_else(|| {
            e.path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        });
        let cell = self.bests_preset.as_ref().map_or_else(
            || " ".repeat(crate::bests::LAMP_COLUMNS + 6),
            |p| crate::bests::chart_cell(self.bests.get(&e.sha256), p),
        );
        let mut text = format!(
            "    {cell} ☆{:<3} {title}",
            h.playlevel.as_deref().unwrap_or("-")
        );
        if let Some(sub) = &h.subtitle {
            text.push(' ');
            text.push_str(sub);
        }
        if let Some(artist) = &h.artist {
            text.push_str("  / ");
            text.push_str(artist);
        }
        text
    }
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
            Vec::new(),
            5_001,
        );
        assert_eq!(s.listed(), super::LIMIT);
        assert!(s.page("").lines[0].text.contains("5000以上 譜面"));
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
}
