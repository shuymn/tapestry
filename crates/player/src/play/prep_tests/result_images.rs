//! The chart's images from the preparation to the result, through the
//! player's preparation and lifecycle (S10-12 PR R, RT8, RT10, RT12-RT14,
//! RT19, RT25, RT26): the facts stage resolves the images' availability,
//! the commit alone hands the attempt's sources and result half to the
//! result skin, a payload carries the fit its view draws, and selection,
//! a retry and practice leave each owner with what the exit table says.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::Preparation;
use game::{AppRig as _, Rig as _};
use skin_model::ScreenKind;
use skin_model::chart_image::ChartImage;

use super::{begin, imports, on_rig, polled, stage, until};
use crate::chart_images::{ChartImages, HOLD};
use crate::render_side::tests::Installers;
use crate::result_skin::{ResultSkin, ResultStart};

/// The design's bound on what the images keep during a play and during
/// its result (design.md "Memory Budget").
const PLAY_BOUND: u64 = 480 << 20;

/// Lets the images' reads go when dropped, a failing test's included.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
    }
}

/// `dir/name`, a `w`×`h` PNG of one colour.
fn png(dir: &Path, name: &str, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [r, g, b, 255].repeat((w * h) as usize);
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A chart naming a 640×480 stage file `s.png` (no back image), the file
/// written beside it.
fn chart_with_stage() -> (tempfile::TempDir, game::Loaded) {
    let (dir, l) = crate::imports::facts::tests::loaded_kept("#STAGEFILE s.png\n", "tapestry");
    png(dir.path(), "s.png", [640, 480], [200, 30, 40]);
    (dir, l)
}

/// The stage file's native size in `set`, `None` when absent.
fn stage_native(set: &ChartImages) -> Option<[u32; 2]> {
    set.read(ChartImage::Stage)
        .image
        .as_ref()
        .ok()
        .map(|f| f.native())
}

/// A fixed Lua result skin keeping a chart layer: the stage file (−100)
/// drawn at 560, 240, 800×600, the centre of a 1 920×1 080 frame; built
/// for a WARP target's limits. The caller holds the importer's lock.
fn result_bundle() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "s.png", [64, 16], [9, 9, 9]);
    std::fs::write(
        d.path().join("s.luaskin"),
        "local skin = { type = 7, name = \"t\", w = 1920, h = 1080 }\n\
         if not skin_config then return skin end\n\
         skin.source = { { id = \"s\", path = \"s.png\" } }\n\
         skin.image = { { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }\n\
         skin.value = { { id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                          digit = 4, ref = 71 } }\n\
         skin.destination = {\n\
           { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
           { id = \"dot\", op = { 90 }, dst = { { x = 0, y = 0, w = 8, h = 8 } } },\n\
           { id = -100, dst = { { x = 560, y = 240, w = 800, h = 600 } } },\n\
         }\n\
         return skin\n",
    )
    .unwrap();
    let choice = game::SkinChoice {
        root: d.path().to_owned(),
        entry: "s.luaskin".to_owned(),
        options: Default::default(),
        files: Default::default(),
    };
    let target = crate::imports::ImportTarget {
        limits: skin_model::AtlasLimits {
            max_side: 4_096,
            max_layers: 16,
            reserved_layers: render::RESERVED_LAYERS,
        },
        size: [1_920, 1_080],
        default_font: || Ok(crate::render_outline::default_font()),
    };
    let built = crate::imports::build(
        ScreenKind::Result,
        &choice,
        false,
        &target,
        &skin_beatoraja::Cancel::new(),
    );
    match built {
        crate::imports::KindOutcome::Bundle(b, _, _) => {
            assert!(b.chart_layer().is_some(), "the result keeps a chart layer");
            (d, b)
        }
        other => panic!("{other:?}"),
    }
}

/// `rig`'s result skin made the fixed `bundle`.
fn fixed(rig: &mut crate::play::PlayerRig<'_>, bundle: &Arc<render::SkinBundle>) {
    rig.result_skin = ResultSkin::new(Some(ResultStart::Fixed(Arc::clone(bundle), None)));
}

/// A coordinator on `rig`, as song selection gives one: the images' jobs
/// run on its threads.
fn with_coordinator(rig: &mut crate::play::PlayerRig<'_>, dir: &Path) {
    let (sel, _) = crate::selection::tests::tree(dir, &[&["b.bms"]]);
    rig.selection = Some(sel);
}

/// `loaded`'s preparation polled to its end.
fn prepared(rig: &mut crate::play::PlayerRig<'_>, loaded: &game::Loaded) -> Preparation {
    let mut h = begin(rig, loaded);
    polled(rig, &mut h).unwrap()
}

/// The result shown for a completed play, as the flow shows it: its
/// layout requested, its end page and its presentation.
fn show_result(rig: &mut crate::play::PlayerRig<'_>, results: &Path) {
    let report = crate::end_screen::tests::report(rules::tapestry());
    let screen = crate::end_screen::tests::screen(results, &report);
    rig.result_compared(&report, &screen);
    rig.show_end(&report, &screen);
}

/// The chart's images the payload published last carries.
fn payload_chart(rig: &crate::play::PlayerRig<'_>) -> Option<Arc<ChartImages>> {
    rig.presenter.payload().and_then(|p| p.chart.clone())
}

/// RT25, RX6e: the direct launch (no coordinator) with a play skin keeping
/// no chart layer and a result skin keeping one: the facts stage decodes
/// the sources on the game thread, so the facts resolve and the
/// preparation readies; the commit hands the result its half, fitted for
/// its chart slot.
#[test]
fn the_direct_launch_resolves_the_results_images_without_a_play_layer() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        assert!(rig.selection.is_none(), "the direct launch");
        rig.play_chart = None;
        fixed(rig, &bundle);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        let images = rig.result_chart.unwrap().images;
        assert!(images.stage && !images.back, "{images:?}");
        assert!(rig.chart_images.current().is_none(), "no play half");
        let half = rig.chart_images.current_result().expect("the result half");
        assert_eq!(stage_native(&half), Some([640, 480]));
        assert_eq!(rig.chart_images.counts(), (1, 1));
        rig.commit_attempt().unwrap();
        let drawn = rig.result_skin.chart_for(&bundle).expect("handed");
        assert!(Arc::ptr_eq(&drawn, &half));
    });
}

/// RT10 (RX6c, RX7): play's publication held past the render link's bound
/// refuses the preparation before any commit, and the commit then asked
/// for fails, handing the result nothing, though the worker's sources and
/// result half were read, so a hand-off before the commit's answer would
/// show.
#[test]
fn a_held_publication_hands_the_result_nothing() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        rig.render
            .set_timeouts(Duration::from_secs(1), Duration::from_secs(5));
        rig.render.hold_chart_publications(true);
        let ended = prepared(rig, &l);
        assert!(
            matches!(&ended, Preparation::Refused(r) if r.reason.contains("images")),
            "{ended:?}"
        );
        assert!(rig.chart_images.current_result().is_some(), "read");
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
        rig.render.hold_chart_publications(false);
        assert_eq!(rig.result_skin.image_bytes(), 0);
        assert!(rig.result_skin.chart_for(&bundle).is_none());
    });
}

/// RT10 (RX7): a preparation answered ready whose commit the render thread
/// refuses (its staging gone, as a discard leaves it: `NothingStaged`)
/// hands the result nothing, the sources and result half read.
#[test]
fn a_refused_commit_hands_the_result_nothing() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        assert!(rig.chart_images.current_result().is_some(), "read");
        rig.render.discard_attempt();
        let committed = rig.commit_attempt();
        assert!(
            matches!(committed, Err(crate::render_side::ScreenError::Lost)),
            "{committed:?}"
        );
        assert_eq!(rig.result_skin.image_bytes(), 0);
        assert!(rig.result_skin.chart_for(&bundle).is_none());
    });
}

/// RT8 (RX3, RX6b): a quit while the facts stage's job decodes, with a
/// coordinator: the preparation ends cancelled, the facts unresolved, and
/// nothing is handed to the result.
#[test]
fn a_quit_in_the_facts_stage_cancels_and_hands_nothing() {
    let _serial = imports::serial();
    let (dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        with_coordinator(rig, dir.path());
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        HOLD.store(true, Ordering::Release);
        let mut h = begin(rig, &l);
        // Held in the facts stage: the job reads, held.
        let first = rig
            .poll_preparation(&mut h, Instant::now() + Duration::from_millis(50))
            .unwrap();
        assert!(matches!(first, game::PrepPoll::Pending), "{first:?}");
        assert_eq!(stage(rig), "facts");
        assert!(!rig.chart_images.idle(), "the job reads");
        rig.controls.command(game::Command::Quit);
        let ended = polled(rig, &mut h).unwrap();
        assert_eq!(ended, Preparation::Cancelled);
        assert_eq!(rig.chart_images.facts(), None);
        assert!(rig.result_chart.is_none(), "no result facts");
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
        assert_eq!(rig.result_skin.image_bytes(), 0);
        assert!(rig.result_skin.chart_for(&bundle).is_none());
    });
}

/// RT26 (RX6d): with a coordinator, the choice's publication held by the
/// render thread, unanswered, and a new generation asked for: the facts
/// stage's job cannot begin while it is, and past the render link's bound
/// the preparation is refused, never left waiting (the hold lasts longer).
#[test]
fn a_publication_held_at_the_choice_refuses_the_facts_stage() {
    let _serial = imports::serial();
    let (dir, l) = chart_with_stage();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        let _watchdog = crate::tests::Watchdog::arm("the facts stage's refusal");
        with_coordinator(rig, dir.path());
        rig.play_chart = Some(2_048);
        rig.render
            .set_timeouts(Duration::from_secs(1), Duration::from_secs(5));
        rig.render.hold_chart_publications(true);
        // The choice: its job reads and publishes its play half, held.
        rig.chart_images_at_choice(&l);
        until("the choice's job ended", || {
            rig.chart_images.service();
            rig.chart_images.idle() && rig.chart_images.current().is_some()
        });
        // A new generation (the next loading's): its sources are read again.
        rig.chart_images.renew();
        let began = Instant::now();
        let ended = prepared(rig, &l);
        let waited = began.elapsed();
        rig.render.hold_chart_publications(false);
        assert!(
            matches!(&ended, Preparation::Refused(r) if r.reason.contains("not read in time")),
            "{ended:?}"
        );
        assert!(waited >= Duration::from_secs(1), "{waited:?}");
        assert_eq!(rig.chart_images.begun(), 1, "no job began for the facts");
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
    });
}

/// RT19 (RX5b): with a coordinator, a play skin keeping no chart layer and
/// a result skin keeping one: the job decodes the stage file once and
/// derives only the result half, play's images are absent, and the result
/// view's payload carries that half.
#[test]
fn a_result_layer_alone_derives_the_result_half() {
    let _serial = imports::serial();
    let (dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let results = tempfile::tempdir().unwrap();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        with_coordinator(rig, dir.path());
        rig.play_chart = None;
        fixed(rig, &bundle);
        rig.chart_images_at_choice(&l);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        assert!(rig.chart_images.current().is_none(), "no play half");
        let half = rig.chart_images.current_result().expect("the result half");
        assert_eq!(stage_native(&half), Some([640, 480]));
        assert_eq!(rig.chart_images.counts(), (1, 1), "one decode, one fit");
        rig.commit_attempt().unwrap();
        show_result(rig, results.path());
        let chart = payload_chart(rig).expect("the payload's images");
        assert!(Arc::ptr_eq(&chart, &half));
        assert_eq!(stage_native(&chart), Some([640, 480]));
    });
}

/// RT12 (RX8a, RX8b), the player's half: the result's upload refused shows
/// the built-in page for the attempt, its payload carrying no images
/// though the result keeps them; an upload withdrawn by a quit while the
/// render thread holds it installs nothing once released, the images kept
/// by the result.
#[test]
fn a_refused_or_withdrawn_upload_carries_no_images() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let results = tempfile::tempdir().unwrap();
    // Refused.
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        screens.refuse_install.store(true, Ordering::Release);
        rig.commit_attempt().unwrap();
        screens.refuse_install.store(false, Ordering::Release);
        assert!(rig.presenter.attempt_fallen().is_some(), "built-in");
        assert!(rig.result_skin.chart_for(&bundle).is_some(), "kept");
        show_result(rig, results.path());
        let p = rig.presenter.payload().expect("a payload");
        assert!(p.skin.is_none(), "the built-in page");
        assert!(p.chart.is_none(), "images on the built-in page");
    });
    // Withdrawn, then released.
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        screens.hold_install.store(true, Ordering::Release);
        let controls = rig.controls;
        let held = Arc::clone(&screens.install_held);
        let committed = std::thread::scope(|s| {
            s.spawn(|| {
                until("the upload held", || held.load(Ordering::Acquire));
                controls.command(game::Command::Quit);
            });
            rig.commit_attempt()
        });
        committed.unwrap();
        assert!(rig.result_withdrawn, "withdrawn");
        screens.hold_install.store(false, Ordering::Release);
        assert!(rig.render.fence(Duration::from_secs(10)));
        let status = rig.render.result_status();
        assert_eq!(status.installed, None, "{status:?}");
        assert!(!status.retained.contains(&bundle.atlas_id()), "{status:?}");
        assert!(rig.result_skin.chart_for(&bundle).is_some(), "kept");
    });
}

/// RT14 (RX21a-c): the owners after a result, a retry, practice and
/// selection. During the play the worker keeps the sources and both
/// halves, and the result skin the attempt's sources and result half;
/// the result drops the play half; both stay within the design's bound.
/// A retry re-resolves (no decode for the unchanged file) and replaces
/// the result's images with its own; practice entry drops the result's
/// and keeps the worker's sources, so its Start decodes nothing;
/// selection entry leaves neither anything.
#[test]
fn each_exit_leaves_the_images_with_their_owners() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let (_skin, bundle) = result_bundle();
    let results = tempfile::tempdir().unwrap();
    on_warp(l.chart(), |rig, _| {
        // No input thread answers the scenes' fences here.
        rig.adjust = None;
        rig.play_chart = Some(2_048);
        fixed(rig, &bundle);
        let kept = |rig: &crate::play::PlayerRig<'_>| {
            rig.chart_images.retained() + rig.result_skin.image_bytes()
        };
        // The play.
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        let half = rig.chart_images.current_result().expect("the result half");
        let sources = rig.chart_images.sources().expect("the sources");
        rig.commit_attempt().unwrap();
        assert!(rig.chart_images.current().is_some(), "the play half");
        assert!(Arc::ptr_eq(
            &rig.result_skin.chart_for(&bundle).unwrap(),
            &half
        ));
        let during_play = kept(rig);
        assert!(
            during_play > 0 && during_play <= PLAY_BOUND,
            "{during_play}"
        );
        // Its result.
        show_result(rig, results.path());
        assert!(rig.chart_images.current().is_none(), "the play half kept");
        let during_result = kept(rig);
        assert!(
            during_result > 0 && during_result < during_play,
            "{during_result} {during_play}"
        );
        assert!(Arc::ptr_eq(
            &payload_chart(rig).expect("the payload's images"),
            &half
        ));
        // A retry: re-resolved, nothing decoded, the result's images its own.
        let decodes = rig.chart_images.counts().0;
        rig.leave_result();
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        let retried = rig.chart_images.current_result().expect("its half");
        let again = rig.chart_images.sources().expect("its sources");
        assert!(!Arc::ptr_eq(&again, &sources), "the same generation");
        assert_eq!(rig.chart_images.counts().0, decodes, "decoded again");
        rig.commit_attempt().unwrap();
        let held = rig.result_skin.chart_for(&bundle).expect("handed");
        assert!(Arc::ptr_eq(&held, &retried), "the last attempt's images");
        assert!(!Arc::ptr_eq(&held, &half));
        // Practice: the result's images go, the worker's sources stay.
        let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
        rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
        assert_eq!(rig.result_skin.image_bytes(), 0);
        assert!(rig.result_skin.chart_for(&bundle).is_none());
        assert!(rig.chart_images.sources().is_some(), "the candidates");
        // Its Start: a new generation, decoding nothing unchanged.
        rig.entered_loading(l.path());
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        assert_eq!(rig.chart_images.counts().0, decodes, "decoded again");
        rig.commit_attempt().unwrap();
        assert!(rig.result_skin.image_bytes() > 0);
        // Back to selection, as the flow goes (the chart left, then
        // selection entered): nothing of the chart's images stays, once
        // the last publication is answered.
        rig.left_chart().unwrap();
        rig.entered_selection();
        assert_eq!(rig.result_skin.image_bytes(), 0);
        assert!(rig.result_skin.chart_for(&bundle).is_none());
        assert!(rig.chart_images.sources().is_none());
        assert!(rig.chart_images.current().is_none());
        assert!(rig.chart_images.current_result().is_none());
        until("nothing retained", || {
            rig.chart_images.service();
            rig.chart_images.retained() == 0
        });
    });
}

/// The centre texel of the frames a [`Sampler`] read back while armed.
#[derive(Clone, Default)]
struct Samples {
    armed: Arc<std::sync::atomic::AtomicBool>,
    seen: Arc<std::sync::Mutex<Vec<[u8; 4]>>>,
}

impl Samples {
    /// Arms the sampler afresh: what it saw goes.
    fn arm(&self) {
        self.seen.lock().unwrap().clear();
        self.armed.store(true, Ordering::Release);
    }

    /// Waits until a frame's centre texel satisfies `want`; the last one
    /// seen when it never does.
    fn until(&self, want: impl Fn([u8; 4]) -> bool) -> Result<[u8; 4], Option<[u8; 4]>> {
        let began = Instant::now();
        loop {
            if let Some(&t) = self.seen.lock().unwrap().iter().rev().find(|&&t| want(t)) {
                return Ok(t);
            }
            if began.elapsed() > Duration::from_secs(20) {
                return Err(self.seen.lock().unwrap().last().copied());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// A frame sink reading each frame's centre texel back while armed.
struct Sampler(Samples);

impl crate::render_side::FrameSink for Sampler {
    fn frame(&mut self, screen: &dyn crate::render_side::Screen, _: &crate::measure::Drawn<'_>) {
        if !self.0.armed.load(Ordering::Acquire) {
            return;
        }
        if let Some((w, h, px)) = screen.read_back() {
            let at = (((h / 2) * w + w / 2) * 4) as usize;
            if let Some(t) = px.get(at..at + 4) {
                self.0.seen.lock().unwrap().push([t[0], t[1], t[2], t[3]]);
            }
        }
    }
}

/// Whether texel `t` is mostly `r`, `g`, `b` (either channel order).
fn like(t: [u8; 4], [r, g, b]: [u8; 3]) -> bool {
    let near = |a: u8, b: u8| a.abs_diff(b) < 40;
    (near(t[0], r) && near(t[1], g) && near(t[2], b))
        || (near(t[2], r) && near(t[1], g) && near(t[0], b))
}

/// `f` on a player's rig over a render thread of real WARP screens with
/// `chart` loaded and the simple play skin, and the centre texels its
/// frames show. Held images are let go before the rig drops.
fn on_warp<R>(
    chart: &bms_model::Chart,
    f: impl FnOnce(&mut crate::play::PlayerRig<'_>, &Samples) -> R,
) -> R {
    let samples = Samples::default();
    let make: crate::render_side::MakeScreen = Box::new(|| {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        Ok(Box::new(crate::menu_screen::MenuScreen::new(
            r,
            Some(render::text::Fonts::runtime()?),
        )) as Box<dyn crate::render_side::Screen>)
    });
    let clock = input::QpcClock::start();
    let link = crate::render_side::spawn(
        make,
        None,
        None,
        clock,
        crate::render_side::tests::view(),
        crate::render_side::tests::measure(clock),
        Box::new(Sampler(samples.clone())),
    )
    .unwrap();
    link.load(Arc::new(chart.clone()), None).unwrap();
    crate::tests::on_rig_hooked(super::settings(), link, clock, |rig, _, _| {
        let _let_go = LetGo;
        f(rig, &samples)
    })
    .0
}

/// RT13 (RX19, RX21b): a retry after the first attempt's result drew its
/// stage file, the file replaced in between: the retry's preparation (play's
/// admission releasing the result's installation) re-resolves it, and the
/// retry's result view draws the new stage file, not the first.
#[test]
fn a_retry_draws_the_stage_file_replaced_since() {
    const FIRST: [u8; 3] = [220, 20, 20];
    const SECOND: [u8; 3] = [20, 220, 20];
    let _serial = imports::serial();
    let (dir, l) = crate::imports::facts::tests::loaded_kept("#STAGEFILE s.png\n", "tapestry");
    png(dir.path(), "s.png", [640, 480], FIRST);
    let (_skin, bundle) = result_bundle();
    let results = tempfile::tempdir().unwrap();
    on_warp(l.chart(), |rig, samples| {
        rig.adjust = None;
        fixed(rig, &bundle);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        rig.commit_attempt().unwrap();
        samples.arm();
        show_result(rig, results.path());
        let first = samples.until(|t| like(t, FIRST));
        assert!(first.is_ok(), "the first stage file: {first:?}");
        // The retry, the file replaced (another size: another identity).
        rig.leave_result();
        png(dir.path(), "s.png", [320, 240], SECOND);
        assert_eq!(prepared(rig, &l), Preparation::Ready);
        rig.commit_attempt().unwrap();
        samples.arm();
        show_result(rig, results.path());
        let second = samples.until(|t| like(t, SECOND));
        assert!(second.is_ok(), "the second stage file: {second:?}");
        assert_eq!(
            payload_chart(rig).as_deref().and_then(stage_native),
            Some([320, 240])
        );
    });
}
