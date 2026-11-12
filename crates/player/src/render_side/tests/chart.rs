//! The chart images' tables through the render thread: a publication is
//! current only for its generation, attempt and target
//! ([`crate::chart_images::PublicationKey::current`]), so a new generation
//! hides the last table from the next loading frame, an acknowledgment of
//! a replaced target is asked again, an answer that never comes expires,
//! and the job publishes its own set for the loading frames.

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use skin_model::chart_image::{ChartKey, ChartTable};

use super::*;
use crate::chart_images::{ChartImageWorker, Publish, Want};

/// What a [`ChartScreen`] saw.
#[derive(Default)]
struct Charts {
    /// Whether a table is published: set by a publication, cleared by an
    /// invalidation.
    table: bool,
    /// Publications made.
    published: u32,
    /// Absent publications among them.
    absent: u32,
    /// A publication waits on `hold` now.
    stalled: bool,
    /// Each publication's target, staged or not.
    staged: Vec<bool>,
    /// Staged targets have no array of their own: they answer
    /// `NoChartLayer`, as a reused specialization's staging does.
    no_staged_array: bool,
    /// Each loading frame's lifetime, and whether a table was published
    /// as it was drawn.
    frames: Vec<(u64, bool)>,
}

/// A screen with a chart layer: its committed layer's key names
/// `installation`, and its publications wait while `hold` is set.
struct ChartScreen {
    charts: Arc<Mutex<Charts>>,
    installation: Arc<AtomicU64>,
    hold: Arc<AtomicBool>,
}

impl ChartScreen {
    fn publish(&self) -> Result<ChartTable, RenderError> {
        while self.hold.load(Ordering::Acquire) {
            self.charts.lock().unwrap().stalled = true;
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut c = self.charts.lock().unwrap();
        c.stalled = false;
        c.table = true;
        c.published += 1;
        Ok(ChartTable::ABSENT)
    }
}

impl Screen for ChartScreen {
    fn load(
        &mut self,
        _chart: &Chart,
        _section: Option<&eval::Section>,
    ) -> Result<(), RenderError> {
        Ok(())
    }
    fn wait(&self) -> bool {
        true
    }
    fn paced(&self) -> bool {
        false
    }
    fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        Ok(())
    }
    fn draw(&mut self) -> Result<FrameTiming, RenderError> {
        Ok(timing(true))
    }
    fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
        Ok(true)
    }
    fn loading(
        &mut self,
        _page: &crate::menu_screen::MenuPage,
        _update: Option<&LoadingUpdate>,
        _changed: bool,
        frame: &render::LoadingFrame<'_>,
    ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
        let mut c = self.charts.lock().unwrap();
        let table = c.table;
        c.frames.push((frame.lifetime, table));
        Ok(Ok(timing(true)))
    }
    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        _images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<ChartTable, RenderError> {
        let staged = matches!(target, render::ChartTarget::Staged(_));
        let mut c = self.charts.lock().unwrap();
        c.staged.push(staged);
        if staged && c.no_staged_array {
            return Err(RenderError::NoChartLayer);
        }
        drop(c);
        self.publish()
    }
    fn publish_chart_absent(
        &mut self,
        _target: render::ChartTarget,
    ) -> Result<ChartTable, RenderError> {
        let t = self.publish();
        self.charts.lock().unwrap().absent += 1;
        t
    }
    fn invalidate_chart_images(&mut self) {
        self.charts.lock().unwrap().table = false;
    }
    fn chart_key(&self) -> Option<ChartKey> {
        Some(ChartKey {
            renderer: 1,
            installation: self.installation.load(Ordering::SeqCst),
            atlas: 1,
            generation: 1,
        })
    }
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
}

struct Rig {
    link: super::super::RenderLink,
    charts: Arc<Mutex<Charts>>,
    installation: Arc<AtomicU64>,
    hold: Arc<AtomicBool>,
}

fn rig() -> Rig {
    let charts = Arc::new(Mutex::new(Charts::default()));
    let installation = Arc::new(AtomicU64::new(1));
    let hold = Arc::new(AtomicBool::new(false));
    let (c, i, h) = (
        Arc::clone(&charts),
        Arc::clone(&installation),
        Arc::clone(&hold),
    );
    let make: MakeScreen = Box::new(move || {
        Ok(Box::new(ChartScreen {
            charts: Arc::clone(&c),
            installation: Arc::clone(&i),
            hold: Arc::clone(&h),
        }) as Box<dyn Screen>)
    });
    let link = spawn_empty(make);
    // The screen built, its chart layer's key mirrored: a publication
    // reaches it.
    assert!(wait_until(|| link.chart_now(0).committed.is_some()));
    Rig {
        link,
        charts,
        installation,
        hold,
    }
}

/// The worker of `link`'s generation, its current set read for a target
/// with a chart layer: both absent (no folder), not a job.
fn worker_with_a_set(link: &super::super::RenderLink) -> ChartImageWorker {
    let w = ChartImageWorker::new(link.chart_generation());
    w.attempt(Want {
        dir: std::path::PathBuf::from("no such folder"),
        names: [String::new(), String::new()],
        play: Some(2_048),
        result: None,
    });
    w.step(None, &|| false, None);
    assert!(w.current().is_some());
    w
}

/// Polls `w`'s publication for `request` until it answers, at most 10 s.
fn answered(w: &ChartImageWorker, link: &super::super::RenderLink, request: Option<u64>) {
    let end = Instant::now() + Duration::from_secs(10);
    while !matches!(w.publish(link, request), Publish::Answered(_)) {
        assert!(Instant::now() < end, "never answered");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A choice's new generation hides the last chart's table from the next
/// loading frame: once the generation moves, every frame of a lifetime
/// begun after it is drawn with no table, though nothing was published
/// or invalidated for it.
#[test]
fn a_new_generation_hides_the_last_table_from_the_next_loading_frame() {
    let r = rig();
    let w = worker_with_a_set(&r.link);
    answered(&w, &r.link, None);
    r.link.begin_loading(&chosen("A"), None);
    r.link.loading().show(one_line("A"));
    let a = r.link.loading_lifetime();
    assert!(wait_until(|| {
        r.charts
            .lock()
            .unwrap()
            .frames
            .iter()
            .any(|&(l, t)| l == a && t)
    }));
    // The next choice: the generation first, then its lifetime.
    w.renew();
    r.link.begin_loading(&chosen("B"), None);
    r.link.loading().show(one_line("B"));
    let b = r.link.loading_lifetime();
    assert!(wait_until(|| {
        r.charts
            .lock()
            .unwrap()
            .frames
            .iter()
            .filter(|&&(l, _)| l == b)
            .count()
            >= 3
    }));
    let drawn: Vec<bool> = r
        .charts
        .lock()
        .unwrap()
        .frames
        .iter()
        .filter(|&&(l, _)| l == b)
        .map(|&(_, t)| t)
        .collect();
    assert!(drawn.iter().all(|t| !t), "B drew A's table: {drawn:?}");
    r.link.exit();
}

/// An acknowledgment of a target since replaced (the committed layer's
/// installation, as a recovery or another skin's commit replaces it) is
/// not current: the same set is published again into the new one, and
/// only that answer is taken.
#[test]
fn an_acknowledgment_of_a_replaced_target_is_asked_again() {
    let r = rig();
    let w = worker_with_a_set(&r.link);
    answered(&w, &r.link, None);
    assert_eq!(r.charts.lock().unwrap().published, 1);
    // Answered again without a publication while the target holds.
    answered(&w, &r.link, None);
    assert_eq!(r.charts.lock().unwrap().published, 1);
    r.installation.store(2, Ordering::SeqCst);
    // The render thread mirrors its targets at its next turn, which a
    // message gives it.
    r.link.loading().show(one_line("next"));
    assert!(wait_until(|| {
        r.link
            .chart_now(w.generation())
            .committed
            .map(|k| k.installation)
            == Some(2)
    }));
    answered(&w, &r.link, None);
    assert_eq!(
        r.charts.lock().unwrap().published,
        2,
        "the old target's answer was taken"
    );
    r.link.exit();
}

/// A publication whose answer never comes expires at the link's bound,
/// counted from the attempt's first publication, never readied.
#[test]
fn an_answer_that_never_comes_expires() {
    let mut r = rig();
    r.link
        .set_timeouts(Duration::from_millis(200), Duration::from_secs(5));
    let w = worker_with_a_set(&r.link);
    r.hold.store(true, Ordering::Release);
    let began = Instant::now();
    let end = began + Duration::from_secs(10);
    let state = loop {
        match w.publish(&r.link, Some(7)) {
            Publish::Waiting => {}
            other => break format!("{other:?}"),
        }
        assert!(Instant::now() < end, "still waiting at 10 s");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(state, "Expired");
    assert!(began.elapsed() >= Duration::from_millis(200));
    r.hold.store(false, Ordering::Release);
    r.link.exit();
}

/// The job that reads a chosen chart's set publishes it as soon as it is
/// read, with nothing polled on the game thread: the loading frames get
/// it while the chart's sounds load.
#[test]
fn the_job_publishes_its_set_for_the_loading_frames() {
    let r = rig();
    let coord = game::coord::Coordinator::new(game::coord::Caps {
        foreground: 4,
        speculative: 1,
    });
    let w = ChartImageWorker::new(r.link.chart_generation());
    w.renew();
    w.attempt(Want {
        dir: std::path::PathBuf::from("no such folder"),
        names: [String::new(), String::new()],
        play: Some(2_048),
        result: None,
    });
    let publisher = r.link.chart_publisher();
    w.step(
        Some((&coord, game::coord::Spawner::SYSTEM)),
        &|| false,
        Some(&publisher),
    );
    assert!(wait_until(|| r.charts.lock().unwrap().published == 1));
    // Its answer is the attempt's once taken: nothing is published again.
    let end = Instant::now() + Duration::from_secs(10);
    while !(w.idle() && w.current().is_some()) {
        assert!(Instant::now() < end);
        w.step(
            Some((&coord, game::coord::Spawner::SYSTEM)),
            &|| false,
            None,
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    answered(&w, &r.link, None);
    assert_eq!(r.charts.lock().unwrap().published, 1);
    r.link.exit();
}

/// A folder holding a 64×48 stage file, `s.png`.
fn folder_with_stage() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(d.path().join("s.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 64, 48);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [10, 20, 30, 255].repeat(64 * 48);
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    d
}

fn want_in(dir: &std::path::Path, side: Option<u32>) -> Want {
    Want {
        dir: dir.to_path_buf(),
        names: ["s.png".to_owned(), String::new()],
        play: side,
        result: None,
    }
}

fn coordinator() -> game::coord::Coordinator {
    game::coord::Coordinator::new(game::coord::Caps {
        foreground: 4,
        speculative: 1,
    })
}

/// A job that panics before it publishes leaves its reply unsent: its
/// failed set is published, absent, and the attempt's answer is that
/// publication's, never taken from the dropped sender.
#[test]
fn a_panicked_job_publishes_its_failed_set_before_the_answer() {
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let d = folder_with_stage();
    let coord = coordinator();
    let w = ChartImageWorker::new(r.link.chart_generation());
    w.renew();
    w.attempt(want_in(d.path(), Some(2_048)));
    let publisher = r.link.chart_publisher();
    crate::chart_images::PANIC.store(true, Ordering::Release);
    w.step(
        Some((&coord, game::coord::Spawner::SYSTEM)),
        &|| false,
        Some(&publisher),
    );
    let end = Instant::now() + Duration::from_secs(10);
    while !(w.idle() && w.current().is_some()) {
        assert!(Instant::now() < end, "{w:?}");
        w.step(
            Some((&coord, game::coord::Spawner::SYSTEM)),
            &|| false,
            None,
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let answer = loop {
        if let Publish::Answered(a) = w.publish(&r.link, None) {
            break a;
        }
        assert!(Instant::now() < end, "never answered");
        std::thread::sleep(Duration::from_millis(1));
    };
    let c = r.charts.lock().unwrap();
    assert_eq!(
        (c.published, answer.is_ok()),
        (1, true),
        "answered before the failed set was published"
    );
    drop(c);
    r.link.exit();
}

/// A stalled render thread holding the first choice's publication, and
/// two more choices: no job reads a set until that publication is
/// answered, so at most the current set and the retiring one are alive;
/// once it is, the last choice's job reads.
#[test]
fn a_stalled_render_thread_holds_back_the_next_reads() {
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (a, b, c) = (
        folder_with_stage(),
        folder_with_stage(),
        folder_with_stage(),
    );
    let coord = coordinator();
    let w = ChartImageWorker::new(r.link.chart_generation());
    let publisher = r.link.chart_publisher();
    let step = |w: &ChartImageWorker| {
        w.step(
            Some((&coord, game::coord::Spawner::SYSTEM)),
            &|| false,
            Some(&publisher),
        );
    };
    r.hold.store(true, Ordering::Release);
    w.renew();
    w.attempt(want_in(a.path(), Some(2_048)));
    step(&w);
    let end = Instant::now() + Duration::from_secs(10);
    while !w.idle() {
        assert!(Instant::now() < end, "{w:?}");
        w.service();
        std::thread::sleep(Duration::from_millis(1));
    }
    // The render thread holds A's publication.
    assert!(wait_until(|| r.charts.lock().unwrap().stalled));
    for d in [&b, &c] {
        w.renew();
        w.attempt(want_in(d.path(), Some(2_048)));
        for _ in 0..20 {
            step(&w);
            w.service();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(w.begun(), 1, "a set read while the render thread held one");
    r.hold.store(false, Ordering::Release);
    while !(w.idle() && w.current().is_some()) {
        assert!(Instant::now() < end, "{w:?}");
        w.service();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(w.begun(), 2, "C's job, and never B's");
    r.link.exit();
}

/// A target with no chart layer: its absence is terminal at the choice and
/// published then, so the loading frames' title card condition reads the
/// images absent while the chart loads, before any preparation.
#[test]
fn a_target_with_no_layer_reads_absent_while_the_chart_loads() {
    let r = rig();
    let coord = coordinator();
    let w = ChartImageWorker::new(r.link.chart_generation());
    w.renew();
    w.attempt(want_in(std::path::Path::new("no such folder"), None));
    let publisher = r.link.chart_publisher();
    w.step(
        Some((&coord, game::coord::Spawner::SYSTEM)),
        &|| false,
        Some(&publisher),
    );
    assert!(wait_until(|| r.charts.lock().unwrap().absent == 1));
    assert_eq!(w.begun(), 0);
    r.link.exit();
}

/// The next chart's job held back at its choice by the last one's thread
/// begins from the loading waits' looks, with nothing else stepped, and
/// its set reaches the loading frames.
#[test]
fn a_job_held_back_begins_from_the_waits() {
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let (a, b) = (folder_with_stage(), folder_with_stage());
    let coord = coordinator();
    let w = ChartImageWorker::new(r.link.chart_generation());
    let publisher = r.link.chart_publisher();
    crate::chart_images::HOLD.store(true, Ordering::Release);
    w.renew();
    w.attempt(want_in(a.path(), Some(2_048)));
    w.step(
        Some((&coord, game::coord::Spawner::SYSTEM)),
        &|| false,
        Some(&publisher),
    );
    assert_eq!(w.begun(), 1);
    // A is left in its read; B chosen: held back.
    w.forget();
    w.renew();
    w.attempt(want_in(b.path(), Some(2_048)));
    w.step(
        Some((&coord, game::coord::Spawner::SYSTEM)),
        &|| false,
        Some(&publisher),
    );
    assert_eq!(w.begun(), 1);
    // The waits look; nothing else steps the worker.
    let controls = crate::controls::Controls::new(QpcClock::start());
    let served = w.clone();
    controls.serve_images(crate::controls::ImageService(Box::new(move || {
        served.service();
    })));
    crate::chart_images::HOLD.store(false, Ordering::Release);
    let end = Instant::now() + Duration::from_secs(10);
    while r.charts.lock().unwrap().published < 1 || w.begun() < 2 {
        assert!(Instant::now() < end, "{w:?}");
        let _ = controls.look();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(w.begun(), 2);
    r.link.exit();
}

/// A publication for a staging the render thread does not hold is
/// admitted nowhere: nothing reaches the committed installation, and the
/// attempt gets no answer from it.
#[test]
fn a_publication_for_a_staging_gone_publishes_nothing() {
    let r = rig();
    let w = worker_with_a_set(&r.link);
    for _ in 0..50 {
        assert_eq!(w.publish(&r.link, Some(99)), Publish::Waiting);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        r.charts.lock().unwrap().published,
        0,
        "a staging's publication reached the committed installation"
    );
    r.link.exit();
}

/// A staging with no array of its own (a pending skin's specialization
/// reused on the installation in use) draws from the committed
/// installation: the attempt's publication goes there, and its answer is
/// the attempt's.
#[test]
fn a_staging_without_its_own_array_publishes_into_the_committed_one() {
    let r = rig();
    r.charts.lock().unwrap().no_staged_array = true;
    let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
    let loaded = r.link.begin_load(Arc::clone(&chart), None);
    loaded.wait_serving(&r.link, || false).unwrap();
    let mut pending = r
        .link
        .begin_attempt(titled(&chart), super::super::AttemptSkin::InUse);
    let answered = loop {
        if let Some(a) = pending.wait_until(None, || false) {
            break a.unwrap();
        }
    };
    assert_eq!(answered.preparation, Preparation::Ready);
    let request = pending.request();
    let w = worker_with_a_set(&r.link);
    answered_for(&w, &r.link, Some(request));
    let c = r.charts.lock().unwrap();
    assert_eq!(
        c.staged,
        [true, false],
        "not published into the committed installation"
    );
    assert_eq!(c.published, 1);
    drop(c);
    r.link.exit();
}

/// [`answered`], for a staging's request.
fn answered_for(w: &ChartImageWorker, link: &super::super::RenderLink, request: Option<u64>) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match w.publish(link, request) {
            Publish::Answered(a) => {
                assert!(a.is_ok(), "{a:?}");
                return;
            }
            Publish::Waiting => {}
            Publish::Expired => panic!("expired"),
        }
        assert!(Instant::now() < end, "never answered");
        std::thread::sleep(Duration::from_millis(1));
    }
}
