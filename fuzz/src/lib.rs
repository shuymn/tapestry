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
    Cancel, Config, DeviationId, Fact, FactDeps, Header, ImportError, ImportFacts, Imported,
    JudgeClass, LuaImported, LuaOutcome, LuaUsage, NoteFallback, NoteKind, SIZES, Severity, import,
    read_header, read_lua_header, resolve,
};
use skin_model::{FntError, FntFile, FntLimits, ScreenKind};

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
///
/// # Panics
///
/// If a loaded chart breaks its base or link invariants, or its
/// presentation validity disagrees with [`endpoint_conflict`]; if the listing
/// or the summary disagrees with the load; and, loaded
/// again at the practice speed [`practice_speed`] picks for `data`, if the
/// mapped chart does, or keeps other rows, positions, tempos or identity
/// than the 1× chart.
pub fn bms_load(data: &[u8]) -> Result<Loaded, LoadError> {
    let loaded = bms_parser::load_bms(data, chart_dir(), &LoadOptions::default());
    let limits = bms_parser::Limits::default();
    if let Ok(listing) = bms_parser::read_listing(data, &limits) {
        // A chart the list keeps is never refused for a 2P lane.
        assert!(
            listing.mode == bms_parser::ChartMode::TwoPlayer
                || !matches!(&loaded, Err(LoadError::Unsupported { what, .. })
                    if what.starts_with("2P lane")),
            "a chart with no two-player object is refused for one"
        );
    }
    // The summary is the load's: only what needs the file system differs.
    let summary = bms_parser::summarize(data, &limits);
    match &loaded {
        Ok(l) => assert_eq!(summary, Ok(bms_model::ChartSummary::of(&l.chart))),
        Err(LoadError::SoundPathEscapes { .. } | LoadError::ChartDir { .. }) => {}
        Err(e) => assert_eq!(summary.as_ref().err(), Some(e)),
    }
    let loaded = loaded?;
    check_loaded(&loaded.chart, bms_model::Speed::NORMAL);
    let speed = practice_speed(data);
    let opts = LoadOptions {
        speed,
        ..LoadOptions::default()
    };
    // A chart too long for the speed is refused there, and only there.
    match bms_parser::load_bms(data, chart_dir(), &opts) {
        Ok(mapped) => {
            check_loaded(&mapped.chart, speed);
            let (one, at) = (&loaded.chart.timeline, &mapped.chart.timeline);
            assert_eq!(one.points().len(), at.points().len());
            for (a, b) in one.points().iter().zip(at.points()) {
                assert_eq!((a.pos, a.tempo), (b.pos, b.tempo), "a mapped row moved");
                if speed.twentieths() <= 20 {
                    assert!(b.at >= a.at, "a slower row is earlier");
                } else {
                    assert!(b.at <= a.at, "a faster row is later");
                }
            }
            assert_eq!(one.bar_ids(), at.bar_ids());
            let (one_notes, at_notes) = (&loaded.chart.lanes, &mapped.chart.lanes);
            assert!(
                one_notes
                    .iter()
                    .flatten()
                    .map(|n| n.anchors)
                    .eq(at_notes.iter().flatten().map(|n| n.anchors)),
                "a mapped note moved rows"
            );
            assert_eq!(
                loaded.chart.identity.resolved_hash,
                mapped.chart.identity.resolved_hash
            );
        }
        Err(LoadError::TooLongAtSpeed { .. }) => {}
        Err(e) => panic!("loads at 1x but not at {}/20: {e}", speed.twentieths()),
    }
    Ok(loaded)
}

/// The practice speed `bms_load` also loads `data` at: a step of the
/// ladder (50 % to 150 %) from the bytes' sum, so every step is reached
/// and the same input always takes the same one.
#[must_use]
pub fn practice_speed(data: &[u8]) -> bms_model::Speed {
    let sum = data
        .iter()
        .fold(0_u32, |s, &b| s.wrapping_add(u32::from(b)));
    bms_model::Speed::from_twentieths(10 + sum % 21).unwrap_or(bms_model::Speed::NORMAL)
}

/// The invariants of a chart loaded at `speed`: base and link validity,
/// the row cap, the tempo metadata, and presentation validity agreeing
/// with [`endpoint_conflict`] (the loader never refuses a conflict).
fn check_loaded(chart: &bms_model::Chart, speed: bms_model::Speed) {
    assert_eq!(chart.validate(), Ok(()), "a loaded chart is valid");
    assert_eq!(
        chart.validate_presentation_links(),
        Ok(()),
        "a loaded chart's links hold"
    );
    let t = &chart.timeline;
    assert!(t.points().len() <= bms_model::MAX_TIMELINE_POINTS);
    let meta = t.meta();
    assert!(meta.min <= meta.initial && meta.initial <= meta.max);
    assert_eq!(meta.speed, speed);
    match chart.validate_presentation() {
        Ok(()) => assert!(!endpoint_conflict(chart), "a conflict passed"),
        Err(bms_model::PresentationError::EndpointConflict { .. }) => {
            assert!(endpoint_conflict(chart), "a conflict without one");
        }
        Err(e) => panic!("links passed but presentation failed: {e}"),
    }
}

/// The presentation oracle: whether two note endpoints of `chart` sit at
/// one µs at different positions, by sorting every endpoint.
fn endpoint_conflict(chart: &bms_model::Chart) -> bool {
    let mut ends: Vec<(i64, i128)> = chart
        .lanes
        .iter()
        .flatten()
        .flat_map(|n| [(n.time.0, n.pos.0), (n.end().0, n.end_pos().0)])
        .collect();
    ends.sort_unstable();
    ends.windows(2)
        .any(|w| w[0].0 == w[1].0 && w[0].1 != w[1].1)
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

/// What an imported skin draws: for a play skin, the quads of frames at a
/// few times of a 100-note play with a perfect judgment at each, each
/// frame within the skin's quad capacity, its loading state built too,
/// within the same capacities; for a selection skin, its frames over a
/// list; for a result skin, frames at 0 and 500 ms of a ready view and of
/// one with no result ([`result_view`]). `seed` names one more readout frame's gain, covers,
/// duration variants and native block.
///
/// # Panics
///
/// If the atlas, the play or the text table is refused, a play or loading
/// frame holds more quads than the skin's capacity, or a refused loading
/// state builds again.
#[must_use]
pub fn draw(imported: &Imported, seed: &[u8]) -> Vec<usize> {
    match imported.skin.kind() {
        ScreenKind::Selection => draw_selection(&imported.skin),
        ScreenKind::Result => draw_result(&imported.skin),
        ScreenKind::Decision => draw_decision(&imported.skin),
        ScreenKind::Play => draw_skin(imported, seed),
    }
}

/// `skin`'s atlas built within the harness's limits: its uv table.
///
/// # Panics
///
/// If the atlas is refused.
fn atlas_uvs(skin: &skin_model::Skin) -> skin_model::UvTable {
    let limits = skin_model::AtlasLimits {
        max_side: 4_096,
        max_layers: 8,
        reserved_layers: 1,
    };
    skin.atlas(skin_model::Variant::Lo, limits, &|| false)
        .expect("an imported sheet builds its atlas")
        .into_parts()
        .1
}

/// A result view of a 600-note play whose best is known with no BP
/// (hidden): ready, or with no result when not `ready`.
#[must_use]
pub fn result_view(ready: bool) -> skin_model::ResultView {
    use eval::Tally;
    use rules::PerJudgment;
    use skin_model::{
        BestFigures, BestView, LampKey, ResultChart, ResultNumber, ResultRate, ResultScore,
        ResultStatus, ResultView, TimingStats, Unavailable,
    };
    let counts = PerJudgment {
        perfect: 500,
        great: 60,
        good: 20,
        bad: 10,
        poor: 10,
    };
    ResultView {
        status: if ready {
            ResultStatus::Ready
        } else {
            ResultStatus::Unavailable(Unavailable::Failed)
        },
        chart: ResultChart {
            level: Some(12),
            total: Some(300),
            notes: Some(600),
            ..ResultChart::default()
        },
        score: ResultScore {
            tally: Tally {
                counts,
                ex_score: 1_060,
                bp: 20,
                max_combo: 200,
                early: counts,
                ..Tally::default()
            },
            gauge_permille: 805,
            clear: LampKey::Normal,
            cleared: true,
            rate: Some(ResultRate {
                rate_bp: 8_833,
                grade: 6,
            }),
            next_rank: 17,
            duration_us: Some(4_321),
        },
        best: BestView::Known(BestFigures {
            ex: 0,
            ex_diff: 1_060,
            bp: ResultNumber::Hidden,
            bp_diff: ResultNumber::Hidden,
            clear: LampKey::NoPlay,
            ex_updated: true,
            bp_updated: true,
            rate: None,
        }),
        timing: Some(TimingStats {
            samples: 590,
            mean_x100: -149,
            sd_x100: 1_234,
        }),
        timelines: None,
        gauge: None,
        deviations: None,
        timing_areas: None,
    }
}

/// What an imported result skin draws: the quads of frames at a few
/// times over a ready view and a view with no result, the fade-out
/// started at the later one, every frame within the skin's capacities.
///
/// # Panics
///
/// If the atlas or the result state is refused, a frame is refused or
/// holds more quads than the skin's capacity.
fn draw_result(skin: &skin_model::Skin) -> Vec<usize> {
    use bms_model::TimeUs;
    use skin_model::{Layout, QuadBatch, ResultFrame, ResultLabels, ResultTimers};
    let uvs = atlas_uvs(skin);
    let labels = ResultLabels {
        title: "t".to_owned(),
        ..ResultLabels::default()
    };
    let mut res = skin.prepare_result();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = Layout::AUTHOR;
    let mut drawn = Vec::new();
    for ready in [true, false] {
        skin.update_result(&mut res, &result_view(ready), &labels)
            .expect("an imported result skin takes a view");
        let requests = skin.result_text_requests(&res, [1_920, 1_080], &layout);
        assert!(requests.len() <= skin_model::MAX_TEXT_ELEMENTS);
        for ms in [0, 500] {
            batch.clear();
            let frame = ResultFrame {
                now: TimeUs(ms * 1_000),
                target: [1_920, 1_080],
                layout: &layout,
                timers: ResultTimers {
                    screen: Some(TimeUs(0)),
                    fadeout: (ms > 0).then_some(TimeUs(0)),
                },
            };
            skin.build_result(&mut res, &frame, &uvs, &mut batch)
                .expect("an imported result skin builds");
            check_batch(skin, &batch);
            drawn.push(batch.quads().len());
        }
    }
    drawn
}

/// What an imported decision skin draws: the quads of frames at the
/// choice, at the fade-out's start and at the screen's end, over the
/// chart's labels with each outline line attached and the bitmap lines
/// laid out, every frame within the skin's capacities.
///
/// # Panics
///
/// If the atlas, the labels or the text table is refused, or a frame is
/// refused or holds more quads than the skin's capacity.
fn draw_decision(skin: &skin_model::Skin) -> Vec<usize> {
    use bms_model::TimeUs;
    use skin_model::{
        DecisionFrame, DecisionTimers, FrameUv, Layout, QuadBatch, ResultLabels, TextImage,
        TextTable,
    };
    let uvs = atlas_uvs(skin);
    let labels = ResultLabels {
        title: "t".to_owned(),
        artist: "a".to_owned(),
        ..ResultLabels::default()
    };
    let mut dec = skin.prepare_decision();
    skin.update_decision(&mut dec, &labels)
        .expect("an imported decision skin takes labels");
    let layout = Layout::AUTHOR;
    let mut lines = vec![None; skin.text_slots()];
    for r in skin.decision_text_requests(&dec, [1_920, 1_080], &layout) {
        let uv = FrameUv {
            uv: [0.0, 0.0, 0.5, 0.5],
            layer: 1,
        };
        if let Some(slot) = lines.get_mut(usize::from(r.slot)) {
            *slot = TextImage::new(uv, [0.0, 0.0, 1.0, 1.0], 1.0);
        }
    }
    skin.attach_decision_text(&mut dec, TextTable::new(lines))
        .expect("one line per slot");
    skin.attach_decision_bitmap_text(&mut dec);
    let t = skin.decision_timing();
    let end = TimeUs(t.scene.0 + t.fadeout.0);
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let mut drawn = Vec::new();
    for now in [TimeUs(0), t.scene, end] {
        batch.clear();
        let frame = DecisionFrame {
            now,
            target: [1_920, 1_080],
            layout: &layout,
            timers: DecisionTimers {
                screen: Some(TimeUs(0)),
                fadeout: (now >= t.scene).then_some(t.scene),
            },
        };
        skin.build_decision(&mut dec, &frame, &uvs, &mut batch)
            .expect("an imported decision skin builds");
        check_batch(skin, &batch);
        drawn.push(batch.quads().len());
    }
    drawn
}

/// What an imported selection skin draws: the quads of frames at a few
/// times over a view of a chart, a folder and the "all charts" row, the
/// cursor on each, every frame within the skin's capacities; a press on
/// each frame's place changes nothing it drew.
///
/// # Panics
///
/// If the atlas or the selection state is refused, a frame is refused or
/// holds more quads than the skin's capacity, or a press changes the
/// state.
fn draw_selection(skin: &skin_model::Skin) -> Vec<usize> {
    use bms_model::TimeUs;
    use skin_model::{
        ChartLabels, Layout, QuadBatch, Ratio, RowKey, RowKind, RowScore, RowView, SelectedChart,
        SelectionFrame, SelectionLabels, SelectionPlace, SelectionTimers, SelectionView,
    };

    let uvs = atlas_uvs(skin);
    let row = |kind: RowKind, title: &str| RowView {
        key: match kind {
            RowKind::Chart => RowKey::Chart(format!("{title}.bms").into()),
            RowKind::All { .. } => RowKey::All,
            _ => RowKey::Folder(skin_model::FolderId::new(title.into())),
        },
        kind,
        title: title.to_owned(),
        subtitle: String::new(),
        artist: String::new(),
        level: "12".to_owned(),
        level_value: 12,
        difficulty: Some(3),
        score: RowScore::NoBest { plays: 1 },
        features: None,
        random: false,
    };
    let rows = vec![
        row(RowKind::Chart, "c"),
        row(RowKind::Folder { charts: 3 }, "f"),
        row(RowKind::All { charts: 9 }, "a"),
    ];
    let labels = SelectionLabels {
        all_charts: "all".to_owned(),
        search_results: "found".to_owned(),
    };
    let mut sel = skin.prepare_selection();
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let layout = Layout::AUTHOR;
    let mut drawn = Vec::new();
    for cursor in 0..rows.len() {
        let selected = (cursor == 0).then(|| SelectedChart {
            labels: ChartLabels {
                title: "c".to_owned(),
                level_value: 12,
                ..ChartLabels::default()
            },
            summary: None,
            random: false,
            max_ex: Some(300),
            score: RowScore::NoBest { plays: 1 },
            judge_class: Some(3),
        });
        let view = SelectionView {
            rows: rows.clone(),
            cursor: Some(cursor),
            position: Ratio::new(u64::try_from(cursor).unwrap_or(0), 2),
            place: SelectionPlace::Folder(vec!["f".to_owned()]),
            query: "q".to_owned(),
            selected,
        };
        skin.update_selection(&mut sel, &view, &labels)
            .expect("an imported selection skin takes a view");
        let requests = skin.selection_text_requests(&sel, [1_920, 1_080], &layout);
        assert!(requests.texts.len() <= skin_model::MAX_TEXT_ELEMENTS);
        assert!(requests.rows.len() <= skin_model::MAX_PLACEMENTS);
        for ms in [0, 500] {
            batch.clear();
            let frame = SelectionFrame {
                now: TimeUs(ms * 1_000),
                target: [1_920, 1_080],
                layout: &layout,
                timers: SelectionTimers {
                    screen: Some(TimeUs(0)),
                    change: Some(TimeUs(0)),
                    panels: [Some(TimeUs(0)), None, None],
                },
            };
            skin.build_selection(&mut sel, &frame, &uvs, &mut batch)
                .expect("an imported selection skin builds");
            check_batch(skin, &batch);
            drawn.push(batch.quads().len());
            let before = format!("{sel:?}");
            for q in batch.quads() {
                let _ = skin.slider_at(&sel, [q.rect[0], q.rect[1]], [1_920, 1_080]);
            }
            assert_eq!(format!("{sel:?}"), before, "a press changed the state");
        }
    }
    drawn
}

/// [`draw`] of a play skin.
fn draw_skin(imported: &Imported, seed: &[u8]) -> Vec<usize> {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
    use rules::{GaugeValue, Judgment, PerJudgment};
    use skin_model::{
        Attempt, ChartText, FrameUv, GaugeDesc, GaugeLook, LaneView, Layout, PlayContext,
        QuadBatch, Side, SkinFrame, TextImage, TextTable,
    };

    let skin = &imported.skin;
    let uvs = atlas_uvs(skin);
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
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        meta: skin_model::ChartMetadata::UNKNOWN,
        score: skin_model::PlayScore::NONE,
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
        ..Default::default()
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
                presses: 0,
            },
        }];
        batch.clear();
        // The play view's note facts after the judgment.
        let mut notes = eval::NoteFacts::new(&chart);
        notes.set_done(lane, 0);
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &effects,
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
            closing: None,
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
            gain: GAIN_600_MS,
            cover,
            lift,
            hidden: if hidden_on { hidden } else { 0 },
        };
        let mut readouts = readouts_for(frame_lanes);
        if !hidden_on {
            readouts.covers.hidden = hidden.min(1_000);
            readouts.covers.hidden_enabled = false;
        }
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: frame_lanes,
            readouts,
            playing: true,
            closing: None,
        };
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        check_batch(skin, &batch);
    }
    rate_and_kind_probes(skin, &chart, &uvs, &mut batch);
    readout_probes(skin, &chart, &uvs, &mut batch, seed);
    choice_probes(skin, &chart, &uvs, &mut batch);
    bar_probes(skin, &ctx, (&tally, &states), &uvs, &mut batch);
    long_note_probes(skin, &ctx, (&tally, &states), &uvs, &mut batch);
    loading_probes(skin, (&text, &table), &gauges[0], &uvs, &mut batch);
    digit_probes(skin, &ctx, &states, &uvs, &mut batch);
    grade_anchor_probes(skin, &ctx, &uvs, &mut batch);
    gauge_probes(skin, &chart, &uvs, &mut batch);
    lane_order_probes(skin, &ctx, &uvs, &mut batch);
    counts
}

/// Chords on all eight lanes, every tenth a long note, at shared and
/// distinct times: frames as they scroll past, with and without scroll
/// left, each finite and inside the skin's capacities whatever the
/// lanes' note order.
fn lane_order_probes(
    skin: &skin_model::Skin,
    ctx: &skin_model::PlayContext<'_>,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{LaneView, Layout, SkinFrame};

    let mut b = ChartBuilder::new(150_000);
    for k in 0..200_i64 {
        for l in 0..8_u8 {
            let Some(lane) = Lane::new(l) else {
                continue;
            };
            let t = 1_000_000 + k * 10_000 + i64::from(l % 3) * 1_000;
            b = if k % 10 == 0 {
                b.long(lane, TimeUs(t), TimeUs(t + 5_000))
            } else {
                b.tap(lane, TimeUs(t))
            };
        }
    }
    let chart = b.build().expect("chords on every lane");
    let mut play = skin
        .prepare(&chart, ctx)
        .expect("an imported skin prepares");
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
        ..Default::default()
    };
    let states = [GaugeState {
        value: GaugeValue(0),
        reference: GaugeValue(0),
        failed_at: None,
    }];
    for (k, cover) in [0_u16, 300, 1_000, 0, 500].into_iter().enumerate() {
        let view = LaneView {
            gain: GAIN_600_MS,
            cover,
            lift: 0,
            hidden: 0,
        };
        batch.clear();
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(800_000 + i64::try_from(k).unwrap_or(0) * 400_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: view,
            readouts: readouts_for(view),
            playing: true,
            closing: None,
        };
        skin.build(&mut play, &frame, uvs, batch)
            .expect("the frame builds");
        check_batch(skin, batch);
    }
}

/// The shown gauge at raw values around its thresholds: a maximum that
/// divides no segment count, values from below 0 to past it and the
/// `i64` extremes, borders at, inside and past it, each frame inside the
/// skin's capacities.
fn gauge_probes(
    skin: &skin_model::Skin,
    chart: &bms_model::Chart,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::TimeUs;
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{Attempt, GaugeDesc, GaugeLook, Layout, PlayContext, Side, SkinFrame};

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
        ..Default::default()
    };
    for (max, border) in [(7, 0), (7, 7), (7, 3), (i64::MAX, i64::MAX / 2)] {
        let descs = [GaugeDesc {
            start: GaugeValue(0),
            max: GaugeValue(max),
            look: GaugeLook::Hard,
            border: GaugeValue(border),
        }];
        let ctx = PlayContext {
            autoplay: false,
            gauges: &descs,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
            meta: skin_model::ChartMetadata::UNKNOWN,
            score: skin_model::PlayScore::NONE,
        };
        let mut play = skin
            .prepare(chart, &ctx)
            .expect("an imported skin prepares");
        let values = [i64::MIN, -1, 0, 1, 3, 4, 6, 7, 8, max - 1, max, i64::MAX];
        for (k, value) in values.into_iter().enumerate() {
            let states = [GaugeState {
                value: GaugeValue(value),
                reference: GaugeValue(value),
                failed_at: None,
            }];
            batch.clear();
            let notes = eval::NoteFacts::default();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                screen_us: 0,
                now: TimeUs(i64::try_from(k).unwrap_or(0) * 16_000),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &states,
                gauge: 0,
                effects: &[],
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                turntable: 0,
                lanes: LANES_OFF,
                readouts: readouts_for(LANES_OFF),
                playing: true,
                closing: None,
            };
            skin.build(&mut play, &frame, uvs, batch)
                .expect("the frame builds");
            check_batch(skin, batch);
        }
    }
}

/// The rects a frame of `imported` draws at a PERFECT with combo 12, the
/// gauge full first when `full`, at the judgment's time, with no cover.
#[must_use]
pub fn judged_rects(imported: &Imported, full: bool) -> Vec<[f32; 4]> {
    use bms_model::{Lane, TimeUs};
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
    use rules::{GaugeValue, Judgment, PerJudgment};
    use skin_model::{
        Attempt, GaugeDesc, GaugeLook, Layout, PlayContext, QuadBatch, Side, SkinFrame,
    };

    let skin = &imported.skin;
    let uvs = atlas_uvs(skin);
    let chart = bms_model::ChartBuilder::new(150_000)
        .build()
        .expect("an empty chart");
    let value = if full {
        GaugeValue::FULL
    } else {
        GaugeValue(20 * GaugeValue::PERCENT)
    };
    let gauges = [GaugeDesc {
        start: value,
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }];
    let ctx = PlayContext {
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        meta: skin_model::ChartMetadata::UNKNOWN,
        score: skin_model::PlayScore::NONE,
    };
    let mut play = skin
        .prepare(&chart, &ctx)
        .expect("an imported skin prepares");
    let states = [GaugeState {
        value,
        reference: value,
        failed_at: None,
    }];
    let tally = Tally {
        counts: PerJudgment {
            perfect: 12,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 24,
        bp: 0,
        combo: 12,
        max_combo: 12,
        empty_poor: 0,
        ..Default::default()
    };
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let event = |ms: i64, kind| JudgeEvent {
        seq: 0,
        time: TimeUs(ms * 1_000),
        origin: None,
        kind,
    };
    let gauge = event(
        900,
        JudgeEventKind::Gauge {
            gauge: 0,
            value,
            reference: value,
        },
    );
    let judged = event(
        1_000,
        JudgeEventKind::Judged {
            lane: Lane::new(1).expect("lane 1"),
            note: 0,
            judgment: Judgment::Perfect,
            deviation: Some(TimeUs(0)),
            release: None,
            combo: 12,
            presses: 0,
        },
    );
    for (ms, effects) in [(900, [gauge]), (1_000, [judged])] {
        batch.clear();
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &effects,
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
            closing: None,
        };
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
    }
    batch.quads().iter().map(|q| q.rect).collect()
}

/// Every judgment, with the gauge full from the fourth, each judged at one
/// time and drawn at later ones on every target: grades, their
/// attachments and the combos on their own clocks, followers moving, each
/// frame finite and inside the skin's capacities.
fn grade_anchor_probes(
    skin: &skin_model::Skin,
    ctx: &skin_model::PlayContext<'_>,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::{Lane, TimeUs};
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
    use rules::{GaugeValue, Judgment, PerJudgment};
    use skin_model::{LaneView, Layout, SkinFrame};

    let chart = bms_model::ChartBuilder::new(150_000)
        .build()
        .expect("an empty chart");
    let mut play = skin
        .prepare(&chart, ctx)
        .expect("an imported skin prepares");
    let lane = Lane::new(1).expect("lane 1");
    let full = [GaugeState {
        value: GaugeValue::FULL,
        reference: GaugeValue::FULL,
        failed_at: None,
    }];
    let tally = Tally {
        counts: PerJudgment {
            perfect: 12,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        },
        ex_score: 24,
        bp: 0,
        combo: 12,
        max_combo: 12,
        empty_poor: 0,
        ..Default::default()
    };
    let judgments = [
        Judgment::Perfect,
        Judgment::Great,
        Judgment::Good,
        Judgment::Perfect,
        Judgment::Bad,
        Judgment::Poor,
    ];
    let mut now = 0_i64;
    for (k, judgment) in judgments.into_iter().enumerate() {
        let gauge = (k == 3).then_some(JudgeEvent {
            seq: 0,
            time: TimeUs(now),
            origin: None,
            kind: JudgeEventKind::Gauge {
                gauge: 0,
                value: GaugeValue::FULL,
                reference: GaugeValue::FULL,
            },
        });
        let judged = JudgeEvent {
            seq: 0,
            time: TimeUs(now),
            origin: None,
            kind: JudgeEventKind::Judged {
                lane,
                note: 0,
                judgment,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: 12,
                presses: 0,
            },
        };
        let effects: Vec<JudgeEvent> = gauge.into_iter().chain([judged]).collect();
        for (step, (target, cover)) in [
            ([1_920, 1_080], 0),
            ([640, 360], 300),
            ([3_840, 2_160], 1_000),
            ([1, 1], 0),
        ]
        .into_iter()
        .enumerate()
        {
            let view = LaneView {
                gain: GAIN_600_MS,
                cover,
                lift: cover / 2,
                hidden: 0,
            };
            batch.clear();
            let notes = eval::NoteFacts::default();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                screen_us: 0,
                now: TimeUs(now),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &full,
                gauge: 0,
                effects: if step == 0 { &effects } else { &[] },
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target,
                layout: &Layout::AUTHOR,
                turntable: 0,
                lanes: view,
                readouts: readouts_for(view),
                playing: true,
                closing: None,
            };
            skin.build(&mut play, &frame, uvs, batch)
                .expect("the frame builds");
            check_batch(skin, batch);
            // Uneven steps, so the grade's and the combo's clocks part.
            now += 37_000;
        }
    }
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
            gain: GAIN_600_MS,
            cover,
            lift: 0,
            hidden: 0,
        };
        let frame = LoadingFrame {
            app: skin_model::AppReadouts::LAUNCH,
            display_offset: TimeUs(0),
            fix_hispeed: 0,
            autoplay: false,
            now: TimeUs(ms * 1_000),
            load: LoadState {
                since: TimeUs(0),
                percent,
            },
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes,
            covers: readouts_for(lanes).covers,
            gauge,
            play_level: None,
            difficulty: None,
            bga: None,
            length: None,
        };
        skin.build_loading(&mut loading, &frame, uvs, batch)
            .expect("the loading frame builds");
        check_batch(skin, batch);
    }
    // Covers the frame's lanes do not show: refused, then for good.
    let mut frame = LoadingFrame {
        app: skin_model::AppReadouts::LAUNCH,
        display_offset: TimeUs(0),
        fix_hispeed: 0,
        autoplay: false,
        now: TimeUs(6_000_000),
        load: LoadState {
            since: TimeUs(0),
            percent: 100,
        },
        target: [1_920, 1_080],
        layout: &Layout::AUTHOR,
        turntable: 0,
        lanes: LANES_OFF,
        covers: readouts_for(LANES_OFF).covers,
        gauge: Some(normal),
        play_level: None,
        difficulty: None,
        bga: None,
        length: None,
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

/// Numbers and combos over a few values, signs and combo lengths, at
/// targets smaller, equal to and larger than the skin's: digit offsets
/// move cells by target pixels, so each frame stays inside the skin's
/// capacities and on the finite plane.
fn digit_probes(
    skin: &skin_model::Skin,
    ctx: &skin_model::PlayContext<'_>,
    states: &[eval::GaugeState],
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
) {
    use bms_model::{Lane, TimeUs};
    use eval::{JudgeEvent, JudgeEventKind, Tally};
    use rules::{Judgment, PerJudgment};
    use skin_model::{Layout, SkinFrame};

    let chart = bms_model::ChartBuilder::new(150_000)
        .build()
        .expect("an empty chart");
    let mut play = skin
        .prepare(&chart, ctx)
        .expect("an imported skin prepares");
    let lane = Lane::new(1).expect("lane 1");
    for (k, (value, target)) in [0_u32, 7, 12, 1_234_567]
        .into_iter()
        .flat_map(|v| [[1_920, 1_080], [640, 360], [3_840, 2_160], [1, 1]].map(|t| (v, t)))
        .enumerate()
    {
        let now = TimeUs(i64::try_from(k).unwrap_or(0) * 100_000);
        let tally = Tally {
            counts: PerJudgment {
                perfect: value,
                great: 0,
                good: 0,
                bad: 0,
                poor: 0,
            },
            ex_score: value,
            bp: 0,
            combo: value,
            max_combo: value,
            empty_poor: 0,
            ..Default::default()
        };
        let effects = [JudgeEvent {
            seq: 0,
            time: now,
            origin: None,
            kind: JudgeEventKind::Judged {
                lane,
                note: 0,
                judgment: Judgment::Perfect,
                deviation: Some(TimeUs(0)),
                release: None,
                combo: value,
                presses: 0,
            },
        }];
        batch.clear();
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now,
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: states,
            gauge: 0,
            effects: &effects,
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target,
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
            closing: None,
        };
        skin.build(&mut play, &frame, uvs, batch)
            .expect("the frame builds");
        check_batch(skin, batch);
    }
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
            presses: 0,
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
                gain: GAIN_600_MS,
                cover,
                lift: 0,
                hidden: 0,
            };
            // The play view's note facts: held from its head on.
            let mut notes = eval::NoteFacts::new(&chart);
            if ms >= 1_000 {
                notes.set_done(lane, 0);
                let hold = eval::NoteHold {
                    note: 0,
                    start: TimeUs(1_000_000),
                    end: TimeUs(4_000_000),
                };
                notes.set_hold(lane, Some(hold));
            }
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                screen_us: 0,
                now: TimeUs(ms * 1_000),
                display_offset: TimeUs(0),
                tally,
                gauges,
                gauge: 0,
                effects,
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                turntable: 0,
                lanes,
                readouts: readouts_for(lanes),
                playing: true,
                closing: None,
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

    let mut builder = ChartBuilder::new(150_000)
        .stop(TimeUs(1_000_000), TimeUs(500_000))
        .tap(Lane::new(1).expect("lane 1"), TimeUs(3_000_000));
    for i in 0..40 {
        builder = builder.bar(TimeUs(i * 125_000));
    }
    for _ in 0..8 {
        builder = builder.bar(TimeUs(3_000_000));
    }
    let chart = builder.build().expect("the bar chart is valid");
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
            gain: GAIN_600_MS,
            cover,
            lift,
            hidden: 0,
        };
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(ms * 1_000),
            display_offset: TimeUs(offset * 1_000),
            tally,
            gauges,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes,
            readouts: readouts_for(lanes),
            playing: true,
            closing: None,
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
        autoplay: false,
        gauges: &descs,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        meta: skin_model::ChartMetadata::UNKNOWN,
        score: skin_model::PlayScore::NONE,
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
            ..Default::default()
        };
        batch.clear();
        let notes = eval::NoteFacts::default();
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(now),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            notes: &notes,
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
            closing: None,
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
        ..Default::default()
    };
    for counted in [false, true] {
        let ctx = PlayContext {
            autoplay: false,
            gauges: &descs,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: counted,
            empty_poor_counted: counted,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
            meta: skin_model::ChartMetadata::UNKNOWN,
            score: skin_model::PlayScore::NONE,
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
            let notes = eval::NoteFacts::default();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                screen_us: 0,
                now: TimeUs(2_000_000),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: snapshot,
                gauge: k,
                effects: &[],
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                turntable: 0,
                lanes: LANES_OFF,
                readouts: readouts_for(LANES_OFF),
                playing: true,
                closing: None,
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

/// The gain that puts 600 ms of 150 BPM over the original lane: `H` 8/3.
const GAIN_600_MS: Option<skin_model::Gain> = skin_model::Gain::new(8, 3);

/// Lanes at 600 ms with no cover.
const LANES_OFF: skin_model::LaneView = skin_model::LaneView {
    gain: GAIN_600_MS,
    cover: 0,
    lift: 0,
    hidden: 0,
};

/// The readouts of a frame showing `view`: no tempo, hi-speed or duration,
/// every cover chosen on and stored at the amount `view` draws (capped at
/// 1 000), so the frame is consistent.
fn readouts_for(view: skin_model::LaneView) -> skin_model::FrameReadouts {
    skin_model::FrameReadouts {
        covers: skin_model::CoverSettings {
            cover: view.cover.min(1_000),
            lift: view.lift.min(1_000),
            hidden: view.hidden.min(1_000),
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
        ..skin_model::FrameReadouts::UNAVAILABLE
    }
}

/// The readouts at their extremes, each frame consistent: explicit
/// tempo, hi-speed and duration, hi-speed unavailable, every cover at
/// 1 000, covers off with stored amounts, durations of wide integer
/// ratios and the largest readouts, the lanes' gain at both ends and
/// absent, the eight duration variants and the native block known and
/// not, before, during and past the chart, then one frame whose gain,
/// covers, variants and native block come from `seed`, each inside the
/// schema's ranges and the skin's capacities.
fn readout_probes(
    skin: &skin_model::Skin,
    chart: &bms_model::Chart,
    uvs: &skin_model::UvTable,
    batch: &mut skin_model::QuadBatch,
    seed: &[u8],
) {
    use bms_model::TimeUs;
    use eval::{GaugeState, Tally};
    use rules::{GaugeValue, PerJudgment};
    use skin_model::{
        Attempt, BpmRange, CoverSettings, FrameReadouts, Gain, GaugeDesc, GaugeLook, LaneView,
        Layout, NativeReadouts, PLAY, PlayContext, READOUT_MAX, Side, SkinFrame, field,
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
        ..Default::default()
    };
    let top = READOUT_MAX.unsigned_abs();
    // A ratio as the producer gives it: unavailable where it rounds past
    // the maximum or has no denominator.
    let ratios = [
        (6_675, 10),
        (u128::MAX, 1),
        (u128::MAX, u128::MAX),
        (u128::from(top) * 2 + 1, 2),
        (1, u128::MAX),
        (0, 7),
        (u128::from(top) * 2 - 1, 2),
        (5, 0),
    ];
    let gains = [
        Gain::new(8, 3),
        Gain::new(1, 100),
        Gain::new(20, 1),
        None,
        Gain::new(u64::MAX, u64::MAX / 20),
        Gain::new(u64::MAX / 100 + 1, u64::MAX),
    ];
    let probes = [
        (Some(15_000), Some(105), 0, 1_000, 1_000, 1_000, true, false),
        (Some(0), None, 1, 500, 250, 0, false, true),
        (Some(top), Some(top), 2, 1_000, 0, 1_000, true, true),
        (None, Some(0), 3, 0, 1_000, 500, false, false),
        (Some(1), None, 4, 251, 999, 1, true, false),
        (None, None, 5, 0, 0, 0, false, false),
    ];
    let frames = [-1_000_i64, 0, 999, 1_500, 2_000, 9_000_000]
        .into_iter()
        .zip(probes)
        .enumerate()
        .map(
            |(k, (now_ms, (bpm, hs, ratio, cover, lift, hidden, on, adjusting)))| {
                let covers = CoverSettings {
                    cover,
                    lift,
                    hidden,
                    cover_enabled: on,
                    lift_enabled: !on,
                    hidden_enabled: on,
                    adjusting,
                };
                let at = |i: usize| {
                    ratios
                        .get((ratio + i) % ratios.len())
                        .copied()
                        .and_then(produced)
                };
                let floating = k % 2 == 0;
                let readouts = FrameReadouts {
                    fix_hispeed: 0,
                    bpm_current_x100: bpm,
                    hispeed_x100: hs,
                    duration: at(0),
                    covers,
                    duration_variants: std::array::from_fn(|i| at(i + 1)),
                    native: NativeReadouts {
                        duration: at(k),
                        target_green: floating.then_some([1, 300, 6_000][k % 3]),
                        floating,
                    },
                };
                (now_ms, gains.get(k).copied().flatten(), readouts)
            },
        );
    let (gain, seeded) = seeded_readouts(seed);
    let frames: Vec<_> = frames.chain([(1_000, gain, seeded)]).collect();
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
            autoplay: false,
            gauges: &descs,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(0),
            bpm_range: range,
            bpm_whole_change: None,
            play_level: None,
            meta: skin_model::ChartMetadata::UNKNOWN,
            score: skin_model::PlayScore::NONE,
        };
        let mut play = skin
            .prepare(chart, &ctx)
            .expect("an imported skin prepares");
        for &(now_ms, gain, readouts) in &frames {
            let c = readouts.covers;
            let effective = |enabled: bool, v: u16| if enabled { v } else { 0 };
            let lanes = LaneView {
                gain,
                cover: effective(c.cover_enabled, c.cover),
                lift: effective(c.lift_enabled, c.lift),
                hidden: effective(c.hidden_enabled, c.hidden),
            };
            batch.clear();
            let notes = eval::NoteFacts::default();
            let frame = SkinFrame {
                app: skin_model::AppReadouts::LAUNCH,
                state_us: 0,
                screen_us: 0,
                now: TimeUs(now_ms * 1_000),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &states,
                gauge: 0,
                effects: &[],
                notes: &notes,
                offsets: skin_model::Offsets::NONE,
                input: &eval::InputFacts::NONE,
                complete: false,
                target: [1_920, 1_080],
                layout: &Layout::AUTHOR,
                turntable: 0,
                lanes,
                readouts,
                playing: true,
                closing: None,
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
            let valid = |known: bool| Some(i32::from(known));
            assert_eq!(
                read(field::HISPEED_VALID),
                valid(readouts.hispeed_x100.is_some())
            );
            assert_eq!(
                read(field::DURATION_VALID),
                valid(readouts.duration.is_some())
            );
            assert_eq!(read(field::BPM_RANGE_VALID), valid(range.is_some()));
            assert_eq!(read(field::COVER_PERMILLE), Some(i32::from(c.cover)));
            for (i, v) in (0_u16..).zip(&readouts.duration_variants) {
                let slot = field::duration_variant(i / 2, i % 2 == 0) + 2;
                assert_eq!(read(slot), valid(v.is_some()), "variant {i}");
            }
            assert_eq!(read(field::NATIVE_HISPEED_VALID), valid(gain.is_some()));
            assert_eq!(
                read(field::NATIVE_DURATION_VALID),
                valid(readouts.native.duration.is_some())
            );
            assert_eq!(
                read(field::NATIVE_TARGET_VALID),
                valid(readouts.native.target_green.is_some())
            );
            assert_eq!(read(field::NATIVE_LIFT_WHITE), Some(i32::from(c.lift)));
            check_batch(skin, batch);
        }
    }
}

/// A duration ratio as the producer gives it: unavailable where it rounds
/// past the readout maximum or has no denominator.
fn produced((numerator, denominator): (u128, u128)) -> Option<skin_model::DurationRatio> {
    let top = u128::from(skin_model::READOUT_MAX.unsigned_abs());
    (denominator > 0 && numerator / denominator < top).then_some(skin_model::DurationRatio {
        numerator,
        denominator,
    })
}

/// The bytes [`seeded_readouts`] reads: the gain's two terms, the switches,
/// three covers, the target, the tempo and the hi-speed, then ten ratios
/// (the eight variants, the native and the source duration) of two
/// 16-byte terms each.
#[cfg(test)]
pub(crate) const SEEDED_BYTES: usize = 8 + 8 + 1 + 3 * 2 + 2 + 4 + 4 + 10 * 32;

/// One readout frame's gain and readouts from `seed`, read cyclically, so
/// any nonempty input names a gain, covers, eight variants, a native
/// block and a source duration, each consistent as a producer gives it;
/// an empty seed reads as zeros.
pub(crate) fn seeded_readouts(
    seed: &[u8],
) -> (Option<skin_model::Gain>, skin_model::FrameReadouts) {
    use skin_model::{CoverSettings, FrameReadouts, Gain, NativeReadouts, READOUT_MAX};

    let top = READOUT_MAX.unsigned_abs();
    let mut bytes = seed.iter().copied().cycle();
    let mut next = |n: usize| {
        (0..n).fold(0_u128, |v, _| {
            v << 8 | u128::from(bytes.next().unwrap_or(0))
        })
    };
    let small = |v: u128| u16::try_from(v % 1_001).unwrap_or(0);
    let (num, den) = (next(8), next(8));
    let gain = Gain::new(
        u64::try_from(num).unwrap_or(0),
        u64::try_from(den).unwrap_or(0),
    );
    let flags = next(1);
    let seeded_covers = CoverSettings {
        cover: small(next(2)),
        lift: small(next(2)),
        hidden: small(next(2)),
        cover_enabled: flags & 1 != 0,
        lift_enabled: flags & 2 != 0,
        hidden_enabled: flags & 4 != 0,
        adjusting: flags & 8 != 0,
    };
    let floating = flags & 16 != 0;
    let target = u32::try_from(next(2) % 6_001).unwrap_or(0);
    let bpm = u32::try_from(next(4)).ok().filter(|&b| b <= top);
    let hispeed = u32::try_from(next(4)).ok().filter(|&h| h <= top);
    let mut ratio = || produced((next(16), next(16)));
    let duration_variants = std::array::from_fn(|_| ratio());
    let native = ratio();
    let seeded = FrameReadouts {
        fix_hispeed: 0,
        bpm_current_x100: bpm,
        hispeed_x100: hispeed,
        duration: ratio(),
        covers: seeded_covers,
        duration_variants,
        native: NativeReadouts {
            duration: native,
            target_green: (floating && target > 0).then_some(target),
            floating,
        },
    };
    (gain, seeded)
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
    let imported = import(&mut def, &header, &Config::defaults(&header), header.kind);
    if let Ok(i) = &imported {
        check_usage(i);
        black_box(draw(i, data));
    }
    Ok((configured, imported))
}

/// The files a `skin_lua` input is split into, in order: the entry file,
/// then files it may load.
pub const LUA_FILES: [&str; 4] = ["s.luaskin", "m.lua", "parts/p.lua", "parts/q.lua"];

/// A folder for the Lua skins, rewritten by each input, beside a copy of
/// [`skin_dir`]'s resources and an `outside.lua` in its parent that no
/// loader may reach. Its own folder, so the two skin targets never share
/// files.
fn lua_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let parent = tempfile::Builder::new()
            .prefix("tapestry-fuzz-lua-")
            .tempdir()
            .expect("create a Lua skin folder");
        std::fs::write(parent.path().join("outside.lua"), "return {}").expect("write a file");
        let dir = parent.path().join("skin");
        std::fs::create_dir_all(dir.join("parts")).expect("create the skin folder");
        write_png(&dir.join("strip.png"), 100, 10);
        write_png(&dir.join("grid.png"), 8, 8);
        parent
    })
    .path()
}

/// The configuration that selects each option's last item, so the
/// configured pass sees another `skin_config` than with the defaults.
fn last_items(h: &Header) -> Config {
    Config {
        options: h
            .options
            .iter()
            .map(|o| o.items.last().map(|i| i.op))
            .collect(),
        ..Config::default()
    }
}

/// A candidate's use of the sandbox is within its caps, and the files it
/// consulted are the input's.
fn check_lua(usage: &LuaUsage, dependencies: &[String]) {
    use skin_beatoraja::limits::{MAX_INCLUDE_PARSES, MAX_INPUT_BYTES};
    use skin_beatoraja::lua::limits::{
        MAX_AST_NODES, MAX_CAPTURES, MAX_EXPANDED_NODES, MAX_HOST_CALLS, MAX_INSTRUCTIONS,
        MAX_LUA_MEMORY, MAX_NATIVE_WORK,
    };
    assert!(
        usage.instructions < MAX_INSTRUCTIONS,
        "instructions over the cap"
    );
    assert!(
        usage.host_calls <= MAX_HOST_CALLS,
        "host calls over the cap"
    );
    assert!(
        usage.native_work <= MAX_NATIVE_WORK,
        "native work over the cap"
    );
    assert!(usage.loads <= MAX_INCLUDE_PARSES, "loads over the cap");
    assert_eq!(
        usage.loads,
        usage.requires + usage.dofiles,
        "loads miscounted"
    );
    assert!(usage.input_bytes <= MAX_INPUT_BYTES, "input over the cap");
    assert!(
        usage.ast_nodes <= MAX_AST_NODES,
        "analysis nodes over the cap"
    );
    assert!(usage.captures <= MAX_CAPTURES, "captures over the cap");
    assert!(
        usage.expanded <= MAX_EXPANDED_NODES,
        "expansion over the cap"
    );
    assert!(
        usage.peak_memory <= u64::try_from(MAX_LUA_MEMORY).unwrap_or(u64::MAX),
        "Lua memory over the cap"
    );
    for d in dependencies {
        let resource = ["strip.png", "grid.png"].contains(&d.as_str());
        assert!(
            LUA_FILES.contains(&d.as_str()) || resource,
            "a file outside the folder"
        );
    }
}

/// Target `skin_lua`: two bytes of import facts ([`lua_facts`]), then Lua
/// files separated by NUL bytes, at most [`LUA_FILES`]`.len()` of them,
/// written to a temporary skin folder under those names (beside
/// `strip.png` and `grid.png`; a file outside the folder is there to be
/// refused). The header pass runs once ([`read_lua_header`]); its header
/// must be within its caps. The configured pass then runs against that
/// snapshot, its play scene taken as the header's kind (a result scene
/// stays one, so a skin of another kind meets the wrong slot), with the header's defaults, twice, which must give the same
/// outcome (the passes share no Lua state), and once with each option's
/// last item. Every candidate's use of the sandbox must be within its caps
/// and consult only the folder's files; an imported or pending result's
/// facts must hold for its snapshot, each fact recorded once; an imported
/// skin's counters must be within the model's caps, and it is drawn
/// ([`draw`]).
///
/// # Errors
///
/// What [`read_lua_header`] refuses.
///
/// # Panics
///
/// If a bound above is broken, the two runs of one configuration differ,
/// or [`draw`] panics.
pub fn skin_lua(data: &[u8]) -> Result<(Header, Result<LuaOutcome, ImportError>), ImportError> {
    static FOLDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _owned = FOLDER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = lua_dir().join("skin");
    let (facts, data) = match data {
        [a, r, rest @ ..] => (lua_facts(*a, *r), rest),
        _ => (ImportFacts::unavailable(ScreenKind::Play), data),
    };
    let mut parts = data.splitn(LUA_FILES.len(), |&b| b == 0);
    for name in LUA_FILES {
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
    let cancel = Cancel::new();
    let h = read_lua_header(&dir, LUA_FILES[0], &cancel)?;
    check_header(h.header());
    check_lua(&h.usage(), h.dependencies());
    let mut facts = facts;
    if facts.scene == ScreenKind::Play {
        facts.scene = h.header().kind;
    }
    let defaults = Config::defaults(h.header());
    let outcome = |config: &Config| {
        let r = h.import(config, &facts, &cancel);
        match &r {
            Ok(LuaOutcome::Imported(i)) => {
                check_lua(&i.usage, &i.dependencies);
                check_usage(&i.imported);
                check_callbacks(i);
                check_facts(&i.facts, &facts);
            }
            Ok(LuaOutcome::Pending(p)) => {
                check_lua(&p.usage, &p.dependencies);
                check_facts(&p.facts, &facts);
                assert!(
                    p.facts.reads.iter().all(|r| r.fact != p.pending),
                    "a pending fact among the reads"
                );
            }
            Err(e) => {
                black_box(e.to_string());
            }
        }
        r
    };
    // A refusal at the Lua time cap depends on the clock: it is not
    // compared, and the input is not run again (each import starts from
    // the header pass's time).
    let timed = |r: &Result<LuaOutcome, ImportError>| {
        matches!(
            r,
            Err(ImportError::Lua {
                why: skin_beatoraja::LuaWhy::Limit(skin_beatoraja::LuaLimit::Time),
                ..
            })
        )
    };
    let first = outcome(&defaults);
    if timed(&first) {
        return Ok((h.header().clone(), first));
    }
    let again = outcome(&defaults);
    if timed(&again) {
        return Ok((h.header().clone(), first));
    }
    match (&first, &again) {
        (Ok(LuaOutcome::Imported(a)), Ok(LuaOutcome::Imported(b))) => {
            assert_eq!(
                a.imported.report, b.imported.report,
                "a run changed the next"
            );
            assert_eq!(a.usage, b.usage, "a run changed the next's usage");
            assert_eq!(
                a.dependencies, b.dependencies,
                "a run changed the next's files"
            );
            assert_eq!(a.facts, b.facts, "a run changed the next's facts");
        }
        (Ok(LuaOutcome::Pending(a)), Ok(LuaOutcome::Pending(b))) => {
            assert_eq!(
                (&a.facts, a.pending, a.usage, &a.dependencies),
                (&b.facts, b.pending, b.usage, &b.dependencies),
                "a run changed the next"
            );
        }
        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "a run changed the next"),
        _ => panic!("two runs of one configuration differ"),
    }
    let _ = outcome(&last_items(h.header()));
    if let Ok(LuaOutcome::Imported(i)) = &first {
        black_box(draw(&i.imported, data));
    }
    Ok((h.header().clone(), first))
}

/// The play snapshot of a `skin_lua` input's first two bytes: autoplay
/// unavailable, off, on or pending by `a` modulo 4, on a result scene
/// showing [`result_view`] for 255; the play level unavailable, pending,
/// or `a / 16` (twice) by `a / 4` modulo 4; the chart's judgment class
/// unavailable, pending, or known by `r`'s low two bits, the rest choosing
/// one of the five classes or none.
#[must_use]
pub fn lua_facts(a: u8, r: u8) -> ImportFacts {
    let scene = if a == u8::MAX {
        ScreenKind::Result
    } else {
        ScreenKind::Play
    };
    let autoplay = match a % 4 {
        0 => Fact::Unavailable,
        1 => Fact::Known(false),
        2 => Fact::Known(true),
        _ => Fact::Pending,
    };
    let judge_class = match r % 4 {
        0 => Fact::Unavailable,
        1 => Fact::Pending,
        _ => Fact::Known(match (r >> 2) % 6 {
            0 => Some(JudgeClass::VeryHard),
            1 => Some(JudgeClass::Hard),
            2 => Some(JudgeClass::Normal),
            3 => Some(JudgeClass::Easy),
            4 => Some(JudgeClass::VeryEasy),
            _ => None,
        }),
    };
    let play_level = match (a >> 2) % 4 {
        0 => Fact::Unavailable,
        1 => Fact::Pending,
        _ => Fact::Known(i32::from(a >> 4)),
    };
    // On a result scene, a ready view whose best has no BP: every result
    // read answers, the hidden ones as `i32::MIN`.
    let result = if scene == ScreenKind::Result {
        Fact::Known(skin_model::ResultFields::of(&result_view(true)))
    } else {
        Fact::Unavailable
    };
    ImportFacts {
        scene,
        autoplay,
        judge_class,
        play_level,
        result,
    }
}

/// A result's recorded facts hold for the snapshot it read them from,
/// each fact once.
fn check_facts(deps: &FactDeps, facts: &ImportFacts) {
    assert!(deps.holds(facts), "a result does not hold for its snapshot");
    for (i, r) in deps.reads.iter().enumerate() {
        assert!(
            deps.reads.iter().skip(i + 1).all(|o| o.fact != r.fact),
            "a fact recorded twice"
        );
    }
}

/// A candidate that imports was refused at none of its callbacks and
/// analysed every root (any other function is inert); each decorative
/// one replaced is reported, among the retained notes or counted as
/// suppressed past the report's cap.
fn check_callbacks(i: &LuaImported) {
    use skin_beatoraja::NoteKind;
    use skin_beatoraja::lua::Disposition;
    let mut replaced = 0;
    for d in &i.callbacks {
        match d {
            Disposition::Lowered
            | Disposition::Compiled
            | Disposition::ClassifiedOnly
            | Disposition::Inert
            | Disposition::Captured => {}
            Disposition::Replaced(_) => replaced += 1,
            // `import` keeps no VM, so it routes nothing to a runtime.
            Disposition::Refused | Disposition::NeverAnalysed | Disposition::Runtime => {
                panic!("an imported candidate with a callback {d:?}")
            }
        }
    }
    let noted = i
        .imported
        .report
        .notes
        .iter()
        .filter(|n| matches!(n.kind, NoteKind::Callback { .. }))
        .count();
    let suppressed = usize::try_from(i.imported.report.suppressed).unwrap_or(usize::MAX);
    assert!(
        noted.saturating_add(suppressed) >= replaced,
        "a replaced callback was not reported"
    );
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
    assert!(
        u.grade_anchor_keys <= u.keys,
        "grade attachment keys outside the keys"
    );
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
        } else if n.kind == NoteKind::ColourClamped {
            // A clamped colour channel is the other fallback (design:
            // docs/stage3.md "Tracks").
            assert_eq!(
                n.severity,
                Severity::Fallback,
                "a clamped colour's severity"
            );
            assert!(n.at.is_some(), "an unlocated clamped colour");
        } else if let NoteKind::InvalidColour { .. } = n.kind {
            // A graph's or visualizer's colour that does not parse draws
            // red or its default (design.md "Rendering and Skins").
            assert_eq!(
                n.severity,
                Severity::Fallback,
                "an invalid colour's severity"
            );
            assert!(n.at.is_some(), "an unlocated invalid colour");
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
    // Digit offsets: the rows resolved and the copies kept within their
    // caps; a strip dropped after its rows were charged keeps none.
    assert!(
        within(
            u.resolved_digit_offsets,
            skin_beatoraja::limits::MAX_RESOLVED_DIGIT_OFFSETS
        ),
        "too many digit offset rows resolved"
    );
    assert!(
        within(u.digit_offsets, skin_model::MAX_DIGIT_OFFSETS),
        "too many digit offsets"
    );
    assert!(
        i.skin.digit_offset_count() <= usize::try_from(u.digit_offsets).unwrap_or(usize::MAX),
        "more digit offsets kept than charged"
    );
}

/// A folder for `skin_png`'s file, rewritten by each input.
fn png_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("tapestry-fuzz-png-")
            .tempdir()
            .expect("create an image folder")
    })
    .path()
}

/// The pixel formats `skin_png` generates, by the input's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PngKind {
    Grey8,
    Grey16,
    GreyAlpha8,
    Rgb8,
    /// With a transparent colour.
    Rgb16Trns,
    Rgba8,
    Rgba16,
    /// With a palette of 256 entries and a transparency table.
    Indexed8Trns,
    /// With a transparent grey level.
    Grey1Trns,
}

impl PngKind {
    const ALL: [Self; 9] = [
        Self::Grey8,
        Self::Grey16,
        Self::GreyAlpha8,
        Self::Rgb8,
        Self::Rgb16Trns,
        Self::Rgba8,
        Self::Rgba16,
        Self::Indexed8Trns,
        Self::Grey1Trns,
    ];

    fn format(self) -> (png::ColorType, png::BitDepth) {
        use png::BitDepth::{Eight, One, Sixteen};
        use png::ColorType::{Grayscale, GrayscaleAlpha, Indexed, Rgb, Rgba};
        match self {
            Self::Grey8 => (Grayscale, Eight),
            Self::Grey16 => (Grayscale, Sixteen),
            Self::GreyAlpha8 => (GrayscaleAlpha, Eight),
            Self::Rgb8 => (Rgb, Eight),
            Self::Rgb16Trns => (Rgb, Sixteen),
            Self::Rgba8 => (Rgba, Eight),
            Self::Rgba16 => (Rgba, Sixteen),
            Self::Indexed8Trns => (Indexed, Eight),
            Self::Grey1Trns => (Grayscale, One),
        }
    }
}

/// The input of `skin_png`, read from its bytes: what to generate.
///
/// Bytes: the format (one of [`PngKind::ALL`]); the width, two bytes; the
/// height, its low bit choosing a file past the full-decode cap; the ask
/// the second build is cancelled at (0 for none); flags (1: a text chunk
/// after the image data, 2: that chunk's CRC wrong, 4: an extra empty
/// image data chunk); then the bytes the samples, palette and
/// transparency are drawn from.
#[derive(Clone, Debug)]
pub struct PngSpec {
    kind: PngKind,
    w: u32,
    h: u32,
    cancel_at: u8,
    text: bool,
    bad_text: bool,
    extra_idat: bool,
    seed: Vec<u8>,
    /// The palette and transparency table, made once from the seed.
    palette: Vec<u8>,
    trns: Vec<u8>,
}

impl PngSpec {
    /// The spec `data` describes.
    #[must_use]
    pub fn read(data: &[u8]) -> Self {
        let at = |i: usize| data.get(i).copied().unwrap_or(0);
        let kind = PngKind::ALL[usize::from(at(0)) % PngKind::ALL.len()];
        let width = u32::from(u16::from_le_bytes([at(1), at(2)]));
        let tall = at(3) & 1 == 1;
        let (w, h) = if tall {
            // Past 12 Mi texels: at least 769 wide, so the height stays
            // within a side; a few rows more at most.
            let w = 769 + width % (16_384 - 769 + 1);
            let h = u32::try_from((skin_model::MAX_IMAGE_TEXELS + 1).div_ceil(u64::from(w)))
                .expect("a height within a side");
            (w, (h + u32::from(at(3) >> 1) % 8).min(16_384))
        } else {
            (1 + width % 200, 1 + u32::from(at(3) >> 1) % 100)
        };
        let flags = at(5);
        let seed = data
            .get(6..)
            .filter(|s| !s.is_empty())
            .unwrap_or(&[7, 1, 99]);
        let mut spec = Self {
            kind,
            w,
            h,
            cancel_at: at(4),
            text: flags & 1 == 1,
            bad_text: flags & 2 == 2,
            extra_idat: flags & 4 == 4,
            seed: seed.to_vec(),
            palette: Vec::new(),
            trns: Vec::new(),
        };
        spec.palette = spec.make_palette();
        spec.trns = spec.make_trns();
        spec
    }

    /// The byte `k` of block `(bx, by)`: samples are constant over blocks
    /// of 64×64, so the file compresses.
    fn byte(&self, bx: u32, by: u32, k: usize) -> u8 {
        let i = usize::try_from(bx * 7 + by * 13).expect("a block index") * 8 + k;
        self.seed[i % self.seed.len()]
    }

    /// The native samples of texel `(x, y)`: up to four channels, a
    /// palette index, or a grey bit.
    fn samples(&self, x: u32, y: u32) -> [u16; 4] {
        let (bx, by) = (x / 64, y / 64);
        let b = |k: usize| self.byte(bx, by, k);
        let wide = |k: usize| u16::from_le_bytes([b(2 * k), b(2 * k + 1)]);
        match self.kind {
            PngKind::Grey16 | PngKind::Rgb16Trns | PngKind::Rgba16 => {
                [wide(0), wide(1), wide(2), wide(3)]
            }
            PngKind::Grey1Trns => [u16::from(b(0) & 1), 0, 0, 0],
            _ => [b(0), b(1), b(2), b(3)].map(u16::from),
        }
    }

    fn make_palette(&self) -> Vec<u8> {
        (0..768)
            .map(|i| self.seed[(i * 5 + 3) % self.seed.len()])
            .collect()
    }

    fn make_trns(&self) -> Vec<u8> {
        match self.kind {
            PngKind::Rgb16Trns => {
                let [r, g, b, _] = self.samples(0, 0);
                [r, g, b].iter().flat_map(|v| v.to_be_bytes()).collect()
            }
            PngKind::Indexed8Trns => {
                let n = 1 + usize::from(self.seed[0]);
                (0..n)
                    .map(|i| self.seed[(i * 3 + 1) % self.seed.len()])
                    .collect()
            }
            PngKind::Grey1Trns => vec![0, self.seed[0] & 1],
            _ => Vec::new(),
        }
    }

    /// The expected RGBA8 texel, straight, by the rules of a whole decode
    /// (`png`'s expansion, then `image`'s conversion: 16-bit samples
    /// rounded as `(v + 128) / 257`), computed here on its own.
    fn expected(&self, x: u32, y: u32) -> [u8; 4] {
        let s = self.samples(x, y);
        let q = |v: u16| u8::try_from((u32::from(v) + 128) / 257).expect("a byte");
        let n = |v: u16| u8::try_from(v).expect("an 8-bit sample");
        match self.kind {
            PngKind::Grey8 => [n(s[0]), n(s[0]), n(s[0]), 255],
            PngKind::Grey16 => [q(s[0]), q(s[0]), q(s[0]), 255],
            PngKind::GreyAlpha8 => [n(s[0]), n(s[0]), n(s[0]), n(s[1])],
            PngKind::Rgb8 => [n(s[0]), n(s[1]), n(s[2]), 255],
            PngKind::Rgb16Trns => {
                let [r, g, b, _] = self.samples(0, 0);
                let a = if [s[0], s[1], s[2]] == [r, g, b] {
                    0
                } else {
                    255
                };
                [q(s[0]), q(s[1]), q(s[2]), a]
            }
            PngKind::Rgba8 => [n(s[0]), n(s[1]), n(s[2]), n(s[3])],
            PngKind::Rgba16 => [q(s[0]), q(s[1]), q(s[2]), q(s[3])],
            PngKind::Indexed8Trns => {
                let i = usize::from(s[0]);
                let p = &self.palette;
                let a = self.trns.get(i).copied().unwrap_or(255);
                [p[3 * i], p[3 * i + 1], p[3 * i + 2], a]
            }
            PngKind::Grey1Trns => {
                let g = n(s[0] * 255);
                let a = if s[0] == u16::from(self.seed[0] & 1) {
                    0
                } else {
                    255
                };
                [g, g, g, a]
            }
        }
    }

    /// One row's bytes as `png` takes them.
    fn row(&self, y: u32, out: &mut Vec<u8>) {
        out.clear();
        if self.kind == PngKind::Grey1Trns {
            for x0 in (0..self.w).step_by(8) {
                let mut byte = 0;
                for x in x0..(x0 + 8).min(self.w) {
                    byte |= u8::try_from(self.samples(x, y)[0]).expect("a bit") << (7 - (x - x0));
                }
                out.push(byte);
            }
            return;
        }
        let (color, depth) = self.kind.format();
        let channels = color.samples();
        for x in 0..self.w {
            let s = self.samples(x, y);
            for &v in &s[..channels] {
                if depth == png::BitDepth::Sixteen {
                    out.extend_from_slice(&v.to_be_bytes());
                } else {
                    out.push(u8::try_from(v).expect("an 8-bit sample"));
                }
            }
        }
    }

    /// The PNG, its CRCs correct unless the spec asks for a bad one.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let (color, depth) = self.kind.format();
        let mut enc = png::Encoder::new(&mut out, self.w, self.h);
        enc.set_color(color);
        enc.set_depth(depth);
        enc.set_compression(png::Compression::Fast);
        enc.set_filter(png::Filter::Sub);
        if color == png::ColorType::Indexed {
            enc.set_palette(self.palette.as_slice());
        }
        if !self.trns.is_empty() {
            enc.set_trns(self.trns.as_slice());
        }
        let mut header = enc.write_header().expect("a PNG header");
        {
            let mut stream = header.stream_writer().expect("a PNG stream");
            let mut row = Vec::new();
            for y in 0..self.h {
                self.row(y, &mut row);
                std::io::Write::write_all(&mut stream, &row).expect("a row");
            }
            stream.finish().expect("the image data");
        }
        if self.extra_idat {
            header
                .write_chunk(png::chunk::IDAT, &[])
                .expect("an extra image data chunk");
        }
        if self.text {
            header
                .write_chunk(png::chunk::tEXt, b"k\0v")
                .expect("a text chunk");
        }
        header.finish().expect("the end");
        if self.text && self.bad_text {
            // The text chunk's CRC: the 4 bytes before `IEND`'s 12.
            let at = out.len() - 16;
            out[at] ^= 1;
        }
        out
    }

    /// Whether the file passes the full-decode cap.
    #[must_use]
    pub fn banded(&self) -> bool {
        u64::from(self.w) * u64::from(self.h) > skin_model::MAX_IMAGE_TEXELS
    }
}

/// What `skin_png` saw.
#[derive(Debug)]
pub struct PngOutcome {
    /// The file passes the full-decode cap.
    pub banded: bool,
    /// The first build: its cells checked against [`PngSpec::expected`].
    pub built: Result<(), skin_model::AtlasError>,
    /// The second build's cancel said yes, and it was cancelled.
    pub cancelled: bool,
}

/// Every page's every level of `atlas`, in order.
fn levels(atlas: &skin_model::Atlas) -> impl Iterator<Item = &[u8]> {
    atlas
        .pages()
        .iter()
        .flat_map(|p| (0..skin_model::MIP_LEVELS).filter_map(move |k| p.level(k)))
}

/// The variant of `e`, and of the image error inside it.
fn variant(
    e: &skin_model::AtlasError,
) -> (
    std::mem::Discriminant<skin_model::AtlasError>,
    Option<std::mem::Discriminant<skin_model::ImageError>>,
) {
    let inner = match e {
        skin_model::AtlasError::Image(i) => Some(std::mem::discriminant(i)),
        _ => None,
    };
    (std::mem::discriminant(e), inner)
}

/// Target `skin_png`: a [`PngSpec`] read from the input, generated as a
/// PNG with correct CRCs (a file past the full-decode cap decoded in
/// bands), admitted to a temporary skin folder, and built into an atlas
/// of four cells of at most 32×32: its corners and one the spec places.
/// Every texel of every cell must be the expected colour premultiplied,
/// computed on its own from the generated samples. A file with a bad CRC
/// after its image data fails in bands and builds whole (outside the
/// fuzzer: `png` checks no CRC under `cfg(fuzzing)`). The atlas is
/// built again with a cancel that says yes from the ask the spec names:
/// it is cancelled exactly when the cancel said yes, and otherwise gives
/// the same pages, or an error of the same variant.
///
/// # Panics
///
/// If the file is not admitted, a texel differs, a build fails other than
/// expected, or the second build disagrees with the cancel or the first.
#[must_use]
pub fn skin_png(data: &[u8]) -> PngOutcome {
    use skin_model::{AtlasError, AtlasLimits, FileDef, Filter, ImageDef, Root, SheetBuilder};
    const LIMITS: AtlasLimits = AtlasLimits {
        max_side: 256,
        max_layers: 8,
        reserved_layers: 1,
    };
    let spec = PngSpec::read(data);
    let dir = png_dir();
    std::fs::write(dir.join("a.png"), spec.encode()).expect("write the image");
    let root = Root::new(dir).expect("the image folder");
    let mut b = SheetBuilder::new(&root);
    let file = b
        .admit_file(FileDef {
            path: "a.png".into(),
            hi: None,
        })
        .expect("a generated file is admitted");
    let (w, h) = (spec.w, spec.h);
    assert_eq!(b.file_dims(file), Some([w, h]), "the admitted size");
    let (cw, ch) = (w.min(32), h.min(32));
    let (px, py) = (
        u32::from(spec.seed[0]) * 97 % (w - cw + 1),
        u32::from(*spec.seed.last().expect("a seed")) * 89 % (h - ch + 1),
    );
    let rects = [
        [0, 0, cw, ch],
        [w - cw, h - ch, cw, ch],
        [px, py, cw, ch],
        [w - cw, 0, cw, ch],
    ];
    for rect in rects {
        b.add_image(ImageDef {
            file,
            rect: Some(rect),
            grid: [1, 1],
            cycle: None,
            filter: Filter::Linear,
        })
        .expect("a cell inside its file");
    }
    let sheet = b.finish();
    let first = sheet.atlas(skin_model::Variant::Lo, LIMITS, &|| false);
    // `png` skips its CRC checks when built for fuzzing (`cfg(fuzzing)`),
    // so a bad CRC fails only outside the fuzzer, as in the smoke tests.
    let fails = spec.banded() && spec.text && spec.bad_text && !cfg!(fuzzing);
    match &first {
        Ok(atlas) => {
            assert!(!fails, "a bad CRC after the image data built in bands");
            let side = usize::try_from(atlas.side()).expect("a side");
            for (rect, f) in rects.iter().zip(atlas.uvs().frames()) {
                let page = atlas.pages()[usize::from(f.layer) - 1]
                    .level(0)
                    .expect("level 0");
                let to = |v: f32| (v * side as f32) as usize;
                let (u, v) = (to(f.uv[0]), to(f.uv[1]));
                let [x0, y0, cw, ch] = *rect;
                for dy in 0..ch {
                    for dx in 0..cw {
                        let [r, g, b, a] = spec.expected(x0 + dx, y0 + dy);
                        let pre = |c: u8| {
                            u8::try_from((u16::from(c) * u16::from(a) + 127) / 255).expect("a byte")
                        };
                        let at = ((v + dy as usize) * side + u + dx as usize) * 4;
                        assert!(
                            page[at..at + 4] == [pre(r), pre(g), pre(b), a],
                            "texel ({}, {}) of {:?}",
                            x0 + dx,
                            y0 + dy,
                            spec.kind
                        );
                    }
                }
            }
        }
        Err(e) => assert!(
            fails && matches!(e, AtlasError::Image(skin_model::ImageError::Png { .. })),
            "the build failed: {e}"
        ),
    }
    let asked = std::cell::Cell::new(0_u32);
    let said = std::cell::Cell::new(false);
    let cancel = || {
        asked.set(asked.get() + 1);
        let yes = spec.cancel_at != 0 && asked.get() >= u32::from(spec.cancel_at);
        said.set(said.get() || yes);
        yes
    };
    let again = sheet.atlas(skin_model::Variant::Lo, LIMITS, &cancel);
    let cancelled = matches!(again, Err(AtlasError::Cancelled));
    assert!(
        cancelled == said.get(),
        "cancelled {cancelled}, cancel said {}",
        said.get()
    );
    if !cancelled {
        match (&first, &again) {
            (Ok(a), Ok(b)) => {
                assert!(levels(a).eq(levels(b)), "a second build differs");
            }
            (Err(e), Err(f)) => assert!(variant(e) == variant(f), "{e:?} then {f:?}"),
            (first, again) => panic!("a second build differs: {first:?} then {again:?}"),
        }
    }
    PngOutcome {
        banded: spec.banded(),
        built: first.map(|_| ()),
        cancelled,
    }
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
            f.glyph(k.first).is_some(),
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

/// The reads a fuzzed program may name: an integer, a boolean, and an
/// integer guarded; with `bad`, also an enumeration and an output, which
/// the verifier refuses.
fn present_reads(bad: bool) -> Vec<skin_model::present::ReadDef> {
    use skin_model::present::{OutTy, ReadDef};
    use skin_model::{PLAY, Scope, Ty, compile};
    let p = |src: &str, ty, outs: &[OutTy]| {
        compile(src, &Scope::new(&PLAY).outputs(outs), ty).expect("a fixed read compiles")
    };
    let kind = match PLAY.field("gauge.kind").map(|(_, f)| f.ty) {
        Some(skin_model::FieldTy::Enum(e)) => Ty::Enum(e),
        _ => Ty::Int,
    };
    let mut reads = vec![
        ReadDef {
            value: p("combo", Ty::Int, &[]),
            guard: None,
        },
        ReadDef {
            value: p("combo > 3", Ty::Bool, &[]),
            guard: None,
        },
        ReadDef {
            value: p("combo * 7", Ty::Int, &[]),
            guard: Some(p("combo > 5", Ty::Bool, &[])),
        },
    ];
    if bad {
        reads.extend([
            ReadDef {
                value: p("gauge.kind", kind, &[]),
                guard: None,
            },
            ReadDef {
                value: p("lua.0.ok", Ty::Bool, &[OutTy::Bool]),
                guard: None,
            },
        ]);
    }
    reads
}

/// A presentation program decoded from `data`: one byte of callback
/// count (its high bit adding the reads the verifier refuses), then per callback seven bytes (its start, its length, its
/// registers, its conversion and its signature, its gate, its `after`),
/// then four-byte instructions to the end (an op byte from 240 the clock
/// or a slot's load or store, a conversion byte's high bit a timer
/// callback); the constants are a fixed table
/// of integers, floats, nil, booleans and the off value, the reads
/// [`present_reads`], and the slots and store domains a fixed table too
/// (a slot or domain index past it, or a store whose domain is not its
/// slot's, refused).
#[must_use]
pub fn present_def(data: &[u8]) -> skin_model::present::PresentDef {
    use skin_model::present::{
        Arith, CallbackDef, Cmp, Conv, Domain, Insn, MathFn, OFF, OutTy, PresentDef, SlotDef, Val,
    };
    let consts = vec![
        Val::Nil,
        Val::Bool(true),
        Val::Bool(false),
        Val::Int(0),
        Val::Int(3),
        Val::Int(-7),
        // ±2^53, which V3 accepts, and one past each, which it refuses.
        Val::Int(1 << 53),
        Val::Int(-(1 << 53)),
        Val::Int((1 << 53) + 1),
        Val::Int(-(1 << 53) - 1),
        Val::Int(i64::from(i32::MAX)),
        Val::Float(0.5),
        Val::Float(-0.0),
        Val::Float(f64::NAN),
        Val::Float(f64::INFINITY),
        Val::Float(4_294_967_296.0),
        OFF,
    ];
    let latch = Domain {
        off: true,
        ints: Some((0, 1 << 53)),
    };
    let small = Domain {
        off: false,
        ints: Some((0, 3)),
    };
    let slots = vec![
        SlotDef {
            init: OFF,
            domain: latch,
        },
        SlotDef {
            init: Val::Int(0),
            domain: small,
        },
    ];
    let (n, rest) = data.split_first().map_or((0, data), |(&n, r)| (n % 8, r));
    let (head, body) = rest.split_at((usize::from(n) * 7).min(rest.len()));
    let mut def = PresentDef {
        consts,
        reads: present_reads(data.first().is_some_and(|b| b & 0x80 != 0)),
        slots,
        domains: vec![
            latch,
            small,
            Domain {
                off: true,
                ints: None,
            },
        ],
        ..PresentDef::default()
    };
    for &[start, len, regs, conv, sig, gate, after] in head.as_chunks::<7>().0 {
        // The high bit a timer callback.
        let conv = if conv & 0x80 != 0 {
            Conv::Timer
        } else if conv % 2 == 0 {
            Conv::Truthy
        } else {
            Conv::Int
        };
        def.callbacks.push(CallbackDef {
            start: u32::from(start),
            len: u16::from(len),
            regs,
            conv,
            gate: (gate < 8).then_some(u16::from(gate)),
            after: (after < 8).then_some(u16::from(after)),
        });
        // A signature that may not match its conversion.
        let ty = match (sig % 5 == 0, conv) {
            (false, Conv::Truthy) | (true, Conv::Int) => OutTy::Bool,
            (false, Conv::Timer) => OutTy::Timer,
            _ => OutTy::Int,
        };
        def.signature.push(ty);
    }
    let arith = [
        Arith::Add,
        Arith::Sub,
        Arith::Mul,
        Arith::Div,
        Arith::IDiv,
        Arith::Mod,
    ];
    let cmp = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le];
    let math = [MathFn::Floor, MathFn::Ceil, MathFn::Abs];
    for &[op, a, b, c] in body.as_chunks::<4>().0 {
        // An op byte from 240 a clock or slot instruction.
        let op = if op >= 240 {
            12 + (op - 240) % 3
        } else {
            op % 12
        };
        def.code.push(match op {
            0 => Insn::Const {
                dst: a,
                k: u16::from(b),
            },
            1 => Insn::Read {
                dst: a,
                read: u16::from(b),
            },
            2 => Insn::Move { dst: a, src: b },
            3 => Insn::Arith {
                op: arith[usize::from(c) % 6],
                dst: a,
                a: b,
                b: c / 6,
            },
            4 => Insn::Neg { dst: a, a: b },
            5 => Insn::Cmp {
                op: cmp[usize::from(c) % 4],
                dst: a,
                a: b,
                b: c / 4,
            },
            6 => Insn::Not { dst: a, a: b },
            7 => Insn::Math {
                f: math[usize::from(c) % 3],
                dst: a,
                a: b,
            },
            8 => Insn::Extreme {
                max: c % 2 == 0,
                dst: a,
                a: b,
                b: c / 2,
            },
            9 => Insn::Jump { to: u16::from(a) },
            10 => Insn::Test {
                cond: a,
                when: b % 2 == 0,
                to: u16::from(c),
            },
            12 => Insn::StateUs { dst: a },
            13 => Insn::LoadSlot {
                dst: a,
                slot: u16::from(b % 4),
            },
            14 => Insn::StoreSlot {
                slot: u16::from(b % 4),
                src: a,
                domain: u16::from(c % 4),
            },
            _ => Insn::Ret { src: a },
        });
    }
    def
}

/// A play skin with the program `data` decodes ([`present_def`]), each
/// output drawn by nothing: `Skin::new` verifies it, never panicking; an
/// accepted one, its per-frame cost within the skin's operation total,
/// builds frames over a moving combo, each output's flag 0 or 1. Its
/// callback count when accepted.
#[must_use]
pub fn present(data: &[u8]) -> Option<usize> {
    use bms_model::{ChartBuilder, TimeUs};
    use eval::{GaugeState, Tally};
    use rules::GaugeValue;
    use skin_model::{
        Anchor, Attempt, Fit, GaugeDesc, GaugeLook, Layout, PlayContext, QuadBatch, Region,
        RegionDef, Side, Skin, SkinDef, SkinFrame, Variant, field,
    };
    let def = present_def(data);
    let n = def.callbacks.len();
    let skin = Skin::new(SkinDef {
        fit: Fit::Adapt,
        regions: [RegionDef {
            anchor: Anchor::TopLeft,
            optional: false,
        }; Region::COUNT],
        whens: Vec::new(),
        sheet: skin_model::Sheet::empty(),
        lanes: None,
        elements: Vec::new(),
        widgets: skin_model::WidgetDefs::default(),
        bomb_up_to: None,
        close: None,
        loadend: None,
        present: Some(def),
    })
    .ok()?;
    assert!(
        skin.ops().total() <= skin_model::MAX_SKIN_OPS,
        "past the frame cost"
    );
    let limits = skin_model::AtlasLimits {
        max_side: 256,
        max_layers: 2,
        reserved_layers: 1,
    };
    let uvs = skin
        .atlas(Variant::Lo, limits, &|| false)
        .expect("an empty sheet builds its atlas")
        .into_parts()
        .1;
    let chart = ChartBuilder::new(150_000).build().expect("an empty chart");
    let gauges = [GaugeDesc {
        start: GaugeValue(20 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Normal,
        border: GaugeValue(80 * GaugeValue::PERCENT),
    }];
    let ctx = PlayContext {
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: TimeUs(0),
        bpm_range: None,
        bpm_whole_change: None,
        play_level: None,
        meta: skin_model::ChartMetadata::UNKNOWN,
        score: skin_model::PlayScore::NONE,
    };
    let mut play = skin.prepare(&chart, &ctx).expect("the skin prepares");
    let states = [GaugeState {
        value: gauges[0].start,
        reference: gauges[0].start,
        failed_at: None,
    }];
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    for combo in [0_u32, 4, 9] {
        let tally = Tally {
            combo,
            max_combo: combo,
            ..Default::default()
        };
        let frame = SkinFrame {
            app: skin_model::AppReadouts::LAUNCH,
            state_us: 0,
            screen_us: 0,
            now: TimeUs(i64::from(combo) * 16_000),
            display_offset: TimeUs(0),
            tally: &tally,
            gauges: &states,
            gauge: 0,
            effects: &[],
            notes: &eval::NoteFacts::default(),
            offsets: skin_model::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: LANES_OFF,
            readouts: readouts_for(LANES_OFF),
            playing: true,
            closing: None,
        };
        batch.clear();
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .expect("the frame builds");
        let f = play.state().fields;
        assert_eq!(f.len(), field::COUNT + 2 * n, "the outputs prepared");
        for k in 0..n {
            assert!(matches!(f[field::COUNT + 2 * k + 1], 0 | 1), "an ok flag");
        }
    }
    Some(n)
}
