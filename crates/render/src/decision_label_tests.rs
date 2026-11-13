//! The decision label slots on a device (WARP): the region, the bounded
//! writes and their scopes, the completion and its device and key checks,
//! the bitmap-only path, a line taller than its row, a pinned slot's
//! restore after a reset, and the worker's supersession and drop. The
//! book's own transitions are tested without a device in
//! `decision_labels`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use bms_model::TimeUs;
use skin_model::{
    DecisionFrame, DecisionStrings, DecisionTimers, Layout, ResultLabels, ScreenKind, TextRequest,
};

use super::{RenderError, Renderer};
use crate::decision_labels::{
    CoolFrom, DECISION_TEXT_WRITE_BUDGET, FillStep, Gen, LABEL_SLOTS, LabelBook, LabelJob,
    LabelKey, LabelScratch, LabelState, LabelStep, LabelStore, LabelWorker, Settled, StagedLabels,
    WorkerHooks, lock, stage_bundle_labels, stage_labels,
};
use crate::decl::AttemptText;
use crate::font_support::{self, Glyph};
use crate::outline::{LineBounds, LineFit, LineLayout, TextNote, TextResidency};
use crate::outline_support::{
    Value, bitmap, bitmap_files, decision_bundle_in, decision_folder, default_font, text, ttf,
};
use crate::test_common::{offscreen, rgb_at};
use crate::{SkinBundle, SkinError};

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
    installed_at(dir, TARGET)
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
    LabelJob::new(
        generation,
        PathBuf::from(chart),
        1,
        strings,
        Arc::new(r.decision_rows().unwrap().unwrap()),
    )
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
    let generation = job.generation();
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
        let j = job(&r, generation, chart, "ABC");
        store.post(&worker, j, &mut || r.watch_completion());
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

/// The longest a test waits for another thread before it fails:
/// `TAPESTRY_TEST_WATCHDOG` seconds, 60 unless set.
fn watchdog() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// The decision frame at 0 of `labels` built and drawn on `r`: the lit
/// pixels in the title's box.
fn title_pixels(r: &mut Renderer, b: &SkinBundle, labels: skin_model::DecisionLabels<'_>) -> usize {
    let skin = Arc::clone(b.skin());
    let mut dec = skin.prepare_decision();
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
    (y..y + h)
        .flat_map(|py| (x..x + w).map(move |px| (px, py)))
        .filter(|&p| rgb_at(&pixels, TARGET[0], p)[0] > 128)
        .count()
}

/// A pinned slot after a reset (the renderer built again, its decision
/// installation restored on a new array): its content is not drawable,
/// only its strings are kept, until it is restored on the new renderer
/// under the same slot and generation; then it draws there.
#[test]
fn reset_pinned_slot_draws_only_after_its_restore() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    let first = job(&r, 1, "a", "ABC");
    let slot = publish(&mut r, &b, &mut store, first);
    lock(store.book())
        .pin(std::path::Path::new("a"), 1, 7)
        .unwrap();
    assert!(store.content(slot, 1).is_some());
    drop(r);
    // Built again: a new device, the installation on a new array.
    let (mut r2, b2) = installed(&dir);
    store.reset();
    lock(store.book()).reset();
    assert!(store.content(slot, 1).is_none(), "no stale table drawn");
    assert!(store.content_of(slot, 1).is_none());
    assert!(lock(store.book()).awaits_restore(slot, 1));
    let strings = store.restore_strings(slot, 1).unwrap().clone();
    // Not restoring at another generation: nothing written.
    let other = stage_bundle_labels(&b2, job_of(&r2, 2, "a", strings.clone()))
        .unwrap()
        .unwrap();
    assert!(!r2.restore_decision_labels(&mut store, slot, other).unwrap());
    assert!(store.content(slot, 1).is_none());
    let staged = stage_bundle_labels(&b2, job_of(&r2, 1, "a", strings))
        .unwrap()
        .unwrap();
    assert!(
        r2.restore_decision_labels(&mut store, slot, staged)
            .unwrap()
    );
    let book = lock(store.book());
    assert!(!book.awaits_restore(slot, 1));
    assert!(matches!(
        book.state(slot),
        Some(LabelState::Pinned {
            generation: 1,
            lifetime: 7,
            restoring: false,
            ..
        })
    ));
    drop(book);
    let labels = store.content(slot, 1).unwrap();
    let lit = title_pixels(&mut r2, &b2, labels);
    assert!(lit > 50, "the restored title draws: {lit} lit pixels");
}

/// Every boundary of a fill opens exactly one pair of error scopes, the
/// last one's submission inside it; one submission and one completion
/// registered.
#[test]
fn one_scope_pair_per_boundary() {
    let dir = decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[text("f", 256, Value::Ref(10), [0, 100, 1_900, 256])],
    );
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "ABCDEFGHIJKLMNOPQRST")));
    store.claim().unwrap();
    let mut boundaries = 0;
    loop {
        let before = r.decision_scope_pairs.get();
        let step = r.write_decision_labels(&mut store);
        boundaries += 1;
        assert_eq!(
            r.decision_scope_pairs.get() - before,
            1,
            "boundary {boundaries}"
        );
        if step == FillStep::Complete {
            break;
        }
    }
    assert!(boundaries > 1);
    let count = |step| store.steps.iter().filter(|s| **s == step).count();
    assert_eq!(count(LabelStep::Submitted), 1);
    assert_eq!(count(LabelStep::Watched), 1);
    // A complete fill opens no more.
    let before = r.decision_scope_pairs.get();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    assert_eq!(r.decision_scope_pairs.get(), before);
}

/// Nothing is published before the completion was seen: a fill whose
/// completion has not fired stays pending through polls, then publishes.
#[test]
fn nothing_published_before_its_completion() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    finish(&r);
    // The completion held back: the test's own flag stands for it.
    let held = Arc::new(AtomicBool::new(false));
    store.progress_mut().unwrap().done = Some(Arc::clone(&held));
    for _ in 0..3 {
        assert_eq!(
            r.settle_decision_labels(&mut store).unwrap(),
            Settled::Pending
        );
    }
    assert!(store.content(slot, 1).is_none());
    assert!(matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Filling { .. })
    ));
    held.store(true, Ordering::Release);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 1
        }
    );
}

/// A scope that reported an error refuses the fill: nothing published,
/// the slot freed, a refusal of the kind.
#[test]
fn scope_error_refuses_the_fill() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    r.decision_in_scope = Some(|gpu| {
        // Mappable for reading and writing at once: a validation error.
        let _ = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::MAP_WRITE,
            mapped_at_creation: false,
        });
    });
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    assert!(store.progress().unwrap().scope_error.is_some());
    finish(&r);
    let e = r.settle_decision_labels(&mut store).unwrap_err();
    assert!(
        matches!(e, RenderError::Atlas(crate::UploadError::Device(_))),
        "{e:?}"
    );
    assert!(e.is_refusal());
    assert!(!store.steps.contains(&LabelStep::Published));
    assert!(matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Free)
    ));
    assert!(store.content(slot, 1).is_none());
}

/// A bitmap-only decision skin: the installation committed with the
/// title and the artist in a bitmap font.
fn bitmap_installed() -> (tempfile::TempDir, Renderer, Arc<SkinBundle>) {
    let dir = decision_folder(
        &bitmap_files(),
        &[bitmap()],
        &[
            text("g", 8, Value::Ref(10), TITLE),
            text("g", 8, Value::Ref(16), ARTIST),
        ],
    );
    let (r, b) = installed(&dir);
    (dir, r, b)
}

/// The bitmap-only path with the device destroyed before its boundary:
/// the device's failure, not a refusal, and nothing published.
#[test]
fn bitmap_only_prep_with_device_lost_publishes_nothing() {
    let (_dir, mut r, b) = bitmap_installed();
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    r.gpu.device().destroy();
    finish(&r);
    let e = r.settle_decision_labels(&mut store).unwrap_err();
    assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    assert!(!e.is_refusal());
    assert!(!lock(store.book()).healthy());
    assert!(store.content(slot, 1).is_none());
}

/// The bitmap-only path with the target changed before its boundary:
/// stale, the slot freed.
#[test]
fn bitmap_only_prep_on_a_changed_target_is_stale() {
    let (_dir, mut r, b) = bitmap_installed();
    let mut store = store_for();
    store.offer(stage(&b, job(&r, 1, "a", "AB")));
    let slot = store.claim().unwrap();
    lock(store.book()).invalidate([1_280, 720]);
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Complete);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Stale
    );
    assert!(matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Free)
    ));
    assert!(store.content(slot, 1).is_none());
}

/// A wake that reports on a channel.
fn waker() -> (Arc<dyn Fn() + Send + Sync>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    (
        Arc::new(move || {
            let _ = tx.send(());
        }),
        rx,
    )
}

/// An output taken before a newer job was posted is never claimed, offered
/// after the post or before it; the newer one is. A waiting output is
/// dropped by the post itself, before any claim.
#[test]
fn output_taken_before_a_newer_post_never_publishes() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let (wake, rx) = waker();
    let worker = LabelWorker::for_bundle(&b, Some(wake)).unwrap();
    let mut store = store_for();
    // Taken, then B posted, then offered.
    store.post(&worker, job(&r, 1, "a", "AB"), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    let a = worker.take().unwrap().unwrap();
    assert!(a.is_current());
    store.post(&worker, job(&r, 2, "b", "BA"), &mut || r.watch_completion());
    assert!(!a.is_current());
    store.offer(a);
    assert_eq!(store.claim(), None);
    assert_eq!(store.waiting(), None);
    // Taken and offered, then C posted: the post drops it.
    rx.recv_timeout(watchdog()).unwrap();
    let bo = worker.take().unwrap().unwrap();
    store.offer(bo);
    store.post(&worker, job(&r, 3, "c", "CA"), &mut || r.watch_completion());
    assert_eq!(store.waiting(), None, "the post dropped the older output");
    rx.recv_timeout(watchdog()).unwrap();
    store.offer(worker.take().unwrap().unwrap());
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot,
            generation: 3
        }
    );
    assert!(!lock(store.book()).holds(&key("a")));
    assert!(!lock(store.book()).holds(&key("b")));
}

/// A worker held between its staging and its last check while a newer
/// job is posted: the superseded output is never stored, so none is alive
/// while the newer job's staging is.
#[test]
fn superseded_output_never_overlaps_the_next_staging() {
    let dir = truetype_skin();
    let (r, b) = installed(&dir);
    let (at_tx, at_rx) = mpsc::channel::<Gen>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go = Mutex::new(go_rx);
    let hook: Arc<dyn Fn(Gen) + Send + Sync> = Arc::new(move |g| {
        let _ = at_tx.send(g);
        let _ = go.lock().unwrap_or_else(PoisonError::into_inner).recv();
    });
    let (wake, rx) = waker();
    let worker = LabelWorker::spawn_hooked(
        Arc::clone(b.skin()),
        b.text_producer().cloned(),
        Some(wake),
        WorkerHooks {
            before_store: Some(hook),
            ..WorkerHooks::default()
        },
    )
    .unwrap();
    let mut store = store_for();
    store.post(&worker, job(&r, 1, "a", "AB"), &mut || r.watch_completion());
    assert_eq!(at_rx.recv_timeout(watchdog()).unwrap(), 1);
    store.post(&worker, job(&r, 2, "b", "BA"), &mut || r.watch_completion());
    go_tx.send(()).unwrap();
    // The newer job staged and held: no older output beside it.
    assert_eq!(at_rx.recv_timeout(watchdog()).unwrap(), 2);
    assert_eq!(worker.output_generation(), None);
    go_tx.send(()).unwrap();
    rx.recv_timeout(watchdog()).unwrap();
    assert_eq!(worker.take().unwrap().unwrap().generation(), 2);
}

/// A producer whose first layout reports that it started and waits until
/// released: a worker held inside a line.
#[derive(Debug)]
struct Gated {
    inner: Arc<dyn AttemptText>,
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
}

impl AttemptText for Gated {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        let entered = self
            .entered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(tx) = entered {
            let _ = tx.send(());
            let release = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(rx) = release {
                let _ = rx.recv();
            }
        }
        self.inner.layout(req, em_px, bounds)
    }

    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
        self.inner.raster(layout, out)
    }

    fn resident(&self) -> TextResidency {
        self.inner.resident()
    }
}

/// Dropping a worker held inside a line returns without waiting for the
/// line, and its thread is joined once the line ends.
#[test]
fn dropping_a_worker_inside_a_line_returns_at_once() {
    let dir = truetype_skin();
    let (r, b) = installed(&dir);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let producer = Arc::new(Gated {
        inner: Arc::clone(b.text_producer().unwrap()),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let worker = LabelWorker::spawn(Arc::clone(b.skin()), Some(producer), None).unwrap();
    let reaped = worker.reaped();
    let mut store = store_for();
    store.post(&worker, job(&r, 1, "a", "AB"), &mut || r.watch_completion());
    entered_rx.recv_timeout(watchdog()).unwrap();
    let (dropped_tx, dropped_rx) = mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(worker);
        let _ = dropped_tx.send(());
    });
    let dropped = dropped_rx.recv_timeout(watchdog());
    let _ = release_tx.send(());
    dropper.join().unwrap();
    assert!(dropped.is_ok(), "the drop waited for the line");
    let (m, cv) = &*reaped;
    let joined = cv
        .wait_timeout_while(m.lock().unwrap(), watchdog(), |j| !*j)
        .unwrap();
    assert!(*joined.0, "the thread was joined");
}

/// A title long enough at em 256 to take more than one boundary.
const LONG: &str = "ABCDEFGHIJKLMNOPQRST";

/// A decision skin whose title is in a Latin face at em 256.
fn long_skin() -> tempfile::TempDir {
    decision_folder(
        &[("ttf.ttf", font_support::build(&font_support::latin("Skin")))],
        &[ttf()],
        &[text("f", 256, Value::Ref(10), [0, 100, 1_900, 256])],
    )
}

/// A producer that notes whether any slot of `book` was filling while it
/// laid out a line.
#[derive(Debug)]
struct Observing {
    inner: Arc<dyn AttemptText>,
    book: Arc<Mutex<LabelBook>>,
    filling_seen: AtomicBool,
}

impl AttemptText for Observing {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        let book = lock(&self.book);
        if (0..LABEL_SLOTS).any(|i| matches!(book.state(i), Some(LabelState::Filling { .. }))) {
            self.filling_seen.store(true, Ordering::Release);
        }
        drop(book);
        self.inner.layout(req, em_px, bounds)
    }

    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
        self.inner.raster(layout, out)
    }

    fn resident(&self) -> TextResidency {
        self.inner.resident()
    }
}

/// A claims a slot and writes part of its rows; B is posted. The post
/// disposes of A before B's staging can exist: A's remaining levels are
/// freed and its slot cancelled, and B's lines are laid out with no slot
/// filling. A never publishes; B does, in the other slot.
#[test]
fn post_after_claim_disposes_the_older_fill() {
    let dir = long_skin();
    let (mut r, b) = installed(&dir);
    let book = LabelBook::shared(TARGET);
    let producer = Arc::new(Observing {
        inner: Arc::clone(b.text_producer().unwrap()),
        book: Arc::clone(&book),
        filling_seen: AtomicBool::new(false),
    });
    let (wake, rx) = waker();
    let worker = LabelWorker::spawn(
        Arc::clone(b.skin()),
        Some(Arc::clone(&producer) as Arc<dyn AttemptText>),
        Some(wake),
    )
    .unwrap();
    let mut store = LabelStore::new(book);
    store.post(&worker, job(&r, 1, "a", LONG), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    store.offer(worker.take().unwrap().unwrap());
    let a = store.claim().unwrap();
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Wrote);
    assert!(store.memory(None).staged > 0);
    store.post(&worker, job(&r, 2, "b", LONG), &mut || r.watch_completion());
    assert_eq!(store.filling(), None, "A's fill disposed in the post");
    assert_eq!(store.memory(None).staged, 0, "A's levels freed");
    assert!(matches!(
        lock(store.book()).state(a),
        Some(LabelState::Cooling {
            from: CoolFrom::Cancelled,
            ..
        })
    ));
    rx.recv_timeout(watchdog()).unwrap();
    assert!(
        !producer.filling_seen.load(Ordering::Acquire),
        "B was laid out beside A's fill"
    );
    store.offer(worker.take().unwrap().unwrap());
    let other = store.claim().unwrap();
    assert_ne!(other, a);
    fill(&mut r, &mut store);
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Published {
            slot: other,
            generation: 2
        }
    );
    assert!(matches!(
        lock(store.book()).state(a),
        Some(LabelState::Free)
    ));
    assert!(store.content(a, 1).is_none());
    assert!(!lock(store.book()).holds(&key("a")));
}

/// A fill whose writes completed, then a newer post that never reached
/// the store: settling it publishes nothing, since publication is fenced
/// by the newest post.
#[test]
fn older_post_never_publishes_its_fill() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let (wake, rx) = waker();
    let worker = LabelWorker::for_bundle(&b, Some(wake)).unwrap();
    let mut store = store_for();
    store.post(&worker, job(&r, 1, "a", "AB"), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    store.offer(worker.take().unwrap().unwrap());
    let slot = store.claim().unwrap();
    fill(&mut r, &mut store);
    assert!(store.steps.contains(&LabelStep::Submitted));
    worker.post_unreconciled(job(&r, 2, "b", "BA"));
    finish(&r);
    assert_eq!(
        r.settle_decision_labels(&mut store).unwrap(),
        Settled::Pending
    );
    assert!(!store.steps.contains(&LabelStep::Published));
    assert!(!matches!(
        lock(store.book()).state(slot),
        Some(LabelState::Ready { .. })
    ));
    assert!(store.content(slot, 1).is_none());
    assert_eq!(store.filling(), None);
}

/// A producer that refuses every line it rasterizes.
#[derive(Debug)]
struct Refusing(Arc<dyn AttemptText>);

impl AttemptText for Refusing {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        self.0.layout(req, em_px, bounds)
    }

    fn raster(&self, _: &LineLayout, _: &mut [u8]) -> Result<(), SkinError> {
        Err(SkinError {
            reason: "refused".to_owned(),
        })
    }

    fn resident(&self) -> TextResidency {
        self.0.resident()
    }
}

/// A refusal queued by the worker is never taken after a cancel or a
/// newer post: the request that returned to a key already held, or moved
/// on, never sees an older job's error.
#[test]
fn obsolete_refusal_never_taken_after_a_newer_post() {
    let dir = truetype_skin();
    let (r, b) = installed(&dir);
    let (wake, rx) = waker();
    let producer = Arc::new(Refusing(Arc::clone(b.text_producer().unwrap())));
    let worker = LabelWorker::spawn(Arc::clone(b.skin()), Some(producer), Some(wake)).unwrap();
    let mut store = store_for();
    store.post(&worker, job(&r, 1, "a", "AB"), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    assert_eq!(worker.output_generation(), Some(1), "A's refusal queued");
    store.cancel(&worker, &mut || r.watch_completion());
    assert!(
        worker.take().is_none(),
        "A's refusal taken after the cancel"
    );
    store.post(&worker, job(&r, 2, "b", "BA"), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    assert_eq!(worker.output_generation(), Some(2));
    store.post(&worker, job(&r, 3, "c", "CA"), &mut || r.watch_completion());
    if let Some(out) = worker.take() {
        assert!(
            matches!(out, Err((3, _))),
            "{:?}",
            out.map(|s| s.generation())
        );
    } else {
        rx.recv_timeout(watchdog()).unwrap();
        assert!(matches!(worker.take(), Some(Err((3, _)))));
    }
}

/// Generation zero cancelled, after its output was taken and while its
/// job is in a line: the taken output is stale and never claimed, and
/// the job's output is never stored. No generation aliases a cancel.
#[test]
fn generation_zero_cancel_supersedes() {
    let dir = truetype_skin();
    let (r, b) = installed(&dir);
    // After the take.
    let (wake, rx) = waker();
    let worker = LabelWorker::for_bundle(&b, Some(wake)).unwrap();
    let mut store = store_for();
    store.post(&worker, job(&r, 0, "a", "AB"), &mut || r.watch_completion());
    rx.recv_timeout(watchdog()).unwrap();
    let a = worker.take().unwrap().unwrap();
    assert!(a.is_current());
    store.cancel(&worker, &mut || r.watch_completion());
    assert!(!a.is_current());
    store.offer(a);
    assert_eq!(store.claim(), None);
    // During the work.
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let producer = Arc::new(Gated {
        inner: Arc::clone(b.text_producer().unwrap()),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let (done_tx, done_rx) = mpsc::channel();
    let done = Mutex::new(done_tx);
    let after: Arc<dyn Fn(Gen, bool) + Send + Sync> = Arc::new(move |g, stored| {
        let _ = done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send((g, stored));
    });
    let worker = LabelWorker::spawn_hooked(
        Arc::clone(b.skin()),
        Some(producer),
        None,
        WorkerHooks {
            after_job: Some(after),
            ..WorkerHooks::default()
        },
    )
    .unwrap();
    store.post(&worker, job(&r, 0, "a", "AB"), &mut || r.watch_completion());
    entered_rx.recv_timeout(watchdog()).unwrap();
    store.cancel(&worker, &mut || r.watch_completion());
    release_tx.send(()).unwrap();
    assert_eq!(done_rx.recv_timeout(watchdog()).unwrap(), (0, false));
    assert!(worker.take().is_none());
}

/// The renderer with `dir`'s decision skin committed, at `target`.
fn installed_at(dir: &tempfile::TempDir, target: [u32; 2]) -> (Renderer, Arc<SkinBundle>) {
    let mut r = offscreen(target[0], target[1]);
    let b = decision_bundle_in(&r, dir.path(), &default_font());
    let token = r.install(ScreenKind::Decision, &b, 1).unwrap();
    r.commit(token).unwrap();
    (r, b)
}

/// Pinned at T, built again and restored at U, then back at T: the pin's
/// key names U, the target its lines were rasterized at (slot, generation
/// and lifetime kept), so nothing holds T's key and its retire never makes
/// it ready for T.
#[test]
fn restore_at_another_target_keys_that_target() {
    const U: [u32; 2] = [1_280, 720];
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    let first = job(&r, 1, "a", "ABC");
    let slot = publish(&mut r, &b, &mut store, first);
    lock(store.book())
        .pin(std::path::Path::new("a"), 1, 7)
        .unwrap();
    drop(r);
    let (mut r2, b2) = installed_at(&dir, U);
    store.reset();
    {
        let mut book = lock(store.book());
        book.reset();
        book.invalidate(U);
    }
    let strings = store.restore_strings(slot, 1).unwrap().clone();
    let plan = r2.decision_rows().unwrap().unwrap();
    assert_eq!(plan.target(), U);
    let staged = stage_bundle_labels(
        &b2,
        LabelJob::new(1, PathBuf::from("a"), 1, strings, Arc::new(plan)),
    )
    .unwrap()
    .unwrap();
    assert!(
        r2.restore_decision_labels(&mut store, slot, staged)
            .unwrap()
    );
    let mut book = lock(store.book());
    assert!(
        matches!(
            book.state(slot),
            Some(LabelState::Pinned {
                generation: 1,
                lifetime: 7,
                restoring: false,
                key,
                ..
            }) if key.target == U
        ),
        "{:?}",
        book.state(slot)
    );
    book.invalidate(TARGET);
    assert!(!book.holds(&key("a")), "T's labels are prepared afresh");
    book.end(7);
    book.retire_ended(&mut || Arc::new(AtomicBool::new(true)));
    book.cool();
    assert!(matches!(book.state(slot), Some(LabelState::Free)));
    assert_eq!(book.pin(std::path::Path::new("a"), 1, 8), None);
}

/// Pins `chart` at generation 1 under lifetime 7, ends it and retires it
/// with its last frame's completion already seen (the test's own flag,
/// set).
fn retire_seen(store: &LabelStore, chart: &str) {
    let mut book = lock(store.book());
    book.pin(std::path::Path::new(chart), 1, 7).unwrap();
    book.end(7);
    book.retire_ended(&mut || Arc::new(AtomicBool::new(true)));
}

/// The device lost while a retired slot cools and nothing fills: the
/// settle reports the loss, the book is unhealthy, and the slot is not
/// made ready.
#[test]
fn device_lost_while_cooling_with_no_fill_promotes_nothing() {
    let dir = truetype_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    let first = job(&r, 1, "a", "AB");
    let slot = publish(&mut r, &b, &mut store, first);
    retire_seen(&store, "a");
    r.gpu.device().destroy();
    finish(&r);
    let e = r.settle_decision_labels(&mut store).unwrap_err();
    assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    assert!(!e.is_refusal());
    let book = lock(store.book());
    assert!(!book.healthy());
    assert!(
        matches!(book.state(slot), Some(LabelState::Cooling { .. })),
        "{:?}",
        book.state(slot)
    );
}

/// The device lost while a retired slot cools and another fill has
/// written only part of its rows: the loss is reported, the book is
/// unhealthy, the retired slot is not made ready and the fill is dropped.
#[test]
fn device_lost_during_an_incomplete_fill_promotes_nothing() {
    let dir = long_skin();
    let (mut r, b) = installed(&dir);
    let mut store = store_for();
    let first = job(&r, 1, "a", LONG);
    let pinned = publish(&mut r, &b, &mut store, first);
    retire_seen(&store, "a");
    store.offer(stage(&b, job(&r, 2, "b", LONG)));
    let slot = store.claim().unwrap();
    assert_ne!(slot, pinned);
    assert_eq!(r.write_decision_labels(&mut store), FillStep::Wrote);
    r.gpu.device().destroy();
    finish(&r);
    let e = r.settle_decision_labels(&mut store).unwrap_err();
    assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    let book = lock(store.book());
    assert!(!book.healthy());
    assert!(
        matches!(book.state(pinned), Some(LabelState::Cooling { .. })),
        "{:?}",
        book.state(pinned)
    );
    assert!(!matches!(book.state(slot), Some(LabelState::Ready { .. })));
    drop(book);
    assert_eq!(store.filling(), None);
}
