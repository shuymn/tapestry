//! Public measurement conditions (design §3.5) and the pure pieces that
//! derive them: the GPU state sampled during play, the effective power mode
//! during play, the adapter the run used, the local wall clock, and the
//! ladder command after a failed run.
//!
//! `conditions.json` on the measurement PC holds everything the app
//! collected, private fields included. Only `PublicConditions` leaves the
//! PC: a typed record, so nested facts (other monitors' modes, the GPU
//! state) survive the export instead of being dropped as untyped arrays.

use crate::evaluate::Summary;
use crate::run::{Profile, RunInfo, RunKind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One active display's exact mode, by GDI name (`DISPLAY1`); friendly
/// monitor names are private and never stored here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DisplayMode {
    pub gdi_name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_num: u32,
    pub refresh_den: u32,
}

/// PresentMon's `HybridPresent` column over the spike's rows (a hybrid
/// present crosses adapters, for example an iGPU rendering for a display on
/// the discrete GPU).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HybridPresent {
    pub rows: u64,
    pub hybrid_rows: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PstateShare {
    pub pstate: String,
    pub pct: f64,
}

/// The selected GPU's performance state during play.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuState {
    /// `nvidia-smi`, or `unavailable: <reason>` (fixed reasons only).
    pub source: String,
    pub samples: u32,
    pub pstates: Vec<PstateShare>,
    pub core_mhz_min: Option<u32>,
    pub core_mhz_p50: Option<u32>,
    pub core_mhz_max: Option<u32>,
    pub mem_mhz_min: Option<u32>,
    pub mem_mhz_p50: Option<u32>,
    pub mem_mhz_max: Option<u32>,
}

impl GpuState {
    pub fn unavailable(reason: &str) -> Self {
        Self {
            source: format!("unavailable: {reason}"),
            ..Self::default()
        }
    }
}

/// The public conditions of one run (§3.5). Missing facts are `null`, never
/// guessed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PublicConditions {
    // Build and content.
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub build_profile: Option<String>,
    pub scene_hash: Option<String>,
    pub chart_hash: Option<String>,
    pub bga_sha256: Option<String>,
    pub bga_codec: Option<String>,
    // Tool versions.
    pub wgpu_version: Option<String>,
    pub ffmpeg_version: Option<String>,
    pub presentmon_version: Option<String>,
    pub obs_version: Option<String>,
    // Machine.
    pub cpu_brand: Option<String>,
    pub cpu_cores: Option<u32>,
    pub ram_gb: Option<u32>,
    pub pc_form: Option<String>,
    // GPU (the adapter the run used).
    pub gpu_adapter: Option<String>,
    pub gpu_vram_mb: Option<u64>,
    pub gpu_driver: Option<String>,
    pub gpu_display_attached: Option<String>,
    pub hybrid_present: Option<HybridPresent>,
    pub gpu_state: Option<GpuState>,
    /// The per-program driver power setting as the owner declared it
    /// (`measure --gpu-power-mode`); not readable without vendor APIs.
    pub gpu_power_mode_declared: Option<String>,
    // Windows and power.
    pub windows_build: Option<String>,
    pub windows_ubr: Option<u32>,
    pub power_source: Option<String>,
    pub power_scheme: Option<String>,
    /// Effective power modes seen during play, in order.
    pub power_mode: Option<String>,
    pub game_mode: Option<String>,
    pub hags: Option<String>,
    // Displays.
    pub run_display: Option<String>,
    pub refresh_num: Option<u64>,
    pub refresh_den: Option<u64>,
    pub vrr: Option<bool>,
    pub aspect_ratio: Option<String>,
    pub render_width: Option<u32>,
    pub render_height: Option<u32>,
    pub tearing_support: Option<bool>,
    pub monitor_count: Option<u32>,
    pub displays: Vec<DisplayMode>,
    pub display_connection: Option<String>,
    // Audio.
    pub audio_mix_format: Option<String>,
    pub audio_share_mode: Option<String>,
    pub audio_period_min: Option<u32>,
    pub audio_period_max: Option<u32>,
    pub audio_period_step: Option<u32>,
    pub audio_period_default: Option<u32>,
    pub audio_period_chosen: Option<u32>,
    pub audio_backend: Option<String>,
    pub audio_enhancements: Option<String>,
    pub audio_device_class: Option<String>,
    pub input: Option<String>,
}

/// Top-level public condition keys (the `PublicConditions` fields).
pub const PUBLIC_KEYS: &[&str] = &[
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
    "gpu_driver",
    "gpu_display_attached",
    "hybrid_present",
    "gpu_state",
    "gpu_power_mode_declared",
    "windows_build",
    "windows_ubr",
    "power_source",
    "power_scheme",
    "power_mode",
    "game_mode",
    "hags",
    "run_display",
    "refresh_num",
    "refresh_den",
    "vrr",
    "aspect_ratio",
    "render_width",
    "render_height",
    "tearing_support",
    "monitor_count",
    "displays",
    "display_connection",
    "audio_mix_format",
    "audio_share_mode",
    "audio_period_min",
    "audio_period_max",
    "audio_period_step",
    "audio_period_default",
    "audio_period_chosen",
    "audio_backend",
    "audio_enhancements",
    "audio_device_class",
    "input",
];

/// Keys inside the nested public records.
pub const NESTED_KEYS: &[&str] = &[
    "gdi_name",
    "width",
    "height",
    "rows",
    "hybrid_rows",
    "source",
    "samples",
    "pstates",
    "pstate",
    "pct",
    "core_mhz_min",
    "core_mhz_p50",
    "core_mhz_max",
    "mem_mhz_min",
    "mem_mhz_p50",
    "mem_mhz_max",
];

impl PublicConditions {
    /// The public fields of the private `conditions.json` map. A public
    /// field of the wrong type is an error, not a silent omission.
    pub fn from_conditions(c: &Map<String, Value>) -> Result<Self, String> {
        let public: Map<String, Value> = c
            .iter()
            .filter(|(k, v)| PUBLIC_KEYS.contains(&k.as_str()) && !v.is_null())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        serde_json::from_value(Value::Object(public))
            .map_err(|e| format!("conditions: a public field has the wrong type ({e})"))
    }
}

/// A hardware adapter as the measurement PC enumerated it (private
/// `conditions.json` → `gpu_adapters`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterRecord {
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub vram_mb: u64,
    pub driver: Option<String>,
}

pub const NVIDIA_VENDOR_ID: u32 = 0x10de;

/// The enumerated adapter the run actually used: same vendor and device id,
/// and the same name when several share them.
pub fn select_adapter<'a>(
    adapters: &'a [AdapterRecord],
    vendor_id: u32,
    device_id: u32,
    name: &str,
) -> Option<&'a AdapterRecord> {
    let same: Vec<&AdapterRecord> = adapters
        .iter()
        .filter(|a| a.vendor_id == vendor_id && a.device_id == device_id)
        .collect();
    match same.as_slice() {
        [one] => Some(one),
        many => many.iter().copied().find(|a| a.name == name),
    }
}

const DAY_MS: i64 = 86_400_000;

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Local wall-clock ms since 1970-01-01 00:00 local time.
pub fn local_ms(y: i64, m: u32, d: u32, ms_of_day: i64) -> i64 {
    days_from_civil(y, m, d) * DAY_MS + ms_of_day
}

/// `YYYY/MM/DD HH:MM:SS.mmm` (nvidia-smi's timestamp) as local ms.
pub fn parse_local_timestamp(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.split(['/', '-']);
    let (y, m, day) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    let ms = (h * 60 + min) * 60_000 + (sec * 1000.0).round() as i64;
    Some(local_ms(y, m, day, ms))
}

/// Local ms of the day the OBS log's first line belongs to. OBS log lines
/// carry only a time of day, and the log necessarily began before the run
/// (measure starts OBS first), less than a day before: the base is the
/// latest midnight with `base + first_line_tod <= origin`.
pub fn obs_log_day_base(origin_local_ms: i64, first_line_tod_ms: i64) -> i64 {
    (origin_local_ms - first_line_tod_ms).div_euclid(DAY_MS) * DAY_MS
}

fn nearest_rank(sorted: &[u32], p: usize) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    Some(sorted[rank - 1])
}

/// The GPU state during play from `nvidia-smi --query-gpu=timestamp,name,
/// pstate,clocks.gr,clocks.mem --format=csv,noheader,nounits` lines, for
/// the GPU named `gpu` and samples in `[from, to]` (local ms).
pub fn gpu_state_from_smi(text: &str, gpu: &str, from: i64, to: i64) -> GpuState {
    let mut pstates: Vec<(String, u32)> = vec![];
    let (mut core, mut mem) = (vec![], vec![]);
    for line in text.lines() {
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        let [ts, name, pstate, gr, memclk] = f.as_slice() else {
            continue;
        };
        let Some(t) = parse_local_timestamp(ts) else {
            continue;
        };
        if *name != gpu || t < from || t > to {
            continue;
        }
        let (Ok(gr), Ok(memclk)) = (gr.parse::<u32>(), memclk.parse::<u32>()) else {
            continue;
        };
        match pstates.iter_mut().find(|(p, _)| p == pstate) {
            Some((_, n)) => *n += 1,
            None => pstates.push(((*pstate).to_owned(), 1)),
        }
        core.push(gr);
        mem.push(memclk);
    }
    if core.is_empty() {
        return GpuState::unavailable("no samples during play");
    }
    core.sort_unstable();
    mem.sort_unstable();
    pstates.sort();
    let n = core.len();
    GpuState {
        source: "nvidia-smi".into(),
        samples: n as u32,
        pstates: pstates
            .into_iter()
            .map(|(pstate, c)| PstateShare {
                pstate,
                pct: (f64::from(c) * 1000.0 / n as f64).round() / 10.0,
            })
            .collect(),
        core_mhz_min: core.first().copied(),
        core_mhz_p50: nearest_rank(&core, 50),
        core_mhz_max: core.last().copied(),
        mem_mhz_min: mem.first().copied(),
        mem_mhz_p50: nearest_rank(&mem, 50),
        mem_mhz_max: mem.last().copied(),
    }
}

/// Windows' effective power mode (`EFFECTIVE_POWER_MODE`) by value.
pub fn power_mode_name(mode: i32) -> &'static str {
    match mode {
        0 => "battery-saver",
        1 => "better-battery",
        2 => "balanced",
        3 => "high-performance",
        4 => "max-performance",
        5 => "game-mode",
        6 => "mixed-reality",
        _ => "other",
    }
}

/// The effective power modes in force during `[from, to]` (run µs), in
/// order and without repeats, from the notifications `(t_us, mode)`: the
/// mode at `from` is the last notification before it.
pub fn power_modes_during(changes: &[(i64, i32)], from: i64, to: i64) -> Option<String> {
    let mut modes: Vec<&str> = vec![];
    let before = changes.iter().rev().find(|(t, _)| *t <= from);
    let inside = changes.iter().filter(|(t, _)| *t > from && *t <= to);
    for (_, m) in before.into_iter().chain(inside) {
        let name = power_mode_name(*m);
        if modes.last() != Some(&name) {
            modes.push(name);
        }
    }
    (!modes.is_empty()).then(|| modes.join(", "))
}

/// What to run after a failed run (design §4.2 ladders): the streaming
/// profile for an OBS cell that failed under standard, and the next audio
/// period after a G5 failure. Flags only; the wrapper adds the run name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ladder {
    /// Run options for the profile rung, e.g. `--profile streaming --fps-cap 120`.
    pub profile: Option<String>,
    /// Run options for the audio rung, e.g. `--audio-period 480`.
    pub audio: Option<String>,
    /// Why no further rung exists, when one would be needed.
    pub notes: Vec<String>,
}

/// The engine period ladder of the endpoint (frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodLadder {
    pub chosen: u32,
    pub step: u32,
    pub max: u32,
}

pub fn ladder(summary: &Summary, info: &RunInfo, periods: Option<PeriodLadder>) -> Ladder {
    let failed = |id: &str| {
        summary
            .gates
            .iter()
            .any(|g| g.id == id && g.applies && !g.pass)
    };
    let mut l = Ladder::default();
    if summary.exit_code != 1 {
        return l;
    }
    let game_side = ["G1", "G4", "G5", "O1", "O2"].iter().any(|g| failed(g));
    if info.kind == RunKind::Obs && game_side {
        match info.profile {
            Profile::Standard => {
                let cap = if info.class_hz == 240 { 120 } else { 60 };
                l.profile = Some(format!("--profile streaming --fps-cap {cap}"));
            }
            Profile::Streaming => l
                .notes
                .push("the cell fails under streaming: design revision (section 6 IF-THEN)".into()),
            Profile::NoBga => {}
        }
    }
    if failed("G5") {
        match periods {
            Some(p) if p.chosen + p.step.max(1) <= p.max => {
                l.audio = Some(format!("--audio-period {}", p.chosen + p.step.max(1)));
            }
            Some(_) => l
                .notes
                .push("no longer shared-mode period on this endpoint".into()),
            None => l.notes.push("audio period ladder unknown".into()),
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluate::evaluate;
    use crate::synth::passing_run;

    #[test]
    fn local_dates_count_days_across_months_and_years() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(
            days_from_civil(2026, 1, 1) - days_from_civil(2025, 12, 31),
            1
        );
        assert_eq!(
            parse_local_timestamp("2026/10/25 00:00:01.500"),
            Some(local_ms(2026, 10, 25, 1_500))
        );
        assert_eq!(parse_local_timestamp("timestamp"), None);
    }

    #[test]
    fn obs_day_base_handles_both_sides_of_midnight() {
        let d = days_from_civil(2026, 10, 25) * DAY_MS;
        // The run crosses midnight; OBS started before it on the same day.
        assert_eq!(obs_log_day_base(d + 86_340_000, 86_310_000), d);
        // OBS started before midnight, the run after it.
        assert_eq!(obs_log_day_base(d + DAY_MS + 10_000, 86_390_000), d);
        // Both after midnight.
        assert_eq!(obs_log_day_base(d + 60_000, 30_000), d);
    }

    #[test]
    fn gpu_state_counts_only_the_named_gpu_during_play() {
        let t0 = local_ms(2026, 10, 25, 0);
        let text = "\
2026/10/24 23:59:59.900, GPU A, P8, 210, 405
2026/10/25 00:00:00.100, GPU A, P5, 900, 810
2026/10/25 00:00:00.200, GPU B, P0, 2500, 11501
2026/10/25 00:00:00.300, GPU A, P0, 2550, 11501
2026/10/25 00:00:00.400, GPU A, P0, 2550, 11501
2026/10/25 00:00:00.500, GPU A, [N/A], x, 1
2026/10/25 00:00:05.000, GPU A, P8, 210, 405
";
        let s = gpu_state_from_smi(text, "GPU A", t0, t0 + 1_000);
        assert_eq!(s.source, "nvidia-smi");
        assert_eq!(s.samples, 3);
        assert_eq!(
            s.pstates,
            vec![
                PstateShare {
                    pstate: "P0".into(),
                    pct: 66.7
                },
                PstateShare {
                    pstate: "P5".into(),
                    pct: 33.3
                }
            ]
        );
        assert_eq!(
            (s.core_mhz_min, s.core_mhz_p50, s.core_mhz_max),
            (Some(900), Some(2550), Some(2550))
        );
        assert_eq!((s.mem_mhz_min, s.mem_mhz_max), (Some(810), Some(11501)));
        let none = gpu_state_from_smi(text, "GPU C", t0, t0 + 1_000);
        assert!(none.source.starts_with("unavailable"));
    }

    #[test]
    fn power_modes_start_with_the_mode_in_force() {
        let changes = [
            (-5_000_000, 2),
            (1_000_000, 5),
            (2_000_000, 5),
            (9_000_000, 2),
        ];
        assert_eq!(
            power_modes_during(&changes, 0, 5_000_000).as_deref(),
            Some("balanced, game-mode")
        );
        assert_eq!(power_modes_during(&[], 0, 1), None);
    }

    fn adapter(name: &str, vendor_id: u32, device_id: u32, vram_mb: u64) -> AdapterRecord {
        AdapterRecord {
            name: name.into(),
            vendor_id,
            device_id,
            vram_mb,
            driver: Some(format!("{vendor_id}.1")),
        }
    }

    #[test]
    fn the_used_adapter_is_selected_not_the_first() {
        let two = [
            adapter("Discrete GPU", NVIDIA_VENDOR_ID, 0x2702, 16_000),
            adapter("Integrated GPU", 0x1002, 0x164e, 512),
        ];
        let used = select_adapter(&two, 0x1002, 0x164e, "Integrated GPU").unwrap();
        assert_eq!(used.vram_mb, 512);
        let twins = [
            adapter("Card one", NVIDIA_VENDOR_ID, 1, 8_000),
            adapter("Card two", NVIDIA_VENDOR_ID, 1, 12_000),
        ];
        assert_eq!(
            select_adapter(&twins, NVIDIA_VENDOR_ID, 1, "Card two")
                .unwrap()
                .vram_mb,
            12_000
        );
        assert_eq!(select_adapter(&two, 0x8086, 1, "Other"), None);
    }

    #[test]
    fn every_public_field_is_a_listed_key() {
        let full = PublicConditions {
            hybrid_present: Some(HybridPresent::default()),
            gpu_state: Some(GpuState {
                pstates: vec![PstateShare {
                    pstate: "P0".into(),
                    pct: 100.0,
                }],
                ..GpuState::default()
            }),
            displays: vec![DisplayMode::default()],
            ..PublicConditions::default()
        };
        let v = serde_json::to_value(&full).unwrap();
        let top: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(top.len(), PUBLIC_KEYS.len());
        for k in top {
            assert!(PUBLIC_KEYS.contains(&k.as_str()), "{k}");
        }
        for nested in ["hybrid_present", "gpu_state", "displays"] {
            let mut o = v[nested].clone();
            if let Some(a) = o.as_array() {
                o = a[0].clone();
            }
            for (k, x) in o.as_object().unwrap() {
                assert!(NESTED_KEYS.iter().chain(PUBLIC_KEYS).any(|n| n == k), "{k}");
                if let Some(a) = x.as_array() {
                    for k in a[0].as_object().unwrap().keys() {
                        assert!(NESTED_KEYS.iter().chain(PUBLIC_KEYS).any(|n| n == k), "{k}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_wrongly_typed_public_field_is_an_error() {
        let mut m = Map::new();
        m.insert("vrr".into(), Value::String("no".into()));
        assert!(PublicConditions::from_conditions(&m).is_err());
        m.insert("vrr".into(), Value::Bool(false));
        m.insert("user_name".into(), Value::String("private".into()));
        let p = PublicConditions::from_conditions(&m).unwrap();
        assert_eq!(p.vrr, Some(false));
    }

    fn failing(run: &mut crate::run::RunData, gate: &str) -> Summary {
        let mut s = evaluate(run);
        s.exit_code = 1;
        for g in &mut s.gates {
            if g.id == gate {
                g.applies = true;
                g.pass = false;
            }
        }
        s
    }

    #[test]
    fn ladders_follow_the_failed_gate_profile_and_period() {
        let mut run = passing_run();
        run.info.kind = RunKind::Obs;
        run.info.class_hz = 240;
        let p = Some(PeriodLadder {
            chosen: 441,
            step: 1,
            max: 441,
        });
        let l = ladder(&failing(&mut run, "G1"), &run.info, p);
        assert_eq!(
            l.profile.as_deref(),
            Some("--profile streaming --fps-cap 120")
        );
        assert_eq!(l.audio, None);
        run.info.class_hz = 60;
        let ok = Some(PeriodLadder {
            chosen: 480,
            step: 48,
            max: 960,
        });
        let l = ladder(&failing(&mut run, "G5"), &run.info, ok);
        assert_eq!(
            l.profile.as_deref(),
            Some("--profile streaming --fps-cap 60")
        );
        assert_eq!(l.audio.as_deref(), Some("--audio-period 528"));
        let l = ladder(&failing(&mut run, "G5"), &run.info, p);
        assert_eq!(l.audio, None);
        assert_eq!(l.notes.len(), 1);
        run.info.profile = Profile::Streaming;
        let l = ladder(&failing(&mut run, "O1"), &run.info, ok);
        assert_eq!(l.profile, None);
        assert_eq!(l.notes.len(), 1);
        // A passing or invalid run has no ladder.
        let mut s = evaluate(&run);
        s.exit_code = 2;
        assert_eq!(ladder(&s, &run.info, ok), Ladder::default());
    }
}
