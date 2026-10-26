//! The end screen: the play's summary as text lines for the bitmap font,
//! numbers from the re-evaluation of the saved record (`ReEvaluate`).

use std::time::Duration;

use game::{AbortCause, PlayEnd, PlayReport};
use rules::{GaugeValue, RulePreset};

/// How long loading took, shown on the end screen (D11).
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTimes {
    /// From the start of the process to the first countdown.
    pub first: Duration,
    /// From the latest retry to its countdown.
    pub retry: Option<Duration>,
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
    }
}

/// The end screen's lines. `input_offset_us` is the input offset the play
/// used, to which the recommendation adds the median deviation.
pub fn lines(
    report: &PlayReport,
    preset: &RulePreset,
    input_offset_us: i64,
    times: LoadTimes,
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
    let retry = times
        .retry
        .map_or_else(|| "-".to_owned(), |d| d.as_millis().to_string());
    l.push(format!(
        "LOAD {} MS  RETRY {retry} MS",
        times.first.as_millis()
    ));
    l.push(String::new());
    l.push("BACKSPACE: RETRY    ESCAPE: QUIT".to_owned());
    l
}

#[cfg(test)]
mod tests {
    use super::ms;

    #[test]
    fn milliseconds_keep_the_sign_and_truncate_tenths() {
        assert_eq!(ms(0), "+0.0");
        assert_eq!(ms(3_249), "+3.2");
        assert_eq!(ms(-3_249), "-3.2");
        assert_eq!(ms(-999), "-0.9");
        assert_eq!(ms(12_000), "+12.0");
    }
}
