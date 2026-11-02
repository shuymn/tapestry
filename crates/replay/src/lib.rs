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
//! [`FORMAT_VERSION`]. Before the first release the format may change, and
//! the decoder refuses every version but [`FORMAT_VERSION`]: older records
//! are refused, not converted.
//!
//! The session section is the session policy's id, the play type (0 the
//! whole chart, 1 a mid-chart start, 2 a round of section repeat), for a
//! practice round its conditions, and the speed as k of `k/20` (10 to 30;
//! anything else is refused). A practice round's conditions are the
//! section's start and end and its lead-in (signed, in the record's time),
//! the attempt number, and the initial gauges (a count, then each gauge's
//! ID and value); a repeated gauge ID is refused.
//!
//! Each source of the devices section is its kind, receive method, input
//! offset, its bindings (a count, then each control and lane), and its
//! axis bindings (a count, then each axis's control, lane, mode (0
//! absolute, followed by its wrap; 1 relative), press threshold, hold
//! threshold and release time in µs, all unsigned), and the number of
//! axis values it reported outside the range their conversion uses. The
//! audio and display offsets follow the sources, then the input thread's
//! wake policy: 0 woken by input, followed by its heartbeat bound, input
//! gap and drain cut; 1 timer only, followed by its period.
//!
//! Bindings and axes are bounded per source and in all by the evaluator's
//! limits ([`eval::MAX_BINDINGS_PER_SOURCE`], [`eval::MAX_AXES_PER_SOURCE`],
//! [`eval::MAX_BINDINGS`], [`eval::MAX_AXES`]), checked as each count is
//! read, before anything is allocated for it.
//!
//! The status section is the end status (0 played to the end, 1 ended
//! midway, 2 cancelled before playback), then for a played attempt the
//! finalized watermark, then the incomplete reasons as bits. A capture
//! cancelled before playback has no winning opening: its start, events
//! and arrivals sections are empty (no bytes at all), and its status holds
//! no watermark.
//!
//! The history section ([`ReadyHistory`]) is the count of cancelled
//! openings, then each opening's start snapshot (as the start section
//! writes it), origin, closing time, event range and arrival range (start
//! and end each) and cancel reason (0 held, 1 quit, 2 retry, 3 close, 4
//! focus lost, 5 failed, 6 exhausted); then the events of every opening,
//! counted once, each opening's events as the events section writes them
//! from its own opening time; then the arrivals, counted once. The ranges
//! tile the arenas in order. Origins rise strictly, and each opening opens
//! no later than it closes and closes no later than the next one opens, on
//! the last opening's timeline. A played record's openings were all
//! cancelled by a hold, and the last closes no later than the winning
//! opening opens, whose origin (0) lies after every cancelled one; a
//! capture cancelled before playback has at least one opening, the last
//! at origin 0, and every one but the last was cancelled by a hold. The
//! counts are checked against [`MAX_CANCELLED_OPENINGS`],
//! [`MAX_CANCELLED_EVENTS`] and [`MAX_OPENING_ARRIVALS`], and each range
//! as it is read, before anything is allocated for them.

#![forbid(unsafe_code)]

mod error;
mod record;
pub mod verify;
mod wire;

use std::ops::Range;

use bms_model::{ChartIdentity, Lane, Speed, TimeUs};
use eval::{
    AxisBinding, AxisMode, Binding, ControlId, InputKind, InputStart, PhysicalInputEvent,
    ScratchSettings, SourceId, SourceProfile,
};
use rules::{GaugeValue, RulePreset};
use sha2::{Digest, Sha256};

pub use error::{DecodeError, Section};
pub use record::{
    Arrival, BuildProvenance, CancelReason, CancelledOpening, ChartRecord, DeviceKind,
    DeviceRecord, EndStatus, Incomplete, InitialGauge, PASSTHROUGH, PlayRecord, PlayType, Practice,
    PresetRecord, ReadyHistory, ReceiveMethod, SourceRecord, Status, TransformId, TransformRecord,
    Versions, WakePolicy,
};

use wire::{Reader, Writer};

/// The format version this crate writes, and the only one it reads.
pub const FORMAT_VERSION: u16 = 8;

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
    /// The limits for records the app saves: 64 MiB either side of compression,
    /// 64 sources, and 4 Mi events. The game caps a play's recording so that
    /// its record always fits (`game`'s recording policy, design.md "Recording
    /// Capacity").
    pub const STANDARD: Self = Self {
        max_compressed: 64 << 20,
        max_uncompressed: 64 << 20,
        max_sources: 64,
        max_events: 4 << 20,
    };
}

/// The most bytes one input event takes in the body: its source (1 byte
/// below 128 sources, and [`DecodeLimits::STANDARD`] allows 64), its time
/// delta (10), its kind tag (1), its control (5) and its value (5). A
/// disconnect takes 12.
pub const MAX_EVENT_BYTES: usize = 22;

/// Body bytes kept for everything but the events and the history in
/// [`max_record_events`]: versions, chart, preset, devices (at most 64
/// sources, each within the evaluator's binding limits, about 120 KiB),
/// the snapshot (bounded by the bindings), arrivals and status.
pub const RECORD_OVERHEAD_BYTES: usize = 1 << 20;

/// The most READY openings a record's history holds (design.md "Recording
/// Capacity").
pub const MAX_CANCELLED_OPENINGS: usize = 64;
/// The most events of all its cancelled openings together.
pub const MAX_CANCELLED_EVENTS: usize = 65_536;
/// The most arrivals one cancelled opening keeps.
pub const MAX_OPENING_ARRIVALS: usize = 16;
/// The most controls one start snapshot names: what a profile may bind.
pub const MAX_HELD: usize = eval::MAX_BINDINGS + eval::MAX_AXES;

/// The most body bytes a full history takes: per opening its fixed fields
/// (at most 64 bytes), a full snapshot (13 bytes a control) and its
/// arrivals (11 bytes each), the events at [`MAX_EVENT_BYTES`], and three
/// counts.
pub const HISTORY_MAX_BYTES: usize = MAX_CANCELLED_OPENINGS
    * (64 + MAX_HELD * 13 + MAX_OPENING_ARRIVALS * 11)
    + MAX_CANCELLED_EVENTS * MAX_EVENT_BYTES
    + 3 * 10;

/// The most input events a record may hold and still decode under
/// `limits`: within its event count, and with every event at
/// [`MAX_EVENT_BYTES`], within its uncompressed body after
/// [`RECORD_OVERHEAD_BYTES`] and a full history ([`HISTORY_MAX_BYTES`]).
#[must_use]
pub fn max_record_events(limits: &DecodeLimits) -> usize {
    let bytes = (limits.max_uncompressed as usize)
        .saturating_sub(RECORD_OVERHEAD_BYTES + HISTORY_MAX_BYTES);
    usize::try_from(limits.max_events)
        .unwrap_or(usize::MAX)
        .min(bytes / MAX_EVENT_BYTES)
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self::STANDARD
    }
}

/// SHA-256 of the preset's canonical bytes, which identifies it in records,
/// hashed as they are written: it allocates nothing, so evaluating a record
/// under a preset of long names needs no buffer for them.
#[must_use]
pub fn preset_hash(p: &RulePreset) -> [u8; 32] {
    let mut h = Sha256::new();
    p.write_canonical(|b| h.update(b));
    h.finalize().into()
}

/// Encodes `rec` as a file.
///
/// # Panics
///
/// If the body or one of its sections exceeds `u32::MAX` bytes, which the
/// recording capacities rule out, or as [`body_bytes`].
#[must_use]
pub fn encode(rec: &PlayRecord) -> Vec<u8> {
    let body = body_bytes(rec);
    let compressed = zstd::bulk::compress(&body, ZSTD_LEVEL)
        .expect("zstd compression of an in-memory buffer does not fail");
    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.push(ENCODING_ZSTD);
    out.extend_from_slice(&u32_len(body.len()).to_le_bytes());
    out.extend_from_slice(&compressed);
    out
}

/// The canonical uncompressed body of `rec`. A capture cancelled before
/// playback writes no winning opening: its `start`, `events`, `arrivals`
/// and finalized watermark are not written.
///
/// # Panics
///
/// If a section exceeds `u32::MAX` bytes, or a cancelled opening's event
/// range does not lie in the history's arena.
#[must_use]
pub fn body_bytes(rec: &PlayRecord) -> Vec<u8> {
    let PlayRecord {
        versions,
        chart,
        preset,
        session_policy,
        devices,
        play_type,
        speed,
        start,
        events,
        arrivals,
        status,
        history,
    } = rec;
    let played = status.end.played();
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
        match play_type {
            PlayType::Full => w.uint(0),
            PlayType::MidChartStart(p) => {
                w.uint(1);
                write_practice(w, p);
            }
            PlayType::SectionRepeat(p) => {
                w.uint(2);
                write_practice(w, p);
            }
        }
        w.uint(u64::from(speed.twentieths()));
    });
    section(&|w| write_devices(w, devices));
    section(&|w| {
        if played {
            write_start(w, start);
        }
    });
    section(&|w| {
        if played {
            w.len(events.len());
            write_events(w, start.opened_at, events);
        }
    });
    section(&|w| {
        if played {
            write_arrivals(w, arrivals);
        }
    });
    section(&|w| write_status(w, status));
    section(&|w| write_history(w, history));
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
                played_hash,
                loader_version: _, // in the versions section
                random_results,
            },
        random_seed,
        transform: TransformRecord { id, version },
    } = c;
    w.raw(sha256);
    w.raw(md5);
    w.raw(resolved_hash);
    w.raw(played_hash);
    w.uint(*random_seed);
    w.len(random_results.len());
    for r in random_results {
        w.uint(u64::from(*r));
    }
    w.str(id);
    w.uint(u64::from(*version));
}

fn write_practice(w: &mut Writer, p: &Practice) {
    let Practice {
        start,
        end,
        lead_in,
        attempt,
        initial_gauges,
    } = p;
    w.int(start.0);
    w.int(end.0);
    w.int(lead_in.0);
    w.uint(u64::from(*attempt));
    w.len(initial_gauges.len());
    for InitialGauge { id, value } in initial_gauges {
        w.str(id);
        w.int(value.0);
    }
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

fn write_devices(w: &mut Writer, d: &DeviceRecord) {
    let DeviceRecord {
        sources,
        audio_offset,
        display_offset,
        wake,
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
            axis_out_of_range,
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
        write_axes(w, axes);
        w.uint(*axis_out_of_range);
    }
    w.int(audio_offset.0);
    w.int(display_offset.0);
    match *wake {
        WakePolicy::Input {
            heartbeat_us,
            input_gap_us,
            drain_cut,
        } => {
            w.uint(0);
            w.uint(u64::from(heartbeat_us));
            w.uint(u64::from(input_gap_us));
            w.uint(u64::from(drain_cut));
        }
        WakePolicy::Timer { heartbeat_us } => {
            w.uint(1);
            w.uint(u64::from(heartbeat_us));
        }
    }
}

fn write_axes(w: &mut Writer, axes: &[AxisBinding]) {
    w.len(axes.len());
    for AxisBinding {
        control,
        lane,
        scratch:
            ScratchSettings {
                mode,
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

/// `events` without their count, each source's first counting from
/// `opened_at`.
fn write_events(w: &mut Writer, opened_at: TimeUs, events: &[PhysicalInputEvent]) {
    // The previous time per source; a source's first event counts from
    // `opened_at`. Wrapping arithmetic keeps every `i64` pair exact.
    let mut prev: Vec<i64> = Vec::new();
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

const fn reason_tag(r: CancelReason) -> u64 {
    match r {
        CancelReason::Held => 0,
        CancelReason::Quit => 1,
        CancelReason::Retry => 2,
        CancelReason::Close => 3,
        CancelReason::FocusLost => 4,
        CancelReason::Failed => 5,
        CancelReason::Exhausted => 6,
    }
}

fn write_history(w: &mut Writer, h: &ReadyHistory) {
    let ReadyHistory {
        openings,
        events,
        arrivals,
    } = h;
    w.len(openings.len());
    for CancelledOpening {
        start,
        origin,
        closed,
        events,
        arrivals,
        reason,
    } in openings
    {
        write_start(w, start);
        w.int(origin.0);
        w.int(closed.0);
        for r in [events, arrivals] {
            w.uint(u64::from(r.start));
            w.uint(u64::from(r.end));
        }
        w.uint(reason_tag(*reason));
    }
    w.len(events.len());
    for o in openings {
        let run = events
            .get(o.events.start as usize..o.events.end as usize)
            .expect("a cancelled opening's events lie in the history's arena");
        write_events(w, o.start.opened_at, run);
    }
    write_arrivals(w, arrivals);
}

const OVERFLOW: u64 = 1;
const SOURCE_LIMIT: u64 = 1 << 1;
const SUSPEND: u64 = 1 << 2;
const INPUT_FAILED: u64 = 1 << 3;

fn write_status(w: &mut Writer, s: &Status) {
    let Status {
        end,
        finalized,
        incomplete:
            Incomplete {
                overflow,
                source_limit,
                suspend,
                input_failed,
            },
    } = s;
    w.uint(match end {
        EndStatus::PlayedToEnd => 0,
        EndStatus::EndedMidway => 1,
        EndStatus::CancelledBeforePlayback => 2,
    });
    if end.played() {
        w.int(finalized.0);
    }
    let bit = |set: bool, bit: u64| if set { bit } else { 0 };
    w.uint(
        bit(*overflow, OVERFLOW)
            | bit(*source_limit, SOURCE_LIMIT)
            | bit(*suspend, SUSPEND)
            | bit(*input_failed, INPUT_FAILED),
    );
}

/// Decodes a file written by [`encode`] in [`FORMAT_VERSION`]; every other
/// version is refused, older ones included.
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
    if version != FORMAT_VERSION {
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
    decode_body(&body, limits)
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

fn decode_body(body: &[u8], limits: &DecodeLimits) -> Result<PlayRecord, DecodeError> {
    let mut r = Reader::new(body);
    let (versions, loader) = read_section(&mut r, Section::Versions, read_versions)?;
    let chart = read_section(&mut r, Section::Chart, |r| read_chart(r, loader))?;
    let preset = read_section(&mut r, Section::Preset, read_preset)?;
    let (session_policy, play_type, speed) = read_section(&mut r, Section::Session, |r| {
        let policy = r.string()?;
        let play_type = match r.uint()? {
            0 => PlayType::Full,
            1 => PlayType::MidChartStart(read_practice(r)?),
            2 => PlayType::SectionRepeat(read_practice(r)?),
            _ => return Err(DecodeError::InvalidValue("play type")),
        };
        let speed = u32::try_from(r.uint()?)
            .ok()
            .and_then(Speed::from_twentieths)
            .ok_or(DecodeError::InvalidValue("speed"))?;
        Ok((policy, play_type, speed))
    })?;
    let devices = read_section(&mut r, Section::Devices, |r| read_devices(r, limits))?;
    let n_sources = devices.sources.len();
    // The winning opening's sections are read once the status says there
    // is one; a capture cancelled before playback leaves them empty.
    let mut start_r = section(&mut r, Section::Start)?;
    let mut events_r = section(&mut r, Section::Events)?;
    let mut arrivals_r = section(&mut r, Section::Arrivals)?;
    let status = read_section(&mut r, Section::Status, read_status)?;
    let (start, events, arrivals) = if status.end.played() {
        let start = read_start(&mut start_r, n_sources)?;
        start_r.finish()?;
        let n = events_r.uint()?;
        if n > limits.max_events {
            return Err(DecodeError::TooManyEvents(n));
        }
        // Source, delta and kind.
        let n = events_r.check_count(n, 3)?;
        let mut events = Vec::with_capacity(n);
        read_events(&mut events_r, n, n_sources, start.opened_at, &mut events)?;
        events_r.finish()?;
        let arrivals = read_arrivals(&mut arrivals_r)?;
        arrivals_r.finish()?;
        (start, events, arrivals)
    } else {
        for sub in [&start_r, &events_r, &arrivals_r] {
            sub.finish()?;
        }
        (InputStart::default(), Vec::new(), Vec::new())
    };
    let history = read_section(&mut r, Section::History, |r| {
        read_history(r, n_sources, status.end, start.opened_at)
    })?;
    r.finish()?;
    Ok(PlayRecord {
        versions,
        chart,
        preset,
        session_policy,
        devices,
        play_type,
        speed,
        start,
        events,
        arrivals,
        status,
        history,
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
    let played_hash = r.array()?;
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
            played_hash,
            loader_version,
            random_results,
        },
        random_seed,
        transform,
    })
}

/// The fewest bytes an initial gauge takes: an empty ID's length and a
/// one-byte value.
const MIN_INITIAL_GAUGE_BYTES: usize = 2;

fn read_practice(r: &mut Reader<'_>) -> Result<Practice, DecodeError> {
    let start = TimeUs(r.int()?);
    let end = TimeUs(r.int()?);
    let lead_in = TimeUs(r.int()?);
    let attempt = r.u32()?;
    let n = r.count(MIN_INITIAL_GAUGE_BYTES)?;
    let mut initial_gauges = Vec::with_capacity(n);
    for _ in 0..n {
        let id = r.string()?;
        let value = GaugeValue(r.int()?);
        initial_gauges.push(InitialGauge { id, value });
    }
    // Gauges are taken by ID, so an ID names one value.
    let mut ids: Vec<&str> = initial_gauges.iter().map(|g| g.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(DecodeError::InvalidValue("initial gauge ID"));
    }
    Ok(Practice {
        start,
        end,
        lead_in,
        attempt,
        initial_gauges,
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

fn read_devices(r: &mut Reader<'_>, limits: &DecodeLimits) -> Result<DeviceRecord, DecodeError> {
    let n = r.uint()?;
    if n == 0 {
        return Err(DecodeError::NoSources);
    }
    if n > u64::from(limits.max_sources) {
        return Err(DecodeError::TooManySources(n));
    }
    // Kind, receive method, offset and binding count.
    let mut sources = Vec::with_capacity(r.check_count(n, 4)?);
    let (mut all_bindings, mut all_axes) = (0, 0);
    for _ in 0..n {
        let kind = read_kind(r)?;
        let receive = match r.uint()? {
            0 => ReceiveMethod::RawInput,
            1 => ReceiveMethod::Scripted,
            _ => return Err(DecodeError::InvalidValue("receive method")),
        };
        let input_offset = TimeUs(r.int()?);
        let nb = r.count(2)?;
        all_bindings += nb;
        if nb > eval::MAX_BINDINGS_PER_SOURCE || all_bindings > eval::MAX_BINDINGS {
            return Err(DecodeError::TooManyBindings);
        }
        let mut bindings = Vec::with_capacity(nb);
        for _ in 0..nb {
            let control = ControlId(r.u32()?);
            let lane = read_lane(r)?;
            bindings.push(Binding { control, lane });
        }
        let axes = read_axes(r, &mut all_axes)?;
        let axis_out_of_range = r.uint()?;
        sources.push(SourceRecord {
            kind,
            receive,
            profile: SourceProfile {
                input_offset,
                bindings,
                axes,
            },
            axis_out_of_range,
        });
    }
    let audio_offset = TimeUs(r.int()?);
    let display_offset = TimeUs(r.int()?);
    let wake = match r.uint()? {
        0 => WakePolicy::Input {
            heartbeat_us: r.u32()?,
            input_gap_us: r.u32()?,
            drain_cut: r.u32()?,
        },
        1 => WakePolicy::Timer {
            heartbeat_us: r.u32()?,
        },
        _ => return Err(DecodeError::InvalidValue("wake policy")),
    };
    Ok(DeviceRecord {
        sources,
        audio_offset,
        display_offset,
        wake,
    })
}

/// One source's axes; `all` counts the axes of every source so far.
fn read_axes(r: &mut Reader<'_>, all: &mut usize) -> Result<Vec<AxisBinding>, DecodeError> {
    // Control, lane, mode, two thresholds and the release time.
    let n = r.count(6)?;
    *all += n;
    if n > eval::MAX_AXES_PER_SOURCE || *all > eval::MAX_AXES {
        return Err(DecodeError::TooManyBindings);
    }
    let mut axes = Vec::with_capacity(n);
    for _ in 0..n {
        let control = ControlId(r.u32()?);
        let lane = read_lane(r)?;
        let mode = match r.uint()? {
            0 => AxisMode::Absolute { wrap: r.u32()? },
            1 => AxisMode::Relative,
            _ => return Err(DecodeError::InvalidValue("axis mode")),
        };
        let scratch = ScratchSettings {
            mode,
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

/// The held-input snapshot: at most as many controls per source, and in
/// all, as the evaluator lets a profile bind (buttons and axes), each named
/// once; the count is checked before anything is allocated for it.
fn read_start(r: &mut Reader<'_>, n_sources: usize) -> Result<InputStart, DecodeError> {
    read_snapshot(r, n_sources, &mut Vec::new(), true)
}

/// [`read_start`] with `named` as the scratch its checks sort: the
/// snapshot's controls are kept only when `keep` holds.
fn read_snapshot(
    r: &mut Reader<'_>,
    n_sources: usize,
    named: &mut Vec<(SourceId, ControlId)>,
    keep: bool,
) -> Result<InputStart, DecodeError> {
    let opened_at = TimeUs(r.int()?);
    let n = r.count(3)?;
    if n > MAX_HELD {
        return Err(DecodeError::TooManyHeld);
    }
    let mut held = if keep {
        Vec::with_capacity(n)
    } else {
        Vec::new()
    };
    named.clear();
    named.reserve(n);
    for _ in 0..n {
        let source = read_source(r, n_sources)?;
        let control = ControlId(r.u32()?);
        let value = r.i32()?;
        named.push((source, control));
        if keep {
            held.push((source, control, value));
        }
    }
    named.sort_unstable();
    let per_source = eval::MAX_BINDINGS_PER_SOURCE + eval::MAX_AXES_PER_SOURCE;
    let repeated = named.windows(2).any(|w| w[0] == w[1]);
    let crowded = named
        .chunk_by(|a, b| a.0 == b.0)
        .any(|controls| controls.len() > per_source);
    if repeated || crowded {
        return Err(DecodeError::TooManyHeld);
    }
    Ok(InputStart { opened_at, held })
}

/// Reads `n` events onto `events`, each source's first counting from
/// `opened_at`; the caller checked `n` and reserved room for them.
fn read_events(
    r: &mut Reader<'_>,
    n: usize,
    n_sources: usize,
    opened_at: TimeUs,
    events: &mut Vec<PhysicalInputEvent>,
) -> Result<(), DecodeError> {
    let mut prev = vec![opened_at.0; n_sources];
    for _ in 0..n {
        events.push(read_event(r, n_sources, &mut prev)?);
    }
    Ok(())
}

/// Reads one event, its time counted from its source's last in `prev`,
/// which it replaces.
fn read_event(
    r: &mut Reader<'_>,
    n_sources: usize,
    prev: &mut [i64],
) -> Result<PhysicalInputEvent, DecodeError> {
    let source = read_source(r, n_sources)?;
    let last = &mut prev[source.index()];
    *last = last.wrapping_add(r.int()?);
    let received = TimeUs(*last);
    let kind = match r.uint()? {
        0 => InputKind::Control {
            control: ControlId(r.u32()?),
            value: r.i32()?,
        },
        1 => InputKind::Disconnected,
        _ => return Err(DecodeError::InvalidValue("input kind")),
    };
    Ok(PhysicalInputEvent {
        received,
        source,
        kind,
    })
}

fn read_arrivals(r: &mut Reader<'_>) -> Result<Vec<Arrival>, DecodeError> {
    let n = r.count(2)?;
    let mut arrivals = Vec::with_capacity(n);
    for _ in 0..n {
        arrivals.push(read_arrival(r)?);
    }
    Ok(arrivals)
}

fn read_arrival(r: &mut Reader<'_>) -> Result<Arrival, DecodeError> {
    let at = TimeUs(r.int()?);
    Ok(Arrival {
        at,
        kind: read_kind(r)?,
    })
}

fn read_status(r: &mut Reader<'_>) -> Result<Status, DecodeError> {
    let end = match r.uint()? {
        0 => EndStatus::PlayedToEnd,
        1 => EndStatus::EndedMidway,
        2 => EndStatus::CancelledBeforePlayback,
        _ => return Err(DecodeError::InvalidValue("end status")),
    };
    let finalized = if end.played() {
        TimeUs(r.int()?)
    } else {
        TimeUs(0)
    };
    let bits = r.uint()?;
    if bits & !(OVERFLOW | SOURCE_LIMIT | SUSPEND | INPUT_FAILED) != 0 {
        return Err(DecodeError::InvalidValue("incomplete reasons"));
    }
    Ok(Status {
        end,
        finalized,
        incomplete: Incomplete {
            overflow: bits & OVERFLOW != 0,
            source_limit: bits & SOURCE_LIMIT != 0,
            suspend: bits & SUSPEND != 0,
            input_failed: bits & INPUT_FAILED != 0,
        },
    })
}

/// The fewest bytes a cancelled opening takes: its opening time, held
/// count, origin, closing time, four range bounds and reason.
const MIN_OPENING_BYTES: usize = 9;

fn read_reason(r: &mut Reader<'_>) -> Result<CancelReason, DecodeError> {
    Ok(match r.uint()? {
        0 => CancelReason::Held,
        1 => CancelReason::Quit,
        2 => CancelReason::Retry,
        3 => CancelReason::Close,
        4 => CancelReason::FocusLost,
        5 => CancelReason::Failed,
        6 => CancelReason::Exhausted,
        _ => return Err(DecodeError::InvalidValue("cancel reason")),
    })
}

/// A range of an arena, which starts where the previous one ended, ends
/// at most at `max` and holds at most `per`.
fn read_range(
    r: &mut Reader<'_>,
    from: u32,
    per: usize,
    max: usize,
) -> Result<Range<u32>, DecodeError> {
    let (start, end) = (r.u32()?, r.u32()?);
    if start != from || end < start {
        return Err(DecodeError::InvalidHistory("range"));
    }
    if end as usize > max || (end - start) as usize > per {
        return Err(DecodeError::TooManyHistoryEvents(u64::from(end)));
    }
    Ok(start..end)
}

/// What the history's checks keep of one opening, so nothing is
/// allocated for them.
#[derive(Clone, Copy, Default)]
struct Span {
    opened_at: TimeUs,
    origin: TimeUs,
    closed: TimeUs,
    /// How many events it holds.
    events: u32,
    /// How many arrivals it holds.
    arrivals: u32,
    /// Whether a hold cancelled it.
    held: bool,
}

/// The history section of a record whose end status is `end` and whose
/// winning opening opened at `opened_at`. It is checked whole before
/// anything is allocated for it: a first pass over a copy of `r`, with
/// scratch bounded by the format's limits and `n_sources`, checks every
/// count, range, source and time and the agreement with the end status;
/// a second pass then builds it.
fn read_history(
    r: &mut Reader<'_>,
    n_sources: usize,
    end: EndStatus,
    opened_at: TimeUs,
) -> Result<ReadyHistory, DecodeError> {
    walk_history(&mut r.clone(), n_sources, end, opened_at, None)?;
    let mut history = ReadyHistory::default();
    walk_history(r, n_sources, end, opened_at, Some(&mut history))?;
    Ok(history)
}

/// One pass of [`read_history`], building the history into `out` if
/// given.
fn walk_history(
    r: &mut Reader<'_>,
    n_sources: usize,
    end: EndStatus,
    opened_at: TimeUs,
    mut out: Option<&mut ReadyHistory>,
) -> Result<(), DecodeError> {
    let n = r.uint()?;
    if n > MAX_CANCELLED_OPENINGS as u64 {
        return Err(DecodeError::TooManyOpenings(n));
    }
    let n = r.check_count(n, MIN_OPENING_BYTES)?;
    if let Some(h) = out.as_deref_mut() {
        h.openings.reserve_exact(n);
    }
    let mut spans = [Span::default(); MAX_CANCELLED_OPENINGS];
    let mut named = Vec::new();
    let (mut events_end, mut arrivals_end) = (0, 0);
    for i in 0..n {
        let start = read_snapshot(r, n_sources, &mut named, out.is_some())?;
        let origin = TimeUs(r.int()?);
        let closed = TimeUs(r.int()?);
        let events = read_range(r, events_end, MAX_CANCELLED_EVENTS, MAX_CANCELLED_EVENTS)?;
        let arrivals = read_range(
            r,
            arrivals_end,
            MAX_OPENING_ARRIVALS,
            MAX_CANCELLED_OPENINGS * MAX_OPENING_ARRIVALS,
        )?;
        let reason = read_reason(r)?;
        if closed < start.opened_at {
            return Err(DecodeError::InvalidHistory("closing time"));
        }
        if let Some(prev) = spans[..i].last() {
            if origin <= prev.origin {
                return Err(DecodeError::InvalidHistory("origin"));
            }
            if !closes_before(prev.origin, prev.closed, origin, start.opened_at) {
                return Err(DecodeError::InvalidHistory("chronology"));
            }
        }
        spans[i] = Span {
            opened_at: start.opened_at,
            origin,
            closed,
            events: events.end - events.start,
            arrivals: arrivals.end - arrivals.start,
            held: reason == CancelReason::Held,
        };
        (events_end, arrivals_end) = (events.end, arrivals.end);
        if let Some(h) = out.as_deref_mut() {
            h.openings.push(CancelledOpening {
                start,
                origin,
                closed,
                events,
                arrivals,
                reason,
            });
        }
    }
    let spans = &spans[..n];
    check_history(spans, end, opened_at)?;
    if r.uint()? != u64::from(events_end) {
        return Err(DecodeError::InvalidHistory("event count"));
    }
    // Source, delta and kind.
    let total = r.check_count(u64::from(events_end), 3)?;
    if let Some(h) = out.as_deref_mut() {
        h.events.reserve_exact(total);
    }
    let mut prev = vec![0; n_sources];
    for s in spans {
        prev.fill(s.opened_at.0);
        for _ in 0..s.events {
            let event = read_event(r, n_sources, &mut prev)?;
            if let Some(h) = out.as_deref_mut() {
                h.events.push(event);
            }
        }
    }
    if r.uint()? != u64::from(arrivals_end) {
        return Err(DecodeError::InvalidHistory("arrival count"));
    }
    // Time and kind.
    let total = r.check_count(u64::from(arrivals_end), 2)?;
    if let Some(h) = out.as_deref_mut() {
        h.arrivals.reserve_exact(total);
    }
    for s in spans {
        for _ in 0..s.arrivals {
            let arrival = read_arrival(r)?;
            if let Some(h) = out.as_deref_mut() {
                h.arrivals.push(arrival);
            }
        }
    }
    Ok(())
}

/// Whether an opening at `origin` closing at `closed` closed no later than
/// one at `next_origin` opening at `opened` opened, on the last opening's
/// timeline; computed wide, so no sum overflows.
fn closes_before(origin: TimeUs, closed: TimeUs, next_origin: TimeUs, opened: TimeUs) -> bool {
    i128::from(origin.0) + i128::from(closed.0) <= i128::from(next_origin.0) + i128::from(opened.0)
}

/// The history's agreement with the end status: a played record's
/// openings were cancelled by a hold and lie before the winning opening
/// (origin 0) opened at `opened_at`; a capture cancelled before playback
/// has at least one, the last at origin 0, every earlier one cancelled by
/// a hold.
fn check_history(spans: &[Span], end: EndStatus, opened_at: TimeUs) -> Result<(), DecodeError> {
    if end.played() {
        if !spans.iter().all(|s| s.held) {
            return Err(DecodeError::InvalidHistory("cancel reason"));
        }
        if let Some(last) = spans.last() {
            if last.origin >= TimeUs(0) {
                return Err(DecodeError::InvalidHistory("origin"));
            }
            if !closes_before(last.origin, last.closed, TimeUs(0), opened_at) {
                return Err(DecodeError::InvalidHistory("chronology"));
            }
        }
        return Ok(());
    }
    let Some((last, earlier)) = spans.split_last() else {
        return Err(DecodeError::InvalidHistory("no opening"));
    };
    if last.origin != TimeUs(0) {
        return Err(DecodeError::InvalidHistory("origin"));
    }
    if !earlier.iter().all(|s| s.held) {
        return Err(DecodeError::InvalidHistory("cancel reason"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
