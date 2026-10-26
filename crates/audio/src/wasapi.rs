//! Windows: WASAPI shared-mode output (design.md "Audio", "Path from Press
//! to Sound"; stage1.md D12).
//!
//! The stream runs on the default render endpoint in the endpoint's mix
//! format, event-driven. `IAudioClient3` initializes it at the shortest
//! engine period; if the endpoint refuses that, a fresh `IAudioClient`
//! initializes it at the default period, and the refusal is recorded in
//! [`Grant::fallback`]. The output keeps two periods queued (fewer only if
//! the buffer cannot hold them), and the thread that opens it joins the
//! MMCSS "Pro Audio" task.
//!
//! The mix clock is taken from the buffer events by the engine.
//! `IAudioClock::GetPosition` is read at each wake only as a diagnostic
//! ([`WasapiStats::position`]).

use std::fmt;

use bms_model::{TimeSource, TimeUs};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOT_FOUND, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioClient3, IAudioClock, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{HRESULT, Interface, w};

use crate::CHANNELS;
use crate::output::{AudioOutput, OutputError, Wake};

/// Periods kept queued on the device (design.md "Path from Press to
/// Sound": with one, audio dropped out repeatedly).
pub const QUEUE_PERIODS: u32 = 2;

/// How long a wait for a buffer event may take before the output reports
/// [`OutputError::Stalled`], ms: far beyond any shared-mode period.
pub const EVENT_TIMEOUT_MS: u32 = 500;

/// Why the output could not be opened or closed.
#[derive(Debug)]
pub enum WasapiError {
    /// There is no default render endpoint.
    NoEndpoint,
    /// The endpoint's mix format is not 32-bit float with at least two
    /// channels.
    UnsupportedFormat {
        /// `wFormatTag`.
        tag: u16,
        /// `wBitsPerSample`.
        bits: u16,
        /// `nChannels`.
        channels: u16,
    },
    /// An OS call failed.
    Os {
        /// The call.
        call: &'static str,
        /// Its error.
        error: windows::core::Error,
    },
}

impl WasapiError {
    const fn os(call: &'static str, error: windows::core::Error) -> Self {
        Self::Os { call, error }
    }
}

impl fmt::Display for WasapiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoEndpoint => f.write_str("there is no default audio output device"),
            Self::UnsupportedFormat {
                tag,
                bits,
                channels,
            } => write!(
                f,
                "the audio device's shared format is not supported \
                 (tag {tag:#06x}, {bits} bits, {channels} channels; \
                 32-bit float with at least 2 channels is required)"
            ),
            Self::Os { call, error } => write!(f, "{call} failed: {error}"),
        }
    }
}

impl std::error::Error for WasapiError {}

/// The shared-mode engine periods an endpoint offers, in frames
/// (`IAudioClient3::GetSharedModeEnginePeriod`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EnginePeriods {
    /// The default period.
    pub default: u32,
    /// The step between allowed periods.
    pub fundamental: u32,
    /// The shortest period.
    pub min: u32,
    /// The longest period.
    pub max: u32,
}

/// Why `IAudioClient3` was not used: the call that refused and its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fallback {
    /// The refusing call.
    pub call: &'static str,
    /// Its `HRESULT`.
    pub code: i32,
}

/// What the endpoint granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// Frames per second.
    pub rate: u32,
    /// Channels of the endpoint's format; the output fills the first two.
    pub channels: u16,
    /// The engine period, frames.
    pub period: u32,
    /// The device buffer, frames.
    pub buffer: u32,
    /// Periods kept queued.
    pub queue_periods: u32,
    /// The periods `IAudioClient3` offered, when it answered.
    pub engine_periods: Option<EnginePeriods>,
    /// Why the default period was used instead of the shortest, if it was.
    pub fallback: Option<Fallback>,
    /// Whether the opening thread joined MMCSS "Pro Audio".
    pub mmcss: bool,
}

impl Grant {
    /// The engine period, µs (floor).
    #[must_use]
    pub fn period_us(&self) -> i64 {
        i64::from(self.period) * 1_000_000 / i64::from(self.rate.max(1))
    }
}

/// Counters of a running output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WasapiStats {
    /// Buffer events received.
    pub wakes: u64,
    /// Frames released to the device.
    pub frames: u64,
    /// The device position at the latest wake, frames
    /// (`IAudioClock::GetPosition`). Diagnostic only: the mix clock does
    /// not use it.
    pub position: Option<u64>,
    /// Wakes whose position read failed.
    pub position_failures: u64,
}

/// Balances a successful `CoInitializeEx` on the thread that made it.
struct Com;

impl Com {
    /// Joins the multithreaded apartment. A thread already in a
    /// single-threaded apartment keeps it, and COM is usable there too.
    fn init() -> Result<Option<Self>, WasapiError> {
        // SAFETY: per-thread COM initialization, balanced by the drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Ok(None);
        }
        hr.ok().map_err(|e| WasapiError::os("CoInitializeEx", e))?;
        Ok(Some(Self))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct Mmcss(HANDLE);

impl Mmcss {
    fn pro_audio() -> Option<Self> {
        let mut task_index = 0;
        // SAFETY: a valid task name and out pointer.
        unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }
            .ok()
            .map(Self)
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this
        // thread.
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, WasapiError> {
        // SAFETY: an unnamed auto-reset event, initially unsignaled.
        unsafe { CreateEventW(None, false, false, None) }
            .map(Self)
            .map_err(|e| WasapiError::os("CreateEventW", e))
    }

    /// Waits for the event; false on a timeout or failure.
    fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: a valid event handle.
        unsafe { WaitForSingleObject(self.0, timeout_ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: an owned handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The endpoint's mix format, owned (allocated by `GetMixFormat`).
struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat.
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

/// The rate and channels of a mix format, if the output can write it:
/// 32-bit float, plain or extensible, with at least two channels.
///
/// # Safety
///
/// `format` points to a readable `WAVEFORMATEX`, followed by the rest of a
/// `WAVEFORMATEXTENSIBLE` when its tag says so.
unsafe fn check_format(format: *const WAVEFORMATEX) -> Result<(u32, u16), WasapiError> {
    // SAFETY: readable per the contract; the packed struct is read
    // unaligned.
    let f = unsafe { format.read_unaligned() };
    let tag = f.wFormatTag;
    let float = match u32::from(tag) {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            let ext = format.cast::<WAVEFORMATEXTENSIBLE>();
            // SAFETY: an extensible tag means the whole extensible struct is
            // readable (contract).
            let sub = unsafe { std::ptr::addr_of!((*ext).SubFormat).read_unaligned() };
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    };
    let (bits, channels) = (f.wBitsPerSample, f.nChannels);
    if !float || bits != 32 || usize::from(channels) < CHANNELS {
        return Err(WasapiError::UnsupportedFormat {
            tag,
            bits,
            channels,
        });
    }
    Ok((f.nSamplesPerSec, channels))
}

/// Periods to keep queued in a buffer of `buffer` frames.
fn queue_periods(buffer: u32, period: u32) -> u32 {
    (buffer / period.max(1)).clamp(1, QUEUE_PERIODS)
}

/// Frames to write: whole periods up to `queue` frames queued.
fn room(queue: u32, padding: u32, period: u32) -> u32 {
    let period = period.max(1);
    queue.saturating_sub(padding) / period * period
}

/// A duration in 100 ns units as frames at `rate` (floor).
fn hns_to_frames(hns: i64, rate: u32) -> u32 {
    u32::try_from(i128::from(hns.max(0)) * i128::from(rate) / 10_000_000).unwrap_or(u32::MAX)
}

/// Copies interleaved stereo `src` into `dst`, native-endian `f32` frames
/// of `channels` channels: left and right into the first two, silence in
/// the rest.
fn spread(src: &[f32], dst: &mut [u8], channels: usize) {
    let sample = size_of::<f32>();
    for (frame, out) in src
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .zip(dst.chunks_exact_mut(channels * sample))
    {
        for (c, slot) in out.chunks_exact_mut(sample).enumerate() {
            let x = frame.get(c).copied().unwrap_or(0.0);
            slot.copy_from_slice(&x.to_ne_bytes());
        }
    }
}

/// A device position in `hz` units per second as frames at `rate`
/// (floor); `None` for a zero frequency or past `u64`.
fn position_frames(position: u64, hz: u64, rate: u32) -> Option<u64> {
    let frames = u128::from(position) * u128::from(rate);
    u64::try_from(frames.checked_div(u128::from(hz))?).ok()
}

/// The output error for a failed call on a running stream.
fn output_error(e: &windows::core::Error) -> OutputError {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        OutputError::DeviceLost
    } else {
        OutputError::Failed(e.code().0)
    }
}

/// The open error for a failed default endpoint lookup.
fn endpoint_error(e: windows::core::Error) -> WasapiError {
    if e.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
        WasapiError::NoEndpoint
    } else {
        WasapiError::os("GetDefaultAudioEndpoint", e)
    }
}

/// The client initialized at the shortest engine period, and that period.
///
/// # Safety
///
/// `format` is the endpoint's mix format.
unsafe fn init_shortest(
    probe: &IAudioClient,
    format: *const WAVEFORMATEX,
    periods: &mut Option<EnginePeriods>,
) -> Result<(IAudioClient, u32), Fallback> {
    fn refused(call: &'static str) -> impl Fn(windows::core::Error) -> Fallback {
        move |e| Fallback {
            call,
            code: e.code().0,
        }
    }
    let c3: IAudioClient3 = probe.cast().map_err(refused("IAudioClient3"))?;
    let mut p = EnginePeriods::default();
    // SAFETY: a valid format and out pointers.
    unsafe {
        c3.GetSharedModeEnginePeriod(
            format,
            &mut p.default,
            &mut p.fundamental,
            &mut p.min,
            &mut p.max,
        )
    }
    .map_err(refused("GetSharedModeEnginePeriod"))?;
    *periods = Some(p);
    // SAFETY: a valid format; the client is not yet initialized.
    unsafe {
        c3.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, p.min, format, None)
    }
    .map_err(refused("InitializeSharedAudioStream"))?;
    let mut current: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut period = 0;
    // SAFETY: out pointers; the returned format (null on failure) is freed
    // at once.
    let granted = unsafe { c3.GetCurrentSharedModeEnginePeriod(&mut current, &mut period) };
    drop(MixFormat(current));
    granted.map_err(refused("GetCurrentSharedModeEnginePeriod"))?;
    let client = c3.cast().map_err(refused("IAudioClient"))?;
    Ok((client, period))
}

/// The WASAPI shared-mode output on the default render endpoint.
///
/// It is used on the thread that opened it, which is the audio thread: the
/// COM objects and the MMCSS registration belong to that thread, so the
/// type is not `Send`. Wake times are read from `C`. Waiting and writing
/// allocate nothing.
pub struct WasapiShared<C> {
    // COM objects first: fields drop in order, and COM must outlive them.
    render: IAudioRenderClient,
    position: Option<(IAudioClock, u64)>,
    client: IAudioClient,
    event: Event,
    clock: C,
    grant: Grant,
    /// The engine's stereo block, spread into the device buffer.
    scratch: Vec<f32>,
    /// Queued frames as of the latest wake, plus those written since.
    padding: u32,
    woken: bool,
    started: bool,
    fault: Option<OutputError>,
    stats: WasapiStats,
    _mmcss: Option<Mmcss>,
    _com: Option<Com>,
}

impl<C: TimeSource> WasapiShared<C> {
    /// Opens the default render endpoint in shared mode, at the shortest
    /// engine period `IAudioClient3` offers, else at the default period.
    /// The stream starts at the second [`AudioOutput::wait`], after the
    /// first write has filled the queue.
    ///
    /// # Errors
    ///
    /// [`WasapiError::NoEndpoint`] without a default endpoint,
    /// [`WasapiError::UnsupportedFormat`] for a mix format other than
    /// 32-bit float stereo or wider, and the failing call otherwise.
    pub fn open(clock: C) -> Result<Self, WasapiError> {
        Self::open_inner(clock, None)
    }

    /// [`WasapiShared::open`], with `IAudioClient3` refused as `refuse`
    /// says when it is set (to exercise the fallback).
    fn open_inner(clock: C, refuse: Option<Fallback>) -> Result<Self, WasapiError> {
        let com = Com::init()?;
        let mmcss = Mmcss::pro_audio();
        let os = WasapiError::os;
        // SAFETY: COM calls on an initialized thread, on live interfaces;
        // the mix format outlives every call that reads it.
        let (client, grant, render, position) = unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| os("CoCreateInstance(MMDeviceEnumerator)", e))?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(endpoint_error)?;
            let probe: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| os("Activate(IAudioClient)", e))?;
            let format = MixFormat(probe.GetMixFormat().map_err(|e| os("GetMixFormat", e))?);
            let (rate, channels) = check_format(format.0)?;
            let mut engine_periods = None;
            let shortest = match refuse {
                Some(f) => Err(f),
                None => init_shortest(&probe, format.0, &mut engine_periods),
            };
            let (client, period, fallback) = match shortest {
                Ok((client, period)) => (client, period, None),
                Err(fallback) => {
                    // A fresh client: a refused one cannot be initialized
                    // again.
                    let client: IAudioClient = device
                        .Activate(CLSCTX_ALL, None)
                        .map_err(|e| os("Activate(IAudioClient)", e))?;
                    client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                            0,
                            0,
                            format.0,
                            None,
                        )
                        .map_err(|e| os("IAudioClient::Initialize", e))?;
                    let mut default = 0;
                    client
                        .GetDevicePeriod(Some(&mut default), None)
                        .map_err(|e| os("GetDevicePeriod", e))?;
                    (client, hns_to_frames(default, rate), Some(fallback))
                }
            };
            let buffer = client.GetBufferSize().map_err(|e| os("GetBufferSize", e))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| os("GetService(IAudioRenderClient)", e))?;
            let position = client
                .GetService::<IAudioClock>()
                .and_then(|c| {
                    let hz = c.GetFrequency()?;
                    Ok((c, hz))
                })
                .ok();
            let grant = Grant {
                rate,
                channels,
                period,
                buffer,
                queue_periods: queue_periods(buffer, period),
                engine_periods,
                fallback,
                mmcss: mmcss.is_some(),
            };
            (client, grant, render, position)
        };
        let event = Event::new()?;
        // SAFETY: a valid event handle, kept until the client is released.
        unsafe { client.SetEventHandle(event.0) }.map_err(|e| os("SetEventHandle", e))?;
        let queue = grant.queue_periods * grant.period;
        Ok(Self {
            render,
            position,
            client,
            event,
            clock,
            grant,
            scratch: vec![0.0; queue as usize * CHANNELS],
            padding: 0,
            woken: false,
            started: false,
            fault: None,
            stats: WasapiStats::default(),
            _mmcss: mmcss,
            _com: com,
        })
    }

    /// What the endpoint granted.
    #[must_use]
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// The counters so far.
    #[must_use]
    pub fn stats(&self) -> &WasapiStats {
        &self.stats
    }

    /// Stops the stream and releases the device.
    ///
    /// # Errors
    ///
    /// `IAudioClient::Stop`'s failure; the device is released anyway.
    pub fn close(mut self) -> Result<WasapiStats, WasapiError> {
        let stats = self.stats;
        if self.started {
            self.started = false;
            // SAFETY: stopping the stream this output started.
            unsafe { self.client.Stop() }.map_err(|e| WasapiError::os("IAudioClient::Stop", e))?;
        }
        Ok(stats)
    }

    /// The buffer state as of the latest wake and writes, at `at`.
    fn wake_at(&self, at: TimeUs) -> Wake {
        Wake {
            queued_from: self.stats.frames.saturating_sub(u64::from(self.padding)),
            at,
            padding: self.padding,
        }
    }

    fn wake_now(&self) -> Wake {
        self.wake_at(self.clock.now())
    }
}

impl<C: TimeSource> AudioOutput for WasapiShared<C> {
    fn rate(&self) -> u32 {
        self.grant.rate
    }

    fn period(&self) -> u32 {
        self.grant.period
    }

    /// The first wake returns at once, for the first write to fill the
    /// queue; the next starts the stream. A wait that times out or a
    /// failed call makes the next write fail.
    fn wait(&mut self) -> Wake {
        if self.fault.is_some() {
            return self.wake_now();
        }
        if !self.woken {
            self.woken = true;
            return self.wake_now();
        }
        if !self.started {
            // SAFETY: starting the initialized stream.
            if let Err(e) = unsafe { self.client.Start() } {
                self.fault = Some(output_error(&e));
                return self.wake_now();
            }
            self.started = true;
        }
        let signaled = self.event.wait(EVENT_TIMEOUT_MS);
        let at = self.clock.now();
        if !signaled {
            self.fault = Some(OutputError::Stalled);
            return self.wake_now();
        }
        self.stats.wakes += 1;
        // SAFETY: a query on the running stream.
        match unsafe { self.client.GetCurrentPadding() } {
            Ok(padding) => self.padding = padding,
            Err(e) => {
                self.fault = Some(output_error(&e));
                return self.wake_now();
            }
        }
        if let Some((clock, hz)) = &self.position {
            let mut pos = 0;
            // SAFETY: an out pointer; the QPC position is not asked for.
            match unsafe { clock.GetPosition(&mut pos, None) } {
                Ok(()) => self.stats.position = position_frames(pos, *hz, self.grant.rate),
                Err(_) => self.stats.position_failures += 1,
            }
        }
        self.wake_at(at)
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        if let Some(e) = self.fault {
            return Err(e);
        }
        let g = &self.grant;
        let frames = room(g.queue_periods * g.period, self.padding, g.period);
        if frames == 0 {
            f(&mut []);
            return Ok(());
        }
        let block = &mut self.scratch[..frames as usize * CHANNELS];
        // SAFETY: `frames` is at most the queue, which the buffer holds.
        let data = unsafe { self.render.GetBuffer(frames) }.map_err(|e| output_error(&e))?;
        f(block);
        let bytes = frames as usize * usize::from(g.channels) * size_of::<f32>();
        // SAFETY: GetBuffer returned room for `frames` frames of the mix
        // format, `channels` 32-bit samples each, valid until the release.
        let dst = unsafe { std::slice::from_raw_parts_mut(data, bytes) };
        spread(block, dst, usize::from(g.channels));
        // SAFETY: releases exactly the frames obtained, all written.
        unsafe { self.render.ReleaseBuffer(frames, 0) }.map_err(|e| output_error(&e))?;
        self.padding += frames;
        self.stats.frames += u64::from(frames);
        Ok(())
    }
}

impl<C> Drop for WasapiShared<C> {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: stopping the stream this output started.
            let _ = unsafe { self.client.Stop() };
        }
    }
}

impl<C> fmt::Debug for WasapiShared<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasapiShared")
            .field("grant", &self.grant)
            .field("stats", &self.stats)
            .field("started", &self.started)
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bms_model::TimeUs;
    use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE};
    use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
    use windows::core::GUID;

    use super::*;

    /// µs since the clock was made.
    struct Elapsed(Instant);

    impl TimeSource for Elapsed {
        fn now(&self) -> TimeUs {
            TimeUs(self.0.elapsed().as_micros() as i64)
        }
    }

    fn plain(tag: u16, bits: u16, channels: u16, rate: u32) -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: tag,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(channels) * u32::from(bits / 8),
            nBlockAlign: channels * bits / 8,
            wBitsPerSample: bits,
            cbSize: 0,
        }
    }

    fn extensible(sub: GUID, bits: u16, channels: u16, rate: u32) -> WAVEFORMATEXTENSIBLE {
        let mut f = WAVEFORMATEXTENSIBLE {
            Format: plain(0xFFFE, bits, channels, rate),
            SubFormat: sub,
            ..Default::default()
        };
        f.Format.cbSize = 22;
        f
    }

    // The float and PCM subformat GUIDs, from ksmedia.h.
    const FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);
    const PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);

    fn check(f: *const WAVEFORMATEX) -> Result<(u32, u16), (u16, u16, u16)> {
        // SAFETY: the callers pass a whole format on their stack.
        unsafe { check_format(f) }.map_err(|e| match e {
            WasapiError::UnsupportedFormat {
                tag,
                bits,
                channels,
            } => (tag, bits, channels),
            other => panic!("{other}"),
        })
    }

    fn check_ext(f: &WAVEFORMATEXTENSIBLE) -> Result<(u32, u16), (u16, u16, u16)> {
        check(std::ptr::from_ref(f).cast())
    }

    #[test]
    fn float_mix_formats_of_two_or_more_channels_are_accepted_at_any_rate() {
        assert_eq!(check(&plain(3, 32, 2, 44_100)), Ok((44_100, 2)));
        assert_eq!(
            check_ext(&extensible(FLOAT, 32, 6, 96_000)),
            Ok((96_000, 6))
        );
    }

    #[test]
    fn other_mix_formats_are_refused_with_what_they_are() {
        // Integer PCM, plain and extensible.
        assert_eq!(check(&plain(1, 16, 2, 48_000)), Err((1, 16, 2)));
        assert_eq!(
            check_ext(&extensible(PCM, 32, 2, 48_000)),
            Err((0xFFFE, 32, 2))
        );
        // Float, but 64-bit or mono.
        assert_eq!(check(&plain(3, 64, 2, 48_000)), Err((3, 64, 2)));
        assert_eq!(
            check_ext(&extensible(FLOAT, 32, 1, 48_000)),
            Err((0xFFFE, 32, 1))
        );
    }

    #[test]
    fn two_periods_are_queued_when_the_buffer_holds_them() {
        assert_eq!(queue_periods(1_056, 480), 2);
        assert_eq!(queue_periods(4_800, 480), 2);
        assert_eq!(queue_periods(959, 480), 1);
        assert_eq!(queue_periods(100, 480), 1);
    }

    #[test]
    fn writes_are_whole_periods_up_to_the_queue() {
        assert_eq!(room(960, 0, 480), 960);
        assert_eq!(room(960, 480, 480), 480);
        assert_eq!(room(960, 481, 480), 0);
        // A buffer fuller than the queue (a longer one) writes nothing.
        assert_eq!(room(960, 1_000, 480), 0);
        // A zero period does not divide by zero.
        assert_eq!(room(960, 0, 0), 960);
    }

    #[test]
    fn durations_convert_to_frames_by_floor() {
        // By hand: 10 ms at 48 kHz is 480 frames, at 44.1 kHz 441;
        // 10.1587 ms at 48 kHz is 487.6.
        assert_eq!(hns_to_frames(100_000, 48_000), 480);
        assert_eq!(hns_to_frames(100_000, 44_100), 441);
        assert_eq!(hns_to_frames(101_587, 48_000), 487);
        assert_eq!(hns_to_frames(-5, 48_000), 0);
    }

    #[test]
    fn device_positions_convert_to_frames_or_none() {
        // 1 s in 100 ns units at 48 kHz; a third of a second at 48 kHz.
        assert_eq!(
            position_frames(10_000_000, 10_000_000, 48_000),
            Some(48_000)
        );
        assert_eq!(position_frames(1, 3, 48_000), Some(16_000));
        assert_eq!(position_frames(1, 0, 48_000), None);
        assert_eq!(position_frames(u64::MAX, 1, 2), None);
    }

    fn floats(bytes: &[u8]) -> Vec<f32> {
        let (words, rest) = bytes.as_chunks::<4>();
        assert!(rest.is_empty());
        words.iter().map(|&b| f32::from_ne_bytes(b)).collect()
    }

    #[test]
    fn stereo_fills_the_first_two_channels_and_silences_the_rest() {
        let src = [0.25, -0.5, 0.75, -1.0];
        let mut two = [0xFF; 16];
        spread(&src, &mut two, 2);
        assert_eq!(floats(&two), [0.25, -0.5, 0.75, -1.0]);
        let mut four = [0xFF; 32];
        spread(&src, &mut four, 4);
        assert_eq!(floats(&four), [0.25, -0.5, 0.0, 0.0, 0.75, -1.0, 0.0, 0.0]);
    }

    #[test]
    fn errors_map_to_device_lost_and_no_endpoint() {
        let invalidated = windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED);
        assert_eq!(output_error(&invalidated), OutputError::DeviceLost);
        let fail = windows::core::Error::from_hresult(E_FAIL);
        assert_eq!(
            output_error(&fail),
            OutputError::Failed(0x8000_4005_u32 as i32)
        );
        // HRESULT_FROM_WIN32(ERROR_NOT_FOUND), by hand: 0x8007_0490.
        let not_found = windows::core::Error::from_hresult(HRESULT(0x8007_0490_u32 as i32));
        assert!(matches!(endpoint_error(not_found), WasapiError::NoEndpoint));
        assert!(matches!(
            endpoint_error(fail),
            WasapiError::Os {
                call: "GetDefaultAudioEndpoint",
                ..
            }
        ));
    }

    #[test]
    fn a_thread_in_a_single_threaded_apartment_keeps_it() {
        std::thread::spawn(|| {
            // SAFETY: per-thread initialization, balanced below.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                .ok()
                .unwrap();
            assert!(matches!(Com::init(), Ok(None)));
            // SAFETY: balances the initialization above.
            unsafe { CoUninitialize() };
        })
        .join()
        .unwrap();
    }

    /// Fills the queue, then takes `wakes` buffer events, writing silence.
    fn play_silence(out: &mut WasapiShared<Elapsed>, wakes: u32) {
        let queue = out.grant().queue_periods * out.grant().period;
        let first = out.wait();
        assert_eq!((first.queued_from, first.padding), (0, 0));
        let mut filled = 0;
        out.write(|b| {
            filled = b.len();
            b.fill(0.0);
        })
        .unwrap();
        assert_eq!(filled, queue as usize * CHANNELS);
        for _ in 0..wakes {
            out.wait();
            out.write(|b| b.fill(0.0)).unwrap();
        }
        assert_eq!(out.stats().wakes, u64::from(wakes));
    }

    #[test]
    #[ignore = "needs an audio output device"]
    fn a_refused_client3_falls_back_to_the_default_period_and_records_it() {
        let refused = Fallback {
            call: "IAudioClient3",
            code: E_NOINTERFACE.0,
        };
        let mut out = WasapiShared::open_inner(Elapsed(Instant::now()), Some(refused)).unwrap();
        let g = *out.grant();
        println!("fallback grant: {g:?}");
        assert_eq!(g.fallback, Some(refused));
        assert_eq!(g.engine_periods, None);
        assert!(g.period > 0);
        play_silence(&mut out, 20);
        out.close().unwrap();
    }

    #[test]
    #[ignore = "needs an audio output device"]
    fn a_missing_buffer_event_fails_the_next_write_within_the_timeout() {
        let mut out = WasapiShared::open(Elapsed(Instant::now())).unwrap();
        play_silence(&mut out, 3);
        // Wait on an event the device never signals; the device's own
        // event stays open until the end.
        let _signaled = std::mem::replace(&mut out.event, Event::new().unwrap());
        let start = Instant::now();
        out.wait();
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(u64::from(EVENT_TIMEOUT_MS)));
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        let mut called = false;
        assert_eq!(out.write(|_| called = true), Err(OutputError::Stalled));
        assert!(!called);
        // Later waits return at once.
        let start = Instant::now();
        out.wait();
        assert!(start.elapsed() < Duration::from_millis(u64::from(EVENT_TIMEOUT_MS)));
        out.close().unwrap();
    }
}
