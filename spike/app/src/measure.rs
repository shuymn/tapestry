//! `measure`: a windowed run under PresentMon, optionally with OBS
//! streaming to the bundle's local ingest (design §4, §5 P4).
//!
//! 1. Conditions go to `<out>/private/conditions.json`, with the PresentMon
//!    version and the owner-declared GPU power setting (`--gpu-power-mode`;
//!    the per-program driver setting is not readable without vendor APIs).
//! 2. Helper processes start, each owned by a `Tool` that stops it however
//!    `measure` ends (error, early return or panic) and that fails fast if
//!    the process exits during its start-up check:
//!    - With `--ingest`, MediaMTX on 127.0.0.1:1935. The port must be free
//!      first (another instance is never touched), and the listener must
//!      come up; only the process started here is stopped.
//!    - `nvidia-smi` samples P-state and clocks every 100 ms into
//!      `<out>/private/nvidia-smi.csv` when an NVIDIA adapter is present
//!      (`report` keeps the run's adapter, during play).
//!    - PresentMon 2.6 captures this process by name into
//!      `<out>/private/presentmon.csv` (`--qpc_time`,
//!      `--track_hybrid_present`). It runs unelevated (Performance Log
//!      Users); `--terminate_on_proc_exit` does not fire unelevated, so the
//!      session is stopped with `--terminate_existing_session`.
//!
//!    The effective power mode is watched over the run
//!    (`<out>/private/power_modes.csv`).
//! 3. With `--obs`, OBS starts streaming with the bundle's profile and
//!    scene collection; after the run the owner stops streaming and exits
//!    OBS, and the newest OBS log is copied to `<out>/private/obs.log`.
//!    `--ingest-check` also reads the ingest for 10 s during play (game
//!    video and audio present; the run is then diagnostic).
//! 4. `report <out>` then evaluates and exports.
//!
//! `--unattended` (design §4.3): nobody is at the PC. OBS starts in the
//! tray as in attended runs (no OBS window that could take the focus);
//! after the run measure stops the stream through obs-websocket and
//! closes OBS's hidden main window itself (never a kill; if the
//! WebSocket is unreachable OBS is left running for the owner and measure
//! fails), and MediaMTX records the ingest to `<out>/private/ingest/` for
//! the owner's later review. The run is marked unattended, so its V7 owner
//! confirmation is pending until the owner notes answer it.
//!
//! `--dry-run` prints the commands without starting anything.

use crate::run::RunArgs;
use crate::{conditions, parse_run, window};
use serde_json::json;
use spike_core::conditions::NVIDIA_VENDOR_ID;
use spike_core::run::{EncoderClass, RunKind};
use std::net::{SocketAddr, TcpListener, TcpStream};
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
/// The local ingest's RTMP listener (obs/mediamtx.yml, `obs::SERVER`).
pub const INGEST_ADDR: &str = "127.0.0.1:1935";
const INGEST_SETTLE: Duration = Duration::from_secs(5);
/// Values of the driver's per-program power management setting the owner
/// may declare (`--gpu-power-mode`).
pub const GPU_POWER_MODES: &[&str] = &[
    "default",
    "normal",
    "optimal-power",
    "adaptive",
    "prefer-max-performance",
    "prefer-consistent-performance",
];
/// The bus id stays in the private samples only: it tells several GPUs of
/// one model apart (`report` then records the state as ambiguous).
const SMI_QUERY: &str = "--query-gpu=timestamp,name,pci.bus_id,pstate,clocks.gr,clocks.mem";

struct Measure {
    presentmon: PathBuf,
    obs: Option<PathBuf>,
    obs_profile: String,
    obs_collection: String,
    /// MediaMTX executable and its configuration.
    ingest: Option<(PathBuf, PathBuf)>,
    ingest_check: bool,
    gpu_power_mode: Option<String>,
    unattended: bool,
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

/// A helper process `measure` started. It is stopped by `stop` (graceful,
/// then killed after `grace`) when `finish` is called or when the `Tool`
/// is dropped on any other path, so no helper outlives `measure`.
pub struct Tool {
    name: &'static str,
    child: Option<Child>,
    stop: Box<dyn FnMut(&mut Child)>,
    grace: Duration,
}

impl Tool {
    pub fn spawn(
        name: &'static str,
        cmd: &mut Command,
        stop: Box<dyn FnMut(&mut Child)>,
        grace: Duration,
    ) -> Result<Self, String> {
        let child = cmd.spawn().map_err(|e| format!("start {name}: {e}"))?;
        Ok(Self {
            name,
            child: Some(child),
            stop,
            grace,
        })
    }

    /// Waits up to `limit` for `ready` (checked every 50 ms; `None` waits
    /// the whole `limit`). Fails if the process exits first, or if `ready`
    /// never holds.
    pub fn settle(
        &mut self,
        limit: Duration,
        mut ready: Option<&mut dyn FnMut() -> bool>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(c) = self.child.as_mut()
                && let Some(status) = c.try_wait().map_err(|e| e.to_string())?
            {
                self.child = None;
                return Err(format!("{} exited during start-up ({status})", self.name));
            }
            if let Some(r) = ready.as_mut()
                && r()
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return match ready {
                    Some(_) => Err(format!("{} did not become ready", self.name)),
                    None => Ok(()),
                };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The process is still running.
    pub fn running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// Waits up to `limit` for the process to exit on its own; `false` if
    /// it is still running.
    pub fn wait_for(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            match self.child.as_mut().map(Child::try_wait) {
                None | Some(Ok(Some(_))) | Some(Err(_)) => {
                    self.child = None;
                    return true;
                }
                Some(Ok(None)) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        false
    }

    /// Lets the process run on unowned (it is neither stopped nor killed).
    pub fn release(&mut self) {
        self.child = None;
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Stops the process; `Err` if it had to be killed.
    pub fn finish(mut self) -> Result<(), String> {
        self.shutdown()
    }

    /// Waits for the process to exit on its own (OBS is closed by the
    /// owner, the ingest check ends by itself); nothing is stopped.
    pub fn wait(mut self) -> Result<std::process::ExitStatus, String> {
        match self.child.take() {
            Some(mut c) => c.wait().map_err(|e| e.to_string()),
            None => Err(format!("{} was not running", self.name)),
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        (self.stop)(&mut child);
        let deadline = Instant::now() + self.grace;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(format!("{} did not stop and was killed", self.name))
    }
}

impl Drop for Tool {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            eprintln!("measure: {e}");
        }
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
}

/// PresentMon under its session name; stopped by terminating the session.
fn start_presentmon(exe: &Path, args: &[String], log: &Path) -> Result<Tool, String> {
    let log = std::fs::File::create(log).map_err(|e| e.to_string())?;
    let stop_exe = exe.to_owned();
    let mut tool = Tool::spawn(
        "PresentMon",
        Command::new(exe)
            .args(args)
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log),
        Box::new(move |_| {
            let _ = Command::new(&stop_exe)
                .args(["--terminate_existing_session", "--session_name", SESSION])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }),
        Duration::from_secs(15),
    )?;
    tool.settle(PRESENTMON_SETTLE, None)
        .map_err(|e| format!("{e}; see private/presentmon.log"))?;
    Ok(tool)
}

/// The local ingest on `addr`: refused if something already listens there
/// (an instance this run did not start is never stopped), and ready once
/// its listener accepts.
pub fn start_ingest(cmd: &mut Command, addr: SocketAddr, limit: Duration) -> Result<Tool, String> {
    if TcpListener::bind(addr).is_err() {
        return Err(format!(
            "{addr} is already in use by another program (another MediaMTX?); stop it first"
        ));
    }
    let mut tool = Tool::spawn("MediaMTX", cmd, Box::new(kill), Duration::from_secs(2))?;
    let mut up = || TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok();
    tool.settle(limit, Some(&mut up))?;
    Ok(tool)
}

/// MediaMTX's recorder settings for every path, writing fragmented MP4
/// under `dir` (MediaMTX reads `MTX_*` variables over its configuration).
pub fn record_env(dir: &Path) -> Vec<(&'static str, String)> {
    let dir = dir.to_string_lossy().replace('\\', "/");
    vec![
        ("MTX_PATHDEFAULTS_RECORD", "yes".into()),
        ("MTX_PATHDEFAULTS_RECORDFORMAT", "fmp4".into()),
        (
            "MTX_PATHDEFAULTS_RECORDPATH",
            format!("{dir}/%path/%Y-%m-%d_%H-%M-%S-%f"),
        ),
    ]
}

/// Whether a process with this image name runs (any user session's view
/// through `tasklist`).
fn process_running(image: &str) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
        .output()
        .is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains(&image.to_lowercase())
        })
}

/// Unattended OBS exit: stop the stream through obs-websocket, then close
/// the main window and wait for OBS to exit by itself.
fn unattended_exit(obs: &mut Tool) -> Result<(), String> {
    crate::obsws::stop_stream(Duration::from_secs(20))?;
    let pid = obs.pid().ok_or("OBS is not running")?;
    if !crate::obsws::close_main_window(pid) {
        return Err("no OBS window to close".into());
    }
    if obs.wait_for(Duration::from_secs(40)) {
        Ok(())
    } else {
        Err("OBS did not exit after its window was closed".into())
    }
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

/// The file version of an executable (`2.6.0`), from its version resource.
fn file_version(exe: &Path) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    use windows::core::{HSTRING, w};
    let name = HSTRING::from(exe.as_os_str());
    // SAFETY: the buffer has the size the first call reports; the fixed
    // info pointer points into it.
    unsafe {
        let len = GetFileVersionInfoSizeW(&name, None);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetFileVersionInfoW(&name, None, len, buf.as_mut_ptr().cast()).ok()?;
        let mut p = std::ptr::null_mut();
        let mut n = 0u32;
        if !VerQueryValueW(buf.as_ptr().cast(), w!("\\"), &mut p, &mut n).as_bool() || p.is_null() {
            return None;
        }
        let f = &*(p as *const VS_FIXEDFILEINFO);
        Some(format!(
            "{}.{}.{}",
            f.dwFileVersionMS >> 16,
            f.dwFileVersionMS & 0xffff,
            f.dwFileVersionLS >> 16
        ))
    }
}

/// The version in a release file name (`PresentMon-2.6.0-x64.exe` →
/// `2.6.0`), for executables without a version resource.
fn version_in_name(exe: &Path) -> Option<String> {
    let stem = exe.file_stem()?.to_str()?;
    stem.split('-')
        .find(|p| {
            p.contains('.')
                && p.split('.')
                    .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_owned)
}

fn parse(args: &[String]) -> Result<(Measure, Option<EncoderClass>, Vec<String>), String> {
    let mut m = Measure {
        presentmon: PathBuf::new(),
        obs: None,
        obs_profile: "tapestry-spike-nvenc".into(),
        obs_collection: crate::obs::COLLECTION.into(),
        ingest: None,
        ingest_check: false,
        gpu_power_mode: None,
        unattended: false,
        dry_run: false,
    };
    let mut encoder = None;
    let (mut ingest, mut ingest_config) = (None, None);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => m.dry_run = true,
            "--ingest-check" => m.ingest_check = true,
            "--unattended" => m.unattended = true,
            "--presentmon" | "--obs" | "--obs-profile" | "--obs-collection" | "--obs-encoder"
            | "--ingest" | "--ingest-config" | "--gpu-power-mode" => {
                let v = it.next().ok_or(format!("{a} needs a value"))?.clone();
                match a.as_str() {
                    "--presentmon" => m.presentmon = PathBuf::from(v),
                    "--obs" => m.obs = Some(PathBuf::from(v)),
                    "--obs-profile" => m.obs_profile = v,
                    "--obs-collection" => m.obs_collection = v,
                    "--ingest" => ingest = Some(PathBuf::from(v)),
                    "--ingest-config" => ingest_config = Some(PathBuf::from(v)),
                    "--gpu-power-mode" => {
                        if !GPU_POWER_MODES.contains(&v.as_str()) {
                            return Err(format!(
                                "--gpu-power-mode {v}: one of {}",
                                GPU_POWER_MODES.join(", ")
                            ));
                        }
                        m.gpu_power_mode = Some(v);
                    }
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
    m.ingest = match (ingest, ingest_config) {
        (Some(e), Some(c)) => Some((e, c)),
        (None, None) => None,
        _ => return Err("--ingest and --ingest-config go together".into()),
    };
    if m.ingest_check && (m.obs.is_none() || m.ingest.is_none()) {
        return Err("--ingest-check needs --obs and --ingest".into());
    }
    Ok((m, encoder, rest))
}

pub fn run(args: &[String]) -> Result<ExitCode, String> {
    let (m, encoder, rest) = parse(args)?;
    let mut a: RunArgs = parse_run(&rest)?;
    if a.offscreen.is_some() {
        return Err("measure runs on a display, not offscreen".into());
    }
    let required = a.duration_s.is_none() && a.loops == spike_core::evaluate::REQUIRED_LOOPS;
    a.meta.kind = match (&m.obs, required && !m.ingest_check) {
        (_, false) => RunKind::Diagnostic,
        (Some(_), true) => RunKind::Obs,
        (None, true) => RunKind::Baseline,
    };
    a.meta.obs_encoder = encoder;
    a.meta.unattended = m.unattended;
    if m.obs.is_some() && !rest.iter().any(|x| x == "--countdown-s") {
        a.countdown_us = OBS_COUNTDOWN_US;
    }
    let private = a.out.join("private");
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
        if let Some((exe, cfg)) = &m.ingest {
            println!(
                "0. {} (refused if {INGEST_ADDR} is in use; this process only is stopped after the run)",
                show(exe, &[cfg.display().to_string()])
            );
        }
        println!("1. nvidia-smi {SMI_QUERY} -lms 100 (NVIDIA adapters only); power-mode watch");
        println!("   {}", show(&m.presentmon, &pm_args));
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
    std::fs::create_dir_all(&private).map_err(|e| e.to_string())?;
    let mut cond = conditions::collect(
        a.audio.as_ref().and_then(|x| x.device.as_deref()),
        a.bga.as_deref(),
    )?;
    cond.insert(
        "presentmon_version".into(),
        json!(file_version(&m.presentmon).or_else(|| version_in_name(&m.presentmon))),
    );
    cond.insert(
        "gpu_power_mode_declared".into(),
        json!(m.gpu_power_mode.as_deref().unwrap_or("undeclared")),
    );
    std::fs::write(
        private.join(conditions::CONDITIONS),
        serde_json::to_vec_pretty(&cond).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let ingest = match &m.ingest {
        Some((exe, cfg)) => {
            let log =
                std::fs::File::create(private.join("mediamtx.log")).map_err(|e| e.to_string())?;
            let addr: SocketAddr = INGEST_ADDR.parse().map_err(|_| "ingest address")?;
            let mut cmd = Command::new(exe);
            cmd.arg(cfg)
                .stdout(log.try_clone().map_err(|e| e.to_string())?)
                .stderr(log);
            if m.unattended {
                // MediaMTX's own recorder, configured for this run only.
                for (k, v) in record_env(&private.join("ingest")) {
                    cmd.env(k, v);
                }
            }
            let t = start_ingest(&mut cmd, addr, INGEST_SETTLE)?;
            println!("measure: local ingest listening on {INGEST_ADDR}");
            Some(t)
        }
        None => None,
    };
    let power = crate::power::Watch::start()
        .map_err(|e| eprintln!("measure: {e}; power mode not recorded"))
        .ok();
    let nvidia = conditions::dxgi()
        .0
        .iter()
        .any(|a| a.vendor_id == NVIDIA_VENDOR_ID);
    let smi = if nvidia {
        let out = std::fs::File::create(a.out.join(crate::report::GPU_SAMPLES))
            .map_err(|e| e.to_string())?;
        Tool::spawn(
            "nvidia-smi",
            Command::new("nvidia-smi")
                .args([SMI_QUERY, "--format=csv,noheader,nounits", "-lms", "100"])
                .stdout(out)
                .stderr(Stdio::null()),
            Box::new(kill),
            Duration::from_secs(1),
        )
        .map_err(|e| eprintln!("measure: {e}; GPU state not recorded"))
        .ok()
    } else {
        None
    };
    let mut pm = start_presentmon(&m.presentmon, &pm_args, &private.join("presentmon.log"))?;
    // OBS and the ingest check are Tools too: an error return closes them
    // with the rest; on the normal path OBS is closed by the owner and the
    // check ends by itself (`Tool::wait`).
    let mut obs_child = None;
    if let Some((exe, args)) = &obs {
        if process_running("obs64.exe") {
            // A second OBS only shows "already running"; the running one
            // belongs to the owner and is never touched.
            return Err("OBS is already running; close it before an OBS run".into());
        }
        let dir = exe.parent().unwrap_or(Path::new("."));
        obs_child = Some(Tool::spawn(
            "OBS",
            Command::new(exe).args(args).current_dir(dir),
            Box::new(kill),
            Duration::from_secs(2),
        )?);
        std::thread::sleep(OBS_SETTLE);
    }
    let check = if m.ingest_check {
        let delay = (a.countdown_us / 1_000_000 + 5).to_string();
        let out =
            std::fs::File::create(private.join("ingest-check.txt")).map_err(|e| e.to_string())?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        Some(Tool::spawn(
            "ingest check",
            Command::new(exe)
                .args([
                    "ingest-check",
                    "--url",
                    &format!("rtmp://{INGEST_ADDR}/live/{}", crate::obs::STREAM_KEY),
                    "--delay-s",
                    &delay,
                    "--seconds",
                    "10",
                ])
                .stdout(out)
                .stderr(Stdio::null()),
            Box::new(kill),
            Duration::from_secs(1),
        )?)
    } else {
        None
    };
    let result = window::windowed(&a);
    let pm_alive = pm.running();
    let stopped = pm.finish();
    if let Some(mut t) = smi {
        // A sampler that died during the run leaves the GPU capture
        // incomplete (report: contradiction unknown).
        let running = t.running();
        let _ = t.finish();
        std::fs::write(
            a.out.join(crate::report::GPU_SAMPLER),
            json!({ "running_at_end": running }).to_string(),
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some(w) = power {
        let mut text = String::new();
        for (q, mode) in w.changes() {
            text.push_str(&format!("{q},{mode}\n"));
        }
        std::fs::write(a.out.join(crate::power::POWER_MODES), text).map_err(|e| e.to_string())?;
    }
    let mut ingest_ok = true;
    if let Some(c) = check {
        let status = c.wait()?;
        let text = std::fs::read_to_string(private.join("ingest-check.txt")).unwrap_or_default();
        print!("{text}");
        ingest_ok = status.success();
    }
    if let Some(mut child) = obs_child {
        if m.unattended {
            if let Err(e) = unattended_exit(&mut child) {
                // Never kill OBS: the stream or OBS stays for the owner.
                child.release();
                return Err(format!(
                    "unattended OBS exit failed ({e}); OBS left running for the owner"
                ));
            }
            println!("measure: stream stopped and OBS closed (unattended)");
        } else {
            println!(
                "measure: stop streaming and exit OBS now (File > Exit); waiting for OBS to close..."
            );
            let _ = child.wait();
        }
        match newest_obs_log() {
            Some(p) => {
                std::fs::copy(&p, a.out.join(crate::report::OBS_LOG))
                    .map_err(|e| format!("copy OBS log: {e}"))?;
                println!("measure: OBS log collected");
            }
            None => eprintln!("measure: no OBS log found"),
        }
    }
    if let Some(t) = ingest {
        t.finish()?;
    }
    if !pm_alive {
        eprintln!("measure: PresentMon exited before the run ended (capture incomplete, V4)");
    }
    stopped?;
    let code = result?;
    if !ingest_ok {
        eprintln!("measure: the ingest check failed (see above)");
        return Ok(ExitCode::from(1));
    }
    println!(
        "measure: capture in {}; next: tapestry-spike report {}",
        csv.display(),
        a.out.display()
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn long_running() -> Command {
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null());
        c
    }

    fn alive(pid: u32) -> bool {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }

    /// A fake capture tool, then an OBS launch that fails.
    fn launch(stopped: Arc<AtomicBool>, pid: &mut u32) -> Result<(), String> {
        let mut pm = Tool::spawn(
            "fake capture",
            &mut long_running(),
            Box::new(move |c| {
                stopped.store(true, Ordering::SeqCst);
                let _ = c.kill();
            }),
            Duration::from_secs(5),
        )?;
        pm.settle(Duration::from_millis(200), None)?;
        *pid = pm.child.as_ref().map_or(0, Child::id);
        Command::new("no-such-obs-executable-for-the-test")
            .spawn()
            .map_err(|e| format!("start OBS: {e}"))?;
        Ok(())
    }

    #[test]
    fn a_failed_launch_after_the_capture_still_stops_it() {
        let stopped = Arc::new(AtomicBool::new(false));
        let mut pid = 0;
        assert!(launch(stopped.clone(), &mut pid).is_err());
        // Stopped by the time the error reached the caller.
        assert!(stopped.load(Ordering::SeqCst));
        assert!(
            pid != 0 && !alive(pid),
            "the fake capture outlived the failure"
        );
    }

    #[test]
    fn owner_closed_tools_are_waited_for_and_error_paths_close_them() {
        // Normal path: the process ends by itself and is not stopped.
        let stopped = Arc::new(AtomicBool::new(false));
        let s = stopped.clone();
        let mut c = Command::new("cmd");
        c.args(["/c", "exit 0"]);
        let t = Tool::spawn(
            "fake OBS",
            &mut c,
            Box::new(move |c| {
                s.store(true, Ordering::SeqCst);
                let _ = c.kill();
            }),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(t.wait().unwrap().success());
        assert!(!stopped.load(Ordering::SeqCst));
        // Error path after the launch (for example a failed file write):
        // the early return drops the tool, which closes the process.
        let mut pid = 0;
        let mut after_launch = || -> Result<(), String> {
            let obs = Tool::spawn(
                "fake OBS",
                &mut long_running(),
                Box::new(kill),
                Duration::from_secs(1),
            )?;
            pid = obs.child.as_ref().map_or(0, Child::id);
            std::fs::File::create("Z:/no/such/dir/ingest-check.txt").map_err(|e| e.to_string())?;
            obs.wait().map(|_| ())
        };
        assert!(after_launch().is_err());
        assert!(pid != 0 && !alive(pid), "the fake OBS outlived the error");
    }

    #[test]
    fn a_tool_that_exits_during_start_up_fails_fast() {
        let mut c = Command::new("cmd");
        c.args(["/c", "exit 3"]);
        let mut t = Tool::spawn("fake", &mut c, Box::new(kill), Duration::from_secs(1)).unwrap();
        let t0 = Instant::now();
        let e = t.settle(Duration::from_secs(10), None).unwrap_err();
        assert!(e.contains("exited during start-up"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_ingest_never_starts_over_a_port_in_use_and_must_come_up() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let e = start_ingest(&mut long_running(), addr, Duration::from_secs(1))
            .err()
            .unwrap();
        assert!(e.contains("already in use"), "{e}");
        drop(held);
        // A process that never listens is reported and stopped.
        let free = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = start_ingest(&mut long_running(), free, Duration::from_millis(500))
            .err()
            .unwrap();
        assert!(e.contains("did not become ready"), "{e}");
    }

    #[test]
    fn presentmon_version_from_the_release_name() {
        let v = |p: &str| version_in_name(Path::new(p));
        assert_eq!(
            v("presentmon/PresentMon-2.6.0-x64.exe").as_deref(),
            Some("2.6.0")
        );
        assert_eq!(v("PresentMon.exe"), None);
    }

    #[test]
    fn the_ingest_recorder_writes_under_the_run() {
        let env = record_env(Path::new(r"C:\run\private\ingest"));
        assert!(env.contains(&("MTX_PATHDEFAULTS_RECORD", "yes".into())));
        assert!(env.iter().any(|(k, v)| *k == "MTX_PATHDEFAULTS_RECORDPATH"
            && v.starts_with("C:/run/private/ingest/%path/")
            && v.contains("%f")));
    }

    #[test]
    fn a_released_tool_is_left_running() {
        let mut t = Tool::spawn(
            "fake OBS",
            &mut long_running(),
            Box::new(kill),
            Duration::from_secs(1),
        )
        .unwrap();
        let pid = t.pid().unwrap();
        assert!(!t.wait_for(Duration::from_millis(200)));
        t.release();
        drop(t);
        assert!(alive(pid));
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }

    #[test]
    fn measure_options_are_checked() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse(&a(&["--presentmon", "p", "--gpu-power-mode", "turbo"])).is_err());
        assert!(parse(&a(&["--presentmon", "p", "--ingest", "m.exe"])).is_err());
        assert!(parse(&a(&["--presentmon", "p", "--ingest-check"])).is_err());
        let (m, _, rest) = parse(&a(&[
            "--presentmon",
            "p",
            "--gpu-power-mode",
            "prefer-max-performance",
            "--duration-s",
            "60",
        ]))
        .unwrap();
        assert_eq!(m.gpu_power_mode.as_deref(), Some("prefer-max-performance"));
        assert_eq!(rest, a(&["--duration-s", "60"]));
    }
}
