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
mod attempt_context;
mod audio_side;
mod background;
mod bests;
mod controls;
#[cfg(feature = "dev-tools")]
mod dev;
mod end_screen;
mod imports;
mod input_side;
mod keys;
mod loading_state;
mod loading_text;
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
mod result_present;
mod result_skin;
mod result_view;
mod screens;
mod select;
mod selection;
mod selection_skin;
mod shelf;
mod start_gate;
mod tuning;
mod ui_text;
mod window;

// The render crate's WARP helpers and its declarative, imported and
// rasterized-text skin fixtures, for the tests that drive a real menu
// screen or build skins.
#[cfg(test)]
#[path = "../../render/tests/font_support/mod.rs"]
mod font_support;
#[cfg(test)]
#[path = "../../render/tests/common/mod.rs"]
mod render_common;
#[cfg(test)]
#[path = "../../render/tests/decl_support/mod.rs"]
mod render_decl;
#[cfg(test)]
#[path = "../../render/tests/json_support/mod.rs"]
mod render_json;
#[cfg(test)]
#[path = "../../render/tests/outline_support/mod.rs"]
mod render_outline;

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

/// READY before chart time 0 (design.md "Play Lifecycle").
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
    #[cfg(feature = "dev-tools")]
    let ran = launch(&chart, p.input, p.dev, clock, headless, play);
    #[cfg(not(feature = "dev-tools"))]
    let ran = play(&chart, p.input, None, clock, |_| {});
    report(ran)
}

/// What configures a play's rig from the development options (dev-tools).
#[cfg(feature = "dev-tools")]
type Tune = Box<dyn FnOnce(&mut PlayerRig<'_>)>;

/// Launches `play` of `chart` with its development options (dev-tools):
/// an input log or `--dev-headless` through `headless` ([`headless`] in
/// production), else the interactive play through `interactive`
/// ([`play`], keeping no presents). Both get the mid-chart start and the
/// same tuner ([`dev::Options::tune`]), so no option applies to one path
/// only.
#[cfg(feature = "dev-tools")]
fn launch<T>(
    chart: &Path,
    input: PlayInput,
    mut options: dev::Options,
    clock: QpcClock,
    headless: impl FnOnce(
        &Path,
        PlayInput,
        Option<TimeUs>,
        Option<PathBuf>,
        (u32, u32),
        QpcClock,
        Tune,
    ) -> T,
    interactive: impl FnOnce(&Path, PlayInput, Option<TimeUs>, QpcClock, bool, Tune) -> T,
) -> T {
    // A mid-chart start (`--dev-start`).
    let start = options.start;
    let scripted = options.headless || matches!(input, PlayInput::Script(..));
    let frames = options.frames.take();
    let size = options.size.unwrap_or(dev::DEFAULT_SIZE);
    let tune: Tune = Box::new(move |rig| options.tune(rig));
    if scripted {
        headless(chart, input, start, frames, size, clock, tune)
    } else {
        interactive(chart, input, start, clock, false, tune)
    }
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
    let settings = Settings::load(root).with_context(|| root.path().display().to_string())?;
    ui_text::init(settings.player.language);
    Ok(settings)
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
    /// An input log replayed as a script, with the configured keys, and
    /// the speed keys pressed on its keyboard as unrecorded packets the
    /// adjustment tap observes; plays are saved (dev-tools).
    #[cfg(feature = "dev-tools")]
    Script(dev::ScriptInput),
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
        PlayInput::Script(..) => (
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
        PlayInput::Script(input) => {
            let script = dev::script(&input.log, &input.speed)?;
            let loading = dev::loading_script(&input.log, &input.loading)?;
            let script = input_side::Script {
                log: script.clone(),
                current: script,
                sources: source_config(),
                loading,
                rearm_delay: input.rearm_delay,
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
/// the first play; with the plays' gain, measured now (design.md
/// "Algorithms", per-chart volume normalization: without a library each
/// load measures, and nothing is stored). A close cancels the measurement.
fn start_engine(
    loaded: &Loaded,
    schedule: Option<Vec<BgmEvent>>,
    audio: audio_side::AudioThread,
    opened: audio_side::Opened,
    probe: rtrb::Producer<game::AudioProbe>,
    controls: &Controls,
) -> anyhow::Result<(audio_side::AudioLink, audio::Gain)> {
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
    let bank = Arc::new(bank);
    let name = shelf::file_name(loaded.path());
    let mut host = PlayHost {
        loaded,
        rate: opened.rate,
        controls,
    };
    let gain = loudness::unstored_gain(chart, &bank, &name, &mut host);
    let mut engine = AudioEngine::with_schedule(
        bank,
        schedule.unwrap_or_else(|| loaded.audio_schedule()),
        opened.rate,
        opened.period,
        MAX_VOICES,
        SCHEDULE_LOG,
    )
    .context("the audio engine cannot start")?;
    loaded.settings().device.apply_to(&mut engine);
    Ok((audio.start(engine, probe), gain))
}

/// What `tapestry play`'s load lends its loudness measurement: the 1×
/// chart and bank for a chart at another speed, and the close or the quit
/// key as its cancel.
struct PlayHost<'a> {
    loaded: &'a Loaded,
    rate: u32,
    controls: &'a Controls,
}

impl loudness::Host for PlayHost<'_> {
    fn at_normal(&mut self) -> anyhow::Result<(bms_model::Chart, Arc<audio::SoundBank>)> {
        let chart = self.loaded.chart_at(bms_model::Speed::NORMAL)?;
        let bank = load_bank_at(
            &chart,
            self.rate,
            bms_model::Speed::NORMAL,
            &BankLimits::default(),
        )?;
        Ok((chart, Arc::new(bank)))
    }

    fn measuring(&mut self, _name: &str) {}

    fn cancelled(&mut self) -> bool {
        self.controls.quitting() || self.controls.leaving()
    }
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
/// `refresh_mhz` (1 kHz when unknown): READY, the chart to its last
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

/// The frames' inputs.
fn view() -> View {
    View { effects: EFFECTS }
}

/// The controls, with the settings keys starting from the loaded values,
/// waking the calling thread: the game thread. The quit key is taken once
/// by the play or result it arrives in, with or without selection: in a
/// play's READY state it leaves the chart, while the chart plays it ends the
/// play on its result, after completion it skips the tail to the result
/// (design.md "Play Lifecycle", "Audio tail"); only closing the window
/// latches quitting.
fn controls(s: &Settings, clock: QpcClock) -> Arc<Controls> {
    let tuning = LiveTuning::new(Tuning::of(s)).with_loaded_volume(s.device.master_volume);
    let controls = Controls::with_tuning(clock, Arc::new(tuning));
    controls.quit_leaves();
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

/// Installs the adjustments in `rig` (design.md "Play Adjustments"): the
/// bindings of `settings`'s key configs planned for the tap, the driver
/// with the player's options and the display offset, its snapshots handed
/// to the render thread, and the tap with its start gate to the input
/// thread. In an autoplay session F1 and F2 come from the window
/// ([`Controls::set_window_keys`]), as no keyboard reaches the tap.
///
/// # Errors
///
/// What the plan refuses of the bindings (more controls than the tap
/// takes), or what the tap refuses.
fn install_adjustments(rig: &mut PlayerRig<'_>, settings: &Settings) -> anyhow::Result<()> {
    let device = &settings.device;
    let keys = game::KeyConfigs::from(device);
    let config = game::AdjustmentConfig::of(device);
    // A wrapping axis steps by its revolution; any other by the quantum
    // `device.toml` sets.
    let quantum = config
        .counts_per_step
        .and_then(game::adjustment::AxisQuantum::per_step);
    let plan = config.plan(&keys, quantum, device.scratch.wrap)?;
    let options = settings.player.options()?;
    let (driver, mut tap, reader) =
        game::AdjustDriver::new(&plan, options, rig.controls.tuning().display_offset_us())?;
    if rig.autoplay {
        let number = |code: u32| {
            plan.bindings
                .controls
                .iter()
                .find(|(c, _)| *c == input::TapControl::Key(eval::ControlId(code)))
                .map(|&(_, n)| n)
        };
        let [f1, f2] = game::SPEED_KEYS;
        let (keys, receiver) = input::window_keys();
        tap.set_window_keys(receiver, [number(f1), number(f2)]);
        rig.controls.set_window_keys(keys);
    }
    rig.panels = Some(tap.panel_level());
    rig.render.presentation(reader);
    rig.gate = Some(crate::start_gate::PlayerGate::install(&rig.input, tap));
    rig.adjust = Some(driver);
    Ok(())
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
) -> anyhow::Result<PlayerRig<'a>> {
    let Threads {
        input,
        audio,
        render,
        steps,
    } = threads;
    let device = &settings.device;
    let mut rig = PlayerRig {
        input,
        audio,
        render,
        skin: render_side::SkinState::Ready,
        pending: None,
        controls,
        clock,
        recorded: input_side::Recorded {
            keyboard: device.keyboard.recorded(),
            controller: device.controller.recorded(),
            controller_device: device.controller_device.map(Into::into),
            scratch: device.scratch,
        },
        gauge: settings.player.displayed_gauge,
        attempt_gauge: settings.player.displayed_gauge,
        autoplay,
        input_offset_us: device.input_offset_us,
        files: SettingsFiles::new(root.clone(), settings.clone()),
        seen: Delivered::default(),
        shown_speed: end_screen::SpeedLine::default(),
        end_page: None,
        hints: Hints::new(device.keyboard.retry, device.keyboard.quit),
        result_input: result_input::ResultInput::new(Arc::new(MenuQueue::default()), None),
        viewed: None,
        evaluated: 0,
        #[cfg(feature = "dev-tools")]
        result_script: None,
        load_start: TimeUs(0),
        pending_retry: None,
        window: None,
        retry_from: None,
        times: LoadTimes::default(),
        ready_state_at: None,
        origin: None,
        declared_close: None,
        steps,
        steps_dropped: 0,
        figures: None,
        interval: None,
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
        loading_script: false,
        #[cfg(feature = "dev-tools")]
        adjust_in_play: Vec::new(),
        #[cfg(feature = "dev-tools")]
        speed_in_play: Vec::new(),
        #[cfg(feature = "dev-tools")]
        adjust_at: TimeUs(0),
        #[cfg(feature = "dev-tools")]
        adjust_on_end: Vec::new(),
        selection: None,
        gain: audio::Gain::HEADROOM,
        gate: None,
        adjust: None,
        panels: None,
        result_skin: crate::result_skin::ResultSkin::default(),
        result_chart: None,
        presenter: crate::result_present::Presenter::default(),
        result_labels: None,
        result_withdrawn: false,
        result_facts: None,
        result_offset: None,
        result_leaving: false,
    };
    install_adjustments(&mut rig, settings)
        .context("the adjustment controls cannot be installed")?;
    // What a selection skin shows of the settings keys, read each frame.
    rig.render.tuning(Arc::clone(controls.tuning()));
    Ok(rig)
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
    let (audio, gain) = start_engine(&loaded, schedule, audio, opened, probes.sounds.0, &controls)?;
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
    let link = window_thread.link();
    let make: MakeScreen = Box::new(move || {
        let target = Target::Window {
            window: surface.into(),
            width: surface.width,
            height: surface.height,
        };
        Renderer::new(target, AdapterChoice::HighPerformance).map(|r| {
            let screen = MenuScreen::new(r, None).with_sink(Box::new(link.clone()));
            Box::new(screen) as Box<dyn Screen>
        })
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
        view(),
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
    render.follow(&window_thread.extent());
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
    )?;
    if let Some(keyboard) = keyboard {
        rig.recorded.keyboard = keyboard;
    }
    rig.window = Some(window_thread.shown());
    rig.restorer = restorer(&loaded, &rig);
    rig.gain = gain;
    rig.result_input = menus;
    rig.queue_most = opened.most_queued_periods;
    tune(&mut rig);
    let ran = skins_then_chart(&mut loaded, &mut rig);
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

/// Startup's skins of `launch` for `rig` ([`imports::startup`]), from
/// `settings`, the window's close stopping them.
fn start_skins(
    rig: &PlayerRig<'_>,
    settings: &Settings,
    launch: imports::Launch,
) -> Result<imports::Started, imports::StartupEnd> {
    let build: imports::Build = Arc::new(imports::build);
    imports::startup(
        &rig.render,
        &settings.player.skins,
        launch,
        rig.autoplay,
        &build,
        || rig.controls.quitting(),
    )
}

/// `tapestry play`'s chart after its skins: a play skin that cannot be
/// used fails the run before any recording opens, and a close during
/// startup ends it with nothing played.
fn skins_then_chart(loaded: &mut Loaded, rig: &mut PlayerRig<'_>) -> anyhow::Result<()> {
    match start_skins(rig, loaded.settings(), imports::Launch::Direct) {
        Ok(started) => {
            rig.pending = started.pending;
            rig.result_skin = crate::result_skin::ResultSkin::new(started.result);
            game::live::run_chart(loaded, rig)
                .map(|_| ())
                .map_err(anyhow::Error::from)
        }
        Err(imports::StartupEnd::Closed) => Ok(()),
        Err(imports::StartupEnd::Failed(e)) => Err(e),
    }
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
    let size = scenario
        .as_ref()
        .and_then(|sc| sc.size)
        .or(s.dev.size)
        .unwrap_or(dev::DEFAULT_SIZE);
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
    #[cfg_attr(
        not(feature = "dev-tools"),
        expect(unused_mut, reason = "`--dev-stop-timeout` sets its bound")
    )]
    let mut shelf = shelf::Shelf::new(library, roots.clone(), rescan, Some(root.records()));
    #[cfg(feature = "dev-tools")]
    if let Some(bound) = s.dev.stop_timeout {
        shelf.set_stop_timeout(bound);
    }
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
    let link = window_thread.as_ref().map(window::WindowThread::link);
    let refresh_mhz = surface.and_then(|s| s.refresh_mhz);
    let make: MakeScreen = match (surface, link) {
        (Some(surface), Some(link)) => Box::new(move || {
            let target = Target::Window {
                window: surface.into(),
                width: surface.width,
                height: surface.height,
            };
            let r = Renderer::new(target, AdapterChoice::HighPerformance)?;
            let fonts = render::text::Fonts::runtime()?;
            let screen = MenuScreen::new(r, Some(fonts)).with_sink(Box::new(link.clone()));
            Ok(Box::new(screen) as Box<dyn Screen>)
        }),
        // Headless (dev-tools): offscreen on WARP.
        #[cfg(feature = "dev-tools")]
        _ => Box::new(move || {
            let target = Target::Offscreen {
                width: size.0,
                height: size.1,
            };
            let r = Renderer::new(target, AdapterChoice::Warp)?;
            let fonts = render::text::Fonts::runtime()?;
            Ok(Box::new(MenuScreen::new(r, Some(fonts))) as Box<dyn Screen>)
        }),
        // A production run always has its window.
        #[cfg(not(feature = "dev-tools"))]
        _ => Box::new(|| Err(render::RenderError::SurfaceUnsupported)),
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
    let mut render = match render_side::spawn(make, None, None, clock, view(), measure, frames) {
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
    if let Some(w) = &window_thread {
        render.follow(&w.extent());
    }
    let threads = Threads {
        input,
        audio,
        render,
        steps: probes.steps.0,
    };
    let mut rig = rig(&root, &settings, autoplay, threads, &controls, clock)?;
    rig.window = window_thread.as_ref().map(window::WindowThread::shown);
    rig.queue_most = opened.most_queued_periods;
    rig.result_input = menus;
    #[cfg_attr(not(feature = "dev-tools"), allow(unused_mut))]
    let mut screen = select::SelectScreen::new(roots);
    // A script opens on the all-chart list, where its steps pick by name.
    #[cfg(feature = "dev-tools")]
    if s.dev.script.is_some() {
        screen.show_all();
    }
    rig.selection = Some(selection::Selection {
        shelf,
        screen,
        menu,
        dropped: 0,
        preset_refusal: false,
        search_pending: false,
        preset_steps: std::collections::VecDeque::new(),
        feed: selection_skin::SkinFeed::default(),
        ime,
        rate: opened.rate,
        chosen_at: TimeUs(0),
        loading: None,
        practicing: false,
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
            sent: None,
            practice: None,
            shown: false,
            rounds: 0,
            nearest: None,
        });
    }
    // The skins, before the first screen: a refused play skin admits no
    // chart, and another kind's refusal is the select screen's notice.
    let (begin, failed) = match start_skins(&rig, &settings, imports::Launch::Selection) {
        Ok(started) => {
            if let Some(notice) = started.play {
                rig.skin = render_side::SkinState::Refused(notice);
            }
            // A pending play skin admits charts, each built for.
            rig.pending = started.pending;
            rig.result_skin = crate::result_skin::ResultSkin::new(started.result);
            if let Some(sel) = &mut rig.selection {
                sel.started(started.notice, started.selection);
            }
            (true, None)
        }
        Err(imports::StartupEnd::Closed) => (false, None),
        Err(imports::StartupEnd::Failed(e)) => (false, Some(e)),
    };
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
    let ran = if begin { app.run(&mut rig) } else { Ok(()) };
    #[cfg(feature = "dev-tools")]
    println!(
        "dev: skin imports {} lua passes {}",
        imports::jobs_started(),
        skin_beatoraja::vm_hooks::passes()
    );
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
    if let Some(e) = failed {
        return Err(e);
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
    size: (u32, u32),
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
    let (audio, gain) = start_engine(&loaded, schedule, audio, opened, probes.sounds.0, &controls)?;
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
    let make: MakeScreen = Box::new(move || {
        let target = Target::Offscreen {
            width: size.0,
            height: size.1,
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
        view(),
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
    )?;
    if let Some(keyboard) = keyboard {
        rig.recorded.keyboard = keyboard;
    }
    rig.restorer = restorer(&loaded, &rig);
    rig.gain = gain;
    rig.quit_after_end = Some(std::time::Duration::from_millis(300));
    rig.queue_most = opened.most_queued_periods;
    tune(&mut rig);
    let ran = skins_then_chart(&mut loaded, &mut rig);
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

    /// Both launch paths, `tapestry play <chart>` as song selection, make
    /// the quit key a command taken once by the play or result it arrives
    /// in, so a complete play's tail can skip to the result; only closing
    /// the window latches quitting.
    #[test]
    fn the_quit_key_is_taken_once_in_both_launch_paths() {
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let c = super::controls(&settings, QpcClock::start());
        c.command(game::Command::Quit);
        assert!(!c.quitting());
        assert_eq!(c.take(), Some(game::Command::Quit));
        assert_eq!(c.take(), None);
        c.close();
        assert!(c.quitting());
    }

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

    /// `f` on a play's rig, as `play` and `headless` build it from
    /// `device`, on threads that open nothing: an input link no thread
    /// serves, a simulated audio output with an empty bank and a render
    /// thread on stub screens; what `f` returned and whether the rig ended
    /// with a start gate.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn on_rig<R>(
        device: game::DeviceSettings,
        f: impl FnOnce(&mut crate::play::PlayerRig<'_>, &game::DeviceSettings) -> R,
    ) -> (R, bool) {
        on_rig_with(
            game::Settings {
                device,
                player: game::PlayerSettings::default(),
            },
            crate::render_side::tests::Installers::default().spawn_bare(),
            |rig, settings| f(rig, &settings.device),
        )
    }

    /// [`on_rig`] with `settings` and the render thread `render`.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn on_rig_with<R>(
        settings: game::Settings,
        render: crate::render_side::RenderLink,
        f: impl FnOnce(&mut crate::play::PlayerRig<'_>, &game::Settings) -> R,
    ) -> (R, bool) {
        on_rig_hooked(settings, render, QpcClock::start(), |rig, settings, _| {
            f(rig, settings)
        })
    }

    /// The input side of [`on_rig_hooked`]'s rig: its hook, the waker its
    /// link wakes and the rig's clock, for a test to run the input thread.
    #[cfg(feature = "dev-tools")]
    pub(crate) struct InputEnd {
        pub(crate) hook: input_side::Hook,
        pub(crate) waker: input::Waker,
        pub(crate) clock: QpcClock,
    }

    /// [`on_rig_with`] on `clock`, `f` given the rig's input side too.
    #[cfg(feature = "dev-tools")]
    pub(crate) fn on_rig_hooked<R>(
        settings: game::Settings,
        render: crate::render_side::RenderLink,
        clock: QpcClock,
        f: impl FnOnce(&mut crate::play::PlayerRig<'_>, &game::Settings, InputEnd) -> R,
    ) -> (R, bool) {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().join("data"));
        let controls = super::controls(&settings, clock);
        let waker = input::Waker::new().unwrap();
        let (hook, input) = input_side::link(Arc::clone(&controls), Mode::Clock, waker.clone());
        let (audio, opened) = crate::audio_side::spawn(
            move || {
                Ok::<_, std::convert::Infallible>((crate::dev::recording_output(clock, 1), None))
            },
            |_| {},
        )
        .unwrap();
        let bank = audio::SoundBank::from_sounds(opened.rate, Vec::new());
        let engine = audio::AudioEngine::new(
            Arc::new(bank),
            &[],
            opened.rate,
            opened.period,
            audio::MAX_VOICES,
            super::SCHEDULE_LOG,
        )
        .unwrap();
        let audio = audio.start(engine, rtrb::RingBuffer::new(1).0);
        let threads = super::Threads {
            input,
            audio,
            render,
            steps: rtrb::RingBuffer::new(1).0,
        };
        let mut rig = super::rig(&root, &settings, false, threads, &controls, clock).unwrap();
        let out = f(&mut rig, &settings, InputEnd { hook, waker, clock });
        let gated = rig.gate.is_some() && rig.adjust.is_some();
        let _ = rig.shut_down();
        (out, gated)
    }

    /// A retry pressed in Closing with the quit key, in either order, is
    /// taken as Closing's through the real controls: the quit first, then
    /// one retry, whose receipt the rig keeps as the next play's retry
    /// (`pending_retry`, then `retry_from` at the next preparation).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_retry_in_closing_is_received_once() {
        use game::{Command, Rig as _};
        for order in [
            [Command::Quit, Command::Retry],
            [Command::Retry, Command::Quit],
        ] {
            let (taken, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
                for c in order {
                    rig.controls.command(c);
                }
                let mut taken = Vec::new();
                while let Some(c) = rig.take_closing() {
                    taken.push(c);
                }
                let receipt = rig.controls.retry_at();
                assert!(receipt.is_some());
                assert_eq!(rig.pending_retry, receipt, "{order:?}");
                rig.prepare_screen().unwrap();
                assert_eq!(rig.retry_from, receipt, "{order:?}");
                taken
            });
            assert_eq!(taken, [Command::Quit, Command::Retry], "{order:?}");
        }
    }

    /// Commands pending together are taken in the controls' order,
    /// whatever order they were published in, with the retry's receipt
    /// kept only when the retry is taken: a suspend before a retry, both
    /// taken; a quit during play dropping a retry pressed with it, no
    /// receipt.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_batch_of_commands_is_taken_in_the_controls_order() {
        use Command::{Quit, Retry, Suspend};
        use game::{Command, Rig as _};
        for (batch, taken, receipt) in [
            ([Retry, Suspend], [Some(Suspend), Some(Retry)], true),
            ([Suspend, Retry], [Some(Suspend), Some(Retry)], true),
            ([Retry, Quit], [Some(Quit), None], false),
            ([Quit, Retry], [Some(Quit), None], false),
        ] {
            let ((), _) = on_rig(game::DeviceSettings::default(), |rig, _| {
                for c in batch {
                    rig.controls.command(c);
                }
                assert_eq!([rig.take(), rig.take()], taken, "{batch:?}");
                assert_eq!(rig.take(), None, "{batch:?}");
                let expected = if receipt {
                    rig.controls.retry_at()
                } else {
                    None
                };
                assert_eq!(rig.pending_retry, expected, "{batch:?}");
                assert_eq!(rig.pending_retry.is_some(), receipt, "{batch:?}");
            });
        }
    }

    /// The gauge each play frame was drawn with.
    #[cfg(feature = "dev-tools")]
    type Gauges = Arc<std::sync::Mutex<Vec<usize>>>;

    /// A screen that notes the gauge of every play frame it builds.
    #[cfg(feature = "dev-tools")]
    struct GaugeFrames {
        seen: Gauges,
    }

    #[cfg(feature = "dev-tools")]
    impl crate::render_side::Screen for GaugeFrames {
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
        fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.seen.lock().unwrap().push(f.gauge);
            Ok(())
        }
        fn draw(&mut self) -> Result<render::FrameTiming, render::RenderError> {
            Ok(render::FrameTiming {
                cpu: std::time::Duration::from_micros(50),
                sprites: 0,
                dropped: 0,
                presented: true,
                present_count: None,
            })
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, render::RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// What a play frame saw of the screens' runtimes: whether a play's
    /// interval was open, the selection runtime parked (its calls) and
    /// whether its VM was gone.
    #[cfg(feature = "dev-tools")]
    type Rests = Arc<std::sync::Mutex<Vec<(bool, Option<(u64, bool)>, bool)>>>;

    /// A screen that notes, at every play frame it builds, what
    /// [`Rests`] holds of `scripts`, once set.
    #[cfg(feature = "dev-tools")]
    struct RestFrames {
        scripts: Arc<std::sync::OnceLock<crate::screens::Scripts>>,
        gone: Arc<std::sync::atomic::AtomicBool>,
        seen: Rests,
    }

    #[cfg(feature = "dev-tools")]
    impl crate::render_side::Screen for RestFrames {
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
        fn build(&mut self, _: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            if let Some(s) = self.scripts.get() {
                self.seen.lock().unwrap().push((
                    s.inactive().is_none(),
                    s.parked(skin_model::ScreenKind::Selection),
                    self.gone.load(std::sync::atomic::Ordering::Acquire),
                ));
            }
            Ok(())
        }
        fn draw(&mut self) -> Result<render::FrameTiming, render::RenderError> {
            Ok(render::FrameTiming {
                cpu: std::time::Duration::from_micros(50),
                sprites: 0,
                dropped: 0,
                presented: true,
                present_count: None,
            })
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, render::RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A play started and ended through the rig (`PlayerRig::start`, which
    /// opens the play's interval, and `play_over`, which closes it after
    /// the tail and Closing), its cell holding a parked selection runtime,
    /// after the parking acknowledgment (`prepare_audio`): every play
    /// frame sees the interval open, the runtime parked with no call made
    /// and its VM alive. With `panic`, the render thread fails during the
    /// play: no VM is destroyed until `play_over`, which retires them with
    /// the interval's token. Without, the runtime is still parked after the
    /// play, and the run's end destroys it (design.md "beatoraja Lua
    /// Skins", Quiescence acknowledged).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_play_leaves_the_screens_runtimes_at_rest_until_its_end() {
        let _serial = crate::imports::tests::serial();
        use game::Rig as _;
        use std::sync::atomic::Ordering;
        for panic in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let settings = gauge_settings(0);
            let loaded = loaded_under(dir.path(), &settings, "tapestry");
            let fixture = crate::selection_skin::runtime_fixture();
            let limits = skin_model::AtlasLimits {
                max_side: 4_096,
                max_layers: 16,
                reserved_layers: render::RESERVED_LAYERS,
            };
            let (_, slot) =
                crate::selection_skin::fixture_scripted(limits, fixture.path(), "rt.luaskin");
            let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
            slot.on_vm_drop(&gone);
            let scripts = Arc::new(std::sync::OnceLock::new());
            let seen = Rests::default();
            let render = {
                let (scripts, gone, seen) =
                    (Arc::clone(&scripts), Arc::clone(&gone), Arc::clone(&seen));
                crate::render_side::tests::spawn_empty(Box::new(move || {
                    Ok(Box::new(RestFrames {
                        scripts: Arc::clone(&scripts),
                        gone: Arc::clone(&gone),
                        seen: Arc::clone(&seen),
                    })
                        as Box<dyn crate::render_side::Screen>)
                }))
            };
            render.load(Arc::new(loaded.chart().clone()), None).unwrap();
            render
                .scripts()
                .commit(skin_model::ScreenKind::Selection, 1, slot);
            let _ = scripts.set(render.scripts().clone());
            let during = Rests::default();
            on_rig_with(settings, render, |rig, settings| {
                rig.prepare_audio().unwrap();
                assert_eq!(
                    rig.prepare_attempt(&loaded).unwrap(),
                    game::Preparation::Ready
                );
                rig.commit_attempt().unwrap();
                let device = settings.device.profile(&[input::DeviceKind::Keyboard], &[]);
                let (_writer, reader) = input::source_queue(8);
                let (_logic, side) = game::Logic::<input::SourceReader>::new(
                    loaded.chart(),
                    loaded.preset(),
                    &device,
                    &eval::InputStart::default(),
                    vec![reader],
                    game::LogicConfig {
                        inputs: 8,
                        effects: 8,
                    },
                    Vec::new(),
                )
                .unwrap();
                let origin = bms_model::TimeSource::now(&rig.clock);
                let (_sink, sounds) = game::audio_link_at(8, origin);
                rig.start(origin, sounds, side);
                assert!(crate::render_side::tests::wait_until(|| seen
                    .lock()
                    .unwrap()
                    .len()
                    >= 3));
                let s = rig.render.scripts().clone();
                if panic {
                    rig.render.send(crate::render_side::RenderMsg::Panic);
                    assert!(crate::render_side::tests::wait_until(|| s.orphaned()));
                    assert!(!gone.load(Ordering::Acquire), "the interval is open");
                }
                // The frames up to the play's end: later ones may follow
                // the close.
                *during.lock().unwrap() = seen.lock().unwrap().clone();
                rig.play_over();
                assert!(s.inactive().is_some(), "the play's end closed the interval");
                if panic {
                    assert!(
                        gone.load(Ordering::Acquire),
                        "retired with the close's token"
                    );
                } else {
                    assert_eq!(
                        s.parked(skin_model::ScreenKind::Selection),
                        Some((0, false))
                    );
                    assert!(!gone.load(Ordering::Acquire));
                }
            });
            let frames = during.lock().unwrap().clone();
            assert!(frames.len() >= 3, "{panic}: {frames:?}");
            assert!(
                frames.iter().all(|&f| f == (true, Some((0, false)), false)),
                "{panic}: {frames:?}"
            );
            assert!(
                gone.load(Ordering::Acquire),
                "{panic}: retired by the run's end"
            );
        }
    }

    /// Settings showing gauge `displayed`.
    #[cfg(feature = "dev-tools")]
    fn gauge_settings(displayed: usize) -> game::Settings {
        game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings {
                displayed_gauge: displayed,
                ..Default::default()
            },
        }
    }

    /// A one-note chart loaded in `dir` with `settings` under the play
    /// preset `preset`.
    #[cfg(feature = "dev-tools")]
    fn loaded_under(
        dir: &std::path::Path,
        settings: &game::Settings,
        preset: &str,
    ) -> game::Loaded {
        let mut settings = settings.clone();
        settings.player.preset = preset.to_owned();
        let root = game::DataRoot::new(dir.join(preset));
        let chart_path = dir.join("a.bms");
        std::fs::write(&chart_path, "#BPM 120\n#00111:01\n").unwrap();
        let config = game::SessionConfig {
            lead_in: bms_model::TimeUs(1_000_000),
            step: bms_model::TimeUs(1_000),
            start: bms_model::TimeUs(0),
            effects: 64,
            seed: 1,
            speed: bms_model::Speed::NORMAL,
            app_version: "0.0.0-test".to_owned(),
            provenance: replay::BuildProvenance::Development,
            receive: replay::ReceiveMethod::Scripted,
            autoplay: None,
        };
        let parts = game::Parts::unsaved(&settings.player);
        game::Loaded::load(root, settings, &chart_path, config, parts).unwrap()
    }

    /// A render thread whose screen notes each play frame's gauge into
    /// `seen`, with `loaded`'s chart.
    #[cfg(feature = "dev-tools")]
    fn gauge_render(seen: &Gauges, loaded: &game::Loaded) -> crate::render_side::RenderLink {
        let noted = Arc::clone(seen);
        let render = crate::render_side::tests::spawn_empty(Box::new(move || {
            Ok(Box::new(GaugeFrames {
                seen: Arc::clone(&noted),
            }) as Box<dyn crate::render_side::Screen>)
        }));
        render.load(Arc::new(loaded.chart().clone()), None).unwrap();
        render
    }

    /// The gauge the settings show stays the settings' across attempts
    /// under different presets on one rig: under tapestry, which has two
    /// gauges, the attempt shows the first for the settings' HAZARD (5);
    /// the next chart's loading gauge under beatoraja-style is still
    /// HAZARD's, and so is that attempt's.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_settings_gauge_survives_a_preset_without_it() {
        use game::Rig as _;
        let dir = tempfile::tempdir().unwrap();
        let settings = gauge_settings(5);
        let short = loaded_under(dir.path(), &settings, "tapestry");
        let long = loaded_under(dir.path(), &settings, "beatoraja-style");
        assert_eq!(short.displayed(), game::GaugeIndex(0));
        assert_eq!(long.displayed(), game::GaugeIndex(5));
        let seen = Gauges::default();
        let render = gauge_render(&seen, &short);
        on_rig_with(settings, render, |rig, _| {
            assert_eq!(
                rig.prepare_attempt(&short).unwrap(),
                game::Preparation::Ready
            );
            assert_eq!(rig.attempt_gauge, 0);
            let preset = rules::beatoraja_style();
            assert!(crate::loading_state::shown_gauge(&preset, 5).is_some());
            assert_eq!(
                crate::play::loading_gauge(&preset, rig.gauge),
                crate::loading_state::shown_gauge(&preset, 5)
            );
            assert_eq!(
                rig.prepare_attempt(&long).unwrap(),
                game::Preparation::Ready
            );
            assert_eq!(rig.attempt_gauge, 5);
        });
    }

    /// The play's frames are drawn with the gauge the attempt resolved
    /// (`game::Loaded::displayed`), sent with the play
    /// (`RenderMsg::Play`), whatever the settings name: the HARD gauge as
    /// set, and a gauge the preset does not have as its first.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_play_frames_show_the_attempts_resolved_gauge() {
        use game::Rig as _;
        for (set, shown) in [(3, 3), (99, 0)] {
            let dir = tempfile::tempdir().unwrap();
            let settings = gauge_settings(set);
            let loaded = loaded_under(dir.path(), &settings, "beatoraja-style");
            assert_eq!(loaded.displayed(), game::GaugeIndex(shown));
            let seen = Gauges::default();
            let render = gauge_render(&seen, &loaded);
            on_rig_with(settings, render, |rig, settings| {
                assert_eq!(
                    rig.prepare_attempt(&loaded).unwrap(),
                    game::Preparation::Ready
                );
                rig.commit_attempt().unwrap();
                let device = settings.device.profile(&[input::DeviceKind::Keyboard], &[]);
                let (_writer, reader) = input::source_queue(8);
                let (_logic, side) = game::Logic::<input::SourceReader>::new(
                    loaded.chart(),
                    loaded.preset(),
                    &device,
                    &eval::InputStart::default(),
                    vec![reader],
                    game::LogicConfig {
                        inputs: 8,
                        effects: 8,
                    },
                    Vec::new(),
                )
                .unwrap();
                let origin = bms_model::TimeSource::now(&rig.clock);
                let (_sink, sounds) = game::audio_link_at(8, origin);
                rig.start(origin, sounds, side);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while seen.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
            let frames = seen.lock().unwrap().clone();
            assert!(frames.len() >= 3, "{set}: {frames:?}");
            assert!(frames.iter().all(|&g| g == shown), "{set}: {frames:?}");
        }
    }

    /// The rig answers the Closing duration the play skin declares from
    /// the attempt's commit receipt: none before the first commit, the
    /// committed skin's after it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_rig_declares_the_committed_skins_close() {
        use game::Rig as _;
        let three = Some(bms_model::TimeUs(3_000_000));
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().join("loaded"));
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let chart_path = dir.path().join("a.bms");
        std::fs::write(&chart_path, "#BPM 120\n#00111:01\n").unwrap();
        let config = game::SessionConfig {
            lead_in: bms_model::TimeUs(1_000_000),
            step: bms_model::TimeUs(1_000),
            start: bms_model::TimeUs(0),
            effects: 64,
            seed: 1,
            speed: bms_model::Speed::NORMAL,
            app_version: "0.0.0-test".to_owned(),
            provenance: replay::BuildProvenance::Development,
            receive: replay::ReceiveMethod::Scripted,
            autoplay: None,
        };
        let loaded = game::Loaded::load(
            root,
            settings.clone(),
            &chart_path,
            config,
            game::Parts::unsaved(&settings.player),
        )
        .unwrap();
        let render = crate::render_side::tests::Installers::default().spawn(
            &Arc::new(loaded.chart().clone()),
            &crate::render_side::tests::bundle_closing(three),
        );
        let (declared, _) = on_rig_with(settings, render, |rig, _| {
            let before = rig.declared_close();
            assert_eq!(
                rig.prepare_attempt(&loaded).unwrap(),
                game::Preparation::Ready
            );
            rig.commit_attempt().unwrap();
            (before, rig.declared_close())
        });
        assert_eq!(declared, (None, three));
    }

    /// Which launcher [`super::launch`] chose for `input` and `options`,
    /// the tuner it handed over applied to a rig of `device`: whether the
    /// rig ended with a start gate.
    #[cfg(feature = "dev-tools")]
    fn launched(
        input: super::PlayInput,
        options: crate::dev::Options,
        device: &game::DeviceSettings,
    ) -> (&'static str, bool) {
        let run = |name, tune: super::Tune| {
            let ((), gated) = on_rig(device.clone(), |rig, _| tune(rig));
            (name, gated)
        };
        super::launch(
            std::path::Path::new("chart.bms"),
            input,
            options,
            QpcClock::start(),
            |_, _, _, _, _, _, tune| run("headless", tune),
            |_, _, _, _, _, tune| run("interactive", tune),
        )
    }

    /// Every launcher's rig runs the start gate with the adjustments'
    /// driver, whatever the development options: production plays are
    /// gated.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn every_launcher_gates_its_plays() {
        use super::PlayInput;
        let device = game::DeviceSettings::default();
        let launched_devices =
            launched(PlayInput::Devices, crate::dev::Options::default(), &device);
        assert_eq!(launched_devices, ("interactive", true));
        let script = PlayInput::Script(crate::dev::ScriptInput {
            log: std::path::PathBuf::from("log.tprl"),
            speed: Vec::new(),
            loading: Vec::new(),
            rearm_delay: bms_model::TimeUs(0),
        });
        assert_eq!(
            launched(script, crate::dev::Options::default(), &device),
            ("headless", true)
        );
    }

    /// While the end screen shows, the game thread sends its page again
    /// when the hi-speed line changes, as when a settings value does, and
    /// not otherwise.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_end_screen_follows_the_hi_speed_line() {
        let (shown, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
            rig.end_page = Some(crate::render_side::EndPage::default());
            let pages = |rig: &crate::play::PlayerRig<'_>| rig.render.end_pages_shown();
            let before = pages(rig);
            // The first look: the line differs from the default one.
            rig.sync();
            let first = pages(rig);
            rig.sync();
            let unchanged = pages(rig);
            // The adjustments changed the line since it was sent.
            rig.shown_speed = crate::end_screen::SpeedLine {
                hispeed_x100: Some(370),
                ..rig.shown_speed
            };
            rig.sync();
            let changed = pages(rig);
            (first - before, unchanged - first, changed - unchanged)
        });
        assert_eq!(shown, (1, 0, 1));
    }

    /// ResultPreparing (design.md "State Model"): the result's input is
    /// taken over as the preparation begins, so a key queued during the
    /// play or its save is dropped there and one pressed after is the
    /// result's; the input offset keys stay refused until the result shows.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_preparation_takes_the_result_input_and_offsets_wait_for_the_result() {
        use crate::select::{MenuInput, MenuKey};
        use game::Rig as _;
        let (seen, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
            let dir = tempfile::tempdir().unwrap();
            let report = crate::end_screen::tests::report(rules::tapestry());
            let screen = crate::end_screen::tests::screen(dir.path(), &report);
            let tuning = Arc::clone(rig.controls.tuning());
            // As a play starts: no screen reads the queues.
            rig.result_input.release();
            rig.result_input.menu.push(MenuInput::Key(MenuKey::Right));
            rig.begin_result(&report, &screen);
            let stale = rig.take_menu();
            rig.result_input.menu.push(MenuInput::Key(MenuKey::Right));
            let fresh = rig.take_menu();
            let during = tuning.adjust(game::Adjust::InputOffsetUp);
            rig.show_end(&report, &screen);
            let shown = tuning.adjust(game::Adjust::InputOffsetUp);
            (stale, fresh, during, shown)
        });
        assert_eq!(
            seen,
            (None, Some(game::MenuCommand::NextPreset), false, true)
        );
    }

    /// A fence the input thread does not answer within its bound fails the
    /// adjustments for good: the failure is the run's input failure, and
    /// nothing more is asked of the driver, whatever the input thread does
    /// after.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_failed_fence_while_loading_ends_the_run() {
        use game::Rig as _;
        let (failed, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
            // The input thread does not run: the entry's fence times out.
            rig.enter_attempt();
            let first = rig.input_failure();
            let asked = rig.adjustments().is_some();
            // A second entry asks nothing and keeps the first failure.
            rig.enter_attempt();
            (first, asked, rig.input_failure())
        });
        assert_eq!(
            failed,
            (
                Some(game::InputWaitError::Timeout),
                false,
                Some(game::InputWaitError::Timeout)
            )
        );
    }

    /// Entering selection is a scene change: it fences the input thread
    /// for the adjustments (here it never answers, so the fence fails and
    /// ends the run, which shows it was asked).
    #[cfg(feature = "dev-tools")]
    #[test]
    fn entering_selection_fences_the_adjustments() {
        use game::AppRig as _;
        use game::Rig as _;
        let (failed, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
            assert_eq!(rig.input_failure(), None);
            rig.entered_selection();
            rig.input_failure()
        });
        assert_eq!(failed, Some(game::InputWaitError::Timeout));
    }

    /// Every scene change and attempt entry moves the window's scene epoch
    /// on with the adjustments' context: selection, the practice page's
    /// loading state, a play's opening. A settings key held across one
    /// repeats no action (the window's guard). Here the input thread never
    /// answers, so the first fence fails and is latched: the epoch moves on
    /// all the same.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn every_scene_change_moves_the_window_scene_on() {
        use game::AppRig as _;
        let (moved, _) = on_rig(game::DeviceSettings::default(), |rig, _| {
            let mut moved = Vec::new();
            let mut last = rig.controls.scene();
            let mut step = |rig: &mut crate::play::PlayerRig<'_>, what: &'static str| {
                let now = rig.controls.scene();
                moved.push((what, now > last));
                last = now;
            };
            rig.entered_selection();
            step(rig, "selection");
            rig.entered_loading(std::path::Path::new("chart.bms"));
            step(rig, "loading");
            moved
        });
        assert_eq!(moved, [("selection", true), ("loading", true)]);
    }
}
