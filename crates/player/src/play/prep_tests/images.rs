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
        let mut h = begin(rig, &l);
        let began = Instant::now();
        let ready = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                HOLD.store(false, Ordering::Release);
            });
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
            rig.chart_images.publish(&rig.render, None).is_some(),
            "the publication answered before readiness"
        );
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
        let mut h = begin(rig, &l);
        let controls = rig.controls;
        let ended = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                controls.command(game::Command::Quit);
            });
            polled(rig, &mut h).unwrap()
        });
        assert_eq!(ended, Preparation::Cancelled);
        assert!(rig.commit_attempt().is_err(), "nothing to commit");
    });
}
