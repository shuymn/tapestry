//! The end screen: the play's summary as text lines for the bitmap font,
//! numbers from the re-evaluation of the saved record (`ReEvaluate`).

use std::time::Duration;

use game::{AbortCause, PlayEnd, PlayReport};
use rules::{GaugeValue, RulePreset};

/// How long loading took, shown on the end screen (D11).
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTimes {
    /// From the start of the process to the first play's first frame.
    pub first: Option<Duration>,
    /// From the latest retry's receipt to its play's first frame.
    pub retry: Option<Duration>,
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
    }
}

/// The end screen's lines. `input_offset_us` is the input offset the play
/// used, to which the recommendation adds the median deviation.
pub fn lines(
    report: &PlayReport,
    preset: &RulePreset,
    input_offset_us: i64,
    times: LoadTimes,
    hints: &Hints,
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
            l.push(format!(
                "EMPTY POOR {}  LN BREAKS {}",
                t.empty_poor, t.ln_breaks
            ));
            for (g, rule) in e.gauges.iter().zip(&preset.gauges) {
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
        Some(Err(e)) => l.push(format!("NO RE-EVALUATION: {e}").to_uppercase()),
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
    if let Some(rec) = &report.record {
        let i = rec.status.incomplete;
        if i.any() {
            let mut why = Vec::new();
            if i.overflow {
                why.push("OVERFLOW");
            }
            if i.source_limit {
                why.push("SOURCE LIMIT");
            }
            if i.suspend {
                why.push("SUSPEND");
            }
            l.push(format!("RECORD INCOMPLETE: {}", why.join(", ")));
        }
    }
    match &report.saved {
        Some(Ok(r)) => {
            let name = r
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().to_uppercase());
            l.push(format!("SAVED AS RECORD {name}"));
        }
        Some(Err(e)) => l.push(format!("NOT SAVED: {e}").to_uppercase()),
        None => {}
    }
    if let Some(e) = report.output_error {
        l.push(format!("AUDIO: {e}").to_uppercase());
    }
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
    fn milliseconds_keep_the_sign_and_truncate_tenths() {
        assert_eq!(ms(0), "+0.0");
        assert_eq!(ms(3_249), "+3.2");
        assert_eq!(ms(-3_249), "-3.2");
        assert_eq!(ms(-999), "-0.9");
        assert_eq!(ms(12_000), "+12.0");
    }
}
