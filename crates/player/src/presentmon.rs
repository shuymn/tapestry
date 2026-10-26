//! `tapestry measure play <chart>` (dev-tools, PR17): the play of
//! `tapestry play` under a PresentMon capture of this process, then each
//! saved play's lost refreshes, off-grid and never-displayed counts from
//! the capture over its play span, by the counting rules of design.md
//! "Frame Pacing in Refreshes", added to its `.measure.toml`.
//!
//! PresentMon is an external tool the owner installs; it is never
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
pub fn locate(setting: Option<OsString>, path: Option<OsString>) -> Result<PathBuf, String> {
    if let Some(s) = setting.filter(|s| !s.is_empty()) {
        let exe = PathBuf::from(s);
        return if exe.is_file() {
            Ok(exe)
        } else {
            Err(format!(
                "{ENV} names {}, which is not a file",
                exe.display()
            ))
        };
    }
    path.iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join("PresentMon.exe"))
        .find(|exe| exe.is_file())
        .ok_or_else(|| {
            format!(
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
    pub fn start(exe: &Path, process: &str, csv: &Path, log: &Path) -> Result<Self, String> {
        let out = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let child = Command::new(exe)
            .args(args(process, csv))
            .stdout(out)
            .stderr(err)
            .spawn()
            .map_err(|e| format!("PresentMon ({}) cannot start: {e}", exe.display()))?;
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
                return Err(format!(
                    "PresentMon exited while starting ({status}); see {}",
                    log.display()
                ));
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
    pub fn stop(mut self) -> Result<(), String> {
        self.shut_down()
    }

    fn shut_down(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "PresentMon exited before the run ended ({status}): the capture is incomplete"
            ));
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
        Err("PresentMon did not stop and was killed".to_owned())
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
}

/// Why a capture could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmError {
    /// A required column is missing from the header.
    MissingColumn,
    /// A row has the wrong field count or an unreadable value (a file cut
    /// off mid-row), at this 1-based line.
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
    let mut lines = text.lines();
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
            Ok((app_ok, start, busy, change, until))
        })();
        let Ok((app_ok, start, busy, change, until)) = row else {
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
        });
    }
    frames.sort_by_key(|f| f.present_us);
    (frames, error)
}

/// One play's figures from the capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayCapture {
    /// The counted span, or `None` (N/A) without the evidence.
    pub count: Option<SpanCount>,
    /// The capture's presents in the span.
    pub presents: u64,
    /// Those never displayed.
    pub never_displayed: u64,
}

/// Counts the play span of `fig` over `frames` at refresh rate
/// `refresh_mhz` and k = 1. The capture covers the play only when it holds
/// as many presents in the span as the game issued.
pub fn count_play(frames: &[PmFrame], fig: &Figures, refresh_mhz: u32) -> PlayCapture {
    let (a, b) = (fig.span.0.0, fig.span.1.0);
    let in_span: Vec<&PmFrame> = frames
        .iter()
        .filter(|f| (a..b).contains(&f.present_us))
        .collect();
    let presents = in_span.len() as u64;
    let never_displayed = in_span.iter().filter(|f| f.display_us.is_none()).count() as u64;
    let mut displayed: Vec<i64> = frames
        .iter()
        .filter_map(|f| f.display_us)
        .map(|us| us * 1_000)
        .collect();
    displayed.sort_unstable();
    let period_ns = 1_000_000_000_000 / i64::from(refresh_mhz.max(1));
    let covered = presents > 0 && presents == fig.presents;
    PlayCapture {
        count: count_span(&displayed, (a * 1_000, b * 1_000), period_ns, 1, covered),
        presents,
        never_displayed,
    }
}

/// The capture's figures as a TOML table, numbers only (N/A left out).
pub fn to_toml(c: &PlayCapture, refresh_mhz: u32) -> String {
    let mut out = String::from("\n[presentmon]\n");
    let _ = writeln!(out, "refresh_mhz = {refresh_mhz}\nk = 1");
    if let Some(s) = c.count {
        let _ = writeln!(
            out,
            "lost = {}\nexpected = {}\nlongest = {}\noff_grid = {}",
            s.lost, s.expected, s.longest, s.off_grid
        );
    }
    let _ = writeln!(
        out,
        "presents = {}\nnever_displayed = {}",
        c.presents, c.never_displayed
    );
    out
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

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
        // 166 667 ticks = 16 666 µs, plus 1.2 ms of CPU work, then
        // displayed 4 ms later; the NA row is kept, never displayed.
        assert_eq!(
            frames,
            [
                PmFrame {
                    present_us: 16_666 + 1_200,
                    display_us: Some(16_666 + 1_200 + 4_000),
                },
                PmFrame {
                    present_us: 33_333 + 1_200,
                    display_us: None,
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

    fn figures(span: (i64, i64), presents: u64) -> Figures {
        Figures {
            span: (TimeUs(span.0), TimeUs(span.1)),
            presents,
            ..Figures::default()
        }
    }

    /// Presents 4 ms apart at 250 Hz, each shown 1 ms later, one stalled
    /// a refresh and one never shown: counted over the play span when the
    /// capture holds every present the game issued, N/A when it lacks one.
    #[test]
    fn a_play_span_is_counted_only_with_every_present() {
        let mut frames: Vec<PmFrame> = (0..10)
            .map(|k| PmFrame {
                present_us: k * 4_000,
                display_us: Some(k * 4_000 + 1_000),
            })
            .collect();
        // Present 5 shows a refresh late (at 25 ms, not 21) and present 6
        // never shows: 17 → 25 is two refreshes (one lost), 25 → 29 one.
        frames[5].display_us = Some(25_000);
        frames[6].display_us = None;
        // Span [2 ms, 30 ms): presents 1 to 7, the predecessor shown at 1
        // ms, the successor at 33 ms.
        let c = count_play(&frames, &figures((2_000, 30_000), 7), 250_000);
        assert_eq!((c.presents, c.never_displayed), (7, 1));
        let s = c.count.expect("counted");
        // Grid 1 + 4j ms inside [2, 30): 5, 9, ..., 29.
        assert_eq!((s.lost, s.longest, s.off_grid, s.expected), (1, 1, 0, 7));
        // The game issued one more present than the capture holds.
        let c: PlayCapture = count_play(&frames, &figures((2_000, 30_000), 8), 250_000);
        assert_eq!(c.count, None);
    }

    #[test]
    fn presentmon_is_found_by_its_setting_or_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("PresentMon-2.6.0-x64.exe");
        std::fs::write(&named, b"").unwrap();
        let found = locate(Some(named.clone().into_os_string()), None).unwrap();
        assert_eq!(found, named);
        let e = locate(Some(dir.path().join("none.exe").into_os_string()), None).unwrap_err();
        assert!(e.contains("not a file"), "{e}");
        let on_path = dir.path().join("PresentMon.exe");
        std::fs::write(&on_path, b"").unwrap();
        let path = std::env::join_paths([dir.path().join("empty"), dir.path().to_owned()]).unwrap();
        assert_eq!(locate(Some("".into()), Some(path)).unwrap(), on_path);
        let e = locate(None, Some(dir.path().join("empty").into_os_string())).unwrap_err();
        assert!(e.contains("PresentMon was not found"), "{e}");
    }
}
