//! `report <run-dir>` and `report --from-export <zip>` (design §2, §3.1).
//!
//! `report <run-dir>` completes the run's `RunData` with PresentMon rows
//! (`private/presentmon.csv`, placed on the run clock by
//! `private/origin.json`), the OBS session (`private/obs.log`), the owner's
//! notes (`owner-notes.toml`), and the run's conditions (the adapter it
//! used, the GPU state and effective power mode during play), evaluates it,
//! writes `summary.json`, `report.md` and `ladder.txt` (the next rung after
//! a gate failure), and builds the sanitized export `export/<run-id>.zip`,
//! refused if the scan finds a path, a private name or a raw QPC value. It
//! fails closed: without readable conditions and their private names
//! nothing is exported. Exit 0 pass, 1 gate failed, 2 invalid.
//!
//! `report --from-export <zip>` reads only the export, recomputes the
//! summary and compares it with the recorded one.

use crate::conditions::{self, CONDITIONS};
use crate::rundata::{ORIGIN, Origin, RUNDATA};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use spike_core::conditions::{
    AdapterRecord, GpuState, NVIDIA_VENDOR_ID, PeriodLadder, PublicConditions, gpu_state_from_smi,
    ladder, obs_log_day_base, power_mode_contradicted, power_modes_during, select_adapter,
};
use spike_core::evaluate::{Summary, evaluate};
use spike_core::export::{self, Export, ExportError, ScanContext};
use spike_core::obslog::{self, WallWindow};
use spike_core::presentmon::{self, QpcClock};
use spike_core::run::{OwnerPublic, RunData};
use spike_core::time::qpc_to_us;
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

pub const PRESENTMON_CSV: &str = "private/presentmon.csv";
pub const OBS_LOG: &str = "private/obs.log";
/// `nvidia-smi` samples during the run (`measure`), local timestamps.
pub const GPU_SAMPLES: &str = "private/nvidia-smi.csv";
pub const LADDER: &str = "ladder.txt";
/// Whether the GPU sampler was still running when the run ended (`measure`).
pub const GPU_SAMPLER: &str = "private/gpu_sampler.json";
pub const APP: &str = "tapestry-spike.exe";

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T, String> {
    let b = std::fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))?;
    serde_json::from_slice(&b).map_err(|e| format!("parse {}: {e}", p.display()))
}

/// A run completed with its private context.
pub struct Completed {
    pub run: RunData,
    /// The private conditions with the run's facts added.
    pub conditions: Map<String, Value>,
    pub public: PublicConditions,
    pub origin: Origin,
    /// Names the export scan must not find (never empty).
    pub private_names: Vec<String>,
}

/// The private names from `conditions.json`. Missing, malformed or empty
/// names are an error: the export scan would otherwise pass unchecked.
fn private_names(cond: &Map<String, Value>) -> Result<Vec<String>, String> {
    let names: Vec<String> = cond
        .get("private_names")
        .and_then(Value::as_array)
        .ok_or("conditions have no private_names list; the export cannot be scanned")?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or("conditions' private_names holds a non-string")?;
    if names.iter().all(|n| n.trim().is_empty()) {
        return Err("conditions' private_names is empty; the export cannot be scanned".into());
    }
    Ok(names)
}

/// GPU state during play: `nvidia-smi` samples of the run's adapter when it
/// is an NVIDIA GPU, otherwise unavailable (never a failure).
fn gpu_state(dir: &Path, vendor: u32, name: &str, from: i64, to: i64) -> GpuState {
    if vendor != NVIDIA_VENDOR_ID {
        return GpuState::unavailable("adapter is not an NVIDIA GPU");
    }
    // A sampler that exited before the run ended (or an unknown status)
    // leaves the capture incomplete.
    let sampler_ok = read_json::<Value>(&dir.join(GPU_SAMPLER))
        .ok()
        .and_then(|v| v["running_at_end"].as_bool())
        .unwrap_or(false);
    match std::fs::read_to_string(dir.join(GPU_SAMPLES)) {
        Ok(text) => gpu_state_from_smi(&text, name, from, to, sampler_ok),
        Err(_) => GpuState::unavailable("no nvidia-smi samples"),
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The run's `RunData` with PresentMon, OBS and owner facts added, and the
/// conditions with the run's facts.
pub fn complete(dir: &Path, notes: &Path) -> Result<Completed, String> {
    let mut run: RunData = read_json(&dir.join(RUNDATA))?;
    let origin: Origin = read_json(&dir.join(ORIGIN))?;
    let mut cond: Map<String, Value> =
        read_json(&dir.join("private").join(CONDITIONS)).map_err(|e| {
            format!("{e}; the export needs the run's conditions for its private-name scan")
        })?;
    let names = private_names(&cond)?;
    let owner = conditions::owner_notes(notes);
    for (k, v) in &owner {
        cond.insert(k.clone(), v.clone());
    }
    // V3: VRR as the owner reports it; unreported counts as on (invalid).
    run.info.vrr = owner.get("vrr").and_then(Value::as_bool).unwrap_or(true);
    run.owner = OwnerPublic {
        game_visible: owner.get("game_visible").and_then(Value::as_bool),
        audio_present: owner.get("audio_present").and_then(Value::as_bool),
    };
    let play_start = origin.play_start_us;
    let play_end = play_start + origin.play_len_us;
    let pm = dir.join(PRESENTMON_CSV);
    match std::fs::read(&pm) {
        Ok(bytes) => {
            let clock = QpcClock {
                origin: origin.qpc_origin,
                freq: origin.qpc_freq,
            };
            let (frames, err) = presentmon::parse(&bytes, APP, clock);
            run.pm_frames = frames;
            run.info.presentmon_error = err.map(|e| e.message());
            cond.insert(
                "hybrid_present".into(),
                json!(presentmon::hybrid_present(&bytes, APP)),
            );
        }
        Err(_) => run.info.presentmon_error = Some("no PresentMon capture".into()),
    }
    if let Ok(log) = std::fs::read_to_string(dir.join(OBS_LOG)) {
        // OBS log lines carry a time of day only; their day is placed by the
        // date-aware origin (the log began before the run).
        let base = obslog::first_time_of_day(&log)
            .map(|tod| obs_log_day_base(origin.local_ms(0), tod))
            .ok_or("the OBS log has no timestamped line")?;
        run.obs = Some(obslog::derive(
            &log,
            WallWindow {
                start_ms: origin.local_ms(play_start) - base,
                end_ms: origin.local_ms(play_end) - base,
            },
        ));
        if let Some(v) = obs_version(&log) {
            cond.insert("obs_version".into(), json!(v));
        }
    }
    cond.insert("refresh_num".into(), json!(run.info.refresh.num));
    cond.insert("refresh_den".into(), json!(run.info.refresh.den));
    cond.insert("vrr".into(), json!(run.info.vrr));
    let render: Value = read_json(&dir.join("render.json"))?;
    if let Some(size) = render.get("size").and_then(Value::as_array)
        && let [Some(w), Some(h)] = [size[0].as_u64(), size[1].as_u64()]
    {
        cond.insert("render_width".into(), json!(w));
        cond.insert("render_height".into(), json!(h));
        let g = gcd(w as u32, h as u32).max(1);
        cond.insert(
            "aspect_ratio".into(),
            json!(format!("{}:{}", w as u32 / g, h as u32 / g)),
        );
    }
    if let Some(m) = render.pointer("/monitor/gdi_name").and_then(Value::as_str) {
        cond.insert("run_display".into(), json!(m.trim_start_matches(r"\\.\")));
    }
    // The adapter the run used, not the first one enumerated.
    let used_name = render["adapter"].as_str().unwrap_or_default().to_owned();
    let vendor = render["adapter_vendor"].as_u64().unwrap_or(0) as u32;
    let device = render["adapter_device"].as_u64().unwrap_or(0) as u32;
    let adapters: Vec<AdapterRecord> = cond
        .get("gpu_adapters")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let used = select_adapter(&adapters, vendor, device, &used_name);
    cond.insert("gpu_adapter".into(), json!(used_name));
    cond.insert("gpu_vram_mb".into(), json!(used.map(|a| a.vram_mb)));
    cond.insert(
        "gpu_driver".into(),
        json!(used.and_then(|a| a.driver.clone())),
    );
    let state = gpu_state(
        dir,
        vendor,
        &used_name,
        origin.local_ms(play_start),
        origin.local_ms(play_end),
    );
    let declared = cond
        .get("gpu_power_mode_declared")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let contradicted = power_mode_contradicted(declared.as_deref(), &state);
    if contradicted == Some(true) {
        eprintln!(
            "report: WARNING: prefer-max-performance was declared but the GPU never reached P0 during play; the driver's program profile is per executable path, so it may not cover this copy"
        );
    }
    cond.insert("gpu_state".into(), json!(state));
    cond.insert("gpu_power_mode_contradicted".into(), json!(contradicted));
    // Effective power modes during play (`measure`'s watch); unknown without
    // one, never the mode at collection time.
    let modes = std::fs::read_to_string(dir.join(crate::power::POWER_MODES))
        .ok()
        .map(|text| {
            text.lines()
                .filter_map(|l| {
                    let (q, m) = l.split_once(',')?;
                    let t = qpc_to_us(q.parse().ok()?, origin.qpc_origin, origin.qpc_freq);
                    Some((t, m.trim().parse().ok()?))
                })
                .collect::<Vec<(i64, i32)>>()
        });
    cond.insert(
        "power_mode".into(),
        json!(modes.and_then(|m| power_modes_during(&m, play_start, play_end))),
    );
    if let Ok(p) = read_json::<Value>(&dir.join("play.json"))
        && let Some(r) = p.pointer("/audio/render")
    {
        cond.insert("audio_backend".into(), r["backend"].clone());
        cond.insert("audio_period_chosen".into(), r["period_frames"].clone());
    }
    let public = PublicConditions::from_conditions(&cond)?;
    Ok(Completed {
        run,
        conditions: cond,
        public,
        origin,
        private_names: names,
    })
}

/// `OBS 33.0.0 (64-bit, windows)` from the log's header.
pub fn obs_version(log: &str) -> Option<String> {
    log.lines().find_map(|l| {
        let msg = l.split_once(": ").map(|(_, m)| m)?;
        let v = msg.strip_prefix("OBS ")?;
        let v = v.split_whitespace().next()?;
        v.chars()
            .all(|c| c.is_ascii_digit() || c == '.')
            .then(|| v.to_owned())
    })
}

/// A run id derived from the run's data (no names, no times).
fn run_id(run: &RunData) -> String {
    let bytes = serde_json::to_vec(run).unwrap_or_default();
    let h = Sha256::digest(&bytes);
    format!(
        "run-{}",
        h.iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn zip_bytes(e: &Export) -> Result<Vec<u8>, String> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in &e.files {
        w.start_file(name.as_str(), opts)
            .map_err(|x| x.to_string())?;
        w.write_all(bytes).map_err(|x| x.to_string())?;
    }
    Ok(w.finish().map_err(|x| x.to_string())?.into_inner())
}

fn unzip(path: &Path) -> Result<Export, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    let mut e = Export::default();
    for i in 0..z.len() {
        let mut file = z.by_index(i).map_err(|x| x.to_string())?;
        let mut b = Vec::new();
        file.read_to_end(&mut b).map_err(|x| x.to_string())?;
        let name = file.name().map_err(|x| x.to_string())?.into_owned();
        e.files.insert(name, b);
    }
    Ok(e)
}

fn print_summary(s: &Summary) {
    for c in &s.validity {
        println!(
            "{} {}{}",
            c.id,
            if c.ok { "ok" } else { "INVALID" },
            if c.reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", c.reasons.join(", "))
            }
        );
    }
    for g in &s.gates {
        println!(
            "{} {}{} value {:?} threshold {} {} ({} samples)",
            g.id,
            if g.pass { "pass" } else { "FAIL" },
            if g.applies { "" } else { " (recorded)" },
            g.value,
            g.threshold,
            g.unit,
            g.samples
        );
    }
    println!("exit {}", s.exit_code);
}

/// `ladder.txt`: the next rungs (`profile=`, `audio=` run options, `note=`
/// lines), empty when none applies; `run-obs.cmd` prints the commands.
fn write_ladder(dir: &Path, summary: &Summary, c: &Completed) -> Result<(), String> {
    let u = |k: &str| {
        c.conditions
            .get(k)
            .and_then(Value::as_u64)
            .map(|v| v as u32)
    };
    // The ladder stops at the default period (design D5), not the
    // engine's maximum.
    let periods = match (
        u("audio_period_chosen"),
        u("audio_period_step"),
        u("audio_period_default"),
    ) {
        (Some(chosen), Some(step), Some(default)) => Some(PeriodLadder {
            chosen,
            step,
            default,
        }),
        _ => None,
    };
    let l = ladder(summary, &c.run.info, periods);
    let mut text = String::new();
    for (k, v) in [("profile", &l.profile), ("audio", &l.audio)] {
        if let Some(v) = v {
            text.push_str(&format!("{k}={v}\n"));
            println!("ladder: next {k} rung: {v}");
        }
    }
    for n in &l.notes {
        text.push_str(&format!("note={n}\n"));
        println!("ladder: {n}");
    }
    std::fs::write(dir.join(LADDER), text).map_err(|e| e.to_string())
}

/// `report <run-dir> [--notes owner-notes.toml]`.
pub fn run(dir: &Path, notes: &Path) -> Result<ExitCode, String> {
    // Outputs of an earlier report never stand for this one.
    for f in [export::SUMMARY, export::REPORT, LADDER] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let c = complete(dir, notes)?;
    let (run, origin) = (&c.run, &c.origin);
    let summary = evaluate(run);
    let ctx = ScanContext {
        forbidden_names: c.private_names.clone(),
        qpc_range: Some((
            origin.qpc_origin as u64,
            origin.qpc_end.max(origin.qpc_origin) as u64,
        )),
    };
    let e = match export::build(run, &c.public, &ctx) {
        Ok(e) => e,
        Err(ExportError::Rejected(f)) => {
            return Err(format!("export refused by the scan: {f:?}"));
        }
        Err(e) => return Err(format!("export: {e:?}")),
    };
    // Public outputs only after the scan accepted them: the local summary
    // and report are the export's own files.
    for f in [export::SUMMARY, export::REPORT] {
        std::fs::write(dir.join(f), &e.files[f]).map_err(|e| e.to_string())?;
    }
    write_ladder(dir, &summary, &c)?;
    let id = run_id(run);
    let out = dir.join("export").join(format!("{id}.zip"));
    std::fs::create_dir_all(out.parent().unwrap_or(dir)).map_err(|e| e.to_string())?;
    std::fs::write(&out, zip_bytes(&e)?).map_err(|e| e.to_string())?;
    print_summary(&summary);
    println!(
        "report: {} and report.md written; export {}",
        dir.join("summary.json").display(),
        out.display()
    );
    Ok(ExitCode::from(summary.exit_code))
}

/// `report --from-export <zip>`: exit as the recomputed summary, or 3 if it
/// differs from the recorded one.
pub fn from_export(zip: &Path) -> Result<ExitCode, String> {
    let e = unzip(zip)?;
    let r = export::recompute(&e).map_err(|e| format!("export rejected: {e:?}"))?;
    print_summary(&r.summary);
    if r.matches_recorded {
        println!("recomputed summary is identical to the recorded one");
        Ok(ExitCode::from(r.summary.exit_code))
    } else {
        println!("recomputed summary DIFFERS from the recorded one");
        Ok(ExitCode::from(3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_version_comes_from_the_log_header() {
        let log = "02:41:41.267: Portable mode: false\n02:41:41.442: OBS 33.0.0 (64-bit, windows)\n02:41:41.450: audio settings reset:\n";
        assert_eq!(obs_version(log).as_deref(), Some("33.0.0"));
        assert_eq!(obs_version("02:41:41.442: OBS is starting\n"), None);
    }

    #[test]
    fn an_export_survives_the_zip_round_trip() {
        let run = spike_core::synth::passing_run();
        let e = export::build(&run, &PublicConditions::default(), &ScanContext::default()).unwrap();
        let dir = std::env::temp_dir().join(format!("spike-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.zip");
        std::fs::write(&p, zip_bytes(&e).unwrap()).unwrap();
        let back = unzip(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(back, e);
        assert!(export::recompute(&back).unwrap().matches_recorded);
    }

    /// A run directory with the synthetic passing run and the given private
    /// conditions file content (`None`: no file).
    fn run_dir(tag: &str, conditions: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spike-report-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("private")).unwrap();
        let run = spike_core::synth::passing_run();
        std::fs::write(dir.join(RUNDATA), serde_json::to_vec(&run).unwrap()).unwrap();
        let origin = Origin {
            qpc_origin: 52_000_000_000,
            qpc_freq: 10_000_000,
            wall_origin_local_ms: spike_core::conditions::local_ms(2026, 10, 25, 3_600_000),
            play_start_us: run.info.timeline.play_start_us,
            play_len_us: run.info.timeline.play_end_us() - run.info.timeline.play_start_us,
            qpc_end: 52_000_000_000 + 700 * 10_000_000,
        };
        std::fs::write(dir.join(ORIGIN), serde_json::to_vec(&origin).unwrap()).unwrap();
        let render = json!({"adapter": "Synthetic GPU", "adapter_vendor": 1, "adapter_device": 2,
            "size": [1920, 1080], "monitor": {"gdi_name": "\\\\.\\DISPLAY1"}});
        std::fs::write(dir.join("render.json"), render.to_string()).unwrap();
        if let Some(c) = conditions {
            std::fs::write(dir.join("private").join(CONDITIONS), c).unwrap();
        }
        dir
    }

    fn notes(dir: &Path, text: &str) -> std::path::PathBuf {
        let p = dir.join("owner-notes.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn report_refuses_to_export_without_valid_private_context() {
        let good = r#"{"private_names": ["Studio Speakers XR"], "cpu_brand": "Synthetic CPU"}"#;
        for (tag, c) in [
            ("missing", None),
            ("malformed", Some("{not json")),
            ("no-names", Some(r#"{"cpu_brand": "Synthetic CPU"}"#)),
            ("empty-names", Some(r#"{"private_names": []}"#)),
            ("bad-names", Some(r#"{"private_names": [3]}"#)),
        ] {
            let dir = run_dir(tag, c);
            let n = notes(&dir, "vrr = false\n");
            assert!(run(&dir, &n).is_err(), "{tag}");
            assert!(!dir.join("export").exists(), "{tag}");
            let _ = std::fs::remove_dir_all(&dir);
        }
        // With valid context, a private endpoint name planted in a public
        // owner note is found and the export refused.
        let dir = run_dir("planted", Some(good));
        let n = notes(
            &dir,
            "vrr = false\naudio_device_class = \"studio speakers xr\"\n",
        );
        let err = run(&dir, &n).err().unwrap();
        assert!(err.contains("refused") && err.contains("Name"), "{err}");
        assert!(!dir.join("export").exists());
        // No public output is left with the name either.
        for f in [export::SUMMARY, export::REPORT, LADDER] {
            assert!(!dir.join(f).exists(), "{f}");
        }
        // The same run with a harmless note exports.
        let n = notes(
            &dir,
            "vrr = false\naudio_device_class = \"USB interface\"\n",
        );
        assert!(run(&dir, &n).is_ok());
        assert!(dir.join(export::REPORT).exists());
        // A later refused report removes the earlier outputs.
        let planted = notes(
            &dir,
            "vrr = false\naudio_device_class = \"studio speakers xr\"\n",
        );
        assert!(run(&dir, &planted).is_err());
        assert!(!dir.join(export::REPORT).exists());
        let n = notes(
            &dir,
            "vrr = false\naudio_device_class = \"USB interface\"\n",
        );
        assert!(run(&dir, &n).is_ok());
        let _ = std::fs::remove_dir_all(dir.join("export"));
        assert!(run(&dir, &n).is_ok());
        let zip = std::fs::read_dir(dir.join("export"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(from_export(&zip).is_ok());
        let back = unzip(&zip).unwrap();
        let c: PublicConditions = serde_json::from_slice(&back.files[export::CONDITIONS]).unwrap();
        assert_eq!(c.run_display.as_deref(), Some("DISPLAY1"));
        assert_eq!(
            c.gpu_state.unwrap().source,
            "unavailable: adapter is not an NVIDIA GPU"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_takes_the_gpu_facts_of_the_adapter_the_run_used() {
        let c = r#"{"private_names": ["host-name-x"], "gpu_adapters": [
            {"name": "Discrete GPU", "vendor_id": 4318, "device_id": 9986, "vram_mb": 16000, "driver": "32.0"},
            {"name": "Synthetic GPU", "vendor_id": 1, "device_id": 2, "vram_mb": 512, "driver": "31.0"}]}"#;
        let dir = run_dir("adapter", Some(c));
        let n = notes(&dir, "vrr = false\n");
        let done = complete(&dir, &n).unwrap();
        assert_eq!(done.public.gpu_adapter.as_deref(), Some("Synthetic GPU"));
        assert_eq!(done.public.gpu_vram_mb, Some(512));
        assert_eq!(done.public.gpu_driver.as_deref(), Some("31.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn obs_sessions_map_across_midnight() {
        // Play from 23:59:30 for ten minutes; OBS started at 23:59:00 the
        // same day and streamed across midnight.
        let c = r#"{"private_names": ["host-name-x"]}"#;
        let dir = run_dir("midnight", Some(c));
        let mut origin: Origin = read_json(&dir.join(ORIGIN)).unwrap();
        let day = spike_core::conditions::local_ms(2026, 10, 25, 0);
        let play_start = origin.play_start_us;
        for (log_start_tod, origin_local) in [
            // OBS and the origin before midnight; play crosses it.
            (86_340_000, day + 86_370_000 - play_start / 1000),
            // OBS before midnight; the run entirely after it.
            (86_380_000, day + 86_400_000 + 5_000 - play_start / 1000),
        ] {
            origin.wall_origin_local_ms = origin_local;
            std::fs::write(dir.join(ORIGIN), serde_json::to_vec(&origin).unwrap()).unwrap();
            let start_ms = origin.local_ms(play_start) - day;
            let tod = |ms: i64| {
                let ms = ms.rem_euclid(86_400_000);
                format!(
                    "{:02}:{:02}:{:02}.{:03}",
                    ms / 3_600_000,
                    ms / 60_000 % 60,
                    ms / 1000 % 60,
                    ms % 1000
                )
            };
            let log = format!(
                "{}: OBS 33.0.0 (64-bit, windows)\n{}: ==== Streaming Start ===============================================\n{}: ==== Streaming Stop ================================================\n",
                tod(log_start_tod),
                tod(start_ms - 10_000),
                tod(start_ms + origin.play_len_us / 1000 + 5_000),
            );
            std::fs::write(dir.join(OBS_LOG), log).unwrap();
            let n = notes(&dir, "vrr = false\n");
            let done = complete(&dir, &n).unwrap();
            let s = done
                .run
                .obs
                .unwrap()
                .session
                .expect("session overlaps play");
            assert_eq!(s.start_lead_ms, Some(10_000));
            assert_eq!(s.post_roll_ms, Some(5_000));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
