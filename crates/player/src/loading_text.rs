//! The load's lines on the play screen (design.md "Play Lifecycle"): from a
//! chart chosen to play, or a practice Start, to its READY state, the play
//! screen is drawn in its loading state, and these lines over it name the
//! chart and the play preset and say what the load is doing
//! ([`MenuPage`], laid out by the render thread's text path). Everything
//! here runs before recording opens, where allocation is allowed.

use crate::menu_screen::{LineStyle, MenuLine, MenuPage};
use crate::ui_text::{Label, Message};

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
    /// Waiting for the library's scan to pause before READY.
    Scan,
    /// Building a pending play skin for the chart.
    Skin,
}

/// The wait for a pending play skin's specialization on a page other than
/// the loading state's (a retry, a practice round).
pub fn skin_wait() -> &'static str {
    Label::SkinPreparing.text()
}

impl Stage {
    /// The sounds done and the files to decode, while their count is
    /// known: `None` for any other stage, and for the sounds' stage
    /// before decoding, when it is reported with 0 files.
    pub const fn known_sounds(self) -> Option<(usize, usize)> {
        match self {
            Self::Sounds { done, files } if files > 0 => Some((done, files)),
            _ => None,
        }
    }

    /// The stage in the console's words.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chart => "reading the chart",
            Self::Sounds { .. } => "loading the sounds",
            Self::Loudness => "measuring the loudness",
            Self::Screen => "preparing the screen",
            Self::Scan => "waiting for the scan to pause",
            Self::Skin => "building the play skin",
        }
    }

    /// The stage on the play screen.
    fn text(self) -> String {
        match self {
            Self::Chart => Label::ReadingChart.text().to_owned(),
            Self::Sounds { files: 0, .. } => Label::LoadingSounds.text().to_owned(),
            Self::Sounds { done, files } => Message::SoundsProgress { done, files }.text(),
            Self::Loudness => Label::MeasuringLoudness.text().to_owned(),
            Self::Screen => Label::PreparingScreen.text().to_owned(),
            Self::Scan => Label::ScanPauseWait.text().to_owned(),
            Self::Skin => skin_wait().to_owned(),
        }
    }
}

/// The page a play without the selection screen shows while a pending play
/// skin is specialized for its chart: the wait alone.
pub fn skin_wait_page() -> MenuPage {
    MenuPage {
        lines: vec![MenuLine {
            text: skin_wait().to_owned(),
            style: LineStyle::Notice,
        }],
        caret: None,
    }
}

/// The chosen chart's descriptive fields as its library entry has them,
/// each `None` when unknown: what a skin's loading frame may show of the
/// chart. Never filled from the file's name or another chart's fields; the
/// lines' fallbacks stay in [`LoadingText`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChosenChart {
    /// `#TITLE`.
    pub title: Option<String>,
    /// `#SUBTITLE`.
    pub subtitle: Option<String>,
    /// `#ARTIST`.
    pub artist: Option<String>,
    /// `#SUBARTIST`.
    pub subartist: Option<String>,
    /// `#GENRE`.
    pub genre: Option<String>,
    /// The play level its `#PLAYLEVEL` gives ([`level_of`](crate::imports::facts::level_of)):
    /// known once the entry is, a missing header 0, as an attempt's is.
    pub play_level: Option<i32>,
}

/// The chart being loaded, as its lines name it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadingText {
    /// The title, or the file's name without one.
    pub title: String,
    /// The artist; empty without one.
    pub artist: String,
    /// The play preset's name.
    pub preset: String,
    /// Whether it is a practice's Start.
    pub practice: bool,
    /// Whether the play loaded is autoplay.
    pub autoplay: bool,
    /// The chart's own fields, which a loading lifetime's state starts
    /// from ([`crate::loading_state`]).
    pub chart: ChosenChart,
}

impl LoadingText {
    /// The lines at `stage`: the heading, the chart's lines and the stage.
    pub fn lines(&self, stage: Stage) -> MenuPage {
        let line = |text: String, style| MenuLine { text, style };
        let heading = if self.practice {
            Label::NowLoadingPractice
        } else {
            Label::NowLoading
        }
        .text();
        MenuPage {
            lines: vec![
                line(heading.to_owned(), LineStyle::Folder),
                line(self.title.clone(), LineStyle::Title),
                line(self.artist.clone(), LineStyle::Chart),
                line(
                    Message::Preset { name: &self.preset }.text(),
                    LineStyle::Chart,
                ),
                line(stage.text(), LineStyle::Search),
            ],
            caret: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadingText, Stage};
    use crate::menu_screen::{LineStyle, PAGE_LINES};

    /// The lines name the chart, its artist and the preset, and say the
    /// stage, with the sounds' progress once their count is known.
    #[test]
    fn the_lines_name_the_chart_and_the_stage() {
        let text = LoadingText {
            title: "譜面A".to_owned(),
            artist: "テスト".to_owned(),
            preset: "lr2-style".to_owned(),
            practice: false,
            autoplay: false,
            chart: super::ChosenChart::default(),
        };
        let strings = |stage| {
            text.lines(stage)
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        };
        let lines = strings(Stage::Sounds { done: 3, files: 12 });
        assert!(lines.contains(&"譜面A".to_owned()), "{lines:?}");
        assert!(lines.contains(&"テスト".to_owned()), "{lines:?}");
        assert!(lines.contains(&"プリセット: lr2-style".to_owned()));
        assert!(lines.contains(&"音を読み込んでいます  3 / 12".to_owned()));
        let lines = strings(Stage::Sounds { done: 0, files: 0 });
        assert!(
            lines.contains(&"音を読み込んでいます".to_owned()),
            "{lines:?}"
        );
        for stage in [
            Stage::Chart,
            Stage::Loudness,
            Stage::Screen,
            Stage::Scan,
            Stage::Skin,
        ] {
            let p = text.lines(stage);
            assert!(p.lines.len() <= PAGE_LINES);
            // No cursor: the play screen draws no bar under a line.
            assert!(p.lines.iter().all(|l| l.style != LineStyle::Cursor));
        }
    }

    /// Every line's whole text: the heading of a play and of a practice,
    /// the chart's and the preset's as stored (non-ASCII ones included),
    /// each stage's words, and the skin wait's page.
    #[test]
    fn the_lines_say_each_stage_in_full() {
        let mut text = LoadingText {
            title: "曲名".to_owned(),
            artist: "作者".to_owned(),
            preset: "自作プリセット".to_owned(),
            practice: false,
            autoplay: false,
            chart: super::ChosenChart::default(),
        };
        let strings = |text: &LoadingText, stage| {
            text.lines(stage)
                .lines
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>()
        };
        for (stage, words) in [
            (Stage::Chart, "譜面を読み込んでいます"),
            (Stage::Sounds { done: 0, files: 0 }, "音を読み込んでいます"),
            (
                Stage::Sounds { done: 3, files: 12 },
                "音を読み込んでいます  3 / 12",
            ),
            (Stage::Loudness, "音量を測定しています"),
            (Stage::Screen, "画面を準備しています"),
            (Stage::Scan, "ライブラリのスキャンの一時停止を待っています"),
            (Stage::Skin, "スキンを準備しています"),
        ] {
            assert_eq!(
                strings(&text, stage),
                [
                    "NOW LOADING",
                    "曲名",
                    "作者",
                    "プリセット: 自作プリセット",
                    words
                ],
                "{stage:?}"
            );
        }
        text.practice = true;
        assert_eq!(strings(&text, Stage::Chart)[0], "NOW LOADING (練習)");
        assert_eq!(
            super::skin_wait_page()
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["スキンを準備しています"]
        );
    }
}
