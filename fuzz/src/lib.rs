//! The bodies of the fuzz targets, one function per target, so the smoke
//! tests below run the seeds through the same code the fuzzer does. The
//! targets are in `fuzz_targets/`, the seed generator in [`seeds`].
//!
//! A target fails on a panic: a library panic, or a broken invariant
//! asserted here.

#![forbid(unsafe_code)]

pub mod seeds;

use std::hint::black_box;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use bms_parser::{LoadError, LoadOptions, Loaded};
use input::raw;
use replay::{DecodeError, DecodeLimits, FORMAT_VERSION, MAGIC, PlayRecord};
use rules::{CanonicalError, RulePreset};

/// An empty folder for the chart's sounds. Nothing is written to it, so
/// every `#WAV` name resolves to no file.
fn chart_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("tapestry-fuzz-chart-")
            .tempdir()
            .expect("create an empty chart folder")
    })
    .path()
}

/// Target `bms_load`: a chart file's bytes.
///
/// # Errors
///
/// What [`bms_parser::load_bms`] refuses.
pub fn bms_load(data: &[u8]) -> Result<Loaded, LoadError> {
    bms_parser::load_bms(data, chart_dir(), &LoadOptions::default())
}

/// Target `replay_file`: an input log file's bytes, header and compressed
/// body.
///
/// # Errors
///
/// What [`replay::decode`] refuses.
pub fn replay_file(data: &[u8]) -> Result<PlayRecord, DecodeError> {
    replay::decode(data, &DecodeLimits::STANDARD)
}

/// The body encoding `replay` writes: zstd. The crate keeps it private.
const ENCODING_ZSTD: u8 = 1;

/// A valid file around the uncompressed `body`, as [`replay::encode`]
/// writes it but at zstd level 1, which keeps executions fast.
///
/// # Panics
///
/// If `body` is longer than `u32::MAX` bytes.
#[must_use]
pub fn envelope(body: &[u8]) -> Vec<u8> {
    let compressed = zstd::bulk::compress(body, 1).expect("compress an in-memory buffer");
    let mut out = Vec::with_capacity(11 + compressed.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.push(ENCODING_ZSTD);
    out.extend_from_slice(
        &u32::try_from(body.len())
            .expect("a body fits in u32")
            .to_le_bytes(),
    );
    out.extend_from_slice(&compressed);
    out
}

/// Target `replay_body`: an uncompressed body, decoded from a valid file
/// around it, so every input reaches the sections, counts and varints.
///
/// # Errors
///
/// What [`replay::decode`] refuses.
///
/// # Panics
///
/// If a body decodes but [`replay::body_bytes`] of the record differs from
/// it: every record must have exactly one encoding.
pub fn replay_body(body: &[u8]) -> Result<PlayRecord, DecodeError> {
    let decoded = replay::decode(&envelope(body), &DecodeLimits::STANDARD);
    if let Ok(rec) = &decoded {
        assert!(
            replay::body_bytes(rec) == body,
            "a decoded body differs from the canonical body of its record"
        );
    }
    decoded
}

/// Target `preset_canonical`: a preset's canonical bytes.
///
/// # Errors
///
/// What [`RulePreset::from_canonical_bytes`] refuses.
///
/// # Panics
///
/// If bytes decode but the preset's canonical bytes differ from them: the
/// decoder must accept only the encoder's output.
pub fn preset_canonical(data: &[u8]) -> Result<RulePreset, CanonicalError> {
    let decoded = RulePreset::from_canonical_bytes(data);
    if let Ok(preset) = &decoded {
        assert!(
            preset.canonical_bytes() == data,
            "decoded bytes differ from the canonical bytes of their preset"
        );
    }
    decoded
}

/// What [`raw_input`] found in one input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawSummary {
    /// The block count the input asked for.
    pub count: usize,
    /// The blocks the walk returned.
    pub blocks: usize,
    /// Blocks that decoded as a keyboard packet.
    pub packets: usize,
    /// Blocks that decoded as a HID device's.
    pub hid_blocks: usize,
    /// Reports in those HID blocks.
    pub reports: usize,
}

/// Target `raw_input`: a block count byte, then a `GetRawInputBuffer`
/// buffer. Each block the walk returns is decoded as a keyboard and as a
/// HID block, and so is the whole buffer.
///
/// # Panics
///
/// If a block is shorter than a header, a key packet's value is not 0 or
/// 1, or a HID block's reports differ in length or do not fit in it.
#[must_use]
pub fn raw_input(data: &[u8]) -> RawSummary {
    let Some((&count, buf)) = data.split_first() else {
        return RawSummary::default();
    };
    let mut summary = RawSummary {
        count: usize::from(count),
        ..RawSummary::default()
    };
    for block in raw::blocks(buf, summary.count) {
        assert!(
            block.len() >= raw::HEADER_SIZE,
            "a block shorter than a header"
        );
        summary.blocks += 1;
        if decode_block(block, &mut summary) {
            summary.packets += 1;
        }
    }
    assert!(
        summary.blocks <= summary.count,
        "more blocks than asked for"
    );
    let mut whole = RawSummary::default();
    decode_block(buf, &mut whole);
    summary
}

/// Decodes `block` both ways, counting HID blocks and reports in
/// `summary`. Returns whether it is a key packet.
fn decode_block(block: &[u8], summary: &mut RawSummary) -> bool {
    let packet = raw::decode_keyboard(block);
    if let Some(p) = packet {
        assert!(
            p.value == 0 || p.value == 1,
            "a key value other than 0 or 1"
        );
    }
    if let Some(hid) = raw::decode_hid(block) {
        summary.hid_blocks += 1;
        let mut size = None;
        let mut total = 0;
        for report in hid.reports() {
            assert!(!report.is_empty(), "an empty report");
            assert!(
                *size.get_or_insert(report.len()) == report.len(),
                "reports of different sizes"
            );
            total += report.len();
            summary.reports += 1;
        }
        assert!(total <= block.len(), "reports longer than their block");
    }
    packet.is_some()
}

/// Target `canary`, for the self-test only (scripts/fuzz-selftest.sh): one
/// input per failure the runs must catch. `panic` panics, `hang` never
/// returns, `malloc` makes one 128 MiB allocation, and `rss` holds 512 MiB
/// of small, written allocations for 5 s, long enough for the once a second
/// RSS check. Any other input returns.
///
/// # Panics
///
/// On the input `panic`.
pub fn canary(data: &[u8]) {
    match data {
        b"panic" => panic!("canary: panic"),
        b"hang" => loop {
            black_box(());
        },
        b"malloc" => {
            black_box(vec![1u8; 128 << 20]);
        }
        b"rss" => {
            let held: Vec<Vec<u8>> = (0..512).map(|_| vec![1u8; 1 << 20]).collect();
            black_box(&held);
            std::thread::sleep(Duration::from_secs(5));
            black_box(&held);
        }
        _ => {}
    }
}

#[cfg(test)]
mod smoke;
