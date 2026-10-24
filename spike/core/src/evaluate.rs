//! Matrix eligibility M1, validity V1–V8, gates G1–G7, O1, O2 (design
//! §3.3, §3.4).
//!
//! `evaluate` turns a `RunData` into a `Summary`. Exit code: 2 if any
//! validity check fails, otherwise 1 if any applicable gate fails,
//! otherwise 0. Performance gates use heavy-window samples only; whole-run
//! statistics are recorded for information. Ratios are compared from
//! integer counts.

use crate::chart::{LANES, MIN_NOTES};
use crate::heavy::Timeline;
use crate::obslog::{EncoderFamily, ObsSession};
use crate::run::{
    EncoderClass, EventKind, PmFrame, Profile, RunData, RunKind, StreamCount, ThreadTag,
};
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
/// Required runs play the chart this many times (600 s).
pub const REQUIRED_LOOPS: u32 = 4;
/// V8: longest gap between audio-period records inside the play window
/// (a late wake-up is a diagnostic, lost records are not).
pub const MAX_AUDIO_PERIOD_GAP_US: i64 = 50_000;
/// One BGA frame at 60 fps, rounded up.
pub const BGA_FRAME_US: i64 = 16_667;
/// V8: a stream must reach within this many of its own periods of play
/// start and play end (audio: device periods; BGA: frame intervals;
/// rendered frames: refresh intervals of the G1 target). BGA due records
/// also may not be further apart than this.
pub const EDGE_PERIODS: i64 = 2;
/// V7 measurement-window bounds.
pub const MAX_OBS_PRE_ROLL_MS: i64 = 30_000;
pub const MAX_OBS_POST_ROLL_MS: i64 = 60_000;

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
    let target_hz = refresh_class_hz.map(|c| target(run, c));
    let validity = vec![
        m1(run),
        v1(run),
        v2(run),
        v3(run, refresh_class_hz),
        v4(run),
        v5(run),
        v6(run),
        v7(run),
        v8(run),
    ];
    let (gates, mut records) = gates(run, &tl, target_hz);
    records.obs_pre_roll_ms = session(run).and_then(|s| s.connect_lead_ms.max(s.start_lead_ms));
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

/// G1 target T: the refresh class for R1 and for standard-profile OBS runs;
/// the cap only for streaming-profile OBS runs and diagnostics.
fn target(run: &RunData, class: u32) -> u32 {
    match (run.info.kind, run.info.profile) {
        (RunKind::Baseline, _) | (RunKind::Obs, Profile::Standard) => class,
        _ => run.info.fps_cap.unwrap_or(class),
    }
}

/// Matrix eligibility: a run counts as a required cell only in the
/// configuration the matrix defines (§2 profiles, §4.2). Anything else must
/// be marked `Diagnostic`.
fn m1(run: &RunData) -> Check {
    let i = &run.info;
    let mut r = vec![];
    match i.kind {
        RunKind::Diagnostic => {}
        RunKind::Baseline => {
            if i.profile != Profile::Standard {
                r.push("baseline-not-standard-profile");
            }
            if i.fps_cap.is_some() {
                r.push("baseline-capped");
            }
        }
        RunKind::Obs => match i.profile {
            Profile::Standard if i.fps_cap.is_some() => r.push("standard-profile-capped"),
            Profile::Streaming if i.fps_cap.is_none() => r.push("streaming-profile-uncapped"),
            Profile::NoBga => r.push("no-bga-is-diagnostic-only"),
            _ => {}
        },
    }
    if i.kind != RunKind::Diagnostic && i.timeline.loops != REQUIRED_LOOPS {
        r.push("not-4-loops");
    }
    check("M1", r)
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
        // Rendered frames, presented or not: V2 checks the load, not the
        // presentation.
        for f in run.frames.iter().filter(|f| inw(f.render_us)) {
            if f.lanes_with_effects >= LANES {
                let s = ((f.render_us - w.start_us) / 1_000_000) as usize;
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
        .filter_map(|f| f.present_us)
        .filter(|&t| tl.in_play(t))
        .collect();
    // Every spike present in the play window has its own PresentMon row.
    let missing = match_presents(&spike, &pm)
        .iter()
        .filter(|m| m.is_none())
        .count();
    // Presents whose frame record was lost still have present counts: a
    // count gap between two retained presents in play means that many
    // presents were issued between them, and PresentMon must have a row
    // for each strictly between the two (rows matched to the retained
    // presents themselves, and rows outside play, do not count).
    let mut counted: Vec<(u32, i64)> = run
        .frames
        .iter()
        .filter_map(|f| Some((f.present_count?, f.present_us?)))
        .filter(|&(_, t)| tl.in_play(t))
        .collect();
    counted.sort_unstable();
    let uncaptured = counted.windows(2).any(|w| {
        let ((ca, ta), (cb, tb)) = (w[0], w[1]);
        let issued_between = u64::from(cb.wrapping_sub(ca)).saturating_sub(1);
        issued_between > 0 && {
            let lo = pm.partition_point(|&t| t <= ta + PRESENT_MATCH_TOL_US);
            let hi = pm.partition_point(|&t| t < tb - PRESENT_MATCH_TOL_US);
            ((hi.saturating_sub(lo)) as u64) < issued_between
        }
    });
    if missing > 0 || uncaptured {
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

/// One-to-one match of spike present times to PresentMon present times
/// (both sorted), within `PRESENT_MATCH_TOL_US`.
fn match_presents(spike: &[i64], pm: &[i64]) -> Vec<Option<usize>> {
    let mut j = 0;
    spike
        .iter()
        .map(|&p| {
            while j < pm.len() && pm[j] < p - PRESENT_MATCH_TOL_US {
                j += 1;
            }
            (j < pm.len() && pm[j] <= p + PRESENT_MATCH_TOL_US).then(|| {
                j += 1;
                j - 1
            })
        })
        .collect()
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
    let tl = run.info.timeline;
    let covered = matches!(
        (run.audio.capture_from_us, run.audio.capture_to_us),
        (Some(a), Some(b)) if a <= tl.play_start_us && b >= tl.play_end_us()
    );
    if !covered {
        r.push("loopback-does-not-cover-play");
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
    // Counters cover the session from the successful connection and the
    // streaming start; both must precede play by at most the pre-roll bound.
    let lead_ok = |l: Option<i64>| l.is_some_and(|l| (0..=MAX_OBS_PRE_ROLL_MS).contains(&l));
    if !lead_ok(s.connect_lead_ms) || !lead_ok(s.start_lead_ms) {
        r.push("obs-pre-roll");
    }
    if s.other_output_active {
        r.push("obs-other-output-active");
    }
    if !s
        .post_roll_ms
        .is_some_and(|p| (0..=MAX_OBS_POST_ROLL_MS).contains(&p))
    {
        r.push("obs-post-roll");
    }
    let enc = s.encoder.as_ref();
    let family_ok = match (run.info.obs_encoder, enc.and_then(|e| e.family)) {
        (Some(EncoderClass::X264), Some(EncoderFamily::X264)) => true,
        (Some(EncoderClass::Hardware), Some(f)) => f != EncoderFamily::X264,
        _ => false,
    };
    let video_ok = s.video.as_ref().is_some_and(|v| v.matches_procedure());
    if !(family_ok && enc.is_some_and(|e| e.matches_procedure()) && video_ok) {
        r.push("obs-settings-mismatch");
    }
    if s.bandwidth_dropped.unwrap_or(0) > 0 {
        r.push("obs-bandwidth-drop");
    }
    // O2 needs the stream's own encoding-lag summary, or proof there was
    // none (obslog: ownership rule).
    if s.encode_lag_ambiguous {
        r.push("obs-encode-lag-ambiguous");
    } else if s.encode_skipped.is_none() && !s.teardown_confirmed {
        r.push("obs-teardown-unconfirmed");
    }
    if run.owner.game_visible != Some(true) || run.owner.audio_present != Some(true) {
        r.push("owner-did-not-confirm-capture");
    }
    check("V7", r)
}

/// The times of `rows` (sorted) must reach within `edge` of both ends of
/// play and, with `max_gap`, have no larger hole inside play.
fn spans<T>(
    rows: &[T],
    time: impl Fn(&T) -> i64,
    (start, end): (i64, i64),
    edge: i64,
    max_gap: Option<i64>,
) -> bool {
    let (Some(a), Some(b)) = (rows.first(), rows.last()) else {
        return false;
    };
    time(a) <= start + edge
        && time(b) >= end - edge
        && max_gap.is_none_or(|g| {
            rows.windows(2).all(|w| {
                let (a, b) = (time(&w[0]), time(&w[1]));
                b - a <= g || b <= start || a >= end
            })
        })
}

fn complete(c: StreamCount, rows: usize) -> bool {
    c.lost == 0 && c.produced == rows as u64
}

/// V8: measurement evidence is complete. Every stream reports what its
/// producer made and lost, so a gate is never computed from a partial
/// stream (one sample cannot satisfy G4, V2 or G7).
fn v8(run: &RunData) -> Check {
    let tl = run.info.timeline;
    let play = (tl.play_start_us, tl.play_end_us());
    let mut r = vec![];
    let s = &run.streams;
    let frame_us = 1_000_000 / i64::from(refresh_class(run).map_or(60, |c| target(run, c)));
    if !complete(s.frames, run.frames.len())
        || !spans(
            &run.frames,
            |f| f.render_us,
            play,
            EDGE_PERIODS * frame_us,
            None,
        )
    {
        r.push("frames-incomplete");
    }
    // One synthetic press per note.
    if !complete(s.input, run.input.len())
        || run.input.len() as u64 != u64::from(run.counters.notes_total)
    {
        r.push("input-incomplete");
    }
    if !complete(s.audio_periods, run.audio_periods.len())
        || !spans(
            &run.audio_periods,
            |p| p.t_us,
            play,
            EDGE_PERIODS * i64::from(run.info.audio_period_us),
            Some(MAX_AUDIO_PERIOD_GAP_US),
        )
    {
        r.push("audio-periods-incomplete");
    }
    if bga_on(run)
        && (!complete(s.bga, run.bga.len())
            || !spans(
                &run.bga,
                |b| b.due_us,
                play,
                EDGE_PERIODS * BGA_FRAME_US,
                Some(EDGE_PERIODS * BGA_FRAME_US),
            ))
    {
        r.push("bga-incomplete");
    }
    let threads = [
        ThreadTag::Input,
        ThreadTag::Logic,
        ThreadTag::Audio,
        ThreadTag::Render,
    ];
    if !threads
        .iter()
        .all(|t| run.counters.app_allocs.contains_key(t))
    {
        r.push("allocation-counters-missing");
    }
    if run
        .pm_frames
        .iter()
        .any(|f| tl.is_heavy(f.window_time_us()) && f.gpu_busy_ms.is_none())
    {
        r.push("gpu-time-missing");
    }
    check("V8", r)
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
    // TODO(P5): PresentMon prints durations rounded to a fixed number of
    // decimals, so a frame exactly one refresh apart may print above T
    // (16.6667 > 1000/60). Settle the comparison with a real CSV in P5.
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

    // G2: render CPU work, assigned to windows like G1/G3: by the display
    // time of the frame's PresentMon row, or its render time if it has none.
    let pm_present: Vec<i64> = run.pm_frames.iter().map(|f| f.present_us).collect();
    let presented: Vec<i64> = run.frames.iter().filter_map(|f| f.present_us).collect();
    let mut matched = match_presents(&presented, &pm_present).into_iter();
    let cpu: Vec<i64> = run
        .frames
        .iter()
        .filter(|f| {
            let row = f.present_us.and_then(|_| matched.next().flatten());
            tl.is_heavy(row.map_or(f.render_us, |i| run.pm_frames[i].window_time_us()))
        })
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
            let k = run.frames.partition_point(|s| s.render_us < f.present_us);
            let near = [k.checked_sub(1), Some(k)]
                .into_iter()
                .flatten()
                .filter_map(|i| run.frames.get(i))
                .min_by_key(|s| (s.present_us.unwrap_or(s.render_us) - f.present_us).abs());
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
