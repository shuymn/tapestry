//! The selection screen's snapshot (docs/stage3.md, the presentation
//! snapshot's Selection row): a bounded window of the list's rows around the
//! cursor, each with its identity, kind, text, level and typed score, the
//! cursor's position as an exact ratio, the place and the query, and what is
//! known of the selected chart. The player fills it; whatever draws the
//! screen reads it.

use std::path::{Path, PathBuf};

use bms_model::ChartSummary;

/// The most rows a [`SelectionView`] holds.
pub const MAX_VISIBLE_ROWS: usize = 64;

/// What selection shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionView {
    /// At most [`MAX_VISIBLE_ROWS`], in list order.
    pub rows: Vec<RowView>,
    /// The cursor's row, an index into `rows`; `None` when nothing is listed.
    pub cursor: Option<usize>,
    /// The cursor among the selectable rows of the whole list, exactly:
    /// `None` under two.
    pub position: Option<Ratio>,
    /// Where selection is.
    pub place: SelectionPlace,
    /// The search box's text.
    pub query: String,
    /// The chart under the cursor; `None` on any other row.
    pub selected: Option<SelectedChart>,
}

/// An exact ratio `numerator / denominator`, the denominator never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ratio {
    pub numerator: u64,
    pub denominator: u64,
}

impl Ratio {
    /// `numerator / denominator`; `None` for a zero denominator.
    #[must_use]
    pub const fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if denominator == 0 {
            None
        } else {
            Some(Self {
                numerator,
                denominator,
            })
        }
    }
}

/// Where selection is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SelectionPlace {
    /// The "all charts" row, then the library's roots or the one root's level.
    #[default]
    Top,
    /// A folder's level, by its directory's name.
    Folder(String),
    /// The all-chart list.
    All,
    /// A query's results, whatever the place under them.
    Results,
}

/// A folder's identity: its path as the library stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FolderId(PathBuf);

impl FolderId {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A row's identity, never its text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    All,
    Folder(FolderId),
    Chart(PathBuf),
    /// The heading of a folder's charts in the all-chart list and results.
    Heading(FolderId),
}

/// What a row is, with what it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// The "all charts" row; its label is the drawer's own.
    All {
        charts: u32,
    },
    /// A folder row: the listed charts under it.
    Folder {
        charts: u32,
    },
    Chart,
    /// Not selectable.
    Heading,
}

/// One row as it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowView {
    pub key: RowKey,
    pub kind: RowKind,
    /// A chart's `#TITLE` as stored, a folder's directory name, a heading's
    /// folder under its root; empty for the "all charts" row and a chart
    /// without a title.
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    /// `#PLAYLEVEL` as stored; empty when there is none.
    pub level: String,
    /// `#DIFFICULTY` when it is 1 to 5.
    pub difficulty: Option<u8>,
    /// Under the chosen preset; `Unplayed` on a row that is not a chart.
    pub score: RowScore,
}

/// A chart's best under the chosen preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowScore {
    /// Never played under it, or no preset is chosen.
    Unplayed,
    /// Played, and no play set a best.
    NoBest { plays: u64 },
    Best {
        plays: u64,
        ex: u32,
        bp: u32,
        lamp: OwnLamp,
    },
}

/// A best's lamp under the chosen preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnLamp {
    Failed,
    /// The cleared gauge of the highest lamp rank: its index in the preset's
    /// gauges and that rank.
    Cleared {
        gauge: u16,
        rank: u16,
    },
    FullCombo,
}

/// A chart's descriptive header, each field as stored, empty when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChartLabels {
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub subartist: String,
    pub genre: String,
    pub level: String,
    /// `#DIFFICULTY` when it is 1 to 5.
    pub difficulty: Option<u8>,
}

/// The chart under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedChart {
    pub labels: ChartLabels,
    /// The summary of its draw with seed 0; `None` when that draw is refused.
    pub summary: Option<ChartSummary>,
    /// Whether it declares `#RANDOM` or `#SETRANDOM`.
    pub random: bool,
    /// The largest EX score its summary allows under the chosen preset, for
    /// rates: `None` for a chart that draws, whose best may come from a draw
    /// with another note count, and for one without a summary or preset.
    pub max_ex: Option<u64>,
    pub score: RowScore,
}

#[cfg(test)]
mod tests {
    use super::Ratio;

    #[test]
    fn a_ratio_has_a_denominator() {
        assert_eq!(Ratio::new(1, 0), None);
        assert_eq!(
            Ratio::new(2, 3),
            Some(Ratio {
                numerator: 2,
                denominator: 3
            })
        );
    }
}
