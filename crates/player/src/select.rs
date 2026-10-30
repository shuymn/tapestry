//! The select screen's state on the game thread (design.md "Play
//! Lifecycle", "Loading and Song Selection"): the charts a library search
//! found, grouped by folder, the cursor, the search box and the order, and
//! the page the render thread draws from them ([`MenuPage`]).
//!
//! Its input comes through winit, from the keyboard (a controller's comes
//! later): the navigation keys move the cursor, Enter chooses,
//! Tab gives the search box focus, and text typed or committed through the
//! IME goes into the search box only while it has focus. The keyboard's
//! lane keys type nothing into the list. Everything here runs on menus,
//! where allocation is allowed.

use std::path::{Path, PathBuf};

use library::{ChartEntry, Search, Sort};

use crate::menu_screen::{LineStyle, MenuLine, MenuPage, PAGE_LINES};

/// The most charts one search lists; a search that finds more says so.
pub const LIMIT: usize = 5_000;
/// The list's lines on a page: the rest are the title, the search box, the
/// notice and the keys.
const LIST_LINES: usize = PAGE_LINES - 4;
/// Lines Page Up and Page Down move.
const PAGE_STEP: usize = LIST_LINES - 2;

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
    Backspace,
    Escape,
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

/// What an input did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acted {
    /// Nothing changed.
    Nothing,
    /// What the page shows changed.
    Changed,
    /// The search or the order changed: the library is searched again.
    Search,
    /// The search box gained or lost focus: the IME is allowed only while
    /// it has it.
    Focus(bool),
    /// The chart under the cursor was chosen.
    Choose(PathBuf),
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
    /// The heading of the folder of chart `.0`, the first in it.
    Folder(usize),
    /// Chart `.0`.
    Chart(usize),
}

/// The select screen.
#[derive(Debug, Default)]
pub struct SelectScreen {
    /// The charts found, grouped by folder, at most [`LIMIT`].
    entries: Vec<ChartEntry>,
    /// Whether the search found more than [`LIMIT`].
    more: bool,
    /// The list's lines.
    rows: Vec<Row>,
    /// The chart under the cursor, by index into `entries`.
    cursor: usize,
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
    /// The library roots, for folder names.
    roots: Vec<PathBuf>,
}

impl SelectScreen {
    /// An empty screen naming folders relative to `roots`.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            ..Self::default()
        }
    }

    /// The search that fills the list: one more than [`LIMIT`], to tell
    /// when there are more.
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

    /// The charts a search found, in its order; the cursor stays on the
    /// chart it was on when that chart is still listed.
    pub fn set_entries(&mut self, mut entries: Vec<ChartEntry>) {
        let kept = self.selected().map(|e| e.path.clone());
        self.more = entries.len() > LIMIT;
        entries.truncate(LIMIT);
        self.entries = entries;
        self.rows.clear();
        let mut folder: Option<&Path> = None;
        for (i, e) in self.entries.iter().enumerate() {
            let parent = e.path.parent();
            if folder != parent {
                folder = parent;
                self.rows.push(Row::Folder(i));
            }
            self.rows.push(Row::Chart(i));
        }
        let at = kept.and_then(|p| self.entries.iter().position(|e| e.path == p));
        self.cursor = at.unwrap_or(self.cursor.min(self.entries.len().saturating_sub(1)));
    }

    /// Puts the cursor on the chart at `path`; `false` when it is not
    /// listed.
    pub fn select_path(&mut self, path: &Path) -> bool {
        match self.entries.iter().position(|e| e.path == path) {
            Some(i) => {
                self.cursor = i;
                true
            }
            None => false,
        }
    }

    /// The chart under the cursor.
    pub fn selected(&self) -> Option<&ChartEntry> {
        self.entries.get(self.cursor)
    }

    /// How many charts are listed.
    pub fn listed(&self) -> usize {
        self.entries.len()
    }

    /// The charts listed, in order (dev-tools scripts).
    #[cfg(feature = "dev-tools")]
    pub fn entries(&self) -> &[ChartEntry] {
        &self.entries
    }

    /// Shows why the chosen chart was refused, or nothing.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
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
        let last = self.entries.len().saturating_sub(1);
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
            MenuKey::Enter => {
                return match self.selected() {
                    Some(e) => {
                        let path = e.path.clone();
                        self.notice = None;
                        Acted::Choose(path)
                    }
                    None => Acted::Nothing,
                };
            }
            // The retry and quit keys mean nothing here.
            MenuKey::Backspace | MenuKey::Escape => return Acted::Nothing,
        };
        if to == self.cursor {
            return Acted::Nothing;
        }
        self.cursor = to;
        Acted::Changed
    }

    fn search_input(&mut self, input: MenuInput) -> Acted {
        match input {
            MenuInput::Text(text) => {
                self.query.push_str(&text);
                Acted::Search
            }
            MenuInput::Preedit(text) => {
                self.preedit = text;
                Acted::Changed
            }
            // While the IME composes, its keys are its own.
            MenuInput::Key(_) if !self.preedit.is_empty() => Acted::Nothing,
            MenuInput::Key(MenuKey::Backspace) => {
                if self.query.pop().is_some() {
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
        let mut lines = vec![line(
            format!(
                "曲選択  {}{more} 譜面  並び順: {sort}  {status}",
                self.entries.len()
            ),
            LineStyle::Title,
        )];
        lines.push(if self.searching {
            line(
                format!("検索: {}{}_", self.query, self.preedit),
                LineStyle::Search,
            )
        } else {
            line(format!("検索 (Tab): {}", self.query), LineStyle::Search)
        });
        lines.push(line(
            self.notice.clone().unwrap_or_default(),
            LineStyle::Notice,
        ));
        let at = self
            .rows
            .iter()
            .position(|r| *r == Row::Chart(self.cursor))
            .unwrap_or(0);
        let first = at
            .saturating_sub(LIST_LINES / 3)
            .min(self.rows.len().saturating_sub(LIST_LINES));
        for row in self.rows.iter().skip(first).take(LIST_LINES) {
            lines.push(match *row {
                Row::Folder(i) => line(self.folder_name(i), LineStyle::Folder),
                Row::Chart(i) => {
                    let style = if i == self.cursor {
                        LineStyle::Cursor
                    } else {
                        LineStyle::Chart
                    };
                    line(self.chart_line(i), style)
                }
            });
        }
        if self.entries.is_empty() {
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
            "↑↓ 選択  Enter 決定  Tab 検索  ←→ 並び順  F1-F8 設定  ウィンドウを閉じて終了"
                .to_owned(),
            LineStyle::Help,
        ));
        MenuPage { lines }
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

    /// Chart `i`'s line: level, title, subtitle and artist, or its file
    /// name when it has no title.
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
        let mut text = format!("    ☆{:<3} {title}", h.playlevel.as_deref().unwrap_or("-"));
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

    use super::{Acted, MenuInput, MenuKey, SelectScreen};
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
        // The retry and quit keys do nothing here.
        assert_eq!(s.input(key(MenuKey::Backspace)), Acted::Nothing);
        assert_eq!(s.input(key(MenuKey::Escape)), Acted::Nothing);
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
}
