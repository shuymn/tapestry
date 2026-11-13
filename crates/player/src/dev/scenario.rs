//! `tapestry --dev-scenario <file.toml>` (dev-tools): song selection run
//! headless on the game's threads, driven by a file of steps (design.md
//! "Shortening Launch-Based Checks").
//!
//! The file compiles into the same scripted select and result input as
//! `--dev-select` ([`SelectScript`]), each step keeping the scenario step it
//! comes from, so a failure names it. The steps run where the keyboard's
//! input does (the select screen and a result), and a play's steps where
//! the rig prepares, starts, waits on and ends a play: its input is chosen
//! when its chart loads, its `until` command is sent from the logic step's
//! wait, and the frames its captures name are written by the render
//! thread's frame sink.
//!
//! ```toml
//! version = 8
//! output = "out"                 # needed by captures; made by the run
//! size = [1920, 1080]            # optional: the offscreen target
//! adapter = "hardware"           # optional: "warp" (the default, CI's) or
//!                                # the system's hardware adapter, local runs
//!
//! [library]
//! roots = ["library"]            # relative to this file
//! rescan_ms = 50                 # optional
//!
//! [[step]]
//! capture = { at = 3000, file = "a.png" }   # chart ms, the next play's
//! # or { ready_at = 500, file = … }: ms since its first candidate's READY
//! # state began; or { closing_at = 300, file = … }: ms since its Closing
//! # began, which no frame is guaranteed to show; or
//! # { loading = true, file = … }: its loading state's first frame; or
//! # { loading_at = 1500, file = … }: ms on its loading state's clock,
//! # since its loading lifetime's origin, the screen clock's (failed if the
//! # loading state ends first); or
//! # { result_at = 400, file = … }: ms since the next play's result's first
//! # presented frame. On the select screen, { select_at = 250, file = … }:
//! # ms since the first presented selection frame that applied the update
//! # the steps before it published. Written on a result, { result_at = … }
//! # is that result's, once it draws a view or page published after the
//! # steps before it. A select_at or result_at capture holds the next step
//! # (and a result's leaving or view command) until it is written or has
//! # failed, at most the render thread bound; `hold = false` does not.
//! # Beside each PNG, a JSON file of the same name says what the frame was,
//! # and a play frame of an imported skin a JSON lines file its display
//! # list.
//!
//! [[step]]
//! cursor = { path = "library/a/a.bms" }     # or { search = "text" }:
//! # the cursor onto the chart, which is not chosen
//!
//! [[step]]
//! select = { path = "library/a/a.bms" }     # or { search = "text" }
//!
//! [[step]]
//! play = { input = "auto", until = "end" }
//! # input = "auto" | "<input log, relative to this file>" | { miss = "all" }
//! # { miss = "all" }: generated for the chart, no key pressed, played
//! # and saved as a log's play is: its judgeable notes are missed, and it
//! # ends as such a play does
//! # until = "end" | { at_ms = 2250, command = "retry" | "quit" | "skip" }
//! # "quit": the quit key before the play completes, which ends it midway
//! # on its result; "skip": the quit key once it is complete, its tail
//! # still sounding, which skips to its result. Both land on the result,
//! # which a `result` step leaves; at_ms is never in READY, where
//! # the quit key would leave the chart without one
//!
//! [[step]]
//! result = "next-preset"         # "prev-preset" | "retry" | "back" | "stay"
//! # "stay": nothing pressed once the result is drawn; it leaves on its
//! # own (a result skin's scene time), and selection takes the next step;
//! # with no scene time in force the run fails
//!
//! [[step]]
//! practice = { path = "library/a/a.bms", from = 1, to = 2, speed = 75, lead_in_ms = 2000, gauge = 20, repeat = true, input = "logs/a.tprl", rounds = 1 }
//! # or { search = "text", … }; measures from 1, both included; speed in
//! # percent; the displayed gauge's initial value in percent; rounds
//! # played, then the quit key at the next round's start, in its READY state,
//! # which returns to the page, and back
//!
//! [[step]]
//! quit = {}
//! ```

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use bms_model::{Speed, TimeUs};
use game::Command;
use input::{QpcClock, ScriptedInput};
use rules::GaugeValue;
use serde::Deserialize;

use super::{SelectScript, SelectStep};
use crate::args::UsageError;
use crate::input_side::Recorded;
use crate::measure::{Drawn, ResultId, ScreenKind};
use crate::render_side::{FrameSink, RenderEnd, Screen};

/// The one version of the file this build reads.
const VERSION: i64 = 8;

/// The largest file read.
const MAX_FILE: usize = 1 << 20;

/// What kind of scenario step a compiled step comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// `select`.
    Select,
    /// `play`.
    Play,
    /// `result`.
    Result,
    /// `capture`.
    Capture,
    /// `quit`.
    Quit,
    /// `practice`.
    Practice,
    /// `cursor`.
    Cursor,
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Select => "select",
            Self::Play => "play",
            Self::Result => "result",
            Self::Capture => "capture",
            Self::Quit => "quit",
            Self::Practice => "practice",
            Self::Cursor => "cursor",
        })
    }
}

/// A scenario step: its number in the file, from 1, and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    /// The step's number.
    pub index: u16,
    /// Its kind.
    pub kind: StepKind,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {} ({})", self.index, self.kind)
    }
}

/// A chart chosen by its file: the path as written, made absolute, and
/// the file's identity (its canonical path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The path, absolute and normalized.
    pub path: PathBuf,
    /// The canonical path.
    pub identity: PathBuf,
}

/// Where a play's input comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRef {
    /// Autoplay.
    Auto,
    /// The scenario's `k`th input log.
    Log(u16),
}

/// A `play` step compiled: its input, and the command sent at a chart
/// time, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayStep {
    /// The play's input.
    pub input: InputRef,
    /// When, in chart time, and which command ends the play early.
    pub until: Option<(TimeUs, UntilCommand)>,
}

/// What an `until` sends, and so where the play lands: each is the key it
/// names, and the run checks the play's end against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UntilCommand {
    /// The retry key: the chart is played again.
    Retry,
    /// The quit key before the play completes: it ends midway, through
    /// Closing once the chart started outside autoplay, and its result
    /// shows.
    Quit,
    /// The quit key once the play is complete, its tail still sounding:
    /// the tail is skipped to the result (design.md "Audio tail").
    Skip,
}

impl UntilCommand {
    /// The command its key sends.
    #[must_use]
    pub const fn command(self) -> Command {
        match self {
            Self::Retry => Command::Retry,
            Self::Quit | Self::Skip => Command::Quit,
        }
    }

    /// Why a play that ended as `report` did not land where this command
    /// says, if it did not. `autoplay`: the play was autoplayed; `sent_at`:
    /// the play time the key was sent at.
    ///
    /// A quit is checked by what the play did, not by when it was asked
    /// for: the play's own cause is the quit, unless the key came in a
    /// Closing that had started before it was sent, where it does nothing,
    /// whether a failure or an earlier quit started it (told apart from a
    /// quit that was taken, whose Closing starts as it is taken); Closing follows only a played attempt outside
    /// autoplay, and must once the play's evaluation finalized its chart's
    /// start.
    /// A retry is taken once: the play, its tail or its Closing ended by
    /// it.
    #[must_use]
    pub fn missed(
        self,
        report: &game::PlayReport,
        autoplay: bool,
        sent_at: TimeUs,
    ) -> Option<&'static str> {
        let completed = report.end == game::PlayEnd::Completed;
        let quit = report.end == game::PlayEnd::Aborted(game::AbortCause::Quit);
        match self {
            Self::Quit if completed => Some(
                "the quit came after the play completed, which skips to its result: \
                 use command = \"skip\"",
            ),
            // Sent once a Closing had started, by a failure or an earlier
            // quit alike: discarded, whatever the play's own cause.
            Self::Quit if report.closing.is_some_and(|c| c.started_at < sent_at) => {
                Some("the quit came during Closing, where it does nothing")
            }
            Self::Quit if !quit => Some("the play ended for another cause before the quit"),
            Self::Quit if report.closing.is_some() && !may_close(report, autoplay) => {
                Some("the quit entered Closing in autoplay or before playback, which it never does")
            }
            Self::Quit if report.closing.is_none() && must_close(report, autoplay) => {
                Some("the quit during play did not enter Closing")
            }
            Self::Skip if !report.tail_skipped => {
                Some("the play was not complete, its tail still sounding, at at_ms")
            }
            Self::Retry if !retried(report) => Some("the retry was not taken"),
            Self::Retry | Self::Quit | Self::Skip => None,
        }
    }
}

/// Whether Closing may follow the quit key that ended `report`'s play: a
/// played attempt outside autoplay.
fn may_close(report: &game::PlayReport, autoplay: bool) -> bool {
    !autoplay && report.played()
}

/// Whether Closing must follow it: it may, and the play's evaluation had
/// already finalized its chart's start (a round's section start), so the
/// quit was taken during play whatever the input side's lag.
fn must_close(report: &game::PlayReport, autoplay: bool) -> bool {
    let Some(record) = report.record.as_ref() else {
        return false;
    };
    let start = match &record.play_type {
        replay::PlayType::Full => TimeUs(0),
        replay::PlayType::MidChartStart(p) | replay::PlayType::SectionRepeat(p) => p.start,
    };
    may_close(report, autoplay) && record.status.finalized >= start
}

/// Whether the retry key ended `report`'s play, its tail or its Closing.
fn retried(report: &game::PlayReport) -> bool {
    let retry = game::AbortCause::Retry;
    report.end == game::PlayEnd::Aborted(retry)
        || report.tail_cut == Some(retry)
        || report
            .closing
            .is_some_and(|c| c.end == game::ClosingEnd::Retry)
}

/// `--dev-scenario`'s arguments.
#[derive(Debug)]
pub struct ScenarioArgs {
    /// The scenario file.
    pub file: PathBuf,
    /// `--dev-render-timeout`, as on song selection.
    pub render_timeout: Option<Duration>,
    /// `--dev-specializing <mode>`: what the run's first specialization
    /// of the mode's stage gets once its Lua executes, as the select
    /// script's `<mode>-specializing` step.
    pub specializing: Option<(super::SpecializingStage, super::Specializing)>,
}

/// The arguments after `--dev-scenario`: the file, then
/// `--dev-render-timeout <s>` and `--dev-specializing <mode>`, each at
/// most once.
///
/// # Errors
///
/// A missing file, or any other argument.
pub fn args(p: &mut lexopt::Parser) -> Result<ScenarioArgs, UsageError> {
    let file = PathBuf::from(p.value()?);
    let mut render_timeout = None;
    let mut specializing = None;
    while let Some(arg) = p.next()? {
        match arg {
            lexopt::Arg::Long("dev-render-timeout") if render_timeout.is_none() => {
                render_timeout = Some(super::render_timeout(p)?);
            }
            lexopt::Arg::Long("dev-specializing") if specializing.is_none() => {
                let mode = p.value()?;
                let mode = mode.to_string_lossy();
                specializing = Some(super::Specializing::staged(&mode).ok_or_else(|| {
                    UsageError::Usage(
                        "--dev-specializing takes quit, close, input-failure, escalate or \
                         escalate-input-failure, optionally after result-image- or \
                         result-layout-"
                            .to_owned(),
                    )
                })?);
            }
            other => return Err(other.unexpected().into()),
        }
    }
    Ok(ScenarioArgs {
        file,
        render_timeout,
        specializing,
    })
}

/// Only the version, read before the schema: any other version is refused
/// whatever else the file holds.
#[derive(Deserialize)]
struct Header {
    version: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[expect(dead_code, reason = "checked by `Header` first")]
    version: i64,
    output: Option<PathBuf>,
    size: Option<[i64; 2]>,
    adapter: Option<Adapter>,
    library: Library,
    #[serde(default)]
    step: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Library {
    roots: Vec<PathBuf>,
    rescan_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Step {
    Select(SelectSpec),
    Play(PlaySpec),
    Result(ResultKind),
    Capture(CaptureSpec),
    Quit(Empty),
    Practice(PracticeSpec),
    Cursor(SelectSpec),
}

impl Step {
    const fn kind(&self) -> StepKind {
        match self {
            Self::Select(_) => StepKind::Select,
            Self::Play(_) => StepKind::Play,
            Self::Result(_) => StepKind::Result,
            Self::Capture(_) => StepKind::Capture,
            Self::Quit(_) => StepKind::Quit,
            Self::Practice(_) => StepKind::Practice,
            Self::Cursor(_) => StepKind::Cursor,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectSpec {
    path: Option<PathBuf>,
    search: Option<String>,
    decision: Option<DecisionSpec>,
}

/// What a choice's decision screen must draw: `decision = "skin"`, the
/// decision skin, its labels ready before the choice and pinned by it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DecisionSpec {
    Skin,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaySpec {
    input: InputSpec,
    until: Until,
}

/// A `play` step's input: autoplay or a log by its path, or input made for
/// the chart.
#[derive(Deserialize)]
#[serde(untagged)]
enum InputSpec {
    Named(String),
    Generated(GeneratedSpec),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedSpec {
    miss: MissWord,
}

/// Which notes generated input leaves unpressed.
#[derive(Deserialize)]
enum MissWord {
    #[serde(rename = "all")]
    All,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Until {
    End(EndWord),
    At(AtSpec),
}

#[derive(Deserialize)]
enum EndWord {
    #[serde(rename = "end")]
    End,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AtSpec {
    at_ms: i64,
    command: CommandWord,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CommandWord {
    Retry,
    Quit,
    Skip,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ResultKind {
    NextPreset,
    PrevPreset,
    Retry,
    Back,
    Stay,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureSpec {
    at: Option<i64>,
    ready_at: Option<i64>,
    closing_at: Option<i64>,
    loading: Option<bool>,
    loading_at: Option<i64>,
    select_at: Option<i64>,
    result_at: Option<i64>,
    hold: Option<bool>,
    file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PracticeSpec {
    path: Option<PathBuf>,
    search: Option<String>,
    decision: Option<DecisionSpec>,
    from: i64,
    to: i64,
    speed: i64,
    lead_in_ms: i64,
    gauge: i64,
    repeat: bool,
    input: String,
    rounds: i64,
}

/// What a `practice` step sets on the practice page, as the page's model
/// holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PracticeGoal {
    /// The first measure, by index.
    pub from: usize,
    /// Just past the last measure, by index.
    pub to: usize,
    /// The speed.
    pub speed: Speed,
    /// The lead-in.
    pub lead_in: TimeUs,
    /// The displayed gauge's initial value.
    pub value: GaugeValue,
    /// Whether rounds repeat.
    pub repeat: bool,
    /// The rounds' input.
    pub input: InputRef,
}

/// A loaded scenario: its library, its compiled steps, and what the run
/// keeps of it.
#[derive(Debug)]
pub struct Scenario {
    /// The wait between scan passes, ms.
    pub rescan_ms: Option<u64>,
    /// The steps.
    pub script: SelectScript,
    /// The rest.
    pub session: Session,
}

/// What a scenario's run keeps beside its steps.
pub struct Session {
    /// The library roots, absolute, in place of `device.toml`'s.
    pub roots: Vec<PathBuf>,
    /// The input logs, by [`InputRef::Log`].
    pub logs: Vec<ScriptedInput>,
    /// The captures, by [`SelectStep::Capture`].
    pub captures: Captures,
    /// The offscreen target's size, when the file gives one.
    pub size: Option<(u32, u32)>,
    /// What draws the run's screen.
    pub adapter: Adapter,
}

/// What draws a scenario's screen (design.md "Shortening Launch-Based
/// Checks").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    /// WARP, the software rasterizer: the default, and CI's.
    Warp,
    /// The system's high-performance hardware adapter, for local runs; a
    /// CPU adapter is refused ([`admit`]).
    Hardware,
}

impl Adapter {
    /// The renderer's choice for it.
    #[must_use]
    pub const fn choice(self) -> render::AdapterChoice {
        match self {
            Self::Warp => render::AdapterChoice::Warp,
            Self::Hardware => render::AdapterChoice::HighPerformance,
        }
    }
}

/// Whether the adapter `info` a screen was built on serves what was
/// `asked`: a hardware choice refuses a CPU adapter.
///
/// # Errors
///
/// [`render::RenderError::CpuAdapter`] naming the adapter.
pub fn admit(
    asked: render::AdapterChoice,
    info: &render::AdapterInfo,
) -> Result<(), render::RenderError> {
    if asked != render::AdapterChoice::Warp && info.device_type == render::DeviceType::Cpu {
        return Err(render::RenderError::CpuAdapter {
            name: info.name.clone(),
        });
    }
    Ok(())
}

/// The environment variable of a scenario's injected adapter fault
/// (tests of recovery): `cpu<N>`, the N-th build of the screen, counted
/// from 1, reported as a CPU adapter to its line and its admission.
pub const ADAPTER_FAULTS_ENV: &str = "TAPESTRY_DEV_ADAPTER_FAULTS";

/// The build reported as a CPU adapter ([`ADAPTER_FAULTS_ENV`]); 0 none,
/// `u64::MAX` not read yet.
static CPU_BUILD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
/// The screen's builds so far.
static BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The adapter a build of the screen got, `info`, or the CPU adapter an
/// injected fault reports for this build ([`ADAPTER_FAULTS_ENV`]).
#[must_use]
pub fn built_adapter(info: &render::AdapterInfo) -> std::borrow::Cow<'_, render::AdapterInfo> {
    use std::sync::atomic::Ordering;
    if CPU_BUILD.load(Ordering::SeqCst) == u64::MAX {
        let list = std::env::var(ADAPTER_FAULTS_ENV).unwrap_or_default();
        let n = list
            .split(',')
            .find_map(|f| f.trim().strip_prefix("cpu")?.parse().ok())
            .unwrap_or(0);
        CPU_BUILD.store(n, Ordering::SeqCst);
    }
    let build = BUILDS.fetch_add(1, Ordering::SeqCst) + 1;
    if CPU_BUILD.load(Ordering::SeqCst) == build {
        return std::borrow::Cow::Owned(render::AdapterInfo {
            name: "injected CPU adapter".to_owned(),
            ..render::AdapterInfo::new(render::DeviceType::Cpu, info.backend)
        });
    }
    std::borrow::Cow::Borrowed(info)
}

/// The line each build of a scenario's screen prints: what it asked for
/// and what it got.
#[must_use]
pub fn adapter_line(asked: render::AdapterChoice, info: &render::AdapterInfo) -> String {
    let asked = match asked {
        render::AdapterChoice::Warp => "warp",
        render::AdapterChoice::HighPerformance => "hardware",
        render::AdapterChoice::LowPower => "low-power",
    };
    format!(
        "dev: adapter asked {asked}, got {} ({:?}, {:?})",
        info.name, info.device_type, info.backend
    )
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("roots", &self.roots)
            .field("logs", &self.logs.len())
            .finish_non_exhaustive()
    }
}

/// Loads, checks and compiles the scenario `file`, making its output
/// folder. Nothing else is opened.
///
/// # Errors
///
/// Why the file is refused, under `scenario <file>`.
pub fn load(file: &Path) -> anyhow::Result<Scenario> {
    let context = || format!("scenario {}", file.display());
    let read = || super::read_limited(fs::File::open(file)?, MAX_FILE);
    let bytes = read().with_context(context)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("not UTF-8"))
        .with_context(context)?;
    let base = std::path::absolute(file)
        .ok()
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .with_context(context)?;
    parse(&text, &base)
        .map_err(anyhow::Error::msg)
        .with_context(context)
}

/// [`load`] on the file's `text`, its relative paths resolved against
/// `base`.
fn parse(text: &str, base: &Path) -> Result<Scenario, String> {
    let header: Header = toml::from_str(text).map_err(|e| e.to_string())?;
    match header.version {
        None => return Err(format!("no version; this build reads version {VERSION}")),
        Some(toml::Value::Integer(VERSION)) => {}
        Some(v) => {
            return Err(format!(
                "version {v} is not supported; this build reads version {VERSION}"
            ));
        }
    }
    let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
    if file.step.len() > usize::from(u16::MAX) {
        return Err(format!("more than {} steps", u16::MAX));
    }
    let size = file
        .size
        .map(|[w, h]| super::checked_size(w, h))
        .transpose()?;
    let adapter = file.adapter.unwrap_or(Adapter::Warp);
    let roots = roots(&file.library.roots, base)?;
    let compiled = compile(&file, base, &roots)?;
    let output = match &file.output {
        Some(o) => Some(make_output(base, o, &compiled.captures)?),
        None => None,
    };
    let captures = compiled
        .captures
        .into_iter()
        .map(|c| Capture {
            axis: c.axis,
            path: output
                .as_deref()
                .map_or_else(PathBuf::new, |o| o.join(&c.file)),
            origin: c.origin,
            hold: c.hold,
            local: c.local,
            state: CaptureState::Unarmed,
            reported: false,
        })
        .collect();
    Ok(Scenario {
        rescan_ms: file.library.rescan_ms,
        script: SelectScript::compiled(compiled.steps),
        session: Session {
            roots: roots.into_iter().map(|(path, _)| path).collect(),
            logs: compiled.logs,
            captures: Captures(Arc::new(Mutex::new(captures))),
            size,
            adapter,
        },
    })
}

/// The axis a `capture` step names, exactly one of `at`, `ready_at`,
/// `closing_at`, `loading = true`, `select_at` and `result_at`, and
/// whether it holds: `select_at` and `result_at` do unless `hold = false`;
/// the others take no `hold`.
fn capture_axis(c: &CaptureSpec) -> Result<(Axis, bool), String> {
    let ms = |v: i64, name: &str| {
        if v < 0 && name != "at" {
            return Err(format!("{name} must be 0 or more"));
        }
        v.checked_mul(1_000)
            .map(TimeUs)
            .ok_or_else(|| format!("{name} is too large"))
    };
    let axis = match (
        c.at,
        c.ready_at,
        c.closing_at,
        c.loading,
        c.loading_at,
        c.select_at,
        c.result_at,
    ) {
        (Some(at), None, None, None, None, None, None) => Axis::Chart(ms(at, "at")?),
        (None, Some(r), None, None, None, None, None) => Axis::Ready(ms(r, "ready_at")?),
        (None, None, Some(c), None, None, None, None) => Axis::Closing(ms(c, "closing_at")?),
        (None, None, None, Some(true), None, None, None) => Axis::Loading,
        (None, None, None, None, Some(l), None, None) => Axis::LoadingAt(ms(l, "loading_at")?),
        (None, None, None, None, None, Some(s), None) => Axis::Select(ms(s, "select_at")?),
        (None, None, None, None, None, None, Some(r)) => Axis::Result(ms(r, "result_at")?),
        _ => {
            return Err(
                "give one of at, ready_at, closing_at, loading = true, loading_at, \
                 select_at and result_at"
                    .to_owned(),
            );
        }
    };
    match (axis, c.hold) {
        (Axis::Select(_) | Axis::Result(_), hold) => Ok((axis, hold.unwrap_or(true))),
        (_, None) => Ok((axis, false)),
        (_, Some(_)) => Err("hold is for select_at and result_at".to_owned()),
    }
}

/// `p` against `base`, absolute and normalized.
fn resolve(base: &Path, p: &Path) -> Result<PathBuf, String> {
    std::path::absolute(base.join(p)).map_err(|e| format!("{}: {e}", p.display()))
}

/// The library roots, each with its identity; refused when one is not a
/// folder or two are the same folder.
fn roots(given: &[PathBuf], base: &Path) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    if given.is_empty() {
        return Err("library.roots is empty".to_owned());
    }
    let mut roots: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(given.len());
    for r in given {
        let path = resolve(base, r)?;
        if !path.is_dir() {
            return Err(format!("library root {} is not a folder", path.display()));
        }
        let identity = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some((other, _)) = roots.iter().find(|(_, id)| *id == identity) {
            return Err(format!(
                "library roots {} and {} are the same folder",
                other.display(),
                path.display()
            ));
        }
        roots.push((path, identity));
    }
    Ok(roots)
}

/// The steps compiled, the logs decoded and the captures in file order.
struct Compiled {
    steps: Vec<(SelectStep, Origin)>,
    logs: Vec<ScriptedInput>,
    captures: Vec<CompiledCapture>,
}

/// A capture as its step compiles.
struct CompiledCapture {
    /// Its file, under the output.
    file: PathBuf,
    axis: Axis,
    origin: Origin,
    hold: bool,
    /// A `result_at` written on a result: that result's.
    local: bool,
}

/// Where the sequence of steps stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seq {
    /// On the select screen.
    Select,
    /// A chart chosen, its first play due.
    Chosen,
    /// A play of the chosen chart due again.
    Replay,
    /// On a result.
    Result,
    /// After `quit`.
    End,
}

fn compile(file: &File, base: &Path, roots: &[(PathBuf, PathBuf)]) -> Result<Compiled, String> {
    let mut steps = Vec::new();
    let mut logs = Vec::new();
    // Each log decoded once, by its path; a generated one by what it is.
    let mut log_paths: Vec<LogKey> = Vec::new();
    let mut captures: Vec<CompiledCapture> = Vec::new();
    // Captures no play has followed yet.
    let mut waiting: Vec<Origin> = Vec::new();
    // `result_at` captures waiting for the next play's result.
    let mut results: Vec<(Origin, bool)> = Vec::new();
    // The holding captures of the result shown.
    let mut holds: Vec<Origin> = Vec::new();
    let mut seq = Seq::Select;
    // The chosen chart's input and the step that set it.
    let mut chart_input: Option<(InputRef, Origin)> = None;
    let mut last: Option<Origin> = None;
    for (i, step) in file.step.iter().enumerate() {
        let index = u16::try_from(i + 1).map_err(|_| format!("more than {} steps", u16::MAX))?;
        let o = Origin {
            index,
            kind: step.kind(),
        };
        let at = |why: String| format!("{o}: {why}");
        let out_of_place = || match last {
            Some(m) => at(format!("{} cannot follow {m}", o.kind)),
            None => at(format!("{} cannot come first", o.kind)),
        };
        seq = match (seq, step) {
            (Seq::Select, Step::Select(s)) => {
                match (&s.path, &s.search) {
                    (Some(p), None) => {
                        let target = target(base, p, roots).map_err(at)?;
                        steps.push((SelectStep::PickPath(target), o));
                    }
                    (None, Some(text)) => {
                        steps.push((SelectStep::Query(text.clone()), o));
                        steps.push((SelectStep::PickSearched(text.clone()), o));
                    }
                    _ => return Err(at("give either path or search".to_owned())),
                }
                if s.decision == Some(DecisionSpec::Skin) {
                    steps.push((SelectStep::DecisionReady, o));
                }
                steps.push((SelectStep::Enter, o));
                chart_input = None;
                Seq::Chosen
            }
            (Seq::Select, Step::Cursor(s)) => {
                match (&s.path, &s.search) {
                    (Some(p), None) => {
                        let target = target(base, p, roots).map_err(at)?;
                        steps.push((SelectStep::PickPath(target), o));
                    }
                    (None, Some(text)) => {
                        steps.push((SelectStep::Query(text.clone()), o));
                        steps.push((SelectStep::PickSearched(text.clone()), o));
                    }
                    _ => return Err(at("give either path or search".to_owned())),
                }
                Seq::Select
            }
            (Seq::Chosen | Seq::Replay, Step::Play(p)) => {
                // A log by its path, or one generated: the index it has, or
                // gets once made.
                let key = match &p.input {
                    InputSpec::Named(name) if name == "auto" => None,
                    InputSpec::Named(name) => {
                        Some(LogKey::Path(resolve(base, Path::new(name)).map_err(at)?))
                    }
                    InputSpec::Generated(GeneratedSpec {
                        miss: MissWord::All,
                    }) => Some(LogKey::NoPress),
                };
                let log = key.map(|key| {
                    let k = log_paths.iter().position(|l| *l == key);
                    (k.unwrap_or(log_paths.len()), k.is_none().then_some(key))
                });
                let input = match &log {
                    None => InputRef::Auto,
                    // Below the step count, itself within u16.
                    Some((k, _)) => InputRef::Log(
                        u16::try_from(*k).map_err(|_| at("too many logs".to_owned()))?,
                    ),
                };
                match chart_input {
                    Some((first, from)) if first != input => {
                        return Err(at(format!(
                            "input differs from step {}'s; select the chart again",
                            from.index
                        )));
                    }
                    Some(_) => {}
                    None => chart_input = Some((input, o)),
                }
                if let Some((_, Some(key))) = log {
                    logs.push(match &key {
                        LogKey::Path(path) => {
                            super::script(path, &[]).map_err(|e| at(format!("{e:#}")))?
                        }
                        // One keyboard that presses nothing, as a log of no
                        // events plays.
                        LogKey::NoPress => ScriptedInput::from_log(
                            &eval::InputStart::default(),
                            &[],
                            &[input::DeviceKind::Keyboard],
                        ),
                    });
                    log_paths.push(key);
                }
                let until = match &p.until {
                    Until::End(EndWord::End) => None,
                    Until::At(a) => {
                        if a.at_ms < 0 {
                            return Err(at("at_ms must be 0 or more".to_owned()));
                        }
                        let us = a
                            .at_ms
                            .checked_mul(1_000)
                            .ok_or_else(|| at("at_ms is too large".to_owned()))?;
                        let command = match a.command {
                            CommandWord::Retry => UntilCommand::Retry,
                            CommandWord::Quit => UntilCommand::Quit,
                            CommandWord::Skip => UntilCommand::Skip,
                        };
                        Some((TimeUs(us), command))
                    }
                };
                steps.push((SelectStep::Play(PlayStep { input, until }), o));
                waiting.clear();
                // A quit, at chart time 0 or later, ends the play midway on
                // its result, as a skip does once it is complete.
                match until {
                    None | Some((_, UntilCommand::Quit | UntilCommand::Skip)) => {
                        holds = results
                            .iter()
                            .filter(|(_, h)| *h)
                            .map(|(c, _)| *c)
                            .collect();
                        results.clear();
                        Seq::Result
                    }
                    Some((_, UntilCommand::Retry)) => {
                        if let Some((c, _)) = results.first() {
                            return Err(format!(
                                "{c}: no result follows: step {} ends in a retry",
                                o.index
                            ));
                        }
                        Seq::Replay
                    }
                }
            }
            (Seq::Result, Step::Result(r)) => {
                if let (ResultKind::Stay, Some(h)) = (r, holds.first()) {
                    return Err(format!(
                        "{h}: a holding capture cannot wait on stay, which leaves on its own; \
                         give hold = false, or leave with back"
                    ));
                }
                let (step, next) = match r {
                    ResultKind::NextPreset => (SelectStep::Next, Seq::Result),
                    ResultKind::PrevPreset => (SelectStep::Prev, Seq::Result),
                    ResultKind::Retry => (SelectStep::Retry, Seq::Replay),
                    ResultKind::Back => (SelectStep::Back, Seq::Select),
                    ResultKind::Stay => (SelectStep::Stay, Seq::Select),
                };
                if next != Seq::Result {
                    holds.clear();
                }
                steps.push((step, o));
                next
            }
            (Seq::Select | Seq::Result, Step::Capture(c)) => {
                let (axis, hold) = capture_axis(c).map_err(at)?;
                let rel = capture_file(&c.file).map_err(at)?;
                let key = rel.to_string_lossy().to_lowercase();
                if let Some(other) = captures
                    .iter()
                    .find(|other| other.file.to_string_lossy().to_lowercase() == key)
                {
                    return Err(at(format!(
                        "{} is also step {}'s file",
                        c.file.display(),
                        other.origin.index
                    )));
                }
                let k = u16::try_from(captures.len())
                    .map_err(|_| at("too many captures".to_owned()))?;
                // A selection's needs the select screen; a result's written
                // on a result is that result's, else the next play's.
                let local = match (axis, seq) {
                    (Axis::Select(_), Seq::Result) => {
                        return Err(at("select_at needs the select screen".to_owned()));
                    }
                    (Axis::Select(_), _) => false,
                    (Axis::Result(_), Seq::Result) => {
                        if hold {
                            holds.push(o);
                        }
                        true
                    }
                    (Axis::Result(_), _) => {
                        results.push((o, hold));
                        waiting.push(o);
                        false
                    }
                    _ => {
                        waiting.push(o);
                        false
                    }
                };
                captures.push(CompiledCapture {
                    file: rel,
                    axis,
                    origin: o,
                    hold,
                    local,
                });
                steps.push((SelectStep::Capture(k), o));
                seq
            }
            (Seq::Select, Step::Practice(p)) => {
                if let Some((c, _)) = results.first() {
                    return Err(format!(
                        "{c}: a practice shows no result: step {} practices",
                        o.index
                    ));
                }
                let goal = practice_goal(p, base, &mut log_paths, &mut logs).map_err(at)?;
                match (&p.path, &p.search) {
                    (Some(path), None) => {
                        let target = target(base, path, roots).map_err(at)?;
                        steps.push((SelectStep::PickPath(target), o));
                    }
                    (None, Some(text)) => {
                        steps.push((SelectStep::Query(text.clone()), o));
                        steps.push((SelectStep::PickSearched(text.clone()), o));
                    }
                    _ => return Err(at("give either path or search".to_owned())),
                }
                // One more than the rounds fits: the count reaches it at the
                // boundary after the last, where the rounds are left.
                let n = u16::try_from(p.rounds)
                    .ok()
                    .filter(|&n| n >= 1 && n.checked_add(1).is_some())
                    .ok_or_else(|| at(format!("rounds must be 1 to {}", u16::MAX - 1)))?;
                if p.decision == Some(DecisionSpec::Skin) {
                    steps.push((SelectStep::DecisionReady, o));
                }
                steps.push((SelectStep::PracticeKey(goal.input), o));
                steps.push((SelectStep::PracticeSet(goal), o));
                steps.push((SelectStep::PracticeStart, o));
                steps.push((SelectStep::Rounds(n), o));
                steps.push((SelectStep::PracticeBack, o));
                waiting.clear();
                chart_input = None;
                Seq::Select
            }
            (Seq::Select | Seq::Result, Step::Quit(Empty {})) => {
                steps.push((SelectStep::Close, o));
                Seq::End
            }
            _ => return Err(out_of_place()),
        };
        last = Some(o);
    }
    if let Some(o) = waiting.first() {
        return Err(format!("{o}: no play follows"));
    }
    match last {
        Some(o) if seq != Seq::End => return Err(format!("{o}: the last step must be quit")),
        None => return Err("no steps".to_owned()),
        Some(_) => {}
    }
    if !captures.is_empty() && file.output.is_none() {
        return Err("output is required for captures".to_owned());
    }
    Ok(Compiled {
        steps,
        logs,
        captures,
    })
}

/// A compiled log: read from its path, or made for the chart.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LogKey {
    Path(PathBuf),
    /// No key pressed.
    NoPress,
}

/// A `practice` step's page settings and input, checked: measures from 1
/// with `to ≥ from`, a speed on the ladder in percent, a lead-in on the
/// page's grid, a gauge value in percent, repeat on, and an input log (an
/// autoplay session cannot practice), decoded once by its path.
fn practice_goal(
    p: &PracticeSpec,
    base: &Path,
    log_paths: &mut Vec<LogKey>,
    logs: &mut Vec<ScriptedInput>,
) -> Result<PracticeGoal, String> {
    let from = p
        .from
        .checked_sub(1)
        .and_then(|f| usize::try_from(f).ok())
        .ok_or("from must be 1 or more")?;
    let to = usize::try_from(p.to)
        .ok()
        .filter(|&t| t > from)
        .ok_or("to must be from or more")?;
    let speed = (p.speed % 5 == 0)
        .then(|| u32::try_from(p.speed / 5).ok())
        .flatten()
        .and_then(Speed::from_twentieths)
        .ok_or("speed must be a multiple of 5 from 50 to 150")?;
    let lead_in = p
        .lead_in_ms
        .checked_mul(1_000)
        .map(TimeUs)
        .filter(|l| {
            (game::MIN_LEAD_IN..=game::MAX_LEAD_IN).contains(l) && l.0 % game::LEAD_IN_STEP.0 == 0
        })
        .ok_or("lead_in_ms must be from 1000 to 10000 in steps of 500")?;
    let value = p
        .gauge
        .checked_mul(GaugeValue::PERCENT)
        .filter(|_| (0..=100).contains(&p.gauge))
        .map(GaugeValue)
        .ok_or("gauge must be a percentage from 0 to 100")?;
    if !p.repeat {
        return Err("repeat = false is not supported; give repeat = true".to_owned());
    }
    if p.input == "auto" {
        return Err("input cannot be auto: an autoplay session cannot practice".to_owned());
    }
    let path = resolve(base, Path::new(&p.input))?;
    let key = LogKey::Path(path.clone());
    let k = match log_paths.iter().position(|l| *l == key) {
        Some(k) => k,
        None => {
            logs.push(super::script(&path, &[]).map_err(|e| format!("{e:#}"))?);
            log_paths.push(key);
            log_paths.len() - 1
        }
    };
    let input = InputRef::Log(u16::try_from(k).map_err(|_| "too many logs".to_owned())?);
    Ok(PracticeGoal {
        from,
        to,
        speed,
        lead_in,
        value,
        repeat: true,
        input,
    })
}

/// A `select { path }`: the file must exist under a root.
fn target(base: &Path, p: &Path, roots: &[(PathBuf, PathBuf)]) -> Result<Target, String> {
    let path = resolve(base, p)?;
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let identity = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !roots.iter().any(|(_, root)| identity.starts_with(root)) {
        return Err(format!("{} is under no library root", path.display()));
    }
    Ok(Target { path, identity })
}

/// Whether `p` is only normal components: it stays below the folder it is
/// joined to.
fn stays_below(p: &Path) -> bool {
    // A component holding a colon would be a drive or a stream once
    // joined (`a/C:b` pushes `C:b`, which replaces the path).
    p.components().next().is_some()
        && p.components().all(|c| match c {
            Component::Normal(n) => !n.to_string_lossy().contains(':'),
            _ => false,
        })
}

/// A capture's file, relative to the output folder.
fn capture_file(file: &Path) -> Result<PathBuf, String> {
    if !stays_below(file) {
        return Err(format!("{} leaves the output folder", file.display()));
    }
    if !file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        return Err(format!("{}: file must end in .png", file.display()));
    }
    Ok(file.to_path_buf())
}

/// Whether `meta` is a link: a symbolic link or any other reparse point
/// (a junction).
fn is_link(meta: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Makes the output folder `output` under `base`, and the folders of the
/// captures in it: the folders on the way may exist but be no link, and
/// the output itself must not exist. Returns its path.
fn make_output(
    base: &Path,
    output: &Path,
    captures: &[CompiledCapture],
) -> Result<PathBuf, String> {
    if !stays_below(output) {
        return Err(format!(
            "output {} leaves the scenario's folder",
            output.display()
        ));
    }
    let mut at = base.to_path_buf();
    let parts: Vec<_> = output.components().collect();
    for (i, c) in parts.iter().enumerate() {
        at.push(c);
        let last = i + 1 == parts.len();
        match fs::symlink_metadata(&at) {
            Ok(meta) if is_link(&meta) => {
                return Err(format!("output {} is a link", at.display()));
            }
            Ok(_) if last => return Err(format!("output {} already exists", at.display())),
            Ok(meta) if !meta.is_dir() => {
                return Err(format!("output {} is not a folder", at.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&at).map_err(|e| format!("output {}: {e}", at.display()))?;
            }
            Err(e) => return Err(format!("output {}: {e}", at.display())),
        }
    }
    // Inside a folder this run just made: no link can be there.
    for c in captures {
        if let Some(dir) = c.file.parent().filter(|d| !d.as_os_str().is_empty()) {
            let dir = at.join(dir);
            fs::create_dir_all(&dir)
                .map_err(|e| format!("{}: {}: {e}", c.origin, dir.display()))?;
        }
    }
    Ok(at)
}

/// The index of the listed file that is `target`: first by its path as
/// listed, ignoring ASCII case; else by identity, each listed file's looked
/// up at most once per step in `identities`.
pub fn find<'a>(
    listed: impl Iterator<Item = &'a Path> + Clone,
    target: &Target,
    identities: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> Option<usize> {
    let wanted = target.path.as_os_str();
    if let Some(i) = listed
        .clone()
        .position(|p| p.as_os_str().eq_ignore_ascii_case(wanted))
    {
        return Some(i);
    }
    listed.into_iter().position(|p| {
        identities
            .entry(p.to_path_buf())
            .or_insert_with(|| fs::canonicalize(p).ok())
            .as_ref()
            == Some(&target.identity)
    })
}

/// How the captures of a play ended, as [`Captures::settle`] reports
/// them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Each written one: its step and file.
    pub written: Vec<(Origin, PathBuf)>,
    /// Each failed one: its step and why.
    pub failed: Vec<(Origin, String)>,
}

/// How far a capture has come.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    /// Its step has not run.
    Unarmed,
    /// Its step ran: the next play that starts takes it.
    Pending,
    /// A play of this generation takes it.
    Bound(u64),
    /// A `select_at` capture armed by the select screen's update of this
    /// lifetime and revision, published after the steps before it: a
    /// selection frame that applied it or a later one takes it, the first
    /// such frame presented being its origin.
    Selecting {
        /// The update's lifetime.
        lifetime: u64,
        /// The update's revision.
        revision: u64,
        /// When the first frame that applied it was presented.
        origin: Option<TimeUs>,
    },
    /// A `result_at` capture bound to the play of this generation, until
    /// its result begins.
    AwaitingResult(u64),
    /// A `result_at` capture of the result numbered `seq`: a frame of it
    /// that drew an end page numbered `page` or later and, when given, a
    /// view keyed `view` or later takes it.
    Resulting {
        /// The result's number.
        seq: u64,
        /// The end page published after the steps before it.
        page: u64,
        /// The view desired after the steps before it.
        view: Option<u64>,
    },
    /// The render thread writes it from a frame of this generation
    /// ([`SCREEN_SAVE`] for a selection or result frame).
    Saving(u64),
    /// Written.
    Succeeded,
    /// Not written, and why.
    Failed(String),
}

/// What a capture waits for, of the play it is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// The first presented frame at or after this chart time.
    Chart(TimeUs),
    /// The first presented frame at or after this time since its READY
    /// state began, of the attempt's first candidate: a frame past READY
    /// or of a rearmed candidate fails it (design.md "Play Lifecycle").
    Ready(TimeUs),
    /// The first presented frame at or after this time since Closing
    /// began, as the frame's view gives Closing's start (design.md "Play
    /// Lifecycle", Closing): no presented frame is guaranteed.
    Closing(TimeUs),
    /// The first presented frame of its loading state.
    Loading,
    /// The first presented frame of its loading state at or after this
    /// time on that state's clock, since its loading lifetime's origin
    /// (the screen clock's, docs/stage3.md D3): a play frame of its play
    /// first fails it.
    LoadingAt(TimeUs),
    /// The first presented selection frame at or after this time since
    /// the first that applied the update its step was armed with.
    Select(TimeUs),
    /// The first presented frame of its result at or after this time
    /// since that result's first presented frame.
    Result(TimeUs),
}

impl Axis {
    const fn name(self) -> &'static str {
        match self {
            Self::Chart(_) => "chart",
            Self::Ready(_) => "ready",
            Self::Closing(_) => "closing",
            Self::Loading => "loading",
            Self::LoadingAt(_) => "loading_at",
            Self::Select(_) => "select",
            Self::Result(_) => "result",
        }
    }

    const fn target(self) -> i64 {
        match self {
            Self::Chart(t)
            | Self::Ready(t)
            | Self::Closing(t)
            | Self::LoadingAt(t)
            | Self::Select(t)
            | Self::Result(t) => t.0,
            Self::Loading => 0,
        }
    }

    /// The screen whose leaving fails it, by name.
    const fn screen(self) -> &'static str {
        match self {
            Self::Select(_) => "selection",
            _ => "result",
        }
    }
}

/// The generation a selection or result frame is saved under: no play's.
pub const SCREEN_SAVE: u64 = u64::MAX;

/// How a selection or a result was left, for the captures it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaving {
    /// It was left: a choice, back, its scene time, a retry, practice.
    Left,
    /// The run ended.
    Ended,
}

/// A capture as the step that runs it sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureStatus {
    /// What it waits for.
    pub axis: Axis,
    /// Whether it holds the next step.
    pub hold: bool,
    /// A `result_at` written on a result: that result's.
    pub local: bool,
    /// How far it has come.
    pub state: CaptureState,
}

impl CaptureStatus {
    /// Whether it has ended: written or failed.
    #[must_use]
    pub const fn ended(&self) -> bool {
        matches!(
            self.state,
            CaptureState::Succeeded | CaptureState::Failed(_)
        )
    }
}

/// One capture: a frame of the play it is bound to, as its axis says,
/// written to `path`, with what the frame was beside it.
#[derive(Clone, Debug)]
pub struct Capture {
    /// What it waits for.
    pub axis: Axis,
    /// The file, created new.
    pub path: PathBuf,
    /// Its step.
    pub origin: Origin,
    /// Whether it holds the next step until it ends (`select_at` and
    /// `result_at` unless `hold = false`).
    pub hold: bool,
    /// A `result_at` written on a result: that result's, not the next
    /// play's.
    pub local: bool,
    /// How far it has come.
    pub state: CaptureState,
    /// Whether the game thread has reported how it ended.
    reported: bool,
}

/// The run's captures, shared by the game thread, which arms, binds and
/// settles them, and the render thread, which writes them. Neither holds
/// the lock while it waits for or works on anything else.
#[derive(Clone, Debug, Default)]
pub struct Captures(Arc<Mutex<Vec<Capture>>>);

impl Captures {
    /// The list, whether or not a holder panicked.
    fn lock(&self) -> MutexGuard<'_, Vec<Capture>> {
        match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The list if it is free now; `None` when the other side holds it.
    fn try_lock(&self) -> Option<MutexGuard<'_, Vec<Capture>>> {
        match self.0.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Capture `k`'s step ran: the next play that starts takes it.
    pub fn arm(&self, k: u16) {
        if let Some(c) = self.lock().get_mut(usize::from(k))
            && c.state == CaptureState::Unarmed
        {
            c.state = CaptureState::Pending;
        }
    }

    /// The play of `generation` starts: it takes every capture armed, a
    /// `result_at` one for the result it shows.
    pub fn bind(&self, generation: u64) {
        for c in self.lock().iter_mut() {
            if c.state == CaptureState::Pending {
                c.state = match c.axis {
                    Axis::Result(_) => CaptureState::AwaitingResult(generation),
                    _ => CaptureState::Bound(generation),
                };
            }
        }
    }

    /// Capture `k`, as its step sees it.
    pub fn status(&self, k: u16) -> Option<CaptureStatus> {
        self.lock().get(usize::from(k)).map(|c| CaptureStatus {
            axis: c.axis,
            hold: c.hold,
            local: c.local,
            state: c.state.clone(),
        })
    }

    /// Capture `k`, a `select_at`, is armed with the select screen's
    /// update published after the steps before it.
    pub fn arm_select(&self, k: u16, lifetime: u64, revision: u64) {
        if let Some(c) = self.lock().get_mut(usize::from(k))
            && c.state == CaptureState::Unarmed
        {
            c.state = CaptureState::Selecting {
                lifetime,
                revision,
                origin: None,
            };
        }
    }

    /// Capture `k`, a `result_at` written on result `seq`, is armed with
    /// the end page and view published after the steps before it.
    pub fn arm_result(&self, k: u16, seq: u64, page: u64, view: Option<u64>) {
        if let Some(c) = self.lock().get_mut(usize::from(k))
            && c.state == CaptureState::Unarmed
        {
            c.state = CaptureState::Resulting { seq, page, view };
        }
    }

    /// Result `id` begins: the `result_at` captures bound to its play are
    /// its.
    pub fn begin_result(&self, id: ResultId) {
        for c in self.lock().iter_mut() {
            if c.state == CaptureState::AwaitingResult(id.generation) {
                c.state = CaptureState::Resulting {
                    seq: id.seq,
                    page: 0,
                    view: None,
                };
            }
        }
    }

    /// Whether a holding capture of the selection (`None`) or of result
    /// `seq` has not ended.
    pub fn held(&self, result: Option<u64>) -> bool {
        self.lock()
            .iter()
            .any(|c| c.hold && Self::of_screen(c, result))
    }

    /// Whether `c` is a capture of the selection (`None`) or of result
    /// `seq` that has not ended, one being written included.
    fn of_screen(c: &Capture, result: Option<u64>) -> bool {
        match (&c.state, result, c.axis) {
            (CaptureState::Selecting { .. }, None, _)
            | (CaptureState::Saving(SCREEN_SAVE), None, Axis::Select(_)) => true,
            (CaptureState::Resulting { seq, .. }, Some(r), _) => *seq == r,
            (CaptureState::Saving(SCREEN_SAVE), Some(_), Axis::Result(_)) => true,
            _ => false,
        }
    }

    /// The selection (`None`) or result `seq` was left as `how`: each of
    /// its captures whose frame was not taken fails; one being written is
    /// left to its writer. Returns how those not reported yet ended.
    pub fn leave(&self, result: Option<u64>, how: Leaving) -> Settled {
        let mut list = self.lock();
        for c in list.iter_mut() {
            let waiting = Self::waiting_on(c, result);
            if waiting {
                let what = match how {
                    Leaving::Left => format!("the {} left", c.axis.screen()),
                    Leaving::Ended => "the run ended".to_owned(),
                };
                c.state = CaptureState::Failed(format!(
                    "{what} before a frame at {} ms on its {} axis",
                    c.axis.target() / 1_000,
                    c.axis.name()
                ));
            }
        }
        Self::report(&mut list)
    }

    /// The holding captures of the selection (`None`) or of result `seq`
    /// whose frame was not taken fail, the render bound passed. Returns
    /// how those not reported yet ended.
    pub fn expire(&self, result: Option<u64>) -> Settled {
        let mut list = self.lock();
        for c in list.iter_mut() {
            let waiting = Self::waiting_on(c, result);
            if c.hold && waiting {
                c.state = CaptureState::Failed(format!(
                    "the capture did not resolve within the render bound ({} ms on its {} axis)",
                    c.axis.target() / 1_000,
                    c.axis.name()
                ));
            }
        }
        Self::report(&mut list)
    }

    /// Whether `c` waits for a frame of the selection (`None`) or of
    /// result `seq`: armed, its frame not taken.
    fn waiting_on(c: &Capture, result: Option<u64>) -> bool {
        match (&c.state, result) {
            (CaptureState::Selecting { .. }, None) => true,
            (CaptureState::Resulting { seq, .. }, Some(r)) => *seq == r,
            _ => false,
        }
    }

    /// The captures that ended and were not reported yet, now reported.
    fn report(list: &mut [Capture]) -> Settled {
        let mut settled = Settled::default();
        for c in list.iter_mut().filter(|c| !c.reported) {
            match &c.state {
                CaptureState::Succeeded => settled.written.push((c.origin, c.path.clone())),
                CaptureState::Failed(why) => settled.failed.push((c.origin, why.clone())),
                _ => continue,
            }
            c.reported = true;
        }
        settled
    }

    /// The play of `generation` is over, its frames drawn when `answered`
    /// (the render thread answered its end): each capture bound to it ends
    /// here. Returns how those not reported yet ended; `Err` when the
    /// render thread holds the list (it did not answer).
    pub fn settle(&self, generation: u64, answered: bool) -> Result<Settled, ()> {
        let mut list = self.try_lock().ok_or(())?;
        for c in list.iter_mut() {
            match c.state {
                CaptureState::Bound(g) if g == generation => {
                    c.state = CaptureState::Failed(if answered {
                        format!(
                            "the play ended before a frame at {} ms on its {} axis",
                            c.axis.target() / 1_000,
                            c.axis.name()
                        )
                    } else {
                        "the render thread did not answer".to_owned()
                    });
                }
                CaptureState::Saving(g) if g == generation => {
                    c.state = CaptureState::Failed("the render thread did not answer".to_owned());
                }
                _ => {}
            }
        }
        Ok(Self::report(&mut list))
    }

    /// After the render thread has ended: each capture that did not end,
    /// or whose failure was not reported, with why. A list the render
    /// thread still holds (it is stuck) leaves every capture unknown.
    pub fn unfinished(&self) -> Vec<String> {
        let Some(list) = self.try_lock() else {
            return vec!["the captures are held by the render thread".to_owned()];
        };
        list.iter()
            .filter_map(|c| match &c.state {
                CaptureState::Pending
                | CaptureState::Bound(_)
                | CaptureState::Saving(_)
                | CaptureState::Selecting { .. }
                | CaptureState::AwaitingResult(_)
                | CaptureState::Resulting { .. } => {
                    Some(format!("{}: {} did not run", c.origin, c.path.display()))
                }
                CaptureState::Failed(why) if !c.reported => Some(format!("{}: {why}", c.origin)),
                _ => None,
            })
            .collect()
    }

    /// Every capture as it stands (tests).
    #[cfg(test)]
    pub fn states(&self) -> Vec<CaptureState> {
        self.lock().iter().map(|c| c.state.clone()).collect()
    }

    /// A list of captures (tests).
    #[cfg(test)]
    pub fn of(captures: Vec<(TimeUs, PathBuf)>) -> Self {
        let origin = Origin {
            index: 1,
            kind: StepKind::Capture,
        };
        Self(Arc::new(Mutex::new(
            captures
                .into_iter()
                .map(|(at, path)| Capture {
                    axis: Axis::Chart(at),
                    path,
                    origin,
                    hold: false,
                    local: false,
                    state: CaptureState::Unarmed,
                    reported: false,
                })
                .collect(),
        )))
    }
}

/// Writes `bytes` to a new file at `path`: an existing file, folder or
/// link there fails it, and nothing is ever replaced.
///
/// # Errors
///
/// Why the file cannot be created or written.
pub fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The render thread's frame sink of a scenario: each presented play frame
/// writes the captures it is due for, taking them out of the list first
/// and putting the outcome back after, so the list is never held while the
/// frame is read back, encoded or written. The render thread only reads
/// the frame back (and its display list); the sink's writer thread encodes
/// and writes it, so the encoding, which takes seconds on a loaded
/// machine, delays no later frame and no capture due on one. At most
/// [`WRITE_FRAMES`] frames and [`WRITE_BYTES`] bytes wait for or are in the
/// writer: a frame past either fails its captures at once, so the render
/// thread never waits for the writer. The play's end waits for the
/// writer's backlog ([`FrameSink::finish`]).
pub struct ScenarioFrames {
    captures: Captures,
    /// How long each capture's read-back stalls first: a test's stand-in
    /// for a render thread stuck in its work ([`STALL_ENV`]).
    stall: Option<Duration>,
    /// The presented frames so far: captures of one number are one frame.
    frame: u64,
    /// The writer, started with the first captured frame.
    writer: Option<Writer>,
    /// What waits for or is in the writer.
    backlog: Arc<(Mutex<Backlog>, Condvar)>,
    /// The bytes the backlog may hold: [`WRITE_BYTES`], less in tests.
    write_bytes: usize,
    /// The newest result seen, by its number, and when its first frame
    /// was presented: its captures' origin.
    result_origin: Option<(u64, TimeUs)>,
}

/// The captured frames that may wait for or be in the writer at once.
const WRITE_FRAMES: usize = 4;

/// The bytes of captured frames (their pixels and display lists) that may
/// wait for or be in the writer at once: one frame of the largest target.
/// One frame is taken whatever its size when nothing waits.
const WRITE_BYTES: usize = super::MAX_SIZE_SIDE as usize * super::MAX_SIZE_SIDE as usize * 4;

/// What waits for or is in the writer.
#[derive(Debug, Default)]
struct Backlog {
    frames: usize,
    bytes: usize,
}

/// A captured frame for the writer: its pixels, display list, and each
/// capture due on it (its index in the list, file, sidecar, generation).
struct Job {
    pixels: Option<(u32, u32, Vec<u8>)>,
    list: Option<Result<String, String>>,
    due: Vec<(usize, PathBuf, String, u64)>,
    /// Its reservation in the backlog, released when the job is dropped:
    /// written, discarded by a writer that ended, or never sent.
    bytes: usize,
    backlog: Arc<(Mutex<Backlog>, Condvar)>,
    captures: Captures,
}

impl Job {
    /// Each of its captures still saving fails, saying `why`.
    fn fail(&self, why: &str) {
        let mut list = self.captures.lock();
        for (i, path, _, generation) in &self.due {
            if let Some(c) = list.get_mut(*i)
                && c.state == CaptureState::Saving(*generation)
            {
                c.state = CaptureState::Failed(format!("{}: {why}", path.display()));
            }
        }
    }
}

impl Drop for Job {
    /// A job dropped before its captures were written (its writer ended,
    /// by a panic, with it current or queued) fails them; any job leaves
    /// the backlog, and a waiting end is woken.
    fn drop(&mut self) {
        self.fail("its writer ended before writing it");
        let (lock, done) = &*self.backlog;
        let mut b = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        b.frames -= 1;
        b.bytes -= self.bytes;
        done.notify_all();
    }
}

/// The sink's writer thread and its queue.
struct Writer {
    jobs: mpsc::Sender<Job>,
    thread: JoinHandle<()>,
}

/// The environment variable that stalls each capture's read-back by its
/// value in milliseconds, for tests of a stuck render thread.
pub const STALL_ENV: &str = "TAPESTRY_DEV_CAPTURE_STALL_MS";

impl Captures {
    /// Capture `i` waits on `axis` (tests).
    #[cfg(test)]
    pub fn set_axis(&self, i: usize, axis: Axis) {
        if let Some(c) = self.lock().get_mut(i) {
            c.axis = axis;
        }
    }

    /// Capture `i` holds (tests).
    #[cfg(test)]
    pub fn set_hold(&self, i: usize, hold: bool) {
        if let Some(c) = self.lock().get_mut(i) {
            c.hold = hold;
        }
    }

    /// Capture `i` is a capture written on a result (tests).
    #[cfg(test)]
    pub fn set_local(&self, i: usize) {
        if let Some(c) = self.lock().get_mut(i) {
            c.local = true;
        }
    }

    /// Capture `i` is in `state` (tests).
    #[cfg(test)]
    pub fn set_state(&self, i: usize, state: CaptureState) {
        if let Some(c) = self.lock().get_mut(i) {
            c.state = state;
        }
    }
}

impl ScenarioFrames {
    /// The sink of `captures` (tests).
    #[cfg(test)]
    pub fn new(captures: Captures) -> Self {
        Self::stalled(captures, None)
    }

    /// The sink of `captures`, stalled as [`STALL_ENV`] says.
    pub fn from_env(captures: Captures) -> Self {
        let stall = std::env::var(STALL_ENV)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(Duration::from_millis);
        Self::stalled(captures, stall)
    }

    fn stalled(captures: Captures, stall: Option<Duration>) -> Self {
        Self {
            captures,
            stall,
            frame: 0,
            writer: None,
            backlog: Arc::default(),
            write_bytes: WRITE_BYTES,
            result_origin: None,
        }
    }

    /// Reserves `bytes` of a frame in the backlog; why not when it is full.
    fn admit(&self, bytes: usize) -> Result<(), String> {
        let mut b = self
            .backlog
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if b.frames >= WRITE_FRAMES || (b.frames > 0 && b.bytes + bytes > self.write_bytes) {
            return Err(format!(
                "the capture writer is behind ({} frames, {} bytes waiting; at most {WRITE_FRAMES} frames, {} bytes)",
                b.frames, b.bytes, self.write_bytes
            ));
        }
        b.frames += 1;
        b.bytes += bytes;
        Ok(())
    }

    /// Hands `job`, admitted, to the writer, started if it is not. A job
    /// the writer cannot take fails its captures, saying why, and leaves
    /// the backlog as it drops.
    fn send(&mut self, job: Job) {
        if self.writer.is_none() {
            match self.start() {
                Ok(w) => self.writer = Some(w),
                Err(why) => return job.fail(&why),
            }
        }
        if let Some(w) = &self.writer
            && let Err(mpsc::SendError(job)) = w.jobs.send(job)
        {
            job.fail("its writer ended");
        }
    }

    /// Starts the writer.
    fn start(&self) -> Result<Writer, String> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("capture writer".to_owned())
            // A job leaves the backlog as it drops: written, or discarded
            // with the queue when a panic ends the thread.
            .spawn(move || {
                for mut job in queue {
                    write_frame(&mut job);
                }
            })
            .map_err(|e| format!("its writer did not start: {e}"))?;
        Ok(Writer { jobs, thread })
    }
}

impl Drop for ScenarioFrames {
    /// The writer writes what it was given, then ends. A write stuck in
    /// the file system holds it, and with it this thread: the render
    /// link's bounds leave both to the process's exit.
    fn drop(&mut self) {
        if let Some(Writer { jobs, thread }) = self.writer.take() {
            drop(jobs);
            let _ = thread.join();
        }
    }
}

/// Tests: a writer waits while this is write-locked.
#[cfg(test)]
static HOLD_WRITES: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Encodes a captured frame and writes it as each capture due on it, with
/// the frame's display list beside a play frame, putting each outcome in
/// the list unless the capture has ended meanwhile.
fn write_frame(job: &mut Job) {
    // Tests: held while a test holds the writers.
    #[cfg(test)]
    let _held = HOLD_WRITES.read();
    // Tests: a capture named so makes its writer panic.
    #[cfg(test)]
    if job.due.iter().any(|(_, p, ..)| p.ends_with("panic.png")) {
        panic!("the capture writer fails (a test)");
    }
    // Encoded once, for every capture due.
    let image = job
        .pixels
        .take()
        .map(|(w, h, pixels)| super::png(w, h, &pixels));
    for (i, path, side, generation) in &job.due {
        let outcome = match &image {
            None => Err("nothing to read back".to_owned()),
            Some(png) => write_new(path, png)
                .and_then(|()| write_new(&path.with_extension("json"), side.as_bytes()))
                .map_err(|e| e.to_string())
                .and_then(|()| match &job.list {
                    None => Ok(()),
                    Some(Err(why)) => Err(why.clone()),
                    Some(Ok(lines)) => write_new(&path.with_extension("jsonl"), lines.as_bytes())
                        .map_err(|e| e.to_string()),
                }),
        };
        if let Some(c) = job.captures.lock().get_mut(*i)
            && c.state == CaptureState::Saving(*generation)
        {
            c.state = match outcome {
                Ok(()) => CaptureState::Succeeded,
                Err(why) => CaptureState::Failed(format!("{}: {why}", path.display())),
            };
        }
    }
}

impl FrameSink for ScenarioFrames {
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>) {
        if !drawn.timing.presented {
            return;
        }
        self.frame = self.frame.wrapping_add(1);
        // A result's origin: its first presented frame, whatever drew it.
        if drawn.screen.is_result()
            && let Some(id) = drawn.result.and_then(|r| r.id)
            && self.result_origin.is_none_or(|(seq, _)| seq != id.seq)
        {
            self.result_origin = Some((id.seq, drawn.presented_at));
        }
        let result_origin = self.result_origin;
        let due: Vec<(usize, PathBuf, Axis, u64, Stamp)> = {
            let mut list = self.captures.lock();
            list.iter_mut()
                .enumerate()
                .filter_map(|(i, c)| match capture_due(c, drawn, result_origin)? {
                    Due::Save(generation, stamp) => {
                        c.state = CaptureState::Saving(generation);
                        Some((i, c.path.clone(), c.axis, generation, stamp))
                    }
                    Due::Fail(why) => {
                        c.state = CaptureState::Failed(format!("{}: {why}", c.path.display()));
                        None
                    }
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        if let Some(stall) = self.stall {
            std::thread::sleep(stall);
        }
        // Read back once, for every capture due, and what the frame was.
        let pixels = screen.read_back();
        // Its list too, outside the play's measured region; loading frames
        // are smoke only and have none.
        let list = drawn
            .play
            .and_then(|_| screen.display_list())
            .map(|l| display_list_lines(&l, drawn));
        let bytes = pixels.as_ref().map_or(0, |p| p.2.len())
            + list
                .as_ref()
                .map_or(0, |l| l.as_ref().map_or_else(String::len, String::len));
        if let Err(why) = self.admit(bytes) {
            let mut list = self.captures.lock();
            for (i, path, _, generation, _) in due {
                if let Some(c) = list.get_mut(i)
                    && c.state == CaptureState::Saving(generation)
                {
                    c.state = CaptureState::Failed(format!("{}: {why}", path.display()));
                }
            }
            return;
        }
        let due = due
            .into_iter()
            .map(|(i, path, axis, generation, stamp)| {
                let side = sidecar(axis, self.frame, drawn, stamp, screen.adapter());
                (i, path, side, generation)
            })
            .collect();
        self.send(Job {
            pixels,
            list,
            due,
            bytes,
            backlog: Arc::clone(&self.backlog),
            captures: self.captures.clone(),
        });
    }

    fn finish(&mut self) {
        let (lock, done) = &*self.backlog;
        let mut b = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while b.frames > 0 {
            b = done
                .wait(b)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// What a frame means for a capture waiting on it.
enum Due {
    /// It is the frame: saved under this generation, with what its capture
    /// waited for.
    Save(u64, Stamp),
    /// The capture can no longer be met.
    Fail(&'static str),
}

/// What a selection or result capture waited for, for its sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stamp {
    /// A play axis's: its frame says it all.
    Play,
    /// The update's revision it was armed with, and its origin.
    Select { revision: u64, origin: TimeUs },
    /// Its result's origin, and the end page and view it was armed with.
    Result {
        origin: TimeUs,
        page: u64,
        view: Option<u64>,
    },
}

/// What `drawn` means for capture `c`, if anything: a selection capture's
/// origin is set on the first frame that applied its update; a result
/// capture's is its result's, `result_origin`.
fn capture_due(
    c: &mut Capture,
    drawn: &Drawn<'_>,
    result_origin: Option<(u64, TimeUs)>,
) -> Option<Due> {
    match c.axis {
        // Armed, whether or not its play has started; its frames carry no
        // play to bind to.
        Axis::Loading => {
            let loading = drawn.screen == ScreenKind::Loading;
            return match c.state {
                CaptureState::Pending if loading => Some(Due::Save(0, Stamp::Play)),
                CaptureState::Bound(g) if loading => Some(Due::Save(g, Stamp::Play)),
                _ => None,
            };
        }
        // Armed, whether or not its play has started, as `Loading`; its
        // play's first frame of play ends it.
        Axis::LoadingAt(since) => {
            let g = match c.state {
                CaptureState::Pending => 0,
                CaptureState::Bound(g) => g,
                _ => return None,
            };
            if drawn.screen != ScreenKind::Loading {
                let played = drawn.play.is_some() && drawn.generation == Some(g);
                return (g != 0 && played).then_some(Due::Fail("its loading state ended first"));
            }
            let stamp = drawn.loading_stamp?;
            return (stamp.sampled.0 - stamp.origin.0 >= since.0)
                .then_some(Due::Save(g, Stamp::Play));
        }
        Axis::Select(since) => {
            let CaptureState::Selecting {
                lifetime,
                revision,
                origin,
            } = &mut c.state
            else {
                return None;
            };
            if drawn.screen != ScreenKind::Selection {
                return None;
            }
            let (l, r) = drawn.selection?;
            if l != *lifetime || r < *revision {
                return None;
            }
            let o = *origin.get_or_insert(drawn.presented_at);
            let stamp = Stamp::Select {
                revision: *revision,
                origin: o,
            };
            return (drawn.presented_at.0 - o.0 >= since.0)
                .then_some(Due::Save(SCREEN_SAVE, stamp));
        }
        Axis::Result(since) => {
            let CaptureState::Resulting { seq, page, view } = c.state else {
                return None;
            };
            if !drawn.screen.is_result() {
                return None;
            }
            let drew = drawn.result?;
            if drew.id?.seq != seq || drew.page < page {
                return None;
            }
            if let Some(v) = view
                && drew.view.is_none_or(|drawn_view| drawn_view < v)
            {
                return None;
            }
            let (of, origin) = result_origin?;
            if of != seq {
                return None;
            }
            let stamp = Stamp::Result { origin, page, view };
            return (drawn.presented_at.0 - origin.0 >= since.0)
                .then_some(Due::Save(SCREEN_SAVE, stamp));
        }
        Axis::Chart(_) | Axis::Ready(_) | Axis::Closing(_) => {}
    }
    let (Some(generation), Some(t)) = (drawn.generation, drawn.play) else {
        return None;
    };
    if c.state != CaptureState::Bound(generation) {
        return None;
    }
    match c.axis {
        Axis::Chart(at) => (t >= at).then_some(Due::Save(generation, Stamp::Play)),
        Axis::Ready(since) => {
            let view = drawn.view?;
            if view.generation != 0 {
                return Some(Due::Fail("its READY candidate was rearmed first"));
            }
            let (origin, end) = view.ready?;
            if t >= end {
                return Some(Due::Fail("its READY state ended first"));
            }
            (t.0 - origin.0 >= since.0).then_some(Due::Save(generation, Stamp::Play))
        }
        Axis::Closing(since) => {
            let start = drawn.view?.closing?;
            (t.0 - start.0 >= since.0).then_some(Due::Save(generation, Stamp::Play))
        }
        Axis::Loading | Axis::LoadingAt(_) | Axis::Select(_) | Axis::Result(_) => None,
    }
}

/// What the capture of `axis` on frame number `frame` was: its PNG's
/// companion. A frame's play state may lag its time, which the view says.
fn sidecar(
    axis: Axis,
    frame: u64,
    drawn: &Drawn<'_>,
    stamp: Stamp,
    adapter: Option<&render::AdapterInfo>,
) -> String {
    let opt = |v: Option<i64>| v.map_or_else(|| "null".to_owned(), |v| v.to_string());
    let num = |v: Option<u64>| opt(v.and_then(|v| i64::try_from(v).ok()));
    let view = drawn.view;
    let ready = view.and_then(|v| v.ready);
    let closing = view.and_then(|v| v.closing);
    let loading = drawn.loading_stamp;
    let at = drawn.presented_at.0;
    let (select_origin, stamp_revision) = match stamp {
        Stamp::Select { revision, origin } => (Some(origin.0), Some(revision)),
        _ => (None, None),
    };
    let (result_origin, stamp_page, stamp_view) = match stamp {
        Stamp::Result { origin, page, view } => (Some(origin.0), Some(page), view),
        _ => (None, None, None),
    };
    let result = drawn.result;
    let id = result.and_then(|r| r.id);
    let screen = format!(
        "\"screen\":\"{}\",\"adapter\":{},\"presented_us\":{at},\"select_us\":{},\"select_origin_us\":{},\
         \"selection_lifetime\":{},\"selection_revision\":{},\"stamp_revision\":{},\
         \"result_us\":{},\"result_origin_us\":{},\"result_seq\":{},\"result_generation\":{},\
         \"result_drawn\":{},\"result_page\":{},\"result_view\":{},\"stamp_page\":{},\
         \"stamp_view\":{},\"sampled_us\":{},\"result_timer_origin_us\":{}",
        drawn.screen.name(),
        adapter.map_or_else(|| "null".to_owned(), |a| json_string(&a.name)),
        opt(select_origin.map(|o| at - o)),
        opt(select_origin),
        num(drawn.selection.map(|(l, _)| l)),
        num(drawn.selection.map(|(_, r)| r)),
        num(stamp_revision),
        opt(result_origin.map(|o| at - o)),
        opt(result_origin),
        num(id.map(|i| i.seq)),
        num(id.map(|i| i.generation)),
        result.map_or_else(|| "null".to_owned(), |r| format!("\"{}\"", r.path.name())),
        num(result.map(|r| r.page)),
        num(result.and_then(|r| r.view)),
        num(stamp_page),
        num(stamp_view),
        opt(result.map(|r| r.sampled.0)),
        opt(result.and_then(|r| r.timer_origin).map(|t| t.0)),
    );
    format!(
        "{{\"axis\":\"{}\",\"target_us\":{},\"frame\":{frame},\"phase\":\"{}\",\
         \"chart_us\":{},\"ready_us\":{},\"ready_origin_us\":{},\"play_at_us\":{},\
         \"closing_us\":{},\"closing_start_us\":{},\"loading_us\":{},\"loading_origin_us\":{},\"settled\":{},\"screen_us\":{},\"completed_us\":{},\
         \"generation\":{},\"view_generation\":{},\"finalized_us\":{},\"events\":{},{screen}}}\n",
        axis.name(),
        axis.target(),
        phase(drawn),
        opt(drawn.play.map(|t| t.0)),
        opt(drawn.play.zip(ready).map(|(t, (origin, _))| t.0 - origin.0)),
        opt(ready.map(|(origin, _)| origin.0)),
        opt(ready.map(|(_, end)| end.0)),
        opt(drawn.play.zip(closing).map(|(t, start)| t.0 - start.0)),
        opt(closing.map(|start| start.0)),
        opt(loading.map(|l| l.sampled.0 - l.origin.0)),
        opt(loading.map(|l| l.origin.0)),
        loading.map_or_else(|| "null".to_owned(), |l| l.settled.to_string()),
        // A loading frame runs `screen` from its lifetime's origin, so its
        // screen clock is its loading clock (`loading_frame_of`).
        opt(drawn
            .screen_us
            .or_else(|| loading.map(|l| l.sampled.0 - l.origin.0))),
        drawn.at.0,
        opt(drawn.generation.and_then(|g| i64::try_from(g).ok())),
        opt(view.and_then(|v| i64::try_from(v.generation).ok())),
        opt(view.and_then(|v| v.finalized).map(|t| t.0)),
        opt(view.and_then(|v| i64::try_from(v.events).ok())),
    )
}

/// A JSON string of `s`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `values` as a JSON array.
fn json_array<T: fmt::Display>(values: &[T]) -> String {
    let inner: Vec<String> = values.iter().map(ToString::to_string).collect();
    format!("[{}]", inner.join(","))
}

/// `drawn`'s list as JSON lines: the frame's state, then the elements, then
/// the quads in draw order; an error if it would misattribute.
fn display_list_lines(list: &skin_model::DisplayList, drawn: &Drawn<'_>) -> Result<String, String> {
    use std::fmt::Write as _;

    use skin_model::{Blend, QuadSource};

    if list.observations_dropped > 0 {
        return Err(format!(
            "{} observations had no room",
            list.observations_dropped
        ));
    }
    let opt = |v: Option<i64>| v.map_or_else(|| "null".to_owned(), |v| v.to_string());
    let mut out = String::new();
    let state = drawn.view.map(|v| {
        let c = v.tally.counts;
        let gauges: Vec<String> = v.gauges.iter().map(|g| opt(*g)).collect();
        format!(
            ",\"counts\":[{},{},{},{},{}],\"empty_poor\":{},\"ex_score\":{},\"combo\":{},\
             \"max_combo\":{},\"shown_gauge\":{},\"gauges\":[{}],\"look\":{},\"closing_start_us\":{}",
            c.perfect,
            c.great,
            c.good,
            c.bad,
            c.poor,
            v.tally.empty_poor,
            v.tally.ex_score,
            v.tally.combo,
            v.tally.max_combo,
            v.shown_gauge,
            gauges.join(","),
            v.look
                .map_or_else(|| "null".to_owned(), |l| json_string(&format!("{l:?}"))),
            opt(v.closing.map(|start| start.0)),
        )
    });
    let _ = writeln!(
        out,
        "{{\"state\":true,\"element_count\":{},\"quads_dropped\":{}{}}}",
        list.element_count,
        list.quads_dropped,
        state.unwrap_or_default()
    );
    for e in &list.elements {
        let o = e.observation;
        let _ = writeln!(
            out,
            "{{\"element\":{},\"quad_start\":{},\"outcome\":\"{:?}\",\"value\":{},\"choice\":{},\
             \"t_us\":{},\"text\":{}}}",
            o.element,
            o.quad_start,
            o.outcome,
            opt(o.value),
            opt(o.choice.map(i64::from)),
            opt(o.t.map(|t| t.0)),
            e.text
                .as_deref()
                .map_or_else(|| "null".to_owned(), json_string),
        );
    }
    for (seq, q) in list.quads.iter().enumerate() {
        let source = match &q.source {
            QuadSource::File {
                frame,
                name,
                rect,
                flip,
            } => format!(
                "\"frame\":{frame},\"file\":{},\"src\":{},\"flip\":{}",
                json_string(name),
                json_array(rect),
                json_array(flip)
            ),
            QuadSource::Solid { frame, color } => {
                format!("\"frame\":{frame},\"solid\":{}", json_array(color))
            }
            QuadSource::Other => "\"frame\":null".to_owned(),
        };
        let _ = writeln!(
            out,
            "{{\"seq\":{seq},\"element_of\":{},\"rect\":{},\"pivot\":{},\"angle\":{},\"color\":{},\
             \"blend\":\"{}\",\"clip\":{},\"clip_geometry\":{},\"clip_rect\":{},\"filter\":\"{:?}\",{source}}}",
            opt(q.element.map(i64::from)),
            json_array(&q.rect),
            json_array(&q.pivot),
            q.angle,
            json_array(&q.color),
            match q.blend {
                Blend::Alpha => "alpha",
                Blend::Add => "add",
                Blend::Multiply => "multiply",
            },
            opt(q.clip.map(i64::from)),
            q.clip_geometry
                .map_or_else(|| "null".to_owned(), |r| json_array(&r)),
            q.clip_rect
                .map_or_else(|| "null".to_owned(), |r| json_array(&r)),
            q.filter,
        );
    }
    Ok(out)
}

/// The play phase a frame's sidecar names: one decision over the acquired
/// view's Closing, completion and playing flag and the frame's time against
/// where play starts, the inputs the screens draw from.
///
/// It names the lifecycle state the frame was drawn in, not a promise about
/// its pixels: timers and the built-in screen's READY text follow the frame's
/// time alone, so a completed view before play starts is `tail` though those
/// still read as READY (design.md "Mid-Chart Start and Section Repeat": a
/// round completed there rings out without its notes). A quit during READY
/// stays `ready`: no field says it, and it draws as READY. The views a
/// playing flag before play starts would need do not occur, the lifecycle
/// raising it only from the chart's start.
fn phase(drawn: &Drawn<'_>) -> &'static str {
    let (Some(t), Some(v)) = (drawn.play, drawn.view) else {
        return match drawn.screen {
            ScreenKind::Loading => "loading",
            ScreenKind::Selection => "selection",
            ScreenKind::Result | ScreenKind::End => "result",
            _ => "other",
        };
    };
    let before_play = v.ready.is_some_and(|(_, start)| t < start);
    match (v.closing.is_some(), v.complete, v.playing, before_play) {
        (true, ..) => "closing",
        (false, _, true, false) => "play",
        (false, true, ..) | (false, false, false, false) => "tail",
        (false, false, _, true) => "ready",
    }
}

/// The rig's part of a scenario run (dev-tools).
#[derive(Debug)]
pub struct State {
    /// The input logs, by [`InputRef::Log`].
    pub logs: Vec<ScriptedInput>,
    /// The captures.
    pub captures: Captures,
    /// What a play of a log records: the key configs'.
    pub recorded: Recorded,
    /// The loaded chart's input.
    pub chart_input: Option<InputRef>,
    /// The `play` step whose play is being readied.
    pub pending: Option<Origin>,
    /// The current play's `until`, until it is sent.
    pub until: Option<(TimeUs, UntilCommand)>,
    /// The current play's `until` command once sent, with the play time
    /// it was sent at, until its end is checked against it.
    pub sent: Option<(UntilCommand, TimeUs)>,
    /// The practice page as last shown.
    pub practice: Option<game::PracticeScreen>,
    /// Whether the page was shown since a step last pressed a key on it:
    /// a key is pressed only on what the page shows.
    pub shown: bool,
    /// The rounds of the current `practice` step started so far.
    pub rounds: u16,
    /// The practice page's nearest approach to the goal its step sets,
    /// in keys: only a page nearer than this is progress.
    pub nearest: Option<u64>,
    /// The result shown, from its preparation until it is left.
    pub result_id: Option<ResultId>,
    /// The results begun so far: the next one's number less one.
    pub result_seq: u64,
}

/// Runs the scenario of `args`: song selection, headless, on its steps.
///
/// # Errors
///
/// The file's refusal, or the run's first failure.
pub fn run(args: ScenarioArgs, clock: QpcClock) -> anyhow::Result<()> {
    let Scenario {
        rescan_ms,
        mut script,
        session,
    } = load(&args.file)?;
    // A holding capture waits at most the render bound.
    script.set_hold_bound(args.render_timeout.unwrap_or(super::HOLD_BOUND));
    if let Some((stage, mode)) = args.specializing {
        super::specializing(stage, mode);
    }
    crate::select(
        crate::args::Select {
            input: crate::PlayInput::Autoplay,
            dev: super::SelectOptions {
                headless: true,
                script: Some(script),
                rescan_ms,
                frames: None,
                render_timeout: args.render_timeout,
                stop_timeout: None,
                size: None,
                scenario: Some(session),
            },
        },
        clock,
    )
}

/// The outcome of a scenario run, from the first failure on: the script's
/// (which names its step), the run's error (named by the step it came
/// in), a render thread that did not end cleanly, a capture that did not
/// end, and the steps that never ran. The first is the error; the others
/// are printed after it.
///
/// # Errors
///
/// The first failure.
pub fn conclude<E: std::error::Error + 'static>(
    ran: Result<(), E>,
    script: Option<&SelectScript>,
    pending: Option<Origin>,
    end: RenderEnd,
    captures: &Captures,
) -> anyhow::Result<()> {
    let mut failures: Vec<String> = Vec::new();
    if let Some(why) = script.and_then(SelectScript::failure) {
        failures.push(why.to_owned());
    }
    if let Err(e) = ran {
        let chain = game::Chain(&e).to_string();
        failures.push(
            match pending.or_else(|| script.and_then(SelectScript::origin)) {
                Some(o) => format!("{o}: {chain}"),
                None => chain,
            },
        );
    }
    if end != RenderEnd::Joined {
        failures.push("the render thread did not end cleanly".to_owned());
    }
    failures.extend(captures.unfinished());
    if let Some(script) = script {
        failures.extend(
            script
                .not_run()
                .into_iter()
                .map(|o| format!("{o} did not run: the run ended")),
        );
    }
    let mut all = failures.into_iter();
    let Some(first) = all.next() else {
        return Ok(());
    };
    // One message, the first failure leading: the run's report prints it
    // whole.
    let mut message = format!("dev: scenario: {first}");
    for also in all {
        message.push_str("\ndev: scenario: also: ");
        message.push_str(&also);
    }
    Err(anyhow::Error::msg(message))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::path::{Path, PathBuf};

    use bms_model::TimeUs;

    use super::{
        CaptureState, Captures, InputRef, Origin, PlayStep, SelectStep, StepKind, UntilCommand,
        find, parse, write_new,
    };

    /// A play report ending as `end`, its record finalized at
    /// `finalized` (none for `None`), with `closing`.
    fn ended(
        end: game::PlayEnd,
        finalized: Option<i64>,
        closing: Option<game::ClosingReport>,
    ) -> game::PlayReport {
        let preset = rules::tapestry();
        let record = finalized.map(|f| replay::PlayRecord {
            start_tuning: Some(replay::StartTuning::default()),
            versions: replay::Versions {
                evaluator: 1,
                app: "0.0.0-test".to_owned(),
                provenance: replay::BuildProvenance::Development,
            },
            chart: replay::ChartRecord::default(),
            preset: replay::PresetRecord::of(&preset),
            session_policy: "close-on-failure".to_owned(),
            devices: replay::DeviceRecord {
                sources: Vec::new(),
                audio_offset: TimeUs(0),
                display_offset: TimeUs(0),
                wake: replay::WakePolicy::Timer {
                    heartbeat_us: 1_000,
                },
            },
            play_type: replay::PlayType::Full,
            speed: bms_model::Speed::NORMAL,
            start: eval::InputStart::default(),
            events: Vec::new(),
            arrivals: Vec::new(),
            status: replay::Status {
                end: replay::EndStatus::EndedMidway,
                finalized: TimeUs(f),
                incomplete: replay::Incomplete::default(),
            },
            history: Default::default(),
        });
        game::PlayReport {
            attempt: 1,
            end,
            tail_cut: None,
            tail_skipped: false,
            closing,
            reload_error: None,
            output_error: None,
            input_error: None,
            live: Vec::new(),
            tally: eval::Tally::default(),
            displayed_gauge: game::GaugeIndex(0),
            record,
            saved: None,
            result: None,
            best: None,
            preset,
        }
    }

    fn closed(started_at: i64, end: game::ClosingEnd) -> Option<game::ClosingReport> {
        Some(game::ClosingReport {
            started_at: TimeUs(started_at),
            ended_at: TimeUs(started_at + 1_000_000),
            end,
        })
    }

    /// The `until quit` oracle checks what the play did: a quit taken
    /// during play with Closing, in READY or autoplay without it, whatever
    /// the gauges; a quit sent into a Closing a failure started is told
    /// apart as ignored; a quit that should have closed the play and did
    /// not, or closed one in autoplay, is a miss.
    #[test]
    fn the_quit_oracle_reads_the_plays_phase_and_cause() {
        use game::{AbortCause, ClosingEnd, PlayEnd};
        let quit = PlayEnd::Aborted(AbortCause::Quit);
        let failed = PlayEnd::Aborted(AbortCause::GaugeFailed);
        let ran_out = ClosingEnd::RanOut;
        let at = TimeUs(1_500_000);
        let q = UntilCommand::Quit;
        // Taken during play: Closing.
        assert_eq!(
            q.missed(
                &ended(quit, Some(1_498_000), closed(1_500_000, ran_out)),
                false,
                at
            ),
            None
        );
        // In READY: none.
        assert_eq!(
            q.missed(&ended(quit, Some(-2_000), None), false, TimeUs(-1_000)),
            None
        );
        // In autoplay during play: none, and Closing there is a miss.
        assert_eq!(
            q.missed(&ended(quit, Some(1_498_000), None), true, at),
            None
        );
        assert!(
            q.missed(
                &ended(quit, Some(1_498_000), closed(1_500_000, ran_out)),
                true,
                at
            )
            .is_some()
        );
        // During play with no Closing: a miss.
        assert_eq!(
            q.missed(&ended(quit, Some(1_498_000), None), false, at),
            Some("the quit during play did not enter Closing")
        );
        // Sent into a Closing an earlier quit started: ignored too, though
        // the play's cause is a quit.
        assert_eq!(
            q.missed(
                &ended(quit, Some(800_000), closed(800_000, ran_out)),
                false,
                at
            ),
            Some("the quit came during Closing, where it does nothing")
        );
        // Sent into the Closing of a failure: ignored, told apart.
        assert_eq!(
            q.missed(
                &ended(failed, Some(1_200_000), closed(1_204_000, ran_out)),
                false,
                at
            ),
            Some("the quit came during Closing, where it does nothing")
        );
        // A failure before the quit, with no Closing.
        assert_eq!(
            q.missed(&ended(failed, Some(1_200_000), None), false, at),
            Some("the play ended for another cause before the quit")
        );
    }

    /// The `until retry` oracle wants the retry taken: the play, or its
    /// Closing, ended by it.
    #[test]
    fn the_retry_oracle_wants_the_retry_taken() {
        use game::{AbortCause, ClosingEnd, PlayEnd};
        let r = UntilCommand::Retry;
        let at = TimeUs(1_500_000);
        let retried = PlayEnd::Aborted(AbortCause::Retry);
        let failed = PlayEnd::Aborted(AbortCause::GaugeFailed);
        assert_eq!(
            r.missed(&ended(retried, Some(1_498_000), None), false, at),
            None
        );
        assert_eq!(
            r.missed(
                &ended(
                    failed,
                    Some(1_200_000),
                    closed(1_204_000, ClosingEnd::Retry)
                ),
                false,
                at
            ),
            None
        );
        assert_eq!(
            r.missed(
                &ended(
                    failed,
                    Some(1_200_000),
                    closed(1_204_000, ClosingEnd::RanOut)
                ),
                false,
                at
            ),
            Some("the retry was not taken")
        );
    }

    /// A folder with `library/a/a.bms` and `library/b/b.bms`.
    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for c in ["a", "b"] {
            let d = dir.path().join("library").join(c);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(format!("{c}.bms")), "#TITLE x\n").unwrap();
        }
        dir
    }

    /// The file of `steps` (TOML `[[step]]` bodies, one per entry).
    fn file(output: Option<&str>, steps: &[&str]) -> String {
        let mut text = "version = 8\n".to_owned();
        if let Some(o) = output {
            writeln!(text, "output = \"{o}\"").unwrap();
        }
        text.push_str("[library]\nroots = [\"library\"]\n");
        for s in steps {
            write!(text, "[[step]]\n{s}\n").unwrap();
        }
        text
    }

    fn refused(dir: &Path, output: Option<&str>, steps: &[&str]) -> String {
        parse(&file(output, steps), dir).unwrap_err()
    }

    fn origin(index: u16, kind: StepKind) -> Origin {
        Origin { index, kind }
    }

    /// Each step compiles into the steps of the screens it runs on, each
    /// with the step it comes from.
    #[test]
    fn steps_compile_with_their_origins() {
        let dir = folder();
        let s = parse(
            &file(
                Some("out"),
                &[
                    "capture = { at = 1500, file = \"x.png\" }",
                    "select = { path = \"library/a/a.bms\" }",
                    "play = { input = \"auto\", until = { at_ms = 2, command = \"retry\" } }",
                    "play = { input = \"auto\", until = \"end\" }",
                    "result = \"next-preset\"",
                    "result = \"back\"",
                    "select = { search = \"B\" }",
                    "play = { input = \"auto\", until = { at_ms = 3, command = \"quit\" } }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        let mut script = s.script;
        let mut got = Vec::new();
        while let Some(step) = script.front().cloned() {
            got.push((step, script.origin().unwrap()));
            script.done();
        }
        let a = dir.path().join("library").join("a").join("a.bms");
        let SelectStep::PickPath(target) = &got[1].0 else {
            panic!("{got:?}");
        };
        assert_eq!(target.path, a);
        let play = |until| {
            SelectStep::Play(PlayStep {
                input: InputRef::Auto,
                until,
            })
        };
        let expected = [
            (SelectStep::Capture(0), origin(1, StepKind::Capture)),
            (got[1].0.clone(), origin(2, StepKind::Select)),
            (SelectStep::Enter, origin(2, StepKind::Select)),
            (
                play(Some((TimeUs(2_000), UntilCommand::Retry))),
                origin(3, StepKind::Play),
            ),
            (play(None), origin(4, StepKind::Play)),
            (SelectStep::Next, origin(5, StepKind::Result)),
            (SelectStep::Back, origin(6, StepKind::Result)),
            (
                SelectStep::Query("B".to_owned()),
                origin(7, StepKind::Select),
            ),
            (
                SelectStep::PickSearched("B".to_owned()),
                origin(7, StepKind::Select),
            ),
            (SelectStep::Enter, origin(7, StepKind::Select)),
            (
                play(Some((TimeUs(3_000), UntilCommand::Quit))),
                origin(8, StepKind::Play),
            ),
            (SelectStep::Close, origin(9, StepKind::Quit)),
        ];
        assert_eq!(got, expected);
        assert_eq!(origin(7, StepKind::Select).to_string(), "step 7 (select)");
        assert!(dir.path().join("out").is_dir());
    }

    /// A timed quit lands on the play's result whichever its word: `quit`,
    /// before completion, ends the play midway; `skip`, once it is
    /// complete, skips its tail. A `result` step then takes the result and
    /// a `select` cannot.
    #[test]
    fn a_timed_quit_lands_on_the_result() {
        let dir = folder();
        let select = "select = { path = \"library/a/a.bms\" }";
        let at = |word: &str| {
            format!(
                "play = {{ input = \"auto\", until = {{ at_ms = 5000, command = \"{word}\" }} }}"
            )
        };
        let skip = at("skip");
        let quit = at("quit");
        let s = parse(
            &file(None, &[select, &skip, "result = \"back\"", "quit = {}"]),
            dir.path(),
        )
        .unwrap();
        let mut script = s.script;
        let mut got = Vec::new();
        while let Some(step) = script.front().cloned() {
            got.push(step);
            script.done();
        }
        assert!(got.contains(&SelectStep::Play(PlayStep {
            input: InputRef::Auto,
            until: Some((TimeUs(5_000_000), UntilCommand::Skip)),
        })));
        assert!(got.contains(&SelectStep::Back));
        assert_eq!(
            refused(dir.path(), None, &[select, &skip, select, "quit = {}"]),
            "step 3 (select): select cannot follow step 2 (play)"
        );
        let back = parse(
            &file(None, &[select, &quit, "result = \"back\"", "quit = {}"]),
            dir.path(),
        );
        assert!(back.is_ok(), "{:?}", back.err());
        assert_eq!(
            refused(dir.path(), None, &[select, &quit, select, "quit = {}"]),
            "step 3 (select): select cannot follow step 2 (play)"
        );
        let after_quit = parse(&file(None, &[select, &quit, "quit = {}"]), dir.path());
        assert!(after_quit.is_ok(), "{:?}", after_quit.err());
    }

    /// The input `{ miss = "all" }` compiles to played on the session's
    /// play path on a virtual clock (design.md "Play Lifecycle"), with the
    /// HAZARD gauge shown and a failure closing the play: its first miss
    /// fails the gauge at that wake, so the play is aborted by it with
    /// exactly one POOR and nothing else, and enters Closing. Autoplay in
    /// its place compiles to no log, and plays every note.
    #[test]
    fn generated_no_press_input_misses_into_closing_on_the_play_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("library").join("a");
        fs::create_dir_all(&a).unwrap();
        // Four notes, 500 ms apart from 2 s.
        let chart = a.join("a.bms");
        fs::write(&chart, "#PLAYER 1\n#TITLE a\n#BPM 120\n#00111:01010101\n").unwrap();
        let s = parse(
            &file(
                None,
                &[
                    "select = { path = \"library/a/a.bms\" }",
                    "play = { input = { miss = \"all\" }, until = \"end\" }",
                    "result = \"back\"",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        let mut script = s.script;
        let mut input = None;
        while let Some(step) = script.front().cloned() {
            if let SelectStep::Play(p) = step {
                input = Some(p.input);
            }
            script.done();
        }
        let Some(InputRef::Log(k)) = input else {
            panic!("{input:?}");
        };
        let mut logs = s.session.logs;
        let log = logs.swap_remove(usize::from(k));

        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings {
                // HAZARD.
                displayed_gauge: 5,
                on_failure: game::FailureAction::Close,
                ..game::PlayerSettings::default()
            },
        };
        let parts = game::Parts::unsaved(&settings.player);
        let config = game::SessionConfig {
            lead_in: TimeUs(1_000_000),
            step: TimeUs(1_000),
            start: TimeUs(0),
            effects: 64,
            seed: 1,
            speed: bms_model::Speed::NORMAL,
            app_version: "0.0.0-test".to_owned(),
            provenance: replay::BuildProvenance::Development,
            receive: replay::ReceiveMethod::Scripted,
            autoplay: None,
        };
        let root = game::DataRoot::new(dir.path().join("root"));
        let loaded = game::Loaded::load(root, settings, &chart, config, parts).unwrap();
        let bank = std::sync::Arc::new(audio::SoundBank::from_sounds(48_000, Vec::new()));
        let engine = audio::AudioEngine::new(
            bank,
            &loaded.chart().bgm,
            48_000,
            480,
            audio::MAX_VOICES,
            64,
        )
        .unwrap();
        let device = audio::SimDevice::new(audio::SimConfig::steady(48_000, 480, TimeUs(0)));
        let runner = game::RunnerAudio::new(engine, audio::RecordingOutput::new(device, 0));
        let mut session = loaded.start(
            runner,
            input::SourceSet::new(crate::source_config()).unwrap(),
        );
        let report = session
            .play(game::PlayScript {
                input: log,
                commands: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            report.end,
            game::PlayEnd::Aborted(game::AbortCause::GaugeFailed)
        );
        let c = report.tally.counts;
        assert_eq!(
            (c.perfect, c.great, c.good, c.bad, c.poor),
            (0, 0, 0, 0, 1),
            "{c:?}"
        );
        assert!(report.closing.is_some(), "{:?}", report.closing);
        let result = report.result.as_ref().unwrap();
        assert!(result.matches_live);
        let record = report.record.as_ref().unwrap();
        assert!(
            record
                .events
                .iter()
                .all(|e| !matches!(e.kind, eval::InputKind::Control { value, .. } if value != 0)),
            "{:?}",
            record.events
        );
    }

    /// A step where its screen does not show is refused, naming it and the
    /// step before.
    #[test]
    fn out_of_place_steps_are_refused_naming_the_step() {
        let dir = folder();
        let select = "select = { path = \"library/a/a.bms\" }";
        let auto = "play = { input = \"auto\", until = \"end\" }";
        assert_eq!(
            refused(dir.path(), None, &["result = \"back\"", "quit = {}"]),
            "step 1 (result): result cannot come first"
        );
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[select, auto, "result = \"back\"", auto, "quit = {}"]
            ),
            "step 4 (play): play cannot follow step 3 (result)"
        );
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &[
                    select,
                    auto,
                    "capture = { at = 0, file = \"a.png\" }",
                    "quit = {}"
                ]
            ),
            "step 3 (capture): no play follows"
        );
        assert_eq!(
            refused(dir.path(), None, &[select, auto]),
            "step 2 (play): the last step must be quit"
        );
        assert_eq!(
            refused(dir.path(), None, &["quit = {}", "quit = {}"]),
            "step 2 (quit): quit cannot follow step 1 (quit)"
        );
        let log = dir.path().join("x.tprl");
        fs::write(&log, b"not a log").unwrap();
        let e = refused(
            dir.path(),
            None,
            &[
                select,
                "play = { input = \"x.tprl\", until = \"end\" }",
                "quit = {}",
            ],
        );
        assert!(e.starts_with("step 2 (play): "), "{e}");
        assert!(e.contains("x.tprl"), "{e}");
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[
                    select,
                    "play = { input = \"auto\", until = { at_ms = 1, command = \"retry\" } }",
                    "play = { input = \"x.tprl\", until = \"end\" }",
                    "quit = {}",
                ]
            ),
            "step 3 (play): input differs from step 2's; select the chart again"
        );
        assert_eq!(
            refused(dir.path(), None, &["select = {}", "quit = {}"]),
            "step 1 (select): give either path or search"
        );
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[
                    "capture = { at = 0, file = \"a.png\" }",
                    select,
                    auto,
                    "quit = {}"
                ]
            ),
            "output is required for captures"
        );
    }

    /// Roots, chart paths and the output resolve against the file's folder.
    #[test]
    fn paths_resolve_against_the_file() {
        let dir = folder();
        let s = parse(
            &file(
                Some("runs/out"),
                &[
                    "capture = { at = 0, file = \"deep/a.png\" }",
                    "select = { path = \"library/b/../a/a.bms\" }",
                    "play = { input = \"auto\", until = \"end\" }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        assert_eq!(s.session.roots, [dir.path().join("library")]);
        assert!(dir.path().join("runs").join("out").join("deep").is_dir());
        let mut script = s.script;
        script.done();
        let Some(SelectStep::PickPath(t)) = script.front() else {
            panic!();
        };
        assert_eq!(t.path, dir.path().join("library").join("a").join("a.bms"));
        assert_eq!(
            s.session.captures.lock()[0].path,
            dir.path()
                .join("runs")
                .join("out")
                .join("deep")
                .join("a.png")
        );
        let e = refused(dir.path(), None, &["select = { path = \"nothing.bms\" }"]);
        assert!(e.ends_with("nothing.bms is not a file"), "{e}");
        let text =
            file(None, &["quit = {}"]).replace("\"library\"", "\"library\", \"library/a/..\"");
        let e = parse(&text, dir.path()).unwrap_err();
        assert!(e.contains("are the same folder"), "{e}");
    }

    /// A chart path matches the listed file whatever the spelling: by its
    /// path ignoring case first, else by identity, each listed file looked
    /// up once. A file outside the roots is refused when loaded.
    #[test]
    fn select_paths_match_by_identity() {
        let dir = folder();
        let s = parse(
            &file(
                None,
                &[
                    "select = { path = \"LIBRARY/A/A.BMS\" }",
                    "play = { input = \"auto\", until = \"end\" }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        let Some(SelectStep::PickPath(t)) = s.script.front().cloned() else {
            panic!();
        };
        let lib = dir.path().join("library");
        let listed = [lib.join("b").join("b.bms"), lib.join("a").join("a.bms")];
        let mut ids = std::collections::HashMap::new();
        assert_eq!(
            find(listed.iter().map(PathBuf::as_path), &t, &mut ids),
            Some(1)
        );
        assert!(ids.is_empty());
        // Listed under another spelling of its folder: by identity.
        let other = [
            lib.join("b").join("b.bms"),
            lib.join("b").join("..").join("a").join("a.bms"),
        ];
        assert_eq!(
            find(other.iter().map(PathBuf::as_path), &t, &mut ids),
            Some(1)
        );
        assert_eq!(ids.len(), 2);
        let outside = dir.path().join("outside.bms");
        fs::write(&outside, "#TITLE x\n").unwrap();
        let e = refused(dir.path(), None, &["select = { path = \"outside.bms\" }"]);
        assert!(e.ends_with("outside.bms is under no library root"), "{e}");
    }

    /// A capture's file stays inside the output folder.
    #[test]
    fn capture_paths_stay_in_the_output() {
        let dir = folder();
        for file in [
            "../a.png",
            "C:/a.png",
            "C:a.png",
            "/a.png",
            "a/../../b.png",
            "a/C:b.png",
            "a.png:stream",
        ] {
            let e = refused(
                dir.path(),
                Some("out"),
                &[&format!("capture = {{ at = 0, file = \"{file}\" }}")],
            );
            assert!(e.ends_with("leaves the output folder"), "{file}: {e}");
        }
        let e = refused(
            dir.path(),
            Some("out"),
            &["capture = { at = 0, file = \"a.bmp\" }"],
        );
        assert!(e.ends_with("file must end in .png"), "{e}");
        let e = refused(
            dir.path(),
            Some("out"),
            &[
                "capture = { at = 0, file = \"a.png\" }",
                "capture = { at = 1, file = \"A.PNG\" }",
            ],
        );
        assert_eq!(e, "step 2 (capture): A.PNG is also step 1's file");
        let e = refused(dir.path(), Some("../out"), &["quit = {}"]);
        assert!(e.ends_with("leaves the scenario's folder"), "{e}");
        // A drive inside a component would replace the path once joined.
        let e = refused(dir.path(), Some("safe/C:escaped"), &["quit = {}"]);
        assert!(e.ends_with("leaves the scenario's folder"), "{e}");
        assert!(!dir.path().join("safe").exists());
        let e = refused(
            dir.path(),
            Some("out"),
            &["capture = { at = 0, file = \"a/C:b.png\" }"],
        );
        assert!(e.ends_with("leaves the output folder"), "{e}");
        // An output that exists is refused, not reused.
        fs::create_dir(dir.path().join("out")).unwrap();
        let e = refused(dir.path(), Some("out"), &["quit = {}"]);
        assert!(e.ends_with("already exists"), "{e}");
    }

    /// The writer never replaces a file: a second write to the same path
    /// fails and leaves the first bytes.
    #[test]
    fn capture_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        write_new(&path, b"first").unwrap();
        assert!(write_new(&path, b"second").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        // Nor a folder.
        let folder = dir.path().join("b.png");
        fs::create_dir(&folder).unwrap();
        assert!(write_new(&folder, b"x").is_err());
        assert!(folder.is_dir());
    }

    /// Step counts and times beyond their bounds are refused.
    #[test]
    fn bounds_are_checked() {
        let dir = folder();
        let mut text = "version = 8\nstep = [".to_owned();
        for _ in 0..=u16::MAX {
            text.push_str("{quit={}},");
        }
        text.push_str("]\n[library]\nroots = [\"library\"]\n");
        assert!(text.len() < super::MAX_FILE);
        assert_eq!(
            parse(&text, dir.path()).unwrap_err(),
            "more than 65535 steps"
        );
        let select = "select = { path = \"library/a/a.bms\" }";
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[
                    select,
                    "play = { input = \"auto\", until = { at_ms = -1, command = \"quit\" } }"
                ]
            ),
            "step 2 (play): at_ms must be 0 or more"
        );
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[
                    select,
                    "play = { input = \"auto\", until = { at_ms = 9223372036854775807, command = \"quit\" } }"
                ]
            ),
            "step 2 (play): at_ms is too large"
        );
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &["capture = { at = 9223372036854775807, file = \"a.png\" }"]
            ),
            "step 1 (capture): at is too large"
        );
    }

    /// A file over the bound is refused after reading one byte past it.
    #[test]
    fn an_oversized_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.toml");
        let mut text = "version = 8\n".to_owned();
        text.push_str(&"#".repeat(super::MAX_FILE));
        fs::write(&path, text).unwrap();
        let e = format!("{:#}", super::load(&path).unwrap_err());
        assert!(e.contains("big.toml: larger than 1048576 bytes"), "{e}");
    }

    /// A step of two kinds, an unknown kind and an unknown field are
    /// refused with the line they are on; the version is read first.
    #[test]
    fn two_kinds_in_one_step_and_unknown_fields_are_refused() {
        let dir = folder();
        for (step, line) in [
            ("quit = {}\nresult = \"back\"", 4),
            ("jump = {}", 5),
            ("quit = { now = true }", 5),
        ] {
            let e = refused(dir.path(), None, &[step]);
            assert!(e.contains(&format!("line {line}")), "{step}: {e}");
        }
        let e = parse("version = 8\nfast = true\n", dir.path()).unwrap_err();
        assert!(e.contains("fast"), "{e}");
        // Another version, the previous one too, is refused.
        assert_eq!(
            parse("version = 1\nfast = true\n", dir.path()).unwrap_err(),
            "version 1 is not supported; this build reads version 8"
        );
        assert_eq!(
            parse("[library]\n", dir.path()).unwrap_err(),
            "no version; this build reads version 8"
        );
    }

    /// The adapter a file names: WARP unless it says `"hardware"`; any
    /// other name refused.
    #[test]
    fn a_scenario_names_its_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = |line: &str| {
            let text =
                format!("version = 8\n{line}[library]\nroots = [\".\"]\n[[step]]\nquit = {{}}\n");
            parse(&text, dir.path()).map(|p| p.session.adapter)
        };
        assert_eq!(adapter(""), Ok(super::Adapter::Warp));
        assert_eq!(adapter("adapter = \"warp\"\n"), Ok(super::Adapter::Warp));
        assert_eq!(
            adapter("adapter = \"hardware\"\n"),
            Ok(super::Adapter::Hardware)
        );
        for bad in ["adapter = \"cpu\"\n", "adapter = 1\n"] {
            assert!(adapter(bad).is_err(), "{bad}");
        }
        assert_eq!(
            super::Adapter::Hardware.choice(),
            render::AdapterChoice::HighPerformance
        );
        assert_eq!(super::Adapter::Warp.choice(), render::AdapterChoice::Warp);
    }

    /// An adapter of `device_type`, named `name`.
    fn adapter_info(name: &str, device_type: wgpu::DeviceType) -> render::AdapterInfo {
        render::AdapterInfo {
            name: name.to_owned(),
            ..render::AdapterInfo::new(device_type, wgpu::Backend::Dx12)
        }
    }

    /// A hardware choice refuses a CPU adapter, naming it; WARP is one and
    /// is admitted; the line says what was asked and what came.
    #[test]
    fn a_hardware_choice_refuses_a_cpu_adapter() {
        use render::AdapterChoice::{HighPerformance, Warp};
        let cpu = adapter_info("Basic Render", wgpu::DeviceType::Cpu);
        let gpu = adapter_info("Some GPU", wgpu::DeviceType::DiscreteGpu);
        assert!(super::admit(HighPerformance, &gpu).is_ok());
        assert!(super::admit(Warp, &cpu).is_ok());
        let e = super::admit(HighPerformance, &cpu).unwrap_err();
        assert_eq!(
            e.to_string(),
            "adapter: hardware was asked for, and the adapter is a CPU adapter: Basic Render"
        );
        assert_eq!(
            super::adapter_line(HighPerformance, &gpu),
            "dev: adapter asked hardware, got Some GPU (DiscreteGpu, Dx12)"
        );
        assert_eq!(
            super::adapter_line(Warp, &cpu),
            "dev: adapter asked warp, got Basic Render (Cpu, Dx12)"
        );
        // The sidecar names the adapter of the screen that drew the frame.
        let side = super::sidecar(
            super::Axis::Chart(TimeUs(0)),
            1,
            &drawn(1, 0),
            super::Stamp::Play,
            Some(&gpu),
        );
        assert!(side.contains("\"adapter\":\"Some GPU\","), "{side}");
        let side = super::sidecar(
            super::Axis::Chart(TimeUs(0)),
            1,
            &drawn(1, 0),
            super::Stamp::Play,
            None,
        );
        assert!(side.contains("\"adapter\":null,"), "{side}");
    }

    /// A capture names one axis, READY's from its start; a size is in
    /// bounds; each refused with its line.
    #[test]
    fn capture_axes_and_sizes_are_checked() {
        let capture = |body: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.bms"), "").unwrap();
            let text = format!(
                "version = 8\noutput = \"out\"\n[library]\nroots = [\".\"]\n[[step]]\ncapture = {{ {body}file = \"a.png\" }}\n[[step]]\nselect = {{ path = \"a.bms\" }}\n[[step]]\nplay = {{ input = \"auto\", until = \"end\" }}\n[[step]]\nresult = \"back\"\n[[step]]\nquit = {{}}\n"
            );
            parse(&text, dir.path()).map(|p| p.session.captures.0.lock().unwrap()[0].axis)
        };
        assert_eq!(capture("at = 5, "), Ok(super::Axis::Chart(TimeUs(5_000))));
        assert_eq!(capture("ready_at = 0, "), Ok(super::Axis::Ready(TimeUs(0))));
        assert_eq!(capture("loading = true, "), Ok(super::Axis::Loading));
        assert_eq!(
            capture("loading_at = 1500, "),
            Ok(super::Axis::LoadingAt(TimeUs(1_500_000)))
        );
        let e = capture("loading_at = -1, ").unwrap_err();
        assert!(e.contains("loading_at must be 0 or more"), "{e}");
        for bad in [
            "",
            "at = 1, ready_at = 1, ",
            "at = 1, loading = true, ",
            "loading = false, ",
            "loading = true, loading_at = 1, ",
            "loading_at = 1, ready_at = 1, ",
        ] {
            let e = capture(bad).unwrap_err();
            assert!(e.contains("give one of"), "{bad}: {e}");
        }
        assert_eq!(
            capture("closing_at = 3, "),
            Ok(super::Axis::Closing(TimeUs(3_000)))
        );
        for bad in ["closing_at = 1, at = 1, ", "closing_at = 1, ready_at = 1, "] {
            let e = capture(bad).unwrap_err();
            assert!(e.contains("give one of"), "{bad}: {e}");
        }
        let e = capture("ready_at = -1, ").unwrap_err();
        assert!(e.contains("ready_at must be 0 or more"), "{e}");
        let e = capture("closing_at = -1, ").unwrap_err();
        assert!(e.contains("closing_at must be 0 or more"), "{e}");
        assert!(capture("at = -9223372036854775808, ").is_err());

        let dir = tempfile::tempdir().unwrap();
        let sized = |size: &str| {
            let text = format!(
                "version = 8\nsize = {size}\n[library]\nroots = [\".\"]\n[[step]]\nquit = {{}}\n"
            );
            parse(&text, dir.path()).map(|p| p.session.size)
        };
        assert_eq!(sized("[1920, 1080]"), Ok(Some((1920, 1080))));
        for bad in ["[0, 1080]", "[1920, -1]", "[8193, 1080]"] {
            let e = sized(bad).unwrap_err();
            assert!(e.contains("each side must be"), "{bad}: {e}");
        }
    }

    /// The sidecar says the acquired view's own finalized time and event
    /// count.
    #[test]
    fn a_sidecar_says_the_views_finalized_time_and_events() {
        let mut d = drawn(1, 2);
        d.view = Some(crate::measure::ViewStamp {
            generation: 3,
            finalized: Some(TimeUs(1_234)),
            events: 56,
            ..drawn_view()
        });
        let side = super::sidecar(
            super::Axis::Chart(TimeUs(0)),
            9,
            &d,
            super::Stamp::Play,
            None,
        );
        assert!(side.contains("\"view_generation\":3,"), "{side}");
        assert!(side.contains("\"finalized_us\":1234,"), "{side}");
        assert!(side.contains("\"events\":56"), "{side}");
    }

    /// Every combination of what `phase` decides from, for a full play and
    /// a round starting later in the chart, each with what is drawn in it.
    #[test]
    fn a_phase_is_one_decision_over_its_inputs() {
        // Closing, complete, playing, before play starts; the phase; why.
        #[rustfmt::skip]
        const TABLE: [(bool, bool, bool, bool, &str, &str); 16] = [
            (true, false, false, true, "closing", "the failed timer runs from the view's Closing"),
            (true, false, false, false, "closing", "the failed timer runs from the view's Closing"),
            (true, false, true, true, "closing", "Closing comes first, whatever the rest"),
            (true, false, true, false, "closing", "Closing comes first, whatever the rest"),
            (true, true, false, true, "closing", "Closing comes first, whatever the rest"),
            (true, true, false, false, "closing", "Closing comes first, whatever the rest"),
            (true, true, true, true, "closing", "Closing comes first, whatever the rest"),
            (true, true, true, false, "closing", "Closing comes first, whatever the rest"),
            (false, false, false, true, "ready", "READY's timers and text run; a quit in READY too"),
            (false, false, false, false, "tail", "stopped past play's start, the play timer on"),
            (false, false, true, true, "ready", "does not occur; the timers still read READY"),
            (false, false, true, false, "play", "the last-note timer takes the playing flag"),
            (false, true, false, true, "tail", "a round rings out without notes"),
            (false, true, false, false, "tail", "the audio tail or a round's ring-out"),
            (false, true, true, true, "tail", "does not occur; complete rings a round out"),
            (false, true, true, false, "play", "the completing step still enters playing"),
        ];
        for (ready, start) in [(-1_000_000, 0), (-200_000, 2_000_000)] {
            for (closing, complete, playing, before, want, why) in TABLE {
                let mut d = drawn(1, 0);
                d.play = Some(TimeUs(if before { start - 10_000 } else { start }));
                d.view = Some(crate::measure::ViewStamp {
                    closing: closing.then_some(TimeUs(1)),
                    complete,
                    playing,
                    // Never read: a negative input offset moves it anywhere.
                    finalized: Some(TimeUs(start + 9_000)),
                    ready: Some((TimeUs(ready), TimeUs(start))),
                    ..drawn_view()
                });
                assert_eq!(super::phase(&d), want, "{start}: {why}");
            }
        }
    }

    /// A loading frame of the screen at `us` µs since its lifetime's
    /// origin, 7 s on the clock, of play `generation`.
    fn loading_drawn(generation: Option<u64>, us: i64) -> crate::measure::Drawn<'static> {
        let mut d = drawn(1, 0);
        d.play = None;
        d.view = None;
        d.generation = generation;
        d.screen = crate::measure::ScreenKind::Loading;
        d.loading_stamp = Some(crate::measure::LoadingStamp {
            sampled: TimeUs(7_000_000 + us),
            origin: TimeUs(7_000_000),
            settled: true,
        });
        d
    }

    /// A `loading_at` capture waits on its loading state's clock, armed
    /// before its play binds and after; a frame of its play first fails
    /// it; its sidecar gives the frame's loading and screen clocks, which
    /// are one.
    #[test]
    fn a_loading_at_capture_waits_on_the_loading_clock() {
        let dir = tempfile::tempdir().unwrap();
        let due = |c: &Captures, d: &crate::measure::Drawn<'_>| {
            let mut list = c.0.lock().unwrap();
            super::capture_due(&mut list[0], d, None)
        };
        let saves = |c: &Captures, d: &crate::measure::Drawn<'_>, g: u64| matches!(due(c, d), Some(super::Due::Save(s, _)) if s == g);
        // Pending: no play yet.
        let c = Captures::of(vec![(TimeUs(0), dir.path().join("a.png"))]);
        c.set_axis(0, super::Axis::LoadingAt(TimeUs(1_500_000)));
        c.arm(0);
        assert!(!saves(&c, &loading_drawn(None, 1_499_999), 0));
        assert!(saves(&c, &loading_drawn(None, 1_500_000), 0));
        // A play frame while still pending is not its play's: nothing.
        assert!(due(&c, &drawn(1, 0)).is_none());
        // Bound to play 2: not before 1.5 s, then at it and after it.
        c.bind(2);
        assert!(!saves(&c, &loading_drawn(Some(2), 1_499_999), 2));
        assert!(saves(&c, &loading_drawn(Some(2), 1_500_000), 2));
        assert!(saves(&c, &loading_drawn(Some(2), 1_500_001), 2));
        // Another play's frame: nothing; its own play's: failed.
        assert!(due(&c, &drawn(1, 0)).is_none());
        assert!(matches!(
            due(&c, &drawn(2, -900)),
            Some(super::Due::Fail("its loading state ended first"))
        ));
        let side = super::sidecar(
            super::Axis::LoadingAt(TimeUs(1_500_000)),
            4,
            &loading_drawn(Some(2), 1_734_000),
            super::Stamp::Play,
            None,
        );
        assert!(side.contains("\"axis\":\"loading_at\""), "{side}");
        assert!(side.contains("\"target_us\":1500000,"), "{side}");
        assert!(side.contains("\"loading_us\":1734000,"), "{side}");
        assert!(side.contains("\"screen_us\":1734000,"), "{side}");
        // A play frame gives the screen clock it was built with.
        let mut d = drawn(2, 500);
        d.screen_us = Some(3_210_000);
        let side = super::sidecar(
            super::Axis::Chart(TimeUs(0)),
            5,
            &d,
            super::Stamp::Play,
            None,
        );
        assert!(side.contains("\"screen_us\":3210000,"), "{side}");
        assert!(side.contains("\"loading_us\":null,"), "{side}");
    }

    /// A Closing capture waits for a frame whose view has Closing started,
    /// then for its time since that start; its sidecar and the list's
    /// state line say where Closing started.
    #[test]
    fn a_closing_capture_waits_for_its_views_closing() {
        let dir = tempfile::tempdir().unwrap();
        let c = Captures::of(vec![(TimeUs(0), dir.path().join("c.png"))]);
        c.set_axis(0, super::Axis::Closing(TimeUs(300_000)));
        c.arm(0);
        c.bind(1);
        let closing = |ms: i64, start: Option<i64>| {
            let mut d = drawn(1, ms);
            d.view = Some(crate::measure::ViewStamp {
                playing: false,
                closing: start.map(|s| TimeUs(s * 1_000)),
                ..drawn_view()
            });
            d
        };
        let due = |d: &crate::measure::Drawn<'_>| {
            let mut list = c.0.lock().unwrap();
            matches!(
                super::capture_due(&mut list[0], d, None),
                Some(super::Due::Save(1, _))
            )
        };
        // Past its time on the chart, but no Closing in the view: none.
        assert!(!due(&closing(9_000, None)));
        // 200 ms into Closing: not yet; 300 ms: due.
        assert!(!due(&closing(5_200, Some(5_000))));
        assert!(due(&closing(5_300, Some(5_000))));
        let side = super::sidecar(
            super::Axis::Closing(TimeUs(300_000)),
            4,
            &closing(5_300, Some(5_000)),
            super::Stamp::Play,
            None,
        );
        assert!(side.contains("\"closing_us\":300000,"), "{side}");
        assert!(side.contains("\"closing_start_us\":5000000,"), "{side}");
        assert!(side.contains("\"phase\":\"closing\""), "{side}");
        let list = skin_model::DisplayList {
            element_count: 0,
            elements: Vec::new(),
            quads: Vec::new(),
            quads_dropped: 0,
            observations_dropped: 0,
        };
        let lines = super::display_list_lines(&list, &closing(5_300, Some(5_000))).unwrap();
        assert!(lines.contains("\"closing_start_us\":5000000"), "{lines}");
    }

    /// Captures of one frame share its number, and later frames count on;
    /// READY overshoot writes nothing.
    #[test]
    fn captures_due_on_one_frame_are_one_frame() {
        use super::{FrameSink as _, ScenarioFrames};

        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let c = Captures::of(vec![
            (TimeUs(0), path("a.png")),
            (TimeUs(1_000), path("b.png")),
            (TimeUs(5_000), path("c.png")),
        ]);
        {
            let mut list = c.0.lock().unwrap();
            list[2].axis = super::Axis::Ready(TimeUs(9_000_000));
        }
        for i in 0..3 {
            c.arm(i);
        }
        c.bind(1);
        let mut sink = ScenarioFrames::new(c.clone());
        // Released from the start: its read-back answers at once.
        let (entered, _seen) = std::sync::mpsc::channel();
        let (_, held) = std::sync::mpsc::channel::<()>();
        let screen = Stalling {
            entered,
            held: std::sync::Mutex::new(held),
        };
        sink.frame(&screen, &drawn(1, -5));
        sink.frame(&screen, &drawn(1, 2));
        sink.finish();
        assert_eq!(
            fs::read_to_string(path("a.json"))
                .unwrap()
                .split("\"frame\":")
                .nth(1),
            fs::read_to_string(path("b.json"))
                .unwrap()
                .split("\"frame\":")
                .nth(1)
        );
        let side = fs::read_to_string(path("a.json")).unwrap();
        assert!(side.contains("\"frame\":2,"), "{side}");
        assert!(side.contains("\"chart_us\":2000,"), "{side}");
        assert!(side.contains("\"phase\":\"play\""), "{side}");
        // Its READY state ended first.
        assert!(matches!(c.states()[2], CaptureState::Failed(_)));
        assert!(!path("c.png").exists() && !path("c.json").exists());
    }

    /// `n` captures, one per ms from 0, armed and bound to play 1, a sink
    /// of them, and a screen whose read-back answers at once.
    fn released_sink(n: usize) -> (tempfile::TempDir, Captures, super::ScenarioFrames, Stalling) {
        let dir = tempfile::tempdir().unwrap();
        let c = Captures::of(
            (0..n)
                .map(|k| {
                    (
                        TimeUs(k as i64 * 1_000),
                        dir.path().join(format!("{k}.png")),
                    )
                })
                .collect(),
        );
        for k in 0..n {
            c.arm(u16::try_from(k).unwrap());
        }
        c.bind(1);
        let sink = super::ScenarioFrames::new(c.clone());
        // Released from the start: its read-back answers at once.
        let (entered, seen) = std::sync::mpsc::channel();
        // Its notices go nowhere.
        std::mem::forget(seen);
        let (_, held) = std::sync::mpsc::channel::<()>();
        let screen = Stalling {
            entered,
            held: std::sync::Mutex::new(held),
        };
        (dir, c, sink, screen)
    }

    /// A captured frame is encoded and written off the render thread: while
    /// the writer is held, the sink returns and a later frame takes the
    /// next capture; the play's end waits for the writer, and returns once
    /// both frames are written.
    #[test]
    fn a_held_write_delays_no_later_capture() {
        use super::FrameSink as _;

        let (dir, c, mut sink, screen) = released_sink(2);
        let hold = super::HOLD_WRITES.write().unwrap();
        sink.frame(&screen, &drawn(1, 0));
        assert_eq!(
            c.states(),
            [CaptureState::Saving(1), CaptureState::Bound(1)]
        );
        sink.frame(&screen, &drawn(1, 1));
        assert_eq!(
            c.states(),
            [CaptureState::Saving(1), CaptureState::Saving(1)]
        );
        std::thread::scope(|s| {
            let (ended, finished) = std::sync::mpsc::channel();
            s.spawn(move || {
                sink.finish();
                ended.send(()).unwrap();
            });
            assert!(
                finished
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .is_err(),
                "the end waits for the held writer"
            );
            assert!(!dir.path().join("0.png").exists());
            drop(hold);
            finished
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("the end returns once written");
        });
        assert_eq!(
            c.states(),
            [CaptureState::Succeeded, CaptureState::Succeeded]
        );
        let side = fs::read_to_string(dir.path().join("1.json")).unwrap();
        assert!(side.contains("\"frame\":2,"), "{side}");
        assert!(
            fs::read(dir.path().join("0.png"))
                .unwrap()
                .starts_with(b"\x89PNG")
        );
    }

    /// A frame past the writer's backlog fails its captures at once, saying
    /// so, and the render thread does not wait: by frames, and by bytes.
    #[test]
    fn a_frame_past_the_writers_backlog_fails_its_captures() {
        use super::FrameSink as _;

        let behind = |s: &CaptureState| matches!(s, CaptureState::Failed(why) if why.contains("the capture writer is behind"));
        // By frames: one more than the backlog holds.
        let n = super::WRITE_FRAMES + 1;
        let (dir, c, mut sink, screen) = released_sink(n);
        let hold = super::HOLD_WRITES.write().unwrap();
        for k in 0..n {
            sink.frame(&screen, &drawn(1, k as i64));
        }
        let states = c.states();
        assert!(
            states[..n - 1]
                .iter()
                .all(|s| *s == CaptureState::Saving(1)),
            "{states:?}"
        );
        assert!(behind(&states[n - 1]), "{states:?}");
        drop(hold);
        sink.finish();
        let states = c.states();
        assert!(
            states[..n - 1]
                .iter()
                .all(|s| *s == CaptureState::Succeeded),
            "{states:?}"
        );
        assert!(!dir.path().join(format!("{}.png", n - 1)).exists());
        // By bytes: each frame's pixels are 4 bytes, and the backlog holds 4.
        let (dir, c, mut sink, screen) = released_sink(2);
        sink.write_bytes = 4;
        let hold = super::HOLD_WRITES.write().unwrap();
        sink.frame(&screen, &drawn(1, 0));
        sink.frame(&screen, &drawn(1, 1));
        let states = c.states();
        assert_eq!(states[0], CaptureState::Saving(1));
        assert!(behind(&states[1]), "{states:?}");
        drop(hold);
        sink.finish();
        assert_eq!(c.states()[0], CaptureState::Succeeded);
        assert!(!dir.path().join("1.png").exists());
    }

    /// A writer that panics fails the captures of its current job and of
    /// those queued behind it, and releases their place in the backlog:
    /// the play's end returns, and a later frame fails because the writer
    /// ended, not because it is behind.
    #[test]
    fn a_panicking_writer_releases_its_backlog() {
        use super::FrameSink as _;

        let (dir, _, mut sink, screen) = released_sink(0);
        let c = Captures::of(
            ["panic.png", "1.png", "2.png", "3.png"]
                .iter()
                .enumerate()
                .map(|(k, name)| (TimeUs(k as i64 * 1_000), dir.path().join(name)))
                .collect(),
        );
        for k in 0..4 {
            c.arm(k);
        }
        c.bind(1);
        sink.captures = c.clone();
        let hold = super::HOLD_WRITES.write().unwrap();
        for k in 0..3 {
            sink.frame(&screen, &drawn(1, k));
        }
        assert_eq!(c.states()[..3], vec![CaptureState::Saving(1); 3]);
        drop(hold);
        std::thread::scope(|s| {
            let (ended, finished) = std::sync::mpsc::channel();
            let sink = &mut sink;
            s.spawn(move || {
                sink.finish();
                ended.send(()).unwrap();
            });
            finished
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("the end returns once the writer ended");
        });
        let ended = |s: &CaptureState| matches!(s, CaptureState::Failed(why) if why.contains("its writer ended"));
        let states = c.states();
        assert!(states[..3].iter().all(ended), "{states:?}");
        {
            let b = sink.backlog.0.lock().unwrap();
            assert_eq!((b.frames, b.bytes), (0, 0));
        }
        sink.frame(&screen, &drawn(1, 3));
        let states = c.states();
        assert!(ended(&states[3]), "{states:?}");
    }

    /// Captures arm, bind to the next play only, and settle when it ends:
    /// one never written fails naming its time, one written is reported
    /// once, and one still armed when the run ends did not run.
    #[test]
    fn captures_bind_to_the_next_play_and_settle_at_its_end() {
        let c = Captures::of(vec![
            (TimeUs(1_000_000), PathBuf::from("a.png")),
            (TimeUs(2_000_000), PathBuf::from("b.png")),
            (TimeUs(0), PathBuf::from("c.png")),
        ]);
        c.arm(0);
        c.bind(5);
        // Armed after play 5 started: not taken by it.
        c.arm(1);
        assert_eq!(
            c.states(),
            [
                CaptureState::Bound(5),
                CaptureState::Pending,
                CaptureState::Unarmed
            ]
        );
        let settled = c.settle(5, true).unwrap();
        assert!(settled.written.is_empty());
        assert_eq!(settled.failed.len(), 1);
        assert_eq!(
            settled.failed[0].1,
            "the play ended before a frame at 1000 ms on its chart axis"
        );
        c.bind(6);
        c.0.lock().unwrap()[1].state = CaptureState::Succeeded;
        let settled = c.settle(6, true).unwrap();
        assert_eq!(settled.written.len(), 1);
        assert!(settled.failed.is_empty());
        assert_eq!(c.settle(6, true).unwrap(), super::Settled::default());
        // Never armed: its step did not run, which the steps report.
        assert!(c.unfinished().is_empty());
        c.arm(2);
        assert_eq!(c.unfinished(), ["step 1 (capture): c.png did not run"]);
    }

    /// An input log of no events at `dir/log.tprl`.
    fn write_log(dir: &Path) {
        use replay::{
            BuildProvenance, ChartRecord, DeviceRecord, EndStatus, PlayRecord, PlayType,
            PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
        };
        let profile = game::DeviceSettings::default().profile(&[input::DeviceKind::Keyboard], &[]);
        let rec = PlayRecord {
            start_tuning: Some(replay::StartTuning::default()),
            versions: Versions {
                evaluator: 1,
                app: "test".to_owned(),
                provenance: BuildProvenance::Development,
            },
            chart: ChartRecord::default(),
            preset: PresetRecord::of(&rules::tapestry()),
            session_policy: "default".to_owned(),
            devices: DeviceRecord {
                sources: vec![SourceRecord {
                    kind: replay::DeviceKind::Keyboard,
                    receive: ReceiveMethod::Scripted,
                    profile: profile.sources[0].clone(),
                    axis_out_of_range: 0,
                }],
                audio_offset: TimeUs(0),
                display_offset: TimeUs(0),
                wake: replay::WakePolicy::Timer {
                    heartbeat_us: 1_000,
                },
            },
            play_type: PlayType::Full,
            speed: bms_model::Speed::NORMAL,
            start: eval::InputStart {
                opened_at: TimeUs(-1_000_000),
                held: Vec::new(),
            },
            events: Vec::new(),
            arrivals: Vec::new(),
            status: Status {
                end: EndStatus::PlayedToEnd,
                finalized: TimeUs(0),
                incomplete: replay::Incomplete::default(),
            },
            history: Default::default(),
        };
        fs::write(dir.join("log.tprl"), replay::encode(&rec)).unwrap();
    }

    /// A `practice` step of `fields` after the chart's path and before
    /// `rounds`.
    fn practice(fields: &str) -> String {
        format!("practice = {{ path = \"library/a/a.bms\", {fields} }}")
    }

    /// The fields of a valid `practice` step, but `rounds`.
    const FIELDS: &str = "from = 2, to = 3, speed = 75, lead_in_ms = 2500, gauge = 20, \
                          repeat = true, input = \"log.tprl\"";

    /// `decision = "skin"` on `select` and `practice` compiles to a wait
    /// for the decision skin's labels right before the choice (Enter, or
    /// P), naming the step; without it there is none; another value is
    /// refused when the file is loaded.
    #[test]
    fn the_decision_option_survives_parsing() {
        let dir = folder();
        write_log(dir.path());
        let compiled = |steps: &[&str]| {
            let mut script = parse(&file(None, steps), dir.path()).unwrap().script;
            let mut got = Vec::new();
            while let Some(step) = script.front().cloned() {
                got.push((step, script.origin().unwrap()));
                script.done();
            }
            got
        };
        let got = compiled(&[
            "select = { path = \"library/a/a.bms\", decision = \"skin\" }",
            "play = { input = \"log.tprl\", until = \"end\" }",
            "result = \"back\"",
            &practice(&format!("{FIELDS}, rounds = 1, decision = \"skin\"")),
            "quit = {}",
        ]);
        let (select, practised) = (origin(1, StepKind::Select), origin(4, StepKind::Practice));
        let at = |o, step: &SelectStep| got.iter().position(|(s, so)| *so == o && s == step);
        let ready = at(select, &SelectStep::DecisionReady).expect("the select's wait");
        assert_eq!(at(select, &SelectStep::Enter), Some(ready + 1), "{got:?}");
        let ready = at(practised, &SelectStep::DecisionReady).expect("the practice's wait");
        assert!(
            matches!(got[ready + 1], (SelectStep::PracticeKey(_), o) if o == practised),
            "{got:?}"
        );
        let without = compiled(&[
            "select = { path = \"library/a/a.bms\" }",
            "play = { input = \"log.tprl\", until = \"end\" }",
            "result = \"back\"",
            &practice(&format!("{FIELDS}, rounds = 1")),
            "quit = {}",
        ]);
        assert!(
            !without.iter().any(|(s, _)| *s == SelectStep::DecisionReady),
            "{without:?}"
        );
        let e = parse(
            &file(
                None,
                &[
                    "select = { path = \"library/a/a.bms\", decision = \"built-in\" }",
                    "play = { input = \"log.tprl\", until = \"end\" }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .expect_err("refused");
        assert!(e.contains("built-in"), "{e}");
    }

    /// A `practice` step picks its chart as `select` does, presses P, sets
    /// the page through its keys, starts, plays its rounds and goes back,
    /// each compiled step naming it; measures become indices, from 0 and
    /// past the last, and the input is the log's.
    #[test]
    fn practice_step_compiles_to_its_keys() {
        use super::PracticeGoal;
        let dir = folder();
        write_log(dir.path());
        let s = parse(
            &file(
                None,
                &[
                    &practice(&format!("{FIELDS}, rounds = 3")),
                    "select = { search = \"x\" }",
                    "play = { input = \"log.tprl\", until = \"end\" }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        let mut script = s.script;
        let mut got = Vec::new();
        while let Some(step) = script.front().cloned() {
            got.push((step, script.origin().unwrap()));
            script.done();
        }
        let o = origin(1, StepKind::Practice);
        let SelectStep::PickPath(target) = &got[0].0 else {
            panic!("{got:?}");
        };
        assert_eq!(
            target.path,
            dir.path().join("library").join("a").join("a.bms")
        );
        let input = InputRef::Log(0);
        let goal = PracticeGoal {
            from: 1,
            to: 3,
            speed: bms_model::Speed::from_twentieths(15).unwrap(),
            lead_in: TimeUs(2_500_000),
            value: rules::GaugeValue(20 * rules::GaugeValue::PERCENT),
            repeat: true,
            input,
        };
        assert_eq!(
            got[1..6],
            [
                (SelectStep::PracticeKey(input), o),
                (SelectStep::PracticeSet(goal), o),
                (SelectStep::PracticeStart, o),
                (SelectStep::Rounds(3), o),
                (SelectStep::PracticeBack, o),
            ]
        );
        // The step stands where `select` does: a select may follow, and
        // the same log is decoded once.
        assert_eq!(got[6].1, origin(2, StepKind::Select));
        assert_eq!(s.session.logs.len(), 1);
        assert_eq!(
            origin(1, StepKind::Practice).to_string(),
            "step 1 (practice)"
        );
    }

    /// Autoplay cannot practice: an `auto` input is refused naming the step.
    #[test]
    fn practice_with_auto_input_is_refused() {
        let dir = folder();
        let fields = FIELDS.replace("\"log.tprl\"", "\"auto\"");
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[&practice(&format!("{fields}, rounds = 1")), "quit = {}"]
            ),
            "step 1 (practice): input cannot be auto: an autoplay session cannot practice"
        );
    }

    /// Only repeated rounds are scripted: `repeat = false` is refused.
    #[test]
    fn practice_must_repeat() {
        let dir = folder();
        write_log(dir.path());
        let fields = FIELDS.replace("repeat = true", "repeat = false");
        assert_eq!(
            refused(
                dir.path(),
                None,
                &[&practice(&format!("{fields}, rounds = 1")), "quit = {}"]
            ),
            "step 1 (practice): repeat = false is not supported; give repeat = true"
        );
    }

    /// A practice where a result shows, or where a chosen chart's play is
    /// due, is out of place.
    #[test]
    fn practice_on_a_result_is_refused() {
        let dir = folder();
        write_log(dir.path());
        let step = practice(&format!("{FIELDS}, rounds = 1"));
        let select = "select = { path = \"library/a/a.bms\" }";
        let auto = "play = { input = \"auto\", until = \"end\" }";
        assert_eq!(
            refused(dir.path(), None, &[select, auto, &step, "quit = {}"]),
            "step 3 (practice): practice cannot follow step 2 (play)"
        );
        assert_eq!(
            refused(dir.path(), None, &[select, &step, "quit = {}"]),
            "step 2 (practice): practice cannot follow step 1 (select)"
        );
        // A practice is not the last step.
        assert_eq!(
            refused(dir.path(), None, &[&step]),
            "step 1 (practice): the last step must be quit"
        );
    }

    /// Measures, speed, lead-in, gauge and rounds outside their bounds are
    /// refused, the arithmetic checked: from from 1, to from `from`, the
    /// speed on the ladder, the lead-in on the page's grid, rounds below
    /// the most a count holds.
    #[test]
    fn practice_bounds_are_checked() {
        let dir = folder();
        write_log(dir.path());
        let base = "path = \"library/a/a.bms\", repeat = true, input = \"log.tprl\"";
        let ok = [
            ("from", "1"),
            ("to", "1"),
            ("speed", "100"),
            ("lead_in_ms", "1000"),
            ("gauge", "100"),
            ("rounds", "65534"),
        ];
        let step = |change: (&str, &str)| {
            let fields: Vec<String> = ok
                .iter()
                .map(|&(k, v)| format!("{k} = {}", if k == change.0 { change.1 } else { v }))
                .collect();
            format!("practice = {{ {base}, {} }}", fields.join(", "))
        };
        assert!(
            parse(
                &file(None, &[&step(("from", "1")), "quit = {}"]),
                dir.path()
            )
            .is_ok()
        );
        for (change, why) in [
            (("from", "0"), "from must be 1 or more"),
            (("from", "-9223372036854775808"), "from must be 1 or more"),
            (("from", "2"), "to must be from or more"),
            (("to", "-1"), "to must be from or more"),
            (
                ("speed", "45"),
                "speed must be a multiple of 5 from 50 to 150",
            ),
            (
                ("speed", "155"),
                "speed must be a multiple of 5 from 50 to 150",
            ),
            (
                ("speed", "77"),
                "speed must be a multiple of 5 from 50 to 150",
            ),
            (
                ("lead_in_ms", "500"),
                "lead_in_ms must be from 1000 to 10000 in steps of 500",
            ),
            (
                ("lead_in_ms", "10500"),
                "lead_in_ms must be from 1000 to 10000 in steps of 500",
            ),
            (
                ("lead_in_ms", "1250"),
                "lead_in_ms must be from 1000 to 10000 in steps of 500",
            ),
            (
                ("lead_in_ms", "9223372036854775807"),
                "lead_in_ms must be from 1000 to 10000 in steps of 500",
            ),
            (("gauge", "101"), "gauge must be a percentage from 0 to 100"),
            (
                ("gauge", "9223372036854775807"),
                "gauge must be a percentage from 0 to 100",
            ),
            (("rounds", "0"), "rounds must be 1 to 65534"),
            (("rounds", "65535"), "rounds must be 1 to 65534"),
        ] {
            assert_eq!(
                refused(dir.path(), None, &[&step(change), "quit = {}"]),
                format!("step 1 (practice): {why}"),
                "{change:?}"
            );
        }
    }

    /// The render thread stalled in a capture's read-back holds no lock:
    /// settling the play does not wait for it, and fails the capture as
    /// unanswered; the run's outcome is a failure.
    #[test]
    fn a_stalled_read_back_settles_without_waiting() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        use super::{FrameSink as _, ScenarioFrames};
        use crate::render_side::RenderEnd;

        let dir = tempfile::tempdir().unwrap();
        let c = Captures::of(vec![(TimeUs(0), dir.path().join("a.png"))]);
        c.arm(0);
        c.bind(1);
        let (entered, stalled) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let mut sink = ScenarioFrames::new(c.clone());
        let render = std::thread::spawn(move || {
            let screen = Stalling {
                entered,
                held: std::sync::Mutex::new(held),
            };
            sink.frame(&screen, &drawn(1, 0));
        });
        stalled.recv().unwrap();
        let begun = Instant::now();
        let settled = c.settle(1, false).unwrap();
        assert!(begun.elapsed() < Duration::from_secs(1));
        assert_eq!(settled.failed[0].1, "the render thread did not answer");
        let e = super::conclude::<std::io::Error>(Ok(()), None, None, RenderEnd::Joined, &c);
        assert!(e.is_ok(), "a reported failure is the script's to report");
        let e = super::conclude::<std::io::Error>(Ok(()), None, None, RenderEnd::Stuck, &c)
            .unwrap_err();
        assert!(e.to_string().contains("did not end cleanly"), "{e}");
        release.send(()).unwrap();
        render.join().unwrap();
        // The late outcome does not replace the failure.
        assert!(matches!(c.states()[0], CaptureState::Failed(_)));
    }

    /// A list that lost observations would misattribute: its capture fails.
    #[test]
    fn a_list_without_room_for_its_observations_is_refused() {
        let mut list = skin_model::DisplayList {
            element_count: 1,
            elements: Vec::new(),
            quads: Vec::new(),
            quads_dropped: 0,
            observations_dropped: 0,
        };
        let lines = super::display_list_lines(&list, &drawn(1, 2)).unwrap();
        assert!(lines.contains("\"element_count\":1"), "{lines}");
        list.observations_dropped = 3;
        let e = super::display_list_lines(&list, &drawn(1, 2)).unwrap_err();
        assert_eq!(e, "3 observations had no room");
    }

    /// A JSON string escapes what JSON must.
    #[test]
    fn a_json_string_is_escaped() {
        assert_eq!(super::json_string("a\"\\\n"), "\"a\\\"\\\\\\u000a\"");
    }

    /// A view of a full play's first candidate, playing.
    fn drawn_view() -> crate::measure::ViewStamp {
        crate::measure::ViewStamp {
            generation: 0,
            finalized: None,
            events: 0,
            playing: true,
            complete: false,
            closing: None,
            ready: Some((TimeUs(-1_000_000), TimeUs(0))),
            tally: eval::Tally::default(),
            shown_gauge: 0,
            gauges: [None; crate::measure::MAX_STAMPED_GAUGES],
            look: None,
        }
    }

    /// A presented play frame of `generation` at chart time `ms`.
    pub(crate) fn drawn(generation: u64, ms: i64) -> crate::measure::Drawn<'static> {
        crate::measure::Drawn {
            play: Some(TimeUs(ms * 1_000)),
            view: Some(drawn_view()),
            loading_stamp: None,
            screen_us: None,
            screen: crate::measure::ScreenKind::Play,
            selection: None,
            result: None,
            decision: None,
            presented_at: TimeUs(0),
            skin_loading: None,
            generation: Some(generation),
            timing: render::FrameTiming {
                cpu: std::time::Duration::ZERO,
                sprites: 0,
                dropped: 0,
                presented: true,
                present_count: None,
            },
            statistics: None,
            presentation: None,
            effects: &[],
            at: TimeUs(0),
        }
    }

    /// A screen whose read-back waits until released.
    struct Stalling {
        entered: std::sync::mpsc::Sender<()>,
        held: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl crate::render_side::Screen for Stalling {
        fn load(
            &mut self,
            _chart: &bms_model::Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), render::RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<render::FrameTiming, render::RenderError> {
            Err(render::RenderError::SurfaceLost)
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, render::RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.entered.send(()).unwrap();
            let _ = self.held.lock().unwrap().recv();
            Some((1, 1, vec![0; 4]))
        }
    }

    /// Captures under `dir` waiting on `axes`, each holding as given.
    fn screen_captures(dir: &Path, axes: &[(super::Axis, bool)]) -> Captures {
        let c = Captures::of(
            (0..axes.len())
                .map(|i| (TimeUs(0), dir.join(format!("{i}.png"))))
                .collect(),
        );
        for (i, &(axis, hold)) in axes.iter().enumerate() {
            c.set_axis(i, axis);
            c.set_hold(i, hold);
        }
        c
    }

    /// A presented frame of `screen`, presented at `at_ms` on the clock.
    fn screen_drawn(
        screen: crate::measure::ScreenKind,
        at_ms: i64,
    ) -> crate::measure::Drawn<'static> {
        let mut d = drawn(1, 0);
        d.play = None;
        d.view = None;
        d.generation = None;
        d.screen = screen;
        d.presented_at = TimeUs(at_ms * 1_000);
        d
    }

    /// A selection frame that applied update `revision` of `lifetime`.
    fn selection_frame(lifetime: u64, revision: u64, at_ms: i64) -> crate::measure::Drawn<'static> {
        let mut d = screen_drawn(crate::measure::ScreenKind::Selection, at_ms);
        d.selection = Some((lifetime, revision));
        d
    }

    /// A built-in result frame of result `seq` that drew end page `page`
    /// (and view `view`).
    fn result_frame(
        seq: u64,
        page: u64,
        view: Option<u64>,
        at_ms: i64,
    ) -> crate::measure::Drawn<'static> {
        let mut d = screen_drawn(crate::measure::ScreenKind::End, at_ms);
        d.result = Some(crate::measure::ResultStamp {
            id: Some(crate::measure::ResultId { seq, generation: 1 }),
            path: crate::measure::ResultPath::BuiltIn,
            page,
            view,
            sampled: TimeUs(at_ms * 1_000 - 2_000),
            timer_origin: None,
        });
        d
    }

    /// A sink of `c` with a screen whose read-back answers at once.
    fn released(c: &Captures) -> (super::ScenarioFrames, Stalling) {
        let sink = super::ScenarioFrames::new(c.clone());
        let (entered, seen) = std::sync::mpsc::channel();
        std::mem::forget(seen);
        let (_, held) = std::sync::mpsc::channel::<()>();
        let screen = Stalling {
            entered,
            held: std::sync::Mutex::new(held),
        };
        (sink, screen)
    }

    /// I1 on the render side: a selection capture takes only a selection
    /// frame of its update's lifetime that applied its revision or a later
    /// one; its origin is the first such frame presented.
    #[test]
    fn a_select_capture_takes_its_update_or_a_later_one() {
        use crate::measure::ScreenKind;
        let dir = tempfile::tempdir().unwrap();
        let c = screen_captures(dir.path(), &[(super::Axis::Select(TimeUs(250_000)), true)]);
        c.arm_select(0, 1, 5);
        let due = |d: &crate::measure::Drawn<'_>| {
            let mut list = c.0.lock().unwrap();
            matches!(
                super::capture_due(&mut list[0], d, None),
                Some(super::Due::Save(super::SCREEN_SAVE, _))
            )
        };
        assert!(!due(&selection_frame(1, 4, 0)), "an earlier revision");
        assert!(!due(&selection_frame(2, 9, 0)), "another lifetime");
        let mut other = selection_frame(1, 5, 0);
        other.screen = ScreenKind::Page;
        assert!(!due(&other), "another screen");
        // The origin: the first frame that applied revision 5 or later.
        assert!(!due(&selection_frame(1, 6, 10)));
        assert!(!due(&selection_frame(1, 6, 200)));
        assert!(due(&selection_frame(1, 7, 260)));
        assert!(matches!(
            c.states()[0],
            CaptureState::Selecting {
                origin: Some(TimeUs(10_000)),
                ..
            }
        ));
    }

    /// I3 on the render side: a result's origin is its first presented
    /// frame, kept while its pages change; a later result has its own. Each
    /// capture of a result takes only a frame of it.
    #[test]
    fn a_results_origin_is_its_first_presented_frame() {
        use super::FrameSink as _;
        let dir = tempfile::tempdir().unwrap();
        let c = screen_captures(
            dir.path(),
            &[
                (super::Axis::Result(TimeUs(400_000)), true),
                (super::Axis::Result(TimeUs(0)), true),
            ],
        );
        for (i, seq) in [(0, 1), (1, 2)] {
            c.set_state(
                i,
                CaptureState::Resulting {
                    seq,
                    page: 0,
                    view: None,
                },
            );
        }
        let (mut sink, screen) = released(&c);
        sink.frame(&screen, &result_frame(1, 1, None, 1_000));
        sink.frame(&screen, &result_frame(1, 2, None, 1_300));
        assert!(matches!(c.states()[0], CaptureState::Resulting { .. }));
        sink.frame(&screen, &result_frame(1, 3, None, 1_450));
        sink.frame(&screen, &result_frame(2, 4, None, 2_000));
        sink.finish();
        let side = |i: usize| fs::read_to_string(dir.path().join(format!("{i}.json"))).unwrap();
        let first = side(0);
        assert!(first.contains("\"result_origin_us\":1000000,"), "{first}");
        assert!(first.contains("\"result_us\":450000,"), "{first}");
        assert!(first.contains("\"result_seq\":1,"), "{first}");
        assert!(first.contains("\"result_page\":3,"), "{first}");
        assert!(first.contains("\"screen\":\"end\""), "{first}");
        assert!(first.contains("\"phase\":\"result\""), "{first}");
        let second = side(1);
        assert!(second.contains("\"result_origin_us\":2000000,"), "{second}");
        assert!(second.contains("\"result_seq\":2,"), "{second}");
    }

    /// A capture written on a result takes only a frame that drew the end
    /// page and view published after the steps before it.
    #[test]
    fn a_result_capture_waits_for_its_page_and_view() {
        let dir = tempfile::tempdir().unwrap();
        let c = screen_captures(dir.path(), &[(super::Axis::Result(TimeUs(0)), true)]);
        c.arm_result(0, 1, 3, Some(7));
        let due = |d: &crate::measure::Drawn<'_>| {
            let mut list = c.0.lock().unwrap();
            matches!(
                super::capture_due(&mut list[0], d, Some((1, TimeUs(0)))),
                Some(super::Due::Save(..))
            )
        };
        assert!(!due(&result_frame(1, 2, Some(7), 10)), "an earlier page");
        assert!(!due(&result_frame(1, 3, Some(6), 10)), "an earlier view");
        assert!(!due(&result_frame(1, 3, None, 10)), "no view");
        assert!(!due(&result_frame(2, 3, Some(7), 10)), "another result");
        assert!(due(&result_frame(1, 3, Some(7), 10)));
    }

    /// A leaving fails the captures of what was left whose frame was not
    /// taken, saying how; one being written is left to its writer; another
    /// result's are untouched.
    #[test]
    fn a_leaving_fails_only_the_left_screens_waiting_captures() {
        let dir = tempfile::tempdir().unwrap();
        let c = screen_captures(
            dir.path(),
            &[
                (super::Axis::Result(TimeUs(400_000)), true),
                (super::Axis::Result(TimeUs(0)), true),
                (super::Axis::Result(TimeUs(0)), true),
                (super::Axis::Select(TimeUs(600_000_000)), false),
            ],
        );
        let resulting = |seq| CaptureState::Resulting {
            seq,
            page: 0,
            view: None,
        };
        c.set_state(0, resulting(1));
        c.set_state(1, CaptureState::Saving(super::SCREEN_SAVE));
        c.set_state(2, resulting(2));
        c.arm_select(3, 1, 1);
        assert!(c.held(Some(1)) && c.held(Some(2)));
        let settled = c.leave(Some(1), super::Leaving::Left);
        assert_eq!(
            settled.failed,
            [(
                origin(1, StepKind::Capture),
                "the result left before a frame at 400 ms on its result axis".to_owned()
            )]
        );
        let states = c.states();
        assert_eq!(states[1], CaptureState::Saving(super::SCREEN_SAVE));
        assert_eq!(states[2], resulting(2));
        let settled = c.leave(None, super::Leaving::Ended);
        assert_eq!(
            settled.failed[0].1,
            "the run ended before a frame at 600000 ms on its select axis"
        );
    }

    /// A writer's outcome never replaces an ended capture's: the first
    /// ending stands.
    #[test]
    fn late_writer_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let c = screen_captures(dir.path(), &[(super::Axis::Result(TimeUs(0)), true)]);
        c.set_state(0, CaptureState::Failed("left".to_owned()));
        let backlog = std::sync::Arc::new((
            std::sync::Mutex::new(super::Backlog {
                frames: 1,
                bytes: 0,
            }),
            std::sync::Condvar::new(),
        ));
        let mut job = super::Job {
            pixels: Some((1, 1, vec![0; 4])),
            list: None,
            due: vec![(
                0,
                dir.path().join("0.png"),
                "{}\n".to_owned(),
                super::SCREEN_SAVE,
            )],
            bytes: 0,
            backlog,
            captures: c.clone(),
        };
        super::write_frame(&mut job);
        drop(job);
        assert_eq!(c.states()[0], CaptureState::Failed("left".to_owned()));
    }

    /// The refusals of selection and result captures, each naming its
    /// step.
    #[test]
    fn screen_captures_out_of_place_are_refused() {
        let dir = folder();
        let select = "select = { path = \"library/a/a.bms\" }";
        let auto = "play = { input = \"auto\", until = \"end\" }";
        let result_at = "capture = { result_at = 100, file = \"r.png\" }";
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &[
                    result_at,
                    select,
                    "play = { input = \"auto\", until = { at_ms = 0, command = \"retry\" } }",
                    auto,
                    "result = \"back\"",
                    "quit = {}"
                ]
            ),
            "step 1 (capture): no result follows: step 3 ends in a retry"
        );
        write_log(dir.path());
        let practice = practice(&format!("{FIELDS}, rounds = 1"));
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &[result_at, &practice, "quit = {}"]
            ),
            "step 1 (capture): a practice shows no result: step 2 practices"
        );
        assert!(
            refused(
                dir.path(),
                Some("out"),
                &[result_at, select, auto, "result = \"stay\"", "quit = {}"]
            )
            .starts_with("step 1 (capture): a holding capture cannot wait on stay"),
        );
        assert!(
            refused(
                dir.path(),
                Some("out"),
                &[select, auto, result_at, "result = \"stay\"", "quit = {}"]
            )
            .starts_with("step 3 (capture): a holding capture cannot wait on stay"),
        );
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &[
                    select,
                    auto,
                    "capture = { select_at = 0, file = \"s.png\" }",
                    "quit = {}"
                ]
            ),
            "step 3 (capture): select_at needs the select screen"
        );
        assert_eq!(
            refused(
                dir.path(),
                Some("out"),
                &[
                    "capture = { at = 0, hold = true, file = \"a.png\" }",
                    "quit = {}"
                ]
            ),
            "step 1 (capture): hold is for select_at and result_at"
        );
        // Not holding, a result capture may wait on stay.
        parse(
            &file(
                Some("out2"),
                &[
                    "capture = { result_at = 100, hold = false, file = \"r.png\" }",
                    select,
                    auto,
                    "result = \"stay\"",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
    }

    /// A `cursor` step moves the cursor as `select` does, without the
    /// choice.
    #[test]
    fn a_cursor_step_moves_without_choosing() {
        let dir = folder();
        let s = parse(
            &file(
                Some("out"),
                &[
                    "cursor = { path = \"library/b/b.bms\" }",
                    "capture = { select_at = 250, file = \"s.png\" }",
                    "cursor = { search = \"x\" }",
                    "quit = {}",
                ],
            ),
            dir.path(),
        )
        .unwrap();
        let mut script = s.script;
        let mut kinds = Vec::new();
        while let Some(step) = script.front().cloned() {
            kinds.push(step);
            script.done();
        }
        assert!(matches!(kinds[0], SelectStep::PickPath(_)), "{kinds:?}");
        assert_eq!(kinds[1], SelectStep::Capture(0));
        assert_eq!(kinds[2], SelectStep::Query("x".to_owned()));
        assert_eq!(kinds[3], SelectStep::PickSearched("x".to_owned()));
        assert_eq!(kinds[4], SelectStep::Close);
        assert!(!kinds.contains(&SelectStep::Enter), "{kinds:?}");
        let st = s.session.captures.status(0).unwrap();
        assert_eq!(st.axis, super::Axis::Select(TimeUs(250_000)));
        assert!(st.hold && !st.local);
    }

    /// The frame dump names a result's first frame `end.png`, its skin's
    /// or the built-in page, and only a selection frame `select.png`.
    #[test]
    fn the_frame_dump_names_a_result_frame_end() {
        use crate::measure::ScreenKind;
        let dir = tempfile::tempdir().unwrap();
        let mut dump = crate::dev::FrameDump::new(dir.path().to_path_buf(), TimeUs(1_000_000));
        assert!(dump.names(&screen_drawn(ScreenKind::Page, 0)).is_empty());
        assert_eq!(
            dump.names(&screen_drawn(ScreenKind::Selection, 0)),
            ["select.png"]
        );
        assert_eq!(
            dump.names(&screen_drawn(ScreenKind::Result, 0)),
            ["end.png"]
        );
        assert!(dump.names(&screen_drawn(ScreenKind::End, 0)).is_empty());
        let mut dump = crate::dev::FrameDump::new(dir.path().to_path_buf(), TimeUs(1_000_000));
        assert_eq!(dump.names(&screen_drawn(ScreenKind::End, 0)), ["end.png"]);
        assert_eq!(
            dump.names(&screen_drawn(ScreenKind::Loading, 0)),
            ["loading.png"]
        );
    }
}
