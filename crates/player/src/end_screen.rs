//! The end screen: the play's summary as text lines for the bitmap font,
//! numbers from the re-evaluation of the saved record (`ReEvaluate`), in a
//! fixed layout with the result screen's view of it (design.md "Play
//! Lifecycle"): the preset strip, what the view shows, its coverage, the
//! viewed file's error and the latest notices.

use std::time::Duration;

use game::{
    AbortCause, Chain, Notice, PlayEnd, PlayReport, PresetFileError, ResultScreen, SHOWN_GAUGES,
    ShelfError, Tuning, ViewEntry, ViewStatus, ViewSummary,
};
use replay::verify::Coverage;
use rules::{GaugeValue, RulePreset};

/// How long loading took, shown on the end screen (design.md "Latency
/// Figures").
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTimes {
    /// From the start of loading to the first play's first frame.
    pub first: Option<Duration>,
    /// From the latest retry's receipt to its play's first frame.
    pub retry: Option<Duration>,
}

/// A play's audio queue, for the end screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLine {
    /// Periods the play queued, when the output said.
    pub played: Option<u32>,
    /// Its underruns from chart time 0 on, after the countdown.
    pub steady_underruns: u64,
    /// Periods the next play queues.
    pub next: u32,
}

/// The periods the next play will actually queue: the choice, as far as the
/// output's queue holds (`most`, when it says), so the end screen never
/// promises more than the output can keep.
pub fn next_queue(choice: u32, most: Option<u32>) -> u32 {
    most.map_or(choice, |m| choice.min(m))
}

/// The audio queue line: what the play queued, its underruns after the
/// countdown, and the next play's queue when it differs.
pub fn queue_line(q: QueueLine) -> String {
    let played = q.played.map_or_else(|| "N/A".to_owned(), |p| p.to_string());
    let mut line = format!(
        "AUDIO QUEUE {played} PERIODS, {} UNDERRUNS AFTER THE COUNTDOWN",
        q.steady_underruns
    );
    if q.played != Some(q.next) {
        line.push_str(&format!(": {} FROM THE NEXT PLAY", q.next));
    }
    line
}

/// The names of the keys bound to retry and quit, for the end screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hints {
    /// The retry key.
    pub retry: String,
    /// The quit key.
    pub quit: String,
}

impl Hints {
    /// The names of the keys with these scan codes.
    pub fn new(retry: Option<u32>, quit: Option<u32>) -> Self {
        Self {
            retry: key_name(retry),
            quit: key_name(quit),
        }
    }
}

/// A keyboard scan code's name in the bitmap font: the keys a system
/// control is likely bound to by name, any other by its code; an unbound
/// control is "UNBOUND".
pub fn key_name(code: Option<u32>) -> String {
    let Some(code) = code else {
        return "UNBOUND".to_owned();
    };
    let name = match code {
        0x01 => "ESCAPE",
        0x0E => "BACKSPACE",
        0x0F => "TAB",
        0x1C => "ENTER",
        0x39 => "SPACE",
        0x3B..=0x44 => return format!("F{}", code - 0x3A),
        0x57 => "F11",
        0x58 => "F12",
        0xE01C => "NUMPAD ENTER",
        0xE053 => "DELETE",
        0xE047 => "HOME",
        0xE04F => "END",
        _ => return format!("KEY {code:04X}"),
    };
    name.to_owned()
}

/// `us` as signed milliseconds with one decimal, truncated toward zero.
pub fn ms(us: i64) -> String {
    let sign = if us < 0 { "-" } else { "+" };
    let a = us.unsigned_abs();
    format!("{sign}{}.{}", a / 1_000, a % 1_000 / 100)
}

fn cause(c: AbortCause) -> &'static str {
    match c {
        AbortCause::FocusLoss => "FOCUS LOSS",
        AbortCause::LastSourceRemoved => "LAST DEVICE REMOVED",
        AbortCause::Overflow => "INPUT OVERFLOW",
        AbortCause::Retry => "RETRY",
        AbortCause::Quit => "QUIT",
        AbortCause::Suspend => "SUSPEND",
        AbortCause::OutputFailed => "AUDIO OUTPUT FAILED",
        AbortCause::GaugeFailed => "GAUGE FAILED",
        AbortCause::ScreenFailed => "SCREEN FAILED",
        AbortCause::InputFailed => "INPUT FAILED",
    }
}

/// The line naming why the record is incomplete, if it is.
fn incomplete_line(i: replay::Incomplete) -> Option<String> {
    let why: Vec<&str> = [
        (i.overflow, "OVERFLOW"),
        (i.source_limit, "SOURCE LIMIT"),
        (i.suspend, "SUSPEND"),
        (i.input_failed, "INPUT FAILED"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    (!why.is_empty()).then(|| format!("RECORD INCOMPLETE: {}", why.join(", ")))
}

/// Columns a line of the bitmap screen holds (76 glyphs of 24 px from the
/// left edge at 80 px).
pub const COLUMNS: usize = 76;

/// Lines of the summary, the key hints last; the settings lines follow,
/// for 25 in all.
pub const SUMMARY_LINES: usize = 23;

/// `text` as the bitmap font draws it: uppercase, any character it has no
/// glyph for as `?`, and cut to `width` columns with a trailing `..`.
pub fn fit(text: &str, width: usize) -> String {
    let drawn: Vec<char> = text
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c.to_ascii_uppercase()
            } else {
                '?'
            }
        })
        .collect();
    if drawn.len() <= width {
        return drawn.into_iter().collect();
    }
    let mut cut: String = drawn[..width.saturating_sub(2)].iter().collect();
    cut.push_str("..");
    cut
}

/// A preset file's error as the screens show it: the file's name and the
/// cause, never its path, which goes to the console and the log only.
pub fn preset_error_text(e: &PresetFileError) -> String {
    let name = e
        .path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    format!("{name}: {}", Chain(&e.cause))
}

/// Why the shelf cannot open, as the screens show it: by file name and
/// cause, never a path.
pub fn shelf_error_text(e: &ShelfError) -> String {
    match e {
        ShelfError::Unknown(u) => u.to_string(),
        ShelfError::Preset(e) => preset_error_text(e),
        ShelfError::Io { path, kind } => {
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            format!("{name}: {kind}")
        }
    }
}

/// `t` in seconds with three decimals.
fn seconds(t: bms_model::TimeUs) -> String {
    let sign = if t.0 < 0 { "-" } else { "" };
    let a = t.0.unsigned_abs();
    format!("{sign}{}.{:03}", a / 1_000_000, a % 1_000_000 / 1_000)
}

/// The preset strip: the entries before and after the viewed one, the
/// viewed one between `>` and `<`, and its place; `+` marks the play's own
/// preset and `!` an entry whose file has an error.
fn strip(screen: &ResultScreen) -> String {
    let entries = screen.entries();
    let n = entries.len();
    let name = |i: usize| {
        entries.get(i).map_or_else(String::new, |e: &ViewEntry| {
            let own = e.hash == Some(screen.played());
            let mut s = fit(&e.name, 18);
            if own {
                s.push('+');
            }
            if e.error.is_some() {
                s.push('!');
            }
            s
        })
    };
    let at = screen.cursor();
    let prev = (at + n - 1) % n.max(1);
    let next = (at + 1) % n.max(1);
    format!(
        "< {}  >{}<  {} >  {}/{n}",
        name(prev),
        name(at),
        name(next),
        at + 1
    )
}

/// What the view shows, on its line.
fn view_label(status: ViewStatus<'_>, name: &str) -> String {
    match status {
        ViewStatus::Ready(s) if s.as_played => "AS PLAYED".to_owned(),
        ViewStatus::Ready(_) => format!("WHAT-IF UNDER {name}"),
        ViewStatus::CannotLoad(e) => format!("CANNOT LOAD: {}", preset_error_text(e)),
        ViewStatus::OverBudget { .. } => format!("TOO LARGE TO RE-EVALUATE UNDER {name}"),
        ViewStatus::Failed(e) => format!("NO RE-EVALUATION: {}", Chain(e)),
        ViewStatus::NothingRecorded => "NOTHING WAS RECORDED".to_owned(),
    }
}

/// What a what-if's input covers, on its own line; blank as played.
fn coverage_line(status: ViewStatus<'_>) -> String {
    match status {
        ViewStatus::Ready(s) if !s.as_played => match s.coverage {
            Coverage::NoInputAfter(t) => format!("NO INPUT AFTER {} S", seconds(t)),
            Coverage::EndedMidway(t) => format!("ENDED MIDWAY AT {} S, NO CLEAR", seconds(t)),
            Coverage::Recorded => String::new(),
        },
        _ => String::new(),
    }
}

fn notice_line(n: &Notice) -> String {
    match n {
        Notice::Reloaded(name) => format!("{name} RELOADED"),
        Notice::NextPlayUsesIt(_) => "THE NEXT PLAY USES IT".to_owned(),
        Notice::Removed(name) => format!("{name}: REMOVED"),
        Notice::Failed(e) | Notice::ListingFailed(e) => preset_error_text(e),
    }
}

/// The gauge cells: `width` columns each, the gauge's name cut to fit, in
/// the viewed preset's order and named by it; a seventh or later gauge
/// counts in place of the sixth.
fn gauge_cells(s: &ViewSummary, preset: Option<&RulePreset>, width: usize) -> Vec<String> {
    let mut cells: Vec<String> = s
        .gauges
        .iter()
        .enumerate()
        .filter_map(|(i, g)| g.map(|g| (i, g)))
        .map(|(i, g)| {
            let id = preset
                .and_then(|p| p.gauges.get(i))
                .map_or_else(|| format!("GAUGE {}", i + 1), |r| r.id.clone());
            let v = g.value.0;
            let default = if s.started_at_default & (1 << i) != 0 {
                " (DEFAULT START)"
            } else {
                ""
            };
            let rest = format!(
                " {}.{:03}% {}{default}",
                v / GaugeValue::PERCENT,
                v % GaugeValue::PERCENT / 100,
                if g.cleared { "CLEAR" } else { "NOT CLEAR" }
            );
            let room = width.saturating_sub(rest.len());
            fit(&format!("{}{rest}", fit(&id, room)), width)
        })
        .collect();
    if s.more_gauges > 0 {
        cells.truncate(SHOWN_GAUGES - 1);
        cells.push(format!("+{} MORE", u32::from(s.more_gauges) + 1));
    }
    cells
}

/// The end screen's summary: [`SUMMARY_LINES`] lines in a fixed layout, each
/// cut to [`COLUMNS`], the view of `screen` among them. `input_offset_us`
/// is the input offset the play used, to which the recommendation adds the
/// median deviation; whether the play matches the live stream and the
/// recommendation stay the play's own, whatever is viewed. An `autoplay`
/// play (design.md "Autoplay") says it was not saved in place of the saved
/// line and recommends no input offset: its deviations say nothing about
/// the player's.
pub fn lines(
    report: &PlayReport,
    screen: &ResultScreen,
    input_offset_us: i64,
    times: LoadTimes,
    queue: QueueLine,
    hints: &Hints,
    autoplay: bool,
) -> Vec<String> {
    let viewed = screen.viewed();
    let name = viewed.map_or(String::new(), |e| e.name.clone());
    let status = screen.status();
    let mut l = vec![String::new(); SUMMARY_LINES];
    let mut set = |i: usize, text: String| {
        if let Some(line) = l.get_mut(i) {
            *line = fit(&text, COLUMNS);
        }
    };
    set(
        0,
        match report.end {
            PlayEnd::Completed => "RESULT: PLAYED TO END".to_owned(),
            PlayEnd::Aborted(c) => format!("RESULT: ENDED MIDWAY ({})", cause(c)),
        },
    );
    set(1, strip(screen));
    set(2, view_label(status, &name));
    set(3, coverage_line(status));
    if let Some(e) = screen.file_error() {
        set(4, format!("FILE DID NOT RELOAD: {}", preset_error_text(e)));
    }
    for (i, n) in screen.notices().enumerate() {
        set(5 + i, notice_line(n));
    }
    if let Some(e) = &report.reload_error {
        set(
            7,
            format!(
                "THIS PLAY USED THE PREVIOUS {}: {}",
                report.preset.id,
                preset_error_text(e)
            ),
        );
    }
    if let ViewStatus::Ready(s) = status {
        let t = &s.tally;
        let c = &t.counts;
        set(
            8,
            format!(
                "PERFECT {}  GREAT {}  GOOD {}  BAD {}  POOR {}",
                c.perfect, c.great, c.good, c.bad, c.poor
            ),
        );
        set(
            9,
            format!(
                "EX SCORE {}  MAX COMBO {}  BP {}",
                t.ex_score, t.max_combo, t.bp
            ),
        );
        set(10, format!("EMPTY POOR {}", t.empty_poor));
        let preset = viewed.and_then(|e| e.preset.as_deref());
        let width = (COLUMNS - 2) / 2;
        for (row, pair) in gauge_cells(s, preset, width).chunks(2).enumerate() {
            set(11 + row, pair.join("  "));
        }
    }
    let summary = report.result.as_ref();
    if let Some(s) = summary {
        set(
            14,
            if s.matches_live {
                "RE-EVALUATION MATCHES THE LIVE PLAY"
            } else {
                "RE-EVALUATION DIFFERS FROM THE LIVE PLAY"
            }
            .to_owned(),
        );
    }
    if let Some(s) = summary.filter(|_| !autoplay) {
        set(
            15,
            match s.recommended_offset {
                Some(r) => format!(
                    "RECOMMENDED INPUT OFFSET {} MS (NOW {} MS)",
                    ms(input_offset_us.saturating_add(r.median.0)),
                    ms(input_offset_us)
                ),
                None => format!(
                    "RECOMMENDED INPUT OFFSET: NEEDS {} JUDGED PRESSES",
                    game::MIN_OFFSET_SAMPLES
                ),
            },
        );
        if let Some(r) = s.recommended_offset {
            set(
                16,
                format!(
                    "  MEDIAN DEVIATION {} MS, SPREAD {} MS, {} PRESSES",
                    ms(r.median.0),
                    ms(r.spread.0),
                    r.samples
                ),
            );
        }
    }
    if let Some(line) = report
        .record
        .as_ref()
        .and_then(|rec| incomplete_line(rec.status.incomplete))
    {
        set(17, line);
    }
    match &report.saved {
        Some(Ok(r)) => {
            let name = r
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            set(18, format!("SAVED AS RECORD {name}"));
        }
        Some(Err(e)) => set(18, format!("NOT SAVED: {}", Chain(e))),
        None if autoplay => set(18, "AUTOPLAY: NOT SAVED".to_owned()),
        None => {}
    }
    let failures: Vec<String> = report
        .output_error
        .map(|e| format!("AUDIO: {e}"))
        .into_iter()
        .chain(report.input_error.map(|e| format!("INPUT: {e}")))
        .collect();
    set(19, failures.join("  "));
    set(20, queue_line(queue));
    let ms = |d: Option<Duration>| d.map_or_else(|| "-".to_owned(), |d| d.as_millis().to_string());
    set(
        21,
        format!("LOAD {} MS  RETRY {} MS", ms(times.first), ms(times.retry)),
    );
    set(
        22,
        format!(
            "{}: RETRY  {}: BACK  LEFT/RIGHT: PRESET",
            hints.retry, hints.quit
        ),
    );
    l
}

/// The settings keys and their values, under the summary, on the two lines
/// left: the input offset with its keys always among them. The input offset
/// applies from the next play (design.md "Offset Changes and the
/// Watermark") of devices; autoplay plays with 0 whatever it is.
pub fn settings_lines(t: Tuning) -> Vec<String> {
    vec![
        fit(
            &format!(
                "F1/F2 VISIBLE TIME {} MS  F3/F4 VOLUME {}%  F5/F6 DISPLAY OFFSET {} MS",
                t.visible_time_ms,
                t.volume_percent,
                ms(t.display_offset_us)
            ),
            COLUMNS,
        ),
        fit(
            &format!(
                "F7/F8 INPUT OFFSET {} MS FROM THE NEXT PLAY  F9 OVERLAY",
                ms(t.input_offset_us)
            ),
            COLUMNS,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bms_model::{ChartBuilder, Lane, TimeUs};
    use game::{GaugeEnd, Notice, ResultScreen, SHOWN_GAUGES, ViewStatus, ViewSummary};
    use replay::verify::Coverage;
    use rules::GaugeValue;

    use super::{COLUMNS, Hints, LoadTimes, QueueLine, SUMMARY_LINES, key_name, ms};

    #[test]
    fn key_names_follow_the_scan_code() {
        assert_eq!(key_name(Some(0x0E)), "BACKSPACE");
        assert_eq!(key_name(Some(0x01)), "ESCAPE");
        assert_eq!(key_name(Some(0x3B)), "F1");
        assert_eq!(key_name(Some(0x44)), "F10");
        assert_eq!(key_name(Some(0x2C)), "KEY 002C");
        assert_eq!(key_name(None), "UNBOUND");
    }

    #[test]
    fn settings_lines_show_each_value_and_its_keys() {
        let lines = super::settings_lines(game::Tuning {
            visible_time_ms: 575,
            volume_percent: 35,
            display_offset_us: -2_000,
            input_offset_us: 4_000,
        });
        assert_eq!(
            lines,
            [
                "F1/F2 VISIBLE TIME 575 MS  F3/F4 VOLUME 35%  F5/F6 DISPLAY OFFSET -2.0 MS",
                "F7/F8 INPUT OFFSET +4.0 MS FROM THE NEXT PLAY  F9 OVERLAY",
            ]
        );
    }

    /// The queue line says what the play queued and, when the next play
    /// queues another number, that number.
    #[test]
    fn the_queue_line_names_the_next_play_only_when_it_changes() {
        let line = |played, steady_underruns, next| {
            super::queue_line(super::QueueLine {
                played,
                steady_underruns,
                next,
            })
        };
        assert_eq!(
            line(Some(2), 0, 2),
            "AUDIO QUEUE 2 PERIODS, 0 UNDERRUNS AFTER THE COUNTDOWN"
        );
        assert_eq!(
            line(Some(2), 4, 3),
            "AUDIO QUEUE 2 PERIODS, 4 UNDERRUNS AFTER THE COUNTDOWN: 3 FROM THE NEXT PLAY"
        );
        assert_eq!(
            line(Some(3), 7, 3),
            "AUDIO QUEUE 3 PERIODS, 7 UNDERRUNS AFTER THE COUNTDOWN"
        );
        assert_eq!(
            line(None, 1, 3),
            "AUDIO QUEUE N/A PERIODS, 1 UNDERRUNS AFTER THE COUNTDOWN: 3 FROM THE NEXT PLAY"
        );
    }

    /// A raised choice the output cannot hold is not promised: a buffer of
    /// two periods keeps showing two.
    #[test]
    fn the_next_queue_is_what_the_output_holds() {
        assert_eq!(super::next_queue(3, Some(2)), 2);
        assert_eq!(super::next_queue(3, Some(3)), 3);
        assert_eq!(super::next_queue(2, Some(3)), 2);
        assert_eq!(super::next_queue(3, None), 3);
        let line = super::queue_line(super::QueueLine {
            played: Some(2),
            steady_underruns: 9,
            next: super::next_queue(3, Some(2)),
        });
        assert_eq!(
            line,
            "AUDIO QUEUE 2 PERIODS, 9 UNDERRUNS AFTER THE COUNTDOWN"
        );
    }

    #[test]
    fn milliseconds_keep_the_sign_and_truncate_tenths() {
        assert_eq!(ms(0), "+0.0");
        assert_eq!(ms(3_249), "+3.2");
        assert_eq!(ms(-3_249), "-3.2");
        assert_eq!(ms(-999), "-0.9");
        assert_eq!(ms(12_000), "+12.0");
    }

    /// One tap at 1 s on lane 1.
    fn chart() -> bms_model::Chart {
        ChartBuilder::new(150_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap()
    }

    const QUEUE: QueueLine = QueueLine {
        played: Some(2),
        steady_underruns: 0,
        next: 2,
    };

    fn hints() -> Hints {
        Hints::new(Some(0x0E), Some(0x01))
    }

    /// A completed play's report under `preset`, with one gauge result per
    /// preset gauge at 61.234 %, and a record of no input.
    fn report(preset: rules::RulePreset) -> game::PlayReport {
        use eval::{Evaluation, GaugeResult, Tally};
        use rules::PerJudgment;
        let tally = Tally {
            counts: PerJudgment {
                perfect: 3,
                great: 5,
                good: 7,
                bad: 11,
                poor: 13,
            },
            ex_score: 11,
            bp: 24,
            combo: 2,
            max_combo: 4,
            empty_poor: 6,
        };
        let gauges = preset
            .gauges
            .iter()
            .map(|_| GaugeResult {
                value: GaugeValue(6_123_400),
                reference: GaugeValue(6_123_400),
                failed_at: None,
                cleared: false,
                history: Vec::new(),
            })
            .collect();
        game::PlayReport {
            attempt: 2,
            end: game::PlayEnd::Completed,
            tail_cut: None,
            reload_error: None,
            output_error: None,
            input_error: None,
            live: Vec::new(),
            tally,
            displayed_gauge: game::GaugeIndex(0),
            record: Some(record(&preset, replay::Incomplete::default())),
            saved: None,
            result: Some(game::ResultSummary {
                evaluation: Ok(Evaluation {
                    events: Vec::new(),
                    tally,
                    gauges,
                    completed: true,
                }),
                matches_live: true,
                recommended_offset: None,
            }),
            preset,
        }
    }

    /// The result screen of `report` over the presets in `dir`.
    fn screen(dir: &Path, report: &game::PlayReport) -> ResultScreen {
        let shelf = game::Shelf::open(dir.join("presets"), "tapestry").unwrap();
        ResultScreen::new(report, &shelf, &chart(), game::RECORDING_MEMORY)
    }

    /// The end screen of `report`, viewed on `screen`.
    fn lines_of(report: &game::PlayReport, screen: &ResultScreen, autoplay: bool) -> Vec<String> {
        super::lines(
            report,
            screen,
            0,
            LoadTimes::default(),
            QUEUE,
            &hints(),
            autoplay,
        )
    }

    /// The gauges are named by the preset viewed: as played, the preset the
    /// play used, whose renamed gauges are pinned as played.
    #[test]
    fn gauges_are_named_by_the_viewed_preset() {
        let dir = tempfile::tempdir().unwrap();
        let mut preset = rules::tapestry();
        let ids: Vec<String> = preset.gauges.iter().map(|g| g.id.clone()).collect();
        assert!(!ids.is_empty());
        for g in &mut preset.gauges {
            g.id = format!("{}-renamed", g.id);
        }
        let played = report(preset);
        let s = screen(dir.path(), &played);
        let lines = lines_of(&played, &s, false);
        assert_eq!(lines.len(), SUMMARY_LINES);
        assert!(lines.contains(&super::queue_line(QUEUE)), "{lines:?}");
        for id in &ids {
            let named = format!("{}-RENAMED 61.234% NOT CLEAR", id.to_uppercase());
            assert!(
                lines.iter().any(|l| l.contains(&named)),
                "{named} in {lines:?}"
            );
        }
        // Another preset's gauges are named by it.
        let mut what_if = ViewSummary {
            tally: played.tally,
            gauges: [None; SHOWN_GAUGES],
            more_gauges: 0,
            coverage: Coverage::Recorded,
            as_played: false,
            started_at_default: 0b10,
        };
        for g in what_if.gauges.iter_mut().take(2) {
            *g = Some(GaugeEnd {
                value: GaugeValue(5_000_000),
                cleared: true,
            });
        }
        let lr2 = rules::lr2_style();
        let cells = super::gauge_cells(&what_if, Some(&lr2), 37);
        assert_eq!(
            cells,
            [
                format!("{} 50.000% CLEAR", lr2.gauges[0].id.to_uppercase()),
                // Cut within its cell, keeping the value.
                format!(
                    "{}.. 50.000% CLEAR (DEFAULT START)",
                    &lr2.gauges[1].id.to_uppercase()[..5]
                ),
            ]
        );
    }

    /// The view line names the viewed preset, and a what-if's coverage
    /// gets its own line with the time in seconds to three decimals.
    #[test]
    fn what_if_labels_name_the_preset_and_the_time() {
        let mut s = ViewSummary {
            tally: report(rules::tapestry()).tally,
            gauges: [None; SHOWN_GAUGES],
            more_gauges: 0,
            coverage: Coverage::Recorded,
            as_played: true,
            started_at_default: 0,
        };
        let label = |s: &ViewSummary| {
            (
                super::view_label(ViewStatus::Ready(s), "LR2-STYLE"),
                super::coverage_line(ViewStatus::Ready(s)),
            )
        };
        assert_eq!(label(&s), ("AS PLAYED".to_owned(), String::new()));
        s.as_played = false;
        assert_eq!(
            label(&s),
            ("WHAT-IF UNDER LR2-STYLE".to_owned(), String::new())
        );
        s.coverage = Coverage::NoInputAfter(TimeUs(60_180_000));
        assert_eq!(label(&s).1, "NO INPUT AFTER 60.180 S");
        s.coverage = Coverage::EndedMidway(TimeUs(1_649_999));
        assert_eq!(label(&s).1, "ENDED MIDWAY AT 1.649 S, NO CLEAR");
        assert_eq!(
            super::view_label(
                ViewStatus::OverBudget {
                    needed: 9,
                    budget: 1
                },
                "MINE"
            ),
            "TOO LARGE TO RE-EVALUATE UNDER MINE"
        );
        assert_eq!(
            super::view_label(ViewStatus::NothingRecorded, "MINE"),
            "NOTHING WAS RECORDED"
        );
    }

    /// Lines 2 to 8 of a screen whose viewed file broke after reloading,
    /// with the play's own reload error: the strip, the view, the
    /// coverage, the file's error, the two notices and the reload error;
    /// the coverage and the standing error each keep their own line after
    /// the notices roll over.
    #[test]
    fn the_view_lines_show_the_strip_notices_and_reload_error() {
        let dir = tempfile::tempdir().unwrap();
        let presets = dir.path().join("presets");
        std::fs::create_dir_all(&presets).unwrap();
        let mine = presets.join("mine.toml");
        let mut late = rules::tapestry();
        for t in [&mut late.tables.keys, &mut late.tables.scratch] {
            t.windows.bad.late = TimeUs(400_000);
        }
        std::fs::write(&mine, game::preset_to_toml(&late)).unwrap();
        let mut played = report(rules::tapestry());
        let rec = played.record.as_mut().unwrap();
        rec.status.end = replay::EndStatus::PlayedToEnd;
        rec.status.finalized = TimeUs(1_180_000);
        played.reload_error = Some(game::PresetFileError::io(
            presets.join("tapestry-old.toml"),
            std::io::ErrorKind::NotFound,
        ));
        let mut shelf = game::Shelf::open(presets.clone(), "tapestry").unwrap();
        let mut s = ResultScreen::new(&played, &shelf, &chart(), game::RECORDING_MEMORY);
        // tapestry, beatoraja-style, lr2-style, mine: back one.
        s.step(-1, &played, &chart());
        let broken = |text: &str| {
            std::fs::write(&mine, text).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&mine)
                .unwrap()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
                .unwrap();
        };
        broken("windows = [");
        shelf.check();
        s.sync(&played, &shelf, &chart());
        // A second file failing rolls the notices over.
        std::fs::write(presets.join("zz.toml"), "windows = [").unwrap();
        shelf.check();
        s.sync(&played, &shelf, &chart());
        let lines = lines_of(&played, &s, false);
        let error = "MINE.TOML: THE PRESET FILE CANNOT BE READ";
        let got: Vec<&str> = lines[1..8].iter().map(String::as_str).collect();
        assert_eq!(got[0], "< LR2-STYLE  >MINE!<  ZZ! >  4/5");
        assert_eq!(got[1], "WHAT-IF UNDER MINE");
        assert_eq!(got[2], "NO INPUT AFTER 1.180 S");
        assert!(
            got[3].starts_with(&format!("FILE DID NOT RELOAD: {error}")),
            "{got:?}"
        );
        assert!(got[4].starts_with(error), "{got:?}");
        assert!(got[5].starts_with("ZZ.TOML: "), "{got:?}");
        assert_eq!(
            got[6],
            "THIS PLAY USED THE PREVIOUS TAPESTRY: TAPESTRY-OLD.TOML: ENTITY NOT FOUND"
        );
        assert!(
            s.notices().all(|n| matches!(n, Notice::Failed(_))),
            "{:?}",
            s.notices().collect::<Vec<_>>()
        );
        assert!(lines.iter().all(|l| l.len() <= COLUMNS), "{lines:?}");
    }

    /// Long names and errors are cut to the line with a trailing `..`, a
    /// gauge's name within its cell, never the whole row.
    #[test]
    fn long_names_and_errors_are_cut_with_a_marker() {
        let long = "x".repeat(200);
        let cut = super::fit(&long, COLUMNS);
        assert_eq!(cut.len(), COLUMNS);
        assert!(cut.ends_with(".."));
        let mut preset = rules::tapestry();
        preset.gauges[0].id = "g".repeat(60);
        let s = ViewSummary {
            tally: report(rules::tapestry()).tally,
            gauges: [
                Some(GaugeEnd {
                    value: GaugeValue(5_000_000),
                    cleared: false,
                }),
                Some(GaugeEnd {
                    value: GaugeValue(5_000_000),
                    cleared: true,
                }),
                None,
                None,
                None,
                None,
            ],
            more_gauges: 0,
            coverage: Coverage::Recorded,
            as_played: true,
            started_at_default: 0,
        };
        let cells = super::gauge_cells(&s, Some(&preset), 37);
        assert_eq!(cells[0].len(), 37);
        assert!(cells[0].ends_with(".. 50.000% NOT CLEAR"), "{}", cells[0]);
        assert!(
            cells[1].starts_with("SURVIVAL 50.000% CLEAR"),
            "{}",
            cells[1]
        );
    }

    /// A seventh gauge or more shows as a count in place of the sixth.
    #[test]
    fn gauges_past_six_show_a_count() {
        let g = Some(GaugeEnd {
            value: GaugeValue(0),
            cleared: false,
        });
        let s = ViewSummary {
            tally: report(rules::tapestry()).tally,
            gauges: [g; SHOWN_GAUGES],
            more_gauges: 3,
            coverage: Coverage::Recorded,
            as_played: true,
            started_at_default: 0,
        };
        let cells = super::gauge_cells(&s, None, 37);
        assert_eq!(cells.len(), SHOWN_GAUGES);
        assert_eq!(cells[5], "+4 MORE");
        assert_eq!(cells[4], "GAUGE 5 0.000% NOT CLEAR");
    }

    /// A refused file's stem that is not a name is drawn with `?` for
    /// what the bitmap font cannot draw, by its file name only.
    #[test]
    fn refused_stem_is_drawn_ascii_safe() {
        let e = game::PresetFileError::io(
            Path::new("C:/data/presets").join("曲の設定.toml"),
            std::io::ErrorKind::InvalidData,
        );
        let text = super::fit(&super::preset_error_text(&e), COLUMNS);
        assert_eq!(text, "????.TOML: INVALID DATA");
        let io = game::ShelfError::Io {
            path: Path::new("C:/data/presets").to_owned(),
            kind: std::io::ErrorKind::PermissionDenied,
        };
        assert_eq!(super::shelf_error_text(&io), "presets: permission denied");
        let unknown = game::ShelfError::Unknown(game::UnknownPreset("mine".to_owned()));
        assert!(!super::shelf_error_text(&unknown).contains('/'));
    }

    /// Whatever the summary holds, the key hints stay on their line and the
    /// settings lines, the input offset with its keys among them, follow
    /// within the 25 lines the screen draws, after an offset change too.
    #[test]
    fn the_key_hint_line_is_always_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut played = report(rules::tapestry());
        played.end = game::PlayEnd::Aborted(game::AbortCause::InputFailed);
        played.input_error = Some(game::InputWaitError::Gone);
        played.output_error = Some(audio::OutputError::DeviceLost);
        played.record = Some(record(
            &played.preset,
            replay::Incomplete {
                suspend: true,
                ..replay::Incomplete::default()
            },
        ));
        played.result.as_mut().unwrap().recommended_offset = Some(game::RecommendedOffset {
            median: TimeUs(1_000),
            spread: TimeUs(0),
            samples: 40,
        });
        let s = screen(dir.path(), &played);
        let mut page = lines_of(&played, &s, false);
        assert_eq!(
            page[SUMMARY_LINES - 1],
            "BACKSPACE: RETRY  ESCAPE: BACK  LEFT/RIGHT: PRESET"
        );
        page.extend(super::settings_lines(game::Tuning {
            visible_time_ms: 575,
            volume_percent: 100,
            display_offset_us: -12_000,
            input_offset_us: -7_500,
        }));
        assert_eq!(page.len(), 25);
        assert_eq!(
            page[24],
            "F7/F8 INPUT OFFSET -7.5 MS FROM THE NEXT PLAY  F9 OVERLAY"
        );
        assert!(page.iter().all(|l| l.len() <= COLUMNS), "{page:?}");
    }

    /// A play the input thread ended names the cause, the error and, for its
    /// record, the incomplete reason, alone or with others.
    #[test]
    fn an_input_failure_is_named_on_the_end_screen() {
        let dir = tempfile::tempdir().unwrap();
        let preset = rules::tapestry();
        let mut played = report(preset.clone());
        played.end = game::PlayEnd::Aborted(game::AbortCause::InputFailed);
        played.input_error = Some(game::InputWaitError::Gone);
        let lines =
            |played: &game::PlayReport| lines_of(played, &screen(dir.path(), played), false);
        let incomplete = |l: &[String]| {
            l.iter()
                .filter(|l| l.starts_with("RECORD INCOMPLETE"))
                .cloned()
                .collect::<Vec<_>>()
        };
        let shown = lines(&played);
        for line in [
            "RESULT: ENDED MIDWAY (INPUT FAILED)",
            "INPUT: THE INPUT THREAD IS GONE",
        ] {
            assert!(shown.iter().any(|l| l == line), "{line} in {shown:?}");
        }
        assert!(incomplete(&shown).is_empty(), "{shown:?}");
        let only = replay::Incomplete {
            input_failed: true,
            ..replay::Incomplete::default()
        };
        let both = replay::Incomplete {
            suspend: true,
            ..only
        };
        for (reasons, line) in [
            (only, "RECORD INCOMPLETE: INPUT FAILED"),
            (both, "RECORD INCOMPLETE: SUSPEND, INPUT FAILED"),
        ] {
            played.record = Some(record(&preset, reasons));
            assert_eq!(incomplete(&lines(&played)), [line]);
        }
        played.record = Some(record(&preset, replay::Incomplete::default()));
        assert!(incomplete(&lines(&played)).is_empty());
    }

    /// A record of an aborted play with no input, incomplete for `reasons`.
    fn record(preset: &rules::RulePreset, reasons: replay::Incomplete) -> replay::PlayRecord {
        replay::PlayRecord {
            versions: replay::Versions {
                evaluator: rules::Implementation::STANDARD_VERSION,
                app: "0.0.0-test".to_owned(),
                provenance: replay::BuildProvenance::Development,
            },
            chart: replay::ChartRecord::default(),
            preset: replay::PresetRecord::of(preset),
            session_policy: "default".to_owned(),
            devices: replay::DeviceRecord {
                sources: Vec::new(),
                audio_offset: bms_model::TimeUs(0),
                display_offset: bms_model::TimeUs(0),
                wake: replay::WakePolicy::Timer {
                    heartbeat_us: 1_000,
                },
            },
            play_type: replay::PlayType::Full,
            speed: bms_model::Speed::NORMAL,
            start: eval::InputStart {
                opened_at: bms_model::TimeUs(-1_000_000),
                held: Vec::new(),
            },
            events: Vec::new(),
            arrivals: Vec::new(),
            status: replay::Status {
                end: replay::EndStatus::EndedMidway,
                finalized: bms_model::TimeUs(0),
                incomplete: reasons,
            },
        }
    }

    /// P3: an autoplay play's end screen says it was not saved and shows
    /// the result, but neither recommendation line, even with a
    /// recommendation; the same report from a saved session shows both
    /// and no such line.
    #[test]
    fn autoplay_end_screen_says_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let mut played = report(rules::tapestry());
        played.result.as_mut().unwrap().recommended_offset = Some(game::RecommendedOffset {
            median: bms_model::TimeUs(0),
            spread: bms_model::TimeUs(0),
            samples: 40,
        });
        let s = screen(dir.path(), &played);
        let recommends = |l: &[String]| {
            l.iter()
                .filter(|l| {
                    l.contains("RECOMMENDED INPUT OFFSET") || l.contains("MEDIAN DEVIATION")
                })
                .count()
        };
        let auto = lines_of(&played, &s, true);
        assert!(auto.iter().any(|l| l == "AUTOPLAY: NOT SAVED"), "{auto:?}");
        assert!(auto.iter().any(|l| l.starts_with("PERFECT 3")), "{auto:?}");
        assert!(
            auto.iter()
                .any(|l| l == "RE-EVALUATION MATCHES THE LIVE PLAY")
        );
        assert_eq!(recommends(&auto), 0, "{auto:?}");
        let saved = lines_of(&played, &s, false);
        assert!(!saved.iter().any(|l| l.contains("AUTOPLAY")), "{saved:?}");
        assert_eq!(recommends(&saved), 2, "{saved:?}");
    }
}
