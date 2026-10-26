//! The game executable.
//!
//! `tapestry play <chart.bms>` loads the settings and the chart, opens the
//! audio device and loads the sound bank, starts the input thread, then
//! opens a borderless fullscreen window whose render thread prepares the
//! GPU resources, and plays the chart until the player quits. The data
//! root is `%LOCALAPPDATA%\tapestry\` (D10).

#[cfg(not(windows))]
compile_error!("player is Windows-only");

mod audio_side;
mod controls;
#[cfg(feature = "dev-tools")]
mod dev;
mod end_screen;
mod input_side;
mod play;
mod power;
mod render_side;
mod window;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use audio::{AudioEngine, BankLimits, MAX_VOICES, load_bank};
use bms_model::TimeUs;
use game::{DataRoot, DeviceSettings, Loaded, Parts, SessionConfig, Settings, save_settings};
use input::{Capture, InputThread, QpcClock, SourceConfig, SourceSet};
use render::{AdapterChoice, Renderer, Target};
use replay::{BuildProvenance, ReceiveMethod};

use crate::audio_side::AudioMsg;
use crate::controls::Controls;
use crate::input_side::Mode;
use crate::play::{AfterEnd, Threads};
use crate::render_side::{NoFrames, View};

const USAGE: &str = "usage: tapestry play <chart.bms> | tapestry --version";

/// The countdown before chart time 0 (D13).
const LEAD_IN: TimeUs = TimeUs(1_000_000);
/// The visible time at hi-speed 1.0 (D9; tuned by playing).
const VISIBLE_TIME_US: i64 = 600_000;
/// Effects the render queue holds.
const EFFECTS: usize = 1 << 14;

fn main() -> ExitCode {
    let started = Instant::now();
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
        return report(scripted(&chart, &log, dev_options.frames, started));
    }
    report(play(&chart, started))
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
fn sources() -> SourceSet {
    SourceSet::new(SourceConfig {
        max_devices: 16,
        max_sources: 8,
        max_held: 32,
        queue_capacity: 4_096,
        max_arrivals: 16,
    })
    .expect("8 sources are within the limit")
}

/// Loads the bank at the output's rate and hands the engine to the audio
/// thread.
fn start_engine(
    loaded: &Loaded,
    audio: &audio_side::AudioLink,
    opened: audio_side::Opened,
) -> Result<(), String> {
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
        0,
    )
    .map_err(|e| format!("the audio engine cannot start: {e}"))?;
    loaded.settings().device.apply_to(&mut engine);
    audio.send(AudioMsg::Engine(Box::new(engine)));
    Ok(())
}

/// The display offset and the visible time from the settings.
fn view(loaded: &Loaded) -> View {
    let s = loaded.settings();
    let hi_speed = i64::from(s.player.hi_speed_milli.max(1));
    View {
        display_offset: TimeUs(s.device.display_offset_us),
        visible_time: TimeUs(VISIBLE_TIME_US * 1_000 / hi_speed),
        effects: EFFECTS,
    }
}

/// The game: Raw Input, WASAPI and a window.
fn play(chart: &Path, started: Instant) -> Result<(), String> {
    let mut loaded = load(chart, ReceiveMethod::RawInput)?;
    let clock = QpcClock::start();
    let controls = Arc::new(Controls::new());
    let _suspend = power::SuspendWatch::register(Arc::clone(&controls));
    let (audio, opened) = audio_side::spawn(
        move || audio::WasapiShared::open(clock).map_err(|e| format!("audio: {e}")),
        |_| {},
    )?;
    start_engine(&loaded, &audio, opened)?;
    let sources = sources();
    controls.set_latch(sources.gate().handle());
    let (mut hook, input) = input_side::link(Arc::clone(&controls), Mode::Clock);
    let input_thread = InputThread::spawn(clock, sources, Capture::new(), move |s, now| {
        hook.wake(s, now);
    })
    .map_err(|e| format!("input: {e}"))?;
    let keys = &loaded.settings().device.keyboard;
    let (window_thread, window) = window::open(Arc::clone(&controls), keys.retry, keys.quit)?;
    let size = window.inner_size();
    let target = Target::Window {
        window: window.into(),
        width: size.width,
        height: size.height,
    };
    let render = render_side::spawn(
        move || Renderer::new(target, AdapterChoice::HighPerformance).map_err(|e| e.to_string()),
        Arc::new(loaded.chart().clone()),
        clock,
        view(&loaded),
        Box::new(NoFrames),
    );
    let render = match render {
        Ok(r) => r,
        Err(e) => {
            window_thread.close();
            return Err(e);
        }
    };
    let threads = Threads {
        input,
        audio,
        render,
    };
    play::run(
        &mut loaded,
        &threads,
        &controls,
        clock,
        started,
        AfterEnd::Wait,
    );
    threads.shut_down();
    window_thread.close();
    drop(input_thread);
    Ok(())
}

/// A scripted run (dev-tools): the input log as input, a recording of a
/// simulated device as output, WARP offscreen as the screen.
#[cfg(feature = "dev-tools")]
fn scripted(
    chart: &Path,
    log: &Path,
    frames: Option<PathBuf>,
    started: Instant,
) -> Result<(), String> {
    let script = dev::script(log)?;
    let mut loaded = load(chart, ReceiveMethod::Scripted)?;
    let clock = QpcClock::start();
    let controls = Arc::new(Controls::new());
    let last_note = loaded
        .chart()
        .lanes
        .iter()
        .flatten()
        .map(|n| n.time)
        .max()
        .unwrap_or(TimeUs(0));
    let (audio, opened) = audio_side::spawn(
        move || Ok(dev::recording_output(clock, 60)),
        dev::report_audio,
    )?;
    start_engine(&loaded, &audio, opened)?;
    let sources = sources();
    controls.set_latch(sources.gate().handle());
    let (hook, input) = input_side::link(Arc::clone(&controls), Mode::Script(script));
    let script_thread = dev::ScriptThread::spawn(clock, sources, hook)?;
    let dump: Box<dyn render_side::FrameSink> = match frames {
        Some(dir) => Box::new(dev::FrameDump::new(dir, TimeUs(last_note.0 / 2))),
        None => Box::new(NoFrames),
    };
    let render = render_side::spawn(
        || {
            Renderer::new(
                Target::Offscreen {
                    width: 1280,
                    height: 720,
                },
                AdapterChoice::Warp,
            )
            .map_err(|e| e.to_string())
        },
        Arc::new(loaded.chart().clone()),
        clock,
        view(&loaded),
        dump,
    )?;
    let threads = Threads {
        input,
        audio,
        render,
    };
    play::run(
        &mut loaded,
        &threads,
        &controls,
        clock,
        started,
        AfterEnd::QuitAfter(std::time::Duration::from_millis(300)),
    );
    threads.shut_down();
    drop(script_thread);
    Ok(())
}
