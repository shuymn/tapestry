//! The sound bank: every sound a chart references, decoded before play into
//! i16 at the device rate (design.md "Audio"; stage1.md D12, D14).
//!
//! Loading runs in two passes over a pool of at most
//! `min(available cores, BankLimits::max_decode_threads)` threads. The first
//! hashes each referenced file and reads its header; files with identical
//! contents are stored once, and the caps are checked against the frame
//! counts the containers declare, before anything is decoded. The second
//! decodes and checks the caps again against a running count, which bounds
//! files that declare no length. A sound over a cap refuses the whole bank;
//! a file that cannot be read or decoded is left silent with a warning,
//! like a sound file the chart names but the folder lacks.

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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

/// Caps on decoding (stage1.md D14). They bound work on untrusted files and
/// sit far beyond real charts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BankLimits {
    /// Longest sound, in seconds of frames at the device rate.
    pub max_sound_seconds: u32,
    /// Largest bank, in bytes of decoded i16.
    pub max_bank_bytes: u64,
    /// Most decode threads; fewer when the machine has fewer cores.
    pub max_decode_threads: usize,
}

impl Default for BankLimits {
    /// 10 minutes per sound, 512 MiB per bank, 4 decode threads.
    fn default() -> Self {
        Self {
            max_sound_seconds: 600,
            max_bank_bytes: 512 * 1024 * 1024,
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
}

/// How the excess was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// From the frame counts the containers declare, before decoding.
    Declared,
    /// From the running count while decoding.
    WhileDecoding,
}

/// Why a bank was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BankError {
    /// The device rate is zero.
    ZeroRate,
    /// A cap was exceeded at `sound`: `amount` (frames or bytes, by `cap`)
    /// is over `limit`.
    OverCap {
        /// The cap.
        cap: Cap,
        /// How it was found.
        found: Found,
        /// The sound whose length or size crossed the cap.
        sound: SoundId,
        /// Frames of the sound, or bytes of the bank up to and including it.
        amount: u64,
        /// The cap in the same unit.
        limit: u64,
    },
}

impl fmt::Display for BankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRate => write!(f, "the device rate is zero"),
            Self::OverCap {
                cap,
                found,
                sound,
                amount,
                limit,
            } => {
                let what = match cap {
                    Cap::SoundLength => "sound length in frames",
                    Cap::BankMemory => "decoded sound memory in bytes",
                };
                let how = match found {
                    Found::Declared => "as declared",
                    Found::WhileDecoding => "while decoding",
                };
                write!(
                    f,
                    "sound {:02} exceeds the {what} cap {how}: {amount} > {limit}",
                    sound.0
                )
            }
        }
    }
}

impl std::error::Error for BankError {}

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
    pub fn frames(&self) -> u64 {
        (self.samples.len() / self.channels()) as u64
    }

    /// Decoded size in bytes.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.samples.len() as u64 * 2
    }

    fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }
}

/// The decoded sounds of a chart, indexed by [`SoundId`].
#[derive(Clone, Debug, Default)]
pub struct SoundBank {
    rate: u32,
    /// Per [`SoundId`], the index into `sounds`.
    slots: Vec<Option<u32>>,
    sounds: Vec<Sound>,
    warnings: Vec<BankWarning>,
    stats: LoadStats,
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

    /// The sound for `id`, if one was decoded.
    #[must_use]
    pub fn get(&self, id: SoundId) -> Option<&Sound> {
        self.stored(self.slot(id)?)
    }

    /// The stored index for `id`; ids whose files have identical contents
    /// share one.
    #[must_use]
    pub fn slot(&self, id: SoundId) -> Option<u32> {
        self.slots.get(usize::from(id.0)).copied().flatten()
    }

    /// Stored sound `index`.
    #[must_use]
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
}

/// Frames at `rate` for `frames` at `src_rate`, rounded up.
fn device_frames(frames: u64, src_rate: u32, rate: u32) -> u64 {
    let n = u128::from(frames) * u128::from(rate);
    u64::try_from(n.div_ceil(u128::from(src_rate.max(1)))).unwrap_or(u64::MAX)
}

/// What the first pass learns about a file.
struct Probe {
    hash: [u8; 32],
    src_rate: u32,
    /// Channels kept: 1, or 2 for any source with two or more.
    channels: u64,
    declared: Option<u64>,
}

struct Opened {
    format: Box<dyn FormatReader>,
    track: u32,
    params: AudioCodecParameters,
    declared: Option<u64>,
}

fn open(path: &Path) -> Result<Opened, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
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

fn hash_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

fn probe(path: &Path) -> Result<Probe, String> {
    let hash = hash_file(path)?;
    let opened = open(path)?;
    let src_rate = opened.params.sample_rate.ok_or("no sample rate")?;
    let channels = opened.params.channels.as_ref().map_or(2, |c| c.count());
    Ok(Probe {
        hash,
        src_rate,
        channels: if channels >= 2 { 2 } else { 1 },
        declared: opened.declared,
    })
}

/// Why decoding one file stopped.
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

struct Job<'a> {
    path: &'a Path,
    probe: &'a Probe,
    sound: SoundId,
}

struct Shared<'a> {
    rate: u32,
    max_frames: u64,
    max_bytes: u64,
    charged: &'a AtomicU64,
    stop: &'a AtomicBool,
}

fn decode(job: &Job<'_>, shared: &Shared<'_>) -> Result<Sound, Stop> {
    let mut opened = open(job.path)?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&opened.params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;
    let ch = job.probe.channels as usize;
    let src_rate = job.probe.src_rate;
    let reserve = job.probe.declared.map_or(0, |n| n.min(shared.max_frames)) as usize * ch;
    let mut samples: Vec<i16> = Vec::with_capacity(reserve);
    let mut scratch: Vec<i16> = Vec::new();
    let mut src_frames = 0u64;
    let mut charged_frames = 0u64;
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
                sound: job.sound,
                amount: dev,
                limit: shared.max_frames,
            }));
        }
        let add = (dev - charged_frames) * ch as u64 * 2;
        charged_frames = dev;
        let total = shared.charged.fetch_add(add, Ordering::Relaxed) + add;
        if total > shared.max_bytes {
            return Err(Stop::Cap(BankError::OverCap {
                cap: Cap::BankMemory,
                found: Found::WhileDecoding,
                sound: job.sound,
                amount: total,
                limit: shared.max_bytes,
            }));
        }
        for f in 0..frames {
            for c in 0..ch {
                samples.push(scratch.get(f * sc + c.min(sc - 1)).copied().unwrap_or(0));
            }
        }
    }
    if src_rate != shared.rate {
        let out_frames = device_frames(src_frames, src_rate, shared.rate);
        samples = resample(&samples, ch, src_rate, shared.rate, out_frames)?;
    }
    Ok(if ch == 2 {
        Sound::stereo(samples)
    } else {
        Sound::mono(samples)
    })
}

/// Converts interleaved `samples` from `src_rate` to `rate`, giving exactly
/// `out_frames` frames with the resampler's delay removed.
fn resample(
    samples: &[i16],
    ch: usize,
    src_rate: u32,
    rate: u32,
    out_frames: u64,
) -> Result<Vec<i16>, String> {
    let frames = samples.len() / ch;
    let input: Vec<f32> = samples.iter().map(|&s| f32::from(s) / 32_768.0).collect();
    let mut resampler =
        Fft::<f32>::new(src_rate as usize, rate as usize, 1024, ch, FixedSync::Input)
            .map_err(|e| e.to_string())?;
    let adapter = InterleavedSlice::new(&input, ch, frames).map_err(|e| e.to_string())?;
    let output = resampler
        .process_all(&adapter, frames, None)
        .map_err(|e| e.to_string())?
        .take_data();
    let want = out_frames as usize * ch;
    let mut out: Vec<i16> = output
        .iter()
        .take(want)
        .map(|&x| (x * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect();
    out.resize(want, 0);
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
/// [`BankError::ZeroRate`] for a zero rate; [`BankError::OverCap`] when a
/// sound or the whole bank exceeds `limits`, found from the declared frame
/// counts before any decoding, or from the running count while decoding.
pub fn load_bank(chart: &Chart, rate: u32, limits: &BankLimits) -> Result<SoundBank, BankError> {
    if rate == 0 {
        return Err(BankError::ZeroRate);
    }
    let started = Instant::now();
    let max_frames = u64::from(limits.max_sound_seconds) * u64::from(rate);
    let max_bytes = limits.max_bank_bytes;

    // Distinct paths, each with the sounds that use it.
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

    // First pass: hash and read headers.
    let probes = pool(threads, paths.len(), |i| probe(paths[i].0));
    let mut warnings = Vec::new();
    let undecodable = |warnings: &mut Vec<BankWarning>, ids: &[SoundId], reason: &str| {
        warnings.extend(ids.iter().map(|&sound| BankWarning::Undecodable {
            sound,
            reason: reason.to_owned(),
        }));
    };
    // Distinct contents: the first path with them, and every id using them.
    let mut contents: Vec<(&Path, &Probe, Vec<SoundId>)> = Vec::new();
    let mut by_hash: HashMap<[u8; 32], usize> = HashMap::new();
    for ((path, ids), probe) in paths.iter().zip(&probes) {
        match probe {
            Ok(p) => {
                let c = *by_hash.entry(p.hash).or_insert_with(|| {
                    contents.push((path, p, Vec::new()));
                    contents.len() - 1
                });
                contents[c].2.extend_from_slice(ids);
            }
            Err(reason) => undecodable(&mut warnings, ids, reason),
        }
    }

    // Caps against the declared lengths, before decoding.
    let mut declared_bytes = 0u64;
    for &(_, p, ref ids) in &contents {
        let sound = ids[0];
        let Some(n) = p.declared else { continue };
        let frames = device_frames(n, p.src_rate, rate);
        if frames > max_frames {
            return Err(BankError::OverCap {
                cap: Cap::SoundLength,
                found: Found::Declared,
                sound,
                amount: frames,
                limit: max_frames,
            });
        }
        declared_bytes = declared_bytes.saturating_add(frames * p.channels * 2);
        if declared_bytes > max_bytes {
            return Err(BankError::OverCap {
                cap: Cap::BankMemory,
                found: Found::Declared,
                sound,
                amount: declared_bytes,
                limit: max_bytes,
            });
        }
    }

    // Second pass: decode with running counts.
    let charged = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let shared = Shared {
        rate,
        max_frames,
        max_bytes,
        charged: &charged,
        stop: &stop,
    };
    let decoded = pool(threads, contents.len(), |k| {
        let (path, probe, ref ids) = contents[k];
        let job = Job {
            path,
            probe,
            sound: ids[0],
        };
        let r = decode(&job, &shared);
        if matches!(r, Err(Stop::Cap(_))) {
            stop.store(true, Ordering::Relaxed);
        }
        r
    });
    let mut slots = vec![None; chart.sounds.len()];
    let mut sounds = Vec::new();
    let mut cap_error = None;
    for ((_, _, ids), r) in contents.iter().zip(decoded) {
        match r {
            Ok(sound) => {
                for id in ids {
                    slots[usize::from(id.0)] = Some(sounds.len() as u32);
                }
                sounds.push(sound);
            }
            Err(Stop::Cap(e)) => {
                cap_error.get_or_insert(e);
            }
            Err(Stop::Undecodable(reason)) => undecodable(&mut warnings, ids, &reason),
            Err(Stop::Cancelled) => {}
        }
    }
    if let Some(e) = cap_error {
        return Err(e);
    }
    warnings.sort_by_key(|BankWarning::Undecodable { sound, .. }| *sound);
    Ok(SoundBank {
        rate,
        slots,
        sounds,
        warnings,
        stats: LoadStats {
            decode_threads: threads,
            files: paths.len(),
            elapsed: started.elapsed(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::device_frames;

    #[test]
    fn device_frames_round_up_and_keep_equal_rates_exact() {
        assert_eq!(device_frames(44_100, 44_100, 48_000), 48_000);
        assert_eq!(device_frames(1, 44_100, 48_000), 2);
        assert_eq!(device_frames(147, 44_100, 48_000), 160);
        assert_eq!(device_frames(148, 44_100, 48_000), 162);
        assert_eq!(device_frames(12_345, 48_000, 48_000), 12_345);
        assert_eq!(device_frames(0, 22_050, 48_000), 0);
    }
}
