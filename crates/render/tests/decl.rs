//! A declarative skin's installation on WARP, read back: staged with a
//! fresh attempt while the installation in use goes on drawing, then
//! committed whole, its skin, uv table, atlas, clips and capacities
//! switching together; a staging refused or a commit refused leaving the
//! installation in use drawing; the installed bundle reused by a later
//! attempt, which starts fresh; a committed bundle restored on another
//! renderer from memory alone; and the bytes a replacement holds at its
//! peak.
//!
//! The skins are fixed 1920×1080 canvases on a 1920×1080 target, so canvas
//! pixels are target pixels. Each bundle draws one solid image over the
//! whole canvas under its clip 0 ([`decl_support`]).

mod common;
mod decl_support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, attempt, context, input, offscreen, view};
use decl_support::{FULL_LANES, Look, bundle, full_readouts, lane_chart, part_counts};
use eval::GaugeState;
use render::{
    BundleError, CommitError, DeclSkin, FrameInput, MAX_SPRITES, PlaySkin, PreparedToken,
    RenderError, Renderer, SkinBundle, Staging,
};

use rules::GaugeValue;
use skin_model::{BuildError, GaugeDesc, GaugeLook, Side};

const BLACK: [u8; 3] = [0; 3];
const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
/// The left half's centre and the right half's.
const LEFT: (u32, u32) = (480, 540);
const RIGHT: (u32, u32) = (1_440, 540);

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// The longest the GPU may take over the frames before a read back. Each
/// test's frames take milliseconds on WARP; one past this fails here
/// instead of holding every WARP device in the process until the job's
/// timeout.
const FRAME_BUDGET: Duration = Duration::from_secs(10);

/// `r`'s read back once its submitted frames finished within
/// [`FRAME_BUDGET`].
#[track_caller]
fn read_back(r: &Renderer) -> Vec<u8> {
    let done = r.gpu().device().poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(FRAME_BUDGET),
    });
    assert!(done.is_ok(), "frames past {FRAME_BUDGET:?}: {done:?}");
    r.read_back()
}

/// Draws a frame of the default view and reads it back: its sprites and
/// the pixels.
fn frame(r: &mut Renderer) -> (usize, Vec<u8>) {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    assert_eq!(timing.dropped, 0);
    (timing.sprites, read_back(r))
}

/// The RGB of pixel `(x, y)` of a 1920-wide read back.
fn px(pixels: &[u8], point: (u32, u32)) -> [u8; 3] {
    common::rgb_at(pixels, 1_920, point)
}

/// `pixels` show `left` on the left half and `right` on the right.
#[track_caller]
fn halves(pixels: &[u8], left: [u8; 3], right: [u8; 3]) {
    assert_eq!(
        (px(pixels, LEFT), px(pixels, RIGHT)),
        (left, right),
        "left and right halves"
    );
}

/// Stages `b` with a fresh attempt of `chart` for `request`.
fn stage(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    chart: &Chart,
    request: u64,
) -> Result<PreparedToken, RenderError> {
    let ctx = context(&GAUGES, false);
    let a = attempt(r, chart, &ctx);
    r.stage(b, &a, request)
}

/// Red over the left half.
fn red() -> Look {
    Look::left([255, 0, 0, 255])
}

/// Blue over the right half, from a 300-texel image (a 512-texel page),
/// with 9 500 more image elements than one, each over a 10×10 corner: past
/// [`MAX_SPRITES`], and past the 9 216 instances (with the overlay's
/// reserve) of the least buffer.
fn blue() -> Look {
    Look {
        side: 300,
        clip: Some([960.0, 0.0, 960.0, 1_080.0]),
        copies: 9_500,
        ..Look::left([0, 0, 255, 255])
    }
}

/// A bundle is refused before anything is uploaded when its clip table
/// lacks a clip the skin draws under, or when its atlas was built for
/// another layer reservation than the renderer's, whose pages would be
/// uploaded at the wrong layers.
#[test]
fn a_bundle_checks_its_clips_and_its_layers() {
    let dir = tempfile::tempdir().unwrap();
    let r = offscreen(64, 64);
    let skin = decl_support::skin(dir.path(), &red());
    assert_eq!(skin.clip_count(), 1);
    let limits = |reserved_layers| skin_model::AtlasLimits {
        reserved_layers,
        ..r.atlas_limits()
    };
    let atlas = |reserved| {
        skin.atlas(skin_model::Variant::Lo, limits(reserved), &|| false)
            .unwrap()
    };
    let clip = || render::ClipTable::new(vec![[0.0, 0.0, 960.0, 1_080.0]]).unwrap();
    let (no_clip, two, good) = (atlas(1), atlas(2), atlas(1));
    let skin = Arc::new(skin);
    let new = |atlas, clips| SkinBundle::new(Arc::clone(&skin), atlas, clips, Side::Left);
    assert_eq!(
        new(no_clip, render::ClipTable::default()).unwrap_err(),
        BundleError::Clips {
            referenced: 1,
            clips: 0
        }
    );
    assert_eq!(
        new(two, clip()).unwrap_err(),
        BundleError::Layers {
            first: 2,
            last: 2,
            end: 1
        }
    );
    assert!(new(good, clip()).is_ok());
}

/// A staged installation leaves the one in use drawing, through frames,
/// until its commit switches the skin, the uv table, the atlas, the clips
/// and the capacities together: red under the left clip, from a 256-texel
/// page and the least capacity, then blue under the right clip, from a
/// 512-texel page and 9 501 sprites. Any of them switched alone would
/// draw the wrong colour or half, or keep the other size.
#[test]
fn a_commit_switches_the_whole_installation_together() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &red());
    let (b, _) = bundle(&r, dir.path(), &blue());
    let least = (MAX_SPRITES, MAX_SPRITES);

    let token = stage(&mut r, &a, &chart, 1).unwrap();
    assert_eq!(r.staged(), Some(token));
    assert!(r.installed().is_none());
    let before = r.resource_generation();
    r.commit(token).unwrap();
    assert!(r.staged().is_none());
    assert!(Arc::ptr_eq(r.installed().unwrap(), &a));
    assert_eq!(r.resource_generation(), before + 1);
    assert_eq!((r.atlas_size().0, r.capacity()), (256, least));
    assert_eq!(r.clips().len(), 1);
    let (sprites, p) = frame(&mut r);
    assert_eq!(sprites, 1);
    halves(&p, RED, BLACK);

    // Staged: the red installation draws on, frame after frame.
    let token = stage(&mut r, &b, &chart, 2).unwrap();
    for _ in 0..3 {
        let (_, p) = frame(&mut r);
        halves(&p, RED, BLACK);
    }
    assert_eq!((r.atlas_size().0, r.capacity()), (256, least));
    r.commit(token).unwrap();
    assert!(Arc::ptr_eq(r.installed().unwrap(), &b));
    assert_eq!(r.atlas_size().0, 512);
    assert_eq!(r.capacity(), (9_501, 9_501));
    let (sprites, p) = frame(&mut r);
    assert_eq!(sprites, 9_501);
    halves(&p, BLACK, BLUE);
}

/// A staging refused after its resources were made, by the attempt's text
/// producer, is dropped whole: nothing is staged, the installation in use
/// draws the next frame as before, and its resources stay. The refusal is
/// a refusal on a healthy device, never a device failure.
#[test]
fn a_refused_staging_leaves_the_installation_in_use_drawing() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &red());
    let token = stage(&mut r, &a, &chart, 1).unwrap();
    r.commit(token).unwrap();
    let generation = r.resource_generation();

    let look = Look {
        text: true,
        ..blue()
    };
    let (b, text) = bundle(&r, dir.path(), &look);
    let text = text.unwrap();
    text.refuse.store(true, Ordering::SeqCst);
    let refused = stage(&mut r, &b, &chart, 2).unwrap_err();
    assert!(matches!(refused, RenderError::Skin(_)), "{refused:?}");
    assert!(refused.is_refusal());
    assert_eq!(text.calls.load(Ordering::SeqCst), 1);
    assert!(r.staged().is_none());
    assert_eq!(r.memory().staged_atlas, 0);
    assert_eq!(
        r.commit(PreparedToken {
            request: 2,
            device: r.device_generation()
        }),
        Err(CommitError::NothingStaged)
    );
    assert!(Arc::ptr_eq(r.installed().unwrap(), &a));
    assert_eq!(r.resource_generation(), generation);
    let (sprites, p) = frame(&mut r);
    assert_eq!(sprites, 1);
    halves(&p, RED, BLACK);

    // The same bundle once its text prepares: its text element draws too.
    text.refuse.store(false, Ordering::SeqCst);
    let token = stage(&mut r, &b, &chart, 3).unwrap();
    r.commit(token).unwrap();
    let (sprites, p) = frame(&mut r);
    assert_eq!(sprites, 9_502);
    assert_eq!(px(&p, LEFT), BLACK);
}

/// A commit is refused, the staging kept, for a token of another request
/// with the staging's device, and for a token of another device with the
/// staging's request; each alone. The staging's own token installs it.
#[test]
fn a_commit_checks_the_request_and_the_device_apart() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let other = offscreen(64, 64);
    assert_ne!(r.device_generation(), other.device_generation());
    let (a, _) = bundle(&r, dir.path(), &red());
    let token = stage(&mut r, &a, &chart, 7).unwrap();

    let stale = PreparedToken {
        request: 6,
        ..token
    };
    assert_eq!(
        r.commit(stale),
        Err(CommitError::Request {
            staged: 7,
            asked: 6
        })
    );
    let moved = PreparedToken {
        device: other.device_generation(),
        ..token
    };
    assert_eq!(r.commit(moved), Err(CommitError::Device));
    assert_eq!(r.staged(), Some(token));
    assert!(r.installed().is_none());

    r.commit(token).unwrap();
    let (_, p) = frame(&mut r);
    halves(&p, RED, BLACK);
    // Committed once, it is superseded.
    assert_eq!(
        r.commit(token),
        Err(CommitError::Superseded {
            committed: 7,
            asked: 7
        })
    );
    // A discarded staging cannot be committed either.
    let token = stage(&mut r, &a, &chart, 8).unwrap();
    r.discard();
    assert_eq!(r.commit(token), Err(CommitError::NothingStaged));
}

/// The retry's two gauges: the first attempt's, then another.
const TWO: [GaugeDesc; 2] = [
    GAUGES[0],
    GaugeDesc {
        start: GaugeValue(100 * GaugeValue::PERCENT),
        max: GaugeValue::FULL,
        look: GaugeLook::Hazard,
        border: GaugeValue(0),
    },
];

/// The installed bundle staged again, for a retry with another preset,
/// reuses its resources and prepares a fresh attempt from the new preset:
/// the first attempt counted empty POORs as POORs, so its frames, whose
/// snapshot counts more of them than POORs, are refused; the retry counts
/// them apart and has two gauges, and its frames of a two-gauge snapshot
/// build. Kept, the first attempt's state would go on refusing.
#[test]
fn a_retry_reuses_the_resources_with_a_fresh_attempt() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let look = Look {
        text: true,
        ..red()
    };
    let (a, text) = bundle(&r, dir.path(), &look);
    let text = text.unwrap();
    let counted = context(&GAUGES, true);
    let token = r.stage(&a, &attempt(&r, &chart, &counted), 1).unwrap();
    r.commit(token).unwrap();
    let generation = r.resource_generation();
    let v = view(0, 0);
    let f = input(&v, TimeUs(0), TimeUs(0), &[]);
    assert!(matches!(
        r.frame(&f),
        Err(RenderError::Build(BuildError::InconsistentTally))
    ));
    assert_eq!(r.refused(), Some(BuildError::InconsistentTally));
    // Its poisoned `Play` is never built again: every frame answers the
    // refusal and presents nothing.
    assert_eq!(r.build(&f), Err(BuildError::InconsistentTally));
    assert!(!r.submit().unwrap().presented);

    let retry = context(&TWO, false);
    let token = r.stage(&a, &attempt(&r, &chart, &retry), 2).unwrap();
    assert_eq!(r.memory().staged_atlas, 0, "the installed atlas is reused");
    r.commit(token).unwrap();
    assert_eq!(r.resource_generation(), generation);
    assert_eq!(r.refused(), None);
    assert_eq!(text.calls.load(Ordering::SeqCst), 2, "each attempt's text");
    let mut v = view(0, 0);
    v.gauges.push(GaugeState {
        value: GaugeValue::FULL,
        reference: GaugeValue::FULL,
        failed_at: None,
    });
    let f = input(&v, TimeUs(0), TimeUs(0), &[]);
    let timing = r.frame(&f).unwrap();
    assert_eq!(timing.sprites, 2);
    halves(&read_back(&r), RED, BLACK);
}

/// Repeated READY rearms (design.md "Play Lifecycle", the start gate) keep
/// the renderer's memory witness flat: the attempt's prepared resources and
/// text stay attached, never prepared again, while its runtime starts
/// over, and every frame after a rearm draws as the attempt's first. The
/// skin is the full lane fixture, which first draws every note category,
/// its bar group and each cover paint as quads.
#[test]
fn repeated_rearms_keep_the_memory_witness_flat() {
    let chart = lane_chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let look = Look {
        text: true,
        structure: true,
        full_lanes: true,
        ..red()
    };
    let (a, text) = bundle(&r, dir.path(), &look);
    let text = text.unwrap();
    let ctx = context(&GAUGES, false);
    let v = view(0, 0);
    let at = |ms: i64| FrameInput {
        lanes: FULL_LANES,
        readouts: full_readouts(FULL_LANES),
        ..input(&v, TimeUs(ms * 1_000), TimeUs(0), &[])
    };
    // What the attempt's first frame draws, staged apart.
    let mut skin = DeclSkin::new(a.clone());
    skin.prepare(r.gpu(), &attempt(&r, &chart, &ctx)).unwrap();
    let canvas = skin.canvas([1_920, 1_080]);
    let mut staging = Staging::with_runs(skin.capacity(), skin.run_capacity(), &canvas);
    staging.set_clips(a.clips().clone());
    staging.build(&mut skin, &at(900)).unwrap();
    let counts = part_counts(&r, &a, staging.instances());
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    let prepared = text.calls.load(Ordering::SeqCst);
    let token = r.stage(&a, &attempt(&r, &chart, &ctx), 1).unwrap();
    r.commit(token).unwrap();
    let generation = r.resource_generation();
    let first = r.frame(&at(900)).unwrap();
    assert_eq!(first.sprites, staging.instances().len());
    let memory = r.memory();
    for k in 0..50 {
        r.frame(&at(900 + k * 17)).unwrap();
        r.rearm();
        let again = r.frame(&at(900)).unwrap();
        assert_eq!(again.sprites, first.sprites, "rearm {k}");
        assert_eq!(r.memory(), memory, "rearm {k}");
    }
    assert_eq!(r.resource_generation(), generation);
    assert_eq!(
        text.calls.load(Ordering::SeqCst),
        prepared + 1,
        "the text was prepared again"
    );
    halves(&read_back(&r), RED, BLACK);
}

/// A renderer made again after a device failure restores the committed
/// bundle from memory, its image files gone: it draws nothing until an
/// attempt is staged, which reuses the restored resources and draws as
/// before.
#[test]
fn a_restored_bundle_needs_no_file() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &red());
    let token = stage(&mut r, &a, &chart, 1).unwrap();
    r.commit(token).unwrap();
    drop(r);
    dir.close().unwrap();

    let mut again = offscreen(1_920, 1_080);
    again
        .restore(&render::Installation::new(Arc::clone(&a)))
        .unwrap();
    assert!(Arc::ptr_eq(again.installed().unwrap(), &a));
    let generation = again.resource_generation();
    let (sprites, p) = frame(&mut again);
    assert_eq!(sprites, 0);
    halves(&p, BLACK, BLACK);
    let token = stage(&mut again, &a, &chart, 2).unwrap();
    again.commit(token).unwrap();
    assert_eq!(again.resource_generation(), generation);
    let (_, p) = frame(&mut again);
    halves(&p, RED, BLACK);
}

/// What a replacement holds at its peak, between its staging and its
/// commit: the installed bundle's CPU description and resources and the
/// staged ones together, and the upload's staging; after the commit only
/// the new ones, and after a discard only the installed.
#[test]
fn a_replacement_peaks_with_both_installations() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &red());
    let (b, _) = bundle(&r, dir.path(), &blue());
    let token = stage(&mut r, &a, &chart, 1).unwrap();
    r.commit(token).unwrap();
    let installed = r.memory();
    assert_eq!(installed.committed_cpu, a.resident_bytes());
    assert_eq!(
        (
            installed.staged_cpu,
            installed.staged_atlas,
            installed.upload
        ),
        (0, 0, 0)
    );
    assert!(installed.atlas > 0 && installed.batch > 0);

    let token = stage(&mut r, &b, &chart, 2).unwrap();
    let peak = r.memory();
    println!("peak: {peak:?}, {} bytes", peak.total());
    assert_eq!(peak.committed_cpu, a.resident_bytes());
    assert_eq!(peak.staged_cpu, b.resident_bytes());
    assert_eq!((peak.atlas, peak.batch), (installed.atlas, installed.batch));
    // A 512-texel page beside the built-in layer: more than the 256-texel
    // array, and an upload of at least the page's level 0.
    assert!(peak.staged_atlas > installed.atlas);
    assert!(peak.upload >= 512 * 512 * 4);
    assert!(peak.staged_batch > installed.batch);
    assert!(peak.total() > installed.total());

    r.commit(token).unwrap();
    let after = r.memory();
    assert_eq!(after.committed_cpu, b.resident_bytes());
    assert_eq!(
        (
            after.staged_cpu,
            after.staged_atlas,
            after.upload,
            after.staged_batch
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        (after.atlas, after.batch),
        (peak.staged_atlas, peak.staged_batch)
    );

    let token = stage(&mut r, &a, &chart, 3).unwrap();
    assert!(r.memory().staged_atlas > 0);
    r.discard();
    assert_eq!(r.memory(), after);
    assert_eq!(r.commit(token), Err(CommitError::NothingStaged));
}

/// A staging of the installed bundle reuses its resources, yet its fresh
/// attempt's state lives beside the installed attempt's until the commit
/// or a discard: the witness counts both.
#[test]
fn a_reused_bundle_still_counts_both_attempts() {
    let chart = chart();
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    let (a, _) = bundle(&r, dir.path(), &red());
    let token = stage(&mut r, &a, &chart, 1).unwrap();
    r.commit(token).unwrap();
    let installed = r.memory();
    assert!(installed.attempt > 0);
    assert_eq!(installed.staged_attempt, 0);

    let token = stage(&mut r, &a, &chart, 2).unwrap();
    let both = r.memory();
    assert_eq!((both.staged_atlas, both.staged_batch), (0, 0));
    assert_eq!(both.staged_attempt, installed.attempt);
    assert_eq!(both.total(), installed.total() + installed.attempt);
    r.discard();
    assert_eq!(r.memory(), installed);

    let token2 = stage(&mut r, &a, &chart, 3).unwrap();
    assert!(r.memory().total() > installed.total());
    assert_eq!(
        r.commit(token),
        Err(CommitError::Request {
            staged: 3,
            asked: 2
        })
    );
    r.commit(token2).unwrap();
    assert_eq!(r.memory(), installed);
}

/// A frame's model input comes from the frame alone: the tally, the
/// gauges, the completion and the play phase from its one view, the
/// effects as drained, its target, lanes and readouts, and the author's
/// layout.
#[test]
fn a_skin_frame_reads_one_view() {
    let mut v = view(5, 9);
    v.playing = false;
    v.complete = true;
    let effects = [eval::JudgeEvent {
        seq: 4,
        time: TimeUs(10),
        origin: None,
        kind: eval::JudgeEventKind::EmptyPoor {
            lane: common::lane(1),
        },
    }];
    let f = render::FrameInput {
        gauge: 0,
        lanes: skin_model::LaneView {
            gain: skin_model::Gain::new(16, 5),
            cover: 200,
            lift: 300,
            hidden: 0,
        },
        readouts: skin_model::FrameReadouts {
            hispeed_x100: Some(457),
            native: skin_model::NativeReadouts {
                target_green: Some(300),
                floating: true,
                ..skin_model::FrameReadouts::UNAVAILABLE.native
            },
            ..skin_model::FrameReadouts::UNAVAILABLE
        },
        ..input(&v, TimeUs(1_000), TimeUs(20), &effects)
    };
    let s = render::skin_frame(&f);
    assert!(std::ptr::eq(s.tally, &v.tally));
    assert!(std::ptr::eq(s.gauges, v.gauges.as_slice()));
    assert_eq!((s.complete, s.playing), (true, false));
    assert!(std::ptr::eq(s.effects, effects.as_slice()));
    assert_eq!(
        (s.now, s.display_offset, s.target),
        (TimeUs(1_000), TimeUs(20), f.target)
    );
    assert_eq!(*s.layout, skin_model::Layout::AUTHOR);
    assert_eq!(s.lanes, f.lanes);
    assert_eq!(s.readouts, f.readouts);
}

/// The simple skin's attempt state counts what it copies of the chart, so
/// the first replacement by a declarative skin counts it beside the staged
/// attempt's until the commit, which leaves the declarative attempt's.
#[test]
fn a_first_replacement_counts_the_simple_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = offscreen(1_920, 1_080);
    common::prepare(&mut r, &chart());
    let bare = r.memory().attempt;
    let mut b = ChartBuilder::new(120_000);
    for k in 0..100 {
        b = b.tap(common::lane(1), TimeUs(1_000_000 + k * 10_000));
    }
    let notes = b.build().unwrap();
    common::prepare(&mut r, &notes);
    let simple = r.memory();
    let copied = 100 * size_of::<bms_model::Note>() as u64;
    assert!(simple.attempt >= bare + copied, "{bare} {simple:?}");

    let (a, _) = bundle(&r, dir.path(), &red());
    let token = stage(&mut r, &a, &notes, 1).unwrap();
    let peak = r.memory();
    assert_eq!(peak.attempt, simple.attempt);
    assert!(peak.staged_attempt > 0);
    assert!(peak.total() >= simple.total() + peak.staged_attempt);
    r.commit(token).unwrap();
    let after = r.memory();
    assert_eq!(
        (after.attempt, after.staged_attempt),
        (peak.staged_attempt, 0)
    );
}

/// The expected native line of `SimpleSkin` for `v` and `covers`, as its
/// glyphs are counted: every character but a space.
fn native_line(v: &skin_model::NativeValues, covers: &skin_model::CoverSettings) -> String {
    let h = v
        .hispeed_x100
        .map_or_else(|| "-".to_owned(), |x| format!("{}.{:02}", x / 100, x % 100));
    let green = v
        .duration
        .map_or_else(|| "-".to_owned(), |d| d.green.to_string());
    let mode = if v.floating { "FLOATING" } else { "CLASSIC" };
    let target = v
        .target_green
        .map_or_else(String::new, |t| format!(" (TARGET {t})"));
    let white = |on: bool, w: u16| if on { w.to_string() } else { "OFF".to_owned() };
    format!(
        "HI-SPEED {h} {mode}  GREEN {green}{target}  SUDDEN+ {}  LIFT {}  HIDDEN+ {}",
        white(covers.cover_enabled, v.sudden_white),
        white(covers.lift_enabled, v.lift_white),
        white(covers.hidden_enabled, v.hidden_white),
    )
}

/// Both skins draw one snapshot sequence (design.md "Play readouts"): each
/// snapshot, through `render::presentation::frame`, gives one frame input
/// that `SimpleSkin` and `DeclSkin` both build from, across a cover, a
/// target, an `H` and a tempo change. The declarative skin's model reads
/// exactly the lanes and readouts the simple skin's native line shows,
/// and its cover paint follows the cover.
#[test]
fn both_skins_draw_one_snapshot_sequence() {
    let chart = lane_chart();
    let dir = tempfile::tempdir().unwrap();
    let r = offscreen(1_920, 1_080);
    let look = Look {
        structure: true,
        full_lanes: true,
        ..red()
    };
    let (a, _) = bundle(&r, dir.path(), &look);
    let ctx = context(&GAUGES, false);
    let mut decl = DeclSkin::new(a.clone());
    decl.prepare(r.gpu(), &attempt(&r, &chart, &ctx)).unwrap();
    let canvas = decl.canvas([1_920, 1_080]);
    let mut staging = Staging::with_runs(decl.capacity(), decl.run_capacity(), &canvas);
    staging.set_clips(a.clips().clone());
    let mut simple = render::SimpleSkin::new();
    simple.prepare_attempt(&chart, None).unwrap();
    let mut batch = render::SpriteBatch::with_capacity(MAX_SPRITES);
    let v = view(0, 0);

    let snapshot = |covers: game::Covers, g: u32, h: (u64, u64)| {
        let hispeed = game::HiSpeedSettings {
            green_target: g,
            ..game::HiSpeedSettings::default()
        };
        game::adjustment::PresentationSnapshot {
            options: game::PlayOptions::new(hispeed, covers).unwrap(),
            hispeed: game::Ratio::new(h.0, h.1),
            target: None,
            display_offset_us: 0,
            adjusting: false,
            generation: 0,
            sequence: 1,
            epoch: 1,
        }
    };
    let sudden = |amount| game::Covers {
        sudden: game::Cover { amount, on: true },
        ..game::Covers::default()
    };
    let open = game::Covers::default();
    let steps = [
        (snapshot(open, 300, (16, 5)), 150),
        (snapshot(sudden(250), 300, (16, 5)), 150),
        (snapshot(sudden(250), 299, (16, 5)), 150),
        (snapshot(sudden(250), 299, (37, 10)), 150),
        (snapshot(sudden(250), 299, (37, 10)), 300),
    ];
    let mut lines = Vec::new();
    for (s, bpm) in steps {
        let refs = game::References::all(game::Ratio::integer(150));
        let (lanes, readouts) = render::presentation::frame(
            &s,
            Some(game::Ratio::integer(bpm)),
            Some(refs),
            bms_model::Speed::NORMAL,
            true,
        );
        let f = FrameInput {
            lanes,
            readouts,
            ..input(&v, TimeUs(900_000), TimeUs(0), &[])
        };
        // The declarative skin's model reads this frame's lanes and
        // readouts, and builds from them.
        let model = render::skin_frame(&f);
        assert_eq!((model.lanes, model.readouts), (lanes, readouts));
        staging.build(&mut decl, &f).unwrap();
        let counts = format!("{:?}", staging.instances());
        // The simple skin's native line shows the same native values.
        let native = skin_model::native_values(&lanes, &readouts).unwrap();
        assert_eq!(
            skin_model::native_values(&model.lanes, &model.readouts).unwrap(),
            native
        );
        batch.clear();
        simple.build(&f, &mut batch).unwrap();
        let line = native_line(&native, &readouts.covers);
        let glyphs = batch
            .blended(render::Blend::Alpha)
            .filter(|s| s.rect[1] == 1_040.0 && s.rect[3] == 14.0)
            .count();
        assert_eq!(
            glyphs,
            line.bytes().filter(|&c| c != b' ').count(),
            "{line}"
        );
        lines.push((line, counts));
    }
    let shown: Vec<&str> = lines.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        shown,
        [
            "HI-SPEED 3.20 FLOATING  GREEN 300 (TARGET 300)  SUDDEN+ OFF  LIFT OFF  HIDDEN+ OFF",
            "HI-SPEED 3.20 FLOATING  GREEN 225 (TARGET 300)  SUDDEN+ 250  LIFT OFF  HIDDEN+ OFF",
            "HI-SPEED 3.20 FLOATING  GREEN 225 (TARGET 299)  SUDDEN+ 250  LIFT OFF  HIDDEN+ OFF",
            "HI-SPEED 3.70 FLOATING  GREEN 195 (TARGET 299)  SUDDEN+ 250  LIFT OFF  HIDDEN+ OFF",
            "HI-SPEED 3.70 FLOATING  GREEN 97 (TARGET 299)  SUDDEN+ 250  LIFT OFF  HIDDEN+ OFF",
        ]
    );
    // The declarative skin's quads follow the snapshot: the cover, then
    // `H`, move what it draws.
    assert_ne!(lines[0].1, lines[1].1);
    assert_ne!(lines[2].1, lines[3].1);
}
