//! `tapestry measure play <chart>` (dev-tools, PR17): the play of
//! `tapestry play` under a PresentMon capture of this process, then each
//! saved play's lost refreshes, off-grid and never-displayed counts from
//! the capture over its play span, by the counting rules of design.md
//! "Frame Pacing in Refreshes", added to its `.measure.toml`.
//!
//! PresentMon is an external tool the user installs; it is never
//! downloaded. It is found through the `TAPESTRY_PRESENTMON` setting (the
//! path of its executable) or as `PresentMon.exe` on `PATH`, and runs
//! unelevated with the stage 0 flags (spike `measure.rs`): the user must
//! be in Performance Log Users. Its unelevated `--terminate_on_proc_exit`
//! does not fire, so the capture is stopped by terminating its session.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use render::measure::{SpanCount, count_span};

use crate::measure::Figures;

/// The setting that names PresentMon's executable.
pub const ENV: &str = "TAPESTRY_PRESENTMON";
/// PresentMon's ETW session.
const SESSION: &str = "tapestry_measure";
/// PresentMon needs a moment to start its ETW session.
const SETTLE: Duration = Duration::from_secs(3);
/// How long a stopped capture may take to exit before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// PresentMon's executable: `setting` (the value of [`ENV`]) when set,
/// otherwise `PresentMon.exe` in a directory of `path`.
///
/// # Errors
///
/// A setting that names no file, or no PresentMon anywhere.
pub fn locate(setting: Option<OsString>, path: Option<OsString>) -> anyhow::Result<PathBuf> {
    if let Some(s) = setting.filter(|s| !s.is_empty()) {
        let exe = PathBuf::from(s);
        anyhow::ensure!(
            exe.is_file(),
            "{ENV} names {}, which is not a file",
            exe.display()
        );
        return Ok(exe);
    }
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("PresentMon.exe"))
        .find(|exe| exe.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "PresentMon was not found: set {ENV} to its executable or put PresentMon.exe on \
                 PATH (it is installed by hand, never downloaded)"
            )
        })
}

/// The capture's arguments (the stage 0 flags).
pub fn args(process: &str, csv: &Path) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["--process_name", process, "--output_file"]
        .iter()
        .map(OsString::from)
        .collect();
    a.push(csv.as_os_str().to_owned());
    a.extend(
        [
            "--qpc_time",
            "--track_hybrid_present",
            "--session_name",
            SESSION,
            "--stop_existing_session",
            "--terminate_on_proc_exit",
            "--no_console_stats",
        ]
        .iter()
        .map(OsString::from),
    );
    a
}

/// A running capture; stopped when dropped.
pub struct Capture {
    exe: PathBuf,
    child: Option<Child>,
}

impl Capture {
    /// Starts PresentMon capturing `process` into `csv`, its output into
    /// `log`, and waits until its session runs.
    ///
    /// # Errors
    ///
    /// Why it did not start, or that it exited while starting.
    pub fn start(exe: &Path, process: &str, csv: &Path, log: &Path) -> anyhow::Result<Self> {
        let out = std::fs::File::create(log).with_context(|| log.display().to_string())?;
        let err = out.try_clone()?;
        let child = Command::new(exe)
            .args(args(process, csv))
            .stdout(out)
            .stderr(err)
            .spawn()
            .with_context(|| format!("PresentMon ({}) cannot start", exe.display()))?;
        let mut capture = Self {
            exe: exe.to_owned(),
            child: Some(child),
        };
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if let Some(c) = capture.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                capture.child = None;
                anyhow::bail!(
                    "PresentMon exited while starting ({status}); see {}",
                    log.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(capture)
    }

    /// Stops the capture and waits for PresentMon to finish its file.
    ///
    /// # Errors
    ///
    /// When it had to be killed, or had exited during the run.
    pub fn stop(mut self) -> anyhow::Result<()> {
        self.shut_down()
    }

    fn shut_down(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "PresentMon exited before the run ended ({status}): the capture is incomplete"
            );
        }
        let _ = Command::new(&self.exe)
            .args(["--terminate_existing_session", "--session_name", SESSION])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(anyhow::anyhow!("PresentMon did not stop and was killed"))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.shut_down();
    }
}

/// One present of the captured process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmFrame {
    /// When the present call started, µs on the game's clock.
    pub present_us: i64,
    /// When it reached the screen, µs; `None` when it never did.
    pub display_us: Option<i64>,
    /// `SyncInterval`, when the capture has the column.
    pub sync_interval: Option<u32>,
    /// `AllowsTearing`, when the capture has the column.
    pub tearing: Option<bool>,
}

/// Why a capture could not be read. Its message is the variant as written
/// here, such as `BadRow(3)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PmError {
    /// A required column is missing from the header.
    #[error("MissingColumn")]
    MissingColumn,
    /// A row has the wrong field count or an unreadable value (a file cut
    /// off mid-row), at this 1-based line.
    #[error("BadRow({0})")]
    BadRow(usize),
}

const REQUIRED: [&str; 5] = [
    "Application",
    "CPUStartQPC",
    "MsCPUBusy",
    "MsBetweenDisplayChange",
    "MsUntilDisplayed",
];

/// `NA` or empty is `None`; anything else must be a finite, non-negative
/// duration.
fn duration(s: &str) -> Result<Option<f64>, ()> {
    match s.trim() {
        "NA" | "" => Ok(None),
        v => {
            let x: f64 = v.parse().map_err(|_| ())?;
            if x.is_finite() && x >= 0.0 {
                Ok(Some(x))
            } else {
                Err(())
            }
        }
    }
}

fn ms_to_us(ms: f64) -> i64 {
    (ms * 1_000.0).round() as i64
}

/// Parses a PresentMon 2 CSV (`--qpc_time`), keeping the rows whose
/// `Application` is `app` (case-insensitive), sorted by present time.
/// Columns are found by name. A row whose display change is `NA` is kept
/// as never displayed. `to_us` puts a raw QPC value on the game's clock.
///
/// On a bad row the rows read so far come back with the error.
pub fn parse(text: &str, app: &str, to_us: impl Fn(i64) -> i64) -> (Vec<PmFrame>, Option<PmError>) {
    // PresentMon 2.6 starts its file with a byte order mark.
    let mut lines = text.strip_prefix('\u{feff}').unwrap_or(text).lines();
    let Some(header) = lines.next() else {
        return (Vec::new(), Some(PmError::BadRow(1)));
    };
    let names: Vec<&str> = header.split(',').map(str::trim).collect();
    let col = |n: &str| names.iter().position(|h| *h == n);
    let Some(cols) = REQUIRED
        .iter()
        .map(|n| col(n))
        .collect::<Option<Vec<usize>>>()
    else {
        return (Vec::new(), Some(PmError::MissingColumn));
    };
    let (sync_col, tearing_col) = (col("SyncInterval"), col("AllowsTearing"));
    let mut frames = Vec::new();
    let mut error = None;
    for (i, line) in lines.enumerate() {
        let fields: Vec<&str> = line.split(',').collect();
        let row = (|| {
            if fields.len() != names.len() {
                return Err(());
            }
            let app_ok = fields[cols[0]].trim().eq_ignore_ascii_case(app);
            let start: i64 = fields[cols[1]].trim().parse().map_err(|_| ())?;
            let busy = duration(fields[cols[2]])?;
            let change = duration(fields[cols[3]])?;
            let until = duration(fields[cols[4]])?;
            let flag = |c: Option<usize>| -> Result<Option<u32>, ()> {
                c.map(|c| fields[c].trim().parse().map_err(|_| ()))
                    .transpose()
            };
            let present = (flag(sync_col)?, flag(tearing_col)?.map(|t| t != 0));
            Ok((app_ok, start, busy, change, until, present))
        })();
        let Ok((app_ok, start, busy, change, until, (sync_interval, tearing))) = row else {
            error = Some(PmError::BadRow(i + 2));
            break;
        };
        if !app_ok {
            continue;
        }
        let present_us = to_us(start) + busy.map_or(0, ms_to_us);
        let display_us = change.and(until).map(|ms| present_us + ms_to_us(ms));
        frames.push(PmFrame {
            present_us,
            display_us,
            sync_interval,
            tearing,
        });
    }
    frames.sort_by_key(|f| f.present_us);
    (frames, error)
}

/// The longest the game's present time may follow the capture's
/// submission of that present, µs: the present call and the read of the
/// clock after it.
pub const MATCH_TOLERANCE_US: i64 = 2_000;

/// One play's figures from the capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayCapture {
    /// The counted span, or `None` (N/A) without the evidence or outside
    /// the applicable conditions.
    pub count: Option<SpanCount>,
    /// The capture's presents from the span's predecessor to its
    /// successor, when it has both.
    pub presents: Option<u64>,
    /// The capture's presents in the span never displayed; `None` (N/A)
    /// when the capture does not hold every present the game issued.
    pub never_displayed: Option<u64>,
}

/// Counts the play span `[a, b)` of `fig` over `frames` at one refresh of
/// `period_ns` and k = 1 (design.md "Frame Pacing in Refreshes").
///
/// The evidence must reach from the span's predecessor (the last frame
/// displayed before `a`) to its successor (the first displayed at or after
/// `b`), and every present the game issued from the predecessor's present
/// to the successor's must pair, one to one and in order, with a captured
/// one: the game's time (taken just after its present call returned) at
/// most [`MATCH_TOLERANCE_US`] (or half a refresh, if shorter) after the
/// capture's submission time, and no other captured present that close.
/// An unmatched or ambiguous present, at the boundaries too, makes the span
/// N/A. The count applies only when every one of
/// those presents was made with vsync on every refresh and no tearing, as
/// the capture's `SyncInterval` and `AllowsTearing` and the game's own
/// swap chain configuration both confirm. Otherwise the span is N/A.
pub fn count_play(frames: &[PmFrame], fig: &Figures, period_ns: i64) -> PlayCapture {
    let (a, b) = (fig.span.0.0, fig.span.1.0);
    let na = PlayCapture {
        count: None,
        presents: None,
        never_displayed: None,
    };
    let shown = |f: &&PmFrame| f.display_us.is_some();
    let pred = frames
        .iter()
        .filter(shown)
        .filter(|f| f.display_us.is_some_and(|d| d < a))
        .max_by_key(|f| f.display_us);
    let succ = frames
        .iter()
        .filter(shown)
        .filter(|f| f.display_us.is_some_and(|d| d >= b))
        .min_by_key(|f| f.display_us);
    let (Some(pred), Some(succ)) = (pred, succ) else {
        return na;
    };
    let (from, to) = (pred.present_us, succ.present_us);
    let window: Vec<&PmFrame> = frames
        .iter()
        .filter(|f| (from..=to).contains(&f.present_us))
        .collect();
    let presents = Some(window.len() as u64);
    let tol = MATCH_TOLERANCE_US.min(period_ns / 2_000);
    let next = frames
        .iter()
        .find(|f| f.present_us > to)
        .map(|f| f.present_us);
    let covered = fig.present_times.as_ref().is_some_and(|times| {
        let issued: Vec<i64> = times
            .iter()
            .copied()
            .filter(|&g| g > from && g <= to + tol)
            .collect();
        issued.len() == window.len()
            && issued.iter().enumerate().all(|(i, &g)| {
                let c = window[i].present_us;
                let after = window.get(i + 1).map(|f| f.present_us).or(next);
                let before = i.checked_sub(1).map(|j| window[j].present_us);
                (0..=tol).contains(&(g - c))
                    && after.is_none_or(|n| n > g)
                    && before.is_none_or(|p| g - p > tol)
            })
    });
    if !covered {
        return PlayCapture { presents, ..na };
    }
    let never_displayed = Some(
        window
            .iter()
            .filter(|f| (a..b).contains(&f.present_us) && f.display_us.is_none())
            .count() as u64,
    );
    let applicable = fig
        .presentation
        .is_some_and(render::measure::Presentation::countable)
        && window
            .iter()
            .all(|f| f.sync_interval == Some(1) && f.tearing == Some(false));
    let mut displayed: Vec<i64> = frames
        .iter()
        .filter_map(|f| f.display_us)
        .map(|us| us * 1_000)
        .collect();
    displayed.sort_unstable();
    PlayCapture {
        count: count_span(&displayed, (a * 1_000, b * 1_000), period_ns, 1, applicable),
        presents,
        never_displayed,
    }
}

/// The capture's figures as a TOML table, numbers only (N/A left out),
/// with the game's own present count in the span and the period counted
/// at.
pub fn to_toml(c: &PlayCapture, fig: &Figures, period_ns: i64) -> String {
    let mut out = String::from("\n[presentmon]\n");
    let _ = writeln!(
        out,
        "refresh_period_ns = {period_ns}\nk = 1\ngame_presents = {}",
        fig.presents
    );
    if let Some(s) = c.count {
        let _ = writeln!(
            out,
            "lost = {}\nexpected = {}\nlongest = {}\noff_grid = {}",
            s.lost, s.expected, s.longest, s.off_grid
        );
    }
    if let Some(n) = c.presents {
        let _ = writeln!(out, "presents = {n}");
    }
    if let Some(n) = c.never_displayed {
        let _ = writeln!(out, "never_displayed = {n}");
    }
    out
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use render::measure::Presentation;

    use super::{PlayCapture, PmError, PmFrame, count_play, locate, parse};
    use crate::measure::Figures;

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

    /// A 10 MHz QPC with its origin at 1 000 000 000 ticks.
    fn to_us(ticks: i64) -> i64 {
        (ticks - 1_000_000_000) / 10
    }

    #[test]
    fn parses_rows_filters_other_processes_and_keeps_na_rows() {
        let csv = format!(
            "{HEADER}\n{}\n{}\n{}\n",
            row("tapestry.exe", 1_000_166_667, true),
            row("other.exe", 1_000_200_000, true),
            row("TAPESTRY.EXE", 1_000_333_333, false),
        );
        let (frames, err) = parse(&csv, "tapestry.exe", to_us);
        assert_eq!(err, None);
        // PresentMon 2.6 writes a byte order mark first.
        assert_eq!(
            parse(&format!("\u{feff}{csv}"), "tapestry.exe", to_us),
            (frames.clone(), None)
        );
        // 166 667 ticks = 16 666 µs, plus 1.2 ms of CPU work, then
        // displayed 4 ms later; the NA row is kept, never displayed.
        assert_eq!(
            frames,
            [
                PmFrame {
                    present_us: 16_666 + 1_200,
                    display_us: Some(16_666 + 1_200 + 4_000),
                    sync_interval: Some(1),
                    tearing: Some(false),
                },
                PmFrame {
                    present_us: 33_333 + 1_200,
                    display_us: None,
                    sync_interval: Some(1),
                    tearing: Some(false),
                },
            ]
        );
    }

    #[test]
    fn truncated_row_and_missing_column_are_errors() {
        let full = row("tapestry.exe", 1_000_166_667, true);
        let csv = format!("{HEADER}\n{full}\n{}\n", &full[..40]);
        let (frames, err) = parse(&csv, "tapestry.exe", to_us);
        assert_eq!(frames.len(), 1);
        assert_eq!(err, Some(PmError::BadRow(3)));
        let csv = "Application,CPUStartQPC\ntapestry.exe,1\n";
        assert_eq!(
            parse(csv, "x", to_us),
            (Vec::new(), Some(PmError::MissingColumn))
        );
        assert_eq!(
            parse("", "x", to_us),
            (Vec::new(), Some(PmError::BadRow(1)))
        );
    }

    /// A capture error reads as its variant, after the file it is about.
    #[test]
    fn capture_errors_read_as_their_variant() {
        assert_eq!(PmError::MissingColumn.to_string(), "MissingColumn");
        assert_eq!(PmError::BadRow(3).to_string(), "BadRow(3)");
        assert!(std::error::Error::source(&PmError::BadRow(3)).is_none());
        let e = anyhow::Error::new(PmError::BadRow(3)).context("1.presentmon.csv".to_owned());
        assert_eq!(format!("{e:#}"), "1.presentmon.csv: BadRow(3)");
    }

    #[test]
    fn non_finite_or_negative_durations_are_malformed() {
        for bad in ["inf", "NaN", "-1.0"] {
            let r = row("tapestry.exe", 1_000_166_667, true).replace("1.2000", bad);
            let csv = format!("{HEADER}\n{r}\n");
            assert_eq!(
                parse(&csv, "tapestry.exe", to_us).1,
                Some(PmError::BadRow(2)),
                "{bad}"
            );
        }
        // An unreadable QPC value too.
        let r = row("tapestry.exe", 1, true).replace(",1,NA", ",x,NA");
        let csv = format!("{HEADER}\n{r}\n");
        assert_eq!(
            parse(&csv, "tapestry.exe", to_us).1,
            Some(PmError::BadRow(2))
        );
    }

    /// A play over `span` whose game issued presents at `presents` (µs,
    /// each 300 µs after the capture's present time), with vsync on every
    /// refresh and no tearing.
    fn figures(span: (i64, i64), presents: &[i64]) -> Figures {
        Figures {
            span: (TimeUs(span.0), TimeUs(span.1)),
            presentation: Some(Presentation {
                sync_interval: 1,
                tearing: false,
            }),
            present_times: Some(presents.iter().map(|p| p + 300).collect()),
            ..Figures::default()
        }
    }

    fn frame(present_us: i64, display_us: Option<i64>) -> PmFrame {
        PmFrame {
            present_us,
            display_us,
            sync_interval: Some(1),
            tearing: Some(false),
        }
    }

    const T: i64 = 4_000_000;

    /// Presents 4 ms apart at 250 Hz, each shown 1 ms later, one stalled
    /// a refresh and one never shown: counted over the play span when the
    /// capture holds every present the game issued from the predecessor
    /// to the successor, N/A when it lacks one.
    #[test]
    fn a_play_span_is_counted_only_with_every_present() {
        let mut frames: Vec<PmFrame> = (0..10)
            .map(|k| frame(k * 4_000, Some(k * 4_000 + 1_000)))
            .collect();
        // Present 5 shows a refresh late (at 25 ms, not 21) and present 6
        // never shows: 17 → 25 is two refreshes (one lost), 25 → 29 one.
        frames[5].display_us = Some(25_000);
        frames[6].display_us = None;
        let issued: Vec<i64> = (0..10).map(|k| k * 4_000).collect();
        // Span [2 ms, 30 ms): the predecessor is present 0 (shown at 1 ms),
        // the successor present 8 (shown at 33 ms): 9 presents.
        let c = count_play(&frames, &figures((2_000, 30_000), &issued), T);
        assert_eq!((c.presents, c.never_displayed), (Some(9), Some(1)));
        let s = c.count.expect("counted");
        // Grid 1 + 4j ms inside [2, 30): 5, 9, ..., 29.
        assert_eq!((s.lost, s.longest, s.off_grid, s.expected), (1, 1, 0, 7));
        // The game issued one more present than the capture holds.
        let mut more = issued.clone();
        more.push(14_000);
        let c: PlayCapture = count_play(&frames, &figures((2_000, 30_000), &more), T);
        assert_eq!((c.count, c.never_displayed), (None, None));
    }

    /// The review's case: T = 4 ms, span [10, 25), frames shown at 9, 13,
    /// 17, 21 and 25 ms. A present the game issued at 8 ms, before the
    /// span, is missing from the capture: the span is N/A, although the
    /// capture holds every present issued inside the span itself.
    #[test]
    fn a_present_missing_before_the_span_makes_it_na() {
        let frames: Vec<PmFrame> = [5, 9, 13, 17, 21]
            .map(|p| frame(p * 1_000, Some(p * 1_000 + 4_000)))
            .to_vec();
        let all = [5_000, 9_000, 13_000, 17_000, 21_000];
        let c = count_play(&frames, &figures((10_000, 25_000), &all), T);
        assert_eq!(c.count.map(|s| s.lost), Some(0));
        let missing = [5_000, 8_000, 9_000, 13_000, 17_000, 21_000];
        let c = count_play(&frames, &figures((10_000, 25_000), &missing), T);
        assert_eq!((c.count, c.never_displayed), (None, None));
        assert_eq!(c.presents, Some(5));
    }

    /// The confirm review's case: T = 4 ms, captured presents at 0, 4, 12
    /// and 16 ms (shown 1 ms later), the game's at 0.3, 4.3, 8.3, 12.3 and
    /// 19 ms, span [2, 14). Counted in aggregate the window holds four and
    /// four, but 8.3 has no captured present and 16 none issued within the
    /// tolerance: N/A.
    #[test]
    fn presents_pair_one_to_one() {
        let frames: Vec<PmFrame> = [0, 4, 12, 16]
            .map(|p| frame(p * 1_000, Some(p * 1_000 + 1_000)))
            .to_vec();
        let mut fig = figures((2_000, 14_000), &[]);
        fig.present_times = Some(vec![300, 4_300, 8_300, 12_300, 19_000]);
        let c = count_play(&frames, &fig, T);
        assert_eq!((c.count, c.never_displayed), (None, None));
        // Paired one to one, it counts: 12 ms is 2 refreshes after 4, and
        // the game's 16 ms present completes at 16.3.
        fig.present_times = Some(vec![300, 4_300, 12_300, 16_300]);
        let c = count_play(&frames, &fig, T);
        assert_eq!(c.count.map(|s| s.lost), Some(1));
        // A game present that completes at 12.1 ms, after the capture's
        // 12 ms submission, could be either: ambiguous, N/A.
        let mut close = frames.clone();
        close.insert(2, frame(11_000, None));
        fig.present_times = Some(vec![300, 4_300, 11_300, 12_100, 16_300]);
        assert_eq!(count_play(&close, &fig, T).count, None);
    }

    /// An empty capture, or a game without present times, has no figure.
    #[test]
    fn an_empty_or_unchecked_capture_has_no_figures() {
        let none = PlayCapture {
            count: None,
            presents: None,
            never_displayed: None,
        };
        assert_eq!(count_play(&[], &figures((0, 10_000), &[1_000]), T), none);
        let frames: Vec<PmFrame> = (0..5)
            .map(|k| frame(k * 4_000, Some(k * 4_000 + 1_000)))
            .collect();
        let unchecked = Figures {
            present_times: None,
            ..figures((2_000, 10_000), &[])
        };
        let c = count_play(&frames, &unchecked, T);
        assert_eq!((c.count, c.never_displayed), (None, None));
        let toml = super::to_toml(&c, &unchecked, T);
        assert!(!toml.contains("never_displayed"), "{toml}");
        assert!(!toml.contains("lost"), "{toml}");
    }

    /// Counting applies only to vsync on every refresh without tearing,
    /// confirmed by both the capture and the game: a tearing present, a
    /// sync interval of 2, a capture without those columns, or a game
    /// that does not say how it presents make the span N/A.
    #[test]
    fn counting_applies_only_where_confirmed() {
        let frames: Vec<PmFrame> = (0..10)
            .map(|k| frame(k * 4_000, Some(k * 4_000 + 1_000)))
            .collect();
        let issued: Vec<i64> = (0..10).map(|k| k * 4_000).collect();
        let fig = figures((2_000, 30_000), &issued);
        assert!(count_play(&frames, &fig, T).count.is_some());
        let changed = |f: fn(&mut PmFrame)| {
            let mut v = frames.clone();
            f(&mut v[4]);
            count_play(&v, &fig, T)
        };
        for c in [
            changed(|f| f.tearing = Some(true)),
            changed(|f| f.sync_interval = Some(2)),
            changed(|f| f.sync_interval = None),
            changed(|f| f.tearing = None),
        ] {
            assert_eq!(c.count, None);
            assert_eq!(c.never_displayed, Some(0));
        }
        let unknown = Figures {
            presentation: None,
            ..fig.clone()
        };
        assert_eq!(count_play(&frames, &unknown, T).count, None);
    }

    #[test]
    fn presentmon_is_found_by_its_setting_or_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("PresentMon-2.6.0-x64.exe");
        std::fs::write(&named, b"").unwrap();
        let found = locate(Some(named.clone().into_os_string()), None).unwrap();
        assert_eq!(found, named);
        let e = locate(Some(dir.path().join("none.exe").into_os_string()), None).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("not a file"), "{e}");
        let on_path = dir.path().join("PresentMon.exe");
        std::fs::write(&on_path, b"").unwrap();
        let path = std::env::join_paths([dir.path().join("empty"), dir.path().to_owned()]).unwrap();
        assert_eq!(locate(Some("".into()), Some(path)).unwrap(), on_path);
        let e = locate(None, Some(dir.path().join("empty").into_os_string())).unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("PresentMon was not found"), "{e}");
    }
}
