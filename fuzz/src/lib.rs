//! The bodies of the fuzz targets, one function per target, so the smoke
//! tests below run the seeds through the same code the fuzzer does. The
//! targets are in `fuzz_targets/`, the seed generator in [`seeds`].
//!
//! A target fails on a panic: a library panic, or a broken invariant
//! asserted here.

#![forbid(unsafe_code)]

pub mod seeds;

use std::collections::BTreeSet;
use std::hint::black_box;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use bms_parser::{LoadError, LoadOptions, Loaded};
use input::raw;
use replay::{DecodeError, DecodeLimits, FORMAT_VERSION, MAGIC, PlayRecord};
use rules::{CanonicalError, RulePreset};
use skin_beatoraja::{
    Config, DeviationId, Header, ImportError, Imported, NoteFallback, NoteKind, SIZES, Severity,
    import, read_header, resolve,
};
use skin_model::{FntError, FntFile, FntLimits};

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

/// The files a `skin_json` input is split into, in order: the entry file,
/// then files it may include.
pub const SKIN_FILES: [&str; 4] = ["s.json", "i1.json", "i2.json", "i3.json"];

/// A folder for the skin definitions, rewritten by each input, with
/// fixed resources the definitions can name: `strip.png`, ten 10×10
/// frames in a row; `grid.png`, a 2×2 grid of 4×4 frames; `font.ttf`,
/// a file with a font's magic number; and `font.fnt`, the bitmap font
/// [`seeds::FONT_FNT`], whose page is `font_0.png`, 16×8.
fn skin_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("tapestry-fuzz-skin-")
            .tempdir()
            .expect("create a skin folder");
        write_png(&dir.path().join("strip.png"), 100, 10);
        write_png(&dir.path().join("grid.png"), 8, 8);
        std::fs::write(dir.path().join("font.ttf"), [0, 1, 0, 0, 0]).expect("write a font");
        std::fs::write(dir.path().join("font.fnt"), seeds::FONT_FNT).expect("write a font");
        write_png(&dir.path().join("font_0.png"), 16, 8);
        dir
    })
    .path()
}

/// An opaque `w`×`h` PNG at `path`.
fn write_png(path: &Path, w: u32, h: u32) {
    let file = std::fs::File::create(path).expect("create a PNG");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    let mut out = enc.write_header().expect("write a PNG header");
    let texels = usize::try_from(w * h * 4).expect("a small PNG");
    out.write_image_data(&vec![255; texels])
        .expect("write PNG data");
}

/// What an imported skin draws: the quads of frames at a few times of a
/// 100-note play with a perfect judgment at each, each frame within the
/// skin's quad capacity; its loading state is built too, within the same
/// capacities.
///
/// # Panics
///
/// If the atlas, the play or the text table is refused, a play or loading
/// frame holds more quads than the skin's capacity, or a refused loading
/// state builds again.
#[must_use]
pub fn draw(imported: &Imported) -> Vec<usize> {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
    use rules::{GaugeValue, Judgment, PerJudgment};
    use skin_model::{
        AtlasLimits, Attempt, ChartText, FrameUv, GaugeDesc, GaugeLook, LaneView, Layout,
        PlayContext, QuadBatch, Side, SkinFrame, TextImage, TextTable, Variant,
    };

    let skin = &imported.skin;
    let limits = AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    let uvs = skin
        .atlas(Variant::Lo, limits)
        .expect("an imported sheet builds its atlas")
        .into_parts()
        .1;
    let lane = Lane::new(1).expect("lane 1");
    let chart = (0..100_i64)
        .fold(ChartBuilder::new(150_000), |b, i| {
            b.tap(lane, TimeUs(1_000_000 + i * 10_000))
        })
        .build()
        .expect("a chart of taps");
    let gauges = [GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }];
    let ctx = PlayContext {
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
    };
    let mut play = skin
        .prepare(&chart, &ctx)
        .expect("an imported skin prepares");
    let text = ChartText {
        title: "t",
        subtitle: "",
        artist: "a",
        subartist: "",
        genre: "g",
    };
    let mut lines = vec![None; skin.text_slots()];
    for r in skin.text_requests(&text, [1_920, 1_080], &Layout::AUTHOR) {
        let uv = FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 1,
        };
        if let Some(slot) = lines.get_mut(usize::from(r.slot)) {
            *slot = TextImage::new(uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        }
    }
    let table = TextTable::new(lines);
    skin.attach_text(&mut play, table.clone())
        .expect("one line per slot");
    skin.attach_bitmap_text(&mut play, &text);
    let tally = Tally {
        counts: PerJudgment {
            perfect: 1,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 150,
        bp: 0,
        combo: 1,
        max_combo: 1,
        empty_poor: 0,
    };
    let states = [GaugeState {
        value: gauges[0].start,
        reference: gauges[0].start,
        failed_at: None,
    }];
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut counts = Vec::new();
    for ms in [0_i64, 50, 500, 5_000] {
        let effects = [JudgeEvent {
            seq: 0,
            time: TimeUs(ms * 1_000),
            origin: None,
            kind: JudgeEventKind::Judged {
                lane,
                note: 0,
                judgment: Judgment::Perfect,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 1,
            },
        }];
        batch.clear();
        let frame = SkinFrame {
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &effects,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
        };
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        let n = batch.quads().len();
        assert!(
            n <= usize::try_from(skin.quad_capacity()).unwrap_or(usize::MAX),
            "more quads than the capacity"
        );
        check_runs(&batch);
        counts.push(n);
    }
    // The covers moving, past a whole lane too, and HIDDEN+ switched off
    // with an amount stored: followers and trimmed paint stay finite and
    // inside the capacities.
    for (ms, cover, lift, hidden, hidden_on) in [
        (600_i64, 300, 100, 500, true),
        (700, 1_000, 1_000, 1_000, true),
        (800, 1_500, 2_000, u16::MAX, true),
        (900, 0, 0, 0, true),
        (1_000, 300, 250, 500, false),
        (1_100, 0, 1_000, 1_000, false),
    ] {
        batch.clear();
        let frame_lanes = LaneView {
            visible_time: TimeUs(600_000),
            cover,
            lift,
            hidden: if hidden_on { hidden } else { 0 },
        };
        let mut readouts = readouts_for(frame_lanes);
        if !hidden_on {
            readouts.covers.hidden = hidden.min(1_000);
            readouts.covers.hidden_enabled = false;
        }
        let frame = SkinFrame {
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes: frame_lanes,
            readouts,
            playing: true,
        };
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        check_batch(skin, &batch);
    }
    rate_and_kind_probes(skin, &chart, &uvs, &mut batch);
    readout_probes(skin, &chart, &uvs, &mut batch);
    choice_probes(skin, &chart, &uvs, &mut batch);
    bar_probes(skin, &ctx, (&tally, &states), &uvs, &mut batch);
    long_note_probes(skin, &ctx, (&tally, &states), &uvs, &mut batch);
    loading_probes(skin, (&text, &table), &gauges[0], &uvs, &mut batch);
    counts
}

/// The skin's loading state with the chart's lines (`table`, the lines
/// the play attached) installed, built as
/// its clock and progress advance, with its gauge descriptor given,
/// changed and taken away, the covers moving and the lines cleared, each
/// frame inside the skin's capacities; a refused build refuses the state
/// for good.
fn loading_probes(
    skin: &skin_model::Skin,
    (text, table): (&skin_model::ChartText<'_>, &skin_model::TextTable),
    desc: &skin_model::GaugeDesc,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::TimeUs;
    use skin_model::{
        BuildError, GaugeLook, LaneView, Layout, LoadState, LoadingFrame, LoadingGauge, Side,
        TextSlots,
    };

    let mut loading = skin.prepare_loading(Side::Left);
    skin.install_loading_text(&mut loading, TextSlots::ALL, table)
        .expect("one line per slot");
    skin.install_loading_bitmap_text(&mut loading, TextSlots::ALL, text);
    let normal = LoadingGauge::of(0, desc);
    let hard = LoadingGauge {
        look: GaugeLook::Hard,
        ..normal
    };
    for (ms, percent, gauge, cover) in [
        (0_i64, 0, Some(normal), 0_u16),
        (50, 10, Some(normal), 300),
        (500, 40, None, 1_000),
        (900, 40, Some(hard), 1_500),
        (5_000, 100, Some(normal), 0),
    ] {
        if ms == 900 {
            skin.clear_loading_text(&mut loading, TextSlots::ALL);
        }
        batch.clear();
        let lanes = LaneView {
            visible_time: TimeUs(600_000),
            cover,
            lift: 0,
            hidden: 0,
        };
        let frame = LoadingFrame {
            now: TimeUs(ms * 1_000),
            load: LoadState {
                since: TimeUs(0),
                percent,
            },
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes,
            covers: readouts_for(lanes).covers,
            gauge,
        };
        skin.build_loading(&mut loading, &frame, uvs, batch)
            .expect("the loading frame builds");
        check_batch(skin, batch);
    }
    // Covers the frame's lanes do not show: refused, then for good.
    let mut frame = LoadingFrame {
        now: TimeUs(6_000_000),
        load: LoadState {
            since: TimeUs(0),
            percent: 100,
        },
        target: [1_920, 1_080],
        layout: &Layout::AUTHOR,
        lanes: LANES_OFF,
        covers: readouts_for(LANES_OFF).covers,
        gauge: Some(normal),
    };
    frame.covers.cover = 500;
    assert_eq!(
        skin.build_loading(&mut loading, &frame, uvs, batch),
        Err(BuildError::InvalidReadouts)
    );
    frame.covers = readouts_for(LANES_OFF).covers;
    assert_eq!(
        skin.build_loading(&mut loading, &frame, uvs, batch),
        Err(BuildError::InvalidReadouts)
    );
}

/// A long note held and released in LN and CN, with its end in view and
/// past the lanes' top, before, inside and after it: fallback paint and
/// file paint alike, each frame inside the skin's capacities.
fn long_note_probes(
    skin: &skin_model::Skin,
    ctx: &skin_model::PlayContext<'_>,
    (tally, gauges): (&eval::Tally, &[eval::GaugeState]),
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::{ChartBuilder, Lane, LnMode, TimeUs};
    use eval::{JudgeEvent, JudgeEventKind};
    use rules::Judgment;
    use skin_model::{LaneView, Layout, SkinFrame};

    let lane = Lane::new(1).expect("lane 1");
    let held = [JudgeEvent {
        seq: 0,
        time: TimeUs(1_000_000),
        origin: None,
        kind: JudgeEventKind::LnHeld {
            lane,
            note: 0,
            judgment: Judgment::Perfect,
            deviation: TimeUs(0),
        },
    }];
    let mut chart = ChartBuilder::new(150_000)
        .long(lane, TimeUs(1_000_000), TimeUs(4_000_000))
        .mine(Lane::new(2).expect("lane 2"), TimeUs(1_200_000), 10)
        .build()
        .expect("a chart of a long note");
    for mode in [LnMode::Ln, LnMode::Cn] {
        chart.meta.ln_mode = Some(mode);
        let mut play = skin
            .prepare(&chart, ctx)
            .expect("an imported skin prepares");
        for (ms, effects, cover) in [
            (500_i64, &[][..], 0),
            (1_000, &held[..], 300),
            (2_000, &[][..], 0),
            (3_990, &[][..], 1_000),
            (4_500, &[][..], 0),
        ] {
            batch.clear();
            let lanes = LaneView {
                visible_time: TimeUs(600_000),
                cover,
                lift: 0,
                hidden: 0,
            };
            let frame = SkinFrame {
                now: TimeUs(ms * 1_000),
                display_offset: TimeUs(0),
                tally,
                gauges,
                gauge: 0,
                effects,
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                lanes,
                readouts: readouts_for(lanes),
                playing: true,
            };
            skin.build(&mut play, &frame, uvs, batch)
                .expect("the frame builds");
            check_batch(skin, batch);
        }
    }
}

/// The bar line groups over a bounded chart of bars: one every 125 ms
/// across a STOP, eight more equal to the one where a note stands, drawn
/// before the play, through the STOP and past the last bar, with display
/// offsets and the covers moving. Each frame stays inside the skin's
/// capacities; the lanes' budgets drop at most one quad.
fn bar_probes(
    skin: &skin_model::Skin,
    ctx: &skin_model::PlayContext<'_>,
    (tally, gauges): (&eval::Tally, &[eval::GaugeState]),
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use skin_model::{LaneView, Layout, SkinFrame};

    let mut chart = ChartBuilder::new(150_000)
        .stop(TimeUs(1_000_000), TimeUs(500_000))
        .tap(Lane::new(1).expect("lane 1"), TimeUs(3_000_000))
        .build()
        .expect("a chart of a tap");
    let at = |ms: i64| chart.scroll.position_at(TimeUs(ms * 1_000));
    let mut bars: Vec<_> = (0..40).map(|i| at(i * 125)).collect();
    bars.extend(std::iter::repeat_n(at(3_000), 8));
    bars.sort_unstable();
    chart.bar_lines = bars;
    chart.validate().expect("the bar chart is valid");
    let mut play = skin
        .prepare(&chart, ctx)
        .expect("an imported skin prepares");
    for (ms, offset, cover, lift) in [
        (-500_i64, 0_i64, 0, 0),
        (0, 40, 300, 100),
        (1_200, -25, 0, 250),
        (3_000, 0, 1_000, 1_000),
        (9_000, 0, 0, 0),
    ] {
        batch.clear();
        let lanes = LaneView {
            visible_time: TimeUs(600_000),
            cover,
            lift,
            hidden: 0,
        };
        let frame = SkinFrame {
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(offset * 1_000),
            tally,
            gauges,
            gauge: 0,
            effects: &[],
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes,
            readouts: readouts_for(lanes),
            playing: true,
        };
        skin.build(&mut play, &frame, uvs, batch)
            .expect("the frame builds");
        assert!(batch.dropped() <= 1, "more than the lanes' one drop");
        assert!(
            batch.quads().len() <= usize::try_from(skin.quad_capacity()).unwrap_or(usize::MAX),
            "more quads than the capacity"
        );
        assert!(
            batch.runs().len() <= usize::try_from(skin.run_capacity()).unwrap_or(usize::MAX),
            "more runs than the capacity"
        );
        assert!(
            batch
                .quads()
                .iter()
                .all(|q| q.rect.iter().chain(&q.pivot).all(|v| v.is_finite())),
            "a quad off the finite plane"
        );
    }
}

/// Image choices over small and extreme selections and clocks: EX scores
/// selecting each of the first entries and past any table, at times
/// before the play, within it and far past it, each frame inside the
/// skin's capacities.
fn choice_probes(
    skin: &skin_model::Skin,
    chart: &bms_model::Chart,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::TimeUs;
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{Attempt, GaugeDesc, GaugeLook, Layout, PlayContext, Side, SkinFrame};

    let descs = [GaugeDesc {
        start: GaugeValue(50 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Hard,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }];
    let states = [GaugeState {
        value: descs[0].start,
        reference: descs[0].start,
        failed_at: None,
    }];
    let ctx = PlayContext {
        gauges: &descs,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
    };
    let mut play = skin
        .prepare(chart, &ctx)
        .expect("an imported skin prepares");
    let probes = [
        (i64::MIN, 0),
        (-1_000_000, 1),
        (0, 2),
        (1, 3),
        (299_999, 4),
        (1_000_000, 5),
        (i64::MAX, u32::MAX),
    ];
    for (now, ex_score) in probes {
        let tally = Tally {
            counts: PerJudgment {
                perfect: ex_score / 2,
                great: ex_score % 2,
                good: 0,
                bad: 0,
                poor: 0,
            },
            ex_score,
            bp: 0,
            combo: 0,
            max_combo: 0,
            empty_poor: 0,
        };
        batch.clear();
        let frame = SkinFrame {
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
        };
        skin.build(&mut play, &frame, uvs, batch)
            .expect("a consistent snapshot builds");
        check_batch(skin, batch);
    }
}

/// The score rates and the gauge kind at their extremes: both empty-POOR
/// policies over the largest consistent counts, every look shown and
/// indices past the snapshot and the descriptors, each frame inside the
/// schema's ranges and the skin's capacities.
fn rate_and_kind_probes(
    skin: &skin_model::Skin,
    chart: &bms_model::Chart,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::TimeUs;
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{Attempt, GaugeDesc, GaugeLook, Layout, PlayContext, Side, SkinFrame, field};

    let descs = GaugeLook::ALL.map(|look| GaugeDesc {
        start: GaugeValue(50 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    });
    let states = descs.map(|d| GaugeState {
        value: d.start,
        reference: d.start,
        failed_at: None,
    });
    let max = u32::MAX;
    let tally = Tally {
        counts: PerJudgment {
            perfect: max,
            great: max,
            good: max,
            bad: max,
            poor: max,
        },
        ex_score: max,
        bp: max,
        combo: max,
        max_combo: max,
        // As many as the POORs: consistent under either policy.
        empty_poor: max,
    };
    for counted in [false, true] {
        let ctx = PlayContext {
            gauges: &descs,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: counted,
            empty_poor_counted: counted,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
        };
        let mut play = skin
            .prepare(chart, &ctx)
            .expect("an imported skin prepares");
        // Every look, then past the descriptors; a short snapshot leaves
        // the later looks without one.
        for (k, snapshot) in [
            (0..=descs.len(), &states[..]),
            (0..=descs.len(), &states[..3]),
        ]
        .into_iter()
        .flat_map(|(ks, s)| ks.map(move |k| (k, s)))
        {
            batch.clear();
            let frame = SkinFrame {
                now: TimeUs(2_000_000),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: snapshot,
                gauge: k,
                effects: &[],
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                lanes: LANES_OFF,
                readouts: readouts_for(LANES_OFF),
                playing: true,
            };
            skin.build(&mut play, &frame, uvs, batch)
                .expect("a consistent snapshot builds");
            let fields = play.state().fields;
            let read = |slot: u16| fields.get(usize::from(slot)).copied();
            let valid = k < snapshot.len() && k < descs.len();
            assert_eq!(read(field::GAUGE_VALID), Some(i32::from(valid)));
            let kind = descs
                .get(k)
                .filter(|_| valid)
                .map_or(GaugeLook::Normal, |d| d.look);
            let kind = i32::try_from(kind.index()).expect("six looks");
            assert_eq!(read(field::GAUGE_KIND), Some(kind));
            // 10 000 · (2^32 − 1) / (10 · (2^32 − 1)) of the judged
            // targets, less the empty POORs counted; the whole rate over
            // the chart's 100 taps past the field's bound.
            let current = if counted { 1_250 } else { 1_000 };
            assert_eq!(read(field::RATE_CURRENT_BP), Some(current));
            assert_eq!(read(field::RATE_WHOLE_BP), Some(i32::MAX));
            check_batch(skin, batch);
        }
    }
}

/// A built frame's batch is inside `skin`'s capacities: no quad dropped,
/// no more runs than reserved, and every quad on the finite plane.
#[track_caller]
fn check_batch(skin: &skin_model::Skin, batch: &skin_model::QuadBatch) {
    assert_eq!(batch.dropped(), 0, "quads past the capacity");
    assert!(
        batch.runs().len() <= usize::try_from(skin.run_capacity()).unwrap_or(usize::MAX),
        "more runs than the capacity"
    );
    assert!(
        batch
            .quads()
            .iter()
            .all(|q| q.rect.iter().chain(&q.pivot).all(|v| v.is_finite())),
        "a quad off the finite plane"
    );
    check_runs(batch);
}

/// A batch's runs tile its quads in order, each non-empty; neighbours
/// differ in their key or their inline clip, which is finite with a
/// positive area.
#[track_caller]
fn check_runs(batch: &skin_model::QuadBatch) {
    let mut end = 0;
    let mut last: Option<&skin_model::Run> = None;
    for r in batch.runs() {
        assert_eq!(r.start, end, "a gap or overlap between runs");
        assert!(r.end > r.start, "an empty run");
        if let Some(c) = r.clip_rect {
            let [x, y, w, h] = c.rect();
            assert!(
                [x, y, w, h].iter().all(|v| v.is_finite()) && w > 0.0 && h > 0.0,
                "an inline clip without area"
            );
        }
        if let Some(prev) = last {
            assert!(
                prev.key != r.key || prev.clip_rect != r.clip_rect,
                "unmerged neighbouring runs"
            );
        }
        end = r.end;
        last = Some(r);
    }
    assert_eq!(
        usize::try_from(end).unwrap_or(usize::MAX),
        batch.quads().len(),
        "quads past the runs"
    );
}

/// Lanes at 600 ms with no cover.
const LANES_OFF: skin_model::LaneView = skin_model::LaneView {
    visible_time: bms_model::TimeUs(600_000),
    cover: 0,
    lift: 0,
    hidden: 0,
};

/// The readouts of a frame showing `view`: no tempo, hi-speed or duration,
/// every cover chosen on and stored at the amount `view` draws (capped at
/// 1 000), so the frame is consistent.
fn readouts_for(view: skin_model::LaneView) -> skin_model::FrameReadouts {
    skin_model::FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        covers: skin_model::CoverSettings {
            cover: view.cover.min(1_000),
            lift: view.lift.min(1_000),
            hidden: view.hidden.min(1_000),
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
    }
}

/// The readouts at their extremes, each frame consistent: explicit
/// tempo, hi-speed and duration, hi-speed unavailable, every cover at
/// 1 000, covers off with stored amounts, durations of wide integer
/// ratios and the largest readouts, before, during and past the chart,
/// each inside the schema's ranges and the skin's capacities.
fn readout_probes(
    skin: &skin_model::Skin,
    chart: &bms_model::Chart,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::TimeUs;
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{
        Attempt, BpmRange, CoverSettings, DurationRatio, FrameReadouts, GaugeDesc, GaugeLook,
        LaneView, Layout, PLAY, PlayContext, READOUT_MAX, Side, SkinFrame, field,
    };

    let descs = [GaugeDesc {
        start: GaugeValue(50 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }];
    let states = [GaugeState {
        value: descs[0].start,
        reference: descs[0].start,
        failed_at: None,
    }];
    let tally = Tally {
        counts: PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 0,
        bp: 0,
        combo: 0,
        max_combo: 0,
        empty_poor: 0,
    };
    let top = READOUT_MAX.unsigned_abs();
    for range in [
        None,
        Some(BpmRange {
            min_x100: 0,
            max_x100: top,
        }),
        Some(BpmRange {
            min_x100: 15_000,
            max_x100: 15_000,
        }),
    ] {
        let ctx = PlayContext {
            gauges: &descs,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: range,
        };
        let mut play = skin
            .prepare(chart, &ctx)
            .expect("an imported skin prepares");
        let ratios = [
            (6_675, 10),
            (u128::MAX, 1),
            (u128::MAX, u128::MAX),
            (u128::from(top) * 2 + 1, 2),
            (1, u128::MAX),
            (0, 7),
        ];
        let probes = [
            (Some(15_000), Some(105), 0, 1_000, 1_000, 1_000, true, false),
            (Some(0), None, 1, 500, 250, 0, false, true),
            (Some(top), Some(top), 2, 1_000, 0, 1_000, true, true),
            (None, Some(0), 3, 0, 1_000, 500, false, false),
            (Some(1), None, 4, 251, 999, 1, true, false),
            (None, None, 5, 0, 0, 0, false, false),
        ];
        for (now_ms, (bpm, hs, ratio, cover, lift, hidden, on, adjusting)) in
            [-1_000_i64, 0, 999, 1_500, 2_000, 9_000_000]
                .into_iter()
                .zip(probes)
        {
            // A duration rounding past the maximum is refused; this probe
            // draws only valid frames, so those leave the duration unknown.
            let duration = ratios
                .get(ratio)
                .filter(|&&(n, d)| n / d < u128::from(top))
                .map(|&(numerator, denominator)| DurationRatio {
                    numerator,
                    denominator,
                });
            let covers = CoverSettings {
                cover,
                lift,
                hidden,
                cover_enabled: on,
                lift_enabled: !on,
                hidden_enabled: on,
                adjusting,
            };
            let effective = |enabled: bool, v: u16| if enabled { v } else { 0 };
            let lanes = LaneView {
                visible_time: TimeUs(600_000),
                cover: effective(on, cover),
                lift: effective(!on, lift),
                hidden: effective(on, hidden),
            };
            batch.clear();
            let frame = SkinFrame {
                now: TimeUs(now_ms * 1_000),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &states,
                gauge: 0,
                effects: &[],
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                lanes,
                readouts: FrameReadouts {
                    bpm_current_x100: bpm,
                    hispeed_x100: hs,
                    duration,
                    covers,
                },
                playing: true,
            };
            skin.build(&mut play, &frame, uvs, batch)
                .expect("consistent readouts build");
            let fields = play.state().fields;
            for (f, &v) in PLAY.fields.iter().zip(fields) {
                if let skin_model::FieldTy::Int { lo, hi } = f.ty {
                    assert!((lo..=hi).contains(&v), "{} outside its range", f.name);
                }
            }
            let read = |slot: u16| fields.get(usize::from(slot)).copied();
            assert_eq!(read(field::HISPEED_VALID), Some(i32::from(hs.is_some())));
            assert_eq!(
                read(field::DURATION_VALID),
                Some(i32::from(duration.is_some()))
            );
            assert_eq!(
                read(field::BPM_RANGE_VALID),
                Some(i32::from(range.is_some()))
            );
            assert_eq!(read(field::COVER_PERMILLE), Some(i32::from(cover)));
            check_batch(skin, batch);
        }
    }
}

/// The header's counts are inside their caps, its size is an accepted
/// one and every file choice's entries are sorted.
fn check_header(h: &Header) {
    use skin_beatoraja::limits::{
        MAX_FILE_CHOICES, MAX_LIST_ENTRIES, MAX_OFFSETS, MAX_OPTION_ITEMS, MAX_OPTIONS,
    };
    assert!(SIZES.contains(&h.size), "a size outside the list");
    assert!(h.options.len() <= MAX_OPTIONS, "too many options");
    let items: usize = h.options.iter().map(|o| o.items.len()).sum();
    assert!(items <= MAX_OPTION_ITEMS, "too many option items");
    assert!(h.files.len() <= MAX_FILE_CHOICES, "too many file choices");
    assert!(h.offsets.len() <= MAX_OFFSETS, "too many offsets");
    let entries: usize = h.files.iter().map(|f| f.candidates.len()).sum();
    assert!(entries <= MAX_LIST_ENTRIES, "too many folder entries");
    for f in &h.files {
        assert!(
            f.candidates.is_sorted(),
            "a file choice's entries are not sorted"
        );
    }
}

/// Target `skin_json`: skin definition files separated by NUL bytes, at
/// most [`SKIN_FILES`]`.len()` of them, written to a temporary skin folder
/// under those names (beside the resources of [`skin_dir`]). The header is
/// read with no option enabled, then resolved again with every option
/// item's number enabled, then once more with none, also after that
/// configured pass fails. A configured header that resolves is returned
/// with the skin imported with the header's defaults; an imported skin
/// is then drawn ([`draw`]).
///
/// # Errors
///
/// What [`read_header`], [`resolve`] or [`Resolved::header`] refuse.
///
/// # Panics
///
/// If a header breaks [`check_header`]'s bounds, resolving again with no
/// option enabled gives another header than the first pass or fails, an
/// import's charged counters pass the model's caps, or [`draw`] panics.
///
/// [`Resolved::header`]: skin_beatoraja::Resolved::header
pub fn skin_json(data: &[u8]) -> Result<(Header, Result<Imported, ImportError>), ImportError> {
    // One input at a time owns the folder (the smoke tests run in
    // parallel).
    static FOLDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _owned = FOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = skin_dir();
    let mut parts = data.splitn(SKIN_FILES.len(), |&b| b == 0);
    for name in SKIN_FILES {
        let path = dir.join(name);
        match parts.next() {
            Some(part) => std::fs::write(&path, part).expect("write a skin file"),
            None => {
                if path.exists() {
                    std::fs::remove_file(&path).expect("remove a skin file");
                }
            }
        }
    }
    let (mut def, header) = read_header(dir, SKIN_FILES[0])?;
    check_header(&header);
    let enabled: BTreeSet<i32> = header
        .options
        .iter()
        .flat_map(|o| o.items.iter().map(|i| i.op))
        .collect();
    // The configured pass may fail part way, leaving included files in the
    // arena; the pass after it must start from the entry file regardless.
    let configured = resolve(&mut def, &enabled).and_then(|mut r| r.header());
    if let Ok(h) = &configured {
        check_header(h);
    }
    let again = resolve(&mut def, &BTreeSet::new()).and_then(|mut r| r.header());
    assert!(
        matches!(&again, Ok(h) if *h == header),
        "resolving again with no option enabled changed the header: {again:?}"
    );
    let configured = configured?;
    let imported = import(&mut def, &header, &Config::defaults(&header));
    if let Ok(i) = &imported {
        check_usage(i);
        black_box(draw(i));
    }
    Ok((configured, imported))
}

/// An import's charged counters are within the model's caps.
fn check_usage(i: &Imported) {
    use skin_model::{
        MAX_BAR_GROUPS, MAX_ELEMENTS, MAX_FILES, MAX_IMAGE_CHOICE_REFS, MAX_SHEET_GLYPHS,
        MAX_SHEET_KERNINGS, MAX_SKIN_FRAMES, MAX_SKIN_KEYS, MAX_SKIN_OPS, MAX_WIDGET_QUADS,
    };
    let u = i.report.usage;
    let within = |n: u32, cap: usize| usize::try_from(n).is_ok_and(|n| n <= cap);
    assert!(within(u.glyphs, MAX_SHEET_GLYPHS), "too many glyphs");
    assert!(within(u.kernings, MAX_SHEET_KERNINGS), "too many kernings");
    assert!(within(u.elements, MAX_ELEMENTS), "too many elements");
    assert!(within(u.keys, MAX_SKIN_KEYS), "too many keys");
    assert!(within(u.ops, MAX_SKIN_OPS), "too many ops");
    assert!(within(u.frames, MAX_SKIN_FRAMES), "too many frames");
    assert!(within(u.file_attempts, MAX_FILES), "too many file attempts");
    assert!(u.widget_quads <= MAX_WIDGET_QUADS, "too many widget quads");
    assert!(within(u.bar_groups, MAX_BAR_GROUPS), "too many bar groups");
    // A group dropped after it was charged is not kept.
    assert!(
        i.skin.bar_group_count() <= usize::try_from(u.bar_groups).unwrap_or(usize::MAX),
        "more bar groups kept than charged"
    );
    assert!(
        within(u.choice_refs, MAX_IMAGE_CHOICE_REFS),
        "too many image choice entries"
    );
    // A selecting element dropped after its entries were charged keeps
    // none of them.
    assert!(
        i.skin.choice_ref_count() <= usize::try_from(u.choice_refs).unwrap_or(usize::MAX),
        "more image choice entries kept than charged"
    );
    // Lanes reserve their quads beside the elements' and widgets'.
    if i.scratch.is_some() {
        let lanes = skin_model::LANE_QUADS + skin_model::LANE_RESERVED;
        assert!(i.skin.quad_capacity() >= lanes, "lanes without their quads");
    }
    // Every lane approximation is a deviation.
    for n in &i.report.notes {
        if let NoteKind::Deviation(DeviationId::D9(_)) = n.kind {
            assert_eq!(n.severity, Severity::Deviation, "a lane approximation");
        }
    }
    // Fallback paint: at most the 48 mapped parts, each reported where it
    // is named, on a model lane, sharing at most three generated images;
    // only lanes draw it.
    assert!(u.note_fallbacks <= 48, "more fallbacks than mapped parts");
    let mut paints = 0_u8;
    let mut entries = 0_u32;
    for n in &i.report.notes {
        if let NoteKind::NotePartFallback { lane, paint, .. } = n.kind {
            assert_eq!(n.severity, Severity::Fallback, "a fallback's severity");
            assert!(n.at.is_some(), "an unlocated fallback");
            assert!(lane < 8, "a fallback off the lanes");
            paints |= match paint {
                NoteFallback::Tap => 1,
                NoteFallback::Long => 2,
                NoteFallback::Mine => 4,
            };
            entries += 1;
        } else {
            assert_ne!(n.severity, Severity::Fallback, "a fallback of another kind");
        }
    }
    assert!(
        entries == u.note_fallbacks || i.report.suppressed > 0,
        "fallback entries and usage differ"
    );
    assert!(
        u.images >= paints.count_ones(),
        "generated images not charged"
    );
    if u.note_fallbacks > 0 {
        assert!(i.scratch.is_some(), "fallback paint without lanes");
    }

    // An element dropped after its ops were charged keeps none of them.
    assert!(
        i.skin.ops().total() <= usize::try_from(u.ops).unwrap_or(usize::MAX),
        "more ops kept than charged"
    );
}

/// Target `skin_fnt`: a bitmap `.fnt` file's bytes, parsed with one
/// file's caps.
///
/// # Errors
///
/// What [`FntFile::parse`] refuses.
///
/// # Panics
///
/// If a parsed font breaks [`check_fnt`]'s bounds.
pub fn skin_fnt(data: &[u8]) -> Result<FntFile, FntError> {
    let f = FntFile::parse(data, "fonts", FntLimits::FILE)?;
    check_fnt(&f);
    Ok(f)
}

/// A parsed font is within its caps, its tables are sorted and unique,
/// its glyphs are on its pages, and every value is a finite number of ems.
fn check_fnt(f: &FntFile) {
    use skin_model::{MAX_FNT_PAGES, MAX_FNT_VALUE, MAX_FONT_GLYPHS, MAX_FONT_KERNINGS};
    let size = f.size();
    assert!(
        size > 0 && i32::from(size) <= MAX_FNT_VALUE,
        "a size out of range"
    );
    let glyphs = f.glyphs().len() + usize::from(f.missing().is_some());
    assert!(glyphs <= MAX_FONT_GLYPHS, "too many glyphs");
    assert!(f.kernings().len() <= MAX_FONT_KERNINGS, "too many kernings");
    assert!(
        (1..=MAX_FNT_PAGES).contains(&f.pages().len()),
        "a page count out of range"
    );
    assert!(
        f.pages()
            .iter()
            .all(|p| p.len() <= skin_model::MAX_PATH_BYTES),
        "a page path too long"
    );
    assert!(
        f.glyphs().windows(2).all(|w| w[0].code < w[1].code),
        "glyphs not sorted and unique"
    );
    assert!(
        f.kernings()
            .windows(2)
            .all(|w| (w[0].first, w[0].second) < (w[1].first, w[1].second)),
        "kernings not sorted and unique"
    );
    assert!(f.glyph(u32::from(b' ')).is_some(), "no space");
    let em = |v: i32| {
        let e = f64::from(v) / f64::from(size);
        assert!(e.is_finite(), "a value of no finite em");
    };
    em(f.ascent());
    em(f.rep());
    assert!(
        f.glyphs().iter().all(|g| g.code != 0),
        "the missing glyph in the table"
    );
    for g in f.glyphs().iter().chain(f.missing().as_ref()) {
        assert!(
            usize::from(g.page) < f.pages().len(),
            "a glyph off its pages"
        );
        for v in [g.xoff, g.raw_yoff, g.adv] {
            em(i32::from(v));
        }
        for v in g.src {
            em(i32::from(v));
        }
        em(i32::from(g.raw_yoff) - f.ascent());
    }
    for k in f.kernings() {
        assert!(
            f.glyph(u32::from(k.first)).is_some(),
            "a kerning pair without its first glyph"
        );
    }
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
