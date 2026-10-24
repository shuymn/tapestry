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

use crate::conditions::{NESTED_KEYS, PUBLIC_KEYS, PublicConditions};
use crate::evaluate::{Summary, evaluate};
use crate::obslog::ObsDerived;
use crate::run::{
    AudioFacts, AudioPeriod, BgaDue, Counters, InputLatency, LIBRARY_SCOPES, OwnerPublic, PmFrame,
    RunData, RunEvent, RunInfo, SpikeFrame, StreamCounts,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
pub const REPORT: &str = "report.md";
pub const FILES: [&str; 10] = [
    SUMMARY,
    RUN,
    OBS,
    CONDITIONS,
    PM_FRAMES,
    FRAMES,
    AUDIO_PERIODS,
    INPUT,
    BGA,
    REPORT,
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
    "bga_prediction_disagreements",
    "obs_pre_roll_ms",
    "obs_post_roll_ms",
    "present_modes",
    "mode",
    "stream_error",
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
    "class_hz",
    "target_period_ms",
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
    "teardown_confirmed",
    "encode_lag_ambiguous",
];

const CSV_COLUMNS: &[&str] = &[
    "present_us",
    "render_us",
    "display_us",
    "display_change_ms",
    "gpu_busy_ms",
    "render_cpu_us",
    "present_call_us",
    "present_count",
    "bga_index",
    "visible_notes",
    "effect_instances",
    "lanes_with_effects",
    "bga_frame_shown",
    "t_us",
    "active_voices",
    "latency_us",
    "due_us",
    "displayed",
    "present_mode",
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
    /// A time or duration field outside its plausible range (wrong unit,
    /// or a raw QPC value in it).
    OutOfRange(String),
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
    conditions: &PublicConditions,
    ctx: &ScanContext,
) -> Result<Export, ExportError> {
    build_with_summary(run, conditions, ctx).map(|(e, _)| e)
}

/// `build`, also returning the summary it evaluated (the run is evaluated
/// once per report).
pub fn build_with_summary(
    run: &RunData,
    conditions: &PublicConditions,
    ctx: &ScanContext,
) -> Result<(Export, Summary), ExportError> {
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
    files.insert(CONDITIONS.into(), json_bytes(conditions));
    files.insert(
        REPORT.into(),
        markdown(&summary, run, conditions).into_bytes(),
    );
    files.insert(PM_FRAMES.into(), csv_bytes(&run.pm_frames));
    files.insert(FRAMES.into(), csv_bytes(&run.frames));
    files.insert(AUDIO_PERIODS.into(), csv_bytes(&run.audio_periods));
    files.insert(INPUT.into(), csv_bytes(&run.input));
    files.insert(BGA.into(), csv_bytes(&run.bga));
    let export = Export { files };
    let findings = scan(&export, ctx);
    if findings.is_empty() {
        Ok((export, summary))
    } else {
        Err(ExportError::Rejected(findings))
    }
}

/// Every finding in the export; empty means it may leave the PC. Each file
/// is parsed once; its scalar fields serve the raw-QPC scan and the
/// structural check.
pub fn scan(export: &Export, ctx: &ScanContext) -> Vec<Finding> {
    let mut out = Vec::new();
    let json_keys: BTreeSet<&str> = JSON_KEYS
        .iter()
        .chain(LIBRARY_SCOPES)
        .chain(PUBLIC_KEYS)
        .chain(NESTED_KEYS)
        .copied()
        .collect();
    // Scalar fields of the files the structural check covers (`None`:
    // unparsable).
    let mut structural: Vec<(&String, Option<Fields>)> = vec![];
    let in_qpc = |v: &str| {
        ctx.qpc_range
            .is_some_and(|(lo, hi)| digit_runs(v).any(|x| (lo..=hi).contains(&x)))
    };
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
        // The conditions and the report are scanned as whole text and have
        // no time fields; the others by field.
        let whole_text = name == CONDITIONS || name == REPORT;
        let json = name
            .ends_with(".json")
            .then(|| serde_json::from_str::<Value>(text));
        let fields = match &json {
            _ if whole_text => None,
            Some(Ok(v)) => Some(json_fields(v)),
            Some(Err(_)) => None,
            None => csv_fields(text),
        };
        // Raw QPC: time and duration fields (`*_us`, `*_ms`) are checked by
        // unit and range in `structural_findings` (a relative µs value may
        // legitimately fall inside the raw QPC range on a recently booted
        // PC); everything else, text and counts alike, is scanned for it.
        let raw_qpc = if whole_text {
            in_qpc(text)
        } else {
            fields
                .iter()
                .flatten()
                .any(|(k, v)| structural_kind(k).is_none() && in_qpc(v))
        };
        if raw_qpc {
            out.push(Finding::RawQpc(name.clone()));
        }
        let keys: Vec<String> = if name == REPORT {
            // Prose: no keys or columns; times are printed in seconds so the
            // raw-QPC scan above can cover the whole text.
            vec![]
        } else if let Some(json) = &json {
            match json {
                Ok(v) => {
                    let mut k = Vec::new();
                    collect_keys(v, &mut k);
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
        if !whole_text {
            structural.push((name, fields));
        }
    }
    if out.is_empty() {
        out.extend(structural_findings(export, &structural));
    }
    out
}

/// Every forbidden name, however short: names of 4+ characters match as
/// substrings; shorter ones match literally (punctuation inside included,
/// so `a-b` stays one name) where neither neighbour is alphanumeric, so a
/// 2-letter user name is found as a word but not inside a longer word.
fn contains_name(text: &str, names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    names.iter().any(|n| {
        let n = n.trim().to_lowercase();
        if n.is_empty() {
            false
        } else if n.chars().count() >= 4 {
            lower.contains(&n)
        } else {
            // Every start position, overlapping ones included: `a-a` in
            // "extra-a-a" first hits inside "extra" (rejected), then at the
            // overlapping valid position.
            lower.char_indices().any(|(i, _)| {
                lower[i..].starts_with(n.as_str()) && {
                    let before = lower[..i].chars().next_back();
                    let after = lower[i + n.len()..].chars().next();
                    !before.is_some_and(char::is_alphanumeric)
                        && !after.is_some_and(char::is_alphanumeric)
                }
            })
        }
    })
}

/// Times may lie this far outside the play window (countdown, capture
/// lead-in and tail).
const TIME_SLACK_US: i64 = 600_000_000;
const DAY_MS: f64 = 86_400_000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Structural {
    /// Relative µs time.
    Time,
    PlayStart,
    /// Duration or offset with its plausible range, in the field's unit.
    Range(f64, f64),
}

/// How a `*_us` / `*_ms` field is checked; `None` for other keys. An
/// unlisted `*_us` / `*_ms` key is `Range(NaN, NaN)`, which nothing passes,
/// so a new time field must be classified here deliberately.
fn structural_kind(key: &str) -> Option<Structural> {
    use Structural::*;
    Some(match key {
        "play_start_us" => PlayStart,
        "present_us" | "display_us" | "render_us" | "t_us" | "due_us" | "start_us" | "end_us"
        | "capture_from_us" | "capture_to_us" => Time,
        // Per-frame and per-event durations: at most 10 s.
        "latency_us" | "render_cpu_us" | "present_call_us" => Range(0.0, 10_000_000.0),
        "audio_period_us" => Range(1.0, 1_000_000.0),
        "display_change_ms" | "gpu_busy_ms" => Range(0.0, 60_000.0),
        "whole_run_p99_interval_ms" | "whole_run_max_interval_ms" => Range(0.0, 600_000.0),
        "target_period_ms" => Range(0.1, 1_000.0),
        // OBS offsets against the play window.
        "connect_lead_ms" | "start_lead_ms" | "post_roll_ms" | "obs_pre_roll_ms"
        | "obs_post_roll_ms" => Range(-DAY_MS, DAY_MS),
        k if k.ends_with("_us") || k.ends_with("_ms") => Range(f64::NAN, f64::NAN),
        _ => return None,
    })
}

/// `(key, raw value)` pairs of a file's scalars.
type Fields = Vec<(String, String)>;

/// `(key, raw value)` of every scalar of a JSON file (array items take
/// their parent's key).
fn json_fields(v: &Value) -> Fields {
    fn walk(key: &str, v: &Value, out: &mut Fields) {
        match v {
            Value::Object(m) => m.iter().for_each(|(k, x)| walk(k, x, out)),
            Value::Array(a) => a.iter().for_each(|x| walk(key, x, out)),
            Value::String(s) => out.push((key.to_owned(), s.clone())),
            Value::Number(n) => out.push((key.to_owned(), n.to_string())),
            Value::Bool(_) | Value::Null => {}
        }
    }
    let mut out = Vec::new();
    walk("", v, &mut out);
    out
}

/// `(column, raw value)` of every non-empty CSV cell; `None` if unparsable.
fn csv_fields(text: &str) -> Option<Fields> {
    let mut out = Vec::new();
    let mut rdr = csv::Reader::from_reader(text.as_bytes());
    let header = rdr.headers().ok()?.clone();
    for rec in rdr.records() {
        for (k, v) in header.iter().zip(rec.ok()?.iter()) {
            if !v.is_empty() {
                out.push((k.to_owned(), v.to_owned()));
            }
        }
    }
    Some(out)
}

/// Structural unit check over the scanned files' fields: every time and
/// duration field lies in its plausible range (a raw QPC value or a wrong
/// unit does not).
fn structural_findings(e: &Export, files: &[(&String, Option<Fields>)]) -> Vec<Finding> {
    let Ok(run) = json::<RunJson>(e, RUN) else {
        return vec![Finding::Unparsable(RUN.into())];
    };
    let tl = run.info.timeline;
    let (lo, hi) = (
        -TIME_SLACK_US as f64,
        (tl.play_end_us() + TIME_SLACK_US) as f64,
    );
    let mut out = vec![];
    for (name, fields) in files {
        let Some(fields) = fields else {
            out.push(Finding::Unparsable((*name).clone()));
            continue;
        };
        let bad = fields.iter().any(|(k, v)| {
            let Some(kind) = structural_kind(k) else {
                return false;
            };
            let Ok(x) = v.parse::<f64>() else {
                return true;
            };
            match kind {
                Structural::PlayStart => !(0.0..=TIME_SLACK_US as f64).contains(&x),
                Structural::Time => !(lo..=hi).contains(&x),
                Structural::Range(a, b) => !(a..=b).contains(&x),
            }
        });
        if bad {
            out.push(Finding::OutOfRange((*name).clone()));
        }
    }
    out
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
    /// The recomputed summary and report equal the export's `summary.json`
    /// and `report.md`.
    pub matches_recorded: bool,
}

/// `report --from-export`: scans the export, recomputes the summary from the
/// derived data and compares it with the recorded one. No private context
/// is known off the PC, so the scan checks paths, keys, files and the
/// structural time and duration ranges, but not names or raw QPC values.
pub fn recompute(e: &Export) -> Result<Recomputed, ExportError> {
    let findings = scan(e, &ScanContext::default());
    if !findings.is_empty() {
        return Err(ExportError::Rejected(findings));
    }
    let recorded: Summary = json(e, SUMMARY)?;
    let conditions: PublicConditions = json(e, CONDITIONS)?;
    let run = read(e)?;
    let summary = evaluate(&run);
    let report = markdown(&summary, &run, &conditions);
    Ok(Recomputed {
        matches_recorded: summary == recorded && file(e, REPORT)? == report.as_bytes(),
        summary,
    })
}

fn ms(v: Option<f64>) -> String {
    v.map_or("—".into(), |v| format!("{v:.3}"))
}

/// `report.md`: the run's verdict, validity, gates, records, worst frames
/// and public conditions. Times are seconds (fractional digits), never raw
/// integers, so the export scan can search the whole text for raw QPC.
pub fn markdown(s: &Summary, run: &RunData, cond: &PublicConditions) -> String {
    use std::fmt::Write as _;
    let mut m = String::new();
    let _ = writeln!(m, "# Stage 0 spike run report\n");
    let verdict = match s.exit_code {
        0 => "pass",
        1 => "gate failed",
        _ => "invalid",
    };
    let i = &run.info;
    let _ = writeln!(
        m,
        "Result: **{verdict}** (exit {}). Profile {:?}, kind {:?}, class {} Hz, refresh {}/{} Hz, cap {}, loops {}, G1 T {} ms.\n",
        s.exit_code,
        i.profile,
        i.kind,
        i.class_hz,
        i.refresh.num,
        i.refresh.den,
        i.fps_cap.map_or("none".into(), |c| c.to_string()),
        i.timeline.loops,
        ms(s.target_period_ms),
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
            ms(g.value),
            g.threshold,
            g.unit,
            g.samples
        );
    }
    let r = &s.records;
    let modes = if r.present_modes.is_empty() {
        "none".to_owned()
    } else {
        r.present_modes
            .iter()
            .map(|c| format!("{:?} {}", c.mode, c.frames))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let libs = r
        .library_allocs
        .iter()
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        m,
        "\n## Records\n\n- Missed deadlines (G1r): {} ({} not displayed) of {} heavy-window frames\n- Whole run p99 / max interval: {} / {} ms\n- Voice steals: {}; BGA due not displayed: {}\n- Present modes (PresentMon rows): {modes}\n- Library allocations (G6r): {libs}",
        r.missed_deadlines,
        r.not_displayed,
        r.heavy_frames,
        ms(r.whole_run_p99_interval_ms),
        ms(r.whole_run_max_interval_ms),
        r.voice_steals,
        r.bga_skipped,
    );
    let _ = writeln!(
        m,
        "\n## Worst frames\n\n| Present s | Interval ms | Visible notes | Effects | Lanes lit | BGA shown |\n| --- | --- | --- | --- | --- | --- |"
    );
    for w in &r.worst_frames {
        let _ = writeln!(
            m,
            "| {:.6} | {} | {} | {} | {} | {} |",
            w.present_us as f64 / 1e6,
            ms(Some(w.display_change_ms)),
            w.visible_notes,
            w.effect_instances,
            w.lanes_with_effects,
            w.bga_frame_shown
        );
    }
    let _ = writeln!(m, "\n## Conditions (public)\n");
    if let Ok(Value::Object(c)) = serde_json::to_value(cond) {
        for (k, v) in &c {
            let _ = writeln!(m, "- {k} = {v}");
        }
    }
    m
}
