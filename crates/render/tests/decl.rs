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

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{GAUGES, attempt, context, input, offscreen, view};
use decl_support::{Look, bundle};
use eval::GaugeState;
use render::{CommitError, MAX_SPRITES, PreparedToken, RenderError, Renderer, SkinBundle};
use rules::GaugeValue;
use skin_model::{BuildError, GaugeDesc, GaugeLook};

const BLACK: [u8; 3] = [0; 3];
const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
/// The left half's centre and the right half's.
const LEFT: (u32, u32) = (480, 540);
const RIGHT: (u32, u32) = (1_440, 540);

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// Draws a frame of the default view and reads it back: its sprites and
/// the pixels.
fn frame(r: &mut Renderer) -> (usize, Vec<u8>) {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented);
    assert_eq!(timing.dropped, 0);
    (timing.sprites, r.read_back())
}

/// The RGB of pixel `(x, y)` of a 1920-wide read back.
fn px(pixels: &[u8], (x, y): (u32, u32)) -> [u8; 3] {
    let i = ((y * 1_920 + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
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
/// with 9 000 more image elements than one, past [`MAX_SPRITES`].
fn blue() -> Look {
    Look {
        side: 300,
        clip: Some([960.0, 0.0, 960.0, 1_080.0]),
        copies: 9_000,
        ..Look::left([0, 0, 255, 255])
    }
}

/// A staged installation leaves the one in use drawing, through frames,
/// until its commit switches the skin, the uv table, the atlas, the clips
/// and the capacities together: red under the left clip, from a 256-texel
/// page and the least capacity, then blue under the right clip, from a
/// 512-texel page and 9 001 sprites. Any of them switched alone would
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
    assert_eq!(r.capacity(), (9_001, 9_001));
    let (sprites, p) = frame(&mut r);
    assert_eq!(sprites, 9_001);
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
    assert_eq!(sprites, 9_002);
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
    // Committed once, it is gone.
    assert_eq!(r.commit(token), Err(CommitError::NothingStaged));
    // A discarded staging cannot be committed either.
    let token = stage(&mut r, &a, &chart, 8).unwrap();
    r.discard();
    assert_eq!(r.commit(token), Err(CommitError::NothingStaged));
}

/// Two more gauges for the retry.
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
    halves(&r.read_back(), RED, BLACK);
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
    again.restore(&a).unwrap();
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

/// A frame's model input comes from the frame alone: the tally, the
/// gauges, the completion and the play phase from its one view, the
/// effects as drained, its target and visible time, the author's layout,
/// no cover and no readout.
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
    assert_eq!(s.lanes.visible_time, common::VISIBLE);
    assert_eq!((s.lanes.cover, s.lanes.lift, s.lanes.hidden), (0, 0, 0));
    assert!(s.loading.is_none());
    assert_eq!(s.readouts.bpm_current_x100, None);
    assert_eq!(s.readouts.hispeed_x100, None);
    assert!(!s.readouts.covers.adjusting);
}
