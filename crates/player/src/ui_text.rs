//! The player's own screen text by named IDs (design.md "Rendering and
//! Skins", player strings): a [`Label`] is a fixed string, a [`Message`] a
//! line with typed arguments, each in the language `player.toml` chose
//! ([`Language`]), read once at startup ([`init`]). Japanese is the base:
//! its tables match every ID with no catch-all, and every other language's
//! missing entry falls back to it ([`label_with`], [`write_with`]). Chart
//! text, folder and file names, preset names and the query are data: they
//! are never IDs, only a message's arguments, copied as they are.
//! Everything here runs outside in-play code; a message allocates.

use std::sync::OnceLock;

use game::Language;

/// The language chosen at startup.
static LANGUAGE: OnceLock<Language> = OnceLock::new();

/// Sets the language once, from the startup settings; a later call
/// changes nothing (design.md "Settings Files": read once, at startup).
pub fn init(language: Language) {
    let _ = LANGUAGE.set(language);
}

/// The language set by [`init`], Japanese before it.
pub fn language() -> Language {
    LANGUAGE.get().copied().unwrap_or_default()
}

/// A fixed string of the player's screens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Label {
    SortTitle,
    SortArtist,
    SortLevel,
    SortModified,
    /// After a count that stops at the list's limit.
    OrMore,
    /// The "all charts" row and place.
    AllCharts,
    /// A query's results as a place.
    SearchResults,
    /// The empty list with no query: where the library's roots are set.
    NoCharts,
    /// The empty list of a query.
    NotFound,
    /// The select screen's keys.
    SelectHelp,
    /// The library's scan running.
    Scanning,
    /// No library roots configured.
    NoRoots,
    ScanCannotStart,
    ScanFailed,
    /// A READY state waiting for the scan to pause.
    ScanPauseWait,
    /// A play skin being prepared or specialized.
    SkinPreparing,
    /// A play skin's preparation withdrawn.
    SkinCancelled,
    /// The screen kinds, in a skin's notice.
    ScreenPlay,
    ScreenSelection,
    ScreenDecision,
    ScreenResult,
    /// The loading lines' heading, of a play and of a practice.
    NowLoading,
    NowLoadingPractice,
    /// The loading stages.
    ReadingChart,
    LoadingSounds,
    MeasuringLoudness,
    PreparingScreen,
    /// The practice page's rows and keys.
    PracticeFrom,
    PracticeTo,
    PracticeSpeed,
    PracticeLeadIn,
    PracticeRepeat,
    PracticeStart,
    On,
    Off,
    PracticeHelp,
}

/// A line of the player's screens with typed arguments; string arguments
/// are data and are copied as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// The select screen's title: the place (`None` at the top), the rows
    /// listed and whether there are more, the order, the preset and the
    /// library's status.
    SelectTitle {
        place: Option<&'a str>,
        listed: usize,
        more: bool,
        sort: Label,
        preset: &'a str,
        status: &'a str,
    },
    /// The search line while the box has focus, before the composition.
    SearchEditing { query: &'a str },
    /// The search line without focus.
    SearchIdle { query: &'a str },
    /// The "all charts" row with its count.
    AllChartsRow { charts: u64 },
    /// `player.toml` could not be written, for the reason.
    SaveFailed { why: &'a str },
    /// A non-play kind's skin refused or unconfigured: its built-in screen.
    SkinBuiltIn { screen: Label, why: &'a str },
    /// The play skin refused.
    SkinUnusable { why: &'a str },
    /// The play skin could not be installed.
    SkinCannotPrepare { why: &'a str },
    /// The bests rebuilt so far.
    BestsRebuilding { counted: u64 },
    /// The sounds decoded so far.
    SoundsProgress { done: usize, files: usize },
    /// The play preset's name.
    Preset { name: &'a str },
    /// The practice page's title, of the chart's file.
    PracticeTitle { name: &'a str },
    /// The end measure when it is the chart's last.
    PracticeToEnd { to: usize },
    /// The lead-in in seconds, already formatted.
    PracticeLeadInValue { seconds: &'a str },
    /// The initial gauge row's label, with the gauge's id or `-`.
    PracticeGauge { gauge: &'a str },
}

impl Label {
    /// The label in the startup language.
    pub fn text(self) -> &'static str {
        self.text_in(language())
    }

    /// The label in `language`.
    pub fn text_in(self, language: Language) -> &'static str {
        match language {
            Language::Ja => label_with(self, |l| Some(ja::label(l)), ja::label),
        }
    }
}

impl Message<'_> {
    /// The message in the startup language, appended to `out`.
    pub fn write(&self, out: &mut String) {
        self.write_in(language(), out);
    }

    /// The message in `language`, appended to `out`.
    pub fn write_in(&self, language: Language, out: &mut String) {
        match language {
            Language::Ja => write_with(
                self,
                out,
                |m, out| {
                    ja::write(m, out);
                    Some(())
                },
                ja::write,
            ),
        }
    }

    /// The message in the startup language.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

/// `l` from the chosen language's table, or from the base's where the
/// chosen one has no entry.
fn label_with(
    l: Label,
    chosen: fn(Label) -> Option<&'static str>,
    base: fn(Label) -> &'static str,
) -> &'static str {
    chosen(l).unwrap_or_else(|| base(l))
}

/// `m` appended to `out` by the chosen language's table, or by the base's
/// where the chosen one has no entry; what the chosen one wrote before
/// giving up is removed first.
fn write_with(
    m: &Message<'_>,
    out: &mut String,
    chosen: fn(&Message<'_>, &mut String) -> Option<()>,
    base: fn(&Message<'_>, &mut String),
) {
    let start = out.len();
    if chosen(m, out).is_none() {
        out.truncate(start);
        base(m, out);
    }
}

/// Japanese: the base, with an entry for every ID.
mod ja {
    use std::fmt::Write as _;

    use super::{Label, Message};

    pub(super) const fn label(l: Label) -> &'static str {
        match l {
            Label::SortTitle => "タイトル",
            Label::SortArtist => "アーティスト",
            Label::SortLevel => "レベル",
            Label::SortModified => "更新日",
            Label::OrMore => "以上",
            Label::AllCharts => "全譜面",
            Label::SearchResults => "検索結果",
            Label::NoCharts => "譜面がありません (device.toml の [library] roots)",
            Label::NotFound => "見つかりません",
            Label::SelectHelp => {
                "↑↓ 選択  Enter 決定  Esc 戻る  P 練習  Tab 検索  ←→ 並び順  F11/F12 プリセット  F1-F8 設定  ウィンドウを閉じて終了"
            }
            Label::Scanning => "スキャン中",
            Label::NoRoots => "device.toml に [library] roots がありません",
            Label::ScanCannotStart => "スキャンできません",
            Label::ScanFailed => "スキャンに失敗しました",
            Label::ScanPauseWait => "ライブラリのスキャンの一時停止を待っています",
            Label::SkinPreparing => "スキンを準備しています",
            Label::SkinCancelled => "スキンの準備は取り消されました",
            Label::ScreenPlay => "プレイ",
            Label::ScreenSelection => "選曲",
            Label::ScreenDecision => "決定",
            Label::ScreenResult => "リザルト",
            Label::NowLoading => "NOW LOADING",
            Label::NowLoadingPractice => "NOW LOADING (練習)",
            Label::ReadingChart => "譜面を読み込んでいます",
            Label::LoadingSounds => "音を読み込んでいます",
            Label::MeasuringLoudness => "音量を測定しています",
            Label::PreparingScreen => "画面を準備しています",
            Label::PracticeFrom => "開始小節",
            Label::PracticeTo => "終了小節",
            Label::PracticeSpeed => "速度",
            Label::PracticeLeadIn => "リードイン",
            Label::PracticeRepeat => "リピート",
            Label::PracticeStart => "開始",
            Label::On => "オン",
            Label::Off => "オフ",
            Label::PracticeHelp => {
                "↑↓ 選択  ←→ 変更  Enter 編集・開始  Esc 戻る  ウィンドウを閉じて終了"
            }
        }
    }

    pub(super) fn write(m: &Message<'_>, out: &mut String) {
        // Writing to a `String` cannot fail.
        let _ = match *m {
            Message::SelectTitle {
                place,
                listed,
                more,
                sort,
                preset,
                status,
            } => {
                out.push_str("曲選択  ");
                if let Some(place) = place {
                    let _ = write!(out, "{place}  ");
                }
                let more = if more { label(Label::OrMore) } else { "" };
                write!(
                    out,
                    "{listed}{more} 譜面  並び順: {}  プリセット: {preset}  {status}",
                    label(sort)
                )
            }
            Message::SearchEditing { query } => write!(out, "検索: {query}"),
            Message::SearchIdle { query } => write!(out, "検索 (Tab): {query}"),
            Message::AllChartsRow { charts } => {
                write!(out, "  ▶ {}  ({charts})", label(Label::AllCharts))
            }
            Message::SaveFailed { why } => write!(out, "player.toml を保存できません: {why}"),
            Message::SkinBuiltIn { screen, why } => {
                write!(
                    out,
                    "{}のスキンは使えません（内蔵画面）: {why}",
                    label(screen)
                )
            }
            Message::SkinUnusable { why } => write!(out, "スキンを使えません: {why}"),
            Message::SkinCannotPrepare { why } => write!(out, "スキンを準備できません: {why}"),
            Message::BestsRebuilding { counted } => write!(out, "ベスト再構築中 {counted}"),
            Message::SoundsProgress { done, files } => {
                write!(out, "{}  {done} / {files}", label(Label::LoadingSounds))
            }
            Message::Preset { name } => write!(out, "プリセット: {name}"),
            Message::PracticeTitle { name } => write!(out, "練習  {name}"),
            Message::PracticeToEnd { to } => write!(out, "{to} (最後まで)"),
            Message::PracticeLeadInValue { seconds } => write!(out, "{seconds} 秒"),
            Message::PracticeGauge { gauge } => write!(out, "初期ゲージ ({gauge})"),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Label, Message, label_with, write_with};

    /// A language with one label and the start of one message only.
    fn partial_label(l: Label) -> Option<&'static str> {
        (l == Label::On).then_some("ON")
    }

    fn partial_write(m: &Message<'_>, out: &mut String) -> Option<()> {
        match *m {
            Message::Preset { name } => {
                out.push_str("PRESET ");
                out.push_str(name);
                Some(())
            }
            // Writes a part, then has no entry after all.
            Message::SaveFailed { .. } => {
                out.push_str("READ");
                None
            }
            _ => None,
        }
    }

    /// A language's own entry wins; a missing one is the base's, and a
    /// message the chosen language gave up leaves only the base's text
    /// after what `out` held.
    #[test]
    fn a_missing_entry_falls_back_to_the_base() {
        assert_eq!(label_with(Label::On, partial_label, super::ja::label), "ON");
        assert_eq!(
            label_with(Label::Off, partial_label, super::ja::label),
            "オフ"
        );
        let mut out = "held ".to_owned();
        write_with(
            &Message::Preset { name: "曲" },
            &mut out,
            partial_write,
            super::ja::write,
        );
        assert_eq!(out, "held PRESET 曲");
        let mut out = "held ".to_owned();
        write_with(
            &Message::SaveFailed { why: "a.bms" },
            &mut out,
            partial_write,
            super::ja::write,
        );
        assert_eq!(out, "held player.toml を保存できません: a.bms");
        let mut out = "held ".to_owned();
        write_with(
            &Message::SkinUnusable { why: "x" },
            &mut out,
            partial_write,
            super::ja::write,
        );
        assert_eq!(out, "held スキンを使えません: x");
    }

    /// Data is an argument, copied as it is, never looked up.
    #[test]
    fn arguments_are_copied_as_they_are() {
        let odd = "曲 {name} 100%";
        assert_eq!(
            Message::PracticeTitle { name: odd }.text(),
            format!("練習  {odd}")
        );
    }
}
