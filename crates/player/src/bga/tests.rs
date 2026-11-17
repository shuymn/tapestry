//! The BGA pictures' worker (design.md "Rendering and Skins", BGA; "Load
//! Pipeline", Freshness): one owned job of its own kind holding one
//! foreground permit, cancelled through the choice's scope, whose
//! publication of a generation left is never current; and a retry that
//! reuses the array only while every id resolves to the same unchanged
//! file.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use game::coord::{Caps, Class, Coordinator, JobKey, JobKind, Owner, Scope, Spawner};
use skin_model::bga::{BgaEntry, BgaTable};

use super::{Answer, BATCH, BATCH_LIMIT, BgaMsg, BgaWorker, HELD, HOLD, Poll, Want};
use crate::render_side::tests::Installers;
use crate::render_side::{BgaPublisher, RenderLink, RenderMsg};

const CAPS: Caps = Caps {
    foreground: 4,
    speculative: 1,
};

/// Lets the job go when dropped, a failing test's included.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
    }
}

/// A `w`×`h` PNG of `rgb` at `dir/name`, whatever its extension (the
/// format is read from the content).
pub(crate) fn png(dir: &Path, name: &str, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A render thread of recording screens, with a screen built.
fn render(screens: &Installers) -> RenderLink {
    let link = screens.spawn_bare();
    let chart = bms_model::ChartBuilder::new(120_000).build().unwrap();
    link.load(Arc::new(chart), None).unwrap();
    link
}

fn worker(render: &RenderLink) -> BgaWorker {
    BgaWorker::new(render.bga_generation(), render.bga_publisher())
}

fn want(dir: &Path, names: &[&str]) -> Want {
    Want {
        dir: dir.to_path_buf(),
        names: names.iter().map(|&n| n.to_owned()).collect(),
    }
}

/// Steps `w` on `coord` until its pictures are published, within 10 s.
fn ready(w: &BgaWorker, coord: &Coordinator) -> Arc<BgaTable> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        w.step(Some((coord, Spawner::SYSTEM)), &|| false);
        if let Poll::Ready(t) = w.poll() {
            return t;
        }
        assert!(Instant::now() < end, "{w:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits until `f`, within 10 s.
fn until(f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The picture entry's drawn size, `None` for anything else.
fn native(e: BgaEntry) -> Option<[u32; 2]> {
    match e {
        BgaEntry::Picture(c) => Some(c.native),
        _ => None,
    }
}

/// A chart without BGA pictures reads nothing: its empty table is sent at
/// the step, with no job begun and no permit taken, so the load beside it
/// keeps every foreground permit; the render thread's answer is taken by a
/// later step.
#[test]
fn a_chart_without_bga_begins_no_job() {
    let _serial = crate::imports::tests::serial();
    let d = tempfile::tempdir().unwrap();
    let screens = Installers::default();
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    w.renew();
    w.attempt(want(d.path(), &[]));
    w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
    assert_eq!(coord.held(Class::Foreground), 0, "no permit taken");
    let t = ready(&w, &coord);
    assert!(t.entries().is_empty());
    assert_eq!(screens.bga.lock().unwrap().published.len(), 1);
    assert_eq!(w.counts().0, 0, "no job begun");
}

/// The pictures are one job of their own kind, on one foreground permit,
/// published only once it ends; a chart left while it runs cancels it
/// through the choice's scope, and nothing of it is published: a step of a
/// generation left is refused by the render thread.
#[test]
fn bga_images_are_one_owned_job_published_before_ready() {
    let _serial = crate::imports::tests::serial();
    let _let_go = LetGo;
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "a.png", [640, 480], [200, 0, 0]);
    let screens = Installers::default();
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    w.renew();
    w.attempt(want(d.path(), &["a.png"]));
    HOLD.store(true, Ordering::Release);
    w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
    until(|| HELD.load(Ordering::Acquire));
    // Its own kind, keyed by the folder: no second job of it begins.
    let again = coord.spawn_owned(
        Spawner::SYSTEM,
        JobKey {
            kind: JobKind::BgaImages,
            path: d.path().to_path_buf(),
            speed: bms_model::Speed::NORMAL,
        },
        (Class::Foreground, false, Owner::Foreground),
        coord.subscriber(Scope::Choice),
        |_| {},
        |_| (),
    );
    assert!(again.is_err(), "the key is the running job's");
    assert_eq!(coord.held(Class::Foreground), 1, "one foreground permit");
    w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
    assert_eq!(w.poll(), Poll::Waiting, "not published while it runs");
    assert!(screens.bga.lock().unwrap().published.is_empty());
    HOLD.store(false, Ordering::Release);
    let t = ready(&w, &coord);
    assert!(native(t.entries()[0]).is_some());
    assert_eq!(screens.bga.lock().unwrap().published.len(), 1);
    assert_eq!(w.counts().0, 1, "one job");

    // Left while it runs: cancelled through the choice's scope, nothing
    // published, and its generation's late steps refused.
    w.committed();
    w.renew();
    w.attempt(want(d.path(), &["a.png", "b.png"]));
    HOLD.store(true, Ordering::Release);
    w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
    until(|| HELD.load(Ordering::Acquire));
    let left = w.generation();
    coord.advance(Scope::Choice);
    w.forget();
    HOLD.store(false, Ordering::Release);
    until(|| {
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
        w.idle()
    });
    assert_eq!(w.poll(), Poll::Waiting, "nothing asked for");
    let seen = screens.bga.lock().unwrap();
    assert_eq!(seen.published.len(), 1, "nothing of the job left");
    assert!(seen.current().is_none(), "the array dropped");
    drop(seen);
    let answer = Answer::new(Duration::from_secs(10));
    assert!(link.bga_publisher().send(
        left,
        BgaMsg::Publish {
            key: None,
            entries: vec![BgaEntry::Absent],
            reply: Arc::clone(&answer),
        },
    ));
    assert_eq!(
        answer.wait().map(|t| t.err()),
        Some(Some(render::BgaFault::Stale))
    );
    assert_eq!(screens.bga.lock().unwrap().published.len(), 1);
}

/// A retry reuses the array while every id resolves to the same unchanged
/// file, reading nothing; a changed file, a missing name now present and a
/// higher-priority candidate added each read and pack the whole set again.
#[test]
fn bga_set_is_reused_only_on_an_unchanged_key() {
    let _serial = crate::imports::tests::serial();
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "a.png", [640, 480], [200, 0, 0]);
    png(d.path(), "scene.bmp", [300, 300], [0, 200, 0]);
    let screens = Installers::default();
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    let names = ["a.png", "scene.bmp", "late.png"];
    let attempt = |w: &BgaWorker| {
        w.committed();
        w.attempt(want(d.path(), &names));
    };
    w.renew();
    w.attempt(want(d.path(), &names));
    let first = ready(&w, &coord);
    let entries = |t: &BgaTable| t.entries().iter().map(|e| native(*e)).collect::<Vec<_>>();
    assert_eq!(entries(&first), [Some([640, 480]), Some([300, 300]), None]);
    assert_eq!(w.counts(), (1, 2));
    let begins = || screens.bga.lock().unwrap().begins.len();
    assert_eq!(begins(), 1);

    // Unchanged: the array reused, nothing decoded.
    attempt(&w);
    let reused = ready(&w, &coord);
    assert!(Arc::ptr_eq(&first, &reused));
    assert_eq!(w.counts(), (2, 2));
    assert_eq!(begins(), 1);

    // One file rewritten: the whole set again, the new picture shown.
    std::thread::sleep(Duration::from_millis(20));
    png(d.path(), "a.png", [320, 240], [0, 0, 200]);
    attempt(&w);
    let changed = ready(&w, &coord);
    assert_eq!(
        entries(&changed),
        [Some([320, 240]), Some([300, 300]), None]
    );
    assert_eq!(w.counts(), (3, 4));
    assert_eq!(begins(), 2);

    // A missing name now present: read on the retry.
    png(d.path(), "late.png", [400, 400], [9, 9, 9]);
    attempt(&w);
    let late = ready(&w, &coord);
    assert_eq!(entries(&late)[2], Some([400, 400]));
    assert_eq!(w.counts(), (4, 7));

    // A JPEG beside the BMP: the BGA resolver picks it on the retry.
    png(d.path(), "scene.jpg", [500, 260], [1, 2, 3]);
    attempt(&w);
    let jpeg = ready(&w, &coord);
    assert_eq!(entries(&jpeg)[1], Some([500, 260]));
    assert_eq!(w.counts(), (5, 10));
    assert_eq!(begins(), 4);
}

/// Puts the batch limit back when dropped, a failing test's included.
struct BatchBack;

impl Drop for BatchBack {
    fn drop(&mut self) {
        BATCH_LIMIT.store(0, Ordering::SeqCst);
    }
}

/// The cells go to the render thread in batches within the cell term: all
/// of a small set in one step, and with the limit at two cells' bytes, a
/// batch handed over before the picture whose cell would overflow it is
/// decoded, so no batch holds more than the limit.
#[test]
fn bga_cells_go_in_batches_within_the_cell_term() {
    let _serial = crate::imports::tests::serial();
    let _back = BatchBack;
    let d = tempfile::tempdir().unwrap();
    let names = ["1.png", "2.png", "3.png", "4.png", "5.png"];
    for (i, n) in (0_u8..).zip(names) {
        png(d.path(), n, [300, 300], [i * 40, 100, 0]);
    }
    // A 300×300 picture's cell: its gutter on every side.
    let cell = 302 * 302 * 4;
    for (limit, batches) in [(0, vec![5]), (2 * cell, vec![2, 2, 1])] {
        BATCH_LIMIT.store(u64::try_from(limit).unwrap(), Ordering::SeqCst);
        let screens = Installers::default();
        let link = render(&screens);
        let coord = Coordinator::new(CAPS);
        let w = worker(&link);
        w.renew();
        w.attempt(want(d.path(), &names));
        let t = ready(&w, &coord);
        assert!(
            t.entries().iter().all(|e| native(*e) == Some([300, 300])),
            "{limit}"
        );
        let seen = screens.bga.lock().unwrap();
        let counts: Vec<usize> = seen.batches.iter().map(|&(n, _)| n).collect();
        assert_eq!(counts, batches, "{limit}");
        let bound = if limit == 0 {
            usize::try_from(BATCH).unwrap()
        } else {
            limit
        };
        assert!(
            seen.batches
                .iter()
                .all(|&(n, bytes)| bytes == n * cell && bytes <= bound),
            "{:?}",
            seen.batches
        );
        assert_eq!(seen.cells, 5);
    }
}

/// A batch is taken in hand by the render thread and answered only once
/// its writes end at a later frame boundary (design.md "Memory Budget"):
/// with each batch kept three boundaries before it is written, every batch
/// takes four, and the pictures are published once every batch is
/// answered.
#[test]
fn a_batch_is_answered_at_the_render_thread_s_boundaries() {
    let _serial = crate::imports::tests::serial();
    let _back = BatchBack;
    let d = tempfile::tempdir().unwrap();
    let names = ["1.png", "2.png", "3.png"];
    for (i, n) in (0_u8..).zip(names) {
        png(d.path(), n, [300, 300], [i * 40, 100, 0]);
    }
    BATCH_LIMIT.store(2 * 302 * 302 * 4, Ordering::SeqCst);
    let screens = Installers::default();
    screens.bga.lock().unwrap().boundaries_per_batch = 3;
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    w.renew();
    w.attempt(want(d.path(), &names));
    let t = ready(&w, &coord);
    assert!(t.entries().iter().all(|e| native(*e) == Some([300, 300])));
    let seen = screens.bga.lock().unwrap();
    assert_eq!(seen.batches.len(), 2);
    assert_eq!(seen.boundaries, 8, "four boundaries a batch");
    assert_eq!(seen.published.len(), 1);
}

/// A batch in hand whose generation is left is dropped at the next frame
/// boundary, unwritten, and its job answered stale: nothing of it is
/// published.
#[test]
fn a_batch_of_a_generation_left_is_dropped_at_the_boundary() {
    let _serial = crate::imports::tests::serial();
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "a.png", [300, 300], [200, 0, 0]);
    let screens = Installers::default();
    screens.bga.lock().unwrap().boundaries_per_batch = u32::MAX;
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    w.renew();
    w.attempt(want(d.path(), &["a.png"]));
    w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
    until(|| screens.bga.lock().unwrap().boundaries > 0);
    coord.advance(Scope::Choice);
    w.forget();
    until(|| {
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
        w.idle()
    });
    let seen = screens.bga.lock().unwrap();
    assert_eq!(seen.cancelled, 1, "dropped unwritten");
    assert!(seen.batches.is_empty());
    assert!(seen.published.is_empty(), "nothing of it published");
}

/// The absent table's answer as the render thread gives it: its message
/// taken from the link, a worker of `bound` having sent it at the step.
fn absent_answer(bound: Duration) -> (BgaWorker, Arc<Answer>) {
    let (link, rx) = BgaPublisher::channel(bound);
    let w = BgaWorker::new(Arc::new(AtomicU64::new(0)), link);
    w.renew();
    w.attempt(want(Path::new("."), &[]));
    w.step(None, &|| false);
    let Ok(RenderMsg::Bga {
        msg: BgaMsg::Publish { reply, .. },
        ..
    }) = rx.try_recv()
    else {
        panic!("the empty table is sent at the step");
    };
    (w, reply)
}

/// An empty table, as the render thread publishes one.
fn empty() -> Result<Arc<BgaTable>, render::BgaFault> {
    Ok(Arc::new(BgaTable::new(Vec::new())))
}

/// The absent table's answer counts when the render thread gave it within
/// the link's bound, however late a step takes it (other loading work may
/// delay the step); past the bound the render thread publishes nothing and
/// gives no answer, and with none given the attempt is refused.
#[test]
fn an_absent_table_answered_after_the_bound_is_unanswered() {
    const BOUND: Duration = Duration::from_millis(200);
    // `Some(late)`: answered before or after the bound; `None`: never.
    // Returns the poll after the bound and whether the table was published.
    let poll_after_the_bound = |late: Option<bool>| -> (Poll, bool) {
        let (w, reply) = absent_answer(BOUND);
        let mut published = false;
        if late == Some(false) {
            reply.give(|| {
                published = true;
                empty()
            });
        }
        std::thread::sleep(2 * BOUND);
        if late == Some(true) {
            reply.give(|| {
                published = true;
                empty()
            });
        }
        (w.poll(), published)
    };
    let (poll, published) = poll_after_the_bound(Some(false));
    assert!(
        matches!(poll, Poll::Ready(_)) && published,
        "an answer given in time, taken after the bound: {poll:?}"
    );
    assert_eq!(poll_after_the_bound(None), (Poll::Expired, false));
    assert_eq!(
        poll_after_the_bound(Some(true)),
        (Poll::Expired, false),
        "an answer given after the bound publishes nothing"
    );
}

/// An answer the render thread began to give within the bound counts
/// however long its publication takes: a step looking after the bound,
/// while the render thread is still between deciding it is in time and
/// handing its table over, takes the table, never "not published in
/// time". The render thread is paused there, past the bound, here.
#[test]
fn an_answer_begun_in_time_is_taken_after_the_bound() {
    const BOUND: Duration = Duration::from_millis(200);
    let (w, reply) = absent_answer(BOUND);
    let inside = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let giving = {
        let inside = Arc::clone(&inside);
        std::thread::spawn(move || {
            reply.give(|| {
                inside.store(true, Ordering::SeqCst);
                std::thread::sleep(3 * BOUND);
                empty()
            });
        })
    };
    until(|| inside.load(Ordering::SeqCst));
    std::thread::sleep(2 * BOUND);
    let poll = w.poll();
    giving.join().unwrap();
    assert!(matches!(poll, Poll::Ready(_)), "{poll:?}");
}

/// A picture whose header read the allocator refuses, before any picture
/// is decoded, leaves every picture of the attempt absent, the one beside
/// it that reads whole included, under the coordinator and on a direct
/// launch alike: no array is begun. With nothing refused both are read.
#[cfg(feature = "dev-tools")]
#[test]
fn a_refused_header_read_leaves_every_picture_absent() {
    let _serial = crate::imports::tests::serial();
    let d = tempfile::tempdir().unwrap();
    // The refused file runs on to a length no other allocation here is
    // likely to have: its header read reserves the whole file (under the
    // header's cap), the first allocation of that size.
    png(d.path(), "odd.png", [64, 48], [200, 0, 0]);
    let mut bytes = std::fs::read(d.path().join("odd.png")).unwrap();
    bytes.resize(7_919, 0);
    std::fs::write(d.path().join("odd.png"), &bytes).unwrap();
    png(d.path(), "b.png", [640, 480], [0, 200, 0]);
    for coordinated in [true, false] {
        for refused in [false, true] {
            let what = format!("coordinated {coordinated}, refused {refused}");
            let screens = Installers::default();
            let link = render(&screens);
            let coord = Coordinator::new(CAPS);
            let w = worker(&link);
            w.renew();
            w.attempt(want(d.path(), &["odd.png", "b.png"]));
            if refused {
                crate::alloc_refusal::arm(bytes.len());
            }
            let t = if coordinated {
                ready(&w, &coord)
            } else {
                w.step(None, &|| false);
                match w.poll() {
                    Poll::Ready(t) => t,
                    other => panic!("{what}: {other:?}"),
                }
            };
            assert_eq!(crate::alloc_refusal::disarm(), refused, "{what}: refusal");
            let drawn: Vec<_> = t.entries().iter().map(|&e| native(e)).collect();
            let begun = screens.bga.lock().unwrap().begins.len();
            if refused {
                assert_eq!(drawn, [None, None], "{what}");
                assert_eq!(begun, 0, "{what}: no array");
            } else {
                assert_eq!(drawn, [Some([256, 256]), Some([640, 480])], "{what}");
                assert_eq!(begun, 1, "{what}");
            }
        }
    }
}

/// A 64×48 JPEG of one colour, progressive, its three components at full
/// resolution (4:4:4: `SOF2`, each sampled 1×1), made outside the
/// repository by an encoder that writes progressive files (Pillow,
/// quality 75), as no encoder here does.
const PROGRESSIVE_444: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08,
    0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0a, 0x0c, 0x14, 0x0d, 0x0c, 0x0b, 0x0b, 0x0c, 0x19, 0x12,
    0x13, 0x0f, 0x14, 0x1d, 0x1a, 0x1f, 0x1e, 0x1d, 0x1a, 0x1c, 0x1c, 0x20, 0x24, 0x2e, 0x27, 0x20,
    0x22, 0x2c, 0x23, 0x1c, 0x1c, 0x28, 0x37, 0x29, 0x2c, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1f, 0x27,
    0x39, 0x3d, 0x38, 0x32, 0x3c, 0x2e, 0x33, 0x34, 0x32, 0xff, 0xdb, 0x00, 0x43, 0x01, 0x09, 0x09,
    0x09, 0x0c, 0x0b, 0x0c, 0x18, 0x0d, 0x0d, 0x18, 0x32, 0x21, 0x1c, 0x21, 0x32, 0x32, 0x32, 0x32,
    0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32,
    0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32,
    0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0x32, 0xff, 0xc2,
    0x00, 0x11, 0x08, 0x00, 0x30, 0x00, 0x40, 0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11,
    0x01, 0xff, 0xc4, 0x00, 0x15, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0xff, 0xc4, 0x00, 0x16, 0x01, 0x01, 0x01, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x05,
    0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x10, 0x03, 0x10, 0x00, 0x00, 0x01, 0x91, 0xa7,
    0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x3f, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00,
    0x01, 0x05, 0x02, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda, 0x00, 0x08, 0x01, 0x03,
    0x01, 0x01, 0x3f, 0x01, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda, 0x00, 0x08, 0x01,
    0x02, 0x01, 0x01, 0x3f, 0x01, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda, 0x00, 0x08,
    0x01, 0x01, 0x00, 0x06, 0x3f, 0x02, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda, 0x00,
    0x08, 0x01, 0x01, 0x00, 0x01, 0x3f, 0x21, 0x43, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02,
    0x00, 0x03, 0x00, 0x00, 0x00, 0x10, 0x92, 0x49, 0x24, 0x92, 0x49, 0x24, 0x92, 0x49, 0x24, 0x92,
    0x49, 0x24, 0x92, 0x49, 0x24, 0x92, 0x49, 0x24, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff, 0xda,
    0x00, 0x08, 0x01, 0x03, 0x01, 0x01, 0x3f, 0x10, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x11, 0x01, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50, 0xff,
    0xda, 0x00, 0x08, 0x01, 0x02, 0x01, 0x01, 0x3f, 0x10, 0x43, 0xff, 0xc4, 0x00, 0x14, 0x10, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x50,
    0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x3f, 0x10, 0x43, 0xff, 0xd9,
];

/// Production's job holds each picture to one whole-decode bound
/// (`TERMS`) as its allocation limit: a 640×480 baseline JPEG and a
/// progressive 4:4:4 one (drawn on its 256² canvas) are read into cells,
/// and so is one whose file runs on to 15 MiB, whose envelope passes the
/// cell term;
/// beside a JPEG whose decoder's
/// envelope passes the bound by its file's length (a small picture whose
/// file runs on to 21 MiB), the pictures are absent.
#[test]
fn the_job_holds_each_picture_to_the_whole_decode_term() {
    use super::ALLOC_LIMIT;
    let _serial = crate::imports::tests::serial();
    assert_eq!(ALLOC_LIMIT.load(Ordering::SeqCst), 0, "production's limit");
    let d = tempfile::tempdir().unwrap();
    let jpeg = |name: &str, [w, h]: [u32; 2]| {
        image::RgbImage::from_pixel(w, h, image::Rgb([30, 60, 90]))
            .save_with_format(d.path().join(name), image::ImageFormat::Jpeg)
            .unwrap();
    };
    jpeg("base.jpg", [640, 480]);
    assert!(PROGRESSIVE_444.windows(2).any(|m| m == [0xff, 0xc2]));
    std::fs::write(d.path().join("prog.jpg"), PROGRESSIVE_444).unwrap();
    // Two small JPEGs whose files run on: one to 15 MiB, whose envelope
    // (about 120 MiB) passes the cell term but not the whole decode, and
    // one to 21 MiB, whose envelope passes the whole decode.
    for (name, len) in [("mid.jpg", 15_usize << 20), ("long.jpg", 21 << 20)] {
        jpeg(name, [64, 48]);
        let mut bytes = std::fs::read(d.path().join(name)).unwrap();
        bytes.resize(len, 0);
        std::fs::write(d.path().join(name), bytes).unwrap();
    }
    let screens = Installers::default();
    let link = render(&screens);
    let coord = Coordinator::new(CAPS);
    let w = worker(&link);
    let entries = |t: &BgaTable| t.entries().iter().map(|e| native(*e)).collect::<Vec<_>>();
    w.renew();
    w.attempt(want(d.path(), &["base.jpg", "prog.jpg", "mid.jpg"]));
    assert_eq!(
        entries(&ready(&w, &coord)),
        [Some([640, 480]), Some([256, 256]), Some([256, 256])]
    );
    w.committed();
    w.renew();
    w.attempt(want(d.path(), &["base.jpg", "prog.jpg", "long.jpg"]));
    assert_eq!(entries(&ready(&w, &coord)), [None, None, None]);
}
