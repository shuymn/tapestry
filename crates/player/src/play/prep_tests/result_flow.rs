//! The chart's images on the result across attempts, through the
//! player's flow on WARP:
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
pub(super) const RED: [u8; 3] = [200, 30, 30];
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
    on_warp_charged(chart, coord, bundle(), Vec::new(), 0, f)
}

/// [`on_warp`] with `play` chosen, each bundle of `stated` charged its
/// level-0 texels once committed (`Renderer::state_charge`), so the shared
/// limit leaves the other kinds only what is past it, and the first
/// `pools_refused` chart-image pool arrays refused
/// (`Renderer::refuse_next_pools`).
fn on_warp_charged<R>(
    chart: &bms_model::Chart,
    coord: bool,
    play: Arc<render::SkinBundle>,
    stated: Vec<(Arc<render::SkinBundle>, u64)>,
    pools_refused: u32,
    f: impl FnOnce(&mut PlayerRig<'_>, &Seen) -> R,
) -> R {
    let seen = Seen::default();
    let make: MakeScreen = Box::new(move |_| {
        let mut r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        for (b, texels) in &stated {
            r.state_charge(b, *texels);
        }
        r.refuse_next_pools(pools_refused);
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
        render.select(ScreenKind::Play, Some(play)).wait().unwrap(),
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
/// `dir/name`, a `side`×`side` PNG of `rgb`.
fn png_sized(dir: &Path, name: &str, rgb: [u8; 3], side: u32) {
    let file = std::fs::File::create(dir.join(name)).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), side, side);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let [r, g, b] = rgb;
    let texels = [r, g, b, 255].repeat(usize::try_from(side * side).unwrap());
    e.write_header().unwrap().write_image_data(&texels).unwrap();
}

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
pub(super) fn chart(rgb: Option<[u8; 3]>) -> (tempfile::TempDir, game::Loaded) {
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
pub(super) fn fixed_skin() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    fixed_skin_with("")
}

/// [`fixed_skin`] running `pre` (its timing, say) before its destinations.
pub(super) fn fixed_skin_with(pre: &str) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let d = lua_result(pre, &format!("d[#d + 1] = {CHART_DST}"));
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
        crate::imports::SkinSession::default(),
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
pub(super) fn prepared(rig: &mut PlayerRig<'_>, loaded: &game::Loaded) {
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

/// A fixed result skin over two attempts on charts A and
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

/// A pending Lua result skin whose import reads
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

/// V4 (#611 S2, design.md "beatoraja Lua Skins", the result skin): a
/// pending result skin that reads a result fact (154) and draws an image
/// of its own only through a `draw` function the runtime runs, at the
/// mark's place. The attempt's image stage packs that image's cell, so the
/// result shows it drawn with no atlas built and no texture array
/// uploaded after the commit's.
#[test]
fn a_result_drawn_by_its_runtime_builds_and_uploads_nothing() {
    let _serial = imports::serial();
    let (dir, l) = chart(Some(RED));
    let skin = lua_result(
        "local nr = ms.number(154)\n\
         local calls = 0\n\
         local function rt() calls = calls + 1 return calls > 0 end\n\
         skin.image[#skin.image + 1] = { id = \"jd\", src = \"m\", x = 8, y = 0, w = 8, h = 8 }",
        "d[#d + 1] = { id = \"jd\", draw = rt, dst = { { x = 1200, y = 520, w = 40, h = 40 } } }",
    );
    let p = result::pending(skin.path(), result::LIMITS);
    on_warp(l.chart(), false, |rig, seen| {
        // The render thread's runtime cell, as the player gives it, so the
        // layout's runtime draws.
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))))
            .with_scripts(rig.render.scripts().clone());
        prepared(rig, &l);
        assert!(
            matches!(rig.result_skin.upload(), Some(Upload::Pages(_))),
            "the probe's pages uploaded at the commit"
        );
        let (b0, u0) = (crate::result_skin::builds(), crate::dev::result_uploads());
        let drawn = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
        assert!(
            drawn.is_some_and(|[_, m]| near(m, MARK)),
            "the runtime's image drawn: {drawn:?}"
        );
        assert_eq!(crate::result_skin::builds() - b0, 0, "no atlas built");
        assert_eq!(
            crate::dev::result_uploads() - u0,
            0,
            "no texture array uploaded"
        );
    });
}

/// A pending skin reading 154 that adds a 2048-texel image drawn only by a
/// `draw` function the runtime runs while it reads 1, the probe's value:
/// the kept probe holds that image, the plain one does not, and the view
/// (154 is 0 in the view shown) draws the mark. With the level-0 texels of
/// its kept and its plain probe.
fn kept_only_skin() -> (
    tempfile::TempDir,
    Arc<crate::result_skin::PendingResult>,
    u64,
    u64,
) {
    let skin = lua_result(
        "local nr = ms.number(154)\n\
         if nr == 1 then\n\
           local calls = 0\n\
           local function rt() calls = calls + 1 return calls > 0 end\n\
           skin.source[#skin.source + 1] = { id = \"b\", path = \"b.png\" }\n\
           skin.image[#skin.image + 1] = { id = \"big\", src = \"b\", x = 0, y = 0, w = 2048, h = 2048 }\n\
           d[#d + 1] = { id = \"big\", draw = rt, dst = { { x = 0, y = 600, w = 8, h = 8 } } }\n\
         end",
        &format!("d[#d + 1] = {MARK_DST}"),
    );
    png_sized(skin.path(), "b.png", BLUE, 2_048);
    let p = result::pending(skin.path(), result::LIMITS);
    let probe = |kept: bool| {
        let mut rs = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
        rs.set_kept_probe(kept);
        let _ = rs.image_stage(result::chart(), None, || false, || false);
        render::pages_texels(rs.image().expect("prepared").pages())
    };
    let (kept, plain) = (probe(true), probe(false));
    assert!(plain < kept, "the kept probe {kept}, the plain one {plain}");
    (skin, p, kept, plain)
}

/// The selection entries a test makes until one finds the frame
/// interval's allowance free for the pool's regrowth.
const ENTRIES: u64 = 4;

/// A selection installation charged so the kept probe's gate closes: past
/// what leaves room beside it for the largest play and result charges.
const MENUS: u64 = 400 << 20;

/// The pages the attempt's result uploads, and their level-0 texels.
fn uploaded(rig: &PlayerRig<'_>) -> (skin_model::AtlasId, u64) {
    match rig.result_skin.upload() {
        Some(Upload::Pages(pages)) => (pages.id(), render::pages_texels(&pages)),
        other => panic!("not pages: {other:?}"),
    }
}

/// The kept probe is used only where the shared limit cannot refuse it
/// (#611 PR 3a; design.md "beatoraja Lua Skins", the result skin): a
/// selection charged so the gate closes, and the play skin charged so the
/// room left fits the plain probe but is one texel short of the kept one,
/// whose cell only the runtime draws. The probe is plain, admitted at the
/// commit, and the view draws with its skin; the kept probe would be
/// refused and the attempt shown built-in.
#[test]
fn a_kept_probe_past_the_room_leaves_the_plain_one() {
    let _serial = imports::serial();
    assert!(!crate::result_skin::kept_probe_fits(MENUS));
    let (dir, l) = chart(Some(RED));
    let (_skin, p, kept, plain) = kept_only_skin();
    let play = bundle();
    let sel = bundle();
    let charge = render::SHARED_TEXELS - MENUS - (kept - 1);
    on_warp_charged(
        l.chart(),
        false,
        Arc::clone(&play),
        vec![(play, charge), (Arc::clone(&sel), MENUS)],
        0,
        |rig, seen| {
            assert_eq!(
                rig.render
                    .select(ScreenKind::Selection, Some(sel))
                    .wait()
                    .unwrap(),
                Preparation::Ready
            );
            rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
            let mut h = begin(rig, &l);
            readied(rig, &mut h);
            assert_eq!(uploaded(rig).1, plain, "the plain probe's pages");
            rig.commit_attempt().unwrap();
            assert!(
                rig.presenter.attempt_fallen().is_none(),
                "the probe's pages refused"
            );
            let drawn = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
            assert!(
                drawn.is_some_and(|[_, m]| near(m, MARK)),
                "the view drawn with its skin: {drawn:?}"
            );
        },
    );
}

/// The gate counts the selection's pool firm layer whether the pool has
/// it at the report or not (design.md "beatoraja Lua Skins", the result
/// skin): a chart-image selection committed with its pool refused, charged
/// so the gate opens without the firm layer and closes with it, then a
/// selection entry that makes the pool again with no new report. With the
/// play skin charged so the room left fits the plain probe but is one
/// texel short of the kept one, the probe is plain and the view draws
/// with its skin; a gate that left the layer out would pack the kept
/// probe, which the commit refuses.
#[test]
fn a_pool_made_again_after_the_report_leaves_the_plain_probe() {
    use crate::result_skin::kept_probe_fits;
    let _serial = imports::serial();
    let (dir, l) = chart(Some(RED));
    let (_skin, p, kept, plain) = kept_only_skin();
    let fixture = crate::selection_skin::chart_fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let (sel, script) =
        crate::selection_skin::fixture_scripted(limits, fixture.path(), "chart.luaskin");
    let side = u64::from(sel.side().max(skin_model::MIN_TEXT_SIDE));
    let firm = side * side;
    let room = render::SHARED_TEXELS - 2 * (skin_model::MAX_ATLAS_TEXELS + render::VM_TEXELS);
    let menus = room - firm / 2;
    assert!(kept_probe_fits(menus) && !kept_probe_fits(menus + firm));
    let play = bundle();
    let charge = render::SHARED_TEXELS - menus - firm - (kept - 1);
    on_warp_charged(
        l.chart(),
        false,
        Arc::clone(&play),
        vec![(play, charge), (Arc::clone(&sel), menus)],
        2,
        |rig, seen| {
            assert_eq!(
                rig.render
                    .select_with(ScreenKind::Selection, Some(sel), script)
                    .wait()
                    .unwrap(),
                Preparation::Ready
            );
            assert!(rig.render.pool_now().0.is_none(), "the pool refused");
            assert_eq!(
                rig.render.result_status().menus,
                menus + firm,
                "the firm layer counted without the pool"
            );
            // A selection entry's regrowth runs only while the frame
            // interval's allowance is free, else it waits for the next
            // entry (design.md "Memory Budget"): the commit's pool tries
            // and the replaced play's destruction spend it, so entries are
            // made until one finds it free, each with no new report.
            let made = |rig: &PlayerRig<'_>| {
                rig.render
                    .pool_now()
                    .0
                    .is_some_and(|pool| u64::from(pool.side) == side)
            };
            let entered = (1..=ENTRIES).any(|lifetime| {
                rig.render.enter_selection(lifetime);
                rig.render
                    .show_selection(crate::render_side::tests::selection_update(lifetime, 1));
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !made(rig) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let now = made(rig);
                rig.render.leave_selection();
                assert!(rig.render.fence(std::time::Duration::from_secs(60)));
                now
            });
            assert!(entered, "the pool made again at an entry");
            rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
            let mut h = begin(rig, &l);
            readied(rig, &mut h);
            assert_eq!(uploaded(rig).1, plain, "the plain probe's pages");
            rig.commit_attempt().unwrap();
            assert!(
                rig.presenter.attempt_fallen().is_none(),
                "the probe's pages refused"
            );
            let drawn = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
            assert!(
                drawn.is_some_and(|[_, m]| near(m, MARK)),
                "the view drawn with its skin: {drawn:?}"
            );
        },
    );
}

/// The gate is decided again at an installation change (design.md
/// "beatoraja Lua Skins", the result skin): attempt 1, with nothing charged
/// beside play, packs the kept probe; a selection then committed past the
/// gate's room closes it, and attempt 2 drops the kept probe and packs the
/// plain one rather than reuse it.
#[test]
fn a_selection_past_the_room_closes_the_gate() {
    let _serial = imports::serial();
    let (dir, l) = chart(Some(RED));
    let (_skin, p, kept, plain) = kept_only_skin();
    let sel = bundle();
    on_warp_charged(
        l.chart(),
        false,
        bundle(),
        vec![(Arc::clone(&sel), MENUS)],
        0,
        |rig, seen| {
            rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
            prepared(rig, &l);
            let (first, texels) = uploaded(rig);
            assert_eq!(texels, kept, "attempt 1's kept probe");
            let drawn = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
            assert!(drawn.is_some_and(|[_, m]| near(m, MARK)), "{drawn:?}");
            assert_eq!(
                rig.render
                    .select(ScreenKind::Selection, Some(sel))
                    .wait()
                    .unwrap(),
                Preparation::Ready
            );
            assert_eq!(rig.render.result_status().menus, MENUS);
            prepared(rig, &l);
            let (second, texels) = uploaded(rig);
            assert_eq!(texels, plain, "attempt 2's plain probe");
            assert_ne!(second, first, "the kept probe not reused");
        },
    );
}

/// A pending skin reading 191 with the
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

/// The transition probe (`TAPESTRY_LOOP_PROBE`) on, over two attempts
/// whose result layouts differ (chart A's 190 branch, chart B's stage
/// file): each of its marks at its event.
#[test]
fn the_transition_probe_marks_each_event() {
    let _serial = imports::serial();
    crate::dev::loop_probe::tests::turn_on();
    let (a_dir, a) = chart(None);
    let (b_dir, b) = chart(Some(GREEN));
    let (_skin, p) = skin_on_191();
    on_warp(a.chart(), true, |rig, seen| {
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))));
        prepared(rig, &a);
        let first = shown(rig, seen, a_dir.path(), |_, m| near(m, MARK));
        assert!(first.is_some_and(|[_, m]| near(m, MARK)), "{first:?}");
        prepared(rig, &b);
        // Frames of another picture between B's upload and its result's
        // first frame, each drawing a sprite (a built-in page's cursor bar)
        // of another array: the first use is the result's, not theirs.
        let at = probe_now();
        rig.render.show_decision(
            crate::menu_screen::MenuPage {
                lines: vec![crate::menu_screen::MenuLine {
                    text: "between".to_owned(),
                    style: crate::menu_screen::LineStyle::Cursor,
                }],
                caret: None,
            },
            None,
        );
        let end = Instant::now() + Duration::from_secs(10);
        while presents_after(at) < 3 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(presents_after(at) >= 3, "no frame between");
        let second = shown(rig, seen, b_dir.path(), |c, _| near(c, GREEN));
        assert!(second.is_some_and(|[c, _]| near(c, GREEN)), "{second:?}");
    });
    let marks = crate::dev::loop_probe::tests::marks();
    assert_marks(&marks);
}

/// The probe's time of its last mark so far.
fn probe_now() -> u64 {
    crate::dev::loop_probe::tests::marks()
        .last()
        .map_or(0, |m| m.0)
}

/// The frames presented after the probe's time `at`.
fn presents_after(at: u64) -> usize {
    crate::dev::loop_probe::tests::marks()
        .iter()
        .filter(|m| m.1 == "present" && m.0 > at)
        .count()
}

/// [`the_transition_probe_marks_each_event`]'s marks, each at its event.
fn assert_marks(marks: &[crate::dev::loop_probe::Mark]) {
    // Each mark's times and values, by tag.
    let times =
        |tag: &str| -> Vec<u64> { marks.iter().filter(|m| m.1 == tag).map(|m| m.0).collect() };
    let values =
        |tag: &str| -> Vec<i64> { marks.iter().filter(|m| m.1 == tag).map(|m| m.2).collect() };
    let one = |tag: &str, k: usize| -> u64 {
        *times(tag)
            .get(k)
            .unwrap_or_else(|| panic!("{tag} #{k}: {:?}", times(tag)))
    };
    // The values of `tag` marked in (from, to).
    let within = |tag: &str, from: u64, to: u64| -> Vec<i64> {
        marks
            .iter()
            .filter(|m| m.1 == tag && m.0 > from && m.0 < to)
            .map(|m| m.2)
            .collect()
    };
    // Two attempts: each one's steps in order, from its preparation to the
    // first draw of its result's array.
    let steps = [
        "prep:begin",
        "img:begin",
        "img:import_end",
        "img:stage_end",
        "att:send",
        "att:stage_us",
        "att:committed",
        "res:select_sent",
        "res:select_ready",
        "res:install_us",
        "fu:submit_us",
    ];
    for tag in steps {
        assert_eq!(times(tag).len(), 2, "{tag}: {:?}", times(tag));
    }
    for k in 0..2 {
        for w in steps.windows(2) {
            assert!(
                one(w[0], k) <= one(w[1], k),
                "attempt {k}: {} after {}",
                w[0],
                w[1]
            );
        }
    }
    // The preparations' numbers, and both image stages ended Ready.
    assert_eq!(values("prep:begin"), [0, 1]);
    assert_eq!(values("img:stage_end"), [1, 1]);
    // Each attempt's result upload: inside its select (the play skin's own
    // select, at the rig's start, is upload 0), its levels' writes, made in
    // the uploader's steps after its creation and before its answer
    // (design.md "Load Pipeline"), summing to its pages' bytes with the
    // built-in layer's 128² texels, its pages the bundle's; none refused.
    let begins = times("up:selskin:begin");
    assert_eq!(begins.len(), 3, "{begins:?}");
    for k in 0..2 {
        let (sent, ready) = (one("res:select_sent", k), one("res:select_ready", k));
        let t0 = one("up:selskin:t0", k + 1);
        let begin = one("up:selskin:begin", k + 1);
        assert!(sent < t0 && begin < ready, "upload {k} outside its select");
        let written: i64 = within("wt:selskin:bytes", t0, ready).iter().sum();
        let raw = values("up:selskin:raw_bytes")[k + 1];
        assert_eq!(written + 128 * 128 * 4, raw, "upload {k}'s writes");
        assert_eq!(values("res:bundle_level_bytes")[k] + 128 * 128 * 4, raw);
        assert_eq!(
            values("up:selskin:pages")[k + 1],
            values("res:bundle_pages")[k]
        );
        assert_eq!(values("up:selskin:refused")[k + 1], 0);
        assert!(
            within("up:selskin:create_us", t0, ready)
                .iter()
                .all(|&v| v >= 0)
        );
    }
    // Each upload's figures are marked once it was waited for, so no probe
    // work overlaps the GPU's copy its wait times. The result uploads are
    // written in the uploader's steps, which add no wait (design.md "Load
    // Pipeline"): they record a wait of none and no wait marks. Only the
    // play skin's (upload 0) waits for the GPU: in the marks' order (one
    // thread's marks keep its order), its wait begins and ends, then its
    // figures and its wait's time are marked, before the next upload's.
    let order = |tag: &str| -> Vec<usize> {
        marks
            .iter()
            .enumerate()
            .filter(|(_, m)| m.1 == tag)
            .map(|(i, _)| i)
            .collect()
    };
    let [starts, ends, figures, waited] = [
        "up:selskin:wait_begin",
        "up:selskin:wait_end",
        "up:selskin:begin",
        "up:selskin:gpu_wait_us",
    ]
    .map(order);
    assert_eq!((starts.len(), ends.len()), (1, 1), "{starts:?} {ends:?}");
    for o in [&figures, &waited] {
        assert_eq!(o.len(), begins.len(), "{o:?}");
    }
    let chain: Vec<usize> = starts
        .iter()
        .chain(&ends)
        .copied()
        .chain(figures.iter().zip(&waited).flat_map(|(&f, &w)| [f, w]))
        .collect();
    assert!(
        chain.is_sorted(),
        "an upload's figures outside its wait: {chain:?}"
    );
    let waits = values("up:selskin:gpu_wait_us");
    assert_eq!(waits.get(1..), Some(&[0, 0][..]), "{waits:?}");
    // The first result's installation: none before (0); the second's: one
    // installed, another asked for (2).
    assert_eq!(values("res:installed"), [0, 2]);
    // Two installations' resources destroyed, each at a frame boundary
    // with the memory read around it: play's built-in ones, which the play
    // skin's commit replaced, before the first result (READY waits for
    // them); and A's, which B's layout replaces, once B's upload is done
    // (not inside its select).
    assert_eq!(values("dr:site"), [1, 1]);
    assert_eq!(values("dr:kind"), [2, 2]);
    let (play, replaced) = (one("dr:site", 0), one("dr:site", 1));
    assert!(one("up:selskin:begin", 0) < play && play < one("res:select_sent", 0));
    assert!(one("up:selskin:begin", 2) < replaced);
    assert!(
        values("dr:l0")
            .iter()
            .chain(&values("dr:l1"))
            .all(|&v| v > 0)
    );
    // The first use: one per result upload, in its first result frame, with
    // its frames' submits marked after it (the drop's included).
    for k in 0..2 {
        let fu = one("fu:submit_us", k);
        let frame = within("rf:frame_left", one("res:install_us", k), fu);
        assert_eq!(frame, [2], "attempt {k}'s first result frame");
        assert_eq!(
            within("pd:submit_us", fu - 1, fu + 1_000_000).first(),
            values("fu:submit_us").get(k)
        );
    }
    assert_eq!(times("fu:gpu_done_us").len(), 2);
    // Each first use credited to the result's upload (the selection skin
    // caller), and its frame the first probed: the result's frames are
    // counted from the view's commit, not from its installation.
    assert_eq!(
        values("fu:caller"),
        [i64::from(render::probe::SELECT_SKIN); 2]
    );
    for k in 0..2 {
        let fu = one("fu:submit_us", k);
        let (at, left) = marks
            .iter()
            .find(|m| m.1 == "rf:frame_left" && m.0 > one("res:install_us", k))
            .map(|m| (m.0, m.2))
            .unwrap();
        assert_eq!(left, 2, "attempt {k}'s first probed frame");
        assert!(
            within("present", at, fu).is_empty(),
            "attempt {k}: a frame presented between the first probed and the first use"
        );
    }
    // Frames were presented between B's upload and its result: none of
    // them was the first use.
    let between = within(
        "present",
        one("res:select_ready", 1),
        one("res:install_us", 1),
    );
    assert!(between.len() >= 3, "{between:?}");
    // The result frames probed are presented.
    assert!(values("rf:presented").iter().all(|&v| v == 1));
    // B's chart images, written for the result's view inside its staging,
    // its stage file a band a step.
    let (install, chart) = (one("res:install_us", 1), one("res:chart_us", 1));
    let images = within("ci:result:image", install, chart);
    assert!(
        !images.is_empty() && images.iter().all(|&i| i == 0),
        "{images:?}"
    );
    assert!(
        within("ci:result:bytes", install, chart)
            .iter()
            .all(|&b| b > 0)
    );
    // Nothing lost for want of room.
    assert_eq!(crate::dev::loop_probe::tests::dropped(), 0);
}

/// §5.1's step 3 (design.md "Memory Budget", the result pages upload):
/// the attempt's own pages ticket is begun after its PrepareAttempt, its
/// array uploaded before the commit, and the commit waits for its
/// publication instead of selecting the pages; the result draws on them.
/// A retry whose pages are installed begins no ticket and uploads
/// nothing (step 5, V10g).
#[test]
fn the_attempts_pages_are_its_own_ticket_and_a_retry_sends_nothing() {
    let _serial = imports::serial();
    let (dir, l) = chart(Some(RED));
    // A pending skin reading a result fact: its layout is the view's, so
    // the attempt uploads its pages.
    let skin = lua_result(
        "local nr = ms.number(154)",
        &format!("d[#d + 1] = {MARK_DST}"),
    );
    let p = result::pending(skin.path(), result::LIMITS);
    on_warp(l.chart(), true, |rig, seen| {
        rig.result_skin = ResultSkin::new(Some(ResultStart::Pending(Arc::clone(&p))))
            .with_scripts(rig.render.scripts().clone());
        let u0 = crate::dev::result_uploads();
        let mut h = begin(rig, &l);
        readied(rig, &mut h);
        assert!(rig.pages_ticket.is_some(), "begun with the preparation");
        assert_eq!(
            crate::dev::result_uploads() - u0,
            1,
            "its array uploaded before the commit"
        );
        rig.commit_attempt().unwrap();
        assert!(rig.pages_ticket.is_none(), "settled at the commit");
        let (atlas, _) = uploaded(rig);
        assert_eq!(rig.render.result_status().installed, Some(atlas));
        let first = shown(rig, seen, dir.path(), |_, m| near(m, MARK));
        assert!(
            first.is_some_and(|[_, m]| near(m, MARK)),
            "drawn: {first:?}"
        );
        // The retry: its pages installed, nothing begun or uploaded.
        let mut h = begin(rig, &l);
        readied(rig, &mut h);
        assert!(rig.pages_ticket.is_none(), "installed: no ticket");
        rig.commit_attempt().unwrap();
        assert_eq!(
            crate::dev::result_uploads() - u0,
            1,
            "nothing uploaded again"
        );
        assert_eq!(rig.render.result_status().installed, Some(atlas));
    });
}
