//! Validity V1–V7, gates G1–G7, O1, O2 (design §3.3, §3.4).
//!
//! `evaluate` turns a `RunData` into a `Summary`. Exit code: 2 if any
//! validity check fails, otherwise 1 if any applicable gate fails,
//! otherwise 0. Performance gates use heavy-window samples only; whole-run
//! statistics are recorded for information. Ratios are compared from
//! integer counts.

use crate::chart::{LANES, MIN_NOTES};
use crate::heavy::Timeline;
use crate::obslog::{EncoderFamily, ObsSession};
use crate::run::{EncoderClass, EventKind, PmFrame, Profile, RunData, RunKind, ThreadTag};
use crate::stats::{percentile_f64, percentile_i64};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The two display classes (R1): reference 60 Hz, mid-range 240 Hz.
pub const REFRESH_CLASSES: [u32; 2] = [60, 240];
pub const RENDER_CPU_LIMIT_US: i64 = 1_500;
pub const GPU_LIMIT_MS: f64 = 2.0;
pub const INPUT_LIMIT_US: i64 = 1_000;
/// V2: active voices floor and the share of audio periods that must meet it.
pub const MIN_ACTIVE_VOICES: u16 = 100;
/// V4: a PresentMon row matches a spike present within this distance.
pub const PRESENT_MATCH_TOL_US: i64 = 1_000;
/// V4: longest PresentMon row gap allowed while the spike presents.
pub const MAX_CAPTURE_GAP_US: i64 = 1_000_000;
pub const MIN_MIX_RATE_HZ: u32 = 44_100;
/// V7 measurement-window bounds.
pub const MAX_OBS_PRE_ROLL_MS: i64 = 30_000;
pub const MAX_OBS_POST_ROLL_MS: i64 = 60_000;
/// V7 settings (owner procedure §4.1).
pub const OBS_BITRATE_KBPS: u32 = 6_000;
pub const OBS_WIDTH: u32 = 1_920;
pub const OBS_HEIGHT: u32 = 1_080;
pub const OBS_FPS: u32 = 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub ok: bool,
    /// Short failure codes; empty when `ok`.
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub id: String,
    /// Whether the gate decides the exit code for this run kind.
    pub applies: bool,
    pub pass: bool,
    pub value: Option<f64>,
    pub threshold: f64,
    pub unit: String,
    pub samples: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorstFrame {
    pub present_us: i64,
    pub display_change_ms: f64,
    pub visible_notes: u32,
    pub effect_instances: u32,
    pub lanes_with_effects: u8,
    pub bga_frame_shown: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Records {
    /// G1r: heavy-window frames displayed ≥ 1.5 T after the previous one,
    /// plus heavy-window frames not displayed.
    pub missed_deadlines: u64,
    pub not_displayed: u64,
    pub heavy_frames: u64,
    pub whole_run_p99_interval_ms: Option<f64>,
    pub whole_run_max_interval_ms: Option<f64>,
    pub worst_frames: Vec<WorstFrame>,
    pub library_allocs: BTreeMap<String, u64>,
    pub voice_steals: u64,
    pub bga_pool_exhausted: u64,
    pub bga_skipped: u64,
    pub obs_pre_roll_ms: Option<i64>,
    pub obs_post_roll_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub exit_code: u8,
    /// Refresh class from V3, and the G1 target (cap or class).
    pub refresh_class_hz: Option<u32>,
    pub target_hz: Option<u32>,
    pub validity: Vec<Check>,
    pub gates: Vec<Gate>,
    pub records: Records,
}

impl Summary {
    pub fn check(&self, id: &str) -> &Check {
        self.validity
            .iter()
            .find(|c| c.id == id)
            .expect("unknown check")
    }

    pub fn gate(&self, id: &str) -> &Gate {
        self.gates
            .iter()
            .find(|g| g.id == id)
            .expect("unknown gate")
    }
}

pub fn evaluate(run: &RunData) -> Summary {
    let tl = run.info.timeline;
    let refresh_class_hz = refresh_class(run);
    let target_hz = refresh_class_hz.map(|c| run.info.fps_cap.unwrap_or(c));
    let validity = vec![
        v1(run),
        v2(run),
        v3(run, refresh_class_hz),
        v4(run),
        v5(run),
        v6(run),
        v7(run),
    ];
    let (gates, mut records) = gates(run, &tl, target_hz);
    records.obs_pre_roll_ms = session(run).map(|s| s.pre_roll_ms);
    records.obs_post_roll_ms = session(run).and_then(|s| s.post_roll_ms);
    let exit_code = if validity.iter().any(|c| !c.ok) {
        2
    } else if gates.iter().any(|g| g.applies && !g.pass) {
        1
    } else {
        0
    };
    Summary {
        exit_code,
        refresh_class_hz,
        target_hz,
        validity,
        gates,
        records,
    }
}

fn check(id: &str, reasons: Vec<&str>) -> Check {
    Check {
        id: id.into(),
        ok: reasons.is_empty(),
        reasons: reasons.into_iter().map(String::from).collect(),
    }
}

fn session(run: &RunData) -> Option<&ObsSession> {
    run.obs.as_ref().and_then(|o| o.session.as_ref())
}

fn bga_on(run: &RunData) -> bool {
    run.info.profile != Profile::NoBga
}

fn v1(run: &RunData) -> Check {
    let c = &run.counters;
    let mut r = vec![];
    if (c.notes_total as usize) < MIN_NOTES {
        r.push("fewer-than-10000-notes");
    }
    if c.notes_played != c.notes_total {
        r.push("notes-not-all-played");
    }
    check("V1", r)
}

fn v2(run: &RunData) -> Check {
    let tl = run.info.timeline;
    let mut r = vec![];
    for i in 0..tl.window_count() {
        let w = tl.window(i);
        let inw = |t: i64| (w.start_us..w.end_us).contains(&t);
        let periods: Vec<_> = run.audio_periods.iter().filter(|p| inw(p.t_us)).collect();
        let busy = periods
            .iter()
            .filter(|p| p.active_voices >= MIN_ACTIVE_VOICES)
            .count();
        if periods.is_empty() || busy * 100 < periods.len() * 95 {
            r.push("voices-below-100");
        }
        // Every whole second of the window has a frame with all lanes lit.
        let mut lit = vec![false; ((w.end_us - w.start_us) / 1_000_000) as usize];
        for f in run.frames.iter().filter(|f| inw(f.present_us)) {
            if f.lanes_with_effects >= LANES {
                let s = ((f.present_us - w.start_us) / 1_000_000) as usize;
                if let Some(x) = lit.get_mut(s) {
                    *x = true;
                }
            }
        }
        if !lit.iter().all(|&x| x) {
            r.push("lanes-not-all-lit");
        }
        if bga_on(run) && !run.bga.iter().any(|b| inw(b.due_us) && b.displayed) {
            r.push("bga-not-displayed");
        }
    }
    if bga_on(run) && (run.info.bga_width, run.info.bga_height) != (Some(1920), Some(1080)) {
        r.push("bga-not-1080p");
    }
    r.dedup();
    check("V2", r)
}

/// The refresh class when the exact rate is in `[class, class × 1.001]`.
fn refresh_class(run: &RunData) -> Option<u32> {
    let rf = run.info.refresh;
    if rf.den == 0 {
        return None;
    }
    let (n, d) = (u128::from(rf.num), u128::from(rf.den));
    REFRESH_CLASSES.into_iter().find(|&c| {
        let c = u128::from(c);
        n >= c * d && n * 1000 <= c * 1001 * d
    })
}

fn v3(run: &RunData, class: Option<u32>) -> Check {
    let mut r = vec![];
    match class {
        None => r.push("refresh-not-exact-60-or-240"),
        Some(c) => {
            if let Some(cap) = run.info.fps_cap
                && (cap == 0 || c % cap != 0)
            {
                r.push("cap-does-not-divide-refresh");
            }
        }
    }
    check("V3", r)
}

fn v4(run: &RunData) -> Check {
    let tl = run.info.timeline;
    let (start, end) = (tl.play_start_us, tl.play_end_us());
    let mut r = vec![];
    if run.info.presentmon_error.is_some() {
        r.push("presentmon-malformed");
    }
    let pm: Vec<i64> = run.pm_frames.iter().map(|f| f.present_us).collect();
    let (Some(&first), Some(&last)) = (pm.first(), pm.last()) else {
        r.push("presentmon-empty");
        return check("V4", r);
    };
    if first > start || last < end {
        r.push("presentmon-does-not-bracket-play");
    }
    let spike: Vec<i64> = run
        .frames
        .iter()
        .map(|f| f.present_us)
        .filter(|&t| tl.in_play(t))
        .collect();
    // Every spike present in the play window has its own PresentMon row.
    let mut j = 0;
    let mut missing = 0usize;
    for &p in &spike {
        while j < pm.len() && pm[j] < p - PRESENT_MATCH_TOL_US {
            j += 1;
        }
        if j < pm.len() && pm[j] <= p + PRESENT_MATCH_TOL_US {
            j += 1;
        } else {
            missing += 1;
        }
    }
    if missing > 0 {
        r.push("presents-missing-from-capture");
    }
    // No row gap over 1 s during which the spike presented.
    let gap_with_presents = pm.windows(2).any(|w| {
        let (a, b) = (w[0], w[1]);
        b - a > MAX_CAPTURE_GAP_US && a < end && b > start && {
            let k = spike.partition_point(|&t| t <= a);
            k < spike.len() && spike[k] < b
        }
    });
    if gap_with_presents {
        r.push("capture-gap-over-1s");
    }
    check("V4", r)
}

fn v5(run: &RunData) -> Check {
    let tl = run.info.timeline;
    let mut r = vec![];
    let bad = run.events.iter().any(|e| {
        tl.in_play(e.t_us)
            && matches!(
                e.kind,
                EventKind::FocusLoss
                    | EventKind::Suspend
                    | EventKind::Resume
                    | EventKind::SwapChainRecreate
            )
    });
    if bad {
        r.push("focus-suspend-or-recreate-during-play");
    }
    if run.counters.input_overflows > 0 || run.counters.sound_overflows > 0 {
        r.push("queue-overflow");
    }
    check("V5", r)
}

fn v6(run: &RunData) -> Check {
    let mut r = vec![];
    if !run.audio.pilot_calibrated {
        r.push("pilot-not-calibrated");
    }
    if run.audio.unverifiable_spans > 0 {
        r.push("unverifiable-loopback-span");
    }
    if run.info.mix_rate_hz < MIN_MIX_RATE_HZ {
        r.push("mix-rate-below-44100");
    }
    check("V6", r)
}

fn v7(run: &RunData) -> Check {
    if run.info.kind != RunKind::Obs {
        return check("V7", vec![]);
    }
    let mut r = vec![];
    let Some(s) = session(run) else {
        r.push("no-obs-session-overlapping-play");
        return check("V7", r);
    };
    if !s.completed() {
        r.push("obs-session-incomplete");
    }
    if s.frames_output.unwrap_or(0) == 0 || s.drawn.unwrap_or(0) == 0 {
        r.push("obs-no-frames");
    }
    if !(0..=MAX_OBS_PRE_ROLL_MS).contains(&s.pre_roll_ms) {
        r.push("obs-pre-roll");
    }
    if !s
        .post_roll_ms
        .is_some_and(|p| (0..=MAX_OBS_POST_ROLL_MS).contains(&p))
    {
        r.push("obs-post-roll");
    }
    let enc = s.encoder.clone().unwrap_or_default();
    let family_ok = match (run.info.obs_encoder, enc.family) {
        (Some(EncoderClass::X264), Some(EncoderFamily::X264)) => true,
        (Some(EncoderClass::Hardware), Some(f)) => f != EncoderFamily::X264,
        _ => false,
    };
    let settings_ok = family_ok
        && enc
            .rate_control
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("CBR"))
        && enc.bitrate_kbps == Some(OBS_BITRATE_KBPS)
        && enc.width == Some(OBS_WIDTH)
        && enc.height == Some(OBS_HEIGHT)
        && enc.fps_num == Some(OBS_FPS)
        && enc.fps_den.unwrap_or(1) == 1;
    if !settings_ok {
        r.push("obs-settings-mismatch");
    }
    if s.bandwidth_dropped.unwrap_or(0) > 0 {
        r.push("obs-bandwidth-drop");
    }
    if run.owner.game_visible != Some(true) || run.owner.audio_present != Some(true) {
        r.push("owner-did-not-confirm-capture");
    }
    check("V7", r)
}

fn gate(
    id: &str,
    applies: bool,
    pass: bool,
    value: Option<f64>,
    threshold: f64,
    unit: &str,
    samples: usize,
) -> Gate {
    Gate {
        id: id.into(),
        applies,
        pass,
        value,
        threshold,
        unit: unit.into(),
        samples: samples as u64,
    }
}

fn gates(run: &RunData, tl: &Timeline, target_hz: Option<u32>) -> (Vec<Gate>, Records) {
    let kind = run.info.kind;
    let heavy_pm: Vec<&PmFrame> = run
        .pm_frames
        .iter()
        .filter(|f| tl.is_heavy(f.window_time_us()))
        .collect();
    let t_ms = target_hz.map(|hz| 1000.0 / f64::from(hz));

    // G1 / G1r.
    let intervals: Vec<f64> = heavy_pm
        .iter()
        .filter_map(|f| f.display_change_ms)
        .collect();
    let p99 = percentile_f64(&intervals, 99);
    let g1_pass = matches!((p99, t_ms), (Some(p), Some(t)) if p <= t);
    let not_displayed = heavy_pm
        .iter()
        .filter(|f| f.display_change_ms.is_none())
        .count() as u64;
    let late = t_ms.map_or(0, |t| {
        intervals.iter().filter(|&&i| i >= 1.5 * t).count() as u64
    });

    // G2: render CPU work.
    let cpu: Vec<i64> = run
        .frames
        .iter()
        .filter(|f| tl.is_heavy(f.present_us))
        .map(|f| i64::from(f.render_cpu_us))
        .collect();
    let cpu99 = percentile_i64(&cpu, 99);
    // G3: GPU busy.
    let gpu: Vec<f64> = heavy_pm.iter().filter_map(|f| f.gpu_busy_ms).collect();
    let gpu99 = percentile_f64(&gpu, 99);
    // G4: input → judgment.
    let inp: Vec<i64> = run
        .input
        .iter()
        .filter(|x| tl.is_heavy(x.t_us))
        .map(|x| x.latency_us)
        .collect();
    let inp99 = percentile_i64(&inp, 99);
    // G5, G6.
    let episodes = run.audio.episodes.len();
    let app_allocs: u64 = [
        ThreadTag::Input,
        ThreadTag::Logic,
        ThreadTag::Audio,
        ThreadTag::Render,
    ]
    .iter()
    .map(|t| run.counters.app_allocs.get(t).copied().unwrap_or(0))
    .sum();
    // G7.
    let due: Vec<_> = run.bga.iter().filter(|b| tl.is_heavy(b.due_us)).collect();
    let shown = due.iter().filter(|b| b.displayed).count();
    let g7_pass = !due.is_empty() && shown * 100 >= due.len() * 99;
    // O1, O2.
    let obs = kind == RunKind::Obs;
    let (lagged, attempted) = session(run).map_or((0, 0), ObsSession::render_lag);
    let (skipped, total) = session(run).map_or((0, 0), ObsSession::encode_lag);
    let pct = |n: u64, d: u64| (d > 0).then(|| n as f64 * 100.0 / d as f64);

    let gates = vec![
        gate(
            "G1",
            true,
            g1_pass,
            p99,
            t_ms.unwrap_or(0.0),
            "ms",
            intervals.len(),
        ),
        gate(
            "G2",
            kind == RunKind::Baseline,
            cpu99.is_some_and(|v| v <= RENDER_CPU_LIMIT_US),
            cpu99.map(|v| v as f64 / 1000.0),
            RENDER_CPU_LIMIT_US as f64 / 1000.0,
            "ms",
            cpu.len(),
        ),
        gate(
            "G3",
            kind == RunKind::Baseline,
            gpu99.is_some_and(|v| v <= GPU_LIMIT_MS),
            gpu99,
            GPU_LIMIT_MS,
            "ms",
            gpu.len(),
        ),
        gate(
            "G4",
            true,
            inp99.is_some_and(|v| v <= INPUT_LIMIT_US),
            inp99.map(|v| v as f64 / 1000.0),
            INPUT_LIMIT_US as f64 / 1000.0,
            "ms",
            inp.len(),
        ),
        gate(
            "G5",
            true,
            episodes == 0,
            Some(episodes as f64),
            0.0,
            "episodes",
            episodes,
        ),
        gate(
            "G6",
            true,
            app_allocs == 0,
            Some(app_allocs as f64),
            0.0,
            "allocations",
            app_allocs as usize,
        ),
        gate(
            "G7",
            bga_on(run),
            g7_pass,
            pct(shown as u64, due.len() as u64),
            99.0,
            "%",
            due.len(),
        ),
        gate(
            "O1",
            obs,
            attempted > 0 && lagged * 1000 < attempted * 5,
            pct(lagged, attempted),
            0.5,
            "%",
            attempted as usize,
        ),
        gate(
            "O2",
            obs,
            total > 0 && skipped * 1000 < total * 5,
            pct(skipped, total),
            0.5,
            "%",
            total as usize,
        ),
    ];

    let all: Vec<f64> = run
        .pm_frames
        .iter()
        .filter_map(|f| f.display_change_ms)
        .collect();
    let mut worst: Vec<&PmFrame> = run
        .pm_frames
        .iter()
        .filter(|f| f.display_change_ms.is_some())
        .collect();
    worst.sort_by(|a, b| {
        b.display_change_ms
            .unwrap_or(0.0)
            .total_cmp(&a.display_change_ms.unwrap_or(0.0))
            .then(a.present_us.cmp(&b.present_us))
    });
    let worst_frames = worst
        .into_iter()
        .take(10)
        .map(|f| {
            // Load counters of the spike frame presented nearest this row.
            let k = run.frames.partition_point(|s| s.present_us < f.present_us);
            let near = [k.checked_sub(1), Some(k)]
                .into_iter()
                .flatten()
                .filter_map(|i| run.frames.get(i))
                .min_by_key(|s| (s.present_us - f.present_us).abs());
            WorstFrame {
                present_us: f.present_us,
                display_change_ms: f.display_change_ms.unwrap_or(0.0),
                visible_notes: near.map_or(0, |s| s.visible_notes),
                effect_instances: near.map_or(0, |s| s.effect_instances),
                lanes_with_effects: near.map_or(0, |s| s.lanes_with_effects),
                bga_frame_shown: near.is_some_and(|s| s.bga_frame_shown),
            }
        })
        .collect();
    let records = Records {
        missed_deadlines: late + not_displayed,
        not_displayed,
        heavy_frames: heavy_pm.len() as u64,
        whole_run_p99_interval_ms: percentile_f64(&all, 99),
        whole_run_max_interval_ms: all.iter().copied().max_by(f64::total_cmp),
        worst_frames,
        library_allocs: run.counters.library_allocs.clone(),
        voice_steals: run.counters.voice_steals,
        bga_pool_exhausted: run.counters.bga_pool_exhausted,
        bga_skipped: run.bga.iter().filter(|b| !b.displayed).count() as u64,
        obs_pre_roll_ms: None,
        obs_post_roll_ms: None,
    };
    (gates, records)
}
