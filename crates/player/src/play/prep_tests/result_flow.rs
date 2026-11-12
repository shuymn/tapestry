//! The chart's images on the result across attempts, through the
//! player's flow on WARP (S10-12 RC2, RC3, RC4; RX6, RX6a, RX11c, RX11e):
//! each attempt prepared (its facts stage, the result's image stage and
//! `send_screen`'s want), committed (the images handed to the result),
//! shown (`show_end`, the presenter's payload) and drawn by the render
//! thread, whose frames are read back at the chart image's place and at
//! the 190 branch's mark.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use game::Preparation;
use game::Rig as _;
use input::QpcClock;
use skin_model::ScreenKind;
use skin_model::chart_image::ChartImage;

use super::{LetGo, begin, imports, polled, result, settings, stage};
use crate::chart_images::HOLD;
use crate::menu_screen::MenuScreen;
use crate::play::PlayerRig;
use crate::render_side::tests::{bundle, measure, view};
use crate::render_side::{FrameSink, MakeScreen, RenderLink, Screen};
use crate::result_skin::{ResultSkin, ResultStart, Upload};

/// The colours of the stage files: chart A's, chart B's, and the one
/// written again after a deletion.
const RED: [u8; 3] = [200, 30, 30];
const GREEN: [u8; 3] = [30, 200, 30];
const BLUE: [u8; 3] = [30, 30, 200];
/// The skins' own image, the 190 branch's mark.
const MARK: [u8; 3] = [250, 250, 0];

/// The centre of the chart image's place (x 100, y 340, 400×400, whose
/// centre is the same with y up or down).
const CHART_AT: (u32, u32) = (300, 540);
/// The centre of the 190 branch's mark (x 1 200, y 520, 40×40).
const MARK_AT: (u32, u32) = (1_220, 540);

/// The chart image's destination in a Lua skin.
const CHART_DST: &str = "{ id = -100, dst = { { x = 100, y = 340, w = 400, h = 400 } } }";
/// The mark's destination in a Lua skin.
const MARK_DST: &str = "{ id = \"mark\", dst = { { x = 1200, y = 520, w = 40, h = 40 } } }";

/// What the frames read back at [`CHART_AT`] and [`MARK_AT`] while armed.
#[derive(Clone, Default)]
struct Seen {
    armed: Arc<AtomicBool>,
    last: Arc<Mutex<Option<[[u8; 3]; 2]>>>,
}

impl Seen {
    /// Reads back the frames from now on, the last reading dropped.
    fn arm(&self) {
        *self.last.lock().unwrap() = None;
        self.armed.store(true, Ordering::Release);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
    }

    fn last(&self) -> Option<[[u8; 3]; 2]> {
        *self.last.lock().unwrap()
    }
}

/// A frame sink reading back each frame drawn while armed that is not a
/// play's or a loading state's (a result's frame counts as a menu's).
struct Sampler(Seen);

impl FrameSink for Sampler {
    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        if !self.0.armed.load(Ordering::Acquire)
            || drawn.play.is_some()
            || drawn.loading_stamp.is_some()
        {
            return;
        }
        let Some((w, _, pixels)) = screen.read_back() else {
            return;
        };
        let at = |(x, y): (u32, u32)| crate::render_common::rgb_at(&pixels, w, (x, y));
        *self.0.last.lock().unwrap() = Some([at(CHART_AT), at(MARK_AT)]);
    }
}

/// Whether `a` is `b` within the blend's rounding.
fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 4)
}

/// `f` on a player's rig over a render thread on a WARP menu screen,
/// `chart` loaded and a play skin with no chart layer chosen, its frames
/// read back into the [`Seen`] given. With `coord`, the rig has a song
/// selection, whose coordinator runs the images' jobs, so the
/// preparation's polls wait for them in its facts stage; without, the
/// direct launch reads them on the game thread.
fn on_warp<R>(
    chart: &bms_model::Chart,
    coord: bool,
    f: impl FnOnce(&mut PlayerRig<'_>, &Seen) -> R,
) -> R {
    let seen = Seen::default();
    let make: MakeScreen = Box::new(|| {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        Ok(Box::new(MenuScreen::new(r, Some(render::text::Fonts::runtime()?))) as Box<dyn Screen>)
    });
    let clock = QpcClock::start();
    let render: RenderLink = crate::render_side::spawn(
        make,
        None,
        None,
        clock,
        view(),
        measure(clock),
        Box::new(Sampler(seen.clone())),
    )
    .unwrap();
    render.load(Arc::new(chart.clone()), None).unwrap();
    assert_eq!(
        render
            .select(ScreenKind::Play, Some(bundle()))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    // A failing body's panic is taken and raised again once the rig shut
    // down, so a failure ends the test rather than its rig's drop.
    let out = crate::tests::on_rig_with(settings(), render, |rig, _| {
        let _let_go = LetGo;
        let _images = LetImagesGo;
        rig.play_chart = None;
        let list = tempfile::tempdir().unwrap();
        if coord {
            rig.selection = Some(crate::selection::tests::tree(list.path(), &[&["b.bms"]]).0);
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(rig, &seen)))
    })
    .0;
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// Lets the images' reads go when dropped, a failing test's included.
struct LetImagesGo;

impl Drop for LetImagesGo {
    fn drop(&mut self) {
        HOLD.store(false, Ordering::Release);
    }
}

/// `dir/name`, a 64×48 PNG of `rgb`.
fn png(dir: &Path, name: &str, rgb: [u8; 3]) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 64, 48);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let [r, g, b] = rgb;
    let texels = [r, g, b, 255].repeat(64 * 48);
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

/// A chart naming the stage file `s.png`, written beside it in `rgb`
/// when `Some`.
fn chart(rgb: Option<[u8; 3]>) -> (tempfile::TempDir, game::Loaded) {
    let (dir, l) = crate::imports::facts::tests::loaded_kept("#STAGEFILE s.png\n", "tapestry");
    if let Some(rgb) = rgb {
        png(dir.path(), "s.png", rgb);
    }
    (dir, l)
}

/// A Lua result skin in `s.luaskin` running `pre` and placing `places`,
/// its mark image `m.png` of [`MARK`] beside it.
fn lua_result(pre: &str, places: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    png(d.path(), "m.png", MARK);
    std::fs::write(
        d.path().join("s.luaskin"),
        format!(
            "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
             if not skin_config then return skin end\n\
             local ms = require(\"main_state\")\n\
             skin.source = {{ {{ id = \"m\", path = \"m.png\" }} }}\n\
             skin.image = {{ {{ id = \"mark\", src = \"m\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
             skin.value = {{ {{ id = \"ex\", src = \"m\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                              digit = 4, ref = 71 }} }}\n\
             local d = {{ {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
                         {{ id = \"mark\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }} }}\n\
             {pre}\n\
             {places}\n\
             skin.destination = d\n\
             return skin\n"
        ),
    )
    .unwrap();
    d
}

/// A fixed result skin drawing the stage file: it reads no fact.
fn fixed_skin() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let d = lua_result("", &format!("d[#d + 1] = {CHART_DST}"));
    let choice = game::SkinChoice {
        root: d.path().to_owned(),
        entry: "s.luaskin".to_owned(),
        options: Default::default(),
        files: Default::default(),
    };
    let target = crate::imports::ImportTarget {
        limits: result::LIMITS,
        size: [1_920, 1_080],
        default_font: || Ok(crate::render_outline::default_font()),
    };
    match crate::imports::build(
        ScreenKind::Result,
        &choice,
        false,
        &target,
        &skin_beatoraja::Cancel::new(),
    ) {
        crate::imports::KindOutcome::Bundle(b, _, _) => (d, b),
        other => panic!("{other:?}"),
    }
}

/// A pending result skin whose import reads the stage file's
/// availability (191) to choose its layout: the stage file where it is
/// one, else the mark (the 190 branch).
fn skin_on_191() -> (tempfile::TempDir, Arc<crate::result_skin::PendingResult>) {
    let d = lua_result(
        "local stage = ms.option(191)",
        &format!("if stage then d[#d + 1] = {CHART_DST} else d[#d + 1] = {MARK_DST} end"),
    );
    let p = result::pending(d.path(), result::LIMITS);
    (d, p)
}

/// The bundle the attempt's result uploads for every view.
fn every(rig: &PlayerRig<'_>) -> Arc<render::SkinBundle> {
    match rig.result_skin.upload() {
        Some(Upload::Bundle(b)) => b,
        other => panic!("not a bundle for every view: {other:?}"),
    }
}

/// The colour of the first texel of the stage file `rig`'s result gives
/// a payload of `bundle` ([`ResultSkin::chart_for`]).
fn payload_stage(rig: &PlayerRig<'_>, bundle: &Arc<render::SkinBundle>) -> Option<[u8; 3]> {
    let set = rig.result_skin.chart_for(bundle)?;
    let f = set.read(ChartImage::Stage).image.as_ref().ok()?;
    let l = f.levels().first()?;
    Some([l[0], l[1], l[2]])
}

/// The preparation of `loaded` polled to its end, ready, and committed.
fn prepared(rig: &mut PlayerRig<'_>, loaded: &game::Loaded) {
    let mut h = begin(rig, loaded);
    readied(rig, &mut h);
    rig.commit_attempt().unwrap();
}

/// The preparation `h` polled to its end, ready, its final readiness
/// ready; not committed.
fn readied(rig: &mut PlayerRig<'_>, h: &mut game::PrepHandle) {
    assert_eq!(polled(rig, h).unwrap(), Preparation::Ready);
    assert!(matches!(
        rig.ready_attempt(1, h),
        Ok(game::Readiness::Ready)
    ));
}

/// `loaded`'s preparation begun with the images' read held: it stays in
/// the facts stage, the availability unresolved and no job of the
/// result's image stage begun (no import, no file of the skin read),
/// through polls of 50 ms; then the read is let go. The handle.
fn held_in_facts(
    rig: &mut PlayerRig<'_>,
    loaded: &game::Loaded,
    skin: &crate::result_skin::PendingResult,
) -> game::PrepHandle {
    HOLD.store(true, Ordering::Release);
    let (jobs, read) = (imports::jobs_counted(), skin.source_counts());
    let mut h = begin(rig, loaded);
    for _ in 0..5 {
        assert_eq!(rig.chart_images.facts(), None, "the facts while held");
        assert_eq!(stage(rig), "facts");
        let until = Instant::now() + Duration::from_millis(10);
        assert!(matches!(
            rig.poll_preparation(&mut h, until),
            Ok(game::PrepPoll::Pending)
        ));
    }
    assert_eq!(stage(rig), "facts");
    assert_eq!(imports::jobs_counted(), jobs, "an image stage's job");
    assert_eq!(skin.source_counts(), read, "the skin read");
    HOLD.store(false, Ordering::Release);
    h
}

/// The committed attempt's result shown (`show_end`, as the flow shows
/// it), then waited for until a frame read back satisfies `want` (the
/// chart image's place, the mark's), landing the result skin's layouts
/// meanwhile; the last reading. Bounded by 20 s.
fn shown(
    rig: &mut PlayerRig<'_>,
    seen: &Seen,
    dir: &Path,
    want: impl Fn([u8; 3], [u8; 3]) -> bool,
) -> Option<[[u8; 3]; 2]> {
    let report = crate::end_screen::tests::report(rules::tapestry());
    let screen = crate::end_screen::tests::screen(dir, &report);
    seen.arm();
    rig.show_end(&report, &screen);
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let last = seen.last();
        if last.is_some_and(|[c, m]| want(c, m)) || Instant::now() >= end {
            seen.disarm();
            rig.leave_result();
            return last;
        }
        rig.land_result();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// RT20 (RX11c): a fixed result skin over two attempts on charts A and
/// B, each with its own stage file: the second attempt's payload carries
/// B's images and its view draws B's stage file, not A's.
#[test]
fn a_fixed_result_draws_each_attempts_stage_file() {
    let _serial = imports::serial();
    let (a_dir, a) = chart(Some(RED));
    let (b_dir, b) = chart(Some(GREEN));
    let (_skin, fixed) = fixed_skin();
    assert!(
        fixed.chart_layer().is_some(),
        "the skin keeps a chart layer"
    );
    on_warp(a.chart(), false, |rig, seen| {
        rig.result_skin = ResultSkin::new(Some(ResultStart::Fixed(Arc::clone(&fixed), None)));
        prepared(rig, &a);
        let first = shown(rig, seen, a_dir.path(), |c, _| near(c, RED));
        assert!(
            first.is_some_and(|[c, _]| near(c, RED)),
            "A drawn: {first:?}"
        );
        assert_eq!(payload_stage(rig, &fixed), Some(RED));
        prepared(rig, &b);
        let second = shown(rig, seen, b_dir.path(), |c, _| near(c, GREEN));
        assert!(
            second.is_some_and(|[c, _]| near(c, GREEN)),
            "B drawn: {second:?}"
        );
        assert_eq!(payload_stage(rig, &fixed), Some(GREEN), "B's images");
    });
}

/// RT23 (RC2, RX6, RX11e): a pending Lua result skin whose import reads
/// the stage file's availability (191) to choose its layout, on charts A
/// (its stage file missing) and B (otherwise equal, with one). A's image
/// stage never begins while its facts are unresolved: held in the facts
/// stage, no import job and no file of the skin read, until the images'
/// read is let go. A draws the 190 branch. B's image stage does not reuse
/// A's layout: B's bundle for every view keeps a chart layer, the result
/// half is asked for its side, and B's view draws B's stage file.
#[test]
fn a_skin_reading_191_lays_out_each_charts_availability() {
    let _serial = imports::serial();
    let (a_dir, a) = chart(None);
    let (b_dir, b) = chart(Some(GREEN));
    let (_skin, p) = skin_on_191();
    on_warp(a.chart(), true, |rig, seen| {
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
        let mut h = held_in_facts(rig, &a, &p);
        readied(rig, &mut h);
        assert!(!rig.result_chart.unwrap().images.stage);
        rig.commit_attempt().unwrap();
        let on_a = every(rig);
        assert!(on_a.chart_layer().is_none(), "A's layout is the 190 branch");
        let first = shown(rig, seen, a_dir.path(), |_, m| near(m, MARK));
        assert!(
            first.is_some_and(|[c, m]| near(m, MARK) && !near(c, GREEN)),
            "A's 190 branch: {first:?}"
        );
        let mut h = begin(rig, &b);
        readied(rig, &mut h);
        let b_facts = rig.result_chart.unwrap().images;
        let on_b = every(rig);
        // What send_screen asked of the worker, before the commit takes it.
        let asked = (
            rig.result_skin.chart_side(),
            rig.chart_images.current_result(),
        );
        rig.commit_attempt().unwrap();
        assert!(!Arc::ptr_eq(&on_a, &on_b), "B reused A's layout");
        assert!(
            on_b.chart_layer().is_some(),
            "B's layout keeps a chart layer"
        );
        assert!(b_facts.stage, "B's facts: {b_facts:?}");
        let second = shown(rig, seen, b_dir.path(), |c, _| near(c, GREEN));
        assert!(
            second.is_some_and(|[c, m]| near(c, GREEN) && !near(m, MARK)),
            "B's stage file drawn: {second:?}"
        );
        assert_eq!(asked.0, Some(on_b.side()), "the result half's side");
        assert!(
            asked
                .1
                .is_some_and(|h| h.read(ChartImage::Stage).image.is_ok()),
            "the result half derived for B's bundle"
        );
        assert_eq!(payload_stage(rig, &on_b), Some(GREEN));
    });
}

/// RT24's cached layout (RC2, RX6a): a pending skin reading 191 with the
/// layout specialized on 191 cached; a retry after deleting the stage
/// file, and one after writing it again, each hold their facts
/// unresolved until the retry's own sources: the deleted case shows the
/// 190 branch (the cached 191 layout not reused), the added case the 191
/// branch drawing the stage file written again.
#[test]
fn a_retry_resolves_the_stage_file_again_for_the_cached_layout() {
    let _serial = imports::serial();
    let (dir, l) = chart(Some(RED));
    let (_skin, p) = skin_on_191();
    on_warp(l.chart(), true, |rig, seen| {
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
        prepared(rig, &l);
        let on_191 = every(rig);
        assert!(on_191.chart_layer().is_some());
        let first = shown(rig, seen, dir.path(), |c, _| near(c, RED));
        assert!(first.is_some_and(|[c, _]| near(c, RED)), "drawn: {first:?}");
        // Deleted: the retry's facts unresolved until its sources.
        std::fs::remove_file(dir.path().join("s.png")).unwrap();
        let mut h = held_in_facts(rig, &l, &p);
        readied(rig, &mut h);
        assert!(!rig.result_chart.unwrap().images.stage);
        rig.commit_attempt().unwrap();
        assert!(
            every(rig).chart_layer().is_none(),
            "the 190 branch's layout"
        );
        let deleted = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
        assert!(
            deleted.is_some_and(|[c, m]| near(m, MARK) && !near(c, RED)),
            "the 190 branch: {deleted:?}"
        );
        // Written again: the 191 branch, the new file drawn.
        png(dir.path(), "s.png", BLUE);
        let mut h = held_in_facts(rig, &l, &p);
        readied(rig, &mut h);
        assert!(rig.result_chart.unwrap().images.stage);
        rig.commit_attempt().unwrap();
        assert!(
            every(rig).chart_layer().is_some(),
            "the 191 branch's layout"
        );
        let added = shown(rig, seen, dir.path(), |c, _| near(c, BLUE));
        assert!(
            added.is_some_and(|[c, m]| near(c, BLUE) && !near(m, MARK)),
            "the stage file written again: {added:?}"
        );
    });
}
