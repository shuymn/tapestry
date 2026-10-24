//! WASAPI audio (design D3, D4, D5): the shared-mode render stream on the
//! audio thread and the loopback capture of the same endpoint feeding the
//! pilot detector.
//!
//! Render: IAudioClient3 with the period ladder (`--audio-period
//! min|next|<frames>`: the smallest engine period, the next one up, or an
//! explicit multiple of the fundamental period), event-driven, MMCSS "Pro
//! Audio"; if IAudioClient3 refuses, IAudioClient::Initialize at the
//! default period. The thread renders `spike_core::engine::Engine` (mixer,
//! drift resampler, pilot, injected gaps) and schedules each judged sound
//! at a fixed delay after its judgment on the engine's mix clock, taken
//! from the buffer events (judgment → mix, D6, internal timestamps only).
//!
//! Loopback: AUDCLNT_STREAMFLAGS_LOOPBACK on the same endpoint, polled
//! every 2 ms. By default Windows taps the loopback stream before the
//! endpoint volume and mute (AUDCLNT_STREAMOPTIONS: the default is "tapped
//! before volume and/or mute"; POST_VOLUME_LOOPBACK, not used here, moves
//! the tap after them), so the system volume does not change what the
//! detector sees; session volume is part of the mix and is left alone. The
//! countdown's pilot-only capture calibrates the detector, which then gets
//! every packet with its device position and flags (D5).
//!
//! Device names are printed to the console only, never written to files.

use crate::alloc::{self, Tag};
use crate::threads::{Mmcss, PlaySync, SoundCmd};
use crate::timebase::{Clock, HiResTimer, qpc, qpc_freq};
use rtrb::Consumer;
use serde::Serialize;
use spike_core::engine::{Engine, Injection, MixClock};
use spike_core::loopback::{CaptureEval, CaptureFlags};
use spike_core::run::{AudioFacts, AudioPeriod, StreamCount};
use spike_core::sounds::key_bank;
use spike_core::stats::percentile_i64;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread::JoinHandle;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE,
    IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Seed of the generated key sounds.
const SOUND_SEED: u64 = 11;
/// Audio keeps running this long after play: an injected gap at play end
/// still has 3 s of capture after it (D5), and V6 needs capture past play.
pub const AUDIO_POST_US: i64 = 3_500_000;
/// Capture between the render stream's start and calibration (the engine
/// and the loopback settle), and the calibration length.
const CAL_SETTLE_US: i64 = 500_000;
const CAL_LEN_US: i64 = 1_000_000;
/// Loopback poll interval.
const LOOPBACK_POLL_US: i64 = 2_000;
/// The render stream starts silent and its pilot steps in after this
/// (before calibration, `CAL_SETTLE_US`).
const PILOT_START_MS: u64 = 250;
/// Loopback buffer.
const LOOPBACK_BUFFER_HNS: i64 = 2_000_000;
/// Starvation tests (`--starve-ms`) start at this play time and repeat.
const STARVE_FIRST_US: i64 = 10_000_000;
const STARVE_EVERY_US: i64 = 20_000_000;
/// First occurrences of each diagnostic kept with their times.
const DIAG_TIMES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeriodChoice {
    Min,
    Next,
    Frames(u32),
}

#[derive(Clone, Debug)]
pub struct AudioArgs {
    pub period: PeriodChoice,
    /// Part of the render endpoint's name; the default console endpoint
    /// otherwise.
    pub device: Option<String>,
    /// `(every_s, gap_ms)`.
    pub inject: Option<(u32, u32)>,
    pub starve_ms: Option<u32>,
    /// Audio kept queued on the device, in periods (`--audio-queue N`):
    /// each event tops the queue up to this, so a sound starts at most
    /// this much after the frame being mixed.
    pub queue_periods: u32,
}

struct Com;

impl Com {
    fn init() -> Result<Self, String> {
        // SAFETY: per-thread COM initialization, balanced by the drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| format!("CoInitializeEx: {e}"))?;
        Ok(Self)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}

fn hr<T>(what: &str, r: windows::core::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn friendly_name(d: &IMMDevice) -> String {
    // SAFETY: reading one property of a live device; the string is freed
    // with CoTaskMemFree.
    unsafe {
        let Ok(store) = d.OpenPropertyStore(STGM_READ) else {
            return "(unknown)".into();
        };
        let Ok(v) = store.GetValue(&PKEY_Device_FriendlyName) else {
            return "(unknown)".into();
        };
        match PropVariantToStringAlloc(&v) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => "(unknown)".into(),
        }
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    // SAFETY: standard COM activation on an initialized thread.
    hr("MMDeviceEnumerator", unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    })
}

/// Active render endpoints: `(name, is_default)`.
fn render_endpoints() -> Result<Vec<(IMMDevice, String)>, String> {
    let e = enumerator()?;
    // SAFETY: enumerating live endpoints.
    unsafe {
        let c = hr(
            "EnumAudioEndpoints",
            e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE),
        )?;
        let n = hr("GetCount", c.GetCount())?;
        (0..n)
            .map(|i| {
                let d = hr("Item", c.Item(i))?;
                let name = friendly_name(&d);
                Ok((d, name))
            })
            .collect()
    }
}

/// The render endpoint for `part` (case-insensitive substring of its
/// name), or the default console endpoint.
fn open_endpoint(part: Option<&str>) -> Result<IMMDevice, String> {
    match part {
        None => {
            let e = enumerator()?;
            // SAFETY: default endpoint lookup.
            hr("GetDefaultAudioEndpoint", unsafe {
                e.GetDefaultAudioEndpoint(eRender, eConsole)
            })
        }
        Some(p) => {
            let all = render_endpoints()?;
            let names: Vec<String> = all.iter().map(|(_, n)| n.clone()).collect();
            all.into_iter()
                .find(|(_, n)| n.to_lowercase().contains(&p.to_lowercase()))
                .map(|(d, _)| d)
                .ok_or_else(|| format!("no render endpoint named like {p:?}; endpoints: {names:?}"))
        }
    }
}

/// `audio-devices`: the active render endpoints (console only).
pub fn print_endpoints() -> Result<(), String> {
    let _com = Com::init()?;
    let default = {
        let e = enumerator()?;
        // SAFETY: default endpoint lookup; the id string is freed.
        unsafe {
            e.GetDefaultAudioEndpoint(eRender, eConsole)
                .ok()
                .map(|d| friendly_name(&d))
        }
    };
    for (_, name) in render_endpoints()? {
        let mark = if default.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("{name}{mark}");
    }
    Ok(())
}

/// The shared-mode mix format: 32-bit float, at least 2 channels.
struct MixFormat {
    ptr: *mut WAVEFORMATEX,
    rate: u32,
    channels: usize,
}

impl MixFormat {
    fn get(client: &IAudioClient) -> Result<Self, String> {
        // SAFETY: GetMixFormat returns a CoTaskMem allocation owned here;
        // the packed struct is read unaligned.
        unsafe {
            let ptr = hr("GetMixFormat", client.GetMixFormat())?;
            let f = ptr.read_unaligned();
            let tag = f.wFormatTag;
            let float = match u32::from(tag) {
                WAVE_FORMAT_IEEE_FLOAT => true,
                WAVE_FORMAT_EXTENSIBLE => {
                    let ext = ptr as *const WAVEFORMATEXTENSIBLE;
                    std::ptr::addr_of!((*ext).SubFormat).read_unaligned()
                        == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                }
                _ => false,
            };
            let fmt = Self {
                ptr,
                rate: f.nSamplesPerSec,
                channels: usize::from(f.nChannels),
            };
            let bits = f.wBitsPerSample;
            if !float || bits != 32 || fmt.channels < 2 {
                return Err(format!(
                    "unsupported mix format: tag {tag:#x}, {bits} bits, {} channels",
                    fmt.channels
                ));
            }
            Ok(fmt)
        }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat.
        unsafe { CoTaskMemFree(Some(self.ptr as *const _)) };
    }
}

struct OwnedEvent(HANDLE);

impl OwnedEvent {
    fn new() -> Result<Self, String> {
        // SAFETY: plain auto-reset event.
        hr("CreateEventW", unsafe {
            CreateEventW(None, false, false, None)
        })
        .map(Self)
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Engine periods reported by IAudioClient3 (frames).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct EnginePeriods {
    pub default: u32,
    pub fundamental: u32,
    pub min: u32,
    pub max: u32,
}

/// Picks the period of the ladder rung `choice`.
pub fn pick_period(p: EnginePeriods, choice: PeriodChoice) -> Result<u32, String> {
    let f = p.fundamental.max(1);
    let v = match choice {
        PeriodChoice::Min => p.min,
        PeriodChoice::Next => p.min + f,
        PeriodChoice::Frames(n) => n,
    };
    if v < p.min || v > p.max || !(v - p.min).is_multiple_of(f) {
        return Err(format!(
            "period {v} frames is not on the ladder (min {}, max {}, step {f})",
            p.min, p.max
        ));
    }
    Ok(v)
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Diagnostics {
    pub wakes: u64,
    /// Wakes that found nothing queued (the device ran dry).
    pub wakes_padding_zero: u64,
    /// Gaps between wakes longer than 1.5 periods.
    pub wake_gaps_over_1_5_periods: u64,
    pub get_buffer_failures: u64,
    /// Wake to ReleaseBuffer longer than a period.
    pub mix_over_period: u64,
    /// Sounds whose judgment + fixed delay had already been mixed: they
    /// start at the first frame not yet mixed instead.
    pub sounds_at_earliest: u64,
    /// Play-relative µs of the first occurrences.
    pub padding_zero_at_us: Vec<i64>,
    pub wake_gap_at_us: Vec<i64>,
    pub mix_over_period_at_us: Vec<i64>,
}

fn note_at(v: &mut Vec<i64>, t: i64) {
    if v.len() < v.capacity() {
        v.push(t);
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Starve {
    /// Play-relative µs when writes were withheld.
    pub at_us: i64,
    pub withheld_ms: u32,
    /// Audio queued on the device when writes stopped.
    pub queued_us: i64,
    pub exceeds_queued: bool,
    /// Wakes that found the device buffer empty from the start of the
    /// withholding until a period after writes resumed (the diagnostic
    /// that should match the episode).
    pub padding_zero_wakes: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RenderSide {
    pub backend: String,
    /// MMCSS "Pro Audio" registration succeeded.
    pub mmcss: bool,
    pub queue_periods: u32,
    /// Each event's lateness against the least-late event of the mix
    /// clock's window (`engine::MixClock`), µs: what the clock leaves out.
    pub wake_lateness_us_p50: Option<i64>,
    pub wake_lateness_us_p99: Option<i64>,
    pub wake_lateness_us_max: Option<i64>,
    pub periods: Option<EnginePeriods>,
    pub period_frames: u32,
    pub period_us: u32,
    pub buffer_frames: u32,
    pub rate_hz: u32,
    pub channels: usize,
    pub resampler_delay_frames: u64,
    /// Fixed judgment → scheduled-frame delay.
    pub schedule_delay_frames: u64,
    pub diagnostics: Diagnostics,
    /// Play-relative µs (mix clock) of each injected gap, and its device
    /// frame (the stream's frame index).
    pub injected_us: Vec<i64>,
    pub injected_frames: Vec<u64>,
    /// The stream is silent before this frame; its pilot starts there.
    pub pilot_start_frame: u64,
    pub starves: Vec<Starve>,
    pub voice_steals: u64,
    pub hard_cuts: u64,
    pub clamped: u64,
    pub sound_commands: u64,
    pub endpoint_volume_db: Option<f32>,
    pub endpoint_muted: Option<bool>,
}

pub struct RenderOut {
    pub side: RenderSide,
    pub periods: Vec<AudioPeriod>,
    pub period_records: StreamCount,
    /// Judgment → scheduled device frame, µs.
    pub judge_to_mix_us: Vec<i64>,
    /// Mix-clock anchors per wake: (device frame, run µs), for the
    /// private trace.
    pub anchors: Vec<(u64, i64)>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LoopbackSide {
    pub calibration_amplitude: Option<f64>,
    /// Capture position where the render stream's pilot steps in (its
    /// frame `pilot_start_frame`), and each episode's start position:
    /// injected gaps are checked in this frame domain
    /// (`pilot::OnsetFinder`).
    pub onset_position: Option<u64>,
    pub episode_positions: Vec<u64>,
    pub packets: u64,
    pub silent_packets: u64,
    pub discontinuity_packets: u64,
    /// Packets with an uncertain timestamp, left out of the time map.
    pub timestamp_errors: u64,
    /// Largest residual of the capture-position → time map (µs), and the
    /// timestamps it used.
    pub map_error_us: Option<i64>,
    pub anchors_used: usize,
    /// Capture position of the stop cut, and whether capture beyond it had
    /// already been evaluated.
    pub cut_position: Option<u64>,
    pub cut_late: bool,
    pub error: Option<String>,
}

pub struct LoopbackOut {
    pub side: LoopbackSide,
    pub facts: AudioFacts,
    /// Capture packet anchors: (device position, run µs).
    pub anchors: Vec<(u64, i64)>,
}

/// Shared between the render and loopback threads.
pub struct AudioShared {
    /// Run µs when the render stream started (0 until then).
    pub started_us: AtomicI64,
    /// Run µs when the render thread stopped writing (`i64::MAX` until
    /// then): the loopback evaluates capture only up to shortly before it,
    /// since the stream's stop is a cut to silence, not a dropout.
    pub stopped_us: AtomicI64,
    pub render_done: AtomicBool,
    pub stop: Arc<AtomicBool>,
}

pub struct AudioThreads {
    pub render: JoinHandle<Result<RenderOut, String>>,
    pub loopback: JoinHandle<Result<LoopbackOut, String>>,
}

/// Spawns the audio thread (a G6 thread: it waits at `sync`) and the
/// loopback thread.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    args: AudioArgs,
    sounds: Consumer<SoundCmd>,
    sync: Arc<PlaySync>,
    clock: Clock,
    play_len: i64,
    notes: usize,
    stop: Arc<AtomicBool>,
) -> Result<AudioThreads, String> {
    let shared = Arc::new(AudioShared {
        started_us: AtomicI64::new(0),
        stopped_us: AtomicI64::new(i64::MAX),
        render_done: AtomicBool::new(false),
        stop,
    });
    let render = {
        let (shared, args) = (shared.clone(), args.clone());
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                alloc::tag_thread(Tag::Audio);
                let r = render_thread(&args, sounds, &sync, clock, play_len, notes, &shared);
                shared
                    .stopped_us
                    .fetch_min(clock.now_us(), Ordering::AcqRel);
                shared.render_done.store(true, Ordering::Release);
                if let Err(e) = &r {
                    // Release the threads waiting to start (no effect once
                    // play has started).
                    sync.abort(format!("audio: {e}"));
                }
                r
            })
            .map_err(|e| format!("spawn audio thread: {e}"))?
    };
    let loopback = std::thread::Builder::new()
        .name("loopback".into())
        .spawn(move || loopback_thread(&args, clock, &shared))
        .map_err(|e| format!("spawn loopback thread: {e}"))?;
    Ok(AudioThreads { render, loopback })
}

fn endpoint_volume(d: &IMMDevice) -> (Option<f32>, Option<bool>) {
    // SAFETY: read-only queries of the endpoint volume interface.
    unsafe {
        match d.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            Ok(v) => (
                v.GetMasterVolumeLevel().ok(),
                v.GetMute().ok().map(|m| m.as_bool()),
            ),
            Err(_) => (None, None),
        }
    }
}

fn render_thread(
    args: &AudioArgs,
    mut sounds: Consumer<SoundCmd>,
    sync: &PlaySync,
    clock: Clock,
    play_len: i64,
    notes: usize,
    shared: &AudioShared,
) -> Result<RenderOut, String> {
    let _com = Com::init()?;
    let mmcss = Mmcss::join(&windows::core::HSTRING::from("Pro Audio"));
    let device = open_endpoint(args.device.as_deref())?;
    println!("audio: render endpoint {}", friendly_name(&device));
    let (endpoint_volume_db, endpoint_muted) = endpoint_volume(&device);
    // SAFETY: COM calls on live interfaces; the format pointer is owned by
    // `fmt` and outlives every call that reads it.
    let (client, fmt, backend, engine_periods, period) = unsafe {
        let c3: IAudioClient3 = hr("Activate IAudioClient3", device.Activate(CLSCTX_ALL, None))?;
        let base: IAudioClient = hr("IAudioClient", windows::core::Interface::cast(&c3))?;
        let fmt = MixFormat::get(&base)?;
        let mut p = EnginePeriods::default();
        let three = c3
            .GetSharedModeEnginePeriod(
                fmt.ptr,
                &mut p.default,
                &mut p.fundamental,
                &mut p.min,
                &mut p.max,
            )
            .ok()
            .map(|_| pick_period(p, args.period))
            .transpose()
            .or_else(|e| match args.period {
                // Only the smallest period may fall back to the default
                // one; an explicit rung off the ladder is an error.
                PeriodChoice::Min => Ok(None),
                _ => Err(e),
            })?
            .and_then(|period| {
                c3.InitializeSharedAudioStream(
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    period,
                    fmt.ptr,
                    None,
                )
                .ok()
                .map(|_| period)
            });
        match three {
            Some(period) => (
                base,
                fmt,
                format!("IAudioClient3 shared, {period} frames"),
                Some(p),
                period,
            ),
            None => {
                // Fallback: a fresh client at the default period.
                let c: IAudioClient =
                    hr("Activate IAudioClient", device.Activate(CLSCTX_ALL, None))?;
                hr(
                    "IAudioClient::Initialize",
                    c.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        0,
                        0,
                        fmt.ptr,
                        None,
                    ),
                )?;
                let (mut def, mut min) = (0i64, 0i64);
                hr(
                    "GetDevicePeriod",
                    c.GetDevicePeriod(Some(&mut def), Some(&mut min)),
                )?;
                let period = (def * i64::from(fmt.rate) / 10_000_000) as u32;
                (
                    c,
                    fmt,
                    format!("IAudioClient shared default period, {period} frames"),
                    Some(p).filter(|p| p.min > 0),
                    period,
                )
            }
        }
    };
    let rate = fmt.rate;
    let channels = fmt.channels;
    let event = OwnedEvent::new()?;
    // SAFETY: COM calls on the initialized client.
    let (render, buffer_frames) = unsafe {
        hr("SetEventHandle", client.SetEventHandle(event.0))?;
        let render: IAudioRenderClient = hr("IAudioRenderClient", client.GetService())?;
        let buffer = hr("GetBufferSize", client.GetBufferSize())?;
        (render, buffer)
    };
    let mut engine = Engine::new(rate, key_bank(SOUND_SEED, rate), None);
    let mut mix_clock = MixClock::new(rate);
    // Queued audio: whole periods, at most what the buffer holds.
    let queue_periods = args.queue_periods.clamp(1, (buffer_frames / period).max(1));
    let queue_frames = queue_periods * period;
    let pilot_start_frame = u64::from(rate) * PILOT_START_MS / 1000;
    engine.set_pilot_start(pilot_start_frame);
    let period_us = (u64::from(period) * 1_000_000 / u64::from(rate)) as i64;
    let total_us = 6_000_000 + play_len + AUDIO_POST_US;
    let mut mix_anchors: Vec<(u64, i64)> =
        Vec::with_capacity((total_us / period_us.max(1)) as usize * 2 + 1_000);
    let mut period_log: Vec<AudioPeriod> =
        Vec::with_capacity((total_us / period_us.max(1)) as usize * 2 + 1_000);
    let mut lateness: Vec<i64> = Vec::with_capacity(period_log.capacity());
    let mut period_records = StreamCount::default();
    let mut judge_to_mix: Vec<i64> = Vec::with_capacity(notes + 16);
    let mut diag = Diagnostics {
        padding_zero_at_us: Vec::with_capacity(DIAG_TIMES),
        wake_gap_at_us: Vec::with_capacity(DIAG_TIMES),
        mix_over_period_at_us: Vec::with_capacity(DIAG_TIMES),
        ..Default::default()
    };
    // Design D3/`mixer::schedule`: the judgment time plus the resampler's
    // delay and one period, never before the first frame not yet mixed
    // (frames already queued on the device or staged cannot change).
    let schedule_delay = u64::from(period) + engine.resampler_delay();
    let gaps = match args.inject {
        Some((every_s, _)) => (play_len / (i64::from(every_s) * 1_000_000)) as usize,
        None => 0,
    };
    let mut injected_us: Vec<i64> = Vec::with_capacity(gaps);
    let mut injected_frames: Vec<u64> = Vec::with_capacity(gaps);
    let starve_count = match args.starve_ms {
        Some(_) if play_len > STARVE_FIRST_US => {
            ((play_len - STARVE_FIRST_US) / STARVE_EVERY_US + 1) as usize
        }
        _ => 0,
    };
    let mut starves: Vec<Starve> = Vec::with_capacity(starve_count);
    let freq = qpc_freq();

    let play_start = sync.ready()?;
    // Prefill the queue (whole periods, silent before the pilot step),
    // then start.
    // SAFETY: writes `queue_frames` (at most the buffer) into the buffer
    // just obtained.
    unsafe {
        let p = hr("GetBuffer", render.GetBuffer(queue_frames))?;
        let out = std::slice::from_raw_parts_mut(p as *mut f32, queue_frames as usize * channels);
        engine.render(out, channels, queue_frames as usize);
        hr("ReleaseBuffer", render.ReleaseBuffer(queue_frames, 0))?;
        hr("Start", client.Start())?;
    }
    shared
        .started_us
        .store(clock.now_us().max(1), Ordering::Release);

    let mut injection: Option<Injection> = None;
    let mut next_gap = 0u64;
    let mut last_wake: Option<i64> = None;
    let mut starve_until: Option<i64> = None;
    let mut next_starve = 0usize;
    let mut sound_commands = 0u64;
    let end_us = play_len + AUDIO_POST_US;
    loop {
        // SAFETY: valid event handle.
        let w = unsafe { WaitForSingleObject(event.0, 200) };
        let wake_q = qpc();
        let now_play = clock.to_us(wake_q) - play_start;
        if shared.stop.load(Ordering::Relaxed) || now_play > end_us {
            break;
        }
        if w != WAIT_OBJECT_0 {
            continue;
        }
        diag.wakes += 1;
        if let Some(prev) = last_wake
            && (wake_q - prev) * 2 > 3 * period_us * freq / 1_000_000
        {
            diag.wake_gaps_over_1_5_periods += 1;
            note_at(&mut diag.wake_gap_at_us, now_play);
        }
        last_wake = Some(wake_q);
        // SAFETY: COM calls on the started stream.
        let padding = unsafe { client.GetCurrentPadding() }.unwrap_or(buffer_frames);
        if padding == 0 {
            diag.wakes_padding_zero += 1;
            note_at(&mut diag.padding_zero_at_us, now_play);
            if let Some(st) = starves.last_mut()
                && now_play <= st.at_us + i64::from(st.withheld_ms) * 1_000 + period_us
            {
                st.padding_zero_wakes += 1;
            }
        }
        // The engine mix clock: the event fires when the audio engine has
        // taken a period from the buffer, so the first frame still queued
        // (written minus padding) is read at the next pass, a period after
        // that pass. The wake itself may come late; `MixClock` keeps the
        // least-late wake of its window, so lateness does not enter the
        // clock (engine::MixClock). IAudioClock positions were not used:
        // on the RME they advanced a period at a time against a current
        // QPC.
        let queued_from = engine.device_frames() - u64::from(padding);
        mix_clock.observe(queued_from, clock.to_us(wake_q) + period_us);
        if lateness.len() < lateness.capacity() {
            lateness.push(mix_clock.latest_error_us());
        }
        if mix_anchors.len() < mix_anchors.capacity() {
            mix_anchors.push((queued_from, clock.to_us(wake_q) + period_us));
        }
        let mixed_at = |f: u64| mix_clock.at(f).unwrap_or(0);
        if let (None, Some((every_s, gap_ms))) = (injection, args.inject) {
            let every = u64::from(every_s) * u64::from(rate);
            injection = Some(Injection {
                first: mix_clock.frame_at(play_start).unwrap_or(0) + every,
                every,
                len: u64::from(gap_ms) * u64::from(rate) / 1000,
                count: gaps as u64,
            });
            engine.set_injection(injection);
        }
        // Starvation: withhold writes for `starve_ms` from each start.
        if let Some(ms) = args.starve_ms
            && next_starve < starve_count
            && starve_until.is_none()
            && now_play >= STARVE_FIRST_US + next_starve as i64 * STARVE_EVERY_US
        {
            let queued_us = i64::from(padding) * 1_000_000 / i64::from(rate);
            starves.push(Starve {
                at_us: now_play,
                withheld_ms: ms,
                queued_us,
                exceeds_queued: i64::from(ms) * 1_000 > queued_us,
                padding_zero_wakes: 0,
            });
            next_starve += 1;
            starve_until = Some(now_play + i64::from(ms) * 1_000);
        }
        if let Some(u) = starve_until {
            if now_play < u {
                continue;
            }
            starve_until = None;
        }
        // Sound commands: each at a fixed delay after its judgment.
        while let Ok(cmd) = sounds.pop() {
            sound_commands += 1;
            let judged_us = clock.to_us(cmd.judged_qpc);
            let want = mix_clock.frame_at(judged_us).unwrap_or(0) + schedule_delay;
            let at = engine.trigger_at_device(cmd.sound, want);
            if at > want {
                diag.sounds_at_earliest += 1;
            }
            let at_us = mixed_at(at);
            if judge_to_mix.len() < judge_to_mix.capacity() {
                judge_to_mix.push(at_us - judged_us);
            }
        }
        let frames = queue_frames.saturating_sub(padding) / period * period;
        if frames == 0 {
            continue;
        }
        // Injected gaps rendered in this block, timed by the sample clock.
        if let Some(inj) = injection {
            let end = engine.device_frames() + u64::from(frames);
            while next_gap < inj.count && inj.start(next_gap) < end {
                let f = inj.start(next_gap);
                let t = mixed_at(f);
                if injected_us.len() < injected_us.capacity() {
                    injected_us.push(t - play_start);
                    injected_frames.push(f);
                }
                next_gap += 1;
            }
        }
        // SAFETY: writes exactly `frames` frames into the buffer obtained.
        match unsafe { render.GetBuffer(frames) } {
            Ok(p) => unsafe {
                let out = std::slice::from_raw_parts_mut(p as *mut f32, frames as usize * channels);
                engine.render(out, channels, frames as usize);
                let _ = render.ReleaseBuffer(frames, 0);
            },
            Err(_) => {
                diag.get_buffer_failures += 1;
                continue;
            }
        }
        if (qpc() - wake_q) * 1_000_000 > period_us * freq {
            diag.mix_over_period += 1;
            note_at(&mut diag.mix_over_period_at_us, now_play);
        }
        period_records.produced += 1;
        let rec = AudioPeriod {
            t_us: clock.to_us(wake_q),
            active_voices: engine.mixer().active() as u16,
        };
        if period_log.len() < period_log.capacity() {
            period_log.push(rec);
        } else {
            period_records.lost += 1;
        }
    }
    shared
        .stopped_us
        .fetch_min(clock.now_us(), Ordering::AcqRel);
    // SAFETY: stopping the stream this thread started.
    unsafe {
        let _ = client.Stop();
    }
    Ok(RenderOut {
        side: RenderSide {
            backend,
            mmcss: mmcss.is_some(),
            queue_periods,
            wake_lateness_us_p50: percentile_i64(&lateness, 50),
            wake_lateness_us_p99: percentile_i64(&lateness, 99),
            wake_lateness_us_max: lateness.iter().max().copied(),
            periods: engine_periods,
            period_frames: period,
            period_us: period_us as u32,
            buffer_frames,
            rate_hz: rate,
            channels,
            resampler_delay_frames: engine.resampler_delay(),
            schedule_delay_frames: schedule_delay,
            diagnostics: diag,
            injected_us,
            injected_frames,
            pilot_start_frame,
            starves,
            voice_steals: engine.mixer().steals,
            hard_cuts: engine.mixer().hard_cuts,
            clamped: engine.clamped,
            sound_commands,
            endpoint_volume_db,
            endpoint_muted,
        },
        periods: period_log,
        period_records,
        judge_to_mix_us: judge_to_mix,
        anchors: mix_anchors,
    })
}

fn loopback_thread(
    args: &AudioArgs,
    clock: Clock,
    shared: &AudioShared,
) -> Result<LoopbackOut, String> {
    let _com = Com::init()?;
    let device = open_endpoint(args.device.as_deref())?;
    // SAFETY: COM calls on live interfaces; the format outlives them.
    let (client, capture, rate, channels) = unsafe {
        let c: IAudioClient = hr(
            "Activate loopback client",
            device.Activate(CLSCTX_ALL, None),
        )?;
        let fmt = MixFormat::get(&c)?;
        hr(
            "loopback Initialize",
            c.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                LOOPBACK_BUFFER_HNS,
                0,
                fmt.ptr,
                None,
            ),
        )?;
        let cap: IAudioCaptureClient = hr("IAudioCaptureClient", c.GetService())?;
        hr("loopback Start", c.Start())?;
        (c, cap, fmt.rate, fmt.channels)
    };
    let timer = HiResTimer::new()?;
    let freq = qpc_freq();
    let poll_qpc = freq * LOOPBACK_POLL_US / 1_000_000;
    let max_packet = rate as usize; // 1 s, far above a 200 ms buffer
    let mut left = vec![0.0f32; max_packet];
    // One timestamp per packet: up to 15 minutes of 1 ms packets.
    let mut eval = CaptureEval::new(rate, CAL_SETTLE_US, CAL_LEN_US, 1_000_000);
    let mut side = LoopbackSide::default();
    let mut next_poll = qpc();
    let mut drained_after_done = false;
    loop {
        timer.wait_until(next_poll);
        next_poll += poll_qpc;
        let done = shared.render_done.load(Ordering::Acquire);
        let started = shared.started_us.load(Ordering::Acquire) > 0;
        loop {
            // SAFETY: COM calls on the started capture stream; the packet
            // is read before it is released.
            let n = unsafe { capture.GetNextPacketSize() }.unwrap_or(0);
            if n == 0 {
                break;
            }
            let (mut data, mut frames, mut flags, mut pos, mut qpos) =
                (std::ptr::null_mut(), 0u32, 0u32, 0u64, 0u64);
            if unsafe {
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut pos),
                    Some(&mut qpos),
                )
            }
            .is_err()
            {
                break;
            }
            let count = (frames as usize).min(max_packet);
            let f = CaptureFlags {
                silent: flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
                discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                timestamp_error: flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0,
            };
            if !f.silent {
                // SAFETY: the packet holds `frames` frames of `channels`
                // f32 samples.
                let src = unsafe {
                    std::slice::from_raw_parts(data as *const f32, frames as usize * channels)
                };
                for (k, x) in left[..count].iter_mut().enumerate() {
                    *x = src[k * channels];
                }
            } else {
                left[..count].fill(0.0);
            }
            // SAFETY: releases the packet just read.
            unsafe {
                let _ = capture.ReleaseBuffer(frames);
            }
            side.packets += 1;
            side.silent_packets += u64::from(f.silent);
            side.discontinuity_packets += u64::from(f.discontinuity);
            let t_us = clock.to_us((i128::from(qpos) * i128::from(freq) / 10_000_000) as i64);
            eval.push(pos, f, t_us, &left[..count], started);
        }
        if done {
            // One more pass after the render stream stopped, for the
            // engine's last periods.
            if drained_after_done {
                break;
            }
            drained_after_done = true;
            next_poll = qpc() + freq / 20;
        }
    }
    // SAFETY: stopping the stream this thread started.
    unsafe {
        let _ = client.Stop();
    }
    let anchors = eval.anchors().to_vec();
    let stopped = shared.stopped_us.load(Ordering::Acquire);
    let (facts, sum) = eval.finish((stopped != i64::MAX).then_some(stopped));
    side.calibration_amplitude = sum.calibration_amplitude;
    side.onset_position = sum.onset_position;
    side.episode_positions = sum.episode_positions;
    side.timestamp_errors = sum.timestamp_errors;
    side.map_error_us = sum.map_error_us;
    side.anchors_used = sum.anchors_used;
    side.cut_position = sum.cut_position;
    side.cut_late = sum.cut_late;
    side.error = sum.error.map(str::to_owned);
    Ok(LoopbackOut {
        side,
        facts,
        anchors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_ladder() {
        let p = EnginePeriods {
            default: 480,
            fundamental: 48,
            min: 144,
            max: 480,
        };
        assert_eq!(pick_period(p, PeriodChoice::Min), Ok(144));
        assert_eq!(pick_period(p, PeriodChoice::Next), Ok(192));
        assert_eq!(pick_period(p, PeriodChoice::Frames(240)), Ok(240));
        assert!(pick_period(p, PeriodChoice::Frames(200)).is_err());
        assert!(pick_period(p, PeriodChoice::Frames(528)).is_err());
    }
}
