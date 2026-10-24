//! `measure`: a windowed run under PresentMon, optionally with OBS
//! streaming (design §4, §5 P4).
//!
//! 1. Conditions go to `<out>/private/conditions.json`.
//! 2. PresentMon 2.6 captures this process by name into
//!    `<out>/private/presentmon.csv` (`--qpc_time` for the run clock,
//!    `--track_hybrid_present`). It runs unelevated (the logon is in
//!    Performance Log Users). `--terminate_on_proc_exit` does not fire
//!    unelevated, so the session is stopped explicitly after the run with
//!    `--terminate_existing_session --session_name tapestry_spike`.
//! 3. With `--obs`, OBS starts streaming with the bundle's profile and
//!    scene collection; after the run the owner stops streaming and exits
//!    OBS, and the newest OBS log is copied to `<out>/private/obs.log`
//!    (its final `Number of memory leaks:` line confirms teardown, V7).
//! 4. `report <out>` then evaluates and exports.
//!
//! `--dry-run` prints the commands without starting anything.

use crate::run::RunArgs;
use crate::{conditions, parse_run, window};
use spike_core::run::{EncoderClass, RunKind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub const SESSION: &str = "tapestry_spike";
const PROCESS: &str = "tapestry-spike.exe";
/// PresentMon needs a moment to start its ETW session.
const PRESENTMON_SETTLE: Duration = Duration::from_secs(3);
/// OBS is given this long to connect before the run starts (the run's
/// countdown then puts play within V7's 0–30 s lead).
const OBS_SETTLE: Duration = Duration::from_secs(8);
/// OBS countdown default (design §4.2: 20 s).
const OBS_COUNTDOWN_US: i64 = 20_000_000;

struct Measure {
    presentmon: PathBuf,
    obs: Option<PathBuf>,
    obs_profile: String,
    obs_collection: String,
    dry_run: bool,
}

pub fn presentmon_args(csv: &Path) -> Vec<String> {
    [
        "--process_name",
        PROCESS,
        "--output_file",
        &csv.to_string_lossy(),
        "--qpc_time",
        "--track_hybrid_present",
        "--session_name",
        SESSION,
        "--stop_existing_session",
        "--terminate_on_proc_exit",
        "--no_console_stats",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub fn obs_args(profile: &str, collection: &str) -> Vec<String> {
    [
        "--profile",
        profile,
        "--collection",
        collection,
        "--startstreaming",
        "--minimize-to-tray",
        "--disable-shutdown-check",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn show(exe: &Path, args: &[String]) -> String {
    format!("\"{}\" {}", exe.display(), args.join(" "))
}

/// Stops the PresentMon session and waits for the capture process.
fn stop_presentmon(exe: &Path, child: &mut Child) -> Result<(), String> {
    let status = Command::new(exe)
        .args(["--terminate_existing_session", "--session_name", SESSION])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("PresentMon terminate: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("PresentMon did not exit after its session was terminated".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !status.success() {
        eprintln!("measure: PresentMon --terminate_existing_session exited with {status}");
    }
    Ok(())
}

/// The newest OBS log (`%APPDATA%\obs-studio\logs`).
fn newest_obs_log() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?)
        .join("obs-studio")
        .join("logs");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

pub fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut m = Measure {
        presentmon: PathBuf::new(),
        obs: None,
        obs_profile: "tapestry-spike-hardware".into(),
        obs_collection: "tapestry-spike".into(),
        dry_run: false,
    };
    let mut encoder = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => m.dry_run = true,
            "--presentmon" | "--obs" | "--obs-profile" | "--obs-collection" | "--obs-encoder" => {
                let v = it.next().ok_or(format!("{a} needs a value"))?.clone();
                match a.as_str() {
                    "--presentmon" => m.presentmon = PathBuf::from(v),
                    "--obs" => m.obs = Some(PathBuf::from(v)),
                    "--obs-profile" => m.obs_profile = v,
                    "--obs-collection" => m.obs_collection = v,
                    _ => {
                        encoder = Some(match v.as_str() {
                            "hardware" => EncoderClass::Hardware,
                            "x264" => EncoderClass::X264,
                            _ => return Err(format!("--obs-encoder {v}: hardware or x264")),
                        })
                    }
                }
            }
            _ => rest.push(a.clone()),
        }
    }
    if m.presentmon.as_os_str().is_empty() {
        return Err("measure needs --presentmon <exe>".into());
    }
    let mut a: RunArgs = parse_run(&rest)?;
    if a.offscreen.is_some() {
        return Err("measure runs on a display, not offscreen".into());
    }
    let required = a.duration_s.is_none() && a.loops == spike_core::evaluate::REQUIRED_LOOPS;
    a.meta.kind = match (&m.obs, required) {
        (Some(_), _) => RunKind::Obs,
        (None, true) => RunKind::Baseline,
        (None, false) => RunKind::Diagnostic,
    };
    a.meta.obs_encoder = encoder;
    if m.obs.is_some() && !rest.iter().any(|x| x == "--countdown-s") {
        a.countdown_us = OBS_COUNTDOWN_US;
    }
    let csv = a.out.join(crate::report::PRESENTMON_CSV);
    let pm_args = presentmon_args(&csv);
    let obs = m
        .obs
        .as_ref()
        .map(|exe| (exe.clone(), obs_args(&m.obs_profile, &m.obs_collection)));
    if m.dry_run {
        println!(
            "measure (dry run): kind {:?}, countdown {} s",
            a.meta.kind,
            a.countdown_us / 1_000_000
        );
        println!("1. {}", show(&m.presentmon, &pm_args));
        if let Some((exe, args)) = &obs {
            println!(
                "2. (working directory {}) {}",
                exe.parent()
                    .map_or(String::new(), |p| p.display().to_string()),
                show(exe, args)
            );
            println!("   then wait {} s for the connection", OBS_SETTLE.as_secs());
        }
        println!("3. the run: {}", rest.join(" "));
        println!(
            "4. {}",
            show(
                &m.presentmon,
                &[
                    "--terminate_existing_session".into(),
                    "--session_name".into(),
                    SESSION.into()
                ]
            )
        );
        if obs.is_some() {
            println!(
                "5. owner stops streaming and exits OBS; the newest OBS log is copied to private/obs.log"
            );
        }
        println!("then: tapestry-spike report {}", a.out.display());
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::create_dir_all(a.out.join("private")).map_err(|e| e.to_string())?;
    let cond = conditions::collect(
        a.audio.as_ref().and_then(|x| x.device.as_deref()),
        a.bga.as_deref(),
    );
    std::fs::write(
        a.out.join("private").join(conditions::CONDITIONS),
        serde_json::to_vec_pretty(&cond).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let log = std::fs::File::create(a.out.join("private").join("presentmon.log"))
        .map_err(|e| e.to_string())?;
    let mut pm = Command::new(&m.presentmon)
        .args(&pm_args)
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("start PresentMon {}: {e}", m.presentmon.display()))?;
    std::thread::sleep(PRESENTMON_SETTLE);
    let mut obs_child = None;
    if let Some((exe, args)) = &obs {
        let dir = exe.parent().unwrap_or(Path::new("."));
        obs_child = Some(
            Command::new(exe)
                .args(args)
                .current_dir(dir)
                .spawn()
                .map_err(|e| format!("start OBS: {e}"))?,
        );
        std::thread::sleep(OBS_SETTLE);
    }
    let result = window::windowed(&a);
    let stopped = stop_presentmon(&m.presentmon, &mut pm);
    if let Some(mut child) = obs_child {
        println!(
            "measure: stop streaming and exit OBS now (File > Exit); waiting for OBS to close..."
        );
        let _ = child.wait();
        match newest_obs_log() {
            Some(p) => {
                std::fs::copy(&p, a.out.join(crate::report::OBS_LOG))
                    .map_err(|e| format!("copy OBS log: {e}"))?;
                println!("measure: OBS log collected");
            }
            None => eprintln!("measure: no OBS log found"),
        }
    }
    stopped?;
    let code = result?;
    println!(
        "measure: capture in {}; next: tapestry-spike report {}",
        csv.display(),
        a.out.display()
    );
    Ok(code)
}
