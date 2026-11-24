//! The uploader's staging steps on WARP (design.md "Memory Budget", the
//! uploader): a staging with its own array is created with nothing
//! written, and answers ready only after its last step's completion, its
//! initialization and pages written in atlas steps of writes of at most
//! 4 MiB under one submission, at most 32 MiB a step; a hold's bound read
//! before each write closes a step with the writes it has; a step refused,
//! or the device lost, retires the staging, reports its one end and
//! answers its request with the refusal.
//!
//! The fixture is one 3 500² cell on a 3 584-texel page: 49 MiB at level
//! 0 and 16 MiB below, 19 page writes of at most 4 MiB after the
//! initialization's, so three steps.

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

/// The bytes each write of `b`'s array stages, in order: its
/// initialization (the built-in atlas, then a zero texel at each level of
/// each kept layer), then its pages' bands of at most 4 MiB, level by
/// level.
fn write_sizes(b: &render::SkinBundle) -> Vec<u64> {
    use crate::text_upload::padded;
    let pages = b.atlas_pages();
    let kept = u64::from(pages.text_layer().is_some()) + u64::from(pages.chart_layer().is_some());
    let levels = skin_model::MIP_LEVELS as u64;
    let mut sizes = vec![padded(crate::atlas::ATLAS_SIZE) * u64::from(crate::atlas::ATLAS_SIZE)];
    sizes.extend((0..kept * levels).map(|_| padded(1)));
    for _ in pages.pages() {
        for k in 0..skin_model::MIP_LEVELS {
            let s = b.side() >> k;
            let per = (crate::UPLOAD_WRITE / padded(s)).max(1);
            let mut left = u64::from(s);
            while left > 0 {
                let rows = left.min(per);
                sizes.push(padded(s) * rows);
                left -= rows;
            }
        }
    }
    sizes
}

/// The writes of each atlas step for `sizes` in order, by the step's
/// rule: the first always, each later one while the step's bytes with it
/// stay within 32 MiB.
fn steps_of(sizes: &[u64]) -> Vec<u64> {
    let mut steps = Vec::new();
    let (mut n, mut bytes) = (0_u64, 0_u64);
    for &size in sizes {
        if n > 0 && bytes + size > crate::UPLOAD_STEP {
            steps.push(n);
            (n, bytes) = (0, 0);
        }
        n += 1;
        bytes += size;
    }
    if n > 0 {
        steps.push(n);
    }
    steps
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

/// M3: a staging's atlas steps stop at the boundary's slice: each step's
/// completion seen at once and each step moving the slice clock 500 µs, a
/// 501 µs slice takes two of the fixture's three steps (the second begun
/// at 500 µs, none at 1 000 µs), and the next boundary the third.
#[test]
fn a_staging_s_steps_stop_at_the_slice() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    assert_eq!(steps_of(&write_sizes(&b)).len(), 3);
    r.step_cost.set(Some(Duration::from_micros(500)));
    r.step_seen.set(Some(true));
    let up = r.upload_boundary_within(Duration::from_micros(501), &mut || false, None);
    assert_eq!(up.staged, None, "not every step in one boundary");
    assert_eq!(r.step_submits.get(), 2, "two steps in the slice");
    let up = r.upload_boundary_within(Duration::from_micros(501), &mut || false, None);
    assert_eq!(r.step_submits.get(), 3);
    assert!(
        matches!(up.staged, Some(crate::StagingEnd::Ready(_))),
        "{:?}",
        up.staged
    );
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
        if let Some(crate::StagingEnd::Ready(t)) = up.staged {
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
/// atlas steps under one submission each, a write joining its step only
/// while the step's bytes stay within 32 MiB, the initialization's writes
/// first, so the array's writes go over in three submissions.
// Revert: a step's bound counted in writes again (eight a step whatever
// their bytes) -> the first step takes the initialization and eight
// bands, past 32 MiB.
#[test]
fn an_atlas_step_stays_within_its_bytes_under_one_submission() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    let sizes = write_sizes(&b);
    let steps = steps_of(&sizes);
    assert_eq!(steps.len(), 3, "{steps:?}");
    r.step_seen.set(Some(true));
    let up = r.upload_boundary_within(Duration::from_secs(60), &mut || false, None);
    r.step_seen.set(None);
    assert_eq!(*r.step_writes.borrow(), steps);
    assert_eq!(r.step_submits.get(), 3);
    assert!(
        matches!(up.staged, Some(crate::StagingEnd::Ready(_))),
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
    let sizes = write_sizes(&b);
    // Its completion withheld: the step stays in flight.
    r.step_seen.set(Some(false));
    // Read before the step and immediately before each write: the bound
    // passes at the fifth look, before the step's fourth write.
    let mut looks = 0;
    let up = r.upload_boundary_within(
        Duration::from_secs(60),
        &mut || {
            looks += 1;
            looks > 4
        },
        None,
    );
    assert_eq!(*r.step_writes.borrow(), [3], "closed after its third write");
    assert_eq!(r.step_submits.get(), 1, "submitted");
    assert!(r.step.is_some(), "its completion registered");
    assert_eq!(up.staged, None);
    r.step_seen.set(Some(true));
    // Past the bound before the first write: nothing written at all.
    let writes = r.queue_writes();
    let _ = r.upload_boundary_within(Duration::from_secs(60), &mut || true, None);
    assert_eq!(r.step_submits.get(), 1, "no write past the bound");
    assert_eq!(r.queue_writes(), writes, "no write past the bound");
    let up = r.upload_boundary_within(Duration::from_secs(60), &mut || false, None);
    r.step_seen.set(None);
    let mut expected = vec![3];
    expected.extend(steps_of(&sizes[3..]));
    assert_eq!(*r.step_writes.borrow(), expected);
    assert!(matches!(up.staged, Some(crate::StagingEnd::Ready(_))));
}

/// A refusal at a step ends the staging: its array retires into the
/// entry it reserved, nothing is staged, the boundary reports its one end
/// (the event a waiting preparation is asked again on), and its request
/// asked again answers the refusal once.
// Revert: the refusal not reported (`out.staged` left `None` when a
// step retired the staging) -> "its one end reported".
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
    assert_eq!(
        up.staged,
        Some(crate::StagingEnd::Refused(1)),
        "its one end reported"
    );
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

/// H2 (r2): a stepped staging's array is created with nothing written:
/// its creation queues no write; a boundary whose gate refuses every
/// write writes nothing; the first write admitted is its initialization's
/// (the built-in atlas in layer 0), through the uploader.
// Revert: the initialization written at creation again (`make` writing
// the built-in atlas and the zero texels whatever `whole`) -> "the
// creation queued writes".
#[test]
fn a_stepped_creation_writes_nothing_until_a_boundary_admits_it() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    let before = r.queue_writes();
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    assert_eq!(r.queue_writes(), before, "the creation queued writes");
    let _ = r.upload_boundary_within(Duration::from_secs(60), &mut || true, None);
    assert_eq!(r.queue_writes(), before, "a write past the gate");
    assert_eq!(r.step_submits.get(), 0);
    r.step_seen.set(Some(false));
    let mut looks = 0;
    let _ = r.upload_boundary_within(
        Duration::from_secs(60),
        &mut || {
            looks += 1;
            looks > 2
        },
        None,
    );
    r.step_seen.set(None);
    assert_eq!(r.queue_writes(), before + 1, "one write admitted");
    assert_eq!(*r.step_writes.borrow(), [1]);
    let sizes = write_sizes(&b);
    assert_eq!(
        sizes[0],
        crate::text_upload::padded(crate::atlas::ATLAS_SIZE) * u64::from(crate::atlas::ATLAS_SIZE),
        "the initialization first"
    );
}

/// H1 (r2): the device lost while a staging steps ends it at the next
/// boundary with its one end reported, and its request asked again
/// answers the loss: a waiting preparation is never left without an
/// answer.
// Revert: the loss not ending the staging (the `gpu.lost()` branch only
// breaking) -> "its end reported on the loss".
#[test]
fn a_lost_device_ends_a_staging_with_its_one_end() {
    let mut r = offscreen(W, H);
    let b = big(&r);
    let chart = chart();
    let ctx = context(&GAUGES, false);
    assert_eq!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1).unwrap(),
        None
    );
    r.gpu().device().destroy();
    let mut end = None;
    for _ in 0..100 {
        end = r
            .upload_boundary_within(Duration::from_secs(1), &mut || false, None)
            .staged;
        if end.is_some() || r.gpu().lost() {
            break;
        }
    }
    if end.is_none() {
        end = r
            .upload_boundary_within(Duration::from_secs(1), &mut || false, None)
            .staged;
    }
    assert_eq!(
        end,
        Some(crate::StagingEnd::Refused(1)),
        "its end reported on the loss"
    );
    assert!(matches!(
        r.stage_stepped(&b, &attempt(&r, &chart, &ctx), 1),
        Err(RenderError::DeviceLost)
    ));
}
