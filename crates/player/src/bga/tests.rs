//! The BGA pictures' worker (design.md "Rendering and Skins", BGA; "Load
//! Pipeline", Freshness): one owned job of its own kind holding one
//! foreground permit, cancelled through the choice's scope, whose
//! publication of a generation left is never current; and a retry that
//! reuses the array only while every id resolves to the same unchanged
//! file.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::coord::{Caps, Class, Coordinator, JobKey, JobKind, Owner, Scope, Spawner};
use skin_model::bga::{BgaEntry, BgaTable};

use super::{BgaMsg, BgaWorker, HELD, HOLD, Poll, Want};
use crate::render_side::RenderLink;
use crate::render_side::tests::Installers;

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
    let (reply, rx) = std::sync::mpsc::sync_channel(1);
    assert!(link.bga_publisher().send(
        left,
        BgaMsg::Publish {
            key: None,
            entries: vec![BgaEntry::Absent],
            reply,
        },
    ));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        Err(render::BgaFault::Stale)
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
