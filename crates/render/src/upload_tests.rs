//! The uploader's staging steps on WARP (design.md "Memory Budget", the
//! uploader): a staging with its own array answers ready only after its
//! last step's completion, its pages written in atlas steps of up to
//! eight 4 MiB writes under one submission; a hold's bound closes a step
//! with the writes it has; a step refused retires the staging and answers
//! its request with the refusal.
//!
//! The fixture is one 3 500² cell on a 3 584-texel page: 49 MiB at level
//! 0 and 16 MiB below, 19 writes of at most 4 MiB, so three steps.

use std::time::Duration;

use bms_model::Chart;
use bms_model::{ChartBuilder, TimeUs};
use render::{CommitError, RenderError, Renderer, UploadState};

use crate::test_common::{GAUGES, attempt, context, input, offscreen, read_back, rgb_at, view};
use crate::test_decl::{Look, solid_bundle};

const W: u32 = 1_920;
const H: u32 = 1_080;
const GREEN: [u8; 4] = [0, 255, 0, 255];
/// The left half's top and bottom, which sample the cell's first and last
/// rows: a page written only in part leaves the bottom unwritten.
const TOP: (u32, u32) = (480, 60);
const BOTTOM: (u32, u32) = (480, 1_020);

/// The writes of `b`'s pages, each at most 4 MiB as the queue stages
/// them, level by level.
fn writes(b: &render::SkinBundle) -> u64 {
    let side = b.side();
    let per_page: u64 = (0..skin_model::MIP_LEVELS)
        .map(|k| {
            let s = side >> k;
            let rows = (crate::UPLOAD_WRITE / crate::text_upload::padded(s)).max(1);
            u64::from(s).div_ceil(rows)
        })
        .sum();
    per_page * u64::try_from(b.atlas_pages().pages().len()).unwrap()
}

fn chart() -> Chart {
    ChartBuilder::new(120_000).build().unwrap()
}

/// The fixture's bundle on `r`'s device.
fn big(r: &Renderer) -> std::sync::Arc<render::SkinBundle> {
    let b = solid_bundle(r, &Look::left(GREEN), [3_500, 3_500]);
    assert_eq!(b.side(), 3_584);
    b
}

/// The left half's colour at its top and bottom after a play frame.
fn colours(r: &mut Renderer) -> ([u8; 3], [u8; 3]) {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented());
    let pixels = read_back(r);
    (rgb_at(&pixels, W, TOP), rgb_at(&pixels, W, BOTTOM))
}

/// A staging asked again answers `None` until the boundary that sees its
/// last step's completion, and ready from then on; nothing of it commits
/// or draws before (V2).
#[test]
fn a_staging_answers_ready_only_after_its_last_step() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    let staged = r.staged().expect("staged, stepping");
    assert_eq!(r.staged_ready(), None, "nothing drawn while it steps");
    assert_eq!(
        r.upload_state(),
        UploadState::Ready,
        "created, no page written"
    );
    assert_eq!(r.commit(staged), Err(CommitError::NothingStaged));
    let mut before = 0;
    let ready = loop {
        let up = r.upload_boundary_within(Duration::from_secs(10), &mut || false, None);
        if let Some(t) = up.staged {
            break t;
        }
        before += 1;
        assert!(before < 100, "never ready");
        assert_eq!(
            r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
            None,
            "not ready before its last step's completion"
        );
        assert_eq!(r.staged_ready(), None);
        if r.upload_state() == UploadState::InFlight {
            assert!(r.wait_step(Duration::from_secs(10)).unwrap());
        }
    };
    assert!(
        before >= 1,
        "its last step's completion seen at a later boundary"
    );
    assert_eq!(ready, staged);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        Some(staged),
        "ready once its last step completed, asked again"
    );
    assert_eq!(r.staged_ready(), Some(staged));
    assert_eq!(r.upload_state(), UploadState::Idle);
    r.commit(staged).unwrap();
    assert_eq!(
        colours(&mut r),
        ([0, 255, 0], [0, 255, 0]),
        "every row written"
    );
}

/// One boundary with time for every step, each completion seen at once:
/// atlas steps of eight writes under one submission each, the last of
/// what is left, so 19 writes go over in three submissions.
#[test]
fn an_atlas_step_is_eight_writes_under_one_submission() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    let n = writes(&b);
    assert!((17..=24).contains(&n), "{n} writes: three steps");
    r.step_seen.set(Some(true));
    let up = r.upload_boundary_within(Duration::from_secs(60), &mut || false, None);
    r.step_seen.set(None);
    assert_eq!(*r.step_writes.borrow(), [8, 8, n - 16]);
    assert_eq!(r.step_submits.get(), 3);
    assert!(
        up.staged.is_some(),
        "ready in the boundary that saw its last step"
    );
}

/// A hold's bound reached inside a step closes it with the writes it has,
/// submitted with its completion registered; nothing more is written in
/// that boundary, and the next boundary writes the rest.
#[test]
fn the_holds_bound_closes_a_step_with_its_writes() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    let n = writes(&b);
    // Its completion withheld: the step stays in flight.
    r.step_seen.set(Some(false));
    // Read before each write: the bound passes after the third.
    let mut looks = 0;
    let up = r.upload_boundary_within(
        Duration::from_secs(60),
        &mut || {
            looks += 1;
            looks > 3
        },
        None,
    );
    assert_eq!(*r.step_writes.borrow(), [3], "closed after its third write");
    assert_eq!(r.step_submits.get(), 1, "submitted");
    assert!(r.step.is_some(), "its completion registered");
    assert_eq!(up.staged, None);
    r.step_seen.set(Some(true));
    // Past the bound before the first write: nothing written at all.
    let _ = r.upload_boundary_within(Duration::from_secs(60), &mut || true, None);
    assert_eq!(r.step_submits.get(), 1, "no write past the bound");
    let up = r.upload_boundary_within(Duration::from_secs(60), &mut || false, None);
    r.step_seen.set(None);
    assert_eq!(*r.step_writes.borrow(), [3, 8, n - 11]);
    assert!(up.staged.is_some());
}

/// A refusal at a step ends the staging: its array retires into the
/// entry it reserved, nothing is staged, and its request asked again
/// answers the refusal once.
#[test]
fn a_refused_step_retires_the_staging_and_answers_its_request() {
    fn refuse(gpu: &crate::gpu::Gpu) {
        // A copy past the end of a buffer: a validation error.
        let device = gpu.device();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue().write_buffer(&buffer, 8, &[0; 4]);
    }
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    let filled = r.filled();
    r.step_submit_fault = Some(refuse);
    let up = r.upload_boundary();
    r.step_submit_fault = None;
    assert_eq!(up.staged, None);
    assert_eq!(r.staged(), None, "nothing staged");
    assert_eq!(r.filled(), filled + 1, "its array in its entry");
    let e = r
        .stage_stepped(&b, &attempt(&r, &chart, &ctx), 1)
        .unwrap_err();
    assert!(matches!(e, RenderError::Atlas(_)), "{e:?}");
    assert!(e.is_refusal());
    assert!(matches!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1),
        Err(RenderError::Superseded { .. })
    ));
}

/// M1: a text band is at most one write of padded bytes, at least one row:
/// a 2 048-texel level of 1 024 rows (8 MiB) takes two bands of 512, a
/// row wider than a write goes alone.
// Revert: one band a level (`band_rows` answering `left`) -> "8 MiB in
// one band".
#[test]
fn a_text_band_is_at_most_one_write() {
    use crate::text_upload::{band_rows, padded};
    assert_eq!(band_rows(2_048, 1_024), 512, "8 MiB in one band");
    assert_eq!(band_rows(2_048, 100), 100);
    assert_eq!(band_rows(2_000_000, 5), 1);
    for w in [1, 64, 500, 2_048, 4_096] {
        let rows = band_rows(w, u32::MAX);
        assert!(rows >= 1);
        assert!(padded(w) * u64::from(rows) <= crate::UPLOAD_WRITE, "{w}");
    }
}
