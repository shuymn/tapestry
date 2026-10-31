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
//! version = 1
//! output = "out"                 # needed by captures; made by the run
//!
//! [library]
//! roots = ["library"]            # relative to this file
//! rescan_ms = 50                 # optional
//!
//! [[step]]
//! capture = { at = 3000, file = "a.png" }   # chart ms, the next play's
//!
//! [[step]]
//! select = { path = "library/a/a.bms" }     # or { search = "text" }
//!
//! [[step]]
//! play = { input = "auto", until = "end" }
//! # input = "auto" | "<input log, relative to this file>"
//! # until = "end" | { at_ms = 2250, command = "retry" | "quit" }
//!
//! [[step]]
//! result = "next-preset"         # "prev-preset" | "retry" | "back"
//!
//! [[step]]
//! practice = { path = "library/a/a.bms", from = 1, to = 2, speed = 75, lead_in_ms = 2000, gauge = 20, repeat = true, input = "logs/a.tprl", rounds = 1 }
//! # or { search = "text", … }; measures from 1, both included; speed in
//! # percent; the displayed gauge's initial value in percent; rounds
//! # played, then the quit key at the next round's start and back
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
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
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
use crate::measure::Drawn;
use crate::render_side::{FrameSink, RenderEnd, Screen};

/// The one version of the file this build reads.
const VERSION: i64 = 1;

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
    pub until: Option<(TimeUs, Command)>,
}

/// `--dev-scenario`'s arguments.
#[derive(Debug)]
pub struct ScenarioArgs {
    /// The scenario file.
    pub file: PathBuf,
    /// `--dev-render-timeout`, as on song selection.
    pub render_timeout: Option<Duration>,
}

/// The arguments after `--dev-scenario`: the file, then
/// `--dev-render-timeout <s>` at most once.
///
/// # Errors
///
/// A missing file, or any other argument.
pub fn args(p: &mut lexopt::Parser) -> Result<ScenarioArgs, UsageError> {
    let file = PathBuf::from(p.value()?);
    let mut render_timeout = None;
    while let Some(arg) = p.next()? {
        match arg {
            lexopt::Arg::Long("dev-render-timeout") if render_timeout.is_none() => {
                render_timeout = Some(super::render_timeout(p)?);
            }
            other => return Err(other.unexpected().into()),
        }
    }
    Ok(ScenarioArgs {
        file,
        render_timeout,
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
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectSpec {
    path: Option<PathBuf>,
    search: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaySpec {
    input: String,
    until: Until,
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
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ResultKind {
    NextPreset,
    PrevPreset,
    Retry,
    Back,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureSpec {
    at: i64,
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

/// A `practice` step's rounds: `n` played, then the quit key at the start
/// of the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rounds {
    /// Their input.
    pub input: InputRef,
    /// How many are played.
    pub n: u16,
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
    let roots = roots(&file.library.roots, base)?;
    let compiled = compile(&file, base, &roots)?;
    let output = match &file.output {
        Some(o) => Some(make_output(base, o, &compiled.captures)?),
        None => None,
    };
    let captures = compiled
        .captures
        .into_iter()
        .map(|(file, at, origin)| Capture {
            at,
            path: output
                .as_deref()
                .map_or_else(PathBuf::new, |o| o.join(file)),
            origin,
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
        },
    })
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
    /// Each capture's file (under the output), time and step.
    captures: Vec<(PathBuf, TimeUs, Origin)>,
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
    // Each log decoded once, by its path.
    let mut log_paths: Vec<PathBuf> = Vec::new();
    let mut captures: Vec<(PathBuf, TimeUs, Origin)> = Vec::new();
    // Captures no play has followed yet.
    let mut waiting: Vec<Origin> = Vec::new();
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
                steps.push((SelectStep::Enter, o));
                chart_input = None;
                Seq::Chosen
            }
            (Seq::Chosen | Seq::Replay, Step::Play(p)) => {
                // A log by its path: the index it has, or gets once read.
                let log = if p.input == "auto" {
                    None
                } else {
                    let path = resolve(base, Path::new(&p.input)).map_err(at)?;
                    let k = log_paths.iter().position(|l| *l == path);
                    Some((k.unwrap_or(log_paths.len()), k.is_none().then_some(path)))
                };
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
                if let Some((_, Some(path))) = log {
                    logs.push(super::script(&path).map_err(|e| at(format!("{e:#}")))?);
                    log_paths.push(path);
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
                            CommandWord::Retry => Command::Retry,
                            CommandWord::Quit => Command::Quit,
                        };
                        Some((TimeUs(us), command))
                    }
                };
                steps.push((SelectStep::Play(PlayStep { input, until }), o));
                waiting.clear();
                match until {
                    None => Seq::Result,
                    Some((_, Command::Retry)) => Seq::Replay,
                    Some(_) => Seq::Select,
                }
            }
            (Seq::Result, Step::Result(r)) => {
                let (step, next) = match r {
                    ResultKind::NextPreset => (SelectStep::Next, Seq::Result),
                    ResultKind::PrevPreset => (SelectStep::Prev, Seq::Result),
                    ResultKind::Retry => (SelectStep::Retry, Seq::Replay),
                    ResultKind::Back => (SelectStep::Back, Seq::Select),
                };
                steps.push((step, o));
                next
            }
            (Seq::Select | Seq::Result, Step::Capture(c)) => {
                let us =
                    c.at.checked_mul(1_000)
                        .ok_or_else(|| at("at is too large".to_owned()))?;
                let rel = capture_file(&c.file).map_err(at)?;
                let key = rel.to_string_lossy().to_lowercase();
                if let Some((_, _, other)) = captures
                    .iter()
                    .find(|(f, _, _)| f.to_string_lossy().to_lowercase() == key)
                {
                    return Err(at(format!(
                        "{} is also step {}'s file",
                        c.file.display(),
                        other.index
                    )));
                }
                let k = u16::try_from(captures.len())
                    .map_err(|_| at("too many captures".to_owned()))?;
                captures.push((rel, TimeUs(us), o));
                waiting.push(o);
                steps.push((SelectStep::Capture(k), o));
                seq
            }
            (Seq::Select, Step::Practice(p)) => {
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
                // The rounds after the last are counted to its start.
                let n = u16::try_from(p.rounds)
                    .ok()
                    .filter(|&n| n >= 1 && n.checked_add(1).is_some())
                    .ok_or_else(|| at(format!("rounds must be 1 to {}", u16::MAX - 1)))?;
                steps.push((SelectStep::PracticeKey(goal.input), o));
                steps.push((SelectStep::PracticeSet(goal), o));
                steps.push((SelectStep::PracticeStart, o));
                steps.push((
                    SelectStep::Rounds(Rounds {
                        input: goal.input,
                        n,
                    }),
                    o,
                ));
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

/// A `practice` step's page settings and input, checked: measures from 1
/// with `to ≥ from`, a speed on the ladder in percent, a lead-in on the
/// page's grid, a gauge value in percent, repeat on, and an input log (an
/// autoplay session cannot practice), decoded once by its path.
fn practice_goal(
    p: &PracticeSpec,
    base: &Path,
    log_paths: &mut Vec<PathBuf>,
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
    let k = match log_paths.iter().position(|l| *l == path) {
        Some(k) => k,
        None => {
            logs.push(super::script(&path).map_err(|e| format!("{e:#}"))?);
            log_paths.push(path);
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
    captures: &[(PathBuf, TimeUs, Origin)],
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
    for (file, _, o) in captures {
        if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
            let dir = at.join(dir);
            fs::create_dir_all(&dir).map_err(|e| format!("{o}: {}: {e}", dir.display()))?;
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
    /// The render thread writes it from a frame of this generation.
    Saving(u64),
    /// Written.
    Succeeded,
    /// Not written, and why.
    Failed(String),
}

/// One capture: the first presented frame at or after `at` of the play
/// it is bound to, written to `path`.
#[derive(Clone, Debug)]
pub struct Capture {
    /// The chart time.
    pub at: TimeUs,
    /// The file, created new.
    pub path: PathBuf,
    /// Its step.
    pub origin: Origin,
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

    /// The play of `generation` starts: it takes every capture armed.
    pub fn bind(&self, generation: u64) {
        for c in self.lock().iter_mut() {
            if c.state == CaptureState::Pending {
                c.state = CaptureState::Bound(generation);
            }
        }
    }

    /// The play of `generation` is over, its frames drawn when `answered`
    /// (the render thread answered its end): each capture bound to it ends
    /// here. Returns how those not reported yet ended; `Err` when the
    /// render thread holds the list (it did not answer).
    pub fn settle(&self, generation: u64, answered: bool) -> Result<Settled, ()> {
        let mut list = self.try_lock().ok_or(())?;
        let mut settled = Settled::default();
        let Settled { written, failed } = &mut settled;
        for c in list.iter_mut() {
            match c.state {
                CaptureState::Bound(g) if g == generation => {
                    c.state = CaptureState::Failed(if answered {
                        format!("the play ended before a frame at {} ms", c.at.0 / 1_000)
                    } else {
                        "the render thread did not answer".to_owned()
                    });
                }
                CaptureState::Saving(g) if g == generation => {
                    c.state = CaptureState::Failed("the render thread did not answer".to_owned());
                }
                _ => {}
            }
            if c.reported {
                continue;
            }
            match &c.state {
                CaptureState::Succeeded => written.push((c.origin, c.path.clone())),
                CaptureState::Failed(why) => failed.push((c.origin, why.clone())),
                _ => continue,
            }
            c.reported = true;
        }
        Ok(settled)
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
                CaptureState::Pending | CaptureState::Bound(_) | CaptureState::Saving(_) => {
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
                    at,
                    path,
                    origin,
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
/// frame is read back, encoded or written.
pub struct ScenarioFrames {
    captures: Captures,
    /// How long each capture's read-back stalls first: a test's stand-in
    /// for a render thread stuck in its work ([`STALL_ENV`]).
    stall: Option<Duration>,
}

/// The environment variable that stalls each capture's read-back by its
/// value in milliseconds, for tests of a stuck render thread.
pub const STALL_ENV: &str = "TAPESTRY_DEV_CAPTURE_STALL_MS";

impl ScenarioFrames {
    /// The sink of `captures` (tests).
    #[cfg(test)]
    pub const fn new(captures: Captures) -> Self {
        Self {
            captures,
            stall: None,
        }
    }

    /// The sink of `captures`, stalled as [`STALL_ENV`] says.
    pub fn from_env(captures: Captures) -> Self {
        let stall = std::env::var(STALL_ENV)
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(Duration::from_millis);
        Self { captures, stall }
    }
}

impl FrameSink for ScenarioFrames {
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>) {
        let (Some(generation), Some(t)) = (drawn.generation, drawn.play) else {
            return;
        };
        if !drawn.timing.presented {
            return;
        }
        let due: Vec<(usize, PathBuf)> = {
            let mut list = self.captures.lock();
            list.iter_mut()
                .enumerate()
                .filter(|(_, c)| c.state == CaptureState::Bound(generation) && t >= c.at)
                .map(|(i, c)| {
                    c.state = CaptureState::Saving(generation);
                    (i, c.path.clone())
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }
        if let Some(stall) = self.stall {
            std::thread::sleep(stall);
        }
        // Read back and encoded once, for every capture due.
        let image = screen
            .read_back()
            .map(|(w, h, pixels)| super::png(w, h, &pixels));
        for (i, path) in due {
            let outcome = match &image {
                None => Err("nothing to read back".to_owned()),
                Some(png) => write_new(&path, png).map_err(|e| e.to_string()),
            };
            if let Some(c) = self.captures.lock().get_mut(i)
                && c.state == CaptureState::Saving(generation)
            {
                c.state = match outcome {
                    Ok(()) => CaptureState::Succeeded,
                    Err(why) => CaptureState::Failed(format!("{}: {why}", path.display())),
                };
            }
        }
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
    pub until: Option<(TimeUs, Command)>,
    /// The practice page as last shown.
    pub practice: Option<game::PracticeScreen>,
    /// Whether the page was shown since a step last pressed a key on it:
    /// a key is pressed only on what the page shows.
    pub shown: bool,
    /// The rounds of the current `practice` step started so far.
    pub rounds: u16,
}

/// Runs the scenario of `args`: song selection, headless, on its steps.
///
/// # Errors
///
/// The file's refusal, or the run's first failure.
pub fn run(args: ScenarioArgs, clock: QpcClock) -> anyhow::Result<()> {
    let Scenario {
        rescan_ms,
        script,
        session,
    } = load(&args.file)?;
    crate::select(
        crate::args::Select {
            input: crate::PlayInput::Autoplay,
            dev: super::SelectOptions {
                headless: true,
                script: Some(script),
                rescan_ms,
                frames: None,
                render_timeout: args.render_timeout,
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
    use game::Command;

    use super::{
        CaptureState, Captures, InputRef, Origin, PlayStep, SelectStep, StepKind, find, parse,
        write_new,
    };

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
        let mut text = "version = 1\n".to_owned();
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
                play(Some((TimeUs(2_000), Command::Retry))),
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
                play(Some((TimeUs(3_000), Command::Quit))),
                origin(8, StepKind::Play),
            ),
            (SelectStep::Close, origin(9, StepKind::Quit)),
        ];
        assert_eq!(got, expected);
        assert_eq!(origin(7, StepKind::Select).to_string(), "step 7 (select)");
        assert!(dir.path().join("out").is_dir());
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
        let mut text = "version = 1\nstep = [".to_owned();
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
        let mut text = "version = 1\n".to_owned();
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
        let e = parse("version = 1\nfast = true\n", dir.path()).unwrap_err();
        assert!(e.contains("fast"), "{e}");
        assert_eq!(
            parse("version = 2\nfast = true\n", dir.path()).unwrap_err(),
            "version 2 is not supported; this build reads version 1"
        );
        assert_eq!(
            parse("[library]\n", dir.path()).unwrap_err(),
            "no version; this build reads version 1"
        );
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
            "the play ended before a frame at 1000 ms"
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

    /// A `practice` step picks its chart as `select` does, presses P, sets
    /// the page through its keys, starts, plays its rounds and goes back,
    /// each compiled step naming it; measures become indices, from 0 and
    /// past the last, and the input is the log's.
    #[test]
    fn practice_step_compiles_to_its_keys() {
        use super::{PracticeGoal, Rounds};
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
                (SelectStep::Rounds(Rounds { input, n: 3 }), o),
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

    /// A presented play frame of `generation` at chart time `ms`.
    pub(crate) fn drawn(generation: u64, ms: i64) -> crate::measure::Drawn<'static> {
        crate::measure::Drawn {
            play: Some(TimeUs(ms * 1_000)),
            menu: false,
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
        fn build(&mut self, _f: &render::FrameInput<'_>) {}
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
}
