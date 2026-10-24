//! Synthetic runs for tests (and for the app's smoke checks).
//!
//! `passing_run` is a run that is valid and passes every gate: 4 loops at
//! 60 Hz with every frame displayed exactly one refresh apart.

use crate::heavy::Timeline;
use crate::obslog::{EncoderFamily, ObsDerived, ObsEncoderSettings, ObsSession, ObsVideo};
use crate::run::*;

pub const PLAY_START_US: i64 = 20_000_000;
/// Exactly one 60 Hz refresh, in ms.
pub const T60_MS: f64 = 1000.0 / 60.0;

/// Present times every 1/60 s (rounded down to µs) from 1 s before play
/// to 1 s after it.
pub fn present_times(tl: &Timeline, hz: i64) -> Vec<i64> {
    let from = tl.play_start_us - 1_000_000;
    let to = tl.play_end_us() + 1_000_000;
    let n = (to - from) * hz / 1_000_000;
    (0..n).map(|i| from + i * 1_000_000 / hz).collect()
}

pub fn passing_run() -> RunData {
    let timeline = Timeline {
        play_start_us: PLAY_START_US,
        loops: 4,
    };
    let presents = present_times(&timeline, 60);
    let pm_frames: Vec<PmFrame> = presents
        .iter()
        .map(|&t| PmFrame {
            present_us: t,
            display_us: Some(t + 5_000),
            display_change_ms: Some(T60_MS),
            gpu_busy_ms: Some(1.0),
        })
        .collect();
    let frames: Vec<SpikeFrame> = presents
        .iter()
        .map(|&t| SpikeFrame {
            render_us: t - 3_000,
            present_us: Some(t),
            render_cpu_us: 800,
            present_call_us: 50,
            visible_notes: 80,
            effect_instances: 120,
            lanes_with_effects: 8,
            bga_frame_shown: true,
        })
        .collect();
    let (start, end) = (timeline.play_start_us, timeline.play_end_us());
    let audio_periods: Vec<AudioPeriod> = (start..end)
        .step_by(10_000)
        .map(|t| AudioPeriod {
            t_us: t,
            active_voices: 150,
        })
        .collect();
    let input: Vec<InputLatency> = (start..end)
        .step_by(5_000)
        .map(|t| InputLatency {
            t_us: t,
            latency_us: 200,
        })
        .collect();
    let bga: Vec<BgaDue> = (0..(end - start) * 60 / 1_000_000)
        .map(|i| BgaDue {
            due_us: start + i * 1_000_000 / 60,
            displayed: true,
        })
        .collect();
    let mut counters = Counters {
        // One synthetic press (input record) per note.
        notes_total: input.len() as u32,
        notes_played: input.len() as u32,
        ..Counters::default()
    };
    for t in [
        ThreadTag::Input,
        ThreadTag::Logic,
        ThreadTag::Audio,
        ThreadTag::Render,
    ] {
        counters.app_allocs.insert(t, 0);
    }
    counters.library_allocs.insert("submit".into(), 3);
    let mut run = RunData {
        info: RunInfo {
            profile: Profile::Standard,
            kind: RunKind::Baseline,
            obs_encoder: None,
            timeline,
            refresh: Rational { num: 60, den: 1 },
            fps_cap: None,
            bga_width: Some(1920),
            bga_height: Some(1080),
            mix_rate_hz: 48_000,
            presentmon_error: None,
        },
        counters,
        streams: StreamCounts::default(),
        audio: AudioFacts {
            pilot_calibrated: true,
            unverifiable_spans: 0,
            capture_from_us: Some(PLAY_START_US - 10_000_000),
            capture_to_us: Some(timeline.play_end_us() + 3_000_000),
            episodes: vec![],
        },
        owner: OwnerPublic::default(),
        obs: None,
        events: vec![],
        pm_frames,
        frames,
        audio_periods,
        input,
        bga,
    };
    sync_streams(&mut run);
    run
}

/// Sets every stream's produced count to its row count with nothing lost,
/// for tests that edit the rows.
pub fn sync_streams(run: &mut RunData) {
    let c = |n: usize| StreamCount {
        produced: n as u64,
        lost: 0,
    };
    run.streams = StreamCounts {
        frames: c(run.frames.len()),
        input: c(run.input.len()),
        audio_periods: c(run.audio_periods.len()),
        bga: c(run.bga.len()),
    };
}

/// `passing_run` as an R3 cell (x264) with a clean OBS session.
pub fn passing_obs_run() -> RunData {
    let mut run = passing_run();
    run.info.kind = RunKind::Obs;
    run.info.obs_encoder = Some(EncoderClass::X264);
    run.owner = OwnerPublic {
        game_visible: Some(true),
        audio_present: Some(true),
    };
    run.obs = Some(ObsDerived {
        sessions_in_log: 1,
        session: Some(ObsSession {
            start_marker: true,
            connected: true,
            stop_marker: true,
            connect_lead_ms: Some(10_000),
            start_lead_ms: Some(9_900),
            post_roll_ms: Some(20_000),
            video: Some(ObsVideo {
                base_width: Some(1920),
                base_height: Some(1080),
                output_width: Some(1920),
                output_height: Some(1080),
                fps_num: Some(60),
                fps_den: Some(1),
                downscale_filter: Some("Bicubic".into()),
            }),
            encoder: Some(ObsEncoderSettings {
                family: Some(EncoderFamily::X264),
                label: Some("x264 encoder".into()),
                rate_control: Some("CBR".into()),
                bitrate_kbps: Some(6_000),
                preset: Some("veryfast".into()),
                profile: Some("high".into()),
                tune: Some("(null)".into()),
                ..ObsEncoderSettings::default()
            }),
            frames_output: Some(37_800),
            drawn: Some(37_800),
            ..ObsSession::default()
        }),
    });
    run
}
