//! Development features, compiled only with the `dev-tools` feature.
//!
//! `play <chart> --dev-scripted-input <log>` plays the chart on the same
//! threads as the game, without a window or devices: the input log's
//! events drive the input side as a script on the play's timeline, fed by
//! the clocked input thread, the audio engine plays into a recording of a
//! simulated device paced by the clock, and the renderer draws offscreen
//! on WARP. `play --auto <chart> --dev-headless` runs autoplay the same
//! way. `--dev-frames <dir>` also writes a READY frame, a play frame
//! and the end screen there as PNG files. `--dev-overlay` shows the
//! measurement overlay from the start. `--dev-start <ms>` plays a mid-chart
//! start from that time of the chart to its end. `--dev-size <W>x<H>` sets
//! the offscreen target's size.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

pub mod checkpoints;
pub mod scenario;

/// How long each loudness measurement holds before its first block, in
/// milliseconds (a script's `slow-measuring`): a load's and the
/// background's alike.
static SLOW_MEASURING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every loudness measurement from now on holds `ms` before its first
/// block (`slow-measuring`).
pub fn slow_measuring(ms: u64) {
    SLOW_MEASURING.store(ms, Ordering::Relaxed);
}

/// Holds a measurement as `slow-measuring` says, asking `stop` every
/// 10 ms as a measurement asks before each block: `false` once it says
/// to stop.
pub fn hold_measuring(stop: &mut dyn FnMut() -> bool) -> bool {
    let total = Duration::from_millis(SLOW_MEASURING.load(Ordering::Relaxed));
    if total.is_zero() {
        return true;
    }
    println!("dev: the measurement holds {} ms", total.as_millis());
    let _stall = Stall::begin();
    let began = std::time::Instant::now();
    loop {
        if stop() {
            return false;
        }
        let held = began.elapsed();
        if held >= total {
            return true;
        }
        std::thread::sleep((total - held).min(Duration::from_millis(10)));
    }
}

/// How long the next load's decode holds at its first decoded file, ms
/// (a script's `slow-decoding`).
static SLOW_DECODING: AtomicU64 = AtomicU64::new(0);
/// How long the render thread holds the next attempt's preparation, ms
/// (a script's `slow-preparing`).
static SLOW_PREPARING: AtomicU64 = AtomicU64::new(0);
/// How long the next pending play skin's specialization waits after
/// its wait is shown, before its job starts, ms (a script's
/// `slow-specializing`).
static SLOW_SPECIALIZING: AtomicU64 = AtomicU64::new(0);
/// How long the render thread holds the next chart's load, ms (a
/// script's `slow-chart-load`).
static SLOW_CHART_LOAD: AtomicU64 = AtomicU64::new(0);
/// A load's phase holds now (`slow-decoding`, `slow-measuring`,
/// `slow-preparing`): a `press-loading` press waits for it.
static STALLED: AtomicBool = AtomicBool::new(false);

/// Marks a hold for its length.
struct Stall;

impl Stall {
    fn begin() -> Self {
        STALLED.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for Stall {
    fn drop(&mut self) {
        STALLED.store(false, Ordering::SeqCst);
    }
}
/// The phase of the next load in which F2 is pressed on the window (a
/// script's `press-loading`): one of [`LoadPhase`], `u8::MAX` for none.
static PRESS_LOADING: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

/// A phase of a load the game thread waits through, serving the
/// adjustments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadPhase {
    /// The sounds decode.
    Decoding,
    /// The loudness is measured.
    Measuring,
    /// The render thread prepares the attempt.
    Preparing,
    /// The render thread loads the chart.
    ChartLoad,
}

impl LoadPhase {
    /// The phase a script names.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "decoding" => Some(Self::Decoding),
            "measuring" => Some(Self::Measuring),
            "preparing" => Some(Self::Preparing),
            "chart-load" => Some(Self::ChartLoad),
            _ => None,
        }
    }
}

/// The next load's decode holds `ms` at its first decoded file
/// (`slow-decoding`).
pub fn slow_decoding(ms: u64) {
    SLOW_DECODING.store(ms, Ordering::Relaxed);
}

/// Holds the decode as `slow-decoding` says, once: on the decode's own
/// thread, while the game thread serves the adjustments.
pub fn hold_decoding() {
    hold(LoadPhase::Decoding);
}

/// The render thread holds the next chart's load `ms`
/// (`slow-chart-load`).
pub fn slow_chart_load(ms: u64) {
    SLOW_CHART_LOAD.store(ms, Ordering::Relaxed);
}

/// Holds `phase` on the thread running it as its `slow-` step says, once.
/// Only a phase the render thread runs holds here.
pub fn hold(phase: LoadPhase) {
    let (slow, what) = match phase {
        LoadPhase::ChartLoad => (&SLOW_CHART_LOAD, "the chart's load"),
        LoadPhase::Preparing => (&SLOW_PREPARING, "the preparation"),
        LoadPhase::Decoding => (&SLOW_DECODING, "the decode"),
        LoadPhase::Measuring => return,
    };
    let ms = slow.swap(0, Ordering::Relaxed);
    if ms > 0 {
        println!("dev: {what} holds {ms} ms");
        let _stall = Stall::begin();
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// The render thread holds the next attempt's preparation `ms`
/// (`slow-preparing`).
pub fn slow_preparing(ms: u64) {
    SLOW_PREPARING.store(ms, Ordering::Relaxed);
}

/// Holds the attempt's preparation on the render thread as
/// `slow-preparing` says, once, while the game thread waits for it.
pub fn hold_preparing() {
    hold(LoadPhase::Preparing);
}

/// F2 is pressed on the window at the first wait of `phase` in the next
/// load (`press-loading`): an autoplay session's speed key, which reaches
/// the adjustments through the window's queue.
pub fn press_loading(phase: LoadPhase) {
    PRESS_LOADING.store(phase as u8, Ordering::Relaxed);
}

/// Presses F2 on the window when a `press-loading` press is due in
/// `phase`, the load's phase now, while that phase holds.
pub fn press_due(controls: &crate::controls::Controls, phase: LoadPhase) {
    if !STALLED.load(Ordering::SeqCst) {
        return;
    }
    let due = PRESS_LOADING
        .compare_exchange(phase as u8, u8::MAX, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok();
    if due {
        controls.window_key(1, input::WindowKeyEvent::Press, false);
        controls.window_key(1, input::WindowKeyEvent::Release, false);
        println!("dev: F2 pressed while {phase:?}");
    }
}

/// What a script does to the next specialization of its stage (a pending
/// play skin's, or a result skin's image stage or layout) once its Lua
/// executes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Specializing {
    /// Press the quit key (`quit-specializing`).
    Quit,
    /// Close the window (`close-specializing`).
    Close,
    /// Fail the input thread, as its hook's end does
    /// (`input-failure-specializing`).
    InputFailure,
    /// Hold every VM's destruction, press the quit key, and close the
    /// window once the quit has waited past the stop bound
    /// (`escalate-specializing`).
    Escalate,
    /// As [`Escalate`](Self::Escalate), failing the input thread instead
    /// of closing (`escalate-input-failure-specializing`).
    EscalateInputFailure,
}

impl Specializing {
    /// The mode a script or `--dev-specializing` names.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "quit" => Some(Self::Quit),
            "close" => Some(Self::Close),
            "input-failure" => Some(Self::InputFailure),
            "escalate" => Some(Self::Escalate),
            "escalate-input-failure" => Some(Self::EscalateInputFailure),
            _ => None,
        }
    }

    /// The mode and the stage a script or `--dev-specializing` names: a
    /// play skin's mode alone, a result skin's with `result-image-` or
    /// `result-layout-` before it.
    pub fn staged(name: &str) -> Option<(SpecializingStage, Self)> {
        if let Some(mode) = name.strip_prefix("result-image-") {
            Self::named(mode).map(|m| (SpecializingStage::ResultImage, m))
        } else if let Some(mode) = name.strip_prefix("result-layout-") {
            Self::named(mode).map(|m| (SpecializingStage::ResultLayout, m))
        } else {
            Self::named(name).map(|m| (SpecializingStage::Play, m))
        }
    }

    /// Whether every VM's destruction is held and the quit pressed first.
    const fn escalates(self) -> bool {
        matches!(self, Self::Escalate | Self::EscalateInputFailure)
    }
}

/// Whose job a [`Specializing`] step acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecializingStage {
    /// A pending play skin's specialization.
    Play,
    /// A result skin's image stage in an attempt's Preparing.
    ResultImage,
    /// A result skin's layout on P or a switch.
    ResultLayout,
}

/// The specialization's script step armed, the stage it acts on, its
/// steps' baseline, and when an escalation's quit was pressed.
#[expect(
    clippy::type_complexity,
    reason = "one armed step, read and written whole under its lock"
)]
static SPECIALIZING: Mutex<Option<(Specializing, SpecializingStage, u64, Option<Instant>)>> =
    Mutex::new(None);

/// Instruction hook steps past the job's start that show its Lua
/// executing.
const SPECIALIZING_STEPS: u64 = 20;

/// The armed quit was pressed during a specialization, until a practice's
/// rounds take it as their expected end.
static SPECIALIZATION_QUIT: AtomicBool = AtomicBool::new(false);

/// Whether the armed quit was pressed during a specialization, taken: a
/// practice's rounds, back on their page, end there.
pub fn take_specialization_quit() -> bool {
    SPECIALIZATION_QUIT.swap(false, Ordering::AcqRel)
}

/// Arms `s` for the next specialization at `stage`.
pub fn specializing(stage: SpecializingStage, s: Specializing) {
    *SPECIALIZING.lock().unwrap_or_else(PoisonError::into_inner) = Some((s, stage, u64::MAX, None));
}

/// The next specialization holds `ms` once its wait is shown
/// (`slow-specializing`).
pub fn slow_specializing(ms: u64) {
    SLOW_SPECIALIZING.store(ms, Ordering::Relaxed);
}

/// A specialization's job starts, its wait shown: it holds as
/// `slow-specializing` says, once, so the wait is drawn; an armed step
/// counts the Lua's steps from here, and an escalation holds every VM's
/// destruction.
pub fn specialization_began() {
    let ms = SLOW_SPECIALIZING.swap(0, Ordering::Relaxed);
    if ms > 0 {
        println!("dev: the specialization holds {ms} ms");
        std::thread::sleep(Duration::from_millis(ms));
    }
    stage_began(SpecializingStage::Play);
}

/// A job of `stage` starts: an armed step for that stage counts the Lua's
/// steps from here, and an escalation holds every VM's destruction.
pub fn stage_began(stage: SpecializingStage) {
    let mut armed = SPECIALIZING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((s, at, from, _)) = armed.as_mut()
        && *at == stage
    {
        *from = skin_beatoraja::lua::vm_hooks::steps();
        if s.escalates() {
            skin_beatoraja::lua::vm_hooks::hold();
        }
    }
}

/// Acts on the armed step once the specialization's Lua executes, asked
/// at each look of the job's wait; `fail_input` fails the input thread.
pub fn specialization_signal(controls: &crate::controls::Controls, fail_input: impl Fn()) {
    let mut armed = SPECIALIZING.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((s, _, from, quit_at)) = armed.as_mut() else {
        return;
    };
    if skin_beatoraja::lua::vm_hooks::steps() < from.saturating_add(SPECIALIZING_STEPS) {
        return;
    }
    let quit = || {
        controls.command(game::Command::Quit);
        println!("dev: quit pressed while specializing");
    };
    let close = || {
        controls.close();
        println!("dev: closed while specializing");
    };
    let fail = || {
        fail_input();
        println!("dev: input failed while specializing");
    };
    match (*s, *quit_at) {
        (Specializing::Quit, _) => {
            quit();
            SPECIALIZATION_QUIT.store(true, Ordering::Release);
            *armed = None;
        }
        (Specializing::Close, _) => {
            close();
            *armed = None;
        }
        (Specializing::InputFailure, _) => {
            fail();
            *armed = None;
        }
        (Specializing::Escalate | Specializing::EscalateInputFailure, None) => {
            quit();
            *quit_at = Some(Instant::now());
        }
        (s, Some(at)) => {
            if at.elapsed() >= crate::imports::STOP_WAIT + Duration::from_millis(500) {
                if s == Specializing::Escalate {
                    close();
                } else {
                    fail();
                }
                *armed = None;
            }
        }
    }
}

/// Prints the adjustments' published hi-speed after `phase` of a load.
pub fn print_adjustments(phase: &str, driver: Option<&game::AdjustDriver>) {
    let Some(h) = driver.and_then(|d| d.snapshot().hispeed) else {
        return;
    };
    let (n, d) = (u128::from(h.num()), u128::from(h.den()));
    let x = (200 * n + d) / (2 * d);
    println!(
        "dev: adjustments after {phase}: hi-speed {}.{:02}",
        x / 100,
        x % 100
    );
}
/// Background measurements hold at their gate (`hold-background`).
static GATE_ARMED: AtomicBool = AtomicBool::new(false);

/// A background measurement holds at its gate.
static GATE_HELD: AtomicBool = AtomicBool::new(false);

/// Every background measurement from now on holds at its gate until
/// [`release_background`] (`hold-background`).
pub fn hold_background() {
    GATE_ARMED.store(true, Ordering::SeqCst);
}

/// Releases the gate: the measurement held there goes on, and none holds
/// there again.
pub fn release_background() {
    GATE_ARMED.store(false, Ordering::SeqCst);
}

/// Whether a background measurement holds at its gate.
pub fn background_held() -> bool {
    GATE_HELD.load(Ordering::SeqCst)
}

/// Clears [`GATE_HELD`] however the hold ends.
struct Held;

impl Drop for Held {
    fn drop(&mut self) {
        GATE_HELD.store(false, Ordering::SeqCst);
    }
}

/// Holds a background measurement under way at its gate while
/// `hold-background` holds it, asking `stop` every 10 ms as a
/// measurement asks before each block: `false` once it says to stop.
pub fn background_gate(stop: &dyn Fn() -> bool) -> bool {
    if !GATE_ARMED.load(Ordering::SeqCst) {
        return true;
    }
    let _held = Held;
    // Printed before the hold is published: a script waiting on it reads
    // the line before anything it does next.
    println!("dev: the background measurement holds at its gate");
    GATE_HELD.store(true, Ordering::SeqCst);
    while GATE_ARMED.load(Ordering::SeqCst) {
        if stop() {
            println!("dev: the background measurement stopped at its gate");
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

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

/// The offscreen target's size without `--dev-size` or a scenario's `size`.
pub const DEFAULT_SIZE: (u32, u32) = (1280, 720);

/// The largest side `--dev-size` and a scenario's `size` take: what the
/// renderer's offscreen target is sure to hold.
pub const MAX_SIZE_SIDE: u32 = 8192;

/// `w`×`h` if each side is 1 to [`MAX_SIZE_SIDE`].
///
/// # Errors
///
/// Why it is not.
pub fn checked_size(w: i64, h: i64) -> Result<(u32, u32), String> {
    let side = |v: i64| {
        u32::try_from(v)
            .ok()
            .filter(|s| (1..=MAX_SIZE_SIDE).contains(s))
    };
    match (side(w), side(h)) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err(format!(
            "a size of {w}x{h}: each side must be 1 to {MAX_SIZE_SIDE}"
        )),
    }
}

/// The value of `--dev-size`: `<W>x<H>`.
fn size(p: &mut lexopt::Parser) -> Result<(u32, u32), UsageError> {
    let value = p.value()?;
    let value = value.to_string_lossy();
    let parsed = value
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    let Some((w, h)) = parsed else {
        return Err(UsageError::Usage("--dev-size takes <W>x<H>".to_owned()));
    };
    checked_size(w, h).map_err(UsageError::Usage)
}

/// The development options of `play`.
#[derive(Debug, Default)]
pub struct Options {
    /// The input log to play.
    pub scripted_input: Option<PathBuf>,
    /// Where to write frames.
    pub frames: Option<PathBuf>,
    /// The offscreen target's size (`--dev-size`), for a headless run.
    pub size: Option<(u32, u32)>,
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
    /// F1 and F2, by scan code, pressed and released on a scripted play's
    /// keyboard at `adjust_at_ms` (`--dev-speed`): packets the adjustment
    /// tap observes and recording does not, as the speed keys are.
    pub speed_keys: Vec<u32>,
    /// Keys held on a scripted play's keyboard while it loads, before its
    /// first opening (`--dev-loading`): each by scan code with its press and
    /// release times after loading starts, fed only to the adjustment tap.
    pub loading_keys: Vec<(u32, TimeUs, TimeUs)>,
    /// How long after a cancelled READY's release a scripted play's rearm
    /// is answered at the earliest (`--dev-rearm-delay <ms>`): a game thread
    /// late to ask, made on purpose, which the script's chart times do not
    /// follow.
    pub rearm_delay: TimeUs,
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
    /// Applies the options to a play's rig, the same on every path that
    /// plays a chart (`play`, interactive or headless).
    pub fn tune(self, rig: &mut crate::play::PlayerRig<'_>) {
        if self.auto_retry {
            rig.auto_retries = 1;
            rig.quit_after_end = Some(Duration::from_millis(300));
        }
        if self.overlay {
            rig.controls.toggle_overlay();
        }
        if let Some(bound) = self.render_timeout {
            rig.render.set_timeouts(bound, bound);
        }
        rig.adjust_in_play = self.adjust_in_play;
        rig.adjust_at = TimeUs(self.adjust_at_ms.saturating_mul(1_000));
        rig.adjust_on_end = self.adjust_on_end;
        rig.result_script = self.result;
        rig.loading_script = !self.loading_keys.is_empty();
        rig.speed_in_play = self.speed_keys;
    }

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
            _ => Ok(log.map_or(input, |log| {
                let at = TimeUs(self.adjust_at_ms.saturating_mul(1_000));
                crate::PlayInput::Script(ScriptInput {
                    log,
                    speed: self.speed_keys.iter().map(|&k| (k, at)).collect(),
                    loading: self.loading_keys.clone(),
                    rearm_delay: self.rearm_delay,
                })
            })),
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
            "dev-loading" => {
                self.loading_keys =
                    loading_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-speed" => {
                self.speed_keys =
                    speed_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-adjust-end" => {
                self.adjust_on_end =
                    adjust_keys(&p.value()?.to_string_lossy()).map_err(UsageError::Usage)?;
            }
            "dev-scripted-input" => self.scripted_input = Some(PathBuf::from(p.value()?)),
            "dev-frames" => self.frames = Some(PathBuf::from(p.value()?)),
            "dev-size" => self.size = Some(size(p)?),
            "dev-render-timeout" => self.render_timeout = Some(render_timeout(p)?),
            "dev-start" => self.start = Some(start(p)?),
            "dev-rearm-delay" => {
                let bad = || UsageError::Usage("--dev-rearm-delay takes milliseconds".to_owned());
                let ms: u32 = p.value()?.to_string_lossy().parse().map_err(|_| bad())?;
                self.rearm_delay = TimeUs(i64::from(ms) * 1_000);
            }
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
    /// frame, and the first play's READY state, play and end frames.
    pub frames: Option<PathBuf>,
    /// The offscreen target's size (`--dev-size`), for a headless run.
    pub size: Option<(u32, u32)>,
    /// The render thread's bounds, as [`Options::render_timeout`]
    /// (`--dev-render-timeout`).
    pub render_timeout: Option<std::time::Duration>,
    /// How long the end of the run waits for the scan and the background
    /// measurement to stop, in place of [`crate::shelf::STOP_TIMEOUT`]
    /// (`--dev-stop-timeout <seconds>`): a loaded machine can starve their
    /// background-mode threads for seconds.
    pub stop_timeout: Option<std::time::Duration>,
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
            "dev-size" => self.size = Some(size(p)?),
            "dev-render-timeout" => self.render_timeout = Some(render_timeout(p)?),
            "dev-stop-timeout" => {
                let secs = p.value()?.to_string_lossy().parse().map_err(|_| {
                    UsageError::Usage("--dev-stop-timeout takes seconds".to_owned())
                })?;
                self.stop_timeout = Some(std::time::Duration::from_secs(secs));
            }
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
    /// Wait until the folder row of this name is listed, then enter it with
    /// the arrow keys and Enter.
    Folder(String),
    /// On Top: enter the all-chart list with the arrow keys and Enter.
    AllCharts,
    /// Press Escape on the select screen: back one place.
    Up,
    /// Fail unless the cursor is on this row: `all`, a folder's name or a
    /// chart's file name.
    Row(String),
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
    /// Wait on the select screen until this file exists, at most
    /// [`STEP_WATCHDOG`]: a test's signal that it changed something the
    /// run must not see yet.
    File(PathBuf),
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
    /// On a result: nothing, until it leaves on its own (a result skin's
    /// scene time); on the select screen, done at once.
    Stay,
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
    /// Press the quit key once the next chart's load starts measuring its
    /// loudness.
    QuitMeasuring,
    /// Press the quit key once the next chart's load has decoded its first
    /// sound file.
    QuitDecoding,
    /// The next chart's load holds this many milliseconds before its
    /// sounds, cancelled by the quit key or a close as the measurement is:
    /// a slow load.
    SlowLoad(u64),
    /// Halfway through the next slow load, press the quit key.
    QuitLoading,
    /// Halfway through the next slow load, close the window.
    CloseLoading,
    /// What the next specialization of its stage gets once its Lua
    /// executes (`quit-specializing`, `close-specializing`,
    /// `escalate-specializing`, each optionally after `result-image-` or
    /// `result-layout-`).
    Specializing(SpecializingStage, Specializing),
    /// Every loudness measurement from here on holds this many
    /// milliseconds before its first block, stopped as a measurement is: a
    /// slow measurement, a load's or the background's.
    SlowMeasuring(u64),
    /// The next load's decode holds this many milliseconds at its first
    /// file (`slow-decoding`).
    SlowDecoding(u64),
    /// The next attempt's preparation holds this many milliseconds on the
    /// render thread (`slow-preparing`).
    SlowPreparing(u64),
    /// The next pending play skin's specialization holds this many
    /// milliseconds once its wait is shown (`slow-specializing`).
    SlowSpecializing(u64),
    /// The next chart's load holds this many milliseconds on the render
    /// thread (`slow-chart-load`).
    SlowChartLoad(u64),
    /// F2 is pressed on the window in this phase of the next load
    /// (`press-loading`).
    PressLoading(LoadPhase),
    /// Every background measurement from here on holds at its gate, past
    /// its first stop check and its line, until a load waits for it or the
    /// run ends.
    HoldBackground,
    /// Wait until a background measurement holds at its gate (`true`), or
    /// the background has no job held or waiting (`false`), at most
    /// [`STEP_WATCHDOG`]: its thread runs in background mode, which a
    /// loaded machine starves for tens of seconds, so only the watchdog
    /// bounds the wait.
    Background(bool),
    /// Close the window.
    Close,
}

/// How long a scripted step waits for its chart while no scan pass runs.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a scripted step waits in all, scan passes included: a pass
/// runs in background mode, which a loaded machine starves for many
/// seconds, so this bound only catches a pass that never ends.
pub const STEP_WATCHDOG: Duration = Duration::from_secs(600);

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
/// `pick=<file>`, `wait=<file>`, `file=<path>`, `search=<text>`, `sleep=<ms>`,
/// `idle=<ms>`, `enter`, `prev-preset`, `next-preset`, `drawn`,
/// `back`, `retry`, `next`, `prev`, `close-measuring`, `quit-measuring`,
/// `quit-decoding`, `slow-load=<ms>`, `quit-loading`, `close-loading`,
/// `slow-measuring=<ms>`, `hold-background`, `background=<running|idle>`
/// and `close`.
/// Selection steps run on the select screen (`prev-preset` and
/// `next-preset` press F11 and F12; `close-measuring` and
/// `quit-measuring` close the window or press the quit key once the next
/// load measures a chart's loudness, `quit-decoding` once it has decoded
/// its first sound file; `slow-load` holds the next load that long, and
/// `quit-loading` and `close-loading` press the quit key or close the
/// window halfway through it; `slow-measuring` holds every loudness
/// measurement from then on that long before its first block, a load's and
/// the background's; `hold-background` holds every background
/// measurement from then on at its gate until a load waits for it or the
/// run ends, and `background` waits until one holds there, or until the
/// background has no job at all),
/// `drawn` (wait until the result's page is drawn), `back`, `retry`, `next`
/// and `prev` on a result; a step out of place, or a chart not
/// listed within [`STEP_TIMEOUT`] of no scan pass running (or
/// [`STEP_WATCHDOG`] in all), fails the run. A scenario
/// ([`scenario`]) compiles into the same steps, each with the scenario step
/// it comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectScript {
    steps: std::collections::VecDeque<(SelectStep, Option<scenario::Origin>)>,
    /// Since when the front step waits, moved on by the time a scan pass
    /// ran while it waited for a listing ([`scanning`](Self::scanning)).
    since: Option<std::time::Instant>,
    /// Since when the front step waits in all, for [`STEP_WATCHDOG`].
    began: Option<std::time::Instant>,
    /// When a step waiting for a listing last looked at the scan.
    looked: Option<std::time::Instant>,
    /// The input thread's wakes when an `idle` step began.
    pub idle_from: Option<u64>,
    /// The next load's loudness measurement closes the window
    /// (`close-measuring`).
    pub close_measuring: bool,
    /// The next load's loudness measurement presses the quit key
    /// (`quit-measuring`).
    pub quit_measuring: bool,
    /// The next load's first decoded file presses the quit key
    /// (`quit-decoding`).
    pub quit_decoding: bool,
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
                Some(("file", f)) => Ok(SelectStep::File(PathBuf::from(f))),
                Some(("folder", f)) => Ok(SelectStep::Folder(f.to_owned())),
                Some(("row", f)) => Ok(SelectStep::Row(f.to_owned())),
                Some(("search", t)) => Ok(SelectStep::Search(t.to_owned())),
                Some(("sleep", ms)) => ms.parse().map(SelectStep::Sleep).map_err(|_| {
                    format!("{s} is not pick=, wait=, search=, sleep=, idle=, slow-load=, slow-measuring=, slow-decoding=, slow-preparing=, slow-specializing=, slow-chart-load=, press-loading=, hold-background, background=running, background=idle, enter, prev-preset, next-preset, drawn, back, retry, next, prev, close-measuring, quit-measuring, quit-decoding, quit-loading, close-loading, [result-image-|result-layout-]<quit|close|input-failure|escalate|escalate-input-failure>-specializing or close")
                }),
                Some(("idle", ms)) => ms
                    .parse()
                    .map(SelectStep::Idle)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-load", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowLoad)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-measuring", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowMeasuring)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-decoding", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowDecoding)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-specializing", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowSpecializing)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-preparing", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowPreparing)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("slow-chart-load", ms)) => ms
                    .parse()
                    .map(SelectStep::SlowChartLoad)
                    .map_err(|_| format!("{s} takes milliseconds")),
                Some(("press-loading", phase)) => LoadPhase::named(phase)
                    .map(SelectStep::PressLoading)
                    .ok_or_else(|| format!("{s} takes decoding, measuring, preparing or chart-load")),
                Some(("background", "running")) => Ok(SelectStep::Background(true)),
                Some(("background", "idle")) => Ok(SelectStep::Background(false)),
                None if s == "enter" => Ok(SelectStep::Enter),
                None if s == "all" => Ok(SelectStep::AllCharts),
                None if s == "up" => Ok(SelectStep::Up),
                None if s == "prev-preset" => Ok(SelectStep::PrevPreset),
                None if s == "next-preset" => Ok(SelectStep::NextPreset),
                None if s == "drawn" => Ok(SelectStep::Drawn),
                None if s == "back" => Ok(SelectStep::Back),
                None if s == "retry" => Ok(SelectStep::Retry),
                None if s == "next" => Ok(SelectStep::Next),
                None if s == "prev" => Ok(SelectStep::Prev),
                None if s == "close-measuring" => Ok(SelectStep::CloseMeasuring),
                None if s == "quit-measuring" => Ok(SelectStep::QuitMeasuring),
                None if s == "quit-decoding" => Ok(SelectStep::QuitDecoding),
                None if s == "quit-loading" => Ok(SelectStep::QuitLoading),
                None if s == "close-loading" => Ok(SelectStep::CloseLoading),
                None if let Some(mode) = s
                    .strip_suffix("-specializing")
                    .and_then(Specializing::staged) =>
                {
                    Ok(SelectStep::Specializing(mode.0, mode.1))
                }
                None if s == "hold-background" => Ok(SelectStep::HoldBackground),
                None if s == "close" => Ok(SelectStep::Close),
                _ => Err(format!(
                    "{s} is not pick=, wait=, search=, sleep=, idle=, slow-load=, slow-measuring=, slow-decoding=, slow-preparing=, slow-specializing=, slow-chart-load=, press-loading=, hold-background, background=running, background=idle, enter, prev-preset, next-preset, drawn, back, retry, next, prev, close-measuring, quit-measuring, quit-decoding, quit-loading, close-loading, [result-image-|result-layout-]<quit|close|input-failure|escalate|escalate-input-failure>-specializing or close"
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
        self.began = None;
        self.looked = None;
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
        let why = if self.watchdog_expired() {
            format!(
                "{step} waited {} s, scan passes included",
                STEP_WATCHDOG.as_secs()
            )
        } else {
            format!("{step} waited {} s", STEP_TIMEOUT.as_secs())
        };
        self.fail(why);
        false
    }

    /// The front step made progress: its wait starts over, so a step of
    /// many moves times out only once it stalls.
    pub fn progressed(&mut self) {
        self.since = None;
        self.began = None;
    }

    /// The front step waits for a listing and looks at the scan: while a
    /// pass runs (`running`), the time since its last look does not count
    /// toward [`STEP_TIMEOUT`], since the pass may yet list the chart
    /// however long a loaded machine takes to run it. Only
    /// [`STEP_WATCHDOG`] bounds that.
    pub fn scanning(&mut self, running: bool) {
        let now = std::time::Instant::now();
        if running && let (Some(since), Some(looked)) = (self.since, self.looked) {
            self.since = Some((since + now.saturating_duration_since(looked)).min(now));
        }
        self.looked = Some(now);
    }

    /// Moves the front step's wait, and its last look at the scan, `by`
    /// into the past, as if that much time had passed (tests).
    #[cfg(test)]
    pub fn age(&mut self, by: Duration) {
        let back = |t: std::time::Instant| t.checked_sub(by).unwrap_or(t);
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        self.since = Some(back(since));
        let began = *self.began.get_or_insert(since);
        self.began = Some(back(began));
        self.looked = self.looked.map(back);
    }

    /// Whether the front step has waited [`STEP_TIMEOUT`] since it became
    /// the front.
    pub fn expired(&mut self) -> bool {
        self.expired_after(STEP_TIMEOUT)
    }

    /// Whether the front step has waited `bound` since it became the
    /// front, less the time a scan pass ran while it waited for a listing,
    /// or [`STEP_WATCHDOG`] in all.
    fn expired_after(&mut self, bound: Duration) -> bool {
        let since = *self.since.get_or_insert_with(std::time::Instant::now);
        since.elapsed() >= bound || self.watchdog_expired()
    }

    /// Whether the front step has waited [`STEP_WATCHDOG`] in all.
    pub fn watchdog_expired(&mut self) -> bool {
        let began = *self.began.get_or_insert_with(std::time::Instant::now);
        began.elapsed() >= STEP_WATCHDOG
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

/// A menu screen's frames, the select screen's or the play screen's
/// loading state's
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
/// source, and each speed key of `speed` pressed on the first keyboard at
/// its time on the script's timeline and released 50 ms later, as packets
/// no source records.
///
/// # Errors
///
/// Why the log cannot be read, or that it holds no played attempt (a
/// capture cancelled before playback).
pub fn script(log: &Path, speed: &[(u32, TimeUs)]) -> anyhow::Result<ScriptedInput> {
    let limits = DecodeLimits::default();
    let bytes = read_bounded(log, limits.max_compressed)?;
    let rec = replay::decode(&bytes, &limits).with_context(|| log.display().to_string())?;
    if !rec.played() {
        return Err(replay::verify::VerifyError::NoPlayedAttempt)
            .with_context(|| log.display().to_string());
    }
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
    let keyboard = kinds.iter().position(|&k| k == DeviceKind::Keyboard);
    let script = match keyboard {
        Some(i) => script.with_packets(speed.iter().flat_map(|&(code, at)| {
            let packet = |value| input::Packet {
                device: input::DeviceId(i as u64),
                control: eval::ControlId(code),
                value,
                background: false,
            };
            [
                input::ScriptItem {
                    at,
                    action: input::Action::Packet(packet(1)),
                },
                input::ScriptItem {
                    at: TimeUs(at.0 + 50_000),
                    action: input::Action::Packet(packet(0)),
                },
            ]
        })),
        None if speed.is_empty() => script,
        None => anyhow::bail!("--dev-speed needs a keyboard in the input log"),
    };
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

/// Settings keys by name, comma-separated: `F3` to `F8`.
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
                .ok_or_else(|| format!("{name} is not a settings key (F3 to F8)"))
        })
        .collect()
}

/// A scripted play's input (`--dev-scripted-input`): the log, the speed
/// keys pressed during its plays ([`Options::speed_keys`]) and the keys
/// held while it loads ([`Options::loading_keys`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptInput {
    /// The input log.
    pub log: PathBuf,
    /// Speed keys by scan code, each pressed at its time on the script.
    pub speed: Vec<(u32, TimeUs)>,
    /// Keys held while loading, with their press and release times.
    pub loading: Vec<(u32, TimeUs, TimeUs)>,
    /// How long a rearm waits at the least after the release
    /// ([`Options::rearm_delay`]).
    pub rearm_delay: TimeUs,
}

/// The keys of `list` held while loading, comma-separated as
/// `<scan code in hex>@<press ms>-<release ms>` (`10-1500`: Q from the
/// start for 1.5 s).
///
/// # Errors
///
/// An item not of that form, or released before it is pressed.
pub fn loading_keys(list: &str) -> Result<Vec<(u32, TimeUs, TimeUs)>, String> {
    list.split(',')
        .map(|item| {
            let bad = || format!("{item} is not <scan code>@<press ms>-<release ms>");
            let (code, times) = item.split_once('@').ok_or_else(bad)?;
            let (down, up) = times.split_once('-').ok_or_else(bad)?;
            let code = u32::from_str_radix(code, 16).map_err(|_| bad())?;
            let ms = |s: &str| {
                s.parse::<i64>()
                    .map(|m| TimeUs(m * 1_000))
                    .map_err(|_| bad())
            };
            let (down, up) = (ms(down)?, ms(up)?);
            if up <= down {
                return Err(bad());
            }
            Ok((code, down, up))
        })
        .collect()
}

/// The keys held while a scripted play loads ([`loading_keys`]), as a
/// script on the log's first keyboard, which it connects: its times are
/// after loading starts. `None` without keys.
///
/// # Errors
///
/// Why the log cannot be read, or that it has no keyboard.
pub fn loading_script(
    log: &Path,
    keys: &[(u32, TimeUs, TimeUs)],
) -> anyhow::Result<Option<ScriptedInput>> {
    if keys.is_empty() {
        return Ok(None);
    }
    let limits = DecodeLimits::default();
    let bytes = read_bounded(log, limits.max_compressed)?;
    let rec = replay::decode(&bytes, &limits).with_context(|| log.display().to_string())?;
    let Some(i) = rec
        .devices
        .sources
        .iter()
        .position(|s| s.kind == replay::DeviceKind::Keyboard)
    else {
        anyhow::bail!("--dev-loading needs a keyboard in the input log");
    };
    let device = input::DeviceId(i as u64);
    let packet = |code, value| {
        input::Action::Packet(input::Packet {
            device,
            control: eval::ControlId(code),
            value,
            background: false,
        })
    };
    let mut items = vec![input::ScriptItem {
        at: TimeUs(0),
        action: input::Action::Connect(device, DeviceKind::Keyboard),
    }];
    for &(code, down, up) in keys {
        items.push(input::ScriptItem {
            at: down,
            action: packet(code, 1),
        });
        items.push(input::ScriptItem {
            at: up,
            action: packet(code, 0),
        });
    }
    Ok(Some(ScriptedInput::new(items)))
}

/// The speed keys of `list`, comma-separated: `F1` and `F2`.
///
/// # Errors
///
/// A name that is not F1 or F2.
pub fn speed_keys(list: &str) -> Result<Vec<u32>, String> {
    list.split(',')
        .map(|name| match name {
            "F1" => Ok(0x3B),
            "F2" => Ok(0x3C),
            _ => Err(format!("{name} is not a speed key (F1 or F2)")),
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
/// frame, `loading.png` at the first loading state frame,
/// `loading-skin-<lifetime>.png` at each loading lifetime's first frame the
/// skin drew, `ready.png` at
/// the first frame of the lead-in, `play.png` at the first frame at or
/// after `play_at` (and each play's own as `play-<generation>.png`),
/// and `end.png` at the first end screen frame.
pub struct FrameDump {
    dir: PathBuf,
    play_at: TimeUs,
    ready_state: bool,
    play: bool,
    end: bool,
    select: bool,
    loading: bool,
    /// The loading lifetime whose `loading-skin-<lifetime>.png` was written
    /// last.
    skin_loading: Option<u64>,
    /// The generation of the play whose `play-<generation>.png` was
    /// written last.
    numbered: Option<u64>,
}

impl FrameDump {
    /// Frames into `dir`, the play frame at chart time `play_at`.
    pub const fn new(dir: PathBuf, play_at: TimeUs) -> Self {
        Self {
            dir,
            play_at,
            ready_state: false,
            play: false,
            end: false,
            select: false,
            loading: false,
            skin_loading: None,
            numbered: None,
        }
    }

    /// Writes the screen's last frame, read back and encoded once, as each
    /// of `names`, in order.
    fn save(&self, screen: &dyn Screen, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let Some((w, h, pixels)) = screen.read_back() else {
            for name in names {
                eprintln!(
                    "dev: {}: the screen has nothing to read back",
                    self.dir.join(name).display()
                );
            }
            return;
        };
        let encoded = png(w, h, &pixels);
        for name in names {
            let path = self.dir.join(name);
            let written = fs::create_dir_all(&self.dir).and_then(|()| fs::write(&path, &encoded));
            match written {
                Ok(()) => println!("dev: frame {}", path.display()),
                Err(e) => eprintln!("dev: {}: {e}", path.display()),
            }
        }
    }
}

impl FrameSink for FrameDump {
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>) {
        if !drawn.timing.presented {
            return;
        }
        let mut names = Vec::new();
        // Each play's own frame at `play_at`, by its generation.
        if let (Some(t), Some(g)) = (drawn.play, drawn.generation)
            && t >= self.play_at
            && self.numbered != Some(g)
        {
            self.numbered = Some(g);
            names.push(format!("play-{g}.png"));
        }
        if let Some(l) = drawn.skin_loading
            && self.skin_loading != Some(l)
        {
            self.skin_loading = Some(l);
            names.push(format!("loading-skin-{l}.png"));
        }
        match drawn.play {
            Some(t) if t.0 < 0 && !self.ready_state => {
                self.ready_state = true;
                names.push("ready.png".to_owned());
            }
            Some(t) if t >= self.play_at && !self.play => {
                self.play = true;
                names.push("play.png".to_owned());
            }
            None if drawn.loading => {
                if !self.loading {
                    self.loading = true;
                    names.push("loading.png".to_owned());
                }
            }
            None if drawn.menu => {
                if !self.select {
                    self.select = true;
                    names.push("select.png".to_owned());
                }
            }
            None if !self.end => {
                self.end = true;
                names.push("end.png".to_owned());
            }
            _ => {}
        }
        self.save(screen, &names);
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

/// The result's next uploads hold on the render thread while set (tests
/// and development builds: an upload's wait and its withdrawal).
static RESULT_UPLOAD_HELD: AtomicBool = AtomicBool::new(false);
/// Set while an upload holds.
static RESULT_UPLOAD_WAITING: AtomicBool = AtomicBool::new(false);

/// Holds the result's uploads on the render thread from now on (`true`),
/// or lets the one holding go (`false`) (tests).
#[cfg(test)]
pub fn hold_result_uploads(on: bool) {
    RESULT_UPLOAD_HELD.store(on, Ordering::SeqCst);
}

/// Whether a result upload holds now (tests).
#[cfg(test)]
pub fn result_upload_holding() -> bool {
    RESULT_UPLOAD_WAITING.load(Ordering::SeqCst)
}

/// Holds the result's upload on the render thread while
/// [`hold_result_uploads`] says so, before anything of it is staged.
pub fn hold_result_upload() {
    if !RESULT_UPLOAD_HELD.load(Ordering::SeqCst) {
        return;
    }
    RESULT_UPLOAD_WAITING.store(true, Ordering::SeqCst);
    while RESULT_UPLOAD_HELD.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    RESULT_UPLOAD_WAITING.store(false, Ordering::SeqCst);
}

/// The result's stagings on the render thread (each selection, upload and
/// view staging of the result kind) and the texture arrays they uploaded,
/// so far (development builds: the S10-09i design's C6 and C7).
static RESULT_STAGINGS: AtomicU64 = AtomicU64::new(0);
static RESULT_UPLOADS: AtomicU64 = AtomicU64::new(0);

/// One staging of the result kind on the render thread, which uploaded
/// `uploads` texture arrays.
pub fn result_staged(uploads: u64) {
    RESULT_STAGINGS.fetch_add(1, Ordering::SeqCst);
    RESULT_UPLOADS.fetch_add(uploads, Ordering::SeqCst);
}

/// Prints the result's stagings and uploads so far, at `at` in the flow.
pub fn print_result_work(at: &str) {
    println!(
        "dev: result work at {at}: stagings {} uploads {}",
        RESULT_STAGINGS.load(Ordering::SeqCst),
        RESULT_UPLOADS.load(Ordering::SeqCst)
    );
}

/// The environment variable of the result's injected refusals (tests of
/// the real flow): a comma list of `select<N>`, the N-th selection or
/// upload of the result kind refused, `stage<N>`, the N-th view staging
/// refused, and `text<N>`, the N-th view's text refused on a healthy
/// device (the run's fallback), each counted from 1 on the render thread.
pub const RESULT_FAULTS_ENV: &str = "TAPESTRY_DEV_RESULT_FAULTS";

/// The result's selections and view stagings asked for so far.
static RESULT_SELECTS_SEEN: AtomicU64 = AtomicU64::new(0);
static RESULT_STAGES_SEEN: AtomicU64 = AtomicU64::new(0);
static RESULT_TEXTS_SEEN: AtomicU64 = AtomicU64::new(0);

/// Whether [`RESULT_FAULTS_ENV`] names `prefix` with `n`.
fn result_fault(prefix: &str, n: u64) -> bool {
    std::env::var(RESULT_FAULTS_ENV).is_ok_and(|list| {
        list.split(',')
            .filter_map(|f| f.trim().strip_prefix(prefix))
            .any(|k| k.parse() == Ok(n))
    })
}

/// Whether this selection or upload of the result kind is refused.
pub fn refuse_result_select() -> bool {
    result_fault(
        "select",
        RESULT_SELECTS_SEEN.fetch_add(1, Ordering::SeqCst) + 1,
    )
}

/// The frame under the result's barrier refused, counted from 1: `pinned<N>`
/// in [`RESULT_FAULTS_ENV`], read once as the render thread starts; 0 none.
static PINNED_FAULT: AtomicU64 = AtomicU64::new(0);
static PINNED_SEEN: AtomicU64 = AtomicU64::new(0);

/// Reads [`RESULT_FAULTS_ENV`]'s `pinned<N>` before any frame, so a frame
/// reads only atomics.
pub fn read_result_faults() {
    let n = std::env::var(RESULT_FAULTS_ENV)
        .ok()
        .and_then(|list| {
            list.split(',')
                .find_map(|f| f.trim().strip_prefix("pinned")?.parse().ok())
        })
        .unwrap_or(0);
    PINNED_FAULT.store(n, Ordering::SeqCst);
}

/// Whether this result frame, built under the barrier, is refused.
pub fn refuse_pinned_frame() -> bool {
    let n = PINNED_FAULT.load(Ordering::SeqCst);
    n != 0 && PINNED_SEEN.fetch_add(1, Ordering::SeqCst) + 1 == n
}

/// Holds the result skin's settles (the layout job's cancellation) while
/// set (tests); [`RESULT_SETTLE_WAITING`] says one holds.
#[cfg(test)]
static RESULT_SETTLE_HELD: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static RESULT_SETTLE_WAITING: AtomicBool = AtomicBool::new(false);

/// Holds the result skin's settles from now on (`true`), or lets the one
/// holding go (`false`) (tests).
#[cfg(test)]
pub fn hold_result_settles(on: bool) {
    RESULT_SETTLE_HELD.store(on, Ordering::SeqCst);
}

/// Whether a settle holds now (tests).
#[cfg(test)]
pub fn result_settle_holding() -> bool {
    RESULT_SETTLE_WAITING.load(Ordering::SeqCst)
}

/// Holds a settle of the result skin while [`hold_result_settles`] says so.
#[cfg(test)]
pub fn hold_result_settle() {
    if !RESULT_SETTLE_HELD.load(Ordering::SeqCst) {
        return;
    }
    RESULT_SETTLE_WAITING.store(true, Ordering::SeqCst);
    while RESULT_SETTLE_HELD.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    RESULT_SETTLE_WAITING.store(false, Ordering::SeqCst);
}

/// Whether this view's text preparation is refused.
pub fn refuse_result_text() -> bool {
    result_fault("text", RESULT_TEXTS_SEEN.fetch_add(1, Ordering::SeqCst) + 1)
}

/// Whether this view staging of the result kind is refused.
pub fn refuse_result_staging() -> bool {
    result_fault(
        "stage",
        RESULT_STAGES_SEEN.fetch_add(1, Ordering::SeqCst) + 1,
    )
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
