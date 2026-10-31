//! Development features, compiled only with the `dev-tools` feature.
//!
//! `play <chart> --dev-scripted-input <log>` plays the chart on the same
//! threads as the game, without a window or devices: the input log's
//! events drive the input side as a script on the play's timeline, fed by
//! the clocked input thread, the audio engine plays into a recording of a
//! simulated device paced by the clock, and the renderer draws offscreen
//! on WARP. `play --auto <chart> --dev-headless` runs autoplay the same
//! way. `--dev-frames <dir>` also writes a countdown frame, a play frame
//! and the end screen there as PNG files. `--dev-overlay` shows the
//! measurement overlay from the start. `--dev-start <ms>` plays a mid-chart
//! start from that time of the chart to its end.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use audio::{AudioOutput, OutputError, RecordingOutput, SimConfig, SimDevice, Wake};
use bms_model::{TimeSource, TimeUs};
use game::Wakes;
use input::{DeviceKind, QpcClock, ScriptedInput};
use replay::DecodeLimits;

use crate::args::{Command, UsageError};
use crate::measure::Drawn;
use crate::recording::SoundRecording;
use crate::render_side::{FrameSink, Screen};

pub mod scenario;

/// Prints, to the console only, which window took the foreground when the
/// game's window lost focus: its process's executable name and its window
/// class, with the time on the game's clock (µs since start). A focus loss
/// aborts the play (design.md "Play Lifecycle"), so this tells what did it.
pub fn print_foreground(now: TimeUs) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
    };
    use windows::core::PWSTR;

    // SAFETY: plain queries of the foreground window and its process; the
    // buffers outlive the calls and the process handle is closed.
    let (class, process) = unsafe {
        let hwnd = GetForegroundWindow();
        let mut class = [0_u16; 256];
        let n = usize::try_from(GetClassNameW(hwnd, &mut class)).unwrap_or(0);
        let class = String::from_utf16_lossy(&class[..n]);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .ok()
            .and_then(|h| {
                let mut buf = [0_u16; 1024];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    h,
                    PROCESS_NAME_WIN32,
                    PWSTR(buf.as_mut_ptr()),
                    &raw mut len,
                )
                .is_ok();
                let _ = CloseHandle(h);
                ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
            });
        (class, process)
    };
    // The executable's name only: its path may hold the user's name.
    let name = process
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .map_or_else(
            || "unknown".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
    println!(
        "dev: focus lost at {} us to {name} (window class {class})",
        now.0
    );
}

/// `--dev-tools-probe`: says the build has the development features.
pub fn probe() -> ExitCode {
    // CI searches release binaries for this exact string to prove the
    // feature is absent from production builds.
    println!("tapestry dev-tools enabled");
    ExitCode::SUCCESS
}

/// The arguments of `measure play [--perfect] <chart>`, after `measure`.
///
/// # Errors
///
/// The usage error for anything but `play`, one chart and `--perfect`.
pub fn measure_args(p: &mut lexopt::Parser) -> Result<Command, UsageError> {
    const MEASURE_USAGE: &str = "usage: tapestry measure play [--perfect] <chart.bms>";
    match p.next()? {
        Some(lexopt::Arg::Value(v)) if v == "play" => {}
        Some(other) => return Err(other.unexpected().into()),
        None => return Err(UsageError::Usage(MEASURE_USAGE.to_owned())),
    }
    let mut chart = None;
    let mut perfect = false;
    while let Some(arg) = p.next()? {
        match arg {
            lexopt::Arg::Value(v) if chart.is_none() => chart = Some(PathBuf::from(v)),
            // A second `--perfect` is unknown, as a second chart is.
            lexopt::Arg::Long("perfect") if !perfect => perfect = true,
            other => return Err(other.unexpected().into()),
        }
    }
    let Some(chart) = chart else {
        return Err(UsageError::Usage(format!(
            "measure play needs a chart file\n{MEASURE_USAGE}"
        )));
    };
    Ok(Command::Measure { chart, perfect })
}

/// The development options of `play`.
#[derive(Debug, Default)]
pub struct Options {
    /// The input log to play.
    pub scripted_input: Option<PathBuf>,
    /// Where to write frames.
    pub frames: Option<PathBuf>,
    /// Retry once at the first end screen and quit at the second, to
    /// measure the retry time without a key press.
    pub auto_retry: bool,
    /// Settings keys pressed by themselves when the first play reaches
    /// `adjust_at_ms`.
    pub adjust_in_play: Vec<game::Adjust>,
    /// Settings keys pressed by themselves at the first end screen.
    pub adjust_on_end: Vec<game::Adjust>,
    /// The chart time, ms, at which `adjust_in_play` is pressed.
    pub adjust_at_ms: i64,
    /// Show the measurement overlay from the start, without its key.
    pub overlay: bool,
    /// The result screen's input, scripted (`--dev-result`): `next`,
    /// `prev`, `back` and `retry`, one step each time a result is drawn.
    pub result: Option<SelectScript>,
    /// Run autoplay headless: a recording of a simulated audio device and
    /// WARP offscreen, as a scripted run.
    pub headless: bool,
    /// Play generated input that hits every note, with key sounds sent from
    /// judgments, and save the play: the perfect measurement.
    pub perfect: bool,
    /// How long the render thread's answer to a preparation or a chart's
    /// load, and its end, are waited for, in place of the production bounds
    /// (`--dev-render-timeout <seconds>`): a run on WARP on a loaded machine
    /// can take tens of seconds for a frame.
    pub render_timeout: Option<std::time::Duration>,
    /// Play a mid-chart start from this time of the chart's 1× time to its
    /// end (`--dev-start <ms>`; design.md "Shortening Launch-Based Checks").
    pub start: Option<TimeUs>,
}

impl Options {
    /// The session's input with the development options applied: an input
    /// log or the perfect measurement in place of the devices.
    ///
    /// # Errors
    ///
    /// A usage error for options that cannot go together: two of autoplay,
    /// an input log and the perfect measurement, or `--dev-headless`
    /// without generated input.
    pub fn play_input(&self, input: crate::PlayInput) -> Result<crate::PlayInput, UsageError> {
        let log = self.scripted_input.clone();
        let auto = matches!(input, crate::PlayInput::Autoplay);
        // Autoplay plays whole charts only.
        if self.start.is_some() && (auto || self.perfect) {
            return Err(UsageError::Usage(
                "--dev-start cannot be used with --auto or --dev-perfect".to_owned(),
            ));
        }
        match input {
            crate::PlayInput::Autoplay if log.is_some() => Err(UsageError::Usage(
                "--auto and --dev-scripted-input cannot be used together".to_owned(),
            )),
            _ if self.perfect && (auto || log.is_some()) => Err(UsageError::Usage(
                "--dev-perfect cannot be used with --auto or --dev-scripted-input".to_owned(),
            )),
            crate::PlayInput::Autoplay => Ok(input),
            _ if self.perfect => Ok(crate::PlayInput::Perfect),
            _ if self.headless => Err(UsageError::Usage(
                "--dev-headless needs --auto or --dev-perfect".to_owned(),
            )),
            _ => Ok(log.map_or(input, crate::PlayInput::Script)),
        }
    }

    /// Takes the long option `name` (without its dashes) and its value as a
    /// development option of `play`.
    ///
    /// # Errors
    ///
    /// An option `play` does not take, or a missing or bad value.
    pub fn take(&mut self, name: &str, p: &mut lexopt::Parser) -> Result<(), UsageError> {
        match name {
            "dev-auto-retry" => self.auto_retry = true,
            "dev-overlay" => self.overlay = true,
            "dev-headless" => self.headless = true,
            "dev-perfect" => self.perfect = true,
            "dev-adjust-at" => {
                self.adjust_at_ms = p.value()?.to_string_lossy().parse().map_err(|_| {
                    UsageError::Usage("--dev-adjust-at takes milliseconds".to_owned())
                })?;
            }
            "dev-adjust" => {
                self.adjust_in_play =
                    adjust_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-adjust-end" => {
                self.adjust_on_end =
                    adjust_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-scripted-input" => self.scripted_input = Some(PathBuf::from(p.value()?)),
            "dev-frames" => self.frames = Some(PathBuf::from(p.value()?)),
            "dev-render-timeout" => self.render_timeout = Some(render_timeout(p)?),
            "dev-start" => self.start = Some(start(p)?),
            "dev-result" => {
                let steps = p.value()?;
                self.result =
                    Some(SelectScript::parse(&steps.to_string_lossy()).map_err(UsageError::Usage)?);
            }
            _ => return Err(UsageError::Unknown(format!("--{name}"))),
        }
        Ok(())
    }
}

/// The value of `--dev-start`: whole milliseconds, 0 or more, as a time.
fn start(p: &mut lexopt::Parser) -> Result<TimeUs, UsageError> {
    let bad = || UsageError::Usage("--dev-start takes milliseconds, 0 or more".to_owned());
    let ms: i64 = p.value()?.to_string_lossy().parse().map_err(|_| bad())?;
    if ms < 0 {
        return Err(bad());
    }
    ms.checked_mul(1_000).map(TimeUs).ok_or_else(bad)
}

/// The value of `--dev-render-timeout`, in seconds.
fn render_timeout(p: &mut lexopt::Parser) -> Result<std::time::Duration, UsageError> {
    let secs = p
        .value()?
        .to_string_lossy()
        .parse()
        .map_err(|_| UsageError::Usage("--dev-render-timeout takes seconds".to_owned()))?;
    Ok(std::time::Duration::from_secs(secs))
}

/// The development options of song selection (`tapestry [--auto]`).
#[derive(Debug, Default)]
pub struct SelectOptions {
    /// Run headless: a recording of a simulated audio device, WARP
    /// offscreen and no window; needs `--auto` and a script.
    pub headless: bool,
    /// The select screen's input, scripted (`--dev-select`).
    pub script: Option<SelectScript>,
    /// The wait between scan passes (`--dev-rescan-ms`).
    pub rescan_ms: Option<u64>,
    /// Where to write frames (`--dev-frames`): the select screen's first
    /// frame, and the first play's countdown, play and end frames.
    pub frames: Option<PathBuf>,
    /// The render thread's bounds, as [`Options::render_timeout`]
    /// (`--dev-render-timeout`).
    pub render_timeout: Option<std::time::Duration>,
    /// The scenario the run plays (`--dev-scenario`), which no option of
    /// selection sets: its roots, captures and inputs.
    pub scenario: Option<scenario::Session>,
}

impl SelectOptions {
    /// Takes the long option `name` (without its dashes) and its value.
    ///
    /// # Errors
    ///
    /// An option selection does not take, or a missing or bad value.
    pub fn take(&mut self, name: &str, p: &mut lexopt::Parser) -> Result<(), UsageError> {
        match name {
            "dev-headless" => self.headless = true,
            "dev-select" => {
                let steps = p.value()?;
                self.script =
                    Some(SelectScript::parse(&steps.to_string_lossy()).map_err(UsageError::Usage)?);
            }
            "dev-rescan-ms" => {
                self.rescan_ms = Some(p.value()?.to_string_lossy().parse().map_err(|_| {
                    UsageError::Usage("--dev-rescan-ms takes milliseconds".to_owned())
                })?);
            }
            "dev-frames" => self.frames = Some(PathBuf::from(p.value()?)),
            "dev-render-timeout" => self.render_timeout = Some(render_timeout(p)?),
            _ => return Err(UsageError::Unknown(format!("--{name}"))),
        }
        Ok(())
    }

    /// Refuses options that cannot go together.
    ///
    /// # Errors
    ///
    /// `--dev-headless` without `--auto` and a script.
    pub fn check(&self, auto: bool) -> Result<(), UsageError> {
        if self.headless && !(auto && self.script.is_some()) {
            return Err(UsageError::Usage(
                "--dev-headless needs --auto and --dev-select".to_owned(),
            ));
        }
        Ok(())
    }
}

/// One step of a scripted select screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectStep {
    /// Wait until the chart of this file name is listed, then move the
    /// cursor onto it with the arrow keys.
    Pick(String),
    /// Wait until the chart of this file is listed, then move the cursor
    /// onto it (a scenario's `select { path }`): any query is cleared first.
    PickPath(scenario::Target),
    /// Wait until the chart of this file name is listed.
    Wait(String),
    /// Type into the search box: Tab, the text, Enter.
    Search(String),
    /// Replace the search text with this one, as typing does, and wait
    /// until it is searched (a scenario's `select { search }`).
    Query(String),
    /// Move the cursor onto the one chart the search for this text lists,
    /// once a scan pass has completed.
    PickSearched(String),
    /// Wait this many milliseconds on the select screen.
    Sleep(u64),
    /// Wait this many milliseconds on the select screen and print how often
    /// the input thread woke meanwhile.
    Idle(u64),
    /// Press Enter.
    Enter,
    /// Press F11: the previous play preset.
    PrevPreset,
    /// Press F12: the next play preset.
    NextPreset,
    /// A play starts, fed as it says (a scenario's `play`).
    Play(scenario::PlayStep),
    /// Save the frame its capture names, in the next play that starts.
    Capture(u16),
    /// On a result: wait until its page has been drawn, at most
    /// [`STEP_TIMEOUT`].
    Drawn,
    /// On a result: leave it (the quit key).
    Back,
    /// On a result: retry.
    Retry,
    /// On a result: view the next preset (the right arrow).
    Next,
    /// On a result: view the previous preset (the left arrow).
    Prev,
    /// Press P on the chart under the cursor (a scenario's `practice`): its
    /// practice page opens, the chart playing this input.
    PracticeKey(scenario::InputRef),
    /// On the practice page: press keys until it shows this.
    PracticeSet(scenario::PracticeGoal),
    /// On the practice page: Start.
    PracticeStart,
    /// The practice's rounds, this many: the quit key at the start of the
    /// round after the last.
    Rounds(u16),
    /// On the practice page: back to selection (Escape).
    PracticeBack,
    /// Close the window once the next chart's load starts measuring its
    /// loudness: a close during the load.
    CloseMeasuring,
    /// The next chart's load holds this many milliseconds before its
    /// sounds, cancelled by the quit key or a close as the measurement is:
    /// a slow load.
    SlowLoad(u64),
    /// Halfway through the next slow load, press the quit key.
    QuitLoading,
    /// Halfway through the next slow load, close the window.
    CloseLoading,
    /// Close the window.
    Close,
}

/// How long a scripted step waits for its chart.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// A slow load a script asks for (`slow-load`, `quit-loading`,
/// `close-loading`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlowLoad {
    /// How long the load holds.
    pub ms: u64,
    /// The quit key is pressed halfway through it.
    pub quit: bool,
    /// The window closes halfway through it.
    pub close: bool,
}

/// `--dev-select <steps>`: the select screen's input, comma-separated:
/// `pick=<file>`, `wait=<file>`, `search=<text>`, `sleep=<ms>`,
/// `idle=<ms>`, `enter`, `prev-preset`, `next-preset`, `drawn`,
/// `back`, `retry`, `next`, `prev`, `close-measuring`, `slow-load=<ms>`,
/// `quit-loading`, `close-loading` and `close`. Selection steps run on the
/// select screen (`prev-preset` and `next-preset` press F11 and F12;
/// `close-measuring` closes the window once the next load measures a
/// chart's loudness; `slow-load` holds the next load that long, and
/// `quit-loading` and `close-loading` press the quit key or close the
/// window halfway through it),
/// `drawn` (wait until the result's page is drawn), `back`, `retry`, `next`
/// and `prev` on a result; a step out of place, or a chart not
/// listed within [`STEP_TIMEOUT`], fails the run. A scenario
/// ([`scenario`]) compiles into the same steps, each with the scenario step
/// it comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectScript {
    steps: std::collections::VecDeque<(SelectStep, Option<scenario::Origin>)>,
    /// Since when the front step waits.
    since: Option<std::time::Instant>,
    /// The input thread's wakes when an `idle` step began.
    pub idle_from: Option<u64>,
    /// The next load's loudness measurement closes the window
    /// (`close-measuring`).
    pub close_measuring: bool,
    /// How long the next load holds (`slow-load`), and what happens
    /// halfway through it (`quit-loading`, `close-loading`).
    pub slow_load: Option<SlowLoad>,
    /// Why the script failed.
    failed: Option<String>,
    /// The scenario step last announced.
    announced: Option<u16>,
    /// The scenario step of the last step done.
    last_done: Option<scenario::Origin>,
    /// The scenario steps a failure dropped.
    abandoned: Vec<scenario::Origin>,
    /// Why the chart chosen last was refused, until the next choice.
    refusal: Option<String>,
    /// Each listed file's identity, as far as the front step looked them
    /// up: each at most once per step.
    pub identities: std::collections::HashMap<PathBuf, Option<PathBuf>>,
}

impl SelectScript {
    /// Reads the steps.
    ///
    /// # Errors
    ///
    /// A step of none of the forms.
    pub fn parse(list: &str) -> Result<Self, String> {
        let steps = list
            .split(',')
            .map(|s| match s.split_once('=') {
                Some(("pick", f)) => Ok(SelectStep::Pick(f.to_owned())),
                Some(("wait", f)) => Ok(SelectStep::Wait(f.to_owned())),
                Some(("search", t)) => Ok(SelectStep::Search(t.to_owned())),
                Some(("sleep", ms)) => ms.parse().map(SelectStep::Sleep).map_err(|_| {
                    format!("{s} is not pick=, wait=, search=, sleep=, idle=, slow-load=, enter, prev-preset, next-preset, drawn, back, retry, next, prev, close-measuring, quit-loading, close-loading or close")
                }),
                Some(("idle", ms)) => ms
                    .parse()
                    .map(SelectStep::Idle)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-load", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowLoad)
                    .map_err(|_| format!("{s} takes milliseconds")),
                None if s == "enter" => Ok(SelectStep::Enter),
                None if s == "prev-preset" => Ok(SelectStep::PrevPreset),
                None if s == "next-preset" => Ok(SelectStep::NextPreset),
                None if s == "drawn" => Ok(SelectStep::Drawn),
                None if s == "back" => Ok(SelectStep::Back),
                None if s == "retry" => Ok(SelectStep::Retry),
                None if s == "next" => Ok(SelectStep::Next),
                None if s == "prev" => Ok(SelectStep::Prev),
                None if s == "close-measuring" => Ok(SelectStep::CloseMeasuring),
                None if s == "quit-loading" => Ok(SelectStep::QuitLoading),
                None if s == "close-loading" => Ok(SelectStep::CloseLoading),
                None if s == "close" => Ok(SelectStep::Close),
                _ => Err(format!(
                    "{s} is not pick=, wait=, search=, sleep=, idle=, slow-load=, enter, prev-preset, next-preset, drawn, back, retry, next, prev, close-measuring, quit-loading, close-loading or close"
                )),
            })
            .map(|step| step.map(|s| (s, None)))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            steps,
            ..Self::default()
        })
    }

    /// A scenario's steps, each with the scenario step it comes from.
    pub fn compiled(steps: Vec<(SelectStep, scenario::Origin)>) -> Self {
        Self {
            steps: steps.into_iter().map(|(s, o)| (s, Some(o))).collect(),
            ..Self::default()
        }
    }

    /// The step to run now.
    pub fn front(&self) -> Option<&SelectStep> {
        self.steps.front().map(|(s, _)| s)
    }

    /// The scenario step the front step comes from.
    pub fn origin(&self) -> Option<scenario::Origin> {
        self.steps.front().and_then(|&(_, o)| o)
    }

    /// The scenario step of the last step done.
    pub const fn last_done(&self) -> Option<scenario::Origin> {
        self.last_done
    }

    /// Prints the scenario step the front step comes from, once, when it
    /// is first due: on the game thread, outside any play.
    pub fn announce(&mut self) {
        if let Some(o) = self.origin()
            && self.announced != Some(o.index)
        {
            self.announced = Some(o.index);
            println!("dev: scenario: {o}");
        }
    }

    /// The front step is done.
    pub fn done(&mut self) {
        if let Some((_, origin)) = self.steps.pop_front() {
            self.last_done = origin;
        }
        self.since = None;
        self.idle_from = None;
        self.identities.clear();
    }

    /// The front step waits on; `false` once it has waited
    /// [`STEP_TIMEOUT`], which fails the script.
    pub fn waiting(&mut self) -> bool {
        if !self.expired_after(STEP_TIMEOUT) {
            return true;
        }
        let step = format!("{:?}", self.front());
        self.fail(format!("{step} waited {} s", STEP_TIMEOUT.as_secs()));
        false
    }

    /// The front step made progress: its wait starts over, so a step of
    /// many moves times out only once it stalls.
    pub fn progressed(&mut self) {
        self.since = None;
    }

    /// Moves the front step's wait `by` into the past, as if that much
    /// time had passed (tests).
    #[cfg(test)]
    pub fn age(&mut self, by: Duration) {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        self.since = Some(since.checked_sub(by).unwrap_or(since));
    }

    /// Whether the front step has waited [`STEP_TIMEOUT`] since it became
    /// the front.
    pub fn expired(&mut self) -> bool {
        self.expired_after(STEP_TIMEOUT)
    }

    /// Whether the front step has waited `bound` since it became the
    /// front.
    fn expired_after(&mut self, bound: Duration) -> bool {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        since.elapsed() >= bound
    }

    /// The script failed at its front step: no step runs from here on.
    pub fn fail(&mut self, why: String) {
        let why = match self.origin() {
            Some(o) => format!("{o}: {why}"),
            None => why,
        };
        self.failed_with(why);
    }

    /// The script failed at scenario step `origin`, which need not be the
    /// front (a capture settled after its play).
    pub fn fail_at(&mut self, origin: scenario::Origin, why: &str) {
        self.failed_with(format!("{origin}: {why}"));
    }

    fn failed_with(&mut self, why: String) {
        eprintln!("dev: select script: {why}");
        self.failed.get_or_insert(why);
        let left = std::mem::take(&mut self.steps);
        for o in left.into_iter().filter_map(|(_, o)| o) {
            if self.abandoned.last() != Some(&o) {
                self.abandoned.push(o);
            }
        }
    }

    /// The scenario steps that never ran: those a failure dropped and
    /// those still queued, each once.
    pub fn not_run(&self) -> Vec<scenario::Origin> {
        let mut all = self.abandoned.clone();
        for o in self.steps.iter().filter_map(|&(_, o)| o) {
            if all.last() != Some(&o) {
                all.push(o);
            }
        }
        all
    }

    /// Whether the front step has waited `ms` milliseconds since it became
    /// the front.
    pub fn slept(&mut self, ms: u64) -> bool {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        since.elapsed() >= Duration::from_millis(ms)
    }

    /// Why the script failed, if it did.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// A chart was chosen: a refusal from before it is no longer its.
    pub fn chose(&mut self) {
        self.refusal = None;
    }

    /// The chart chosen last was refused, for `why`.
    pub fn refused(&mut self, why: String) {
        self.refusal = Some(why);
    }

    /// Why the chart chosen last was refused, if it was.
    pub fn refusal(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
}

/// A menu screen's frames, the select screen's or the loading page's
/// (dev-tools): CPU time, the interval between frames and the render
/// thread's allocations per frame, kept from the screen's first frame until
/// it is left, then printed.
#[derive(Debug, Default)]
pub struct MenuStats {
    cpu_us: Vec<u64>,
    interval_us: Vec<u64>,
    /// Frames that drew the prepared page.
    steady_allocs: Vec<u64>,
    /// Frames that laid a new page out.
    changed_allocs: Vec<u64>,
    last: Option<std::time::Instant>,
}

impl MenuStats {
    /// One frame: its CPU time, the render thread's allocations during
    /// it, and whether it laid a new page out.
    pub fn frame(&mut self, cpu: Duration, allocs: u64, changed: bool) {
        let now = std::time::Instant::now();
        if let Some(last) = self.last {
            self.interval_us
                .push(u64::try_from(now.duration_since(last).as_micros()).unwrap_or(u64::MAX));
        }
        self.last = Some(now);
        self.cpu_us
            .push(u64::try_from(cpu.as_micros()).unwrap_or(u64::MAX));
        if changed {
            self.changed_allocs.push(allocs);
        } else {
            self.steady_allocs.push(allocs);
        }
    }

    /// Prints what was kept, if anything, as the frames of the `screen`
    /// named, and starts over.
    pub fn report(&mut self, screen: &str) {
        if self.cpu_us.is_empty() {
            return;
        }
        let q = |v: &mut Vec<u64>, p: usize| {
            v.sort_unstable();
            v.get((v.len().saturating_sub(1)) * p / 100)
                .copied()
                .unwrap_or(0)
        };
        let mean = |v: &[u64]| {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<u64>() as f64 / v.len() as f64
            }
        };
        let (cpu50, cpu99, cpumax) = (
            q(&mut self.cpu_us, 50),
            q(&mut self.cpu_us, 99),
            q(&mut self.cpu_us, 100),
        );
        let (gap50, gap99) = (q(&mut self.interval_us, 50), q(&mut self.interval_us, 99));
        println!(
            "dev: {screen} frames {}: cpu p50 {cpu50} us, p99 {cpu99} us, max {cpumax} us; \
             interval p50 {gap50} us, p99 {gap99} us; allocations per frame: \
             steady mean {:.1} max {}, new page mean {:.1} max {} ({} pages)",
            self.cpu_us.len(),
            mean(&self.steady_allocs),
            self.steady_allocs.iter().max().copied().unwrap_or(0),
            mean(&self.changed_allocs),
            self.changed_allocs.iter().max().copied().unwrap_or(0),
            self.changed_allocs.len(),
        );
        *self = Self::default();
    }
}

/// The scripted play's input: the log's events as a script, one device per
/// source.
///
/// # Errors
///
/// Why the log cannot be read.
pub fn script(log: &Path) -> anyhow::Result<ScriptedInput> {
    let limits = DecodeLimits::default();
    let bytes = read_bounded(log, limits.max_compressed)?;
    let rec = replay::decode(&bytes, &limits).with_context(|| log.display().to_string())?;
    let kinds: Vec<DeviceKind> = rec
        .devices
        .sources
        .iter()
        .map(|s| match s.kind {
            replay::DeviceKind::Keyboard => DeviceKind::Keyboard,
            replay::DeviceKind::Controller => DeviceKind::Controller,
        })
        .collect();
    // Each controller declares the axes its record binds, as the adapter
    // would: only declared axes are recorded.
    let script = ScriptedInput::from_log(&rec.start, &rec.events, &kinds);
    Ok(rec
        .devices
        .sources
        .iter()
        .enumerate()
        .fold(script, |script, (i, s)| {
            script.with_axes(input::DeviceId(i as u64), &input::declared_axes(&s.profile))
        }))
}

/// Reads `path` if it holds at most `limit` bytes, reading no more than one
/// byte past the limit: an input log is untrusted (design.md "Input Log
/// Format").
fn read_bounded(path: &Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    let read = || read_limited(fs::File::open(path)?, limit);
    read().with_context(|| path.display().to_string())
}

/// Everything `reader` holds if it is at most `limit` bytes, reading at most
/// one byte past the limit.
fn read_limited(reader: impl std::io::Read, limit: usize) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= limit,
        "larger than {limit} bytes, the largest input log accepted"
    );
    Ok(bytes)
}

/// Settings keys by name, comma-separated: `F1` to `F8`.
///
/// # Errors
///
/// A name that is not a settings key.
pub fn adjust_keys(list: &str) -> Result<Vec<game::Adjust>, String> {
    list.split(',')
        .map(|name| {
            name.strip_prefix('F')
                .and_then(|n| n.parse::<u32>().ok())
                .and_then(|n| game::Adjust::from_key(0x3A + n))
                .ok_or_else(|| format!("{name} is not a settings key (F1 to F8)"))
        })
        .collect()
}

/// One step of a scripted key config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyStep {
    /// A keyboard key pressed and released, by scan code.
    Key(u32),
    /// A controller's control pressed and released.
    Button(u32),
    /// The window closes.
    Close,
}

/// The model a scripted controller press comes from: made up, so a key
/// config scripted with controller controls names a controller and
/// `device.toml` accepts it.
const SCRIPTED_CONTROLLER: input::hid::HidProduct = input::hid::HidProduct {
    vendor_id: 0xFFFE,
    product_id: 0x0001,
    interface: None,
};

/// `keys --dev-scripted-keys <steps>`: presses offered to the capture slot
/// in place of a person, one every 300 ms, comma-separated: a keyboard
/// scan code in hex (`2a`), `b<n>` for a controller's control `n`, or
/// `close` to close the window. Controller presses come from
/// [`SCRIPTED_CONTROLLER`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyScript(Vec<KeyStep>);

impl KeyScript {
    /// Reads the steps.
    ///
    /// # Errors
    ///
    /// A step that is none of the three forms.
    pub fn parse(list: &str) -> Result<Self, String> {
        list.split(',')
            .map(|s| {
                if s == "close" {
                    Ok(KeyStep::Close)
                } else if let Some(n) = s.strip_prefix('b') {
                    n.parse().map(KeyStep::Button).map_err(|_| s.to_owned())
                } else {
                    u32::from_str_radix(s, 16)
                        .map(KeyStep::Key)
                        .map_err(|_| s.to_owned())
                }
            })
            .collect::<Result<_, _>>()
            .map(Self)
            .map_err(|s| format!("{s} is not a scan code, b<n> or close"))
    }

    /// Plays the steps on a thread of its own, as the input thread offers
    /// presses; dropping the handle stops it.
    pub fn spawn(self, capture: input::Capture, controls: Arc<crate::controls::Controls>) -> Stop {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for step in self.0 {
                for _ in 0..30 {
                    if thread_stop.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let (kind, control) = match step {
                    KeyStep::Key(c) => (DeviceKind::Keyboard, c),
                    KeyStep::Button(c) => (DeviceKind::Controller, c),
                    KeyStep::Close => {
                        controls.close();
                        return;
                    }
                };
                for value in [1, 0] {
                    let packet = input::Packet {
                        device: input::DeviceId(1),
                        control: eval::ControlId(control),
                        value,
                        background: false,
                    };
                    match kind {
                        DeviceKind::Keyboard => capture.offer(&packet, kind, false),
                        DeviceKind::Controller => {
                            capture.offer_controller(&packet, Some(SCRIPTED_CONTROLLER), false);
                        }
                    }
                }
            }
        });
        Stop {
            stop,
            handle: Some(handle),
        }
    }
}

/// Stops a scripted thread when dropped.
pub struct Stop {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A simulated device paced by the clock: each wait sleeps until the
/// device's next buffer event is due on the clock, so the engine runs in
/// real time as it does with WASAPI.
pub struct Paced<O> {
    inner: O,
    clock: QpcClock,
}

/// The recording output of a scripted run: 48 kHz, 10 ms periods, waking
/// first now, recording up to `seconds` of audio.
pub fn recording_output(clock: QpcClock, seconds: usize) -> Paced<RecordingOutput<SimDevice>> {
    let device = SimDevice::new(SimConfig::steady(48_000, 480, clock.now()));
    Paced {
        inner: RecordingOutput::new(device, 48_000 * seconds),
        clock,
    }
}

impl<O: AudioOutput + Wakes> AudioOutput for Paced<O> {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> Wake {
        let left = self.inner.next_wake().0 - self.clock.now().0;
        if left > 0 {
            std::thread::sleep(Duration::from_micros(left.unsigned_abs()));
        }
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.inner.write(f)
    }

    fn queue_periods(&self) -> Option<u32> {
        self.inner.queue_periods()
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.inner.set_queue_periods(periods)
    }
}

/// The sound-only recording of a scripted run with song selection: 48 kHz,
/// 10 ms periods, waking first now, keeping up to `seconds` of what
/// sounds. A run with selection waits for loading, analyses and screens
/// between its plays, so it records each play's sound and one silent period
/// for each silence: every onset still follows a period of silence, and the
/// capacity is spent on the plays however long the run waits between them.
pub fn sound_recording(clock: QpcClock, seconds: usize) -> Paced<SoundRecording> {
    let device = SimDevice::new(SimConfig::steady(48_000, 480, clock.now()));
    Paced {
        inner: SoundRecording::new(device, 48_000 * seconds),
        clock,
    }
}

/// An output whose written samples were kept.
pub trait Recorded {
    /// The interleaved stereo samples kept.
    fn samples(&self) -> &[f32];
}

impl Recorded for RecordingOutput<SimDevice> {
    fn samples(&self) -> &[f32] {
        RecordingOutput::samples(self)
    }
}

impl Recorded for SoundRecording {
    fn samples(&self) -> &[f32] {
        SoundRecording::samples(self)
    }
}

/// Prints what the recording output kept.
pub fn report_audio<R: Recorded>(out: &Paced<R>) {
    let samples = out.inner.samples();
    let peak = samples.iter().fold(0.0_f32, |p, s| p.max(s.abs()));
    println!(
        "dev: audio recorded {} frames, peak {peak:.3}",
        samples.len() / audio::CHANNELS
    );
    let onsets = onsets(samples);
    let listed: Vec<String> = onsets.iter().take(64).map(ToString::to_string).collect();
    println!("dev: audio onsets {}: {}", onsets.len(), listed.join(" "));
    // The loudest sample of the 10 ms after each onset, to tell sounds
    // apart.
    let peaks: Vec<String> = onsets
        .iter()
        .take(64)
        .map(|&f| {
            let from = f * audio::CHANNELS;
            let to = (from + 480 * audio::CHANNELS).min(samples.len());
            let peak = samples[from..to]
                .iter()
                .fold(0.0_f32, |p, s| p.max(s.abs()));
            format!("{peak:.3}")
        })
        .collect();
    println!("dev: audio onset peaks: {}", peaks.join(" "));
}

/// The frames where sound starts after at least 10 ms (at 48 kHz) of
/// silence on the left channel: when sounds started, to compare with the
/// chart's times.
fn onsets(samples: &[f32]) -> Vec<usize> {
    const QUIET: usize = 480;
    let mut found = Vec::new();
    let mut silent = QUIET;
    for (frame, s) in samples.iter().step_by(audio::CHANNELS).enumerate() {
        if s.abs() > 1e-3 {
            if silent >= QUIET {
                found.push(frame);
            }
            silent = 0;
        } else {
            silent += 1;
        }
    }
    found
}

/// Writes frames as PNG files: `select.png` at the first select screen
/// frame, `loading.png` at the first loading page frame, `countdown.png` at
/// the first frame of the lead-in, `play.png` at the first frame at or
/// after `play_at`, and `end.png` at the first end screen frame.
pub struct FrameDump {
    dir: PathBuf,
    play_at: TimeUs,
    countdown: bool,
    play: bool,
    end: bool,
    select: bool,
    loading: bool,
}

impl FrameDump {
    /// Frames into `dir`, the play frame at chart time `play_at`.
    pub const fn new(dir: PathBuf, play_at: TimeUs) -> Self {
        Self {
            dir,
            play_at,
            countdown: false,
            play: false,
            end: false,
            select: false,
            loading: false,
        }
    }

    fn save(&self, screen: &dyn Screen, name: &str) {
        let path = self.dir.join(name);
        let Some((w, h, pixels)) = screen.read_back() else {
            eprintln!(
                "dev: {}: the screen has nothing to read back",
                path.display()
            );
            return;
        };
        let written =
            fs::create_dir_all(&self.dir).and_then(|()| fs::write(&path, png(w, h, &pixels)));
        match written {
            Ok(()) => println!("dev: frame {}", path.display()),
            Err(e) => eprintln!("dev: {}: {e}", path.display()),
        }
    }
}

impl FrameSink for FrameDump {
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>) {
        if !drawn.timing.presented {
            return;
        }
        match drawn.play {
            Some(t) if t.0 < 0 && !self.countdown => {
                self.countdown = true;
                self.save(screen, "countdown.png");
            }
            Some(t) if t >= self.play_at && !self.play => {
                self.play = true;
                self.save(screen, "play.png");
            }
            None if drawn.loading => {
                if !self.loading {
                    self.loading = true;
                    self.save(screen, "loading.png");
                }
            }
            None if drawn.menu => {
                if !self.select {
                    self.select = true;
                    self.save(screen, "select.png");
                }
            }
            None if !self.end => {
                self.end = true;
                self.save(screen, "end.png");
            }
            _ => {}
        }
    }
}

/// An RGBA8 image as a PNG file, stored without compression (deflate's
/// stored blocks): enough for a few development screenshots without an
/// image library.
fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks(row) {
        raw.push(0); // no filter
        raw.extend_from_slice(line);
    }
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(0xFFFF).peekable();
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        // `chunks(0xFFFF)` keeps every block within u16.
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr[..]), (b"IDAT", &z[..]), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for &x in data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0_u32;
    for &x in data {
        c ^= u32::from(x);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::{adler32, crc32, png, read_bounded, read_limited};

    /// A 1 MB source that counts the bytes taken from it.
    struct Counted<'a>(&'a std::cell::Cell<u64>);

    impl std::io::Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = 1_000_000 - self.0.get();
            let n = buf.len().min(usize::try_from(left).unwrap());
            buf[..n].fill(7);
            self.0.set(self.0.get() + n as u64);
            Ok(n)
        }
    }

    #[test]
    fn a_log_over_the_limit_is_refused_after_reading_one_byte_more() {
        let taken = std::cell::Cell::new(0);
        let e = format!("{:#}", read_limited(Counted(&taken), 10).unwrap_err());
        assert!(e.contains("larger than 10 bytes"), "{e}");
        assert_eq!(taken.get(), 11);
        assert_eq!(read_limited(&[7_u8; 11][..], 11).unwrap().len(), 11);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.tprl");
        std::fs::write(&path, [7_u8; 11]).unwrap();
        let e = format!("{:#}", read_bounded(&path, 10).unwrap_err());
        assert!(e.contains("big.tprl: larger than 10 bytes"), "{e}");
    }

    #[test]
    fn checksums_match_their_reference_values() {
        // The published check values: CRC-32 of "123456789" is CBF43926,
        // Adler-32 of "Wikipedia" is 11E60398.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_splits_rows_into_stored_blocks() {
        // 20 000 px × 1 row: 80 001 raw bytes, more than one stored block.
        let out = png(20_000, 1, &vec![7_u8; 80_000]);
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        // IDAT follows the signature and the 25-byte IHDR chunk.
        let idat = &out[33..];
        let len = u32::from_be_bytes(idat[..4].try_into().unwrap()) as usize;
        assert_eq!(&idat[4..8], b"IDAT");
        let z = &idat[8..8 + len];
        // The zlib header, a non-final block of 65 535 bytes, a final one
        // of 80 001 − 65 535 = 14 466, then the Adler-32.
        assert_eq!(&z[..7], [0x78, 0x01, 0x00, 0xFF, 0xFF, 0x00, 0x00]);
        let second = 2 + 5 + 65_535;
        assert_eq!(z[second], 1);
        assert_eq!(z[second + 1..second + 3], 14_466_u16.to_le_bytes());
        assert_eq!(z.len(), second + 5 + 14_466 + 4);
    }
}
