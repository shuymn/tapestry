//! The end screen: the play's summary as text lines for the bitmap font,
//! numbers from the re-evaluation of the saved record (`ReEvaluate`).

use std::time::Duration;

use game::{AbortCause, Chain, PlayEnd, PlayReport, Tuning};
use rules::GaugeValue;

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

/// The end screen's lines, the gauges named by the preset the play was
/// evaluated under. `input_offset_us` is the input offset the play
/// used, to which the recommendation adds the median deviation. An
/// `autoplay` play (design.md "Autoplay") says it was not saved in place
/// of the saved line and recommends no input offset: its deviations say
/// nothing about the player's.
pub fn lines(
    report: &PlayReport,
    input_offset_us: i64,
    times: LoadTimes,
    queue: QueueLine,
    hints: &Hints,
    autoplay: bool,
) -> Vec<String> {
    let mut l = Vec::new();
    l.push(match report.end {
        PlayEnd::Completed => "RESULT: PLAYED TO END".to_owned(),
        PlayEnd::Aborted(c) => format!("RESULT: ENDED MIDWAY ({})", cause(c)),
    });
    l.push(String::new());
    let summary = report.result.as_ref();
    match summary.map(|s| &s.evaluation) {
        Some(Ok(e)) => {
            let t = &e.tally;
            let c = &t.counts;
            l.push(format!(
                "PERFECT {}  GREAT {}  GOOD {}  BAD {}  POOR {}",
                c.perfect, c.great, c.good, c.bad, c.poor
            ));
            l.push(format!(
                "EX SCORE {}  MAX COMBO {}  BP {}",
                t.ex_score, t.max_combo, t.bp
            ));
            l.push(format!("EMPTY POOR {}", t.empty_poor));
            for (g, rule) in e.gauges.iter().zip(&report.preset.gauges) {
                let v = g.value.0;
                l.push(format!(
                    "GAUGE {} {}.{:03}%  {}",
                    rule.id.to_uppercase(),
                    v / GaugeValue::PERCENT,
                    v % GaugeValue::PERCENT / 100,
                    if g.cleared { "CLEAR" } else { "NOT CLEAR" }
                ));
            }
        }
        Some(Err(e)) => l.push(format!("NO RE-EVALUATION: {}", Chain(e)).to_uppercase()),
        None => l.push("NOTHING WAS RECORDED".to_owned()),
    }
    if let Some(s) = summary {
        l.push(
            if s.matches_live {
                "RE-EVALUATION MATCHES THE LIVE PLAY"
            } else {
                "RE-EVALUATION DIFFERS FROM THE LIVE PLAY"
            }
            .to_owned(),
        );
    }
    if let Some(s) = summary.filter(|_| !autoplay) {
        l.push(match s.recommended_offset {
            Some(r) => format!(
                "RECOMMENDED INPUT OFFSET {} MS (NOW {} MS)",
                ms(input_offset_us.saturating_add(r.median.0)),
                ms(input_offset_us)
            ),
            None => format!(
                "RECOMMENDED INPUT OFFSET: NEEDS {} JUDGED PRESSES",
                game::MIN_OFFSET_SAMPLES
            ),
        });
        if let Some(r) = s.recommended_offset {
            l.push(format!(
                "  MEDIAN DEVIATION {} MS, SPREAD {} MS, {} PRESSES",
                ms(r.median.0),
                ms(r.spread.0),
                r.samples
            ));
        }
    }
    if let Some(line) = report
        .record
        .as_ref()
        .and_then(|rec| incomplete_line(rec.status.incomplete))
    {
        l.push(line);
    }
    match &report.saved {
        Some(Ok(r)) => {
            let name = r
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().to_uppercase());
            l.push(format!("SAVED AS RECORD {name}"));
        }
        Some(Err(e)) => l.push(format!("NOT SAVED: {}", Chain(e)).to_uppercase()),
        None if autoplay => l.push("AUTOPLAY: NOT SAVED".to_owned()),
        None => {}
    }
    if let Some(e) = report.output_error {
        l.push(format!("AUDIO: {e}").to_uppercase());
    }
    if let Some(e) = report.input_error {
        l.push(format!("INPUT: {e}").to_uppercase());
    }
    l.push(queue_line(queue));
    let ms = |d: Option<Duration>| d.map_or_else(|| "-".to_owned(), |d| d.as_millis().to_string());
    l.push(format!(
        "LOAD {} MS  RETRY {} MS",
        ms(times.first),
        ms(times.retry)
    ));
    l.push(String::new());
    l.push(format!("{}: RETRY    {}: QUIT", hints.retry, hints.quit));
    l
}

/// The settings keys and their values, under the summary. The input offset
/// applies from the next play (design.md "Offset Changes and the Watermark") of
/// devices; autoplay plays with 0 whatever it is.
pub fn settings_lines(t: Tuning) -> Vec<String> {
    vec![
        String::new(),
        format!("F1/F2 HI-SPEED: VISIBLE TIME {} MS", t.visible_time_ms),
        format!(
            "F3/F4 VOLUME {}%    F5/F6 DISPLAY OFFSET {} MS",
            t.volume_percent,
            ms(t.display_offset_us)
        ),
        format!(
            "F7/F8 INPUT OFFSET {} MS FROM THE NEXT PLAY",
            ms(t.input_offset_us)
        ),
        "F9 MEASUREMENT OVERLAY IN PLAY".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{key_name, ms};

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
                "",
                "F1/F2 HI-SPEED: VISIBLE TIME 575 MS",
                "F3/F4 VOLUME 35%    F5/F6 DISPLAY OFFSET -2.0 MS",
                "F7/F8 INPUT OFFSET +4.0 MS FROM THE NEXT PLAY",
                "F9 MEASUREMENT OVERLAY IN PLAY",
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

    /// A completed play's report under `preset`, with one gauge result per
    /// preset gauge at 61.234 %.
    fn report(preset: rules::RulePreset) -> game::PlayReport {
        use eval::{Evaluation, GaugeResult, Tally};
        use rules::{GaugeValue, PerJudgment};
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
            record: None,
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

    #[test]
    fn gauges_are_named_by_the_preset_the_play_used() {
        let mut preset = rules::custom();
        let ids: Vec<String> = preset.gauges.iter().map(|g| g.id.clone()).collect();
        assert!(!ids.is_empty());
        for g in &mut preset.gauges {
            g.id = format!("{}-renamed", g.id);
        }
        let hints = super::Hints::new(Some(0x0E), Some(0x01));
        let queue = super::QueueLine {
            played: Some(2),
            steady_underruns: 0,
            next: 2,
        };
        let lines = super::lines(
            &report(preset),
            0,
            super::LoadTimes::default(),
            queue,
            &hints,
            false,
        );
        assert!(lines.contains(&super::queue_line(queue)), "{lines:?}");
        for id in &ids {
            let named = format!("GAUGE {}-RENAMED 61.234%", id.to_uppercase());
            assert!(
                lines.iter().any(|l| l.starts_with(&named)),
                "{named} in {lines:?}"
            );
        }
        assert_eq!(lines.last().unwrap(), "BACKSPACE: RETRY    ESCAPE: QUIT");
    }

    /// A play the input thread ended names the cause, the error and, for its
    /// record, the incomplete reason, alone or with others.
    #[test]
    fn an_input_failure_is_named_on_the_end_screen() {
        let preset = rules::custom();
        let mut played = report(preset.clone());
        played.end = game::PlayEnd::Aborted(game::AbortCause::InputFailed);
        played.input_error = Some(game::InputWaitError::Gone);
        let hints = super::Hints::new(Some(0x0E), Some(0x01));
        let queue = super::QueueLine {
            played: Some(2),
            steady_underruns: 0,
            next: 2,
        };
        let lines = |played: &game::PlayReport| {
            super::lines(played, 0, super::LoadTimes::default(), queue, &hints, false)
        };
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
                evaluator: 1,
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
        let mut played = report(rules::custom());
        played.result.as_mut().unwrap().recommended_offset = Some(game::RecommendedOffset {
            median: bms_model::TimeUs(0),
            spread: bms_model::TimeUs(0),
            samples: 40,
        });
        let hints = super::Hints::new(Some(0x0E), Some(0x01));
        let queue = super::QueueLine {
            played: Some(2),
            steady_underruns: 0,
            next: 2,
        };
        let lines = |autoplay| {
            super::lines(
                &played,
                0,
                super::LoadTimes::default(),
                queue,
                &hints,
                autoplay,
            )
        };
        let recommends = |l: &[String]| {
            l.iter()
                .filter(|l| {
                    l.contains("RECOMMENDED INPUT OFFSET") || l.contains("MEDIAN DEVIATION")
                })
                .count()
        };
        let auto = lines(true);
        assert!(auto.iter().any(|l| l == "AUTOPLAY: NOT SAVED"), "{auto:?}");
        assert!(auto.iter().any(|l| l.starts_with("PERFECT 3")), "{auto:?}");
        assert!(
            auto.iter()
                .any(|l| l == "RE-EVALUATION MATCHES THE LIVE PLAY")
        );
        assert_eq!(recommends(&auto), 0, "{auto:?}");
        let saved = lines(false);
        assert!(!saved.iter().any(|l| l.contains("AUTOPLAY")), "{saved:?}");
        assert_eq!(recommends(&saved), 2, "{saved:?}");
    }
}
