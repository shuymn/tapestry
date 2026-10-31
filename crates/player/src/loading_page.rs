//! The loading page (design.md "Play Lifecycle"): from a chart chosen to
//! play, or a practice Start, to its countdown, it names the chart and the
//! play preset and says what the load is doing, as lines the render thread
//! draws ([`MenuPage`]). Everything here runs on menus, where allocation is
//! allowed.

use crate::menu_screen::{LineStyle, MenuLine, MenuPage};

/// What the load is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Reading the chart and the preset.
    Chart,
    /// Decoding the sounds: `done` of the chart's `files` distinct files.
    Sounds {
        /// Files done.
        done: usize,
        /// Distinct files; 0 before the count is known.
        files: usize,
    },
    /// Measuring the chart's loudness.
    Loudness,
    /// Waiting for the screen to have the chart.
    Screen,
    /// Waiting for the library's scan to pause before the countdown.
    Scan,
}

impl Stage {
    /// The stage in the console's words.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chart => "reading the chart",
            Self::Sounds { .. } => "loading the sounds",
            Self::Loudness => "measuring the loudness",
            Self::Screen => "preparing the screen",
            Self::Scan => "waiting for the scan to pause",
        }
    }

    /// The stage on the page.
    fn text(self) -> String {
        match self {
            Self::Chart => "譜面を読み込んでいます".to_owned(),
            Self::Sounds { files: 0, .. } => "音を読み込んでいます".to_owned(),
            Self::Sounds { done, files } => format!("音を読み込んでいます  {done} / {files}"),
            Self::Loudness => "音量を測定しています".to_owned(),
            Self::Screen => "画面を準備しています".to_owned(),
            Self::Scan => "ライブラリのスキャンの一時停止を待っています".to_owned(),
        }
    }
}

/// The chart being loaded, as its page names it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadingPage {
    /// The title, or the file's name without one.
    pub title: String,
    /// The artist; empty without one.
    pub artist: String,
    /// The play preset's name.
    pub preset: String,
    /// Whether it is a practice's Start.
    pub practice: bool,
}

impl LoadingPage {
    /// The page at `stage`: the heading, the chart's lines and the stage.
    pub fn page(&self, stage: Stage) -> MenuPage {
        let line = |text: String, style| MenuLine { text, style };
        let heading = if self.practice {
            "練習の読み込み中"
        } else {
            "読み込み中"
        };
        MenuPage {
            lines: vec![
                line(heading.to_owned(), LineStyle::Title),
                line(String::new(), LineStyle::Chart),
                line(self.title.clone(), LineStyle::Cursor),
                line(self.artist.clone(), LineStyle::Chart),
                line(format!("プリセット: {}", self.preset), LineStyle::Chart),
                line(String::new(), LineStyle::Chart),
                line(stage.text(), LineStyle::Search),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadingPage, Stage};
    use crate::menu_screen::{LineStyle, PAGE_LINES};

    /// The page names the chart, its artist and the preset, and says the
    /// stage, with the sounds' progress once their count is known.
    #[test]
    fn the_page_names_the_chart_and_the_stage() {
        let page = LoadingPage {
            title: "譜面A".to_owned(),
            artist: "テスト".to_owned(),
            preset: "lr2-style".to_owned(),
            practice: false,
        };
        let text = |stage| {
            page.page(stage)
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        };
        let lines = text(Stage::Sounds { done: 3, files: 12 });
        assert!(lines.contains(&"譜面A".to_owned()), "{lines:?}");
        assert!(lines.contains(&"テスト".to_owned()), "{lines:?}");
        assert!(
            lines.contains(&"プリセット: lr2-style".to_owned()),
            "{lines:?}"
        );
        assert!(lines.contains(&"音を読み込んでいます  3 / 12".to_owned()));
        let lines = text(Stage::Sounds { done: 0, files: 0 });
        assert!(
            lines.contains(&"音を読み込んでいます".to_owned()),
            "{lines:?}"
        );
        for stage in [Stage::Chart, Stage::Loudness, Stage::Screen, Stage::Scan] {
            let p = page.page(stage);
            assert!(p.lines.len() <= PAGE_LINES);
            // No cursor bar but the title's.
            let cursors = p.lines.iter().filter(|l| l.style == LineStyle::Cursor);
            assert_eq!(cursors.count(), 1);
        }
    }
}
