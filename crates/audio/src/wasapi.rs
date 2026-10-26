//! Windows: WASAPI shared-mode output (design.md "Audio", "Path from Press
//! to Sound"; stage1.md D12).
//!
//! The stream runs on the default render endpoint in the endpoint's mix
//! format, event-driven. `IAudioClient3` initializes it at the shortest
//! engine period; if the endpoint refuses that, a fresh `IAudioClient`
//! initializes it at the default period, and the refusal is recorded in
//! [`Grant::fallback`]. The output keeps two periods queued at first (fewer
//! only if the buffer cannot hold them); between plays the queue can be
//! deepened up to [`MAX_QUEUE_PERIODS`] that the buffer holds
//! ([`AudioOutput::set_queue_periods`]). `IAudioClient3` chooses its own
//! buffer, often just over two periods; when its shortest period is also
//! the default one, nothing is gained from it, and `IAudioClient` is
//! initialized at that period with a buffer of [`MAX_QUEUE_PERIODS`]
//! instead ([`Grant::widened`]). The buffer's size adds no latency: only
//! the queued frames do. The thread that opens it joins the MMCSS "Pro
//! Audio" task.
//!
//! The mix clock is taken from the buffer events by the engine.
//! `IAudioClock::GetPosition` is read at each wake only as a diagnostic
//! ([`WasapiStats::position`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;

use bms_model::{TimeSource, TimeUs};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOT_FOUND, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT,
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
use crate::output::{AudioOutput, MAX_QUEUE_PERIODS, OutputError, Wake};

/// Periods kept queued on the device when it opens (design.md "Path from
/// Press to Sound": with one, audio dropped out repeatedly).
pub const QUEUE_PERIODS: u32 = 2;

/// How long a wait for a buffer event may take before the output reports
/// [`OutputError::Stalled`], ms: far beyond any shared-mode period.
pub const EVENT_TIMEOUT_MS: u32 = 500;

/// Why the output could not be opened or closed.
#[derive(Debug, thiserror::Error)]
pub enum WasapiError {
    /// There is no default render endpoint.
    #[error("there is no default audio output device")]
    NoEndpoint,
    /// The endpoint's mix format is not 32-bit float with at least two
    /// channels.
    #[error(
        "the audio device's shared format is not supported \
         (tag {tag:#06x}, {bits} bits, {channels} channels; \
         32-bit float with at least 2 channels is required)"
    )]
    UnsupportedFormat {
        /// `wFormatTag`.
        tag: u16,
        /// `wBitsPerSample`.
        bits: u16,
        /// `nChannels`.
        channels: u16,
    },
    /// An OS call failed.
    #[error("{call} failed")]
    Os {
        /// The call.
        call: &'static str,
        /// Its error.
        #[source]
        error: windows::core::Error,
    },
}

impl WasapiError {
    const fn os(call: &'static str, error: windows::core::Error) -> Self {
        Self::Os { call, error }
    }
}

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
    /// Periods kept queued when it opened.
    pub queue_periods: u32,
    /// The most periods the queue can hold: what the buffer holds, at most
    /// [`MAX_QUEUE_PERIODS`].
    pub max_queue_periods: u32,
    /// The periods `IAudioClient3` offered, when it answered.
    pub engine_periods: Option<EnginePeriods>,
    /// Why the default period was used instead of the shortest, if it was.
    pub fallback: Option<Fallback>,
    /// Whether `IAudioClient3`'s buffer held fewer than
    /// [`MAX_QUEUE_PERIODS`] at a shortest period equal to the default, so
    /// `IAudioClient` was initialized at that period with a wider buffer.
    pub widened: bool,
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
    /// Wakes whose padding was read more than half a period after the
    /// event returned. Their observation stays valid (the time is read
    /// after the padding) but is looser.
    pub slow_padding_reads: u64,
    /// The latest timed wake.
    pub last: Option<WakeTrace>,
}

/// One timed wake, for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeTrace {
    /// When the wait for the buffer event returned.
    pub woke: TimeUs,
    /// When the padding read returned: the wake's [`Wake::at`].
    pub read: TimeUs,
    /// The padding read, frames.
    pub padding: u32,
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

/// How a wait for a buffer event ended.
#[derive(Debug)]
enum Signal {
    /// The event was signaled.
    Event,
    /// The timeout passed first.
    Timeout,
    /// The wait itself failed, with the thread's last error.
    Failed(windows::core::Error),
}

fn wait_handle(handle: HANDLE, timeout_ms: u32) -> Signal {
    // SAFETY: the wait only reads the handle; an invalid one fails.
    let r = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if r == WAIT_OBJECT_0 {
        Signal::Event
    } else if r == WAIT_TIMEOUT {
        Signal::Timeout
    } else {
        Signal::Failed(windows::core::Error::from_thread())
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

/// Periods a buffer of `buffer` frames can keep queued, at least one and at
/// most [`MAX_QUEUE_PERIODS`].
fn buffer_periods(buffer: u32, period: u32) -> u32 {
    (buffer / period.max(1)).clamp(1, MAX_QUEUE_PERIODS)
}

/// Frames to write: whole periods up to `queue` frames queued.
fn room(queue: u32, padding: u32, period: u32) -> u32 {
    let period = period.max(1);
    queue.saturating_sub(padding) / period * period
}

/// The opening decision once `IAudioClient3` initialized a stream at
/// `period` frames with a `buffer`-frame buffer, having offered `offered`:
/// `Some(period)` to reopen through `IAudioClient` at that same period with
/// a wider buffer, `None` to keep the stream. It reopens when the buffer
/// holds fewer than [`MAX_QUEUE_PERIODS`] periods and the shortest period
/// is the default, so the shorter period `IAudioClient3` exists for is not
/// there to lose.
fn reopen_wider(offered: Option<EnginePeriods>, period: u32, buffer: u32) -> Option<u32> {
    offered
        .filter(|p| buffer_periods(buffer, period) < MAX_QUEUE_PERIODS && p.min >= p.default)
        .map(|_| period)
}

/// The buffer to ask `IAudioClient::Initialize` for, 100 ns units:
/// [`MAX_QUEUE_PERIODS`] default periods and half of one more, so rounding
/// to frames cannot leave it short.
fn wide_buffer_hns(default_period_hns: i64) -> i64 {
    default_period_hns * i64::from(MAX_QUEUE_PERIODS) + default_period_hns / 2
}

/// A fresh client on `device`, initialized at the default period with a
/// buffer of [`MAX_QUEUE_PERIODS`] periods, and that period in frames:
/// `exact` when the caller knows it in frames, else the default period's
/// duration rounded to the nearest frame.
///
/// # Safety
///
/// `format` is the endpoint's mix format.
unsafe fn init_default(
    device: &windows::Win32::Media::Audio::IMMDevice,
    format: *const WAVEFORMATEX,
    rate: u32,
    exact: Option<u32>,
) -> Result<(IAudioClient, u32), WasapiError> {
    let os = WasapiError::os;
    // SAFETY: COM calls on live interfaces; `format` per the contract.
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| os("Activate(IAudioClient)", e))?;
        let mut default = 0;
        client
            .GetDevicePeriod(Some(&mut default), None)
            .map_err(|e| os("GetDevicePeriod", e))?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                wide_buffer_hns(default),
                0,
                format,
                None,
            )
            .map_err(|e| os("IAudioClient::Initialize", e))?;
        Ok((
            client,
            exact.unwrap_or_else(|| hns_to_frames(default, rate)),
        ))
    }
}

/// A duration in 100 ns units as frames at `rate`, rounded to the nearest
/// frame: a period is reported in 100 ns units, which cut most periods
/// short of a whole frame (448 frames at 44.1 kHz are 101 587 units, 447.998
/// frames).
fn hns_to_frames(hns: i64, rate: u32) -> u32 {
    let scaled = i128::from(hns.max(0)) * i128::from(rate);
    u32::try_from((scaled + 5_000_000) / 10_000_000).unwrap_or(u32::MAX)
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

/// The calls the output makes on its stream: the endpoint makes them with
/// COM, and tests with a scripted device.
trait Device {
    /// Starts the stream.
    fn start(&mut self) -> windows::core::Result<()>;
    /// Waits for the next buffer event.
    fn wait(&mut self, timeout_ms: u32) -> Signal;
    /// Frames queued and not yet read by the engine.
    fn padding(&mut self) -> windows::core::Result<u32>;
    /// Gets a buffer of `frames` frames, lets `f` fill its bytes, and
    /// releases it. `f` is not called when getting the buffer fails.
    fn fill(&mut self, frames: u32, f: impl FnOnce(&mut [u8])) -> windows::core::Result<()>;
    /// The device's playback position in frames, if it reports one.
    fn position(&mut self) -> Option<windows::core::Result<u64>>;
    /// Stops the stream.
    fn stop(&mut self) -> windows::core::Result<()>;
}

/// The COM side of an initialized shared-mode stream.
struct Endpoint {
    render: IAudioRenderClient,
    /// The clock service and its frequency, for the diagnostic position.
    position: Option<(IAudioClock, u64)>,
    client: IAudioClient,
    event: Event,
    rate: u32,
    frame_bytes: usize,
}

impl Device for Endpoint {
    fn start(&mut self) -> windows::core::Result<()> {
        // SAFETY: starting the initialized stream.
        unsafe { self.client.Start() }
    }

    fn wait(&mut self, timeout_ms: u32) -> Signal {
        wait_handle(self.event.0, timeout_ms)
    }

    fn padding(&mut self) -> windows::core::Result<u32> {
        // SAFETY: a query on the initialized stream.
        unsafe { self.client.GetCurrentPadding() }
    }

    fn fill(&mut self, frames: u32, f: impl FnOnce(&mut [u8])) -> windows::core::Result<()> {
        // SAFETY: the caller asks for at most the queue, which the buffer
        // holds; GetBuffer returns room for `frames` frames of the mix
        // format, valid until the release, which releases exactly them.
        unsafe {
            let data = self.render.GetBuffer(frames)?;
            f(std::slice::from_raw_parts_mut(
                data,
                frames as usize * self.frame_bytes,
            ));
            self.render.ReleaseBuffer(frames, 0)
        }
    }

    fn position(&mut self) -> Option<windows::core::Result<u64>> {
        let (clock, hz) = self.position.as_ref()?;
        let mut pos = 0;
        // SAFETY: an out pointer; the QPC position is not asked for.
        let read = unsafe { clock.GetPosition(&mut pos, None) };
        Some(read.map(|()| position_frames(pos, *hz, self.rate).unwrap_or(u64::MAX)))
    }

    fn stop(&mut self) -> windows::core::Result<()> {
        // SAFETY: stopping the stream.
        unsafe { self.client.Stop() }
    }
}

/// Where the stream is in its startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Nothing asked for yet: the next wait primes.
    Unprimed,
    /// The priming wake was given: the stream starts at the next wait,
    /// once the queue holds the prefill.
    Primed,
    /// Started.
    Running,
}

/// The output's logic over a [`Device`]: startup order, the timing of
/// each wake, the queue, and faults. Allocates only in [`Stream::new`].
struct Stream<D, C> {
    dev: D,
    clock: C,
    rate: u32,
    period: u32,
    /// Frames to keep queued: whole periods.
    queue: u32,
    /// The most periods the queue may hold.
    max_queue_periods: u32,
    channels: usize,
    period_us: i64,
    /// The engine's stereo block, spread into the device buffer.
    scratch: Vec<f32>,
    /// Queued frames as of the latest wake, plus those written since.
    padding: u32,
    phase: Phase,
    fault: Option<OutputError>,
    stats: WasapiStats,
}

impl<D: Device, C: TimeSource> Stream<D, C> {
    /// A stream of `(rate, period, channels)` keeping `queue_periods`
    /// queued, which may later change up to `max_queue_periods`; its block
    /// is sized for the most.
    fn new(
        dev: D,
        clock: C,
        (rate, period, channels): (u32, u32, u16),
        queue_periods: u32,
        max_queue_periods: u32,
    ) -> Self {
        let max_queue_periods = max_queue_periods.max(1);
        Self {
            dev,
            clock,
            rate,
            period,
            queue: queue_periods.clamp(1, max_queue_periods) * period,
            max_queue_periods,
            channels: usize::from(channels),
            period_us: i64::from(period) * 1_000_000 / i64::from(rate.max(1)),
            scratch: vec![0.0; (max_queue_periods * period) as usize * CHANNELS],
            padding: 0,
            phase: Phase::Unprimed,
            fault: None,
            stats: WasapiStats::default(),
        }
    }

    /// A wake the engine takes no timing from.
    fn priming(&self) -> Wake {
        Wake {
            queued_from: self.stats.frames.saturating_sub(u64::from(self.padding)),
            at: self.clock.now(),
            padding: self.padding,
            priming: true,
        }
    }

    fn fail(&mut self, e: OutputError) -> Wake {
        self.fault = Some(e);
        self.priming()
    }
}

impl<D: Device, C> Stream<D, C> {
    /// Stops the stream if it was started.
    fn stop(&mut self) -> windows::core::Result<()> {
        if self.phase != Phase::Running {
            return Ok(());
        }
        self.phase = Phase::Primed;
        self.dev.stop()
    }
}

impl<D: Device, C: TimeSource> AudioOutput for Stream<D, C> {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn period(&self) -> u32 {
        self.period
    }

    /// The first wake is a priming one, for the first write to fill the
    /// queue; the next starts the stream, once the queue holds that fill,
    /// and waits for its first buffer event. A timed wake's time is read
    /// right after the padding, so the pass that reads its first queued
    /// frame comes after it, within a period. A failed or timed-out wait or
    /// a failed call makes the next write fail.
    fn wait(&mut self) -> Wake {
        if self.fault.is_some() {
            return self.priming();
        }
        match self.phase {
            Phase::Unprimed => {
                self.phase = Phase::Primed;
                return self.priming();
            }
            Phase::Primed if self.stats.frames == 0 => return self.priming(),
            Phase::Primed => {
                if let Err(e) = self.dev.start() {
                    return self.fail(output_error(&e));
                }
                self.phase = Phase::Running;
            }
            Phase::Running => {}
        }
        match self.dev.wait(EVENT_TIMEOUT_MS) {
            Signal::Event => {}
            Signal::Timeout => return self.fail(OutputError::Stalled),
            Signal::Failed(e) => return self.fail(output_error(&e)),
        }
        let woke = self.clock.now();
        self.stats.wakes += 1;
        match self.dev.padding() {
            Ok(padding) => self.padding = padding,
            Err(e) => return self.fail(output_error(&e)),
        }
        let read = self.clock.now();
        if read.0 - woke.0 > self.period_us / 2 {
            self.stats.slow_padding_reads += 1;
        }
        self.stats.last = Some(WakeTrace {
            woke,
            read,
            padding: self.padding,
        });
        match self.dev.position() {
            Some(Ok(frames)) => self.stats.position = Some(frames),
            Some(Err(_)) => self.stats.position_failures += 1,
            None => {}
        }
        Wake {
            queued_from: self.stats.frames.saturating_sub(u64::from(self.padding)),
            at: read,
            padding: self.padding,
            priming: false,
        }
    }

    fn queue_periods(&self) -> Option<u32> {
        Some(self.queue / self.period.max(1))
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        let periods = periods.clamp(1, self.max_queue_periods);
        self.queue = periods * self.period;
        Some(periods)
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        if let Some(e) = self.fault {
            return Err(e);
        }
        let frames = room(self.queue, self.padding, self.period);
        if frames == 0 {
            f(&mut []);
            return Ok(());
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "`room` is at most `queue`, which stays within the `max_queue_periods` periods `scratch` holds"
        )]
        let block = &mut self.scratch[..frames as usize * CHANNELS];
        let channels = self.channels;
        self.dev
            .fill(frames, |dst| {
                f(block);
                spread(block, dst, channels);
            })
            .map_err(|e| output_error(&e))?;
        self.padding += frames;
        self.stats.frames += u64::from(frames);
        Ok(())
    }
}

/// The WASAPI shared-mode output on the default render endpoint.
///
/// It is used on the thread that opened it, which is the audio thread: the
/// COM objects and the MMCSS registration belong to that thread, so the
/// type is not `Send`. Wake times are read from `C`. Waiting and writing
/// allocate nothing.
pub struct WasapiShared<C> {
    // The stream first: fields drop in order, and COM must outlive it.
    stream: Stream<Endpoint, C>,
    grant: Grant,
    _mmcss: Option<Mmcss>,
    _com: Option<Com>,
}

impl<C: TimeSource> WasapiShared<C> {
    /// Opens the default render endpoint in shared mode, at the shortest
    /// engine period `IAudioClient3` offers, else at the default period.
    /// The first [`AudioOutput::wait`] is a priming one; the stream starts
    /// at the second, after the first write has filled the queue.
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
            let (client, period, fallback, widened) = match shortest {
                Ok((client, period)) => {
                    let buffer = client.GetBufferSize().map_err(|e| os("GetBufferSize", e))?;
                    match reopen_wider(engine_periods, period, buffer) {
                        Some(exact) => {
                            // Released before the wider stream opens.
                            drop(client);
                            let (client, period) =
                                init_default(&device, format.0, rate, Some(exact))?;
                            (client, period, None, true)
                        }
                        None => (client, period, None, false),
                    }
                }
                Err(fallback) => {
                    // A fresh client: a refused one cannot be initialized
                    // again.
                    let (client, period) = init_default(&device, format.0, rate, None)?;
                    (client, period, Some(fallback), false)
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
                queue_periods: QUEUE_PERIODS.min(buffer_periods(buffer, period)),
                max_queue_periods: buffer_periods(buffer, period),
                engine_periods,
                fallback,
                widened,
                mmcss: mmcss.is_some(),
            };
            (client, grant, render, position)
        };
        let event = Event::new()?;
        // SAFETY: a valid event handle, kept until the client is released.
        unsafe { client.SetEventHandle(event.0) }.map_err(|e| os("SetEventHandle", e))?;
        let endpoint = Endpoint {
            render,
            position,
            client,
            event,
            rate: grant.rate,
            frame_bytes: usize::from(grant.channels) * size_of::<f32>(),
        };
        Ok(Self {
            stream: Stream::new(
                endpoint,
                clock,
                (grant.rate, grant.period, grant.channels),
                grant.queue_periods,
                grant.max_queue_periods,
            ),
            grant,
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
        &self.stream.stats
    }

    /// Stops the stream and releases the device.
    ///
    /// # Errors
    ///
    /// `IAudioClient::Stop`'s failure; the device is released anyway.
    pub fn close(mut self) -> Result<WasapiStats, WasapiError> {
        let stats = self.stream.stats;
        self.stream
            .stop()
            .map_err(|e| WasapiError::os("IAudioClient::Stop", e))?;
        Ok(stats)
    }
}

impl<C: TimeSource> AudioOutput for WasapiShared<C> {
    fn rate(&self) -> u32 {
        self.stream.rate()
    }

    fn period(&self) -> u32 {
        self.stream.period()
    }

    fn wait(&mut self) -> Wake {
        self.stream.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        self.stream.write(f)
    }

    fn queue_periods(&self) -> Option<u32> {
        self.stream.queue_periods()
    }

    fn set_queue_periods(&mut self, periods: u32) -> Option<u32> {
        self.stream.set_queue_periods(periods)
    }
}

impl<C> Drop for WasapiShared<C> {
    fn drop(&mut self) {
        let _ = self.stream.stop();
    }
}

impl<C> fmt::Debug for WasapiShared<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasapiShared")
            .field("grant", &self.grant)
            .field("stats", &self.stream.stats)
            .field("phase", &self.stream.phase)
            .field("fault", &self.stream.fault)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::{Duration, Instant};

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
    fn the_queue_holds_what_the_buffer_holds_up_to_three_periods() {
        assert_eq!(buffer_periods(1_056, 480), 2);
        assert_eq!(buffer_periods(1_440, 480), 3);
        assert_eq!(buffer_periods(4_800, 480), 3);
        assert_eq!(buffer_periods(959, 480), 1);
        assert_eq!(buffer_periods(100, 480), 1);
    }

    /// `IAudioClient3` is given up only for a buffer short of three periods
    /// at a shortest period that is the default one.
    #[test]
    fn a_short_buffer_is_widened_only_when_no_shorter_period_is_lost() {
        let fixed = EnginePeriods {
            default: 441,
            fundamental: 441,
            min: 441,
            max: 441,
        };
        let shorter = EnginePeriods { min: 147, ..fixed };
        // The development PC's interface: 970 frames hold two periods of
        // 441, reopened at exactly 441.
        assert_eq!(reopen_wider(Some(fixed), 441, 970), Some(441));
        assert_eq!(reopen_wider(Some(fixed), 441, 1_323), None);
        assert_eq!(reopen_wider(Some(shorter), 147, 400), None);
        // A fractional period is kept as `IAudioClient3` gave it.
        let fractional = EnginePeriods {
            default: 448,
            fundamental: 448,
            min: 448,
            max: 448,
        };
        assert_eq!(reopen_wider(Some(fractional), 448, 970), Some(448));
        assert_eq!(reopen_wider(None, 441, 970), None);
        // 10 ms: three periods and a half, 35 ms.
        assert_eq!(wide_buffer_hns(100_000), 350_000);
        assert!(hns_to_frames(wide_buffer_hns(100_000), 44_100) >= 3 * 441);
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
    fn durations_convert_to_the_nearest_frame() {
        // By hand: 10 ms at 48 kHz is 480 frames, at 44.1 kHz 441;
        // 10.1587 ms is 487.6 frames at 48 kHz and 447.998 at 44.1 kHz,
        // which a floor would cut to 447.
        assert_eq!(hns_to_frames(100_000, 48_000), 480);
        assert_eq!(hns_to_frames(100_000, 44_100), 441);
        assert_eq!(hns_to_frames(101_587, 48_000), 488);
        assert_eq!(hns_to_frames(101_587, 44_100), 448);
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
    fn an_os_failure_is_the_source_and_its_text_is_printed_once() {
        use std::error::Error as _;
        let error = windows::core::Error::from_hresult(E_FAIL);
        let e = WasapiError::os("IAudioClient::Start", error.clone());
        assert_eq!(e.to_string(), "IAudioClient::Start failed");
        let source = e.source().expect("the OS error is the source");
        assert_eq!(
            source
                .downcast_ref::<windows::core::Error>()
                .map(windows::core::Error::code),
            Some(error.code())
        );
        assert!(!e.to_string().contains(&source.to_string()));
        assert!(WasapiError::NoEndpoint.source().is_none());
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
        assert_eq!(
            (first.queued_from, first.padding, first.priming),
            (0, 0, true)
        );
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
        let _signaled = std::mem::replace(&mut out.stream.dev.event, Event::new().unwrap());
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        Start,
        Wait,
        Padding,
        Fill(u32),
    }

    /// Reads the fake device's virtual time.
    struct FakeClock(Arc<AtomicI64>);

    impl TimeSource for FakeClock {
        fn now(&self) -> TimeUs {
            TimeUs(self.0.load(Ordering::Relaxed))
        }
    }

    /// A shared-mode device on a virtual timeline (µs). Once started at
    /// time `s`, pass `k` takes a period from the buffer at
    /// `s + first_pass_us + k × period_us` and signals the event. A wait
    /// returns at the event plus that wake's lateness. Every call is
    /// logged.
    struct Fake {
        now: Arc<AtomicI64>,
        period: u64,
        period_us: i64,
        queue: u64,
        first_pass_us: i64,
        started: Option<i64>,
        passes: u64,
        released: u64,
        consumed: u64,
        /// Passes that found less than a period.
        short_passes: u64,
        signaled: bool,
        /// Each wake's lateness after its event, µs, by wake (0 past the end).
        late: Vec<i64>,
        /// A wake whose padding read takes this long, µs.
        slow_read: Option<(u64, i64)>,
        /// How long the first fill takes, µs.
        prefill_us: i64,
        /// What every wait returns instead, if set.
        forced: Option<fn() -> Signal>,
        wakes: u64,
        calls: Vec<Call>,
        buf: Vec<u8>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                now: Arc::new(AtomicI64::new(0)),
                period: 480,
                period_us: 10_000,
                queue: 960,
                first_pass_us: 1_000,
                started: None,
                passes: 0,
                released: 0,
                consumed: 0,
                short_passes: 0,
                signaled: false,
                late: Vec::new(),
                slow_read: None,
                prefill_us: 0,
                forced: None,
                wakes: 0,
                calls: Vec::new(),
                buf: vec![0; 960 * 8],
            }
        }

        fn now(&self) -> i64 {
            self.now.load(Ordering::Relaxed)
        }

        fn pass_at(&self, k: u64) -> Option<i64> {
            Some(self.started? + self.first_pass_us + k as i64 * self.period_us)
        }

        /// Moves time to `t`, running the passes due by then.
        fn advance_to(&mut self, t: i64) {
            while let Some(at) = self.pass_at(self.passes)
                && at <= t
            {
                let take = self.period.min(self.released - self.consumed);
                self.short_passes += u64::from(take < self.period);
                self.consumed += take;
                self.passes += 1;
                self.signaled = true;
            }
            self.now.store(t, Ordering::Relaxed);
        }

        /// The oracle: when the pass that reads `frame` runs (valid while
        /// no pass came up short).
        fn read_at(&self, frame: u64) -> i64 {
            self.pass_at(frame / self.period).unwrap()
        }
    }

    impl Device for Fake {
        fn start(&mut self) -> windows::core::Result<()> {
            self.calls.push(Call::Start);
            self.started = Some(self.now());
            Ok(())
        }

        fn wait(&mut self, timeout_ms: u32) -> Signal {
            self.calls.push(Call::Wait);
            if let Some(forced) = self.forced {
                return forced();
            }
            if !self.signaled {
                match self.pass_at(self.passes) {
                    Some(at) => self.advance_to(at),
                    None => {
                        self.advance_to(self.now() + i64::from(timeout_ms) * 1_000);
                        return Signal::Timeout;
                    }
                }
            }
            self.signaled = false;
            let late = self.late.get(self.wakes as usize).copied().unwrap_or(0);
            self.wakes += 1;
            self.advance_to(self.now() + late);
            Signal::Event
        }

        fn padding(&mut self) -> windows::core::Result<u32> {
            self.calls.push(Call::Padding);
            if let Some((wake, us)) = self.slow_read
                && wake + 1 == self.wakes
            {
                self.advance_to(self.now() + us);
            }
            Ok((self.released - self.consumed) as u32)
        }

        fn fill(&mut self, frames: u32, f: impl FnOnce(&mut [u8])) -> windows::core::Result<()> {
            self.calls.push(Call::Fill(frames));
            if self.released - self.consumed + u64::from(frames) > self.queue {
                return Err(windows::core::Error::from_hresult(HRESULT(
                    0x8889_0006_u32 as i32,
                )));
            }
            f(&mut self.buf[..frames as usize * 8]);
            let first = self.released == 0;
            self.released += u64::from(frames);
            if first {
                self.advance_to(self.now() + self.prefill_us);
            }
            Ok(())
        }

        fn position(&mut self) -> Option<windows::core::Result<u64>> {
            None
        }

        fn stop(&mut self) -> windows::core::Result<()> {
            Ok(())
        }
    }

    fn stream(fake: Fake) -> Stream<Fake, FakeClock> {
        let clock = FakeClock(Arc::clone(&fake.now));
        Stream::new(fake, clock, (48_000, 480, 2), 2, 2)
    }

    #[test]
    fn the_stream_starts_after_the_prefill_and_times_wakes_after_the_padding() {
        let mut s = stream(Fake::new());
        let w = s.wait();
        assert!(w.priming);
        assert!(s.dev.calls.is_empty());
        // A wait before anything was written primes again, unstarted.
        assert!(s.wait().priming);
        assert!(s.dev.calls.is_empty());
        s.write(|b| b.fill(0.0)).unwrap();
        let w = s.wait();
        assert!(!w.priming);
        assert_eq!(
            s.dev.calls,
            [Call::Fill(960), Call::Start, Call::Wait, Call::Padding]
        );
    }

    /// A queue deepened between plays is filled at the next write, never past
    /// what the buffer holds; a buffer of two periods keeps two.
    #[test]
    fn a_deeper_queue_is_filled_at_the_next_write_up_to_the_buffer() {
        let mut fake = Fake::new();
        fake.queue = 1_440;
        fake.buf = vec![0; 1_440 * 8];
        let clock = FakeClock(Arc::clone(&fake.now));
        let mut s = Stream::new(fake, clock, (48_000, 480, 2), 2, 3);
        assert_eq!(s.queue_periods(), Some(2));
        let mut step = || {
            s.wait();
            s.write(|b| b.fill(0.0)).unwrap();
        };
        // Prime (960), then a timed wake after one pass (480 left: 480 more).
        step();
        step();
        assert_eq!(s.set_queue_periods(3), Some(3));
        assert_eq!(s.queue_periods(), Some(3));
        // After the next pass 480 are left: 960 more fill three periods,
        // then each pass is refilled by one.
        let mut step = || {
            s.wait();
            s.write(|b| b.fill(0.0)).unwrap();
        };
        step();
        step();
        let fills: Vec<Call> = s
            .dev
            .calls
            .iter()
            .copied()
            .filter(|c| matches!(c, Call::Fill(_)))
            .collect();
        assert_eq!(
            fills,
            [
                Call::Fill(960),
                Call::Fill(480),
                Call::Fill(960),
                Call::Fill(480)
            ]
        );
        assert_eq!(s.dev.short_passes, 0);
        // Asked for more than the buffer holds, or for none, it keeps what
        // fits.
        assert_eq!(s.set_queue_periods(5), Some(3));
        assert_eq!(s.set_queue_periods(0), Some(1));
        let mut two = stream(Fake::new());
        assert_eq!(two.set_queue_periods(3), Some(2));
        assert_eq!(two.queue_periods(), Some(2));
    }

    #[test]
    fn a_timed_wake_precedes_the_pass_reading_its_first_queued_frame_by_at_most_a_period() {
        let mut fake = Fake::new();
        fake.late = vec![0, 3_000, 700, 9_000, 50, 4_000, 0, 2_500, 9_900, 10];
        // At wake 5 the padding read ends 4 + 7 ms after the event: the
        // next pass runs in between.
        fake.slow_read = Some((5, 7_000));
        let mut s = stream(fake);
        assert!(s.wait().priming);
        s.write(|b| b.fill(0.25)).unwrap();
        let mut timed = 0;
        for _ in 0..40 {
            let w = s.wait();
            assert!(!w.priming);
            let pass = s.dev.read_at(w.queued_from);
            assert!(
                w.at.0 < pass && pass <= w.at.0 + 10_000,
                "{w:?} read at {pass}"
            );
            assert_eq!(w.padding, s.stats.last.unwrap().padding);
            timed += 1;
            s.write(|b| b.fill(0.25)).unwrap();
        }
        assert_eq!(timed, 40);
        assert_eq!(s.dev.short_passes, 0);
        assert_eq!(s.stats.slow_padding_reads, 1);
    }

    #[test]
    fn a_delayed_prefill_does_not_shift_the_mix_clock() {
        // The prefill takes 12 ms; the first pass runs 1 ms after Start.
        let mut fake = Fake::new();
        fake.prefill_us = 12_000;
        let mut s = stream(fake);
        let bank = Arc::new(crate::SoundBank::from_sounds(
            48_000,
            vec![crate::Sound::mono(vec![100; 16])],
        ));
        let mut e = crate::AudioEngine::new(bank, &[], 48_000, 480, 4, 4).unwrap();
        let (_tx, mut rx) = rtrb::RingBuffer::<crate::SoundCmd>::new(4);
        for _ in 0..30 {
            e.step(&mut s, &mut rx).unwrap();
        }
        assert_eq!(s.dev.started, Some(12_000));
        assert_eq!(s.dev.short_passes, 0);
        for frame in [0, 480 * 7, 480 * 29] {
            // By hand: pass k runs at 12 000 + 1 000 + 10 000 k µs.
            let pass = 13_000 + (frame / 480) as i64 * 10_000;
            assert_eq!(s.dev.read_at(frame), pass);
            assert_eq!(e.clock().time_at(frame), Some(TimeUs(pass)), "{frame}");
        }
    }

    #[test]
    fn a_failed_wait_keeps_the_os_error_and_a_timeout_stalls() {
        let mut fake = Fake::new();
        fake.forced = Some(|| {
            Signal::Failed(windows::core::Error::from_hresult(HRESULT(
                0x8007_0006_u32 as i32,
            )))
        });
        let mut s = stream(fake);
        s.wait();
        s.write(|b| b.fill(0.0)).unwrap();
        assert!(s.wait().priming);
        let mut called = false;
        assert_eq!(
            s.write(|_| called = true),
            Err(OutputError::Failed(0x8007_0006_u32 as i32))
        );
        assert!(!called);

        let mut fake = Fake::new();
        fake.forced = Some(|| Signal::Timeout);
        let mut s = stream(fake);
        s.wait();
        s.write(|b| b.fill(0.0)).unwrap();
        s.wait();
        assert_eq!(s.write(|_| {}), Err(OutputError::Stalled));
    }

    #[test]
    fn waiting_on_a_bad_handle_fails_with_the_thread_error() {
        // By hand: HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE) is 0x8007_0006.
        match wait_handle(HANDLE::default(), 0) {
            Signal::Failed(e) => assert_eq!(e.code(), HRESULT(0x8007_0006_u32 as i32)),
            other => panic!("{other:?}"),
        }
        let event = Event::new().unwrap();
        assert!(matches!(wait_handle(event.0, 1), Signal::Timeout));
    }
}
