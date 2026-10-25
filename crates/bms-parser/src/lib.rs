//! BMS chart loader: chart text to a resolved [`Chart`] in integer µs.
//!
//! Supports `#PLAYER 1` 7-key + scratch charts. Charts and sounds are
//! untrusted: every limit in [`Limits`] is checked before the work it bounds,
//! times use exact rationals with checked arithmetic, and sound files are
//! confined to the chart folder. This crate does not use floating point.
//!
//! # Loader version 1
//!
//! These rules are fixed for [`LOADER_VERSION`] 1; changing any of them
//! changes the version.
//!
//! - **Time:** each object's time is accumulated in exact rationals and
//!   floored to µs once. BPM, `#STOPxx` and measure-length values take at
//!   most 6 fractional digits. A `#STOPxx` value counts 1/192 of a 4/4
//!   measure (1/48 beat) whatever the measure length.
//! - **Same position:** a BPM change applies before a STOP, so the STOP uses
//!   the new BPM; a note at a STOP's position sounds at the STOP's start.
//!   Channel 08 wins over channel 03; within a channel the later object in
//!   the file wins. STOPs at one position add up.
//! - **Lanes:** channels 16 (scratch), 11-15 and 18-19 (keys 1-7); 51-59
//!   long notes (`#LNTYPE 1`, consecutive objects pair up); D1-D9 mines,
//!   with the object id as damage. Channel 17 and other unused channels are
//!   ignored with a warning; BGA channels 04, 06 and 07 silently. A 2P lane
//!   object (21-29, 61-69, E1-E9) is an `Unsupported` error.
//! - **`#LNOBJ`:** a channel 1x object with that id ends a long note started
//!   by the previous plain note in its lane. The end plays no sound.
//! - **`#LNTYPE 1` pairing:** endpoints at one time in a lane merge first,
//!   keeping the later object in the file, so a duplicated line does not
//!   split a long note. The end object's sound is the end sound when its id
//!   differs from the start's, otherwise there is none.
//! - **Conflicts in a lane:** at one time one note is kept: a long note over
//!   a tap over a mine, and the later object in the file among equals. A note
//!   at or inside the span of an earlier long note in its lane, mines
//!   included, is dropped. Each dropped note is a warning.
//! - **Sounds:** the table holds every `#WAV` in id order. A reference to an
//!   undefined id plays nothing (BGM events are dropped). A name is tried as
//!   written, then with the extensions `.wav .ogg .flac .mp3`.
//! - **Empty charts:** a chart with no notes loads, with a
//!   [`WarningKind::NoPlayableNotes`] warning; whether to play it is the
//!   caller's decision.
//! - **`#RANDOM n`:** draws `next() % n + 1` from SplitMix64 seeded with
//!   [`LoadOptions::seed`]; `#SETRANDOM n` sets `n`. Every value of a block
//!   in a taken branch is recorded in [`ChartIdentity::random_results`] in
//!   file order, whether or not an `#IF` matches it. A block in a branch not
//!   taken draws nothing. `n` must be a positive integer (an error
//!   otherwise, since the chart's branches would be undefined).
//! - **Blocks:** every line inside an `#IF` branch not taken is skipped,
//!   headers (`#WAVxx`, `#BPMxx`, `#LNOBJ`, ...) included. `#IF k` is taken
//!   when the nearest enclosing block's value is `k`; `#ELSE` when no
//!   earlier branch of its `#IF` was. A `#RANDOM` directly inside another
//!   ends it, since `#ENDRANDOM` is often left out; `#ENDIF` also closes
//!   blocks left open inside it. `#IF` outside any block is never taken, and
//!   a stray `#ELSE`, `#ENDIF` or `#ENDRANDOM` is ignored, each with a
//!   warning: the base chart stays playable. An `#IF` still open at the end
//!   of the file ends there, with a warning. Blocks nest at most
//!   [`Limits::max_nesting`] deep (an error).
//! - **Unsupported:** `#LNTYPE 2`, `#LNMODE` other than 1, negative
//!   `#STOPxx`, `#ELSEIF`, and `#SWITCH`/`#CASE`/`#SKIP`/`#DEF`/`#ENDSW`.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

mod branch;
mod decimal;
mod error;
mod hash;
mod parse;
mod resolve;
mod sound;
mod timing;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bms_model::{Chart, ChartIdentity, ScrollMap, SoundId, SoundRef, TimeUs};

pub use error::{LoadError, Warning, WarningKind};

/// Version of the loader rules recorded in [`ChartIdentity::loader_version`].
pub const LOADER_VERSION: u32 = 1;

/// Bounds on the work a chart may cause (D14). The defaults sit far above
/// real charts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest file accepted, in bytes.
    pub max_file_bytes: usize,
    /// Most lines accepted.
    pub max_lines: usize,
    /// Most non-`00` objects accepted.
    pub max_objects: usize,
    /// Highest measure number accepted.
    pub max_measure: u16,
    /// Longest resolved chart accepted.
    pub max_duration: TimeUs,
    /// Deepest nesting of open `#RANDOM` and `#IF` blocks, each counting one.
    pub max_nesting: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * 1024 * 1024,
            max_lines: 500_000,
            max_objects: 500_000,
            max_measure: 999,
            max_duration: TimeUs(60 * 60 * 1_000_000),
            max_nesting: 16,
        }
    }
}

/// Options for [`load_bms`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadOptions {
    /// Seed for `#RANDOM` branches.
    pub seed: u64,
    /// Bounds on the work.
    pub limits: Limits,
}

/// A loaded chart and what the loader skipped or assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// The resolved chart.
    pub chart: Chart,
    /// Warnings in the order they arose.
    pub warnings: Vec<Warning>,
}

/// Loads a BMS chart from its file bytes. Sound names resolve inside
/// `chart_dir`.
///
/// # Errors
///
/// Returns a [`LoadError`] for a broken limit, a malformed or unsupported
/// directive, a sound path leaving the folder, or a time overflow.
pub fn load_bms(bytes: &[u8], chart_dir: &Path, opts: &LoadOptions) -> Result<Loaded, LoadError> {
    let limits = &opts.limits;
    if bytes.len() > limits.max_file_bytes {
        return Err(LoadError::FileTooLarge { bytes: bytes.len() });
    }
    let mut warnings = Vec::new();
    let mut source = {
        let text = decode(bytes, &mut warnings);
        parse::parse(&text, limits, opts.seed, &mut warnings)?
    };
    let timeline = timing::compute(&source, limits, &mut warnings)?;

    let dir = chart_dir
        .canonicalize()
        .map_err(|e| LoadError::ChartDir { kind: e.kind() })?;
    let mut ids = BTreeMap::new();
    let wavs = std::mem::take(&mut source.wavs);
    let mut sounds = Vec::with_capacity(wavs.len());
    for (id, wav) in wavs {
        let path = if wav.value.is_empty() {
            None
        } else {
            sound::resolve(&wav.value, wav.line, &dir)?
        };
        if path.is_none() {
            warnings.push(Warning {
                line: Some(wav.line),
                kind: WarningKind::MissingSoundFile,
            });
        }
        let index = u16::try_from(sounds.len()).unwrap_or(u16::MAX);
        ids.insert(id, SoundId(index));
        sounds.push(SoundRef {
            name: wav.value,
            path,
        });
    }

    let mut lookup = resolve::Sounds {
        ids: &ids,
        warned: BTreeSet::new(),
    };
    let resolved = resolve::resolve(&timeline.objects, &source.lnobj, &mut lookup, &mut warnings);
    if resolved.lanes.iter().all(Vec::is_empty) {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::NoPlayableNotes,
        });
    }

    let (sha256, md5) = hash::file_hashes(bytes);
    let mut chart = Chart {
        lanes: resolved.lanes,
        bgm: resolved.bgm,
        sounds,
        bar_lines: timeline.bar_lines,
        scroll: ScrollMap::new(timeline.initial_bpm_micro, &timeline.scroll)
            .map_err(LoadError::Model)?,
        meta: source.meta,
        identity: ChartIdentity {
            sha256,
            md5,
            resolved_hash: [0; 32],
            loader_version: LOADER_VERSION,
            random_results: std::mem::take(&mut source.random_results),
        },
    };
    chart.validate().map_err(LoadError::Model)?;
    chart.identity.resolved_hash = hash::resolved_hash(&chart, &timeline.scroll);
    Ok(Loaded { chart, warnings })
}

/// Decodes the text in its detected encoding (Shift_JIS, UTF-8, ...).
fn decode<'a>(bytes: &'a [u8], warnings: &mut Vec<Warning>) -> Cow<'a, str> {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    let (text, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::DecodeErrors,
        });
    }
    text
}
