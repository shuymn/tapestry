//! The run's export-safe facts (`spike_core::run::RunData`) assembled at the
//! end of a windowed run, and the private QPC/wall-clock origin that `report`
//! needs to place PresentMon rows and OBS log lines on the run's clock.
//!
//! `rundata.json` holds relative µs, counts and settings only. PresentMon
//! rows, the OBS session and the owner's notes are added by `report`.

use crate::run::RunArgs;
use crate::session::SessionOut;
use crate::timebase::{Clock, qpc, qpc_freq};
use serde::{Deserialize, Serialize};
use spike_core::heavy::Timeline;
use spike_core::run::{
    BgaDue, Counters, OwnerPublic, Profile, Rational, RunData, RunEvent, RunInfo, RunKind,
    SpikeFrame, StreamCount, StreamCounts,
};
use std::path::Path;
use windows::Win32::System::SystemInformation::GetLocalTime;

pub const RUNDATA: &str = "rundata.json";
pub const ORIGIN: &str = "private/origin.json";

/// `private/origin.json`: raw QPC and local wall clock of the run origin.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Origin {
    pub qpc_origin: i64,
    pub qpc_freq: i64,
    /// Local wall clock of the origin: ms since 1970-01-01 00:00 local time,
    /// so dates carry across midnight (OBS log lines and GPU samples are
    /// placed on the run clock through it).
    pub wall_origin_local_ms: i64,
    pub play_start_us: i64,
    pub play_len_us: i64,
    /// Last QPC of the run (for the export's raw-QPC scan range).
    pub qpc_end: i64,
}

impl Origin {
    /// The origin of `clock`, placed on the local wall clock now.
    pub fn now(clock: Clock, play_start_us: i64, play_len_us: i64) -> Self {
        // SAFETY: plain getter.
        let t = unsafe { GetLocalTime() };
        let now_qpc = qpc();
        let freq = qpc_freq();
        let now_ms = spike_core::conditions::local_ms(
            i64::from(t.wYear),
            u32::from(t.wMonth),
            u32::from(t.wDay),
            i64::from(t.wHour) * 3_600_000
                + i64::from(t.wMinute) * 60_000
                + i64::from(t.wSecond) * 1_000
                + i64::from(t.wMilliseconds),
        );
        let since_origin_ms = (now_qpc - clock.origin_qpc()) * 1_000 / freq;
        Self {
            qpc_origin: clock.origin_qpc(),
            qpc_freq: freq,
            wall_origin_local_ms: now_ms - since_origin_ms,
            play_start_us,
            play_len_us,
            qpc_end: now_qpc,
        }
    }

    /// Local wall-clock ms of run µs `t_us`.
    pub fn local_ms(&self, t_us: i64) -> i64 {
        self.wall_origin_local_ms + t_us.div_euclid(1000)
    }
}

/// What the run was for (`measure` sets these; plain `run` is a
/// diagnostic standard run).
#[derive(Clone, Copy, Debug)]
pub struct RunMeta {
    pub profile: Profile,
    pub kind: RunKind,
    pub obs_encoder: Option<spike_core::run::EncoderClass>,
    /// `measure --unattended` (design §4.3).
    pub unattended: bool,
}

/// Display class of an exact refresh rate: 240 for rates near it, else 60.
pub fn class_of(refresh_mhz: u32) -> u32 {
    if refresh_mhz > 150_000 { 240 } else { 60 }
}

#[allow(clippy::too_many_arguments)]
pub fn assemble(
    args: &RunArgs,
    meta: RunMeta,
    refresh: (u32, u32),
    timeline: Timeline,
    frames: &[SpikeFrame],
    frame_records: StreamCount,
    bga: &[BgaDue],
    bga_size: Option<(u32, u32)>,
    events: &[RunEvent],
    played: &SessionOut,
) -> RunData {
    let (app_allocs, library_allocs) = crate::alloc::snapshot();
    let input = &played.summary.input;
    let audio = played.summary.audio.as_ref();
    let render = audio.and_then(|a| a.render.as_ref());
    let mut facts = audio.map(|a| a.facts.clone()).unwrap_or_default();
    // An audio or loopback failure makes V6 invalid explicitly.
    facts.stream_error =
        audio.is_none_or(|a| a.render_error.is_some() || a.loopback_error.is_some());
    let refresh_mhz = (u64::from(refresh.0) * 1000 / u64::from(refresh.1.max(1))) as u32;
    RunData {
        info: RunInfo {
            profile: meta.profile,
            kind: meta.kind,
            obs_encoder: meta.obs_encoder,
            timeline,
            class_hz: class_of(refresh_mhz),
            refresh: Rational {
                num: u64::from(refresh.0),
                den: u64::from(refresh.1),
            },
            // The owner reports VRR (owner-notes.toml); `report` sets it.
            vrr: true,
            fps_cap: args.fps_cap,
            bga_width: bga_size.map(|s| s.0),
            bga_height: bga_size.map(|s| s.1),
            mix_rate_hz: render.map_or(0, |r| r.rate_hz),
            audio_period_us: render.map_or(0, |r| r.period_us),
            presentmon_error: None,
            unattended: args.meta.unattended,
        },
        counters: Counters {
            notes_total: input.notes_total,
            notes_played: input.judged,
            input_overflows: input.stats.overflows,
            sound_overflows: input.sound_overflows,
            bga_pool_exhausted: 0,
            voice_steals: render.map_or(0, |r| r.voice_steals),
            app_allocs,
            library_allocs,
        },
        streams: StreamCounts {
            frames: frame_records,
            input: input.records,
            audio_periods: audio.map_or(StreamCount::default(), |a| a.period_records),
            bga: StreamCount {
                produced: bga.len() as u64,
                lost: 0,
            },
        },
        audio: facts,
        owner: OwnerPublic::default(),
        obs: None,
        events: events.to_vec(),
        pm_frames: vec![],
        frames: frames.to_vec(),
        audio_periods: played.periods.clone(),
        input: played.input.clone(),
        bga: bga.to_vec(),
    }
}

pub fn write(out: &Path, run: &RunData, origin: &Origin) -> Result<(), String> {
    std::fs::create_dir_all(out.join("private")).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec(run).map_err(|e| e.to_string())?;
    std::fs::write(out.join(RUNDATA), json).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(origin).map_err(|e| e.to_string())?;
    std::fs::write(out.join(ORIGIN), json).map_err(|e| e.to_string())
}
