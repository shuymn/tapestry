//! The game executable.
//!
//! `tapestry play <chart.bms>` loads the settings and the chart, opens the
//! audio device and loads the sound bank, starts the input thread, then
//! opens a borderless fullscreen window whose render thread prepares the
//! GPU resources, and plays the chart until the player quits. The data
//! root is `%LOCALAPPDATA%\tapestry\` (D10).

#[cfg(not(windows))]
compile_error!("player is Windows-only");

#[cfg(feature = "dev-tools")]
mod alloc_count;
mod audio_side;
mod controls;
#[cfg(feature = "dev-tools")]
mod dev;
mod end_screen;
mod input_side;
mod keys;
mod measure;
mod play;
mod power;
#[cfg(feature = "dev-tools")]
mod presentmon;
mod render_side;
mod tuning;
mod window;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::SystemTime;

use audio::{AudioEngine, BankLimits, MAX_VOICES, load_bank};
use bms_model::TimeUs;
use game::{
    DataRoot, DeviceSettings, Loaded, Parts, SessionConfig, Settings, Tuning, save_settings,
};
use input::{Capture, InputThread, QpcClock, SourceConfig, SourceSet};
use render::{AdapterChoice, Renderer, Target};
use replay::{BuildProvenance, ReceiveMethod};

use crate::controls::Controls;
use crate::end_screen::{Hints, LoadTimes};
use crate::input_side::Mode;
use crate::measure::{AudioInfo, Measure, SOUND_QUEUE, STEP_QUEUE, StepSample};
use crate::play::PlayerRig;
use crate::render_side::{MakeScreen, NoFrames, Screen, View};
use crate::tuning::{Delivered, LiveTuning, SettingsFiles};

const USAGE: &str = "usage: tapestry play <chart.bms> | tapestry keys | tapestry --version";

/// The countdown before chart time 0 (D13).
const LEAD_IN: TimeUs = TimeUs(1_000_000);
/// Effects the render queue holds.
const EFFECTS: usize = 1 << 14;
/// Scheduled sound starts the audio engine keeps between two drains (one
/// engine step).
const SCHEDULE_LOG: usize = 256;

fn main() -> ExitCode {
    // The clock reads 0 at the start of the process: load times count from
    // here.
    let clock = QpcClock::start();
    let mut args = std::env::args_os().skip(1);
    let Some(arg) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    #[cfg(feature = "dev-tools")]
    if let Some(code) = dev::handle_arg(&arg) {
        return match args.next() {
            Some(extra) => unknown_argument(&extra),
            None => code,
        };
    }

    if arg == "--version" {
        if let Some(extra) = args.next() {
            return unknown_argument(&extra);
        }
        println!("tapestry {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if arg == "keys" {
        return keys_command(args, clock);
    }
    #[cfg(feature = "dev-tools")]
    if arg == "measure" {
        return measure_command(args, clock);
    }
    if arg != "play" {
        return unknown_argument(&arg);
    }
    let mut chart = None;
    #[cfg(feature = "dev-tools")]
    let mut dev_options = dev::Options::default();
    // Development options take their values from `args`.
    #[cfg_attr(not(feature = "dev-tools"), allow(clippy::while_let_on_iterator))]
    while let Some(a) = args.next() {
        #[cfg(feature = "dev-tools")]
        match dev_options.take(&a, &mut args) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
        if chart.is_some() || a.to_string_lossy().starts_with("--") {
            return unknown_argument(&a);
        }
        chart = Some(PathBuf::from(a));
    }
    let Some(chart) = chart else {
        eprintln!("play needs a chart file\n{USAGE}");
        return ExitCode::from(2);
    };
    #[cfg(feature = "dev-tools")]
    if let Some(log) = dev_options.scripted_input.take() {
        let auto = u32::from(dev_options.auto_retry);
        let frames = dev_options.frames.take();
        return report(scripted(&chart, &log, frames, auto, clock, |rig| {
            if dev_options.overlay {
                rig.controls.toggle_overlay();
            }
            rig.adjust_in_play = dev_options.adjust_in_play;
            rig.adjust_at = TimeUs(dev_options.adjust_at_ms.saturating_mul(1_000));
            rig.adjust_on_end = dev_options.adjust_on_end;
        }));
    }
    report(play(&chart, clock, |_rig| {
        #[cfg(feature = "dev-tools")]
        {
            if dev_options.auto_retry {
                _rig.auto_retries = 1;
                _rig.quit_after_end = Some(std::time::Duration::from_millis(300));
            }
            if dev_options.overlay {
                _rig.controls.toggle_overlay();
            }
            _rig.adjust_in_play = dev_options.adjust_in_play;
            _rig.adjust_at = TimeUs(dev_options.adjust_at_ms.saturating_mul(1_000));
            _rig.adjust_on_end = dev_options.adjust_on_end;
        }
    }))
}

/// `tapestry measure play <chart>` (dev-tools): one play under a
/// PresentMon capture, its counts added to each saved play's
/// `.measure.toml`; the run quits after the end screen shows (retry and
/// quit still work during the play).
#[cfg(feature = "dev-tools")]
fn measure_command(mut args: impl Iterator<Item = OsString>, clock: QpcClock) -> ExitCode {
    const MEASURE_USAGE: &str = "usage: tapestry measure play <chart.bms>";
    match args.next() {
        Some(a) if a == "play" => {}
        Some(a) => return unknown_argument(&a),
        None => {
            eprintln!("{MEASURE_USAGE}");
            return ExitCode::from(2);
        }
    }
    let Some(chart) = args.next() else {
        eprintln!("measure play needs a chart file\n{MEASURE_USAGE}");
        return ExitCode::from(2);
    };
    if let Some(extra) = args.next() {
        return unknown_argument(&extra);
    }
    report(measure_play(Path::new(&chart), clock))
}

#[cfg(feature = "dev-tools")]
fn measure_play(chart: &Path, clock: QpcClock) -> Result<(), String> {
    let exe = presentmon::locate(std::env::var_os(presentmon::ENV), std::env::var_os("PATH"))?;
    let process = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .ok_or("the game's own executable name is unknown")?;
    let dir = data_root()?.path().join("measure");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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
    let plays = play::MeasuredPlays::default();
    let kept = Arc::clone(&plays);
    // One play: the run quits once its end screen has shown.
    let ran = play(chart, clock, move |rig| {
        rig.measured_plays = Some(kept);
        rig.quit_after_end = Some(std::time::Duration::from_millis(300));
    });
    let stopped = capture.stop();
    ran?;
    stopped?;
    let text = std::fs::read_to_string(&csv).map_err(|e| format!("{}: {e}", csv.display()))?;
    let (frames, error) = presentmon::parse(&text, &process, |t| clock.from_qpc(t).0);
    if let Some(e) = error {
        return Err(format!("{}: {e:?}", csv.display()));
    }
    let plays = plays.lock().map_err(|_| "the measured plays were lost")?;
    for (record, fig) in plays.iter() {
        let Some(period) = fig.refresh_period_ns else {
            eprintln!("measure: no refresh rate for {}", record.display());
            continue;
        };
        let counted = presentmon::count_play(&frames, fig, period);
        println!(
            "measure: {}: presentmon {:?}, {} presents, {} never displayed",
            record.display(),
            counted.count,
            counted.presents,
            counted.never_displayed
        );
        let path = measure::measure_path(record);
        let mut toml = std::fs::read_to_string(&path).unwrap_or_default();
        toml.push_str(&presentmon::to_toml(&counted, fig, period));
        game::write_atomic(&path, toml.as_bytes())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

/// `tapestry keys`, with the dev-tools option `--dev-scripted-keys <steps>`.
fn keys_command(args: impl Iterator<Item = OsString>, clock: QpcClock) -> ExitCode {
    let mut args = args.peekable();
    #[cfg(feature = "dev-tools")]
    let mut script = None;
    #[cfg(feature = "dev-tools")]
    while args.peek().is_some_and(|a| a == "--dev-scripted-keys") {
        args.next();
        let parsed = args
            .next()
            .ok_or_else(|| "--dev-scripted-keys needs a value".to_owned())
            .and_then(|v| dev::KeyScript::parse(&v.to_string_lossy()));
        match parsed {
            Ok(s) => script = Some(s),
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        }
    }
    if let Some(a) = args.next() {
        return unknown_argument(&a);
    }
    report(keys::run(
        clock,
        #[cfg(feature = "dev-tools")]
        script,
    ))
}

fn unknown_argument(arg: &OsString) -> ExitCode {
    eprintln!("unknown argument: {}", arg.to_string_lossy());
    ExitCode::from(2)
}

fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

/// `%LOCALAPPDATA%\tapestry\`.
fn data_root() -> Result<DataRoot, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .ok_or("LOCALAPPDATA is not set, so the data folder cannot be found")?;
    Ok(DataRoot::new(PathBuf::from(base).join("tapestry")))
}

/// The settings, writing `device.toml` with the default key config on the
/// first start.
fn settings(root: &DataRoot) -> Result<Settings, String> {
    let device = root.device_settings();
    if !device.exists() {
        save_settings(&device, &DeviceSettings::default())
            .map_err(|e| format!("{}: {e}", device.display()))?;
    }
    Settings::load(root).map_err(|e| format!("{}: {e}", root.path().display()))
}

/// Settings, the chart through ingestion and the transform, and the
/// session's fixed choices.
fn load(chart: &Path, receive: ReceiveMethod) -> Result<Loaded, String> {
    let root = data_root()?;
    let settings = settings(&root)?;
    // `#RANDOM` is resolved with a seed that differs per run; it is
    // recorded.
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let config = SessionConfig {
        lead_in: LEAD_IN,
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: EFFECTS,
        seed,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        provenance: if cfg!(feature = "dev-tools") {
            BuildProvenance::Development
        } else {
            BuildProvenance::Production
        },
        receive,
    };
    let parts = Parts::standard(&root, &settings.player);
    let loaded = Loaded::load(root, settings, chart, config, parts)
        .map_err(|e| format!("{}: {e}", chart.display()))?;
    for w in loaded.warnings() {
        eprintln!("warning: {w}");
    }
    Ok(loaded)
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

/// Loads the bank at the output's rate and hands the engine to the audio
/// thread, which keeps it silent until the first play.
fn start_engine(
    loaded: &Loaded,
    audio: audio_side::AudioThread,
    opened: audio_side::Opened,
    probe: rtrb::Producer<game::AudioProbe>,
) -> Result<audio_side::AudioLink, String> {
    let chart = loaded.chart();
    let bank = load_bank(chart, opened.rate, &BankLimits::default())
        .map_err(|e| format!("the sounds cannot be loaded: {e}"))?;
    for w in bank.warnings() {
        eprintln!("warning: {w:?}");
    }
    let mut engine = AudioEngine::new(
        Arc::new(bank),
        &chart.bgm,
        opened.rate,
        opened.period,
        MAX_VOICES,
        SCHEDULE_LOG,
    )
    .map_err(|e| format!("the audio engine cannot start: {e}"))?;
    loaded.settings().device.apply_to(&mut engine);
    Ok(audio.start(engine, probe))
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

/// The frames' inputs: hi-speed and the display offset as the settings
/// keys leave them.
fn view(controls: &Controls) -> View {
    View {
        tuning: Arc::clone(controls.tuning()),
        effects: EFFECTS,
    }
}

/// The controls, with the settings keys starting from the loaded values.
fn controls(loaded: &Loaded, clock: QpcClock) -> Arc<Controls> {
    let s = loaded.settings();
    let tuning = LiveTuning::new(Tuning::of(s)).with_loaded_volume(s.device.master_volume);
    Arc::new(Controls::with_tuning(clock, Arc::new(tuning)))
}

/// The game thread's rig around the started threads.
fn rig<'a>(
    loaded: &Loaded,
    input: input_side::InputLink,
    audio: audio_side::AudioLink,
    render: render_side::RenderLink,
    controls: &'a Controls,
    clock: QpcClock,
    steps: rtrb::Producer<StepSample>,
) -> PlayerRig<'a> {
    let device = &loaded.settings().device;
    PlayerRig {
        input,
        audio,
        render,
        controls,
        clock,
        recorded: (device.keyboard.recorded(), device.controller.recorded()),
        gauge: loaded.settings().player.displayed_gauge,
        input_offset_us: device.input_offset_us,
        files: SettingsFiles::new(loaded.root().clone(), loaded.settings().clone()),
        seen: Delivered::default(),
        end_lines: None,
        hints: Hints::new(device.keyboard.retry, device.keyboard.quit),
        pending_retry: None,
        retry_from: None,
        times: LoadTimes::default(),
        countdown_at: None,
        steps,
        steps_dropped: 0,
        generation: 0,
        #[cfg(feature = "dev-tools")]
        measured_plays: None,
        lead_in: loaded.config().lead_in,
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
fn open_window_and_input(
    controls: &Arc<Controls>,
    clock: QpcClock,
    sources: SourceSet,
    capture: Capture,
    wake: impl FnMut(&mut SourceSet, TimeUs) + Send + 'static,
    keys: (Option<u32>, Option<u32>),
) -> Result<(window::WindowThread, window::Surface, InputThread), String> {
    let (window_thread, surface) = window::open(Arc::clone(controls), keys.0, keys.1)?;
    let input = InputThread::spawn(clock, sources, capture, wake);
    let input = match input {
        Ok(i) => i,
        Err(e) => {
            window_thread.close();
            return Err(format!("input: {e}"));
        }
    };
    if input::keyboard_target() != Some(input.receiver()) {
        window_thread.close();
        return Err("input: keyboards are not registered to the input thread".to_owned());
    }
    println!("input: keyboard Raw Input targets the input thread's window");
    Ok((window_thread, surface, input))
}

/// The game: Raw Input, WASAPI and a window.
fn play(
    chart: &Path,
    clock: QpcClock,
    tune: impl FnOnce(&mut PlayerRig<'_>),
) -> Result<(), String> {
    #[cfg(feature = "dev-tools")]
    alloc_count::tag(alloc_count::Tag::Logic);
    let mut loaded = load(chart, ReceiveMethod::RawInput)?;
    let controls = controls(&loaded, clock);
    let _suspend = power::SuspendWatch::register(Arc::clone(&controls))?;
    let (audio, opened) = audio_side::spawn(
        move || {
            let out = audio::WasapiShared::open(clock).map_err(|e| format!("audio: {e}"))?;
            let queued = out.grant().queue_periods;
            Ok((out, Some(queued)))
        },
        |_| {},
    )?;
    let probes = Probes::new();
    let audio = start_engine(&loaded, audio, opened, probes.sounds.0)?;
    let sources = sources();
    controls.set_latch(sources.gate().handle());
    let (mut hook, input) = input_side::link(Arc::clone(&controls), Mode::Clock);
    let keys = &loaded.settings().device.keyboard;
    let keys = (keys.retry, keys.quit);
    let (window_thread, surface, input_thread) = match open_window_and_input(
        &controls,
        clock,
        sources,
        Capture::new(),
        move |s, now| {
            #[cfg(feature = "dev-tools")]
            alloc_count::tag(alloc_count::Tag::Input);
            hook.wake(s, now);
        },
        keys,
    ) {
        Ok(opened) => opened,
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
            .map(|r| Box::new(r) as Box<dyn Screen>)
            .map_err(|e| e.to_string())
    });
    let measure = measure(
        clock,
        surface.refresh_mhz,
        opened,
        probes.steps.1,
        probes.sounds.1,
        &controls,
    );
    let render = render_side::spawn(
        make,
        Arc::new(loaded.chart().clone()),
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
            return Err(e);
        }
    };
    let mut rig = rig(
        &loaded,
        input,
        audio,
        render,
        &controls,
        clock,
        probes.steps.0,
    );
    tune(&mut rig);
    let ran = game::live::run(&mut loaded, &mut rig).map_err(|e| e.to_string());
    rig.shut_down();
    window_thread.close();
    drop(input_thread);
    ran
}

/// A scripted run (dev-tools): the input log as input, a recording of a
/// simulated device as output, WARP offscreen as the screen.
#[cfg(feature = "dev-tools")]
fn scripted(
    chart: &Path,
    log: &Path,
    frames: Option<PathBuf>,
    auto_retries: u32,
    clock: QpcClock,
    tune: impl FnOnce(&mut PlayerRig<'_>),
) -> Result<(), String> {
    alloc_count::tag(alloc_count::Tag::Logic);
    let script = dev::script(log)?;
    let mut loaded = load(chart, ReceiveMethod::Scripted)?;
    let controls = controls(&loaded, clock);
    let last_note = loaded
        .chart()
        .lanes
        .iter()
        .flatten()
        .map(|n| n.time)
        .max()
        .unwrap_or(TimeUs(0));
    let (audio, opened) = audio_side::spawn(
        move || {
            let out = dev::recording_output(clock, 60);
            let queued = out.queued_periods();
            Ok((out, Some(queued)))
        },
        dev::report_audio,
    )?;
    let probes = Probes::new();
    let audio = start_engine(&loaded, audio, opened, probes.sounds.0)?;
    let sources = sources();
    controls.set_latch(sources.gate().handle());
    let script = input_side::Script {
        log: script.clone(),
        current: script,
        sources: source_config(),
    };
    let (hook, input) = input_side::link(Arc::clone(&controls), Mode::Script(Box::new(script)));
    let script_thread = dev::ScriptThread::spawn(clock, sources, hook)?;
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
            .map(|r| Box::new(r) as Box<dyn Screen>)
            .map_err(|e| e.to_string())
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
        Arc::new(loaded.chart().clone()),
        clock,
        view(&controls),
        measure,
        dump,
    )?;
    let mut rig = rig(
        &loaded,
        input,
        audio,
        render,
        &controls,
        clock,
        probes.steps.0,
    );
    rig.quit_after_end = Some(std::time::Duration::from_millis(300));
    rig.auto_retries = auto_retries;
    tune(&mut rig);
    let ran = game::live::run(&mut loaded, &mut rig).map_err(|e| e.to_string());
    rig.shut_down();
    drop(script_thread);
    ran
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use input::QpcClock;

    use super::{Controls, Mode, input_side, open_window_and_input, sources};

    /// The real startup order leaves the keyboards registered to the input
    /// thread. Opening the input thread before the window fails it: winit
    /// takes the registration and then removes it.
    #[test]
    #[ignore = "opens a window: needs an interactive desktop"]
    fn keyboards_stay_registered_to_the_input_thread() {
        let clock = QpcClock::start();
        let controls = Arc::new(Controls::new(clock));
        let (mut hook, _link) = input_side::link(Arc::clone(&controls), Mode::Clock);
        let (window, _surface, input) = open_window_and_input(
            &controls,
            clock,
            sources(),
            input::Capture::new(),
            move |s, now| hook.wake(s, now),
            (Some(0x0E), Some(0x01)),
        )
        .expect("startup");
        assert_eq!(input::keyboard_target(), Some(input.receiver()));
        drop(input);
        window.close();
    }
}
