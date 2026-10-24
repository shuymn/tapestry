//! `report <run-dir>` and `report --from-export <zip>` (design §2, §3.1).
//!
//! `report <run-dir>` completes the run's `RunData` with PresentMon rows
//! (`private/presentmon.csv`, placed on the run clock by
//! `private/origin.json`), the OBS session (`private/obs.log`), the owner's
//! notes (`owner-notes.toml`), evaluates it, writes `summary.json` and
//! `report.md`, and builds the sanitized export `export/<run-id>.zip`,
//! refused if the scan finds a path, a private name or a raw QPC value.
//! Exit 0 pass, 1 gate failed, 2 invalid.
//!
//! `report --from-export <zip>` reads only the export, recomputes the
//! summary and compares it with the recorded one.

use crate::conditions::{self, CONDITIONS};
use crate::rundata::{ORIGIN, Origin, RUNDATA};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use spike_core::evaluate::{Summary, evaluate};
use spike_core::export::{self, Export, ExportError, ScanContext};
use spike_core::obslog::{self, WallWindow};
use spike_core::presentmon::{self, QpcClock};
use spike_core::run::{OwnerPublic, RunData};
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

pub const PRESENTMON_CSV: &str = "private/presentmon.csv";
pub const OBS_LOG: &str = "private/obs.log";
pub const APP: &str = "tapestry-spike.exe";

fn read_json<T: serde::de::DeserializeOwned>(p: &Path) -> Result<T, String> {
    let b = std::fs::read(p).map_err(|e| format!("read {}: {e}", p.display()))?;
    serde_json::from_slice(&b).map_err(|e| format!("parse {}: {e}", p.display()))
}

/// The run's `RunData` with PresentMon, OBS and owner facts added, and the
/// conditions with the run's public fields.
pub fn complete(dir: &Path, notes: &Path) -> Result<(RunData, Map<String, Value>, Origin), String> {
    let mut run: RunData = read_json(&dir.join(RUNDATA))?;
    let origin: Origin = read_json(&dir.join(ORIGIN))?;
    let mut cond: Map<String, Value> =
        read_json(&dir.join("private").join(CONDITIONS)).unwrap_or_default();
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
        }
        Err(_) => run.info.presentmon_error = Some("no PresentMon capture".into()),
    }
    if let Ok(log) = std::fs::read_to_string(dir.join(OBS_LOG)) {
        let start = origin.wall_origin_ms + origin.play_start_us / 1000;
        run.obs = Some(obslog::derive(
            &log,
            WallWindow {
                start_ms: start,
                end_ms: start + origin.play_len_us / 1000,
            },
        ));
        if let Some(v) = obs_version(&log) {
            cond.insert("obs_version".into(), json!(v));
        }
    }
    cond.insert("refresh_num".into(), json!(run.info.refresh.num));
    cond.insert("refresh_den".into(), json!(run.info.refresh.den));
    cond.insert("vrr".into(), json!(run.info.vrr));
    if let Ok(r) = read_json::<Value>(&dir.join("render.json"))
        && let Some(size) = r.get("size").and_then(Value::as_array)
    {
        cond.insert("render_width".into(), size[0].clone());
        cond.insert("render_height".into(), size[1].clone());
    }
    if let Ok(p) = read_json::<Value>(&dir.join("play.json"))
        && let Some(r) = p.pointer("/audio/render")
    {
        cond.insert("audio_backend".into(), r["backend"].clone());
        cond.insert("audio_period_chosen".into(), r["period_frames"].clone());
    }
    Ok((run, cond, origin))
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

pub fn markdown(s: &Summary, run: &RunData, cond: &Map<String, Value>) -> String {
    use std::fmt::Write as _;
    let mut m = String::new();
    let _ = writeln!(m, "# Stage 0 spike run report\n");
    let verdict = match s.exit_code {
        0 => "pass",
        1 => "gate failed",
        _ => "invalid",
    };
    let _ = writeln!(
        m,
        "Result: **{verdict}** (exit {}). Profile {:?}, kind {:?}, class {} Hz, refresh {}/{} Hz, cap {:?}, loops {}.\n",
        s.exit_code,
        run.info.profile,
        run.info.kind,
        run.info.class_hz,
        run.info.refresh.num,
        run.info.refresh.den,
        run.info.fps_cap,
        run.info.timeline.loops
    );
    let _ = writeln!(
        m,
        "## Validity\n\n| Check | OK | Reasons |\n| --- | --- | --- |"
    );
    for c in &s.validity {
        let _ = writeln!(m, "| {} | {} | {} |", c.id, c.ok, c.reasons.join(", "));
    }
    let _ = writeln!(
        m,
        "\n## Gates (heavy windows)\n\n| Gate | Applies | Pass | Value | Threshold | Samples |\n| --- | --- | --- | --- | --- | --- |"
    );
    for g in &s.gates {
        let _ = writeln!(
            m,
            "| {} | {} | {} | {} | {} {} | {} |",
            g.id,
            g.applies,
            g.pass,
            g.value.map_or("—".into(), |v| format!("{v:.3}")),
            g.threshold,
            g.unit,
            g.samples
        );
    }
    let r = &s.records;
    let _ = writeln!(
        m,
        "\n## Records\n\n- Missed deadlines (G1r): {} ({} not displayed) of {} heavy-window frames\n- Whole run p99 / max interval: {:?} / {:?} ms\n- Voice steals: {}; BGA due not displayed: {}\n- Present modes (PresentMon rows): {}\n- Library allocations (G6r): {:?}",
        r.missed_deadlines,
        r.not_displayed,
        r.heavy_frames,
        r.whole_run_p99_interval_ms,
        r.whole_run_max_interval_ms,
        r.voice_steals,
        r.bga_skipped,
        if r.present_modes.is_empty() {
            "none".to_owned()
        } else {
            r.present_modes
                .iter()
                .map(|c| format!("{:?} {}", c.mode, c.frames))
                .collect::<Vec<_>>()
                .join(", ")
        },
        r.library_allocs
    );
    let _ = writeln!(
        m,
        "\n## Worst frames\n\n| Present µs | Interval ms | Visible notes | Effects | Lanes lit | BGA shown |\n| --- | --- | --- | --- | --- | --- |"
    );
    for w in &r.worst_frames {
        let _ = writeln!(
            m,
            "| {} | {:?} | {} | {} | {} | {} |",
            w.present_us,
            w.display_change_ms,
            w.visible_notes,
            w.effect_instances,
            w.lanes_with_effects,
            w.bga_frame_shown
        );
    }
    let public = export::sanitize_conditions(cond);
    let _ = writeln!(m, "\n## Conditions (public)\n");
    for (k, v) in &public {
        let _ = writeln!(m, "- {k}: {v}");
    }
    m
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

/// `report <run-dir> [--notes owner-notes.toml]`.
pub fn run(dir: &Path, notes: &Path) -> Result<ExitCode, String> {
    let (run, cond, origin) = complete(dir, notes)?;
    let summary = evaluate(&run);
    std::fs::write(
        dir.join("summary.json"),
        serde_json::to_vec_pretty(&summary).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(dir.join("report.md"), markdown(&summary, &run, &cond))
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = cond
        .get("private_names")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let ctx = ScanContext {
        forbidden_names: names,
        qpc_range: Some((
            origin.qpc_origin as u64,
            origin.qpc_end.max(origin.qpc_origin) as u64,
        )),
    };
    let e = match export::build(&run, &cond, &ctx) {
        Ok(e) => e,
        Err(ExportError::Rejected(f)) => {
            return Err(format!("export refused by the scan: {f:?}"));
        }
        Err(e) => return Err(format!("export: {e:?}")),
    };
    let id = run_id(&run);
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
        let e = export::build(&run, &Map::new(), &ScanContext::default()).unwrap();
        let dir = std::env::temp_dir().join(format!("spike-zip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.zip");
        std::fs::write(&p, zip_bytes(&e).unwrap()).unwrap();
        let back = unzip(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(back, e);
        assert!(export::recompute(&back).unwrap().matches_recorded);
    }
}
