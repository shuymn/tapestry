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
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut c = self.charts.lock().unwrap();
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
        _target: render::ChartTarget,
        _images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<ChartTable, RenderError> {
        self.publish()
    }
    fn publish_chart_absent(
        &mut self,
        _target: render::ChartTarget,
    ) -> Result<ChartTable, RenderError> {
        self.publish()
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
    Rig {
        link: spawn_empty(make),
        charts,
        installation,
        hold,
    }
}

/// The worker of `link`'s generation, its current set read for a target
/// with a chart layer: both absent (no folder), not a job.
fn worker_with_a_set(link: &super::super::RenderLink) -> ChartImageWorker {
    let mut w = ChartImageWorker::new(link.chart_generation());
    w.want(Want {
        dir: std::path::PathBuf::from("no such folder"),
        names: [String::new(), String::new()],
        side: Some(2_048),
    });
    w.step(None, &|| false, None);
    assert!(w.current().is_some());
    w
}

/// Polls `w`'s publication for `request` until it answers, at most 10 s.
fn answered(w: &mut ChartImageWorker, link: &super::super::RenderLink, request: Option<u64>) {
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
    let mut w = worker_with_a_set(&r.link);
    answered(&mut w, &r.link, None);
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
    let mut w = worker_with_a_set(&r.link);
    answered(&mut w, &r.link, None);
    assert_eq!(r.charts.lock().unwrap().published, 1);
    // Answered again without a publication while the target holds.
    answered(&mut w, &r.link, None);
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
    answered(&mut w, &r.link, None);
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
    let mut w = worker_with_a_set(&r.link);
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
    let mut w = ChartImageWorker::new(r.link.chart_generation());
    w.renew();
    w.want(Want {
        dir: std::path::PathBuf::from("no such folder"),
        names: [String::new(), String::new()],
        side: Some(2_048),
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
    answered(&mut w, &r.link, None);
    assert_eq!(r.charts.lock().unwrap().published, 1);
    r.link.exit();
}
