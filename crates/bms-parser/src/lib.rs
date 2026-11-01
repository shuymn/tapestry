//! BMS chart loader: chart text to a resolved [`Chart`], with judgment times
//! in integer µs and display positions in fractions of a beat.
//!
//! Supports `#PLAYER 1` 7-key + scratch charts. Charts and sounds are
//! untrusted: every limit in [`Limits`] is checked before the work it bounds,
//! times use exact rationals whose size is capped, and sound files are
//! confined to the chart folder. [`read_header`] reads only the descriptive
//! header commands, for a song list. This crate does not use floating point.
//! The cast lints at the crate root reject `as` casts that may truncate,
//! wrap, or lose the sign; they cover the library and its unit tests, not
//! the integration tests.
//!
//! # Loader version 8
//!
//! These rules are fixed for [`LOADER_VERSION`] 8; changing any of them
//! changes the version.
//!
//! - **Text encoding:** a UTF-8 or UTF-16 BOM decides the encoding. Without
//!   one, text that is valid UTF-8 (pure ASCII included) is UTF-8; then
//!   text that decodes as Shift_JIS (CP932) without an error is Shift_JIS,
//!   unless chardetng guesses EUC-KR or EUC-JP; then chardetng's guess,
//!   with no top-level-domain hint. Malformed bytes in the chosen encoding
//!   become U+FFFD with one [`WarningKind::DecodeErrors`] warning.
//!   [`read_header`] decodes alike.
//! - **Time:** each object's time is accumulated in exact rationals and
//!   floored to µs once; the time's denominator has at most
//!   [`Limits::max_time_bits`] bits (an error otherwise). BPM, `#STOPxx`,
//!   measure-length and judgment-header values are decimals with an
//!   optional exponent (`1.5`, `.5`, `1.00001E+07`), positive at most 18,
//!   with at most 12 integer digits once the exponent applies; digits past
//!   the 18th fractional one are cut off toward zero. A malformed timing
//!   value is an error, and so is a measure length of 0, as written or cut
//!   off to it. A BPM is at least 0.000001, and a negative one is an error,
//!   since reverse scroll is not supported yet. A `#STOPxx` value counts
//!   1/192 of a 4/4 measure (1/48 beat) whatever the measure length.
//! - **Judgment headers:** every `#RANK`, `#DEFEXRANK` and `#TOTAL` read
//!   (in a taken branch) is kept in file order in
//!   [`ChartMeta::judgment_headers`](bms_model::ChartMeta::judgment_headers)
//!   in canonical form: an integer token (`[+-]?[0-9]+`), another decimal,
//!   each with its exact value in units of `10^-18`, or not a number (text
//!   outside the decimal grammar or its bounds, with a warning). None is
//!   refused, and the loader does not decide which one counts or what a
//!   value means: that is each preset's header policy.
//! - **Display:** a note head, long-note end or bar line sits at its exact
//!   beat count floored once to 2^-80 beat; STOPs and BPM changes add no
//!   beats. The scroll map has a segment per BPM change, STOP start and STOP
//!   end, starting at the first whole µs at or after its exact time, anchored
//!   on the exact curve there (both floored to 2^-80 beat) with the exact
//!   BPM's rate floored as its slope; segments with one start keep the later
//!   change. Before time 0 it runs at the `#BPM` header's rate. A position
//!   or retained time beyond what the map holds (±2^32 µs) is a
//!   [`LoadError::PositionOverflow`].
//! - **Speed:** at a [`LoadOptions::speed`] `k/20` other than 1×, each kept
//!   time is its exact time times `20/k`, floored once to µs, and the scroll
//!   map is built as above from that mapped exact timeline, so its rates
//!   are the mapped BPMs'. The same pass keeps the 1× times and scroll map:
//!   the duration limit applies to them, and the resolved hash covers that
//!   1× chart, so the identity is the same at every speed. A real time
//!   beyond ±2^32 µs is a [`LoadError::TooLongAtSpeed`].
//! - **Identity:** which objects are the same, which survives a conflict,
//!   long-note pairing and a note inside a long note are decided on exact
//!   source positions, never on floored µs: objects at distinct positions
//!   that floor to one µs stay distinct, in position order.
//! - **Same position:** a BPM change applies before a STOP, so the STOP uses
//!   the new BPM; a note at a STOP's position sounds at the STOP's start.
//!   Channel 08 wins over channel 03; within a channel the later object in
//!   the file wins. STOPs at one position add up.
//! - **End:** the chart ends with its last note, mine or BGM object. Bar
//!   lines, BPM changes and STOPs after it are dropped, and the duration
//!   limit applies up to it.
//! - **Lanes:** channels 16 (scratch), 11-15 and 18-19 (keys 1-7); 51-59
//!   long notes (`#LNTYPE 1`, consecutive objects pair up); D1-D9 mines,
//!   with the object id as damage. Channel 17 and other unused channels are
//!   ignored with a warning; BGA channels 04, 06 and 07 and invisible notes
//!   (31-39) silently. A 2P lane object (21-29, 61-69, E1-E9) is an
//!   `Unsupported` error.
//! - **Headers:** headers for what stage 1 does not show or use (`#BASEBPM`,
//!   `#CHARFILE`, `#EXTCHR`, `#MAKER`, `#BMPxx`, `#BGAxx`,
//!   `#TEXTxx`, `#EXRANKxx` and descriptive ones) are ignored silently; any
//!   other unknown header is a warning.
//! - **`#LNOBJ`:** a channel 1x object with that id ends a long note started
//!   by the previous plain note in its lane, if that note sits at an
//!   earlier position. The end plays no sound. An empty `#LNOBJ` is
//!   ignored, with one warning per chart.
//! - **`#LNTYPE`:** read as an integer (`01` is 1). A value naming no type
//!   (`0`, or not a number) is read as 1, with one warning per chart; any
//!   other number, even one too large for `u32`, is unsupported.
//! - **`#LNTYPE 1` pairing:** endpoints at one position in a lane merge
//!   first, keeping the later object in the file, so a duplicated line does
//!   not split a long note. The end object's sound is the end sound when its id
//!   differs from the start's, otherwise there is none.
//! - **Conflicts in a lane:** at one position one note is kept: a long note
//!   over a tap over a mine, and the later object in the file among equals.
//!   A note at or inside the span of an earlier long note in its lane, mines
//!   included, is dropped. Each dropped note is a warning.
//! - **Sounds:** the table holds every `#WAV` in id order. A reference to an
//!   undefined id plays nothing (BGM events are dropped). A name is tried as
//!   written, then with the extensions `.wav .ogg .flac .mp3`.
//! - **Empty charts:** a chart with no notes loads, with a
//!   [`WarningKind::NoPlayableNotes`] warning; whether to play it is the
//!   caller's decision.
//! - **`#RANDOM n`:** draws `next() % n + 1` from SplitMix64 seeded with
//!   [`LoadOptions::seed`]; `#SETRANDOM n` sets `n`. Each is read wherever
//!   it is, inside an `#IF` branch not read too, and every value is recorded
//!   in [`ChartIdentity::random_results`] in file order. `n` must be a
//!   positive integer (an error otherwise, since the chart's branches would
//!   be undefined).
//! - **Blocks**, as charts are written against (the reference is
//!   jbms-parser's `BMSDecoder`): `#RANDOM` and `#SETRANDOM` push a value
//!   and `#ENDRANDOM` pops one; `#IF k` pushes a flag, set when the latest
//!   value is `k`, and `#ENDIF` pops whatever flag is on top. A line is read
//!   when the top flag is set or no flag is open, headers (`#WAVxx`,
//!   `#BPMxx`, `#LNOBJ`, ...) included. `#ELSE`, our addition, sets the top
//!   flag when no earlier branch of its `#IF` was taken and clears it
//!   otherwise. An `#IF` with no value to test pushes no flag, so the flag
//!   already on top still decides (one warning per chart). An `#IF` met over an open `#IF` of
//!   the same `#RANDOM` is warned about once per chart; a stray `#ELSE`,
//!   `#ENDIF` or `#ENDRANDOM` is ignored with a warning, and an `#IF` still
//!   open at the end of the file is a warning. The stacks are bounded by the
//!   line limit.
//! - **Unsupported:** `#LNTYPE` 2 or higher, `#LNMODE` other than 1, negative
//!   `#STOPxx`, `#ELSEIF`, and `#SWITCH`/`#CASE`/`#SKIP`/`#DEF`/`#ENDSW`.
//!   The branching ones are refused even inside a branch not taken.
//! - **Resolved hash:** covers what the chart resolved to at 1×, display
//!   positions, the scroll map and the judgment-header declarations in
//!   canonical form included (so whitespace, a header's case and line
//!   positions do not change it, and `120` and `120.0` differ), not the
//!   `#RANDOM` results that chose it, so two draws resolving to the same
//!   objects share one hash.
//! - **Played hash:** the same encoding over the chart at its speed, under
//!   its own tag and with the speed's k; the resolved hash at 1×. Two
//!   files can resolve alike at 1× and floor apart at a speed, so a record
//!   pins the real timeline it was played on with it.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]
#![deny(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod branch;
mod decimal;
mod error;
mod hash;
mod header;
mod parse;
mod resolve;
mod sound;
mod timing;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bms_model::{
    Chart, ChartIdentity, ModelError, NoteKind, ScrollMap, SoundId, SoundRef, Speed, TimeUs,
};

pub use error::{LoadError, Warning, WarningKind};
pub use header::{Header, read_header};

/// Version of the loader rules recorded in [`ChartIdentity::loader_version`].
pub const LOADER_VERSION: u32 = 8;

/// Bounds on the work a chart may cause (design.md "Loading Limits"). The
/// defaults sit far above real charts.
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
    /// Most bits in the denominator of the exact running time (design.md "Chart
    /// Time").
    pub max_time_bits: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 16 * 1024 * 1024,
            max_lines: 500_000,
            max_objects: 500_000,
            max_measure: 999,
            max_duration: TimeUs(60 * 60 * 1_000_000),
            max_time_bits: 16_384,
        }
    }
}

/// Options for [`load_bms`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadOptions {
    /// Seed for `#RANDOM` branches.
    pub seed: u64,
    /// The practice speed: the chart's times are real times at it, and its
    /// identity is the 1× chart's.
    pub speed: Speed,
    /// Bounds on the work.
    pub limits: Limits,
}

/// The descriptive labels a chart declares besides its title
/// ([`ChartMeta::title`](bms_model::ChartMeta::title)), as written
/// (trimmed), from the commands left after `#RANDOM`/`#IF` resolution; the
/// last declaration wins, and an undeclared one is empty. They are not part
/// of the chart's identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    /// `#SUBTITLE`.
    pub subtitle: String,
    /// `#ARTIST`.
    pub artist: String,
    /// `#SUBARTIST`.
    pub subartist: String,
    /// `#GENRE`.
    pub genre: String,
}

/// A loaded chart and what the loader skipped or assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// The resolved chart.
    pub chart: Chart,
    /// Its labels besides the title, from the same resolution.
    pub labels: Labels,
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
    let mut timeline = timing::compute(&source, limits, opts.speed, &mut warnings)?;

    let dir = chart_dir
        .canonicalize()
        .map_err(|e| LoadError::ChartDir { kind: e.kind() })?;
    let mut ids = BTreeMap::new();
    let wavs = std::mem::take(&mut source.wavs);
    let labels = std::mem::take(&mut source.labels);
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
    let resolved = resolve::resolve(
        &timeline.objects,
        timeline.real.is_some(),
        &source.lnobj,
        &mut lookup,
        &mut warnings,
    );
    if resolved.lanes.iter().all(Vec::is_empty) {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::NoPlayableNotes,
        });
    }

    let (sha256, md5) = hash::file_hashes(bytes);
    let measure = timeline.last_measure;
    let compile = |initial_rate, segments| {
        ScrollMap::from_segments(initial_rate, segments).map_err(|e| match e {
            ModelError::PositionOverflow => LoadError::PositionOverflow { measure },
            e => LoadError::Model(e),
        })
    };
    let real_scroll = timeline.real.take();
    let scroll = compile(timeline.initial_rate, timeline.segments)?;
    // The 1× chart first: its hash is the identity at every speed.
    let mut chart = Chart {
        lanes: resolved.lanes,
        bgm: resolved.bgm,
        sounds,
        bar_lines: timeline.bar_lines,
        scroll,
        meta: source.meta,
        identity: ChartIdentity {
            sha256,
            md5,
            resolved_hash: [0; 32],
            played_hash: [0; 32],
            loader_version: LOADER_VERSION,
            random_results: source.random_results,
        },
    };
    chart.validate().map_err(LoadError::Model)?;
    chart.identity.resolved_hash = hash::resolved_hash(&chart);
    chart.identity.played_hash = chart.identity.resolved_hash;
    // Then, at another speed, the same objects at their real times, on the
    // real timeline's scroll map.
    if let (Some(real), Some(scroll)) = (resolved.real, real_scroll) {
        for (lane, times) in chart.lanes.iter_mut().zip(real.lanes) {
            for (note, (head, tail)) in lane.iter_mut().zip(times) {
                note.time = head;
                if let NoteKind::Long { end, .. } = &mut note.kind {
                    *end = tail;
                }
            }
        }
        for (event, time) in chart.bgm.iter_mut().zip(real.bgm) {
            event.time = time;
        }
        chart.scroll = compile(scroll.initial_rate, scroll.segments)?;
        chart.validate().map_err(LoadError::Model)?;
        chart.identity.played_hash = hash::played_hash(&chart, opts.speed);
    }
    Ok(Loaded {
        chart,
        labels,
        warnings,
    })
}

/// Decodes the text in the encoding of the "Text encoding" rule, warning with
/// [`WarningKind::DecodeErrors`] when it holds malformed bytes.
fn decode<'a>(bytes: &'a [u8], warnings: &mut Vec<Warning>) -> Cow<'a, str> {
    let (text, had_errors) = decode_detected(bytes);
    if had_errors {
        warnings.push(Warning {
            line: None,
            kind: WarningKind::DecodeErrors,
        });
    }
    text
}

/// [`decode`]'s text and whether it held malformed bytes. Valid UTF-8 comes
/// before Shift_JIS since Shift_JIS text with Japanese characters is almost
/// never valid UTF-8, and Shift_JIS comes before the guess since a guess
/// over mostly ASCII text with a few Japanese characters can name a Western
/// or Chinese encoding. A guess of EUC-KR or EUC-JP is the exception: their
/// two-byte characters are often valid Shift_JIS too, as pairs of half-width
/// katakana, which Japanese text in Shift_JIS rarely holds.
fn decode_detected(bytes: &[u8]) -> (Cow<'_, str>, bool) {
    // chardetng depends on encoding_rs without default features; the direct
    // dependency turns on the `alloc` feature that decoding needs.
    if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode_with_bom_removal(bytes);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (Cow::Borrowed(text), false);
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let guess = detector.guess(None, chardetng::Utf8Detection::Deny);
    if guess != encoding_rs::EUC_KR
        && guess != encoding_rs::EUC_JP
        && let Some(text) =
            encoding_rs::SHIFT_JIS.decode_without_bom_handling_and_without_replacement(bytes)
    {
        return (text, false);
    }
    guess.decode_without_bom_handling(bytes)
}
