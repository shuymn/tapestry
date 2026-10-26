//! Input log format.
//!
//! A [`PlayRecord`] holds what re-evaluating a play needs: versions, chart
//! identity, the preset, the devices and offsets, the held-input snapshot,
//! the physical input events, devices that arrived during play, and how the
//! play ended. [`encode`] writes it as a file and [`decode`] reads it back
//! under [`DecodeLimits`].
//!
//! # File layout
//!
//! The magic `TPRL`, a little-endian `u16` format version, a `u8` body
//! encoding (1 = zstd), a little-endian `u32` uncompressed body length, then
//! the compressed body. The body is the sections of [`Section`] in order,
//! each a little-endian `u32` length then its payload. Integers in payloads
//! are LEB128 varints, zigzag for signed values; strings and byte strings are
//! a length then the bytes; hashes are raw. An event's time is the delta
//! from the previous event of the same source, and a source's first event
//! counts from `InputStart::opened_at`. Deltas are taken and accumulated
//! modulo 2^64 as two's complement `i64`, so every pair of times encodes
//! exactly (a delta from `i64::MAX` to `i64::MIN` is `1`).
//!
//! The layout is written field by field, not derived from struct layout.
//! Every record struct is destructured in full when encoded, so a new field
//! fails to compile until it is written; a layout change bumps
//! [`FORMAT_VERSION`]. Decoders read every version they know and refuse
//! newer ones.
//!
//! # Versions
//!
//! A record is written in the oldest version that holds it, so a layout
//! that grows never changes the bytes of a record the old one holds.
//!
//! - 1: the sections above. Each source of the devices section is its kind,
//!   receive method, input offset, then its bindings (a count, then each
//!   control and lane).
//! - 2: each source also has its axis bindings after its bindings: a
//!   count, then each axis's control, lane, mode (0 absolute, followed by
//!   its wrap; 1 relative), invert (0 or 1), press threshold, hold
//!   threshold and release time in µs, all unsigned. Written when any
//!   source binds an axis.

#![forbid(unsafe_code)]

mod error;
mod record;
mod wire;

use bms_model::{ChartIdentity, Lane, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, InputStart, PhysicalInputEvent,
    ScratchSettings, SourceId, SourceProfile,
};
use rules::RulePreset;
use sha2::{Digest, Sha256};

pub use error::{DecodeError, Section};
pub use record::{
    Arrival, BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete,
    PlayRecord, PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, TransformRecord,
    Versions,
};

use wire::{Reader, Writer};

/// The newest format version this crate writes and reads.
pub const FORMAT_VERSION: u16 = 2;

/// The first version with axis bindings.
const AXES_VERSION: u16 = 2;

/// The first bytes of every input log.
pub const MAGIC: [u8; 4] = *b"TPRL";

/// Body encoding: zstd.
const ENCODING_ZSTD: u8 = 1;

/// Length of the header before the compressed body.
const HEADER_LEN: usize = 4 + 2 + 1 + 4;

/// zstd compression level of [`encode`].
const ZSTD_LEVEL: i32 = 19;

/// Limits checked before the work they bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    /// The largest file accepted, header included.
    pub max_compressed: usize,
    /// The largest declared uncompressed body; checked before decompressing,
    /// and decompression stops at the declared length.
    pub max_uncompressed: u32,
    /// The most sources a record may have.
    pub max_sources: u16,
    /// The most input events a record may have.
    pub max_events: u64,
}

impl DecodeLimits {
    /// The limits for records the app saves: 64 MiB either side of
    /// compression, 64 sources, and 8 Mi events (an hour of one 1 kHz axis
    /// together with 16 buttons pressed and released 30 times a second,
    /// 7.1 M, stays below it).
    pub const STANDARD: Self = Self {
        max_compressed: 64 << 20,
        max_uncompressed: 64 << 20,
        max_sources: 64,
        max_events: 8 << 20,
    };
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::STANDARD
    }
}

/// SHA-256 of the preset's canonical bytes, which identifies it in records.
#[must_use]
pub fn preset_hash(p: &RulePreset) -> [u8; 32] {
    Sha256::digest(p.canonical_bytes()).into()
}

/// Encodes `rec` as a file.
///
/// # Panics
///
/// If the body or one of its sections exceeds `u32::MAX` bytes, which the
/// recording capacities rule out.
#[must_use]
pub fn encode(rec: &PlayRecord) -> Vec<u8> {
    let version = version_of(rec);
    let body = body_bytes(rec);
    let compressed = zstd::bulk::compress(&body, ZSTD_LEVEL)
        .expect("zstd compression of an in-memory buffer does not fail");
    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&version.to_le_bytes());
    out.push(ENCODING_ZSTD);
    out.extend_from_slice(&u32_len(body.len()).to_le_bytes());
    out.extend_from_slice(&compressed);
    out
}

/// The oldest format version that holds `rec`: 2 if a source binds an
/// axis, else 1.
#[must_use]
pub fn version_of(rec: &PlayRecord) -> u16 {
    if rec
        .devices
        .sources
        .iter()
        .any(|s| !s.profile.axes.is_empty())
    {
        AXES_VERSION
    } else {
        1
    }
}

/// The canonical uncompressed body of `rec`, in the version
/// [`version_of`] gives.
///
/// # Panics
///
/// If a section exceeds `u32::MAX` bytes.
#[must_use]
pub fn body_bytes(rec: &PlayRecord) -> Vec<u8> {
    let PlayRecord {
        versions,
        chart,
        preset,
        session_policy,
        devices,
        play_type,
        start,
        events,
        arrivals,
        status,
    } = rec;
    let version = version_of(rec);
    let mut body = Writer::default();
    let mut section = |write: &dyn Fn(&mut Writer)| {
        let mut w = Writer::default();
        write(&mut w);
        body.raw(&u32_len(w.0.len()).to_le_bytes());
        body.raw(&w.0);
    };
    section(&|w| write_versions(w, versions, chart.identity.loader_version));
    section(&|w| write_chart(w, chart));
    section(&|w| write_preset(w, preset));
    section(&|w| {
        w.str(session_policy);
        w.uint(match play_type {
            PlayType::Full => 0,
        });
    });
    section(&|w| write_devices(w, devices, version));
    section(&|w| write_start(w, start));
    section(&|w| write_events(w, start.opened_at, events));
    section(&|w| write_arrivals(w, arrivals));
    section(&|w| write_status(w, status));
    body.0
}

fn u32_len(n: usize) -> u32 {
    u32::try_from(n).expect("an input log section fits in u32")
}

fn write_versions(w: &mut Writer, v: &Versions, loader: u32) {
    let Versions {
        evaluator,
        app,
        provenance,
    } = v;
    w.uint(u64::from(*evaluator));
    w.uint(u64::from(loader));
    w.str(app);
    w.uint(match provenance {
        BuildProvenance::Production => 0,
        BuildProvenance::Development => 1,
    });
}

fn write_chart(w: &mut Writer, c: &ChartRecord) {
    let ChartRecord {
        identity:
            ChartIdentity {
                sha256,
                md5,
                resolved_hash,
                loader_version: _, // in the versions section
                random_results,
            },
        random_seed,
        transform: TransformRecord { id, version },
    } = c;
    w.raw(sha256);
    w.raw(md5);
    w.raw(resolved_hash);
    w.uint(*random_seed);
    w.len(random_results.len());
    for r in random_results {
        w.uint(u64::from(*r));
    }
    w.str(id);
    w.uint(u64::from(*version));
}

fn write_preset(w: &mut Writer, p: &PresetRecord) {
    let PresetRecord {
        id,
        sha256,
        canonical_bytes,
    } = p;
    w.str(id);
    w.raw(sha256);
    w.bytes(canonical_bytes);
}

const fn kind_tag(k: DeviceKind) -> u64 {
    match k {
        DeviceKind::Keyboard => 0,
        DeviceKind::Controller => 1,
    }
}

fn write_devices(w: &mut Writer, d: &DeviceRecord, version: u16) {
    let DeviceRecord {
        sources,
        audio_offset,
        display_offset,
    } = d;
    w.len(sources.len());
    for s in sources {
        let SourceRecord {
            kind,
            receive,
            profile:
                SourceProfile {
                    input_offset,
                    bindings,
                    axes,
                },
        } = s;
        w.uint(kind_tag(*kind));
        w.uint(match receive {
            ReceiveMethod::RawInput => 0,
            ReceiveMethod::Scripted => 1,
        });
        w.int(input_offset.0);
        w.len(bindings.len());
        for Binding { control, lane } in bindings {
            w.uint(u64::from(control.0));
            w.len(lane.index());
        }
        if version >= AXES_VERSION {
            write_axes(w, axes);
        }
    }
    w.int(audio_offset.0);
    w.int(display_offset.0);
}

fn write_axes(w: &mut Writer, axes: &[AxisBinding]) {
    w.len(axes.len());
    for AxisBinding {
        control,
        lane,
        scratch:
            ScratchSettings {
                mode,
                invert,
                press_threshold,
                hold_threshold,
                release_after_us,
            },
    } in axes
    {
        w.uint(u64::from(control.0));
        w.len(lane.index());
        match mode {
            AxisMode::Absolute { wrap } => {
                w.uint(0);
                w.uint(u64::from(*wrap));
            }
            AxisMode::Relative => w.uint(1),
        }
        w.uint(u64::from(*invert));
        w.uint(u64::from(*press_threshold));
        w.uint(u64::from(*hold_threshold));
        w.uint(u64::from(*release_after_us));
    }
}

fn write_start(w: &mut Writer, s: &InputStart) {
    let InputStart { opened_at, held } = s;
    w.int(opened_at.0);
    w.len(held.len());
    for (source, control, value) in held {
        w.uint(u64::from(source.0));
        w.uint(u64::from(control.0));
        w.int(i64::from(*value));
    }
}

fn write_events(w: &mut Writer, opened_at: TimeUs, events: &[PhysicalInputEvent]) {
    // The previous time per source; a source's first event counts from
    // `opened_at`. Wrapping arithmetic keeps every `i64` pair exact.
    let mut prev: Vec<i64> = Vec::new();
    w.len(events.len());
    for &PhysicalInputEvent {
        received,
        source,
        kind,
    } in events
    {
        let i = source.index();
        if i >= prev.len() {
            prev.resize(i + 1, opened_at.0);
        }
        w.uint(u64::from(source.0));
        w.int(received.0.wrapping_sub(prev[i]));
        prev[i] = received.0;
        match kind {
            InputKind::Control { control, value } => {
                w.uint(0);
                w.uint(u64::from(control.0));
                w.int(i64::from(value));
            }
            InputKind::Disconnected => w.uint(1),
        }
    }
}

fn write_arrivals(w: &mut Writer, arrivals: &[Arrival]) {
    w.len(arrivals.len());
    for &Arrival { at, kind } in arrivals {
        w.int(at.0);
        w.uint(kind_tag(kind));
    }
}

const OVERFLOW: u64 = 1;
const SOURCE_LIMIT: u64 = 1 << 1;
const SUSPEND: u64 = 1 << 2;

fn write_status(w: &mut Writer, s: &Status) {
    let Status {
        end,
        finalized,
        incomplete:
            Incomplete {
                overflow,
                source_limit,
                suspend,
            },
    } = s;
    w.uint(match end {
        EndStatus::PlayedToEnd => 0,
        EndStatus::EndedMidway => 1,
    });
    w.int(finalized.0);
    let bit = |set: bool, bit: u64| if set { bit } else { 0 };
    w.uint(bit(*overflow, OVERFLOW) | bit(*source_limit, SOURCE_LIMIT) | bit(*suspend, SUSPEND));
}

/// Decodes a file written by [`encode`] of this or an earlier format
/// version.
///
/// # Errors
///
/// Any [`DecodeError`]: a limit of `limits` exceeded, an unknown version or
/// encoding, or malformed data. Each limit is checked before the work it
/// bounds; malformed data is an error, never a panic.
pub fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<PlayRecord, DecodeError> {
    if bytes.len() > limits.max_compressed {
        return Err(DecodeError::FileTooLarge {
            len: bytes.len(),
            max: limits.max_compressed,
        });
    }
    let mut r = Reader::new(bytes);
    if r.array::<4>()? != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = u16::from_le_bytes(r.array()?);
    if version == 0 || version > FORMAT_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    let [encoding] = r.array()?;
    if encoding != ENCODING_ZSTD {
        return Err(DecodeError::UnsupportedEncoding(encoding));
    }
    let declared = r.u32_le()?;
    if declared > limits.max_uncompressed {
        return Err(DecodeError::DeclaredLengthTooLarge {
            declared,
            max: limits.max_uncompressed,
        });
    }
    let compressed = r.raw(r.remaining())?;
    // The output buffer holds exactly `declared` bytes; a body that would
    // decompress to more fails instead of growing it.
    let body = zstd::bulk::decompress(compressed, declared as usize)
        .map_err(|_| DecodeError::Decompress)?;
    if body.len() != declared as usize {
        return Err(DecodeError::LengthMismatch {
            declared,
            actual: body.len(),
        });
    }
    decode_body(&body, version, limits)
}

/// Splits off the next section, checking its length against the bytes left.
fn section<'a>(r: &mut Reader<'a>, section: Section) -> Result<Reader<'a>, DecodeError> {
    let len = r.u32_le()?;
    let remaining = r.remaining();
    if len as usize > remaining {
        return Err(DecodeError::SectionLength {
            section,
            len,
            remaining,
        });
    }
    Ok(Reader::new(r.raw(len as usize)?))
}

/// Reads one section with `read`, which must consume all of it.
fn read_section<'a, T>(
    r: &mut Reader<'a>,
    s: Section,
    read: impl FnOnce(&mut Reader<'a>) -> Result<T, DecodeError>,
) -> Result<T, DecodeError> {
    let mut sub = section(r, s)?;
    let value = read(&mut sub)?;
    sub.finish()?;
    Ok(value)
}

fn decode_body(
    body: &[u8],
    version: u16,
    limits: &DecodeLimits,
) -> Result<PlayRecord, DecodeError> {
    let mut r = Reader::new(body);
    let (versions, loader) = read_section(&mut r, Section::Versions, read_versions)?;
    let chart = read_section(&mut r, Section::Chart, |r| read_chart(r, loader))?;
    let preset = read_section(&mut r, Section::Preset, read_preset)?;
    let (session_policy, play_type) = read_section(&mut r, Section::Session, |r| {
        let policy = r.string()?;
        let play_type = match r.uint()? {
            0 => PlayType::Full,
            _ => return Err(DecodeError::InvalidValue("play type")),
        };
        Ok((policy, play_type))
    })?;
    let devices = read_section(&mut r, Section::Devices, |r| {
        read_devices(r, version, limits)
    })?;
    let n_sources = devices.sources.len();
    let start = read_section(&mut r, Section::Start, |r| read_start(r, n_sources))?;
    let events = read_section(&mut r, Section::Events, |r| {
        read_events(r, limits, n_sources, start.opened_at)
    })?;
    let arrivals = read_section(&mut r, Section::Arrivals, read_arrivals)?;
    let status = read_section(&mut r, Section::Status, read_status)?;
    r.finish()?;
    Ok(PlayRecord {
        versions,
        chart,
        preset,
        session_policy,
        devices,
        play_type,
        start,
        events,
        arrivals,
        status,
    })
}

fn read_versions(r: &mut Reader<'_>) -> Result<(Versions, u32), DecodeError> {
    let evaluator = r.u32()?;
    let loader = r.u32()?;
    let app = r.string()?;
    let provenance = match r.uint()? {
        0 => BuildProvenance::Production,
        1 => BuildProvenance::Development,
        _ => return Err(DecodeError::InvalidValue("build provenance")),
    };
    Ok((
        Versions {
            evaluator,
            app,
            provenance,
        },
        loader,
    ))
}

fn read_chart(r: &mut Reader<'_>, loader_version: u32) -> Result<ChartRecord, DecodeError> {
    let sha256 = r.array()?;
    let md5 = r.array()?;
    let resolved_hash = r.array()?;
    let random_seed = r.uint()?;
    let n = r.count(1)?;
    let mut random_results = Vec::with_capacity(n);
    for _ in 0..n {
        random_results.push(r.u32()?);
    }
    let transform = TransformRecord {
        id: r.string()?,
        version: r.u32()?,
    };
    Ok(ChartRecord {
        identity: ChartIdentity {
            sha256,
            md5,
            resolved_hash,
            loader_version,
            random_results,
        },
        random_seed,
        transform,
    })
}

fn read_preset(r: &mut Reader<'_>) -> Result<PresetRecord, DecodeError> {
    let id = r.string()?;
    let sha256: [u8; 32] = r.array()?;
    let canonical_bytes = r.bytes()?.to_vec();
    if <[u8; 32]>::from(Sha256::digest(&canonical_bytes)) != sha256 {
        return Err(DecodeError::PresetHashMismatch);
    }
    Ok(PresetRecord {
        id,
        sha256,
        canonical_bytes,
    })
}

fn read_kind(r: &mut Reader<'_>) -> Result<DeviceKind, DecodeError> {
    match r.uint()? {
        0 => Ok(DeviceKind::Keyboard),
        1 => Ok(DeviceKind::Controller),
        _ => Err(DecodeError::InvalidValue("device kind")),
    }
}

fn read_lane(r: &mut Reader<'_>) -> Result<Lane, DecodeError> {
    u8::try_from(r.uint()?)
        .ok()
        .and_then(Lane::new)
        .ok_or(DecodeError::InvalidValue("lane"))
}

fn read_devices(
    r: &mut Reader<'_>,
    version: u16,
    limits: &DecodeLimits,
) -> Result<DeviceRecord, DecodeError> {
    let n = r.uint()?;
    if n == 0 {
        return Err(DecodeError::NoSources);
    }
    if n > u64::from(limits.max_sources) {
        return Err(DecodeError::TooManySources(n));
    }
    // Kind, receive method, offset and binding count.
    let mut sources = Vec::with_capacity(r.check_count(n, 4)?);
    for _ in 0..n {
        let kind = read_kind(r)?;
        let receive = match r.uint()? {
            0 => ReceiveMethod::RawInput,
            1 => ReceiveMethod::Scripted,
            _ => return Err(DecodeError::InvalidValue("receive method")),
        };
        let input_offset = TimeUs(r.int()?);
        let nb = r.count(2)?;
        let mut bindings = Vec::with_capacity(nb);
        for _ in 0..nb {
            let control = ControlId(r.u32()?);
            let lane = read_lane(r)?;
            bindings.push(Binding { control, lane });
        }
        let axes = if version >= AXES_VERSION {
            read_axes(r)?
        } else {
            Vec::new()
        };
        sources.push(SourceRecord {
            kind,
            receive,
            profile: SourceProfile {
                input_offset,
                bindings,
                axes,
            },
        });
    }
    let audio_offset = TimeUs(r.int()?);
    let display_offset = TimeUs(r.int()?);
    Ok(DeviceRecord {
        sources,
        audio_offset,
        display_offset,
    })
}

fn read_axes(r: &mut Reader<'_>) -> Result<Vec<AxisBinding>, DecodeError> {
    // Control, lane, mode, invert, two thresholds and the release time.
    let n = r.count(7)?;
    let mut axes = Vec::with_capacity(n);
    for _ in 0..n {
        let control = ControlId(r.u32()?);
        let lane = read_lane(r)?;
        let mode = match r.uint()? {
            0 => AxisMode::Absolute { wrap: r.u32()? },
            1 => AxisMode::Relative,
            _ => return Err(DecodeError::InvalidValue("axis mode")),
        };
        let invert = match r.uint()? {
            0 => false,
            1 => true,
            _ => return Err(DecodeError::InvalidValue("axis inversion")),
        };
        let scratch = ScratchSettings {
            mode,
            invert,
            press_threshold: r.u32()?,
            hold_threshold: r.u32()?,
            release_after_us: r.u32()?,
        };
        axes.push(AxisBinding {
            control,
            lane,
            scratch,
        });
    }
    Ok(axes)
}

fn read_source(r: &mut Reader<'_>, n_sources: usize) -> Result<SourceId, DecodeError> {
    let s = SourceId(r.u16()?);
    if s.index() < n_sources {
        Ok(s)
    } else {
        Err(DecodeError::UnknownSource(s.0))
    }
}

fn read_start(r: &mut Reader<'_>, n_sources: usize) -> Result<InputStart, DecodeError> {
    let opened_at = TimeUs(r.int()?);
    let n = r.count(3)?;
    let mut held = Vec::with_capacity(n);
    for _ in 0..n {
        let source = read_source(r, n_sources)?;
        let control = ControlId(r.u32()?);
        held.push((source, control, r.i32()?));
    }
    Ok(InputStart { opened_at, held })
}

fn read_events(
    r: &mut Reader<'_>,
    limits: &DecodeLimits,
    n_sources: usize,
    opened_at: TimeUs,
) -> Result<Vec<PhysicalInputEvent>, DecodeError> {
    let n = r.uint()?;
    if n > limits.max_events {
        return Err(DecodeError::TooManyEvents(n));
    }
    // Source, delta and kind.
    let mut events = Vec::with_capacity(r.check_count(n, 3)?);
    let mut prev = vec![opened_at.0; n_sources];
    for _ in 0..n {
        let source = read_source(r, n_sources)?;
        let received = prev[source.index()].wrapping_add(r.int()?);
        prev[source.index()] = received;
        let kind = match r.uint()? {
            0 => InputKind::Control {
                control: ControlId(r.u32()?),
                value: r.i32()?,
            },
            1 => InputKind::Disconnected,
            _ => return Err(DecodeError::InvalidValue("input kind")),
        };
        events.push(PhysicalInputEvent {
            received: TimeUs(received),
            source,
            kind,
        });
    }
    Ok(events)
}

fn read_arrivals(r: &mut Reader<'_>) -> Result<Vec<Arrival>, DecodeError> {
    let n = r.count(2)?;
    let mut arrivals = Vec::with_capacity(n);
    for _ in 0..n {
        let at = TimeUs(r.int()?);
        arrivals.push(Arrival {
            at,
            kind: read_kind(r)?,
        });
    }
    Ok(arrivals)
}

fn read_status(r: &mut Reader<'_>) -> Result<Status, DecodeError> {
    let end = match r.uint()? {
        0 => EndStatus::PlayedToEnd,
        1 => EndStatus::EndedMidway,
        _ => return Err(DecodeError::InvalidValue("end status")),
    };
    let finalized = TimeUs(r.int()?);
    let bits = r.uint()?;
    if bits & !(OVERFLOW | SOURCE_LIMIT | SUSPEND) != 0 {
        return Err(DecodeError::InvalidValue("incomplete reasons"));
    }
    Ok(Status {
        end,
        finalized,
        incomplete: Incomplete {
            overflow: bits & OVERFLOW != 0,
            source_limit: bits & SOURCE_LIMIT != 0,
            suspend: bits & SUSPEND != 0,
        },
    })
}

#[cfg(test)]
mod tests;
