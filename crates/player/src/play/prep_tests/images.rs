//! The chart's images before an attempt's readiness (S10-12 design C4):
//! the preparation answers ready only once the images are read and their
//! publication for the attempt has returned, and a quit while they are
//! read cancels it. With no coordinator on the rig, the images are read
//! on the game thread, as for the direct launch.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use game::Preparation;
use game::Rig as _;
use skin_model::chart_image::ChartImage;

use super::{begin, imports, on_rig, polled, stage};
use crate::chart_images::HOLD;
use crate::render_side::tests::Installers;

/// Lets the images' reads go when dropped, a failing test's included.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
    }
}

/// A chart naming a 640×480 stage file, the file written beside it.
fn chart_with_stage() -> (tempfile::TempDir, game::Loaded) {
    let (dir, l) = crate::imports::facts::tests::loaded_kept("#STAGEFILE s.png\n", "tapestry");
    let file = std::fs::File::create(dir.path().join("s.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 640, 480);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let texels = [10, 20, 30, 255].repeat(640 * 480);
    e.write_header().unwrap().write_image_data(&texels).unwrap();
    (dir, l)
}

/// Ready only after the images are read and published: the read held for
/// 100 ms keeps the preparation from answering, and once it answers the
/// worker is idle, the stage file read, and the publication's answer
/// already there.
#[test]
fn the_preparation_waits_for_the_images() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        rig.play_chart = Some(2_048);
        HOLD.store(true, Ordering::Release);
        let began = Instant::now();
        // The direct launch decodes in the facts stage, from the begin on.
        let ready = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                HOLD.store(false, Ordering::Release);
            });
            let mut h = begin(rig, &l);
            polled(rig, &mut h).unwrap()
        });
        assert_eq!(ready, Preparation::Ready);
        assert!(began.elapsed() >= Duration::from_millis(100));
        assert_eq!(stage(rig), "answered");
        assert!(rig.chart_images.idle());
        let set = rig.chart_images.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [640, 480]
        );
        assert!(
            matches!(
                rig.chart_images.publish(&rig.render, None),
                crate::chart_images::Publish::Answered(_)
            ),
            "the publication answered before readiness"
        );
    });
}

/// A publication whose answer the render thread holds past the link's
/// bound: the preparation is refused and what it staged discarded, never
/// ready.
#[test]
fn a_publication_never_answered_refuses_the_preparation() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        rig.render
            .set_timeouts(Duration::from_secs(1), Duration::from_secs(5));
        rig.render.hold_chart_publications(true);
        let mut h = begin(rig, &l);
        let ended = polled(rig, &mut h).unwrap();
        rig.render.hold_chart_publications(false);
        assert!(
            matches!(&ended, Preparation::Refused(r) if r.reason.contains("images")),
            "{ended:?}"
        );
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
    });
}

/// A quit while the images are read cancels the preparation: never ready.
#[test]
fn a_quit_while_the_images_are_read_cancels() {
    let _serial = imports::serial();
    let (_dir, l) = chart_with_stage();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        let _let_go = LetGo;
        rig.play_chart = Some(2_048);
        HOLD.store(true, Ordering::Release);
        let controls = rig.controls;
        // The direct launch decodes in the facts stage, from the begin on.
        let ended = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                controls.command(game::Command::Quit);
            });
            let mut h = begin(rig, &l);
            polled(rig, &mut h).unwrap()
        });
        assert_eq!(ended, Preparation::Cancelled);
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
    });
}

/// RC2, RX6, RX6a (RT24's core): the result's chart facts are the
/// attempt's resolved availability, taken in the facts stage before the
/// result's image stage; a retry after the attempt drew them renews the
/// generation, so a stage file deleted meanwhile is resolved absent, and
/// one added again present.
#[test]
fn the_result_facts_are_each_attempts_images() {
    let _serial = imports::serial();
    let (dir, l) = chart_with_stage();
    let screens = Installers::default();
    on_rig(l.chart(), &screens, false, |rig| {
        rig.play_chart = Some(2_048);
        let images = |rig: &mut crate::play::PlayerRig<'_>| {
            let mut h = begin(rig, &l);
            assert_eq!(polled(rig, &mut h).unwrap(), Preparation::Ready);
            rig.result_chart.unwrap().images
        };
        assert!(images(rig).stage);
        // Drawn: the next attempt re-resolves the file.
        rig.chart_images.committed();
        std::fs::remove_file(dir.path().join("s.png")).unwrap();
        assert!(!images(rig).stage, "the deleted file's facts kept");
        rig.chart_images.committed();
        let file = std::fs::File::create(dir.path().join("s.png")).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), 64, 48);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let texels = [1, 2, 3, 255].repeat(64 * 48);
        e.write_header().unwrap().write_image_data(&texels).unwrap();
        assert!(images(rig).stage);
    });
}
