//! The sound bank: every sound a chart references, decoded before play into i16
//! at the device rate (design.md "Audio").
//!
//! A pool of at most `min(available cores, BankLimits::max_decode_threads)`
//! workers takes one distinct file at a time and finishes it before taking
//! the next: read its bytes (at most [`BankLimits::max_file_bytes`]), hash
//! them, and either drop them at once as a duplicate of contents already
//! taken, or decode them and drop them. So at most one raw file per worker
//! is held at any time.
//!
//! A sound is checked against the caps before decoding, from the frame count
//! its container declares, and while decoding, from a running count. While
//! a sound decodes, the PCM it has allocated is charged to
//! [`BankLimits::max_bank_bytes`] before each allocation: its source buffer
//! (allocated exactly when the length is declared), both buffers while one
//! grows, and the source and converted buffers together while converting.
//! On success the charge becomes the stored sound's allocation; on failure
//! or panic it, and the sound's declared share, are returned.
//!
//! A bank is loaded at a [`Speed`] (design.md "Practice Speed Changes"). Its
//! rate stays the device rate: a sound is converted in up to two `rubato`
//! `Fft` [`Stage`]s, source → device and then the speed stage at the reduced
//! rates `k/g : 20/g`, so pitch follows speed. Before a stage is built, its
//! input, its output and an upper bound for its workspace
//! ([`Stage::workspace_bound`]) are charged to the bank cap, so a sound whose
//! workspace does not fit is refused before the resampler allocates.
//!
//! The caps bound the raw file bytes in flight (workers × the file cap) and
//! the decoded PCM (the bank cap). They do not bound the decoder's own
//! workspace (for example the codebooks a Vorbis header sets up), which only
//! the decoder could limit; design.md "Threat Model" records that gap.
//!
//! A sound over a cap refuses the whole bank. A file that cannot be decoded,
//! declares a sample rate outside the range, or makes the decoder panic is
//! left silent with a warning, like a sound file the chart names but the
//! folder lacks.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, ErrorKind, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub use bms_model::Speed;
use bms_model::{Chart, NoteKind, SoundId};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use sha2::{Digest, Sha256};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Lowest sample rate accepted, for sources and the device.
pub const MIN_RATE: u32 = 8_000;
/// Highest sample rate accepted, for sources and the device.
pub const MAX_RATE: u32 = 384_000;

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Frames `rubato`'s `Fft` is asked to take per call (`FixedSync::Input`).
const CHUNK: usize = 1024;

/// The crate versions [`Stage::workspace_bound`] is derived for. A change to
/// any of them fails a test until the bound is derived again from the new
/// sources.
#[cfg(test)]
const BOUND_DERIVED_FOR: [(&str, &str); 3] = [
    ("rubato", "5.0.1"),
    ("realfft", "3.5.0"),
    ("rustfft", "6.4.1"),
];

/// Bytes per complex point of one real transform of length `2n`, bounding
/// its plan, the scratch `rubato` keeps for it, and the planners' peak while
/// planning. The worst plan is Bluestein's for a prime `n`: an inner
/// transform of length `m ≤ 4n − 4` (a power of two or three quarters of
/// one; rustfft `plan.rs` `design_prime`, `avx_planner.rs`
/// `plan_bluesteins`), whose multiplier (`m`) and twiddles (`n`) stay, whose
/// inner plan keeps at most `2m` twiddles, whose construction runs the inner
/// transform with `m` of scratch (`bluesteins_algorithm.rs` `new`), and whose
/// out-of-place scratch, kept by `rubato`, is `m` plus the inner scratch
/// (`≤ m`); realfft adds `n/2` twiddles (`RealToComplexEven::new`). That is
/// at most `6m + 1.5n ≤ 25.5n` complex values of 8 bytes; composite lengths
/// split into smaller such plans plus at most `2n` twiddles. 26 × 8 bytes.
const FFT_BYTES_PER_POINT: u64 = 208;
/// Bytes per transform for what does not grow with its length: the plan
/// nodes, butterflies and the planners' caches.
const FFT_FIXED_BYTES: u64 = 64 * 1024;

/// One `rubato` `Fft` conversion of interleaved i16 from one rate to
/// another, with an upper bound for the workspace building and running it
/// allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage {
    from: u32,
    to: u32,
    channels: usize,
    workspace: u64,
}

impl Stage {
    /// A stage from rate `from` to rate `to` for `channels` channels (1 or
    /// 2).
    #[must_use]
    pub fn new(from: u32, to: u32, channels: usize) -> Self {
        Self {
            from,
            to,
            channels,
            workspace: workspace_bound(from, to, channels),
        }
    }

    /// The rates, input to output.
    #[must_use]
    pub fn rates(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    /// The channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// An upper bound, in bytes, for everything [`Self::convert`] allocates
    /// besides its output: the resampler with its retained sample, spectrum,
    /// overlap and per-channel scratch buffers, its FFT plans and FFT
    /// scratch, its constructor's temporaries, and the chunk buffers that
    /// feed it. It is computed from the rates and the channel count alone,
    /// before anything is allocated.
    #[must_use]
    pub fn workspace_bound(&self) -> u64 {
        self.workspace
    }

    /// Converts interleaved `samples` chunk by chunk into exactly
    /// `out_frames` frames, with the resampler's delay removed and zeros
    /// past the end of the input. Only one chunk of either side is held as
    /// `f32` at a time. The caller charges [`Self::workspace_bound`] first.
    ///
    /// # Errors
    ///
    /// The resampler's construction or processing error.
    pub fn convert(&self, samples: &[i16], out_frames: u64) -> Result<Vec<i16>, String> {
        resample(samples, self.channels, self.from, self.to, out_frames)
    }
}

/// [`Stage::workspace_bound`] for `rubato` 5.0.1's `Fft::<f32>::new(from, to,
/// CHUNK, ch, FixedSync::Input)` (`synchro.rs`), counting every buffer as
/// live at once. f32 is 4 bytes, a complex value 8, a `Vec` header 24.
fn workspace_bound(from: u32, to: u32, ch: usize) -> u64 {
    const VEC: u64 = 24;
    let (from, to, ch) = (u64::from(from.max(1)), u64::from(to.max(1)), ch as u64);
    let chunk = CHUNK as u64;
    // synchro.rs:222 and 300-321: 4 sub-chunks of 256 frames, rounded
    // up to whole blocks of `from / g` input frames.
    let g = gcd(from, to);
    let blocks = (chunk / 4).div_ceil(from / g).max(1);
    let (n_in, n_out) = (blocks * from / g, blocks * to / g);
    // synchro.rs:482-488: the most output one call can give.
    let out_max = chunk.div_ceil(n_in) * n_out;
    // FftResampler::new, synchro.rs:97-140. make_sincs (sinc.rs:17-49):
    // the values, the window (budgeted with an f64 copy) and the sincs;
    // then filter_t (2n_in f32), filter_f and input_f (n_in + 1 complex),
    // input_buf (2n_in f32), output_f (n_out + 1 complex), output_buf
    // (2n_out f32).
    let sincs = 4 * n_in + (4 + 8) * n_in + 4 * n_in + 2 * VEC;
    let filter = 8 * n_in + 2 * 8 * (n_in + 1) + 8 * n_in;
    let output = 8 * (n_out + 1) + 8 * n_out;
    // The forward plan of 2n_in and the inverse of 2n_out with their kept
    // scratch (synchro.rs:119-124, realfft lib.rs:376-394 and 623-641).
    let ffts = (FFT_BYTES_PER_POINT * n_in + FFT_FIXED_BYTES)
        + (FFT_BYTES_PER_POINT * n_out + FFT_FIXED_BYTES);
    // Fft::new_custom, synchro.rs:329-355: per channel the overlap (n_out),
    // input scratch (CHUNK + n_in) and output scratch (out_max + n_out), and
    // the channel mask.
    let per_channel = 4 * n_out + 4 * (chunk + n_in) + 4 * (out_max + n_out) + 3 * VEC + 1;
    // `resample`'s chunk buffers: CHUNK input and out_max output frames.
    let chunks = 4 * ch * (chunk + out_max);
    sincs + filter + output + ffts + ch * per_channel + 3 * VEC + chunks
}

/// How a sound is converted: source → device when the rates differ, then
/// the speed stage when the speed is not 1×.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conversion {
    /// Source rate → device rate.
    pub device: Option<Stage>,
    /// The speed stage at [`Speed::ratio`].
    pub speed: Option<Stage>,
}

/// The stages for a sound at `src_rate` with `channels` channels, stored at
/// device rate `rate` and `speed`. Nothing is allocated.
#[must_use]
pub fn plan_conversion(src_rate: u32, rate: u32, speed: Speed, channels: usize) -> Conversion {
    let (from, to) = speed.ratio();
    Conversion {
        device: (src_rate != rate).then(|| Stage::new(src_rate, rate, channels)),
        speed: (speed != Speed::NORMAL).then(|| Stage::new(from, to, channels)),
    }
}

/// Caps on decoding (design.md "Audio"). They bound work on untrusted files and
/// sit far beyond real charts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BankLimits {
    /// Longest sound, in seconds of frames at the device rate.
    pub max_sound_seconds: u32,
    /// Largest bank, in bytes of i16 PCM allocated, counting sounds still
    /// being decoded at what they hold: the source buffer, plus the
    /// converted buffer while converting.
    pub max_bank_bytes: u64,
    /// Largest sound file, in bytes.
    pub max_file_bytes: u64,
    /// Most decode threads; fewer when the machine has fewer cores.
    pub max_decode_threads: usize,
}

impl Default for BankLimits {
    /// 10 minutes per sound, 512 MiB per bank, 128 MiB per file, 4 decode
    /// threads.
    fn default() -> Self {
        Self {
            max_sound_seconds: 600,
            max_bank_bytes: 512 * 1024 * 1024,
            max_file_bytes: 128 * 1024 * 1024,
            max_decode_threads: 4,
        }
    }
}

/// Which cap a sound exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// One sound's length ([`BankLimits::max_sound_seconds`]), in frames.
    SoundLength,
    /// The whole bank's decoded size ([`BankLimits::max_bank_bytes`]), in
    /// bytes.
    BankMemory,
    /// One file's size ([`BankLimits::max_file_bytes`]), in bytes.
    FileSize,
}

impl Cap {
    /// What the cap limits, with its unit, as a [`BankError`] names it.
    #[must_use]
    pub const fn what(self) -> &'static str {
        match self {
            Self::SoundLength => "sound length in frames",
            Self::BankMemory => "decoded sound memory in bytes",
            Self::FileSize => "sound file size in bytes",
        }
    }
}

/// How the excess was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// Before decoding: from the frame counts the containers declare, or
    /// from the file size while reading.
    Declared,
    /// From the running count while decoding.
    WhileDecoding,
}

impl Found {
    /// When the excess was found, as a [`BankError`] says it.
    #[must_use]
    pub const fn how(self) -> &'static str {
        match self {
            Self::Declared => "before decoding",
            Self::WhileDecoding => "while decoding",
        }
    }
}

/// Why a bank was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BankError {
    /// The device rate is outside [`MIN_RATE`]..=[`MAX_RATE`].
    #[error("the device rate {rate} Hz is outside {MIN_RATE}-{MAX_RATE} Hz")]
    RateOutOfRange {
        /// The rate.
        rate: u32,
    },
    /// A cap was exceeded at `sound`: `amount` (frames or bytes, by `cap`)
    /// is over `limit`.
    #[error(
        "sound {:02} exceeds the {} cap {}: {amount} > {limit}",
        .sound.0,
        .cap.what(),
        .found.how()
    )]
    OverCap {
        /// The cap.
        cap: Cap,
        /// How it was found.
        found: Found,
        /// The sound whose length or size crossed the cap.
        sound: SoundId,
        /// Frames of the sound, bytes of its file, or bytes of the bank
        /// including it.
        amount: u64,
        /// The cap in the same unit.
        limit: u64,
    },
}

/// A referenced sound left silent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BankWarning {
    /// The file could not be read or decoded.
    Undecodable {
        /// The sound.
        sound: SoundId,
        /// What went wrong.
        reason: String,
    },
}

/// How a bank was loaded (indicators, not checked).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    /// Threads in the decode pool.
    pub decode_threads: usize,
    /// Distinct files read.
    pub files: usize,
    /// Most raw file bytes held at once.
    pub peak_raw_bytes: u64,
    /// Wall time of the load.
    pub elapsed: Duration,
}

/// One decoded sound: i16 at the device rate, mono or interleaved stereo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sound {
    stereo: bool,
    samples: Vec<i16>,
}

impl Sound {
    /// A mono sound.
    #[must_use]
    pub fn mono(samples: Vec<i16>) -> Self {
        Self {
            stereo: false,
            samples,
        }
    }

    /// A stereo sound from interleaved left, right samples. A trailing odd
    /// sample is not part of any frame.
    #[must_use]
    pub fn stereo(samples: Vec<i16>) -> Self {
        Self {
            stereo: true,
            samples,
        }
    }

    /// Whether the sound has two channels.
    #[must_use]
    pub fn is_stereo(&self) -> bool {
        self.stereo
    }

    /// The samples, interleaved when stereo.
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    /// Length in frames.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn frames(&self) -> u64 {
        (self.samples.len() / self.channels()) as u64
    }

    /// Decoded size in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.samples.len() as u64 * 2
    }

    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }

    /// Frame `i` as left, right in `[-1, 1)`.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    #[expect(
        clippy::indexing_slicing,
        reason = "the mixer reads only frames below `frames()`, so every sample index is in bounds"
    )]
    pub(crate) fn frame(&self, i: usize) -> (f32, f32) {
        let to_f32 = |s: i16| f32::from(s) / 32_768.0;
        if self.stereo {
            (to_f32(self.samples[2 * i]), to_f32(self.samples[2 * i + 1]))
        } else {
            let x = to_f32(self.samples[i]);
            (x, x)
        }
    }
}

/// The decoded sounds of a chart, indexed by [`SoundId`].
#[derive(Clone, Debug, Default)]
pub struct SoundBank {
    rate: u32,
    speed: Speed,
    /// Per [`SoundId`], the index into `sounds`.
    slots: Vec<Option<u32>>,
    sounds: Vec<Sound>,
    warnings: Vec<BankWarning>,
    stats: LoadStats,
    /// What the referenced sounds' files held ([`SoundBank::sound_digest`]).
    sound_digest: Option<[u8; 32]>,
}

impl SoundBank {
    /// A bank holding `sounds` at `rate`, sound `i` as `SoundId(i)`.
    #[must_use]
    pub fn from_sounds(rate: u32, sounds: Vec<Sound>) -> Self {
        Self {
            rate,
            slots: (0..sounds.len() as u32).map(Some).collect(),
            sounds,
            ..Self::default()
        }
    }

    /// The device rate the sounds are stored at.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The speed the sounds were converted for.
    #[must_use]
    pub fn speed(&self) -> Speed {
        self.speed
    }

    /// The sound for `id`, if one was decoded.
    #[must_use]
    pub fn get(&self, id: SoundId) -> Option<&Sound> {
        self.stored(self.slot(id)?)
    }

    /// The stored index for `id`; ids whose files have identical contents
    /// share one.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn slot(&self, id: SoundId) -> Option<u32> {
        self.slots.get(usize::from(id.0)).copied().flatten()
    }

    /// Stored sound `index`.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn stored(&self, index: u32) -> Option<&Sound> {
        self.sounds.get(index as usize)
    }

    /// Number of distinct sounds stored.
    #[must_use]
    pub fn stored_count(&self) -> usize {
        self.sounds.len()
    }

    /// Decoded bytes held.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.sounds.iter().map(Sound::bytes).sum()
    }

    /// Referenced sounds left silent.
    #[must_use]
    pub fn warnings(&self) -> &[BankWarning] {
        &self.warnings
    }

    /// How the bank was loaded.
    #[must_use]
    pub fn stats(&self) -> LoadStats {
        self.stats
    }

    /// The sound-content digest of a loaded bank: SHA-256 over the domain
    /// `tapestry sounds v1` and, for each referenced sound id in ascending
    /// order, the id (`u16` little-endian) and a tag: 0 when the sound has
    /// no file, 1 when its file could not be read, 2 followed by the
    /// SHA-256 of the file's bytes when it was read, decodable or not. It
    /// changes when any referenced file's contents change, at no cost: the
    /// files are hashed for deduplication anyway. Part of the loudness
    /// cache key (design.md "Algorithms"); `None` for a bank made in code
    /// ([`from_sounds`](Self::from_sounds)), which is never cached.
    #[must_use]
    pub fn sound_digest(&self) -> Option<[u8; 32]> {
        self.sound_digest
    }
}

/// The domain of [`SoundBank::sound_digest`].
const SOUND_DIGEST_DOMAIN: &[u8] = b"tapestry sounds v1";

/// [`SoundBank::sound_digest`] of `chart`'s referenced sounds, given the
/// hash of each distinct path's bytes (`None` when it was not read).
fn sound_digest(
    chart: &Chart,
    path_index: &HashMap<&Path, usize>,
    hashes: &[Option<[u8; 32]>],
) -> [u8; 32] {
    let mut d = Sha256::new();
    d.update(SOUND_DIGEST_DOMAIN);
    for id in referenced(chart) {
        d.update(id.0.to_le_bytes());
        let path = chart
            .sounds
            .get(usize::from(id.0))
            .and_then(|s| s.path.as_deref());
        let Some(path) = path else {
            d.update([0]);
            continue;
        };
        match path_index
            .get(path)
            .and_then(|&i| hashes.get(i))
            .copied()
            .flatten()
        {
            Some(hash) => {
                d.update([2]);
                d.update(hash);
            }
            None => d.update([1]),
        }
    }
    d.finalize().into()
}

/// Frames at `rate` for `frames` at `src_rate`, rounded up.
fn device_frames(frames: u64, src_rate: u32, rate: u32) -> u64 {
    let n = u128::from(frames) * u128::from(rate);
    u64::try_from(n.div_ceil(u128::from(src_rate.max(1)))).unwrap_or(u64::MAX)
}

/// Bytes of `frames` i16 frames of `ch` channels.
fn pcm_bytes(frames: u64, ch: u64) -> u64 {
    frames.saturating_mul(ch * 2)
}

/// Bytes `samples` has allocated.
fn cap_bytes(samples: &Vec<i16>) -> u64 {
    samples.capacity() as u64 * 2
}

/// Makes room for `extra` more samples, charging what is allocated: during
/// a reallocation the old and the new buffer both, then the new one. The
/// charge is taken before allocating, so a refusal allocates nothing.
fn grow(samples: &mut Vec<i16>, extra: u64, r: &mut Reservation<'_>) -> Result<(), u64> {
    let len = samples.len() as u64;
    let need = len + extra;
    let old = samples.capacity() as u64;
    if need <= old {
        return Ok(());
    }
    let new = need.max(old * 2);
    r.resize((old + new) * 2)?;
    samples.reserve_exact(usize::try_from(new - len).unwrap_or(usize::MAX));
    r.resize(cap_bytes(samples))
}

/// A share of the bank budget held while a sound decodes. Dropping it (on
/// failure, cancellation or unwinding) returns the share; [`Self::commit`]
/// keeps it for a stored sound.
struct Reservation<'a> {
    used: &'a AtomicU64,
    max: u64,
    held: u64,
}

impl<'a> Reservation<'a> {
    fn new(used: &'a AtomicU64, max: u64) -> Self {
        Self { used, max, held: 0 }
    }

    /// Changes the share to `bytes`; on growth, fails with the bank total
    /// if it would pass the cap (the share is then already counted, and
    /// returned on drop).
    fn resize(&mut self, bytes: u64) -> Result<(), u64> {
        if bytes <= self.held {
            self.used.fetch_sub(self.held - bytes, Ordering::Relaxed);
            self.held = bytes;
            return Ok(());
        }
        let add = bytes - self.held;
        self.held = bytes;
        let total = self.used.fetch_add(add, Ordering::Relaxed) + add;
        if total > self.max { Err(total) } else { Ok(()) }
    }

    fn commit(mut self) {
        self.held = 0;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.used.fetch_sub(self.held, Ordering::Relaxed);
    }
}

/// Counts raw file bytes held, and the most held at once.
struct RawBytes<'a> {
    live: &'a AtomicU64,
    n: u64,
}

impl<'a> RawBytes<'a> {
    fn new(live: &'a AtomicU64) -> Self {
        Self { live, n: 0 }
    }

    /// Counts `n` bytes held from now on, before they are allocated.
    fn set(&mut self, n: u64, peak: &AtomicU64) {
        let now = if n >= self.n {
            self.live.fetch_add(n - self.n, Ordering::Relaxed) + (n - self.n)
        } else {
            self.live.fetch_sub(self.n - n, Ordering::Relaxed) - (self.n - n)
        };
        self.n = n;
        peak.fetch_max(now, Ordering::Relaxed);
    }
}

impl Drop for RawBytes<'_> {
    fn drop(&mut self) {
        self.live.fetch_sub(self.n, Ordering::Relaxed);
    }
}

/// Reads `path` into memory, refusing it past `max` bytes (`Err(Ok(n))`,
/// with the bytes read, one past the cap).
/// The buffer is counted in `raw` from before it is allocated.
fn read_capped(
    path: &Path,
    max: u64,
    sound: SoundId,
    raw: &mut RawBytes<'_>,
    peak: &AtomicU64,
) -> Result<Vec<u8>, Stop> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let hint = file
        .metadata()
        .map_or(0, |m| m.len())
        .min(max.saturating_add(1));
    raw.set(hint, peak);
    let mut bytes = Vec::with_capacity(usize::try_from(hint).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    raw.set(bytes.capacity() as u64, peak);
    if bytes.len() as u64 > max {
        return Err(Stop::Cap(BankError::OverCap {
            cap: Cap::FileSize,
            found: Found::Declared,
            sound,
            amount: bytes.len() as u64,
            limit: max,
        }));
    }
    Ok(bytes)
}

struct Opened {
    format: Box<dyn FormatReader>,
    track: u32,
    params: AudioCodecParameters,
    declared: Option<u64>,
}

fn open(bytes: Vec<u8>, ext: Option<&str>) -> Result<Opened, String> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("no audio codec parameters")?
        .clone();
    Ok(Opened {
        track: track.id,
        declared: track.num_frames,
        params,
        format,
    })
}

/// Why one file was not stored.
enum Stop {
    Cap(BankError),
    Undecodable(String),
    /// Another file already exceeded a cap.
    Cancelled,
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Self::Undecodable(reason)
    }
}

/// What one file came to.
enum Outcome {
    Stored(Sound),
    /// Same contents as the file at this path index.
    Duplicate(usize),
    Stopped(Stop),
}

struct Shared<'a> {
    rate: u32,
    speed: Speed,
    limits: &'a BankLimits,
    max_frames: u64,
    /// Bank bytes stored or reserved.
    used: AtomicU64,
    /// Bank bytes the distinct files declare.
    declared: AtomicU64,
    raw_live: AtomicU64,
    raw_peak: AtomicU64,
    stop: AtomicBool,
    /// Contents hash to the path index that decodes them.
    taken: Mutex<HashMap<[u8; 32], usize>>,
}

/// Reads, hashes and decodes the file at path index `i`, holding its bytes
/// only until it returns; with the hash of its bytes when they were read.
fn process(
    i: usize,
    path: &Path,
    sound: SoundId,
    shared: &Shared<'_>,
) -> (Option<[u8; 32]>, Outcome) {
    if shared.stop.load(Ordering::Relaxed) {
        return (None, Outcome::Stopped(Stop::Cancelled));
    }
    let mut raw = RawBytes::new(&shared.raw_live);
    let max = shared.limits.max_file_bytes;
    let bytes = match read_capped(path, max, sound, &mut raw, &shared.raw_peak) {
        Ok(bytes) => bytes,
        Err(stop) => return (None, Outcome::Stopped(stop)),
    };
    let hash: [u8; 32] = Sha256::digest(&bytes).into();
    {
        let mut taken = shared.taken.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&owner) = taken.get(&hash) {
            return (Some(hash), Outcome::Duplicate(owner));
        }
        taken.insert(hash, i);
    }
    let ext = path.extension().and_then(|e| e.to_str());
    let decoded = panic::catch_unwind(AssertUnwindSafe(|| decode(bytes, ext, path, sound, shared)))
        .unwrap_or_else(|_| {
            Err(Stop::Undecodable(
                "the decoder failed on malformed data".into(),
            ))
        });
    let outcome = match decoded {
        Ok(s) => Outcome::Stored(s),
        Err(stop) => Outcome::Stopped(stop),
    };
    (Some(hash), outcome)
}

fn decode(
    bytes: Vec<u8>,
    ext: Option<&str>,
    path: &Path,
    sound: SoundId,
    shared: &Shared<'_>,
) -> Result<Sound, Stop> {
    #[cfg(not(test))]
    let _ = path;
    let max_bytes = shared.limits.max_bank_bytes;
    let mut opened = open(bytes, ext)?;
    let src_rate = opened
        .params
        .sample_rate
        .ok_or_else(|| Stop::Undecodable("no sample rate".into()))?;
    if !(MIN_RATE..=MAX_RATE).contains(&src_rate) {
        return Err(
            format!("sample rate {src_rate} Hz is outside {MIN_RATE}-{MAX_RATE} Hz").into(),
        );
    }
    let ch: u64 = match opened.params.channels.as_ref().map_or(2, |c| c.count()) {
        0 | 1 => 1,
        _ => 2,
    };
    // The declared share is returned if this sound is not stored.
    let mut declared = Reservation::new(&shared.declared, max_bytes);
    if let Some(n) = opened.declared {
        let frames = device_frames(n, src_rate, shared.rate);
        if frames > shared.max_frames {
            return Err(Stop::Cap(BankError::OverCap {
                cap: Cap::SoundLength,
                found: Found::Declared,
                sound,
                amount: frames,
                limit: shared.max_frames,
            }));
        }
        let stored = shared.speed.stretch(frames);
        declared
            .resize(pcm_bytes(n.max(frames).max(stored), ch))
            .map_err(|total| {
                Stop::Cap(BankError::OverCap {
                    cap: Cap::BankMemory,
                    found: Found::Declared,
                    sound,
                    amount: total,
                    limit: max_bytes,
                })
            })?;
    }
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&opened.params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let mut reservation = Reservation::new(&shared.used, max_bytes);
    let over_bank = |total| {
        Stop::Cap(BankError::OverCap {
            cap: Cap::BankMemory,
            found: Found::WhileDecoding,
            sound,
            amount: total,
            limit: max_bytes,
        })
    };
    let mut samples: Vec<i16> = Vec::new();
    if let Some(n) = opened.declared {
        // A known length is allocated exactly, once.
        grow(&mut samples, n * ch, &mut reservation).map_err(over_bank)?;
    }
    let mut scratch: Vec<i16> = Vec::new();
    let mut src_frames = 0u64;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let packet = match opened.format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // A file shorter than its header says ends where its data ends.
            Err(SymphoniaError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string().into()),
        };
        if packet.track_id != opened.track {
            continue;
        }
        let buf = decoder.decode(&packet).map_err(|e| e.to_string())?;
        let frames = buf.frames();
        let sc = buf.spec().channels().count().max(1);
        buf.copy_to_vec_interleaved::<i16>(&mut scratch);
        src_frames += frames as u64;
        let dev = device_frames(src_frames, src_rate, shared.rate);
        if dev > shared.max_frames {
            return Err(Stop::Cap(BankError::OverCap {
                cap: Cap::SoundLength,
                found: Found::WhileDecoding,
                sound,
                amount: dev,
                limit: shared.max_frames,
            }));
        }
        grow(&mut samples, frames as u64 * ch, &mut reservation).map_err(over_bank)?;
        for f in 0..frames {
            for c in 0..ch as usize {
                samples.push(scratch.get(f * sc + c.min(sc - 1)).copied().unwrap_or(0));
            }
        }
        #[cfg(test)]
        inject::check(path, src_frames)?;
    }
    let out_frames = device_frames(src_frames, src_rate, shared.rate);
    let held = cap_bytes(&samples);
    let plan = plan_conversion(src_rate, shared.rate, shared.speed, ch as usize);
    // Each stage holds its input and its output, and its workspace bound is
    // charged before the resampler is built: a workspace that does not fit
    // refuses the sound before it allocates.
    if let Some(stage) = plan.device {
        reservation
            .resize(held + pcm_bytes(out_frames, ch) + stage.workspace_bound())
            .map_err(over_bank)?;
        samples = stage.convert(&samples, out_frames)?;
    } else if samples.capacity() > samples.len() {
        // Shrinking copies into a buffer of the length beside the old one.
        reservation
            .resize(held + pcm_bytes(src_frames, ch))
            .map_err(over_bank)?;
        samples.shrink_to_fit();
    }
    if let Some(stage) = plan.speed {
        let frames = shared.speed.stretch(out_frames);
        reservation
            .resize(cap_bytes(&samples) + pcm_bytes(frames, ch) + stage.workspace_bound())
            .map_err(over_bank)?;
        samples = stage.convert(&samples, frames)?;
    }
    reservation.resize(cap_bytes(&samples)).map_err(over_bank)?;
    reservation.commit();
    declared.commit();
    Ok(if ch == 2 {
        Sound::stereo(samples)
    } else {
        Sound::mono(samples)
    })
}

/// [`Stage::convert`] from `src_rate` to `rate`.
fn resample(
    samples: &[i16],
    ch: usize,
    src_rate: u32,
    rate: u32,
    out_frames: u64,
) -> Result<Vec<i16>, String> {
    let mut resampler = Fft::<f32>::new(
        src_rate as usize,
        rate as usize,
        CHUNK,
        ch,
        FixedSync::Input,
    )
    .map_err(|e| e.to_string())?;
    let mut skip = resampler.output_delay();
    let mut input = vec![0.0f32; resampler.input_frames_max() * ch];
    let mut output = vec![0.0f32; resampler.output_frames_max() * ch];
    let want = usize::try_from(out_frames).map_err(|e| e.to_string())? * ch;
    let mut out: Vec<i16> = Vec::with_capacity(want);
    let mut pos = 0;
    while out.len() < want {
        let need = resampler.input_frames_next();
        // Past the end of the sound, zeros flush the resampler.
        for (k, x) in input[..need * ch].iter_mut().enumerate() {
            *x = samples
                .get(pos + k)
                .map_or(0.0, |&s| f32::from(s) / 32_768.0);
        }
        pos += need * ch;
        let frames_out = output.len() / ch;
        let src =
            InterleavedSlice::new(&input[..need * ch], ch, need).map_err(|e| e.to_string())?;
        let mut dst = InterleavedSlice::new_mut(&mut output[..], ch, frames_out)
            .map_err(|e| e.to_string())?;
        let (_, produced) = resampler
            .process_into_buffer(&src, &mut dst, None)
            .map_err(|e| e.to_string())?;
        let dropped = skip.min(produced);
        skip -= dropped;
        for &x in &output[dropped * ch..produced * ch] {
            if out.len() == want {
                break;
            }
            out.push((x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16);
        }
    }
    Ok(out)
}

/// Runs `f(0..jobs)` on `threads` threads, results in job order.
fn pool<T: Send>(threads: usize, jobs: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, T)> = thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= jobs {
                            break out;
                        }
                        out.push((i, f(i)));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, t)| t).collect()
}

/// Every sound id the chart's notes and BGM reference, in id order.
fn referenced(chart: &Chart) -> Vec<SoundId> {
    let mut used = vec![false; chart.sounds.len()];
    let mut mark = |id: Option<SoundId>| {
        if let Some(u) = id.and_then(|id| used.get_mut(usize::from(id.0))) {
            *u = true;
        }
    };
    for note in chart.lanes.iter().flatten() {
        mark(note.sound);
        if let NoteKind::Long { end_sound, .. } = note.kind {
            mark(end_sound);
        }
    }
    for event in &chart.bgm {
        mark(Some(event.sound));
    }
    (0..chart.sounds.len() as u16)
        .map(SoundId)
        .filter(|id| used[usize::from(id.0)])
        .collect()
}

/// Decodes every sound `chart` references into a bank at `rate`.
///
/// Sounds without a resolved path are absent from the bank. Files whose
/// contents are identical are stored once.
///
/// # Errors
///
/// [`BankError::RateOutOfRange`] for a device rate outside
/// [`MIN_RATE`]..=[`MAX_RATE`]; [`BankError::OverCap`] when a file, a sound
/// or the whole bank exceeds `limits`, found from the file sizes and the
/// declared frame counts before decoding, or from the running count while
/// decoding.
pub fn load_bank(chart: &Chart, rate: u32, limits: &BankLimits) -> Result<SoundBank, BankError> {
    load_bank_at(chart, rate, Speed::NORMAL, limits)
}

/// [`load_bank`] with every sound converted for `speed`: the bank stays at
/// `rate`, and each sound of `n` device frames holds `floor(n × 20/k)`, so
/// it plays slower and lower, or faster and higher. The sound length cap
/// applies to the device frames at 1×; the bank cap to what is held,
/// including each conversion stage's workspace bound.
///
/// # Errors
///
/// As [`load_bank`]; a stage whose input, output and workspace bound pass
/// the bank cap refuses the bank before the stage is built.
pub fn load_bank_at(
    chart: &Chart,
    rate: u32,
    speed: Speed,
    limits: &BankLimits,
) -> Result<SoundBank, BankError> {
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        return Err(BankError::RateOutOfRange { rate });
    }
    let started = Instant::now();

    // Distinct paths, each with the sounds that use it, in id order.
    let mut paths: Vec<(&Path, Vec<SoundId>)> = Vec::new();
    let mut path_index: HashMap<&Path, usize> = HashMap::new();
    for id in referenced(chart) {
        let Some(path) = chart.sounds[usize::from(id.0)].path.as_deref() else {
            continue;
        };
        let i = *path_index.entry(path).or_insert_with(|| {
            paths.push((path, Vec::new()));
            paths.len() - 1
        });
        paths[i].1.push(id);
    }
    let cores = thread::available_parallelism().map_or(1, usize::from);
    let threads = limits.max_decode_threads.min(cores).min(paths.len()).max(1);

    let shared = Shared {
        rate,
        speed,
        limits,
        max_frames: u64::from(limits.max_sound_seconds) * u64::from(rate),
        used: AtomicU64::new(0),
        declared: AtomicU64::new(0),
        raw_live: AtomicU64::new(0),
        raw_peak: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        taken: Mutex::new(HashMap::new()),
    };
    let (hashes, mut outcomes): (Vec<_>, Vec<_>) = pool(threads, paths.len(), |i| {
        let r = process(i, paths[i].0, paths[i].1[0], &shared);
        if matches!(r.1, Outcome::Stopped(Stop::Cap(_))) {
            shared.stop.store(true, Ordering::Relaxed);
        }
        r
    })
    .into_iter()
    .unzip();
    // The first cap error in path order refuses the bank.
    for o in &mut outcomes {
        if let Outcome::Stopped(Stop::Cap(e)) = o {
            return Err(e.clone());
        }
    }

    // Store in path order; a duplicate follows the file that was decoded.
    let mut stored: Vec<Option<u32>> = vec![None; paths.len()];
    let mut dup_of: Vec<Option<usize>> = vec![None; paths.len()];
    let mut reasons: Vec<String> = vec![String::new(); paths.len()];
    let mut sounds = Vec::new();
    for (i, o) in outcomes.into_iter().enumerate() {
        match o {
            Outcome::Stored(s) => {
                stored[i] = Some(sounds.len() as u32);
                sounds.push(s);
            }
            Outcome::Duplicate(owner) => dup_of[i] = Some(owner),
            Outcome::Stopped(Stop::Undecodable(reason)) => reasons[i] = reason,
            Outcome::Stopped(Stop::Cap(_) | Stop::Cancelled) => {}
        }
    }
    let mut slots = vec![None; chart.sounds.len()];
    let mut warnings = Vec::new();
    for (i, (_, ids)) in paths.iter().enumerate() {
        let from = dup_of[i].unwrap_or(i);
        for &id in ids {
            match stored[from] {
                Some(slot) => slots[usize::from(id.0)] = Some(slot),
                None => warnings.push(BankWarning::Undecodable {
                    sound: id,
                    reason: reasons[from].clone(),
                }),
            }
        }
    }
    warnings.sort_by_key(|BankWarning::Undecodable { sound, .. }| *sound);
    let sound_digest = Some(sound_digest(chart, &path_index, &hashes));
    Ok(SoundBank {
        rate,
        speed,
        slots,
        sounds,
        warnings,
        sound_digest,
        stats: LoadStats {
            decode_threads: threads,
            files: paths.len(),
            peak_raw_bytes: shared.raw_peak.load(Ordering::Relaxed),
            elapsed: started.elapsed(),
        },
    })
}

/// A test-only hook that fails or panics the decode of a chosen file once
/// it has decoded some frames.
#[cfg(test)]
mod inject {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::Stop;

    #[derive(Clone, Copy)]
    pub enum Fault {
        Fail,
        Panic,
    }

    static FAULTS: Mutex<Vec<(PathBuf, Fault)>> = Mutex::new(Vec::new());

    pub fn arm(path: &Path, fault: Fault) {
        FAULTS.lock().unwrap().push((path.to_owned(), fault));
    }

    pub fn check(path: &Path, frames: u64) -> Result<(), Stop> {
        let fault = FAULTS
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|&(_, f)| f);
        match fault {
            Some(_) if frames == 0 => Ok(()),
            Some(Fault::Fail) => Err(Stop::Undecodable(format!("injected after {frames} frames"))),
            Some(Fault::Panic) => panic!("injected panic after {frames} frames"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{ChartBuilder, SoundId, TimeUs};

    use super::inject::{self, Fault};
    use super::*;

    /// The workspace bound is derived from these crates' sources; when the
    /// lock file pins another version, the bound must be derived again and
    /// `BOUND_DERIVED_FOR` updated with it.
    #[test]
    fn workspace_bound_matches_the_pinned_resampler() {
        let lock = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock"),
        )
        .unwrap();
        for (name, version) in BOUND_DERIVED_FOR {
            let pinned: Vec<&str> = lock
                .split("[[package]]")
                .filter_map(|p| {
                    let mut lines = p.lines().map(str::trim);
                    lines.find(|l| *l == format!("name = \"{name}\""))?;
                    lines.find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"'))
                })
                .collect();
            assert_eq!(
                pinned,
                [version],
                "{name} is pinned at {pinned:?}; derive the workspace bound again for it"
            );
        }
    }

    #[test]
    fn device_frames_round_up_and_keep_equal_rates_exact() {
        assert_eq!(device_frames(44_100, 44_100, 48_000), 48_000);
        assert_eq!(device_frames(1, 44_100, 48_000), 2);
        assert_eq!(device_frames(147, 44_100, 48_000), 160);
        assert_eq!(device_frames(148, 44_100, 48_000), 162);
        assert_eq!(device_frames(12_345, 48_000, 48_000), 12_345);
        assert_eq!(device_frames(0, 22_050, 48_000), 0);
    }

    /// A 16-bit mono WAV at 8 kHz that declares its length when `known`,
    /// and otherwise carries the "length unknown" sizes of a stream.
    fn wav(samples: &[i16], known: bool) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let (riff_len, data_len) = if known {
            (36 + data_len, data_len)
        } else {
            (u32::MAX, u32::MAX)
        };
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&riff_len.to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        for v in [1u16, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [8_000u32, 16_000] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [2u16, 16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        b
    }

    /// A broken sound of 500 frames (1000 bytes) that fails or panics
    /// after decoding them, then a healthy one of 1000 frames (2000 bytes),
    /// against a 2000-byte bank: the broken one's charges, live and (for a
    /// known length) declared, must be returned for the healthy one to fit.
    fn failure_then_healthy(fault: Fault, known: bool, name: &str) {
        // The workspace's target directory, as for the integration tests.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("audio-unit-")
            .tempdir_in(root)
            .unwrap();
        let broken = dir.path().join(format!("{name}.wav"));
        let healthy = dir.path().join("healthy.wav");
        std::fs::write(&broken, wav(&[100; 500], known)).unwrap();
        std::fs::write(&healthy, wav(&[200; 1_000], known)).unwrap();
        inject::arm(&broken, fault);
        let mut chart = ChartBuilder::new(120_000)
            .sound("broken.wav")
            .sound("healthy.wav")
            .bgm(TimeUs(0), SoundId(0))
            .bgm(TimeUs(1), SoundId(1))
            .build()
            .unwrap();
        chart.sounds[0].path = Some(broken);
        chart.sounds[1].path = Some(healthy);
        let limits = BankLimits {
            max_bank_bytes: 2_000,
            max_decode_threads: 1,
            ..BankLimits::default()
        };
        let bank = load_bank(&chart, 8_000, &limits).unwrap();
        assert_eq!(bank.get(SoundId(0)), None);
        assert_eq!(bank.get(SoundId(1)).unwrap().frames(), 1_000);
        assert_eq!(bank.warnings().len(), 1);
    }

    #[test]
    fn a_failed_decode_returns_its_charge() {
        failure_then_healthy(Fault::Fail, false, "fails");
        failure_then_healthy(Fault::Fail, true, "fails-known");
    }

    #[test]
    fn a_decode_panic_is_contained_and_returns_its_charge() {
        failure_then_healthy(Fault::Panic, false, "panics");
        failure_then_healthy(Fault::Panic, true, "panics-known");
    }
}
