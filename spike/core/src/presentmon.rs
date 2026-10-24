//! PresentMon 2.6 CSV parser (design §3.2).
//!
//! Uses the `csv` crate with serde, matching columns by header name, so
//! column order and extra columns do not matter. Expected invocation:
//! `--process_name tapestry-spike.exe --qpc_time`, which names the start
//! column `CPUStartQPC`. Columns used:
//!
//! - `Application`: rows of other processes are ignored.
//! - `CPUStartQPC`, `MsCPUBusy`: present time = CPU start + CPU busy (the
//!   time from CPU start to the Present call).
//! - `MsUntilDisplayed`: display time = present time + this.
//! - `MsBetweenDisplayChange`: presented interval; `NA` = not displayed.
//! - `MsGPUBusy`: GPU time.
//! - `PresentMode` (optional): how the frame reached the screen.
//!
//! A row is never dropped for `NA`; it becomes a not-displayed frame.

use crate::run::{PmFrame, PresentMode};
use crate::time::{ms_to_us, qpc_to_us};
use serde::{Deserialize, Deserializer};

/// Raw QPC origin and frequency of the run (from `private/origin.json`).
#[derive(Clone, Copy, Debug)]
pub struct QpcClock {
    pub origin: i64,
    pub freq: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PmError {
    /// A required column is missing from the header.
    MissingColumn,
    /// A row has the wrong field count or an unparsable value (for example
    /// a file cut off mid-row). Holds the 1-based line number.
    BadRow(u64),
}

impl PmError {
    /// Fixed message for `RunInfo::presentmon_error` (no file content).
    pub fn message(&self) -> String {
        match self {
            Self::MissingColumn => "required PresentMon column missing".into(),
            Self::BadRow(line) => format!("malformed or truncated PresentMon row at line {line}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Row {
    application: String,
    #[serde(rename = "CPUStartQPC")]
    cpu_start_qpc: i64,
    #[serde(rename = "MsCPUBusy", deserialize_with = "na")]
    ms_cpu_busy: Option<f64>,
    #[serde(rename = "MsGPUBusy", deserialize_with = "na")]
    ms_gpu_busy: Option<f64>,
    #[serde(deserialize_with = "na")]
    ms_between_display_change: Option<f64>,
    #[serde(deserialize_with = "na")]
    ms_until_displayed: Option<f64>,
    #[serde(default)]
    present_mode: Option<String>,
}

/// `NA` or empty is `None`; anything else must be a finite, non-negative
/// duration (`inf`, `NaN` and negative values make the row malformed).
fn na<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    let s = String::deserialize(d)?;
    match s.trim() {
        "NA" | "" => Ok(None),
        v => {
            let x: f64 = v.parse().map_err(serde::de::Error::custom)?;
            if x.is_finite() && x >= 0.0 {
                Ok(Some(x))
            } else {
                Err(serde::de::Error::custom("non-finite or negative duration"))
            }
        }
    }
}

const REQUIRED: [&str; 6] = [
    "Application",
    "CPUStartQPC",
    "MsCPUBusy",
    "MsGPUBusy",
    "MsBetweenDisplayChange",
    "MsUntilDisplayed",
];

/// Parses a PresentMon CSV, keeping rows whose `Application` equals `app`
/// (case-insensitive), sorted by present time.
///
/// On a malformed row the rows parsed so far are returned with the error,
/// so the report can show what was captured while V4 marks the run invalid.
pub fn parse(csv_text: &[u8], app: &str, clock: QpcClock) -> (Vec<PmFrame>, Option<PmError>) {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(false)
        .from_reader(csv_text);
    let headers = match rdr.headers() {
        Ok(h) => h.clone(),
        Err(_) => return (Vec::new(), Some(PmError::BadRow(1))),
    };
    if !REQUIRED.iter().all(|c| headers.iter().any(|h| h == *c)) {
        return (Vec::new(), Some(PmError::MissingColumn));
    }
    let mut frames = Vec::new();
    let mut error = None;
    for (i, rec) in rdr.deserialize::<Row>().enumerate() {
        let row = match rec {
            Ok(r) => r,
            Err(_) => {
                error = Some(PmError::BadRow(i as u64 + 2));
                break;
            }
        };
        if !row.application.eq_ignore_ascii_case(app) {
            continue;
        }
        let present_us = qpc_to_us(row.cpu_start_qpc, clock.origin, clock.freq)
            + row.ms_cpu_busy.map_or(0, ms_to_us);
        let displayed = row.ms_between_display_change.is_some();
        frames.push(PmFrame {
            present_us,
            display_us: if displayed {
                row.ms_until_displayed.map(|ms| present_us + ms_to_us(ms))
            } else {
                None
            },
            display_change_ms: row.ms_between_display_change,
            gpu_busy_ms: row.ms_gpu_busy,
            present_mode: row.present_mode.as_deref().map(PresentMode::parse),
        });
    }
    frames.sort_by_key(|f| f.present_us);
    (frames, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Application,ProcessID,SwapChainAddress,PresentRuntime,SyncInterval,PresentFlags,AllowsTearing,PresentMode,CPUStartQPC,MsBetweenSimulationStart,MsPCLatency,MsBetweenPresents,MsBetweenAppStart,MsCPUBusy,MsCPUWait,MsInPresentAPI,MsGPULatency,MsGPUTime,MsGPUBusy,MsGPUWait,MsVideoBusy,MsAnimationError,AnimationTime,MsFlipDelay,DisplayLatency,DisplayedTime,MsUntilDisplayed,MsBetweenDisplayChange";

    fn row(app: &str, qpc: i64, displayed: bool) -> String {
        let (until, change, shown) = if displayed {
            ("4.0000", "16.6667", "16.6667")
        } else {
            ("NA", "NA", "NA")
        };
        format!(
            "{app},1234,0x0000000000000001,DXGI,1,0,0,Hardware: Independent Flip,{qpc},NA,NA,16.6667,16.6667,1.2000,15.0000,0.1000,0.5000,1.1000,1.0500,0.0000,0.0000,NA,NA,NA,NA,{shown},{until},{change}"
        )
    }

    const CLOCK: QpcClock = QpcClock {
        origin: 1_000_000_000,
        freq: 10_000_000,
    };

    #[test]
    fn parses_rows_filters_other_processes_and_keeps_na_rows() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            row("tapestry-spike.exe", 1_000_166_667, true),
            row("other.exe", 1_000_200_000, true),
            row("tapestry-spike.exe", 1_000_333_333, false),
        );
        let (frames, err) = parse(csv.as_bytes(), "tapestry-spike.exe", CLOCK);
        assert_eq!(err, None);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].present_us, 16_666 + 1_200);
        assert_eq!(frames[0].display_us, Some(16_666 + 1_200 + 4_000));
        assert_eq!(frames[0].display_change_ms, Some(16.6667));
        assert_eq!(frames[0].gpu_busy_ms, Some(1.05));
        assert_eq!(
            frames[0].present_mode,
            Some(PresentMode::HardwareIndependentFlip)
        );
        // (d) NA row kept as a not-displayed frame.
        assert_eq!(frames[1].display_us, None);
        assert_eq!(frames[1].display_change_ms, None);
    }

    #[test]
    fn truncated_row_and_missing_column_are_errors() {
        let full = row("tapestry-spike.exe", 1_000_166_667, true);
        let csv = format!("{HEADER}\n{full}\n{}\n", &full[..40]);
        let (frames, err) = parse(csv.as_bytes(), "tapestry-spike.exe", CLOCK);
        assert_eq!(frames.len(), 1);
        assert_eq!(err, Some(PmError::BadRow(3)));
        let csv = "Application,CPUStartQPC\ntapestry-spike.exe,1\n";
        assert_eq!(
            parse(csv.as_bytes(), "x", CLOCK).1,
            Some(PmError::MissingColumn)
        );
    }

    #[test]
    fn non_finite_or_negative_durations_are_malformed() {
        for bad in ["inf", "NaN", "-1.0"] {
            let r = row("tapestry-spike.exe", 1_000_166_667, true).replace("1.0500", bad);
            let csv = format!(
                "{HEADER}
{r}
"
            );
            assert_eq!(
                parse(csv.as_bytes(), "tapestry-spike.exe", CLOCK).1,
                Some(PmError::BadRow(2)),
                "{bad}"
            );
        }
    }
}
