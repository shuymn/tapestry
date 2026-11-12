//! The decision label slots on a device (WARP): the region, the bounded
//! writes, the completion and its device and key checks, the bitmap-only
//! path, and a line taller than its row. The book's own transitions are
//! tested without a device in `decision_labels`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use bms_model::TimeUs;
use skin_model::{
    DecisionFrame, DecisionStrings, DecisionTimers, Layout, ResultLabels, ScreenKind,
};

use super::{RenderError, Renderer};
use crate::SkinBundle;
use crate::decision_labels::{
    DECISION_TEXT_WRITE_BUDGET, FillStep, LabelBook, LabelJob, LabelKey, LabelScratch, LabelState,
    LabelStep, LabelStore, LabelWorker, Settled, StagedLabels, lock, stage_labels,
};
use crate::font_support::{self, Glyph};
use crate::outline::TextNote;
use crate::outline_support::{
    Value, bitmap, bitmap_files, decision_bundle_in, decision_folder, default_font, text, ttf,
};
use crate::test_common::{offscreen, rgb_at};

const TARGET: [u32; 2] = [1_920, 1_080];
/// The title's box, y down: 18 pixels tall, so its em is 18.
const TITLE: [i32; 4] = [100, 100, 1_200, 18];
/// The artist's box: 24 tall.
const ARTIST: [i32; 4] = [100, 300, 1_200, 24];

/// A decision skin with the title (ref 10) and the artist (ref 16) in a
/// generated Latin face.
fn truetype_skin() -> tempfile::TempDir {
    decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[
            text("f", 18, Value::Ref(10), TITLE),
            text("f", 24, Value::Ref(16), ARTIST),
        ],
    )
}

/// The renderer with `dir`'s decision skin committed.
fn installed(dir: &tempfile::TempDir) -> (Renderer, Arc<SkinBundle>) {
    let mut r = offscreen(TARGET[0], TARGET[1]);
    let b = decision_bundle_in(&r, dir.path(), &default_font());
    let token = r.install(ScreenKind::Decision, &b, 1).unwrap();
    r.commit(token).unwrap();
    (r, b)
}

fn strings(title: &str, artist: &str) -> DecisionStrings {
    DecisionStrings::new(&ResultLabels {
        title: title.to_owned(),
        artist: artist.to_owned(),
        ..ResultLabels::default()
    })
}

fn key(chart: &str) -> LabelKey {
    LabelKey {
        chart: PathBuf::from(chart),
        revision: 1,
        target: TARGET,
    }
}

/// `chart`'s job at `generation`, showing `title` and the artist `AB`, at
/// the renderer's rows.
fn job(r: &Renderer, generation: u64, chart: &str, title: &str) -> LabelJob {
    job_of(r, generation, chart, strings(title, "AB"))
}

/// `chart`'s job at `generation`, showing `strings`.
fn job_of(r: &Renderer, generation: u64, chart: &str, strings: DecisionStrings) -> LabelJob {
    LabelJob {
        generation,
        key: key(chart),
        strings,
        plan: Arc::new(r.decision_rows().unwrap().unwrap()),
    }
}

/// `job` staged on this thread, as the worker stages it.
fn stage(b: &SkinBundle, job: LabelJob) -> StagedLabels {
    let producer = b.text_producer().map(|p| &**p);
    stage_labels(
        b.skin(),
        producer,
        job,
        &mut LabelScratch::default(),
        &|| false,
    )
    .unwrap()
    .unwrap()
}

/// Writes `store`'s fill boundary by boundary to its end: the bytes each
/// boundary staged.
fn fill(r: &mut Renderer, store: &mut LabelStore) -> Vec<u64> {
    let mut boundaries = Vec::new();
    loop {
        let step = r.write_decision_labels(store);
        boundaries.push(store.progress().map_or(0, |p| p.upload));
        match step {
            FillStep::Wrote => {}
            FillStep::Complete => return boundaries,
            FillStep::Idle => panic!("nothing filling"),
        }
    }
}

/// Waits for the GPU outside the boundary's own poll, so the next
/// settle's poll sees the completion.
fn finish(r: &Renderer) {
    let _ = r.gpu.device().poll(wgpu::PollType::wait_indefinitely());
}

/// `job` staged, claimed, written and settled: published.
fn publish(r: &mut Renderer, b: &SkinBundle, store: &mut LabelStore, job: LabelJob) -> usize {
    let generation = job.generation;
    store.offer(stage(b, job));
    let slot = store.claim().unwrap();
    fill(r, store);
    finish(r);
    assert_eq!(
        r.settle_decision_labels(store).unwrap(),
        Settled::Published { slot, generation }
    );
    slot
}

fn store_for() -> LabelStore {
    LabelStore::new(LabelBook::shared(TARGET))
}

/// Every decision line lies in the decision installation's text layer,
/// inside its slot's band and its row; the decision slot keeps no text
/// cache, play's cache is untouched, and the labels draw. The output came
/// from the worker thread.
#[test]
fn decision_labels_live_in_the_decision_region() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let slot_res = &r.screens[1].as_ref().unwrap().resources;
    assert!(slot_res.text_cache.closed());
    let region = slot_res.labels.unwrap();
    let side = slot_res.atlas.side();
    let layer = slot_res.atlas.text_layer().unwrap();
    assert_eq!(region, super::LabelRegion::new(side, layer));
    assert_eq!(
        r.decision_bundle().unwrap().atlas_id(),
        b.atlas_id(),
        "the region is the committed decision array's"
    );
    let plan = r.decision_rows().unwrap().unwrap();
    assert_eq!(plan.rows().len(), 2);
    assert_eq!(plan.rows()[0].h, 56, "pad(min(1024, 2 x 18))");
    assert_eq!(plan.rows()[1].y, 56);

    let (tx, rx) = mpsc::channel();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(());
    });
    let worker = LabelWorker::for_bundle(&b, Some(wake)).unwrap();
    let mut store = store_for();
    let mut drawn = Vec::new();
    for (generation, chart) in [(1, "a"), (2, "b")] {
        worker.post(job(&r, generation, chart, "ABC"));
        rx.recv_timeout(Duration::from_secs(60)).unwrap();
        store.offer(worker.take().unwrap().unwrap());
        let slot = store.claim().unwrap();
        fill(&mut r, &mut store);
        finish(&r);
        assert_eq!(
            r.settle_decision_labels(&mut store).unwrap(),
            Settled::Published { slot, generation }
        );
        drawn.push((slot, generation));
    }
    assert_eq!(drawn.iter().map(|d| d.0).collect::<Vec<_>>(), [0, 1]);
    for &(slot, generation) in &drawn {
        let labels = store.content(slot, generation).unwrap();
        let mut lines = 0;
        for row in plan.rows() {
            let image = labels.text.get(row.slot).copied().unwrap();
            lines += 1;
            let uv = image.uv();
            assert_eq!(u32::from(uv.layer), layer);
            let [l, t, rr, bb] = uv.uv.map(|v| v * side as f32);
            let band = slot as f32 * side as f32 / 2.0;
            let top = band + row.y as f32;
            assert!(l >= 0.0 && rr <= plan.width() as f32, "{:?}", uv.uv);
            assert!(
                t >= top && bb <= top + row.h as f32,
                "{:?} in {row:?}",
                uv.uv
            );
            assert!(bb <= band + side as f32 / 2.0);
        }
        assert_eq!(lines, 2);
    }
    assert_eq!(r.text_cache.len(), 0, "play's cache took no line");

    // The second slot's labels draw.
    let skin = Arc::clone(b.skin());
    let mut dec = skin.prepare_decision();
    let (slot, generation) = drawn[1];
    let labels = store.content(slot, generation).unwrap();
    let frame = DecisionFrame {
        now: TimeUs(0),
        target: TARGET,
        layout: &Layout::AUTHOR,
        timers: DecisionTimers {
            screen: Some(TimeUs(0)),
            fadeout: None,
        },
    };
    r.build_decision(&mut dec, labels, &frame).unwrap();
    r.submit_decision(&[]).unwrap();
    let pixels = r.read_back();
    let [x, y, w, h] = TITLE.map(|v| u32::try_from(v).unwrap());
    let lit = (y..y + h)
        .flat_map(|py| (x..x + w).map(move |px| (px, py)))
        .filter(|&p| rgb_at(&pixels, TARGET[0], p)[0] > 128)
        .count();
    assert!(lit > 50, "the title draws: {lit} lit pixels");
}

/// A long title at em 256 (a line about 2 032 × 170, over 1 MiB at
/// level 0) takes more than one boundary, each within the budget.
#[test]
fn prep_writes_bounded_per_boundary() {
    let dir = decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[text("f", 256, Value::Ref(10), [0, 100, 1_900, 256])],
    );
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "ABCDEFGHIJKLMNOPQRST")));
    let slot = store.claim().unwrap();
    let boundaries = fill(&mut r, &mut store);
    assert!(boundaries.len() > 1, "{boundaries:?}");
    assert!(
        boundaries.iter().all(|&n| n <= DECISION_TEXT_WRITE_BUDGET),
        "{boundaries:?}"
    );
    let written = store
        .steps
        .iter()
        .filter(|s| **s == LabelStep::Written)
        .count();
    assert_eq!(written, boundaries.len());
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 1
        }
    );
}

/// The device lost after the submission and before the completion: the
/// completion publishes nothing, the outcome is the device's (not a
/// refusal), and the pinned slot keeps its state and content.
#[test]
fn completion_with_device_lost_publishes_nothing() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    let first = job(&r, 1, "a", "AB");
    let pinned = publish(&mut r, &b, &mut store, first);
    lock(store.book())
        .pin(std::path::Path::new("a"), 1, 7)
        .unwrap();
    r.decision_after_submit = Some(|gpu| gpu.device().destroy());
    store.offer(stage(&b, job(&r, 2, "b", "BA")));
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    store.steps.clear();
    let mut outcome = Ok(Settled::Pending);
    for _ in 0..1_000 {
        outcome = r.settle_decision_labels(&mut store);
        if !matches!(outcome, Ok(Settled::Pending)) {
            break;
        }
        finish(&r);
    }
    let e = outcome.unwrap_err();
    assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    assert!(!e.is_refusal());
    assert!(
        store.steps.contains(&LabelStep::CompletionSeen),
        "{:?}",
        store.steps
    );
    assert!(store.steps.contains(&LabelStep::LostChecked));
    assert!(!store.steps.contains(&LabelStep::Published));
    let book = lock(store.book());
    assert!(!book.healthy());
    assert!(!matches!(book.state(slot), Some(LabelState::Ready { .. })));
    assert!(matches!(
        book.state(pinned),
        Some(LabelState::Pinned {
            generation: 1,
            lifetime: 7,
            ..
        })
    ));
    drop(book);
    assert!(store.content(slot, 2).is_none());
    assert!(store.content(pinned, 1).is_some());
}

/// The target changed between the submission and the completion: the
/// completion publishes nothing and frees the slot.
#[test]
fn target_change_between_submit_and_completion_publishes_nothing() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    assert!(store.steps.contains(&LabelStep::Submitted));
    lock(store.book()).invalidate([1_280, 720]);
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Stale
    );
    assert!(matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Free)
    ));
    assert!(store.content(slot, 1).is_none());
    assert!(!store.steps.contains(&LabelStep::Published));
}

/// A decision skin whose labels are all in a bitmap font: no region, no
/// write and no submission; its claimed slot publishes at the boundary
/// only while the device is healthy, with its bitmap lines.
#[test]
fn bitmap_only_prep_publishes_at_next_boundary() {
    let dir = decision_folder(
        &bitmap_files(),
        &[bitmap()],
        &[
            text("g", 8, Value::Ref(10), TITLE),
            text("g", 8, Value::Ref(16), ARTIST),
        ],
    );
    let (mut r, b) = installed(&dir);
    assert!(r.screens[1].as_ref().unwrap().resources.labels.is_none());
    let plan = r.decision_rows().unwrap().unwrap();
    assert!(plan.rows().is_empty());
    let mut store = store_for();
    // While the device is not healthy, nothing publishes.
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    lock(store.book()).set_unhealthy();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Unhealthy
    );
    assert!(matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Filling { .. })
    ));
    // Healthy again: published at its boundary, nothing written.
    lock(store.book()).reset();
    store.steps.clear();
    store.offer(stage(&b, job(&r, 2, "a", "AB")));
    let slot = store.claim().unwrap();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    let progress = store.progress().unwrap();
    assert!(!progress.wrote && progress.done.is_none());
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 2
        }
    );
    assert!(!store.steps.contains(&LabelStep::Written));
    assert!(!store.steps.contains(&LabelStep::Submitted));
    let labels = store.content(slot, 2).unwrap();
    assert_eq!(labels.bitmap.len(), b.skin().text_slots());
    assert!(labels.bitmap.heap_bytes() > 0);
}

/// A line taller than its row (a face 3.5 em tall in a 2 em row) is a
/// note, its label not drawn; the preparation is not refused and the
/// other line publishes.
#[test]
fn line_taller_than_its_row_is_a_note() {
    let mut tall = font_support::latin("Tall");
    tall.glyphs[usize::from(font_support::capital(0))] = Glyph {
        advance: 600,
        rect: Some([50, 0, 550, 3_500]),
    };
    let dir = decision_folder(
        &[("ttf.ttf", font_support::build(&tall))],
        &[ttf()],
        &[
            text("f", 18, Value::Ref(10), TITLE),
            text("f", 24, Value::Ref(16), ARTIST),
        ],
    );
    let (mut r, b) = installed(&dir);
    let staged = stage(&b, job_of(&r, 1, "a", strings("A", "B")));
    assert_eq!(staged.notes(), [(0, TextNote::TooLarge)]);
    let mut store = store_for();
    store.offer(staged);
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 1
        }
    );
    let labels = store.content(slot, 1).unwrap();
    assert!(labels.text.get(0).is_none());
    assert!(labels.text.get(1).is_some());
}

/// Rows past half the text layer (three labels at em 256 at side 2048:
/// 3 × 528 rows of 1 024) refuse the kind.
#[test]
fn decision_rows_refuse_a_misfit() {
    let dir = decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[
            text("f", 256, Value::Ref(10), [0, 0, 100, 256]),
            text("f", 256, Value::Ref(11), [0, 300, 100, 256]),
            text("f", 256, Value::Ref(16), [0, 600, 100, 256]),
        ],
    );
    let (r, _) = installed(&dir);
    let side = r.screens[1].as_ref().unwrap().resources.atlas.side();
    let e = r.decision_rows().unwrap().unwrap_err();
    assert!(
        matches!(e, RenderError::LabelRegion { needed: 1_584, band } if band == side / 2),
        "{e:?}"
    );
    assert!(e.is_refusal());
}
