//! The load's lines on the play screen (design.md "Play Lifecycle"): from a
//! chart chosen to play, or a practice Start, to its countdown, the play
//! screen is drawn in its loading state, and these lines over it name the
//! chart and the play preset and say what the load is doing
//! ([`MenuPage`], laid out by the render thread's text path). Everything
//! here runs before recording opens, where allocation is allowed.

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

    /// The stage on the play screen.
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
}

/// One loading update, typed, beside its lines (design.md "Play
/// Lifecycle"): what the load is doing and the chart chosen, bound to the
/// loading lifetime it was shown in. Nothing reads these back out of the
/// formatted lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadingUpdate {
    /// What the load is doing.
    pub stage: Stage,
    /// The chart chosen.
    pub chart: ChosenChart,
    /// The play preset's name.
    pub preset: String,
    /// Whether it is a practice's Start.
    pub practice: bool,
    /// What the round's practice reset of what the player set, when an
    /// edit of the play preset did (`game::Reset`).
    pub notice: Option<String>,
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
    /// The chart's own fields, for the typed updates.
    pub chart: ChosenChart,
    /// What the round's practice reset of what the player set, announced
    /// before recording opens.
    pub notice: Option<String>,
}

impl LoadingText {
    /// The typed update at `stage`, beside [`lines`](Self::lines).
    pub fn update(&self, stage: Stage) -> LoadingUpdate {
        LoadingUpdate {
            stage,
            chart: self.chart.clone(),
            preset: self.preset.clone(),
            practice: self.practice,
            notice: self.notice.clone(),
        }
    }

    /// The lines at `stage`: the heading, the chart's lines and the stage,
    /// then what the practice reset, if anything.
    pub fn lines(&self, stage: Stage) -> MenuPage {
        let line = |text: String, style| MenuLine { text, style };
        let heading = if self.practice {
            "NOW LOADING (練習)"
        } else {
            "NOW LOADING"
        };
        let mut lines = vec![
            line(heading.to_owned(), LineStyle::Folder),
            line(self.title.clone(), LineStyle::Title),
            line(self.artist.clone(), LineStyle::Chart),
            line(format!("プリセット: {}", self.preset), LineStyle::Chart),
            line(stage.text(), LineStyle::Search),
        ];
        if let Some(notice) = &self.notice {
            lines.push(line(notice.clone(), LineStyle::Title));
        }
        MenuPage { lines, caret: None }
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
            chart: super::ChosenChart::default(),
            notice: None,
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
        for stage in [Stage::Chart, Stage::Loudness, Stage::Screen, Stage::Scan] {
            let p = text.lines(stage);
            assert!(p.lines.len() <= PAGE_LINES);
            // No cursor: the play screen draws no bar under a line.
            assert!(p.lines.iter().all(|l| l.style != LineStyle::Cursor));
        }
    }

    /// What a round's practice reset shows as the last line, and in the
    /// typed update beside it.
    #[test]
    fn the_lines_announce_a_reset() {
        let notice = "the gauge normal no longer accepts 15%; it starts at 20%";
        let text = LoadingText {
            practice: true,
            notice: Some(notice.to_owned()),
            ..LoadingText::default()
        };
        let page = text.lines(Stage::Screen);
        assert_eq!(page.lines.last().map(|l| l.text.as_str()), Some(notice));
        assert_eq!(text.update(Stage::Screen).notice.as_deref(), Some(notice));
    }
}
