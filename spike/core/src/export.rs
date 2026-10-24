//! Sanitized export and `--from-export` recompute (design §3.1).
//!
//! The export is the only thing that leaves the measurement PC. It holds the
//! summary, the run's export-safe facts as JSON, the per-sample series as
//! CSVs with relative µs only, and the whitelisted condition fields. Before
//! an export is accepted, `scan` checks every file for paths, forbidden
//! names (user, computer and device names), values in the run's raw QPC
//! range, and keys or columns outside the whitelist.
//!
//! The export is an in-memory file map; the app writes it as a zip (zip I/O
//! stays out of this crate, which only needs the bytes).

use crate::evaluate::{Summary, evaluate};
use crate::obslog::ObsDerived;
use crate::run::{
    AudioFacts, AudioPeriod, BgaDue, Counters, InputLatency, LIBRARY_SCOPES, OwnerPublic, PmFrame,
    RunData, RunEvent, RunInfo, SpikeFrame, StreamCounts,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const SUMMARY: &str = "summary.json";
pub const RUN: &str = "run.json";
pub const OBS: &str = "obs-derived.json";
pub const CONDITIONS: &str = "conditions-public.json";
pub const PM_FRAMES: &str = "pm-frames.csv";
pub const FRAMES: &str = "frames.csv";
pub const AUDIO_PERIODS: &str = "audio-periods.csv";
pub const INPUT: &str = "input-judgment.csv";
pub const BGA: &str = "bga.csv";
pub const FILES: [&str; 9] = [
    SUMMARY,
    RUN,
    OBS,
    CONDITIONS,
    PM_FRAMES,
    FRAMES,
    AUDIO_PERIODS,
    INPUT,
    BGA,
];

/// Condition fields (§3.5) that may leave the PC. Everything else in
/// `conditions.json` (paths, names, serials, wall clock) is dropped.
pub const CONDITION_KEYS: &[&str] = &[
    "commit",
    "dirty",
    "build_profile",
    "scene_hash",
    "chart_hash",
    "bga_sha256",
    "bga_codec",
    "wgpu_version",
    "ffmpeg_version",
    "presentmon_version",
    "obs_version",
    "cpu_brand",
    "cpu_cores",
    "ram_gb",
    "pc_form",
    "gpu_adapter",
    "gpu_vram_mb",
    "gpu_display_attached",
    "hybrid_present",
    "windows_build",
    "windows_ubr",
    "power_source",
    "power_scheme",
    "power_mode",
    "game_mode",
    "hags",
    "refresh_num",
    "refresh_den",
    "aspect_ratio",
    "render_width",
    "render_height",
    "tearing_support",
    "monitor_count",
    "vrr",
    "display_connection",
    "audio_mix_format",
    "audio_share_mode",
    "audio_period_min",
    "audio_period_default",
    "audio_period_chosen",
    "audio_backend",
    "audio_enhancements",
    "audio_device_class",
    "input",
];

/// Keys allowed in the structured JSON files (field names of the exported
/// types). A new field must be added here deliberately.
const JSON_KEYS: &[&str] = &[
    // Summary
    "exit_code",
    "refresh_class_hz",
    "target_hz",
    "validity",
    "gates",
    "records",
    "id",
    "ok",
    "reasons",
    "applies",
    "pass",
    "value",
    "threshold",
    "unit",
    "samples",
    "missed_deadlines",
    "not_displayed",
    "heavy_frames",
    "whole_run_p99_interval_ms",
    "whole_run_max_interval_ms",
    "worst_frames",
    "present_us",
    "display_change_ms",
    "visible_notes",
    "effect_instances",
    "lanes_with_effects",
    "bga_frame_shown",
    "library_allocs",
    "voice_steals",
    "bga_pool_exhausted",
    "bga_skipped",
    "obs_pre_roll_ms",
    "obs_post_roll_ms",
    // run.json
    "info",
    "counters",
    "audio",
    "owner",
    "events",
    "profile",
    "kind",
    "obs_encoder",
    "timeline",
    "play_start_us",
    "loops",
    "refresh",
    "num",
    "den",
    "fps_cap",
    "bga_width",
    "bga_height",
    "mix_rate_hz",
    "audio_period_us",
    "presentmon_error",
    "notes_total",
    "notes_played",
    "input_overflows",
    "sound_overflows",
    "app_allocs",
    "pilot_calibrated",
    "unverifiable_spans",
    "capture_from_us",
    "capture_to_us",
    "episodes",
    "start_us",
    "end_us",
    "game_visible",
    "audio_present",
    "t_us",
    // streams
    "streams",
    "frames",
    "audio_periods",
    "bga",
    "produced",
    "lost",
    // app_allocs keys
    "input",
    "logic",
    "render",
    // obs-derived.json
    "sessions_in_log",
    "session",
    "start_marker",
    "connected",
    "stop_marker",
    "post_roll_ms",
    "connect_lead_ms",
    "start_lead_ms",
    "other_output_active",
    "video",
    "base_width",
    "base_height",
    "output_width",
    "output_height",
    "downscale_filter",
    "label",
    "codec",
    "preset",
    "tune",
    "tuning",
    "multipass",
    "target_usage",
    "width",
    "height",
    "encoder",
    "family",
    "rate_control",
    "bitrate_kbps",
    "fps_num",
    "fps_den",
    "frames_output",
    "drawn",
    "drawn_attempted",
    "lagged",
    "encode_skipped",
    "encode_total",
    "bandwidth_dropped",
];

const CSV_COLUMNS: &[&str] = &[
    "present_us",
    "render_us",
    "display_us",
    "display_change_ms",
    "gpu_busy_ms",
    "render_cpu_us",
    "present_call_us",
    "visible_notes",
    "effect_instances",
    "lanes_with_effects",
    "bga_frame_shown",
    "t_us",
    "active_voices",
    "latency_us",
    "due_us",
    "displayed",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Export {
    pub files: BTreeMap<String, Vec<u8>>,
}

/// Private values the scan must not find in an export.
#[derive(Clone, Debug, Default)]
pub struct ScanContext {
    /// User, computer and device friendly names (case-insensitive).
    pub forbidden_names: Vec<String>,
    /// Raw QPC range of the run (origin to last sample), inclusive.
    pub qpc_range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    UnknownFile(String),
    Path(String),
    Name(String),
    RawQpc(String),
    /// A timestamp outside the run's relative-µs range (wrong unit, or a
    /// raw QPC value in a time field).
    TimestampOutOfRange(String),
    Key {
        file: String,
        key: String,
    },
    Unparsable(String),
}

#[derive(Serialize, Deserialize)]
struct RunJson {
    info: RunInfo,
    counters: Counters,
    streams: StreamCounts,
    audio: AudioFacts,
    owner: OwnerPublic,
    events: Vec<RunEvent>,
}

#[derive(Debug)]
pub enum ExportError {
    Missing(&'static str),
    Parse(&'static str),
    Rejected(Vec<Finding>),
}

/// Keeps only whitelisted scalar condition fields.
pub fn sanitize_conditions(conditions: &Map<String, Value>) -> Map<String, Value> {
    conditions
        .iter()
        .filter(|(k, v)| CONDITION_KEYS.contains(&k.as_str()) && !v.is_object() && !v.is_array())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn csv_bytes<T: Serialize>(rows: &[T]) -> Vec<u8> {
    let mut w = csv::Writer::from_writer(Vec::new());
    for r in rows {
        w.serialize(r).expect("serializing to memory");
    }
    w.into_inner().expect("flushing to memory")
}

fn json_bytes<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(v).expect("serializing to memory")
}

/// Builds the export of `run` and rejects it if the scan finds anything.
pub fn build(
    run: &RunData,
    conditions: &Map<String, Value>,
    ctx: &ScanContext,
) -> Result<Export, ExportError> {
    let summary = evaluate(run);
    let run_json = RunJson {
        info: run.info.clone(),
        counters: run.counters.clone(),
        streams: run.streams,
        audio: run.audio.clone(),
        owner: run.owner.clone(),
        events: run.events.clone(),
    };
    let mut files = BTreeMap::new();
    files.insert(SUMMARY.into(), json_bytes(&summary));
    files.insert(RUN.into(), json_bytes(&run_json));
    files.insert(OBS.into(), json_bytes(&run.obs));
    files.insert(
        CONDITIONS.into(),
        json_bytes(&sanitize_conditions(conditions)),
    );
    files.insert(PM_FRAMES.into(), csv_bytes(&run.pm_frames));
    files.insert(FRAMES.into(), csv_bytes(&run.frames));
    files.insert(AUDIO_PERIODS.into(), csv_bytes(&run.audio_periods));
    files.insert(INPUT.into(), csv_bytes(&run.input));
    files.insert(BGA.into(), csv_bytes(&run.bga));
    let export = Export { files };
    let findings = scan(&export, ctx);
    if findings.is_empty() {
        Ok(export)
    } else {
        Err(ExportError::Rejected(findings))
    }
}

/// Every finding in the export; empty means it may leave the PC.
pub fn scan(export: &Export, ctx: &ScanContext) -> Vec<Finding> {
    let mut out = Vec::new();
    let json_keys: BTreeSet<&str> = JSON_KEYS
        .iter()
        .chain(LIBRARY_SCOPES)
        .chain(CONDITION_KEYS)
        .copied()
        .collect();
    for (name, bytes) in &export.files {
        if !FILES.contains(&name.as_str()) {
            out.push(Finding::UnknownFile(name.clone()));
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            out.push(Finding::Unparsable(name.clone()));
            continue;
        };
        if has_path(text) {
            out.push(Finding::Path(name.clone()));
        }
        if contains_name(text, &ctx.forbidden_names) {
            out.push(Finding::Name(name.clone()));
        }
        let json = name
            .ends_with(".json")
            .then(|| serde_json::from_str::<Value>(text));
        // Raw QPC: numbers in the structured files are checked by unit and
        // range below (a relative µs value may legitimately fall inside the
        // raw QPC range on a recently booted PC); free text, which is all of
        // the condition file and every JSON string, is scanned for it.
        if let Some((lo, hi)) = ctx.qpc_range {
            let free_text = match &json {
                _ if name == CONDITIONS => text.to_owned(),
                Some(Ok(v)) => {
                    let mut strings = Vec::new();
                    collect_strings(v, &mut strings);
                    strings.join(" ")
                }
                _ => String::new(),
            };
            if digit_runs(&free_text).any(|v| (lo..=hi).contains(&v)) {
                out.push(Finding::RawQpc(name.clone()));
            }
        }
        let keys: Vec<String> = if let Some(json) = json {
            match json {
                Ok(v) => {
                    let mut k = Vec::new();
                    collect_keys(&v, &mut k);
                    k.into_iter()
                        .filter(|k| !json_keys.contains(k.as_str()))
                        .collect()
                }
                Err(_) => {
                    out.push(Finding::Unparsable(name.clone()));
                    continue;
                }
            }
        } else {
            let header = text.lines().next().unwrap_or("");
            header
                .split(',')
                .filter(|c| !c.is_empty() && !CSV_COLUMNS.contains(c))
                .map(String::from)
                .collect()
        };
        out.extend(keys.into_iter().map(|key| Finding::Key {
            file: name.clone(),
            key,
        }));
    }
    if out.is_empty() {
        out.extend(timestamp_findings(export));
    }
    out
}

/// Every forbidden name, however short: names of 4+ characters match as
/// substrings; shorter ones match literally (punctuation inside included,
/// so `a-b` stays one name) where neither neighbour is alphanumeric, so a
/// 2-letter user name is found as a word but not inside a longer word.
fn contains_name(text: &str, names: &[String]) -> bool {
    let lower = text.to_lowercase();
    names.iter().any(|n| {
        let n = n.trim().to_lowercase();
        if n.is_empty() {
            false
        } else if n.chars().count() >= 4 {
            lower.contains(&n)
        } else {
            lower.match_indices(n.as_str()).any(|(i, m)| {
                let before = lower[..i].chars().next_back();
                let after = lower[i + m.len()..].chars().next();
                !before.is_some_and(char::is_alphanumeric)
                    && !after.is_some_and(char::is_alphanumeric)
            })
        }
    })
}

/// Times may lie this far outside the play window (countdown, capture
/// lead-in and tail).
const TIME_SLACK_US: i64 = 600_000_000;

/// Structural unit check: every µs time in the export lies within the
/// run's relative range.
fn timestamp_findings(e: &Export) -> Vec<Finding> {
    let Ok(run) = read(e) else {
        return vec![Finding::Unparsable(RUN.into())];
    };
    let tl = run.info.timeline;
    let ok_start = (0..=TIME_SLACK_US).contains(&tl.play_start_us);
    let (lo, hi) = (-TIME_SLACK_US, tl.play_end_us() + TIME_SLACK_US);
    let ok = |t: i64| (lo..=hi).contains(&t);
    let mut out = vec![];
    if !ok_start {
        out.push(Finding::TimestampOutOfRange(RUN.into()));
    }
    let mut check = |file: &str, mut times: Box<dyn Iterator<Item = i64> + '_>| {
        if !times.all(ok) {
            out.push(Finding::TimestampOutOfRange(file.into()));
        }
    };
    check(
        RUN,
        Box::new(
            run.events
                .iter()
                .map(|x| x.t_us)
                .chain(
                    run.audio
                        .episodes
                        .iter()
                        .flat_map(|x| [x.start_us, x.end_us]),
                )
                .chain(run.audio.capture_from_us)
                .chain(run.audio.capture_to_us),
        ),
    );
    check(
        PM_FRAMES,
        Box::new(
            run.pm_frames
                .iter()
                .flat_map(|f| std::iter::once(f.present_us).chain(f.display_us)),
        ),
    );
    check(
        FRAMES,
        Box::new(
            run.frames
                .iter()
                .flat_map(|f| std::iter::once(f.render_us).chain(f.present_us)),
        ),
    );
    check(
        AUDIO_PERIODS,
        Box::new(run.audio_periods.iter().map(|x| x.t_us)),
    );
    check(INPUT, Box::new(run.input.iter().map(|x| x.t_us)));
    check(BGA, Box::new(run.bga.iter().map(|x| x.due_us)));
    if let Ok(summary) = json::<Summary>(e, SUMMARY) {
        check(
            SUMMARY,
            Box::new(summary.records.worst_frames.iter().map(|w| w.present_us)),
        );
    }
    out
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn has_path(text: &str) -> bool {
    let b = text.as_bytes();
    let drive = b
        .windows(3)
        .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && (w[2] == b'\\' || w[2] == b'/'));
    drive
        || text.contains('\\')
        || [
            "/home/",
            "/Users/",
            "/mnt/",
            "/tmp/",
            "file:",
            "%USERPROFILE%",
            "AppData",
        ]
        .iter()
        .any(|p| text.contains(p))
}

/// Integer digit runs of 6+ digits; fractional parts (after a '.') are
/// skipped so float digits are not mistaken for raw QPC values.
fn digit_runs(text: &str) -> impl Iterator<Item = u64> + '_ {
    let b = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() {
            if !b[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let fractional = start > 0 && b[start - 1] == b'.';
            if !fractional
                && i - start >= 6
                && let Ok(v) = text[start..i].parse()
            {
                return Some(v);
            }
        }
        None
    })
}

fn collect_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                collect_keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_keys(x, out)),
        _ => {}
    }
}

fn file<'a>(e: &'a Export, name: &'static str) -> Result<&'a [u8], ExportError> {
    e.files
        .get(name)
        .map(Vec::as_slice)
        .ok_or(ExportError::Missing(name))
}

fn json<T: DeserializeOwned>(e: &Export, name: &'static str) -> Result<T, ExportError> {
    serde_json::from_slice(file(e, name)?).map_err(|_| ExportError::Parse(name))
}

fn rows<T: DeserializeOwned>(e: &Export, name: &'static str) -> Result<Vec<T>, ExportError> {
    csv::Reader::from_reader(file(e, name)?)
        .deserialize()
        .collect::<Result<_, _>>()
        .map_err(|_| ExportError::Parse(name))
}

/// Rebuilds the `RunData` an export was made from.
pub fn read(e: &Export) -> Result<RunData, ExportError> {
    let r: RunJson = json(e, RUN)?;
    let obs: Option<ObsDerived> = json(e, OBS)?;
    Ok(RunData {
        info: r.info,
        counters: r.counters,
        streams: r.streams,
        audio: r.audio,
        owner: r.owner,
        obs,
        events: r.events,
        pm_frames: rows::<PmFrame>(e, PM_FRAMES)?,
        frames: rows::<SpikeFrame>(e, FRAMES)?,
        audio_periods: rows::<AudioPeriod>(e, AUDIO_PERIODS)?,
        input: rows::<InputLatency>(e, INPUT)?,
        bga: rows::<BgaDue>(e, BGA)?,
    })
}

#[derive(Debug)]
pub struct Recomputed {
    pub summary: Summary,
    /// The recomputed summary equals the export's `summary.json`.
    pub matches_recorded: bool,
}

/// `report --from-export`: scans the export (no private context is known
/// off the PC, so only paths, keys and files are checked), recomputes the
/// summary from the derived data and compares it with the recorded one.
pub fn recompute(e: &Export) -> Result<Recomputed, ExportError> {
    let findings = scan(e, &ScanContext::default());
    if !findings.is_empty() {
        return Err(ExportError::Rejected(findings));
    }
    let recorded: Summary = json(e, SUMMARY)?;
    let summary = evaluate(&read(e)?);
    Ok(Recomputed {
        matches_recorded: summary == recorded,
        summary,
    })
}
