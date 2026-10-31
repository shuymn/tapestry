//! The game executable.
//!
//! `tapestry play <chart.bms>` loads the settings and the chart, opens the
//! audio device and loads the sound bank, starts the input thread, then opens a
//! borderless fullscreen window whose render thread prepares the GPU resources,
//! and plays the chart until the player quits. The data root is
//! `%LOCALAPPDATA%\tapestry\` (design.md "Settings Files"); the commands that
//! do work log their loading and outcome to `logs\tapestry.log` in it
//! ([`log_file`]).
//!
//! `tapestry play --auto <chart.bms>` plays the chart by itself on the same
//! window, audio output and renderer (design.md "Autoplay"): the input that
//! hits every note is generated before the window opens and fed by an
//! input thread without Raw Input, its key sounds are scheduled with the
//! BGM, and nothing it plays is saved.

#[cfg(not(windows))]
compile_error!("player is Windows-only");

#[cfg(feature = "dev-tools")]
mod alloc_count;
mod args;
mod audio_side;
mod bests;
mod controls;
#[cfg(feature = "dev-tools")]
mod dev;
mod end_screen;
mod input_side;
mod keys;
mod log_file;
mod loudness;
mod measure;
mod menu_screen;
mod play;
mod power;
mod practice_page;
#[cfg(feature = "dev-tools")]
mod presentmon;
#[cfg(feature = "dev-tools")]
mod recording;
mod render_side;
mod result_input;
mod select;
mod selection;
mod shelf;
mod tuning;
mod window;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Context as _;
use audio::{AudioEngine, BankLimits, MAX_VOICES, load_bank_at};
use bms_model::{BgmEvent, TimeUs};
use eval::ControlId;
use game::{
    AutoKeySounds, DataRoot, DeviceSettings, Loaded, Parts, SessionConfig, Settings, Tuning,
    save_settings,
};
use input::{
    AutoplayInput, Capture, ClockedThread, InputThread, Phase, QpcClock, SourceConfig, SourceSet,
    Waker,
};
use render::{AdapterChoice, Renderer, Target};
use replay::{BuildProvenance, ReceiveMethod};

use crate::args::Command;
use crate::controls::Controls;
use crate::end_screen::{Hints, LoadTimes};
use crate::input_side::Mode;
use crate::measure::{AudioInfo, Measure, SOUND_QUEUE, STEP_QUEUE, StepSample};
use crate::menu_screen::MenuScreen;
use crate::play::PlayerRig;
use crate::render_side::{MakeScreen, NoFrames, RenderEnd, Screen, View};
use crate::select::MenuQueue;
use crate::tuning::{Delivered, LiveTuning, SettingsFiles};

/// The countdown before chart time 0 (design.md "Play Lifecycle").
const LEAD_IN: TimeUs = TimeUs(1_000_000);
/// Effects the render queue holds.
const EFFECTS: usize = 1 << 14;
/// Scheduled sound starts the audio engine keeps between two drains (one
/// engine step), which starts at most this many: none is lost.
const SCHEDULE_LOG: usize = audio::MAX_PENDING_STARTS;

fn main() -> ExitCode {
    // The clock reads 0 at the start of the process: load times count from
    // here unless the command sets something up first.
    let clock = QpcClock::start();
    // A usage error is found before anything is opened, and exits 2.
    let command = match args::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    // Only a command that does work logs, from here on; without a data
    // root, the command reports that itself.
    if let Some(name) = logged(&command)
        && let Ok(root) = data_root()
    {
        log_file::start(&root.path().join("logs"), name);
    }
    match command {
        Command::Version => {
            println!("tapestry {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        #[cfg(feature = "dev-tools")]
        Command::Keys(k) => report(keys::run(clock, k.script)),
        #[cfg(not(feature = "dev-tools"))]
        Command::Keys(args::Keys {}) => report(keys::run(clock)),
        Command::Play(p) => play_command(p, clock),
        Command::Select(s) => report(select(s, clock)),
        #[cfg(feature = "dev-tools")]
        Command::DevToolsProbe => dev::probe(),
        #[cfg(feature = "dev-tools")]
        Command::Measure { chart, perfect } => report(measure_play(&chart, perfect, clock)),
        #[cfg(feature = "dev-tools")]
        Command::Scenario(s) => report(dev::scenario::run(s, clock)),
    }
}

/// The name the log gives `command`, or `None` for one that writes no log:
/// `--version` and the probe only print.
fn logged(command: &Command) -> Option<&'static str> {
    match command {
        Command::Version => None,
        #[cfg(feature = "dev-tools")]
        Command::DevToolsProbe => None,
        Command::Keys(_) => Some("keys"),
        Command::Play(_) => Some("play"),
        Command::Select(_) => Some("select"),
        #[cfg(feature = "dev-tools")]
        Command::Measure { .. } => Some("measure"),
        #[cfg(feature = "dev-tools")]
        Command::Scenario(_) => Some("scenario"),
    }
}

/// `tapestry play [--auto] <chart>`, with its development options.
fn play_command(p: args::Play, clock: QpcClock) -> ExitCode {
    let chart = p.chart;
    let input = p.input;
    #[cfg(feature = "dev-tools")]
    let mut dev_options = p.dev;
    // A mid-chart start (`--dev-start`).
    #[cfg(feature = "dev-tools")]
    let start = dev_options.start;
    #[cfg(not(feature = "dev-tools"))]
    let start = None;
    #[cfg(feature = "dev-tools")]
    if dev_options.headless || matches!(input, PlayInput::Script(_)) {
        let auto = u32::from(dev_options.auto_retry);
        let frames = dev_options.frames.take();
        return report(headless(&chart, input, start, frames, auto, clock, |rig| {
            if dev_options.overlay {
                rig.controls.toggle_overlay();
            }
            if let Some(bound) = dev_options.render_timeout {
                rig.render.set_timeouts(bound, bound);
            }
            rig.adjust_in_play = dev_options.adjust_in_play;
            rig.adjust_at = TimeUs(dev_options.adjust_at_ms.saturating_mul(1_000));
            rig.adjust_on_end = dev_options.adjust_on_end;
            rig.result_script = dev_options.result;
        }));
    }
    report(play(
        &chart,
        input,
        start,
        clock,
        #[cfg(feature = "dev-tools")]
        false,
        |_rig| {
            #[cfg(feature = "dev-tools")]
            {
                if dev_options.auto_retry {
                    _rig.auto_retries = 1;
                    _rig.quit_after_end = Some(std::time::Duration::from_millis(300));
                }
                if dev_options.overlay {
                    _rig.controls.toggle_overlay();
                }
                if let Some(bound) = dev_options.render_timeout {
                    _rig.render.set_timeouts(bound, bound);
                }
                _rig.adjust_in_play = dev_options.adjust_in_play;
                _rig.adjust_at = TimeUs(dev_options.adjust_at_ms.saturating_mul(1_000));
                _rig.adjust_on_end = dev_options.adjust_on_end;
                _rig.result_script = dev_options.result;
            }
        },
    ))
}

/// `tapestry measure play [--perfect] <chart>` (dev-tools): one play
/// under a PresentMon capture, its counts added to each saved play's
/// `.measure.toml`; the run quits after the end screen shows (retry and
/// quit still work during the play). The devices play it, or with
/// `perfect` generated input that hits every note.
#[cfg(feature = "dev-tools")]
fn measure_play(chart: &Path, perfect: bool, clock: QpcClock) -> anyhow::Result<()> {
    let exe = presentmon::locate(std::env::var_os(presentmon::ENV), std::env::var_os("PATH"))?;
    let process = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .context("the game's own executable name is unknown")?;
    let dir = data_root()?.path().join("measure");
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let csv = dir.join(format!("{stamp}.presentmon.csv"));
    let log = dir.join(format!("{stamp}.presentmon.log"));
    println!(
        "measure: PresentMon {} into {}",
        exe.display(),
        csv.display()
    );
    let capture = presentmon::Capture::start(&exe, &process, &csv, &log)?;
    // The capture's start waits for PresentMon to settle: the first load
    // runs from after it.
    let load_start = bms_model::TimeSource::now(&clock);
    let plays = play::MeasuredPlays::default();
    let kept = Arc::clone(&plays);
    // One play: the run quits once its end screen has shown.
    let input = if perfect {
        PlayInput::Perfect
    } else {
        PlayInput::Devices
    };
    let ran = play(chart, input, None, clock, true, move |rig| {
        rig.measured_plays = Some(kept);
        rig.load_start = load_start;
        rig.quit_after_end = Some(std::time::Duration::from_millis(300));
    });
    let stopped = capture.stop();
    ran?;
    stopped?;
    let text = std::fs::read_to_string(&csv).with_context(|| csv.display().to_string())?;
    let (frames, error) = presentmon::parse(&text, &process, |t| clock.from_qpc(t).0);
    if let Some(e) = error {
        return Err(anyhow::Error::new(e).context(csv.display().to_string()));
    }
    let plays = plays
        .lock()
        .map_err(|_| anyhow::anyhow!("the measured plays were lost"))?;
    for (record, fig) in plays.iter() {
        let Some(period) = fig.refresh_period_ns else {
            eprintln!("measure: no refresh rate for {}", record.display());
            continue;
        };
        let counted = presentmon::count_play(&frames, fig, period);
        println!(
            "measure: {}: presentmon {:?}, {:?} presents, {:?} never displayed",
            record.display(),
            counted.count,
            counted.presents,
            counted.never_displayed
        );
        let path = measure::measure_path(record);
        let mut toml = std::fs::read_to_string(&path).unwrap_or_default();
        toml.push_str(&presentmon::to_toml(&counted, fig, period));
        game::write_atomic(&path, toml.as_bytes()).with_context(|| path.display().to_string())?;
    }
    Ok(())
}

/// A run's outcome as its exit code: a failure prints [`diagnostic`], logs
/// it, and exits 1. The log's exit line is written here, the one exit of
/// every command that logs.
fn report(result: anyhow::Result<()>) -> ExitCode {
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            let line = diagnostic(&e);
            eprintln!("{line}");
            log::error!("{line}");
            1
        }
    };
    log_file::finish(code);
    ExitCode::from(code)
}

/// A failure and each of its causes, once, on one line.
fn diagnostic(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

/// `%LOCALAPPDATA%\tapestry\`.
fn data_root() -> anyhow::Result<DataRoot> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .context("LOCALAPPDATA is not set, so the data folder cannot be found")?;
    Ok(DataRoot::new(PathBuf::from(base).join("tapestry")))
}

/// The settings, writing `device.toml` with the default key config on the
/// first start.
fn settings(root: &DataRoot) -> anyhow::Result<Settings> {
    let device = root.device_settings();
    if !device.exists() {
        save_settings(&device, &DeviceSettings::default())
            .with_context(|| device.display().to_string())?;
    }
    Settings::load(root).with_context(|| root.path().display().to_string())
}

/// How a session's input is received, chosen once from the command line.
/// It decides together the receive method, whether plays are saved and
/// the session's overrides (design.md "Autoplay").
#[derive(Debug)]
enum PlayInput {
    /// Raw Input from the devices, with the configured keys; plays are
    /// saved.
    Devices,
    /// Generated input that hits every note, with the built-in keys and an
    /// input offset of 0, its key sounds scheduled with the BGM; nothing
    /// is saved.
    Autoplay,
    /// An input log replayed as a script, with the configured keys; plays
    /// are saved (dev-tools).
    #[cfg(feature = "dev-tools")]
    Script(PathBuf),
    /// Autoplay's generated input, keys and planning, with key sounds sent
    /// from judgments; plays are saved: the perfect measurement
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    Perfect,
}

/// Settings, the chart through ingestion and the transform, and the
/// session's fixed choices, as `input` decides them.
fn load(chart: &Path, input: &PlayInput) -> anyhow::Result<Loaded> {
    let root = data_root()?;
    let settings = settings(&root)?;
    let (receive, parts, autoplay) = match input {
        PlayInput::Devices => (
            ReceiveMethod::RawInput,
            Parts::standard(&root, &settings.player),
            None,
        ),
        PlayInput::Autoplay => (
            ReceiveMethod::Scripted,
            Parts::unsaved(&settings.player),
            Some(AutoKeySounds::Scheduled),
        ),
        #[cfg(feature = "dev-tools")]
        PlayInput::Script(_) => (
            ReceiveMethod::Scripted,
            Parts::standard(&root, &settings.player),
            None,
        ),
        #[cfg(feature = "dev-tools")]
        PlayInput::Perfect => (
            ReceiveMethod::Scripted,
            Parts::standard(&root, &settings.player),
            Some(AutoKeySounds::Judged),
        ),
    };
    let config = session_config(receive, autoplay);
    let loaded = Loaded::load(root, settings, chart, config, parts)
        .with_context(|| chart.display().to_string())?;
    let warnings = loaded.warnings();
    log::info!("loaded {} ({} warnings)", chart.display(), warnings.len());
    log_file::warnings("chart", warnings.iter());
    log_file::missing_sounds(
        loaded
            .chart()
            .sounds
            .iter()
            .filter(|s| s.path.is_none())
            .map(|s| s.name.as_str()),
    );
    Ok(loaded)
}

/// The session's fixed choices: how input is received, and autoplay's.
fn session_config(receive: ReceiveMethod, autoplay: Option<AutoKeySounds>) -> SessionConfig {
    // `#RANDOM` is resolved with a seed that differs per run; it is
    // recorded.
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    SessionConfig {
        lead_in: LEAD_IN,
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: EFFECTS,
        seed,
        // Plays run at 1× until the practice screen chooses a speed.
        speed: bms_model::Speed::NORMAL,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        provenance: if cfg!(feature = "dev-tools") {
            BuildProvenance::Development
        } else {
            BuildProvenance::Production
        },
        receive,
        autoplay,
    }
}

/// The source set: up to 16 devices kept, 8 taking part in a play.
fn source_config() -> SourceConfig {
    SourceConfig {
        max_devices: 16,
        max_sources: 8,
        max_held: 32,
        queue_capacity: 4_096,
        max_arrivals: 16,
    }
}

fn sources() -> SourceSet {
    SourceSet::new(source_config()).expect("8 sources are within the limit")
}

/// Has the configured controller's axes bound to lanes filtered in the mode
/// `device.toml` sets for them, when it sets one, so what is recorded and
/// what the menu sees are the reports their conversion uses.
fn axis_mode(sources: &mut SourceSet, device: &game::DeviceSettings) {
    let (Some(product), Some(relative)) = (device.controller_device, device.scratch.relative)
    else {
        return;
    };
    let axes = device
        .controller
        .lanes
        .iter()
        .map(|k| ControlId(k.control))
        .filter(|&c| input::hid::is_axis(c))
        .collect();
    sources.set_axis_mode(product.into(), axes, relative);
}

/// A loaded session and what its input side starts with.
struct Prepared {
    loaded: Loaded,
    /// How the input thread feeds each play.
    mode: Mode,
    /// The source set the input thread keeps.
    sources: SourceSet,
    /// The audio schedule, when it is not the chart's BGM (autoplay).
    schedule: Option<Vec<BgmEvent>>,
    /// The controls every keyboard records, when they are not the key
    /// config's (autoplay).
    keyboard: Option<Vec<ControlId>>,
}

/// Loads the session `input` chooses and makes what its input side
/// starts with. Autoplay's input and schedule are made here, before
/// anything opens; a chart over the recording budget is refused here. With
/// `start`, every play is a mid-chart start from that time of the chart's
/// 1× time to its end (design.md "Mid-Chart Start and Section Repeat"),
/// refused here, before any thread starts, when the practice refuses it.
fn prepare(chart: &Path, input: &PlayInput, start: Option<TimeUs>) -> anyhow::Result<Prepared> {
    let mut loaded = load(chart, input)?;
    if let Some(start) = start {
        let practice = game::mid_chart_start(
            loaded.chart(),
            loaded.config().speed,
            loaded.preset(),
            start,
        );
        loaded
            .set_practice(Some(practice))
            .with_context(|| format!("a mid-chart start at {} ms", start.0 / 1_000))?;
    }
    match input {
        PlayInput::Devices => Ok(Prepared {
            loaded,
            mode: Mode::Clock,
            sources: sources(),
            schedule: None,
            keyboard: None,
        }),
        PlayInput::Autoplay => generated(loaded, chart),
        #[cfg(feature = "dev-tools")]
        PlayInput::Perfect => {
            println!("input: generated, every note hit on time (perfect measurement)");
            generated(loaded, chart)
        }
        #[cfg(feature = "dev-tools")]
        PlayInput::Script(log) => {
            let script = dev::script(log)?;
            let script = input_side::Script {
                log: script.clone(),
                current: script,
                sources: source_config(),
            };
            Ok(Prepared {
                loaded,
                mode: Mode::Script(Box::new(script)),
                sources: sources(),
                schedule: None,
                keyboard: None,
            })
        }
    }
}

/// The session of generated input (autoplay, the perfect measurement): its
/// input, audio schedule and recorded controls, made before anything opens.
fn generated(loaded: Loaded, chart: &Path) -> anyhow::Result<Prepared> {
    let made = loaded
        .autoplay()
        .with_context(|| chart.display().to_string())?;
    let config = SourceConfig {
        queue_capacity: made.queue_capacity(),
        ..source_config()
    };
    Ok(Prepared {
        loaded,
        mode: Mode::Autoplay(AutoplayInput::new(made.events)),
        sources: SourceSet::new(config).expect("8 sources are within the limit"),
        schedule: Some(made.schedule),
        keyboard: Some(made.recorded),
    })
}

/// Loads the bank at the output's rate and hands the engine, playing
/// `schedule` or else the session's ([`Loaded::audio_schedule`]: a round's,
/// or the chart's BGM), to the audio thread, which keeps it silent until
/// the first play.
fn start_engine(
    loaded: &Loaded,
    schedule: Option<Vec<BgmEvent>>,
    audio: audio_side::AudioThread,
    opened: audio_side::Opened,
    probe: rtrb::Producer<game::AudioProbe>,
) -> anyhow::Result<audio_side::AudioLink> {
    let chart = loaded.chart();
    // At the speed the chart was loaded at, so the sounds keep its real time.
    let bank = load_bank_at(
        chart,
        opened.rate,
        loaded.config().speed,
        &BankLimits::default(),
    )
    .context("the sounds cannot be loaded")?;
    log_file::warnings("sounds", bank.warnings().iter().map(|w| format!("{w:?}")));
    let mut engine = AudioEngine::with_schedule(
        Arc::new(bank),
        schedule.unwrap_or_else(|| loaded.audio_schedule()),
        opened.rate,
        opened.period,
        MAX_VOICES,
        SCHEDULE_LOG,
    )
    .context("the audio engine cannot start")?;
    loaded.settings().device.apply_to(&mut engine);
    Ok(audio.start(engine, probe))
}

/// What restores the rounds of a practice set up at load: the engine's
/// seek source and the rounds' schedule, the one the engine was started
/// with; `None` for full plays.
fn restorer(loaded: &Loaded, rig: &PlayerRig<'_>) -> Option<game::Restorer> {
    loaded
        .section()
        .map(|_| game::Restorer::new(rig.audio.control.seek_source(), loaded.audio_schedule()))
}

/// The measurement's queues: logic steps and key sounds to the render
/// thread's [`Measure`].
struct Probes {
    steps: (rtrb::Producer<StepSample>, rtrb::Consumer<StepSample>),
    sounds: (
        rtrb::Producer<game::AudioProbe>,
        rtrb::Consumer<game::AudioProbe>,
    ),
}

impl Probes {
    fn new() -> Self {
        Self {
            steps: rtrb::RingBuffer::new(STEP_QUEUE),
            sounds: rtrb::RingBuffer::new(SOUND_QUEUE),
        }
    }
}

/// The render thread's measurement of each play.
fn measure(
    clock: QpcClock,
    refresh_mhz: Option<u32>,
    opened: audio_side::Opened,
    steps: rtrb::Consumer<StepSample>,
    sounds: rtrb::Consumer<game::AudioProbe>,
    controls: &Controls,
) -> Measure {
    let audio = AudioInfo {
        period_us: i64::from(opened.period) * 1_000_000 / i64::from(opened.rate.max(1)),
        queued_periods: opened.queued_periods,
    };
    Measure::new(
        clock,
        refresh_mhz,
        Some(audio),
        steps,
        sounds,
        Arc::clone(controls.overlay()),
    )
}

/// Presents a play of `chart` can issue, with room to spare, at
/// `refresh_mhz` (1 kHz when unknown): the countdown, the chart to its last
/// long note end or BGM event, a tail of 15 s, a quarter more.
#[cfg(feature = "dev-tools")]
fn presents_of(chart: &bms_model::Chart, lead_in: TimeUs, refresh_mhz: Option<u32>) -> usize {
    let last = chart
        .lanes
        .iter()
        .flatten()
        .map(bms_model::Note::end)
        .chain(chart.bgm.iter().map(|b| b.time))
        .max()
        .unwrap_or(TimeUs(0));
    let us = u64::try_from(lead_in.0 + last.0 + 15_000_000).unwrap_or(0);
    let mhz = u64::from(refresh_mhz.unwrap_or(1_000_000));
    usize::try_from(us * mhz / 1_000_000_000 * 5 / 4).unwrap_or(usize::MAX)
}

/// The frames' inputs: hi-speed and the display offset as the settings
/// keys leave them.
fn view(controls: &Controls) -> View {
    View {
        tuning: Arc::clone(controls.tuning()),
        effects: EFFECTS,
    }
}

/// The controls, with the settings keys starting from the loaded values,
/// waking the calling thread: the game thread.
fn controls(s: &Settings, clock: QpcClock) -> Arc<Controls> {
    let tuning = LiveTuning::new(Tuning::of(s)).with_loaded_volume(s.device.master_volume);
    let controls = Controls::with_tuning(clock, Arc::new(tuning));
    controls.set_game(std::thread::current());
    Arc::new(controls)
}

/// The started threads' ends the rig holds.
struct Threads {
    input: input_side::InputLink,
    audio: audio_side::AudioLink,
    render: render_side::RenderLink,
    /// Where each logic step's measurement goes.
    steps: rtrb::Producer<StepSample>,
}

/// The game thread's rig around the started threads, for plays under
/// `root` with `settings`, by generated input when `autoplay`.
fn rig<'a>(
    root: &DataRoot,
    settings: &Settings,
    autoplay: bool,
    threads: Threads,
    controls: &'a Controls,
    clock: QpcClock,
) -> PlayerRig<'a> {
    let Threads {
        input,
        audio,
        render,
        steps,
    } = threads;
    let device = &settings.device;
    PlayerRig {
        input,
        audio,
        render,
        controls,
        clock,
        recorded: input_side::Recorded {
            keyboard: device.keyboard.recorded(),
            controller: device.controller.recorded(),
            controller_device: device.controller_device.map(Into::into),
            scratch: device.scratch,
        },
        gauge: settings.player.displayed_gauge,
        autoplay,
        input_offset_us: device.input_offset_us,
        files: SettingsFiles::new(root.clone(), settings.clone()),
        seen: Delivered::default(),
        end_lines: None,
        hints: Hints::new(device.keyboard.retry, device.keyboard.quit),
        result_input: result_input::ResultInput::new(Arc::new(MenuQueue::default()), None),
        viewed: None,
        evaluated: 0,
        #[cfg(feature = "dev-tools")]
        result_script: None,
        load_start: TimeUs(0),
        pending_retry: None,
        retry_from: None,
        times: LoadTimes::default(),
        countdown_at: None,
        origin: None,
        steps,
        steps_dropped: 0,
        figures: None,
        generation: 0,
        queue: game::QueueChoice::new(device.queued_periods),
        queue_most: None,
        queued_now: None,
        queue_line: None,
        #[cfg(feature = "dev-tools")]
        measured_plays: None,
        #[cfg(feature = "dev-tools")]
        scenario: None,
        lead_in: LEAD_IN,
        restorer: None,
        restore: None,
        restore_at: None,
        in_play: false,
        #[cfg(feature = "dev-tools")]
        quit_after_end: None,
        #[cfg(feature = "dev-tools")]
        end_shown: None,
        #[cfg(feature = "dev-tools")]
        auto_retries: 0,
        #[cfg(feature = "dev-tools")]
        adjust_in_play: Vec::new(),
        #[cfg(feature = "dev-tools")]
        adjust_at: TimeUs(0),
        #[cfg(feature = "dev-tools")]
        adjust_on_end: Vec::new(),
        selection: None,
    }
}

/// Opens the window, then starts the input thread, and checks that the
/// keyboards' Raw Input registration targets the input thread.
///
/// The order matters: winit registers keyboards to its own window when its
/// event loop is built, and `listen_device_events(Never)` then removes the
/// keyboard registration. Registration is per process, so the input
/// thread must register after both, or it would receive nothing.
///
/// # Errors
///
/// Why the window or the input thread cannot start, or a registration
/// that does not target the input thread.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one part of the startup the order above needs"
)]
fn open_window_and_input(
    controls: &Arc<Controls>,
    clock: QpcClock,
    sources: SourceSet,
    capture: Capture,
    waker: &Waker,
    wake: impl FnMut(&mut SourceSet, TimeUs) -> Phase + Send + 'static,
    keys: (Option<u32>, Option<u32>),
    monitor: Option<String>,
    menu: Option<Arc<MenuQueue>>,
) -> anyhow::Result<(window::WindowThread, window::Surface, InputThread)> {
    let (window_thread, surface) =
        window::open(Arc::clone(controls), keys.0, keys.1, monitor, menu)?;
    let input = InputThread::spawn(clock, sources, capture, waker, wake);
    let input = match input {
        Ok(i) => i,
        Err(e) => {
            window_thread.close();
            return Err(anyhow::Error::new(e).context("input"));
        }
    };
    if input::keyboard_target() != Some(input.receiver()) {
        window_thread.close();
        anyhow::bail!("input: keyboards are not registered to the input thread");
    }
    println!("input: keyboard Raw Input targets the input thread's window");
    Ok((window_thread, surface, input))
}

/// Opens the window, then starts the clocked input thread (autoplay),
/// which registers no Raw Input.
///
/// # Errors
///
/// Why the window or the input thread cannot start.
#[expect(
    clippy::too_many_arguments,
    reason = "each is one part of the startup, as in `open_window_and_input`"
)]
fn open_window_and_clocked(
    controls: &Arc<Controls>,
    clock: QpcClock,
    sources: SourceSet,
    waker: &Waker,
    wake: impl FnMut(&mut SourceSet, TimeUs) -> Phase + Send + 'static,
    keys: (Option<u32>, Option<u32>),
    monitor: Option<String>,
    menu: Option<Arc<MenuQueue>>,
) -> anyhow::Result<(window::WindowThread, window::Surface, ClockedThread)> {
    let (window_thread, surface) =
        window::open(Arc::clone(controls), keys.0, keys.1, monitor, menu)?;
    match ClockedThread::spawn(clock, sources, waker, wake) {
        Ok(input) => Ok((window_thread, surface, input)),
        Err(e) => {
            window_thread.close();
            Err(anyhow::Error::new(e).context("input"))
        }
    }
}

/// The menus' input, the same in both launch paths: the window's queue,
/// which the window fills and the select and result screens read (one
/// queue, returned for the window), and the configured controller's lane
/// controls as menu events, tapped from `sources`; no controller without a
/// configured one, and none for autoplay, which receives no device.
fn menu_input(
    device: &game::DeviceSettings,
    autoplay: bool,
    sources: &mut SourceSet,
) -> (Arc<MenuQueue>, result_input::ResultInput) {
    let bindings = select::menu_bindings(
        &device.controller,
        device.controller_device.map(Into::into),
        &device.scratch,
    )
    .filter(|_| !autoplay);
    let pad = bindings.map(|b| {
        let (tap, receiver) = input::menu_queue(b);
        sources.set_menu(tap);
        receiver
    });
    let menu = Arc::new(MenuQueue::default());
    (Arc::clone(&menu), result_input::ResultInput::new(menu, pad))
}

/// The game: WASAPI and a window, with the devices' input through Raw
/// Input, or generated input on a clocked input thread (autoplay). The
/// result screen takes the window's keys and the controller's menu events
/// as with selection.
fn play(
    chart: &Path,
    input: PlayInput,
    start: Option<TimeUs>,
    clock: QpcClock,
    #[cfg(feature = "dev-tools")] keep_presents: bool,
    tune: impl FnOnce(&mut PlayerRig<'_>),
) -> anyhow::Result<()> {
    #[cfg(feature = "dev-tools")]
    alloc_count::tag(alloc_count::Tag::Logic);
    let Prepared {
        mut loaded,
        mode,
        sources,
        schedule,
        keyboard,
    } = prepare(chart, &input, start)?;
    let controls = controls(loaded.settings(), clock);
    let waker = Waker::new().context("input")?;
    let _suspend = power::SuspendWatch::register(Arc::clone(&controls))?;
    let (audio, opened) = audio_side::spawn(
        move || -> Result<_, audio::WasapiError> {
            let out = audio::WasapiShared::open(clock)?;
            let most = out.grant().max_queue_periods;
            Ok((out, Some(most)))
        },
        |_| {},
    )?;
    let probes = Probes::new();
    let audio = start_engine(&loaded, schedule, audio, opened, probes.sounds.0)?;
    controls.set_latch(sources.gate().handle());
    controls.set_waker(waker.clone());
    let raw_input = matches!(mode, Mode::Clock);
    let (mut hook, input) = input_side::link(Arc::clone(&controls), mode, waker.clone());
    let device = &loaded.settings().device;
    let mut sources = sources;
    if raw_input {
        axis_mode(&mut sources, device);
    }
    let (menu, menus) = menu_input(device, !raw_input, &mut sources);
    let keys = (device.keyboard.retry, device.keyboard.quit);
    let monitor = device.monitor.clone();
    let wake = move |s: &mut SourceSet, now| {
        #[cfg(feature = "dev-tools")]
        alloc_count::tag(alloc_count::Tag::Input);
        hook.wake(s, now)
    };
    let started = if raw_input {
        open_window_and_input(
            &controls,
            clock,
            sources,
            Capture::new(),
            &waker,
            wake,
            keys,
            monitor,
            Some(Arc::clone(&menu)),
        )
        .map(|(w, s, t)| (w, s, Some(t), None))
    } else {
        open_window_and_clocked(
            &controls,
            clock,
            sources,
            &waker,
            wake,
            keys,
            monitor,
            Some(Arc::clone(&menu)),
        )
        .map(|(w, s, t)| (w, s, None, Some(t)))
    };
    let (window_thread, surface, raw_thread, clocked_thread) = match started {
        Ok(started) => started,
        Err(e) => {
            audio.exit();
            return Err(e);
        }
    };
    let make: MakeScreen = Box::new(move || {
        let target = Target::Window {
            window: surface.into(),
            width: surface.width,
            height: surface.height,
        };
        Renderer::new(target, AdapterChoice::HighPerformance)
            .map(|r| Box::new(MenuScreen::new(r, None)) as Box<dyn Screen>)
    });
    #[cfg_attr(not(feature = "dev-tools"), allow(unused_mut))]
    let mut measure = measure(
        clock,
        surface.refresh_mhz,
        opened,
        probes.steps.1,
        probes.sounds.1,
        &controls,
    );
    #[cfg(feature = "dev-tools")]
    if keep_presents {
        let lead_in = loaded.config().lead_in;
        measure.keep_presents(presents_of(loaded.chart(), lead_in, surface.refresh_mhz));
    }
    let render = render_side::spawn(
        make,
        Some(Arc::new(loaded.chart().clone())),
        loaded.section().cloned(),
        clock,
        view(&controls),
        measure,
        Box::new(NoFrames),
    );
    let render = match render {
        Ok(r) => r,
        Err(e) => {
            audio.exit();
            window_thread.close();
            return Err(e.into());
        }
    };
    let threads = Threads {
        input,
        audio,
        render,
        steps: probes.steps.0,
    };
    let autoplay = loaded.config().autoplay.is_some();
    let mut rig = rig(
        loaded.root(),
        loaded.settings(),
        autoplay,
        threads,
        &controls,
        clock,
    );
    if let Some(keyboard) = keyboard {
        rig.recorded.keyboard = keyboard;
    }
    rig.restorer = restorer(&loaded, &rig);
    rig.result_input = menus;
    rig.queue_most = opened.most_queued_periods;
    tune(&mut rig);
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    #[cfg(feature = "dev-tools")]
    let script_failed = result_script_failure(&rig);
    let rendered = rig.shut_down();
    window_thread.close_after(rendered != RenderEnd::Stuck);
    // Not joined: an input thread that stopped answering must not hold up
    // the exit or the report of why the run ended. Its wake report is not
    // used here.
    if let Some(t) = raw_thread {
        t.detach();
    }
    if let Some(t) = clocked_thread {
        t.detach();
    }
    ran?;
    #[cfg(feature = "dev-tools")]
    if let Some(why) = script_failed {
        anyhow::bail!("dev: the result script failed: {why}");
    }
    Ok(())
}

/// Why the scripted result input of a run without selection failed, if it
/// did (dev-tools).
#[cfg(feature = "dev-tools")]
fn result_script_failure(rig: &PlayerRig<'_>) -> Option<String> {
    rig.result_script
        .as_ref()
        .and_then(|s| s.failure().map(str::to_owned))
}

/// What makes the extension points of each chart the scene flow loads.
type MakeParts = Box<dyn FnMut(&Settings) -> Parts>;

/// The parts of a session whose plays are saved under `root`: each saved
/// play is counted into the bests as its result shows; without the
/// writer, plays are saved and the next launch's catch-up counts them.
fn saved_parts(root: &DataRoot, best_writer: Option<library::BestWriter>) -> MakeParts {
    let records = root.clone();
    let bests = best_writer.map(|w| Arc::new(std::sync::Mutex::new(w)));
    Box::new(move |s: &Settings| {
        let mut parts = Parts::standard(&records, &s.player);
        if let Some(bests) = &bests {
            parts.records = Some(Box::new(bests::BestsRecords::new(
                records.records(),
                Arc::clone(bests),
            )));
        }
        parts
    })
}

/// Opens the shared-mode WASAPI output on the audio thread.
fn wasapi_audio(clock: QpcClock) -> anyhow::Result<(audio_side::AudioThread, audio_side::Opened)> {
    let spawned = audio_side::spawn(
        move || -> Result<_, audio::WasapiError> {
            let out = audio::WasapiShared::open(clock)?;
            let most = out.grant().max_queue_periods;
            Ok((out, Some(most)))
        },
        |_| {},
    )?;
    Ok(spawned)
}

/// The simulated output a headless run with selection records what sounds
/// into (dev-tools).
#[cfg(feature = "dev-tools")]
fn recorded_audio(
    clock: QpcClock,
    seconds: usize,
) -> anyhow::Result<(audio_side::AudioThread, audio_side::Opened)> {
    let spawned = audio_side::spawn(
        // The simulated output always opens.
        move || -> Result<_, std::convert::Infallible> {
            let out = dev::sound_recording(clock, seconds);
            Ok((out, Some(audio::MAX_QUEUE_PERIODS)))
        },
        dev::report_audio,
    )?;
    Ok(spawned)
}

/// `tapestry [--auto]`: song selection (design.md "Play Lifecycle"). The
/// library's lock is taken first, so a second process on the same data
/// root is refused before it reads a setting or opens anything else. Then
/// the threads start without a chart, the scan's first pass with them, and
/// the scene flow runs until the window closes: each chosen chart is
/// loaded onto the running threads and played until the player leaves it.
/// With `--auto` every chart plays by itself and nothing is saved.
fn select(s: args::Select, clock: QpcClock) -> anyhow::Result<()> {
    #[cfg(feature = "dev-tools")]
    alloc_count::tag(alloc_count::Tag::Logic);
    let root = data_root()?;
    let (library, opened) = shelf::open(&root)?;
    println!("{opened}");
    log::info!("{opened}");
    let bests = library.bests_state();
    println!("{bests}");
    log::info!("{bests}");
    let settings = settings(&root)?;
    let autoplay = matches!(s.input, PlayInput::Autoplay);
    #[cfg_attr(
        not(feature = "dev-tools"),
        expect(unused_mut, reason = "a scenario names its own")
    )]
    let mut roots: Vec<PathBuf> = settings
        .device
        .library
        .roots
        .iter()
        .map(PathBuf::from)
        .collect();
    // A scenario's roots, for this run only: nothing is written back.
    #[cfg(feature = "dev-tools")]
    let mut s = s;
    #[cfg(feature = "dev-tools")]
    let scenario = s.dev.scenario.take();
    #[cfg(feature = "dev-tools")]
    if let Some(sc) = &scenario {
        roots.clone_from(&sc.roots);
    }
    // A scenario's plays of input logs are saved and counted live.
    #[cfg(feature = "dev-tools")]
    let counted = scenario.is_some();
    #[cfg(feature = "dev-tools")]
    let (headless, rescan) = (
        s.dev.headless,
        s.dev
            .rescan_ms
            .map_or(shelf::RESCAN_AFTER, std::time::Duration::from_millis),
    );
    #[cfg(not(feature = "dev-tools"))]
    let rescan = shelf::RESCAN_AFTER;
    let shelf = shelf::Shelf::new(library, roots.clone(), rescan, Some(root.records()));
    let best_writer = match shelf.best_writer() {
        Ok(w) => Some(w),
        #[cfg(feature = "dev-tools")]
        Err(e) if counted => {
            anyhow::bail!(
                "dev: scenario: plays cannot be counted live: {}",
                game::Chain(&e)
            );
        }
        Err(e) => {
            let line = format!("library: plays are not counted live: {}", game::Chain(&e));
            eprintln!("{line}");
            log::warn!("{line}");
            None
        }
    };
    let controls = controls(&settings, clock);
    controls.quit_leaves();
    let waker = Waker::new().context("input")?;
    #[cfg(feature = "dev-tools")]
    let _suspend = if headless {
        None
    } else {
        Some(power::SuspendWatch::register(Arc::clone(&controls))?)
    };
    #[cfg(not(feature = "dev-tools"))]
    let _suspend = power::SuspendWatch::register(Arc::clone(&controls))?;
    #[cfg(feature = "dev-tools")]
    let (audio, opened) = if headless {
        recorded_audio(clock, 240)?
    } else {
        wasapi_audio(clock)?
    };
    #[cfg(not(feature = "dev-tools"))]
    let (audio, opened) = wasapi_audio(clock)?;
    let probes = Probes::new();
    // Silent until a chart is loaded: an empty bank and schedule.
    let mut engine = AudioEngine::with_schedule(
        Arc::new(audio::SoundBank::from_sounds(opened.rate, Vec::new())),
        Vec::new(),
        opened.rate,
        opened.period,
        MAX_VOICES,
        SCHEDULE_LOG,
    )
    .context("the audio engine cannot start")?;
    settings.device.apply_to(&mut engine);
    let audio = audio.start(engine, probes.sounds.0);
    // Autoplay's queues are sized by each chart's feed, before its first
    // play; until then they hold nothing.
    let mut sources = sources();
    if autoplay {
        sources.set_queue_capacity(0);
    } else {
        axis_mode(&mut sources, &settings.device);
    }
    let (menu, menus) = menu_input(&settings.device, autoplay, &mut sources);
    controls.set_latch(sources.gate().handle());
    controls.set_waker(waker.clone());
    let mode = if autoplay {
        Mode::Autoplay(AutoplayInput::new(Vec::new()))
    } else {
        Mode::Clock
    };
    let (mut hook, input) = input_side::link(Arc::clone(&controls), mode, waker.clone());
    let wake = move |s: &mut SourceSet, now| {
        #[cfg(feature = "dev-tools")]
        alloc_count::tag(alloc_count::Tag::Input);
        hook.wake(s, now)
    };
    let device = &settings.device;
    let keys = (device.keyboard.retry, device.keyboard.quit);
    let monitor = device.monitor.clone();
    // The window, then the input thread that suits the input.
    macro_rules! windowed {
        () => {
            if autoplay {
                open_window_and_clocked(
                    &controls,
                    clock,
                    sources,
                    &waker,
                    wake,
                    keys,
                    monitor,
                    Some(Arc::clone(&menu)),
                )
                .map(|(w, s, t)| (Some(w), Some(s), None, Some(t)))
            } else {
                open_window_and_input(
                    &controls,
                    clock,
                    sources,
                    Capture::new(),
                    &waker,
                    wake,
                    keys,
                    monitor,
                    Some(Arc::clone(&menu)),
                )
                .map(|(w, s, t)| (Some(w), Some(s), Some(t), None))
            }
        };
    }
    #[cfg(feature = "dev-tools")]
    let started = if headless {
        ClockedThread::spawn(clock, sources, &waker, wake)
            .map(|t| (None, None, None, Some(t)))
            .context("input")
    } else {
        windowed!()
    };
    #[cfg(not(feature = "dev-tools"))]
    let started = windowed!();
    let (window_thread, surface, raw_thread, clocked_thread) = match started {
        Ok(started) => started,
        Err(e) => {
            audio.exit();
            return Err(e);
        }
    };
    let ime = window_thread.as_ref().map(window::WindowThread::ime);
    let refresh_mhz = surface.and_then(|s| s.refresh_mhz);
    let make: MakeScreen = match surface {
        Some(surface) => Box::new(move || {
            let target = Target::Window {
                window: surface.into(),
                width: surface.width,
                height: surface.height,
            };
            let r = Renderer::new(target, AdapterChoice::HighPerformance)?;
            let fonts = render::text::Fonts::runtime()?;
            Ok(Box::new(MenuScreen::new(r, Some(fonts))) as Box<dyn Screen>)
        }),
        // Headless (dev-tools): offscreen on WARP.
        #[cfg(feature = "dev-tools")]
        None => Box::new(|| {
            let target = Target::Offscreen {
                width: 1280,
                height: 720,
            };
            let r = Renderer::new(target, AdapterChoice::Warp)?;
            let fonts = render::text::Fonts::runtime()?;
            Ok(Box::new(MenuScreen::new(r, Some(fonts))) as Box<dyn Screen>)
        }),
        // A production run always has its window.
        #[cfg(not(feature = "dev-tools"))]
        None => Box::new(|| Err(render::RenderError::SurfaceUnsupported)),
    };
    let measure = measure(
        clock,
        refresh_mhz,
        opened,
        probes.steps.1,
        probes.sounds.1,
        &controls,
    );
    #[cfg(feature = "dev-tools")]
    let frames: Box<dyn render_side::FrameSink> = match (&scenario, s.dev.frames) {
        (Some(sc), _) => Box::new(dev::scenario::ScenarioFrames::from_env(sc.captures.clone())),
        (None, Some(dir)) => Box::new(dev::FrameDump::new(dir, TimeUs(1_000_000))),
        (None, None) => Box::new(NoFrames),
    };
    #[cfg(not(feature = "dev-tools"))]
    let frames: Box<dyn render_side::FrameSink> = Box::new(NoFrames);
    #[cfg_attr(not(feature = "dev-tools"), allow(unused_mut))]
    let mut render =
        match render_side::spawn(make, None, None, clock, view(&controls), measure, frames) {
            Ok(r) => r,
            Err(e) => {
                audio.exit();
                if let Some(w) = window_thread {
                    w.close();
                }
                return Err(e.into());
            }
        };
    #[cfg(feature = "dev-tools")]
    if let Some(bound) = s.dev.render_timeout {
        render.set_timeouts(bound, bound);
    }
    let threads = Threads {
        input,
        audio,
        render,
        steps: probes.steps.0,
    };
    let mut rig = rig(&root, &settings, autoplay, threads, &controls, clock);
    rig.queue_most = opened.most_queued_periods;
    rig.result_input = menus;
    rig.selection = Some(selection::Selection {
        shelf,
        screen: select::SelectScreen::new(roots),
        menu,
        dropped: 0,
        preset_refusal: false,
        search_pending: false,
        ime,
        rate: opened.rate,
        chosen_at: TimeUs(0),
        entered_at: TimeUs(0),
        dirty: true,
        #[cfg(feature = "dev-tools")]
        script: s.dev.script,
    });
    // Each chart of a scenario has its input chosen as it loads
    // (`AppRig::configure`), from autoplay's session.
    #[cfg(feature = "dev-tools")]
    if let Some(sc) = scenario {
        rig.scenario = Some(dev::scenario::State {
            logs: sc.logs,
            captures: sc.captures,
            recorded: rig.recorded.clone(),
            chart_input: None,
            pending: None,
            until: None,
            practice: None,
            shown: false,
            rounds: 0,
        });
    }
    // Autoplay saves nothing, but in a scenario, whose logs' plays are
    // saved.
    #[cfg(feature = "dev-tools")]
    let saved = !autoplay || counted;
    #[cfg(not(feature = "dev-tools"))]
    let saved = !autoplay;
    let parts: MakeParts = if saved {
        saved_parts(&root, best_writer)
    } else {
        Box::new(|s: &Settings| Parts::unsaved(&s.player))
    };
    let (receive, auto) = if autoplay {
        (ReceiveMethod::Scripted, Some(AutoKeySounds::Scheduled))
    } else {
        (ReceiveMethod::RawInput, None)
    };
    let mut app = game::App::new(
        root,
        settings,
        session_config(receive, auto),
        parts,
        Box::new(game::NoSessionEvents),
    );
    let ran = app.run(&mut rig);
    #[cfg(feature = "dev-tools")]
    let script_failed = rig
        .selection
        .as_ref()
        .and_then(selection::Selection::script_failure);
    #[cfg(feature = "dev-tools")]
    let scenario = rig.scenario.take().map(|sc| {
        let script = rig.selection.as_mut().and_then(|sel| sel.script.take());
        (sc, script)
    });
    let rendered = rig.shut_down();
    if let Some(w) = window_thread {
        w.close_after(rendered != RenderEnd::Stuck);
    }
    // As in `play`: not joined.
    if let Some(t) = raw_thread {
        t.detach();
    }
    if let Some(t) = clocked_thread {
        t.detach();
    }
    #[cfg(feature = "dev-tools")]
    if let Some((sc, script)) = scenario {
        return dev::scenario::conclude(ran, script.as_ref(), sc.pending, rendered, &sc.captures);
    }
    ran?;
    #[cfg(feature = "dev-tools")]
    if let Some(why) = script_failed {
        anyhow::bail!("dev: the select script failed: {why}");
    }
    Ok(())
}

/// A headless run (dev-tools): an input log replayed as a script, or
/// autoplay (`--auto --dev-headless`), on a clocked input thread, a
/// recording of a simulated device as output, WARP offscreen as the
/// screen.
#[cfg(feature = "dev-tools")]
fn headless(
    chart: &Path,
    input: PlayInput,
    start: Option<TimeUs>,
    frames: Option<PathBuf>,
    auto_retries: u32,
    clock: QpcClock,
    tune: impl FnOnce(&mut PlayerRig<'_>),
) -> anyhow::Result<()> {
    alloc_count::tag(alloc_count::Tag::Logic);
    let Prepared {
        mut loaded,
        mode,
        sources,
        schedule,
        keyboard,
    } = prepare(chart, &input, start)?;
    let controls = controls(loaded.settings(), clock);
    let waker = Waker::new().context("input")?;
    let last_note = loaded
        .chart()
        .lanes
        .iter()
        .flatten()
        .map(|n| n.time)
        .max()
        .unwrap_or(TimeUs(0));
    let (audio, opened) = audio_side::spawn(
        // The simulated output always opens.
        move || -> Result<_, std::convert::Infallible> {
            let out = dev::recording_output(clock, 60);
            Ok((out, Some(audio::MAX_QUEUE_PERIODS)))
        },
        dev::report_audio,
    )?;
    let probes = Probes::new();
    let audio = start_engine(&loaded, schedule, audio, opened, probes.sounds.0)?;
    controls.set_latch(sources.gate().handle());
    controls.set_waker(waker.clone());
    let (mut hook, input) = input_side::link(Arc::clone(&controls), mode, waker.clone());
    let input_thread = ClockedThread::spawn(clock, sources, &waker, move |s, now| {
        alloc_count::tag(alloc_count::Tag::Input);
        hook.wake(s, now)
    })
    .context("input")?;
    let dump: Box<dyn render_side::FrameSink> = match frames {
        Some(dir) => Box::new(dev::FrameDump::new(dir, TimeUs(last_note.0 / 2))),
        None => Box::new(NoFrames),
    };
    let make: MakeScreen = Box::new(|| {
        let target = Target::Offscreen {
            width: 1280,
            height: 720,
        };
        Renderer::new(target, AdapterChoice::Warp)
            .map(|r| Box::new(MenuScreen::new(r, None)) as Box<dyn Screen>)
    });
    // Offscreen: no refresh rate and no frame statistics.
    let measure = measure(
        clock,
        None,
        opened,
        probes.steps.1,
        probes.sounds.1,
        &controls,
    );
    let render = render_side::spawn(
        make,
        Some(Arc::new(loaded.chart().clone())),
        loaded.section().cloned(),
        clock,
        view(&controls),
        measure,
        dump,
    )?;
    let threads = Threads {
        input,
        audio,
        render,
        steps: probes.steps.0,
    };
    let autoplay = loaded.config().autoplay.is_some();
    let mut rig = rig(
        loaded.root(),
        loaded.settings(),
        autoplay,
        threads,
        &controls,
        clock,
    );
    if let Some(keyboard) = keyboard {
        rig.recorded.keyboard = keyboard;
    }
    rig.restorer = restorer(&loaded, &rig);
    rig.quit_after_end = Some(std::time::Duration::from_millis(300));
    rig.auto_retries = auto_retries;
    rig.queue_most = opened.most_queued_periods;
    tune(&mut rig);
    let ran = game::live::run_chart(&mut loaded, &mut rig);
    let script_failed = result_script_failure(&rig);
    // Offscreen: no window to keep.
    let _ = rig.shut_down();
    // As in `play`: not joined.
    input_thread.detach();
    ran?;
    if let Some(why) = script_failed {
        anyhow::bail!("dev: the result script failed: {why}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use input::QpcClock;

    use super::{
        Controls, Mode, axis_mode, diagnostic, input_side, menu_input, open_window_and_input,
        sources,
    };

    /// Both launch paths take the menus' input from `menu_input`: the
    /// window's queue is the one the result screen reads, and a configured
    /// controller's lane controls are tapped from the sources, but not for
    /// autoplay or without a configured controller.
    #[test]
    fn the_menus_share_the_window_queue_and_tap_the_controller() {
        let configured = game::DeviceSettings {
            controller_device: Some(game::ControllerDevice {
                vendor_id: 1,
                product_id: 2,
                interface: None,
            }),
            ..game::DeviceSettings::default()
        };
        let mut set = sources();
        let (window, mut menus) = menu_input(&configured, false, &mut set);
        assert!(Arc::ptr_eq(&window, &menus.menu));
        assert!(menus.pad.receiver.is_some());
        assert!(set.menu_mut().is_some());
        // A key the window queues reaches the result once it shows.
        assert!(menus.enter());
        window.push(crate::select::MenuInput::Key(crate::select::MenuKey::Right));
        assert_eq!(menus.take(), Some(game::MenuCommand::NextPreset));
        for (device, autoplay) in [
            (&configured, true),
            (&game::DeviceSettings::default(), false),
        ] {
            let mut set = sources();
            let (window, menus) = menu_input(device, autoplay, &mut set);
            assert!(Arc::ptr_eq(&window, &menus.menu));
            assert!(menus.pad.receiver.is_none());
            assert!(set.menu_mut().is_none());
        }
    }

    /// `device.toml`'s scratch mode reaches the configured controller's
    /// axes bound to lanes, and nothing else: not its buttons, not an
    /// unbound axis, not another model, and nothing when no mode is set.
    #[test]
    fn the_scratch_mode_reaches_the_bound_axes() {
        use eval::ControlId;
        use input::hid::{HidProduct, axis_cap, axis_control};
        use input::{DeviceId, DeviceKind};
        let product = HidProduct {
            vendor_id: 1,
            product_id: 2,
            interface: None,
        };
        let (x, y) = (axis_control(0x01, 0x30), axis_control(0x01, 0x31));
        let mut device = game::DeviceSettings::default();
        device.controller.lanes = vec![
            game::LaneKey {
                control: x.0,
                lane: 0,
            },
            game::LaneKey {
                control: 1,
                lane: 1,
            },
        ];
        device.controller_device = Some(product.into());
        let cap = |c: ControlId| axis_cap(c, true, 0, 255, 8).unwrap();
        let relative = |device: &game::DeviceSettings, id: u64, p: HidProduct, c: ControlId| {
            let mut set = sources();
            set.connect(DeviceId(id), DeviceKind::Controller);
            set.set_product(DeviceId(id), p);
            axis_mode(&mut set, device);
            set.axis_relative(DeviceId(id), &cap(c))
        };
        assert!(!relative(&device, 1, product, x));
        device.scratch.relative = Some(true);
        assert!(relative(&device, 1, product, x));
        assert!(!relative(&device, 1, product, y));
        let other = HidProduct {
            product_id: 3,
            ..product
        };
        assert!(!relative(&device, 1, other, x));
    }

    /// The report prints the failure and each cause once, on one line: a
    /// chart that cannot be loaded names the file, then the session's and
    /// the loader's reasons.
    #[test]
    fn the_report_prints_every_cause_once() {
        let load = bms_parser::LoadError::Unsupported {
            line: 2,
            what: "#LNTYPE 2".to_owned(),
        };
        let e = game::SessionError::Ingest(game::IngestError::Load(load));
        let e = anyhow::Error::new(e).context("song.bms".to_owned());
        assert_eq!(
            diagnostic(&e),
            "song.bms: the chart cannot be loaded: line 2: unsupported #LNTYPE 2"
        );
    }

    /// The real startup order leaves the keyboards registered to the input
    /// thread. Opening the input thread before the window fails it: winit
    /// takes the registration and then removes it.
    #[test]
    #[ignore = "opens a window: needs an interactive desktop"]
    fn keyboards_stay_registered_to_the_input_thread() {
        let clock = QpcClock::start();
        let controls = Arc::new(Controls::new(clock));
        let waker = input::Waker::new().unwrap();
        let (mut hook, _link) = input_side::link(Arc::clone(&controls), Mode::Clock, waker.clone());
        let (window, _surface, input) = open_window_and_input(
            &controls,
            clock,
            sources(),
            input::Capture::new(),
            &waker,
            move |s, now| hook.wake(s, now),
            (Some(0x0E), Some(0x01)),
            None,
            None,
        )
        .expect("startup");
        assert_eq!(input::keyboard_target(), Some(input.receiver()));
        drop(input);
        window.close();
    }
}
