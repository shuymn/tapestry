//! The attempt's chart images as the uploader's payload on WARP (design.md
//! "Rendering and Skins", chart images; "Memory Budget", the uploader):
//! the target is unavailable from the take, each band of at most 4 MiB is
//! its own step, and the table is published only at the completion of the
//! last band, into a target still the array taken. A discard, a
//! replacement or a loss after the first band writes nothing more and
//! publishes no table; a refused band publishes the all-absent table; the
//! hold's bound read before a band writes none past it.

use std::time::Duration;

use skin_model::chart_image::{ChartImage, ChartTable, Fitted};

use super::tests::{chart_picture, chart_renderer, labels, refuse, stage_labels};
use super::{ChartTarget, ChartUpload, RenderError, Renderer, UploadError};

/// The bands of `images`: each level's rows in writes of at most
/// [`UPLOAD_WRITE`](crate::UPLOAD_WRITE) bytes as the queue stages them.
fn bands(images: [Option<&Fitted>; 3]) -> usize {
    images
        .iter()
        .flatten()
        .map(|f| {
            (0..f.levels().len())
                .map(|k| {
                    let [w, h] = f.level_size(k).unwrap();
                    let rows = (crate::UPLOAD_WRITE / crate::text_upload::padded(w).max(1)).max(1);
                    usize::try_from(u64::from(h).div_ceil(rows)).unwrap()
                })
                .sum::<usize>()
        })
        .sum()
}

/// A stage file and a back image, each as large as its slot of a chart
/// layer of side `side`.
fn big(side: u32) -> [Fitted; 2] {
    let s = side / 2 - 8;
    [
        chart_picture(ChartImage::Stage, side, [s, s], [10, 20, 30]),
        chart_picture(ChartImage::Back, side, [s, s], [40, 50, 60]),
    ]
}

/// One boundary of a zero slice, each step's completion seen at once, so
/// one band: how the publication ended there, if it did.
fn boundary(
    r: &mut Renderer,
    up: &mut ChartUpload,
    images: [Option<&Fitted>; 3],
) -> Option<Result<ChartTable, RenderError>> {
    r.step_seen.set(Some(true));
    let out = r.upload_boundary_within(Duration::ZERO, &mut || false, Some((up, images)));
    r.step_seen.set(None);
    out.chart
}

/// The committed installation's published table.
fn presented(r: &Renderer) -> Option<ChartTable> {
    r.installed.as_ref().unwrap().loading().chart_images()
}

/// A blue play skin keeping a chart layer, over a new atlas.
fn other(r: &Renderer) -> (tempfile::TempDir, std::sync::Arc<crate::SkinBundle>) {
    let dir = tempfile::tempdir().unwrap();
    let look = crate::test_decl::Look {
        chart: true,
        ..crate::test_decl::Look::left([0, 0, 255, 255])
    };
    let (b, _) = crate::test_decl::bundle(r, dir.path(), &look);
    (dir, b)
}

/// V3, M5: one band a step, the target unavailable until the boundary
/// that sees the last band complete, which publishes the table the whole
/// call would.
#[test]
fn a_publication_writes_a_band_a_step_and_publishes_at_its_last_completion() {
    let (mut r, _dir, side) = chart_renderer();
    let [s, b] = big(side);
    let images = [Some(&s), Some(&b), None];
    let n = bands(images);
    assert!(n >= 4, "{n} bands");
    // An earlier chart's table, published whole.
    let small = chart_picture(ChartImage::Stage, side, [64, 64], [1, 2, 3]);
    r.publish_chart_images(ChartTarget::Committed, [Some(&small), None, None])
        .unwrap();
    assert!(presented(&r).is_some());
    let mut up = r
        .begin_chart_upload(ChartTarget::Committed, images)
        .unwrap();
    assert_eq!(presented(&r), None, "unavailable from the take");
    r.chart_submissions.clear();
    for i in 1..n {
        assert!(boundary(&mut r, &mut up, images).is_none(), "band {i}");
        assert_eq!(r.chart_submissions.len(), i, "one band a step");
        assert_eq!(presented(&r), None, "no table before the last band");
    }
    let table = boundary(&mut r, &mut up, images)
        .expect("ended at its last band's completion")
        .unwrap();
    assert_eq!(r.chart_submissions.len(), n);
    assert_eq!(presented(&r), Some(table));
    assert_eq!(r.valid_chart(), Some(table));
    let whole = r
        .publish_chart_images(ChartTarget::Committed, images)
        .unwrap();
    assert_eq!(whole, table, "the whole call's table");
}

/// V3: a staging discarded after the first band ends the publication at
/// the next boundary, nothing more written and no table.
#[test]
fn a_discard_after_the_first_band_ends_it_unwritten() {
    let (mut r, _dir, side) = chart_renderer();
    let (_d, b) = other(&r);
    let token = stage_labels(&mut r, &b, &labels("T", "A", "G"), 2).unwrap();
    let [s, back] = big(side);
    let images = [Some(&s), Some(&back), None];
    let mut up = r
        .begin_chart_upload(ChartTarget::Staged(token), images)
        .unwrap();
    r.chart_submissions.clear();
    assert!(boundary(&mut r, &mut up, images).is_none());
    assert_eq!(r.chart_submissions.len(), 1);
    r.discard();
    let end = boundary(&mut r, &mut up, images).expect("ended");
    assert!(matches!(end, Err(RenderError::ChartChanged)), "{end:?}");
    assert_eq!(r.chart_submissions.len(), 1, "a band after the discard");
}

/// V3: the committed installation replaced after the first band ends the
/// publication, nothing more written and no table on the new one.
#[test]
fn a_replacement_after_the_first_band_ends_it_unwritten() {
    let (mut r, _dir, side) = chart_renderer();
    let [s, back] = big(side);
    let images = [Some(&s), Some(&back), None];
    let mut up = r
        .begin_chart_upload(ChartTarget::Committed, images)
        .unwrap();
    r.chart_submissions.clear();
    assert!(boundary(&mut r, &mut up, images).is_none());
    let (_d, b) = other(&r);
    crate::test_common::install(&mut r, &b, 3);
    let end = boundary(&mut r, &mut up, images).expect("ended");
    assert!(matches!(end, Err(RenderError::ChartChanged)), "{end:?}");
    assert_eq!(r.chart_submissions.len(), 1, "a band after the replacement");
    assert_eq!(presented(&r), None);
    assert_eq!(r.valid_chart(), None);
}

/// V3: a device lost after the first band ends the publication as the
/// device's, nothing more written and no table.
#[test]
fn a_loss_after_the_first_band_ends_it_unwritten() {
    let (mut r, _dir, side) = chart_renderer();
    let [s, back] = big(side);
    let images = [Some(&s), Some(&back), None];
    let mut up = r
        .begin_chart_upload(ChartTarget::Committed, images)
        .unwrap();
    r.chart_submissions.clear();
    assert!(boundary(&mut r, &mut up, images).is_none());
    r.gpu.device().destroy();
    // The device reports its loss at its next poll.
    let _ = r.gpu.device().poll(wgpu::PollType::wait_indefinitely());
    assert!(r.gpu.lost());
    let end = boundary(&mut r, &mut up, images).expect("ended");
    assert!(matches!(end, Err(RenderError::DeviceLost)), "{end:?}");
    assert_eq!(r.chart_submissions.len(), 1, "a band after the loss");
    assert_eq!(r.valid_chart(), None);
}

/// A band the device refuses ends the publication with the all-absent
/// table, as the whole call's refused write does.
#[test]
fn a_refused_band_publishes_the_absent_table() {
    let (mut r, _dir, side) = chart_renderer();
    let [s, back] = big(side);
    let images = [Some(&s), Some(&back), None];
    let mut up = r
        .begin_chart_upload(ChartTarget::Committed, images)
        .unwrap();
    r.chart_fault = Some(refuse);
    let end = boundary(&mut r, &mut up, images).expect("ended");
    assert!(
        matches!(end, Err(RenderError::Atlas(UploadError::Device(_)))),
        "{end:?}"
    );
    let absent = ChartTable::ABSENT.keyed(r.chart_key().unwrap());
    assert_eq!(presented(&r), Some(absent));
}

/// M1: the hold's bound, read before each band, writes none once it has
/// passed; the publication stays in hand.
#[test]
fn the_holds_bound_writes_no_band_past_it() {
    let (mut r, _dir, side) = chart_renderer();
    let [s, back] = big(side);
    let images = [Some(&s), Some(&back), None];
    let mut up = r
        .begin_chart_upload(ChartTarget::Committed, images)
        .unwrap();
    r.chart_submissions.clear();
    let out = r.upload_boundary_within(
        Duration::from_secs(60),
        &mut || true,
        Some((&mut up, images)),
    );
    assert!(out.chart.is_none(), "still in hand");
    assert_eq!(r.chart_submissions.len(), 0, "a band past the bound");
    assert!(boundary(&mut r, &mut up, images).is_none());
    assert_eq!(r.chart_submissions.len(), 1);
}
