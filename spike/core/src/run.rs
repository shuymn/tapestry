//! What a run records, in export-safe form.
//!
//! Every field is either a relative µs time, a count, a duration, or a
//! setting; nothing here holds raw QPC, wall-clock time, paths or names. The
//! app converts raw sources (QPC, PresentMon CSV, OBS log) into these types
//! on the measurement PC, and `report --from-export` rebuilds the same
//! `RunData` from the export.

use crate::heavy::Timeline;
use crate::obslog::ObsDerived;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Standard,
    Streaming,
    NoBga,
}

/// Which matrix cell the run is; decides which gates apply (§3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunKind {
    /// R1: no OBS; G2/G3 gate.
    Baseline,
    /// R2/R3: OBS streaming; O1/O2 gate, G2/G3 recorded.
    Obs,
    /// S0 and other diagnostic runs: G2/G3 recorded.
    Diagnostic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncoderClass {
    /// NVENC, AMD AMF or Intel QSV.
    Hardware,
    X264,
}

/// Exact display refresh rate from QueryDisplayConfig (`num / den` Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: u64,
    pub den: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub profile: Profile,
    pub kind: RunKind,
    /// Required for `RunKind::Obs`.
    pub obs_encoder: Option<EncoderClass>,
    pub timeline: Timeline,
    pub refresh: Rational,
    /// Frame cap; `None` means the cap is the refresh rate.
    pub fps_cap: Option<u32>,
    /// Decoded BGA size; `None` when BGA is off.
    pub bga_width: Option<u32>,
    pub bga_height: Option<u32>,
    pub mix_rate_hz: u32,
    /// Device period the audio thread ran at (V8 audio-stream edges).
    pub audio_period_us: u32,
    /// Set when the PresentMon CSV could not be parsed to the end; a fixed
    /// category message, never file content.
    pub presentmon_error: Option<String>,
}

/// One PresentMon row of the spike's swap chain.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PmFrame {
    /// When the Present call started.
    pub present_us: i64,
    /// When the frame reached the screen; `None` if not displayed.
    pub display_us: Option<i64>,
    /// `MsBetweenDisplayChange`; `None` (NA) if not displayed.
    pub display_change_ms: Option<f64>,
    pub gpu_busy_ms: Option<f64>,
}

impl PmFrame {
    /// Time used for heavy-window assignment: display time, or the present
    /// time for a frame that was not displayed.
    pub fn window_time_us(&self) -> i64 {
        self.display_us.unwrap_or(self.present_us)
    }
}

/// One frame from the spike's own render-thread log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpikeFrame {
    /// When the render thread started building the frame (its load counters
    /// below belong to this time).
    pub render_us: i64,
    /// When the spike called Present for it; `None` if it was not presented.
    pub present_us: Option<i64>,
    /// Update, sprite building, BGA upload command and submission (no waits).
    pub render_cpu_us: u32,
    /// Diagnostic only.
    pub present_call_us: u32,
    /// The spike's own present count for this frame (`GetLastPresentCount`
    /// after its Present); pairs the frame with DXGI statistics and, for
    /// V4, with PresentMon rows. `None` offscreen or if the call failed.
    pub present_count: Option<u32>,
    pub visible_notes: u32,
    pub effect_instances: u32,
    pub lanes_with_effects: u8,
    pub bga_frame_shown: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioPeriod {
    pub t_us: i64,
    pub active_voices: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLatency {
    /// QPC on the input thread.
    pub t_us: i64,
    /// Input thread → logic thread.
    pub latency_us: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BgaDue {
    pub due_us: i64,
    pub displayed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    FocusLoss,
    Suspend,
    Resume,
    SwapChainRecreate,
    RefreshChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    pub t_us: i64,
    pub kind: EventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadTag {
    Input,
    Logic,
    Audio,
    Render,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub notes_total: u32,
    pub notes_played: u32,
    pub input_overflows: u64,
    pub sound_overflows: u64,
    pub bga_pool_exhausted: u64,
    pub voice_steals: u64,
    /// App-code allocations after the countdown, by thread (G6).
    pub app_allocs: BTreeMap<ThreadTag, u64>,
    /// Allocations inside named library scopes, by scope (G6r).
    pub library_allocs: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub start_us: i64,
    pub end_us: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFacts {
    pub pilot_calibrated: bool,
    pub unverifiable_spans: u32,
    /// Span the detector evaluated (after its warm-up), mapped to run µs;
    /// V6 requires it to cover the play window.
    pub capture_from_us: Option<i64>,
    pub capture_to_us: Option<i64>,
    /// Confirmed discontinuity episodes over the whole run (G5).
    pub episodes: Vec<Episode>,
}

/// Owner-note fields that may leave the PC.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerPublic {
    /// OBS runs: the game was visible in the ingest output.
    pub game_visible: Option<bool>,
    /// OBS runs: game audio was present in the ingest output.
    pub audio_present: Option<bool>,
}

/// Producer-side count of one measurement stream: records produced and
/// records the recorder could not keep (ring full). Written records are the
/// stream's rows; evidence is complete only if `produced == rows` and
/// `lost == 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCount {
    pub produced: u64,
    pub lost: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCounts {
    pub frames: StreamCount,
    pub input: StreamCount,
    pub audio_periods: StreamCount,
    pub bga: StreamCount,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunData {
    pub info: RunInfo,
    pub counters: Counters,
    pub streams: StreamCounts,
    pub audio: AudioFacts,
    pub owner: OwnerPublic,
    pub obs: Option<ObsDerived>,
    pub events: Vec<RunEvent>,
    pub pm_frames: Vec<PmFrame>,
    pub frames: Vec<SpikeFrame>,
    pub audio_periods: Vec<AudioPeriod>,
    pub input: Vec<InputLatency>,
    pub bga: Vec<BgaDue>,
}

/// Named library scopes the allocator attributes allocations to (G6r). The
/// export scan accepts only these as `library_allocs` keys.
pub const LIBRARY_SCOPES: &[&str] = &[
    "create_command_encoder",
    "write_texture",
    "write_buffer",
    "submit",
    "get_current_texture",
    "present",
    "ffmpeg_decode",
    "other_library",
];
