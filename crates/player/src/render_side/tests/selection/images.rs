//! The cursor's chart images on a real selection screen, through the
//! render thread on WARP (dev-tools): the selection skin keeps no chart
//! layer and draws the cursor's row from a slot of its chart-image pool.
//! An update naming a resident slot of the live generation draws it from
//! its first frame, with no write; a pending or absent row, a slot of
//! another generation or one not written yet draws nothing; the slot drawn
//! last is the render thread's pin; and a pool's generation ends with its
//! device, its skin or its refusal, the cursor's row read again into the
//! next.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use game::Preparation;
use skin_model::chart_image::POOL_LAYER;
use skin_model::{Quad, ScreenKind, SelectionPlace};

use super::super::wait_until;
use super::update_of;
use super::warp::{listed, lossy_spawn};
use crate::chart_currency::CursorChart;
use crate::chart_images::{ChartImageWorker, ChartImages, CursorId, EntryState};
use crate::render_side::{FrameSink, RenderLink, Screen};
use crate::selection_skin::{
    BANNER_AT, BANNER_DST, STAGE_AT, STAGE_DST, SelectionUpdate, chart_mark,
};

const RED: [u8; 3] = [200, 10, 10];
const GREEN: [u8; 3] = [10, 200, 10];

/// One selection frame as drawn: its quads, its device, the update it
/// applied (lifetime, revision), and the pixels at the stage file's and
/// the banner's points when sampled.
#[derive(Clone, Debug)]
struct Seen {
    quads: Vec<Quad>,
    device: u64,
    update: Option<(u64, u64)>,
    pixels: Option<[[u8; 3]; 2]>,
}

/// The selection frames drawn, each read back while `sample` is set.
#[derive(Clone, Default)]
struct Frames {
    seen: Arc<Mutex<Vec<Seen>>>,
    sample: Arc<AtomicBool>,
}

impl FrameSink for Frames {
    fn frame(&mut self, screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        let Some(quads) = screen.selection_quads() else {
            return;
        };
        let pixels = self
            .sample
            .load(Ordering::Acquire)
            .then(|| screen.read_back())
            .flatten()
            .map(|(w, _, px)| {
                [STAGE_AT, BANNER_AT].map(|[x, y]| {
                    let at = (y * usize::try_from(w).unwrap() + x) * 4;
                    [px[at], px[at + 1], px[at + 2]]
                })
            });
        self.seen.lock().unwrap().push(Seen {
            quads,
            device: screen.device(),
            update: drawn.selection,
            pixels,
        });
    }
}

impl Frames {
    /// Forgets the frames drawn so far.
    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Waits for `n` frames drawn since the last [`clear`](Self::clear)
    /// for which `on` holds, and returns them.
    fn next_of(&self, n: usize, on: impl Fn(&Seen) -> bool) -> Vec<Seen> {
        assert!(
            wait_until(|| self.seen.lock().unwrap().iter().filter(|s| on(s)).count() >= n),
            "no {n} selection frames"
        );
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| on(s))
            .cloned()
            .collect()
    }

    /// The first `n` frames of update `update` drawn since the last
    /// [`clear`](Self::clear).
    fn of(&self, n: usize, update: (u64, u64)) -> Vec<Seen> {
        self.next_of(n, |s| s.update == Some(update))
    }

    /// Waits for a frame drawn since the last [`clear`](Self::clear) for
    /// which `f` holds, and returns it.
    fn until(&self, f: impl Fn(&Seen) -> bool) -> Seen {
        let found = Mutex::new(None);
        let held = wait_until(|| {
            let s = self.seen.lock().unwrap().iter().find(|s| f(s)).cloned();
            let some = s.is_some();
            *found.lock().unwrap() = s;
            some
        });
        assert!(
            held,
            "no such frame: {:?}",
            self.seen.lock().unwrap().last()
        );
        found.into_inner().unwrap().unwrap()
    }
}

/// Whether `q` holds a quad on `rect`, rounded, drawn from the pool if
/// `pool` is given.
fn has(q: &[Quad], rect: [f32; 4], pool: bool) -> bool {
    q.iter()
        .any(|q| q.rect.map(f32::round) == rect && (!pool || q.layer & POOL_LAYER != 0))
}

/// Whether options 190 to 193 read `want` in every way the fixture reads
/// them: its `op`s and its callbacks.
fn reads(s: &Seen, want: [bool; 4]) -> bool {
    (0..3).all(|row| {
        want.iter()
            .enumerate()
            .all(|(i, w)| has(&s.quads, chart_mark(row, i), false) == *w)
    })
}

/// Whether a frame drew both images from the pool with 191 and 193
/// holding in its conditions and its callbacks, and 190 and 192 not.
fn draws_images(s: &Seen) -> bool {
    has(&s.quads, STAGE_DST, true)
        && has(&s.quads, BANNER_DST, true)
        && reads(s, [false, true, false, true])
}

/// Whether a frame drew neither image, with 190 and 192 holding and 191
/// and 193 not.
fn draws_none(s: &Seen) -> bool {
    !has(&s.quads, STAGE_DST, false)
        && !has(&s.quads, BANNER_DST, false)
        && reads(s, [true, false, true, false])
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

/// Update `revision` of lifetime `lifetime` listing all charts, its view
/// filtered by `search`, naming `chart` for the cursor.
fn update(lifetime: u64, revision: u64, search: &str, chart: CursorChart) -> SelectionUpdate {
    let mut u = update_of(lifetime, revision, listed(SelectionPlace::All, search));
    u.cursor_chart = chart;
    u
}

/// What [`rig`] gives: the fixture's folder, the link, its frames and the
/// screens' `lose` flag.
struct Rig {
    _dir: tempfile::TempDir,
    link: RenderLink,
    frames: Frames,
    lose: Arc<AtomicBool>,
}

impl Rig {
    /// The live pool as the render thread mirrored it.
    fn pool(&self) -> render::PoolLayout {
        self.link
            .pool_now()
            .0
            .expect("the selection skin has a pool")
    }
}

/// The chart fixture's selection skin, with its runtime, built for a WARP
/// target: it keeps no chart layer.
fn chart_skin() -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
    let dir = crate::selection_skin::chart_fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let (skin, script) =
        crate::selection_skin::fixture_scripted(limits, dir.path(), "chart.luaskin");
    assert!(
        skin.chart_layer().is_none(),
        "the selection keeps a chart layer"
    );
    (dir, skin, script.untimed())
}

/// A render thread on WARP with the chart fixture's selection skin
/// installed, its callbacks the runtime's, a selection shown, and a frame
/// of it drawn with the cursor's row pending; its pool mirrored.
fn rig_on(bad_month: &Arc<AtomicBool>) -> Rig {
    let (dir, skin, script) = chart_skin();
    let frames = Frames::default();
    let (link, lose, _) = lossy_spawn(None, bad_month, Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    link.enter_selection(1);
    link.show_selection(update(1, 1, "", CursorChart::Pending));
    frames.until(draws_none);
    assert!(wait_until(|| link.pool_now().0.is_some()));
    Rig {
        _dir: dir,
        link,
        frames,
        lose,
    }
}

fn rig() -> Rig {
    rig_on(&Arc::default())
}

/// Writes `set` into slot `slot` of pool generation `generation`, and the
/// answer.
fn write(
    link: &RenderLink,
    generation: u64,
    slot: u16,
    set: &Arc<ChartImages>,
) -> Result<(), render::PoolWriteError> {
    link.write_pool(generation, slot, Arc::clone(set))
        .recv_timeout(Duration::from_secs(30))
        .expect("answered")
}

/// An update naming a resident slot of the live generation draws both
/// images from its first frame, with no write since, their texels
/// sampled, and the render thread's pin is that slot (AC-S1, AC-S7); a
/// view update naming it still draws it from its first frame. An update
/// pending, naming a slot of another generation or absent draws nothing
/// from its first frame, and pins nothing (A9).
#[test]
fn a_resident_slot_draws_from_the_first_frame_of_its_update() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let pool = r.pool();
    let g = pool.generation;
    let (_images, set) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    assert_eq!(write(&r.link, g, 0, &set), Ok(()));
    let resident = CursorChart::Resident {
        generation: g,
        slot: 0,
    };
    r.frames.sample.store(true, Ordering::Release);
    r.frames.clear();
    r.link.show_selection(update(1, 2, "", resident));
    let first = &r.frames.of(1, (1, 2))[0];
    assert!(draws_images(first), "{first:?}");
    let drawn = r
        .frames
        .until(|s| s.update == Some((1, 2)) && s.pixels.is_some());
    let [stage, banner] = drawn.pixels.unwrap();
    assert!(near(stage, RED), "{stage:?}");
    assert!(near(banner, GREEN), "{banner:?}");
    r.frames.sample.store(false, Ordering::Release);
    assert_eq!(r.link.pool_now().1, CursorChart::pack(g, 0), "not pinned");
    // Another view (a panel opened) naming the same slot.
    r.link.show_selection(update(1, 3, "x", resident));
    assert!(r.frames.of(1, (1, 3)).iter().all(draws_images));
    // Pending, another generation's slot, absent: nothing from the first
    // frame, and no pin.
    let others = [
        CursorChart::Pending,
        CursorChart::Resident {
            generation: g + 1_000,
            slot: 0,
        },
        CursorChart::Absent,
    ];
    for (revision, chart) in (4..).zip(others) {
        r.link.show_selection(update(1, revision, "", chart));
        let seen = r.frames.of(3, (1, revision));
        assert!(seen.iter().all(draws_none), "{chart:?}: {:?}", seen[0]);
        assert_eq!(r.link.pool_now().1, 0, "{chart:?} pinned a slot");
    }
    r.link.exit();
}

/// The two channels in either order (A6, A9): an update naming a slot
/// whose write is not applied yet draws nothing, and draws the slot from
/// the first frame after the write lands, with no update since; a write
/// into another slot while one is drawn leaves the frames drawing it.
#[test]
fn an_update_may_name_its_slot_before_the_write_lands() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let pool = r.pool();
    let g = pool.generation;
    assert!(pool.slots >= 2, "{pool:?}");
    let (_both, both) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    let (_stage, stage_only) = crate::selection_skin::selection_images(pool.cells, GREEN, None);
    let chart = CursorChart::Resident {
        generation: g,
        slot: 1,
    };
    r.frames.clear();
    r.link.show_selection(update(1, 2, "", chart));
    assert!(r.frames.of(3, (1, 2)).iter().all(draws_none));
    assert_eq!(write(&r.link, g, 1, &both), Ok(()));
    r.frames.until(draws_images);
    // Slot 0 written while slot 1 is drawn.
    assert_eq!(write(&r.link, g, 0, &stage_only), Ok(()));
    assert!(r.link.fence(Duration::from_secs(60)));
    r.frames.clear();
    assert!(r.frames.of(3, (1, 2)).iter().all(draws_images));
    r.link.exit();
}

/// Across a play with the same pool (AC-S4's render half): selection left
/// and a chart's loading begun, a write is answered stale; selection
/// entered again, the pool's generation is the one before and an update
/// naming the slot written before draws it from its first frame.
#[test]
fn a_slot_written_before_a_play_draws_after_it() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let pool = r.pool();
    let g = pool.generation;
    let (_images, set) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    assert_eq!(write(&r.link, g, 0, &set), Ok(()));
    r.link.leave_selection();
    r.link
        .begin_loading(&crate::render_side::tests::chosen("A"), None);
    assert_eq!(
        write(&r.link, g, 1, &set),
        Err(render::PoolWriteError::Stale)
    );
    r.frames.clear();
    r.link.enter_selection(2);
    let resident = CursorChart::Resident {
        generation: g,
        slot: 0,
    };
    r.link.show_selection(update(2, 1, "", resident));
    let first = &r.frames.of(1, (2, 1))[0];
    assert!(draws_images(first), "{first:?}");
    assert_eq!(r.pool().generation, g, "the pool made again");
    r.link.exit();
}

/// The worker and the coordinator of a cursor on the select screen of
/// `link`.
fn cursor_worker(link: &RenderLink) -> (ChartImageWorker, game::coord::Coordinator) {
    let w = ChartImageWorker::new(link.chart_generation());
    let coord = game::coord::Coordinator::new(game::coord::Caps {
        foreground: 4,
        speculative: 1,
    });
    (w, coord)
}

/// The names [`crate::selection_skin::selection_images`] writes.
fn fixture_names() -> [library::Name; 2] {
    [
        library::Name::Named("s.png".to_owned()),
        library::Name::Named("n.png".to_owned()),
    ]
}

/// Steps `w` as the select screen does until `done`, at most 30 s; a read
/// asked again ([`ChartImageWorker::take_rearm`]) is asked by `rest`.
fn step_until(
    w: &ChartImageWorker,
    coord: &game::coord::Coordinator,
    link: &RenderLink,
    rest: &dyn Fn(),
    done: impl Fn(&ChartImageWorker) -> bool,
) {
    let end = std::time::Instant::now() + Duration::from_secs(30);
    while !done(w) {
        assert!(std::time::Instant::now() < end, "{w:?}");
        if w.take_rearm() {
            rest();
        }
        w.cursor_step(coord, game::coord::Spawner::SYSTEM, &link.chart_publisher());
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The cursor's row resident in pool generation `generation`, and nothing
/// running or waiting.
fn resident_in(w: &ChartImageWorker, generation: u64) -> bool {
    matches!(
        w.cursor_entry(),
        Some((EntryState::Resident { generation: g, .. }, ..)) if g == generation
    ) && w.idle()
        && !w.write_pending()
        && w.checks_pending() == (false, false)
}

/// The cursor rested on a chart: the worker reads its stage file and
/// banner into a slot and the next update names it, the frames drawing
/// both, sampled (SC1-SC3). The device lost (AC-S13): the generation
/// ends, the update's slot draws nothing on the rebuilt screen, and the
/// cursor's row, asked this rest, is read again into the new pool with no
/// move, the next update naming its new slot.
#[test]
fn a_lost_device_reads_the_cursors_row_into_the_next_pool() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let pool = r.pool();
    let (images, _) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    let (w, coord) = cursor_worker(&r.link);
    let row = CursorId {
        dir: images.path().to_path_buf(),
        names: fixture_names(),
    };
    let rest = || w.rest(w.cursor_request(row.dir.clone(), row.names.clone(), &coord));
    w.cursor_changed(&coord, Some(row.clone()));
    r.link
        .show_selection(update(1, 2, "", w.update_chart(false)));
    rest();
    step_until(&w, &coord, &r.link, &rest, |w| {
        resident_in(w, pool.generation)
    });
    let chart = w.update_chart(false);
    assert!(matches!(chart, CursorChart::Resident { .. }), "{chart:?}");
    r.frames.sample.store(true, Ordering::Release);
    r.frames.clear();
    r.link.show_selection(update(1, 3, "", chart));
    let drawn = r.frames.until(|s| draws_images(s) && s.pixels.is_some());
    let [stage, banner] = drawn.pixels.unwrap();
    assert!(near(stage, RED), "{stage:?}");
    assert!(near(banner, GREEN), "{banner:?}");
    r.frames.sample.store(false, Ordering::Release);
    let device = drawn.device;
    assert_eq!((w.begun(), w.counts().0), (1, 2));
    // The device lost: no pool until the game thread's next preparation
    // builds the screen again.
    r.lose.store(true, Ordering::Release);
    assert!(wait_until(|| r.link.failed()));
    assert!(wait_until(|| r.link.pool_now().0.is_none()));
    r.link.prepare().unwrap();
    r.link.show_selection(update(1, 4, "", chart));
    assert!(wait_until(|| r
        .link
        .pool_now()
        .0
        .is_some_and(|p| p.generation != pool.generation)));
    let next = r.pool().generation;
    r.frames.clear();
    let rebuilt = r.frames.until(|s| s.device != device).device;
    assert!(
        r.frames
            .next_of(3, |s| s.device == rebuilt)
            .iter()
            .all(draws_none)
    );
    step_until(&w, &coord, &r.link, &rest, |w| resident_in(w, next));
    assert_eq!((w.begun(), w.counts().0), (2, 4));
    let again = w.update_chart(false);
    assert!(
        matches!(again, CursorChart::Resident { generation, .. } if generation == next),
        "{again:?}"
    );
    r.link.show_selection(update(1, 5, "", again));
    r.frames.clear();
    r.frames
        .until(|s| s.device == rebuilt && s.update == Some((1, 5)) && draws_images(s));
    r.link.exit();
}

/// Another selection skin committed (AC-S13, the replacement): its pool
/// is a new generation, so an update naming a slot of the last one draws
/// nothing on it.
#[test]
fn a_replaced_selection_skin_has_a_new_pool_generation() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let r = rig();
    let pool = r.pool();
    let (_images, set) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    assert_eq!(write(&r.link, pool.generation, 0, &set), Ok(()));
    let chart = CursorChart::Resident {
        generation: pool.generation,
        slot: 0,
    };
    r.frames.clear();
    r.link.show_selection(update(1, 2, "", chart));
    r.frames.until(draws_images);
    let (_dir, skin, script) = chart_skin();
    assert_eq!(
        r.link
            .select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    r.link.show_selection(update(1, 3, "", chart));
    assert!(wait_until(|| r
        .link
        .pool_now()
        .0
        .is_some_and(|p| p.generation != pool.generation)));
    assert!(r.link.fence(Duration::from_secs(60)));
    r.frames.clear();
    r.link.show_selection(update(1, 4, "", chart));
    assert!(r.frames.of(3, (1, 4)).iter().all(draws_none));
    r.link.exit();
}

/// A selection skin refused at runtime leaves no installation and no
/// pool: the generation ends, no slot is pinned, and the worker begins no
/// job for a rest after it (SX18).
#[test]
fn a_refused_selection_skin_ends_its_pool() {
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let bad = Arc::new(AtomicBool::new(false));
    let r = rig_on(&bad);
    let pool = r.pool();
    let (images, _) = crate::selection_skin::selection_images(pool.cells, RED, Some(GREEN));
    let (w, coord) = cursor_worker(&r.link);
    let row = CursorId {
        dir: images.path().to_path_buf(),
        names: fixture_names(),
    };
    let rest = || w.rest(w.cursor_request(row.dir.clone(), row.names.clone(), &coord));
    w.cursor_changed(&coord, Some(row.clone()));
    rest();
    step_until(&w, &coord, &r.link, &rest, |w| {
        resident_in(w, pool.generation)
    });
    r.frames.clear();
    r.link
        .show_selection(update(1, 2, "", w.update_chart(false)));
    r.frames.until(draws_images);
    // Refused at runtime: its installation and pool removed.
    bad.store(true, Ordering::Release);
    assert!(wait_until(|| r.link.pool_now().0.is_none()));
    assert_eq!(r.link.pool_now().1, 0, "a slot still pinned");
    w.cursor_changed(&coord, None);
    w.cursor_changed(&coord, Some(row.clone()));
    rest();
    let begun = w.begun();
    for _ in 0..50 {
        w.cursor_step(
            &coord,
            game::coord::Spawner::SYSTEM,
            &r.link.chart_publisher(),
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(w.begun(), begun, "read with no pool");
    assert_eq!(w.writes_sent(), 1);
    assert!(
        w.cursor_entry().is_none(),
        "a resident entry outlived its pool"
    );
    r.link.exit();
}

/// Two bundles of `bundle`'s skin over one new atlas: the second staged
/// while the first is committed is a layout on its array.
fn same_array(bundle: &Arc<render::SkinBundle>) -> [Arc<render::SkinBundle>; 2] {
    let skin = bundle.skin();
    let variant = bundle.atlas_pages().variant();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let atlas = skin.atlas(variant, limits, &|| false).unwrap();
    let second = atlas
        .rebind_keeping(skin.sheet(), variant, skin.kept_layers())
        .unwrap();
    let of = |a| {
        Arc::new(
            render::SkinBundle::new(
                Arc::clone(skin),
                a,
                render::ClipTable::default(),
                skin_model::Side::Left,
            )
            .unwrap(),
        )
    };
    [of(atlas), of(second)]
}

/// With the transition probe on, a selection skin's first use (`fu:*`) is
/// the first frame that samples its array, though another bundle laid out
/// on that array, binding a pool of its own, was committed before any
/// frame drew.
#[test]
fn the_probe_marks_the_first_use_through_a_layout_on_the_same_array() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    turn_on();
    let count = |tag: &str| marks().iter().filter(|m| m.1 == tag).count();
    let (_dir, skin, script) = chart_skin();
    let (_other_dir, _, other_script) = chart_skin();
    let [first, second] = same_array(&skin);
    let frames = Frames::default();
    let (link, _, _) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(first), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    assert_eq!(count("up:selskin:total_us"), 1, "{:?}", marks());
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(second), other_script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    // A layout on the array, with a pool bound of its own: nothing
    // uploaded, nothing drawn yet.
    let paths: Vec<i64> = marks()
        .iter()
        .filter(|m| m.1 == "st:selskin:path")
        .map(|m| m.2)
        .collect();
    assert_eq!(paths, [2, 1], "own resources, then a layout");
    assert_eq!(count("up:selskin:total_us"), 1, "{:?}", marks());
    assert_eq!(count("fu:submit_us"), 0, "drawn before the selection");
    link.enter_selection(1);
    link.show_selection(update(1, 1, "", CursorChart::Pending));
    frames.until(draws_none);
    assert!(
        wait_until(|| count("fu:submit_us") == 1),
        "no first use of the array: {:?}",
        marks()
    );
    // Credited to the selection skin's upload it awaited.
    let caller: Vec<i64> = marks()
        .iter()
        .filter(|m| m.1 == "fu:caller")
        .map(|m| m.2)
        .collect();
    assert_eq!(caller, [i64::from(render::probe::SELECT_SKIN)]);
    link.exit();
}

/// With the transition probe on, a frame that draws no sprite does not
/// use the array it would sample: a selection skin whose song list and
/// image wait for the image callback's thirteenth call draws frames with
/// no quad first, and
/// its first use (`fu:*`) is the first frame that draws the image, after
/// every one of them was presented.
#[test]
fn the_probe_marks_the_first_use_at_the_first_frame_that_draws() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    turn_on();
    let dir = crate::selection_skin::lua_fixture();
    let lua = std::fs::read_to_string(dir.path().join("s.luaskin")).unwrap();
    let at = lua.find("skin.destination = {").unwrap();
    std::fs::write(
        dir.path().join("late.luaskin"),
        format!(
            "{}local calls = 0\n\
             skin.destination = {{\n\
               {{ id = \"list\", draw = function() return calls > 12 end, \
                 dst = {{ {{ x = 0, y = 0, w = 0, h = 0 }} }} }},\n\
               {{ id = \"b\", draw = function() calls = calls + 1; return calls > 12 end, \
                 dst = {{ {{ x = 100, y = 100, w = 8, h = 8 }} }} }} }}\n\
             return skin\n",
            &lua[..at]
        ),
    )
    .unwrap();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let (skin, script) =
        crate::selection_skin::fixture_scripted(limits, dir.path(), "late.luaskin");
    let frames = Frames::default();
    let (link, _, _) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script.untimed())
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let count = |tag: &str| marks().iter().filter(|m| m.1 == tag).count();
    assert_eq!(count("fu:submit_us"), 0, "drawn before the selection");
    let before = marks().last().map_or(0, |m| m.0);
    link.enter_selection(1);
    link.show_selection(update(1, 1, "", CursorChart::Pending));
    frames.until(|s| !s.quads.is_empty());
    assert!(
        wait_until(|| count("fu:submit_us") == 1),
        "no first use of the array: {:?}",
        marks()
    );
    let empty = frames
        .seen
        .lock()
        .unwrap()
        .iter()
        .take_while(|s| s.quads.is_empty())
        .count();
    assert!(empty > 0, "no frame without a quad");
    let all = marks();
    let fu = all.iter().find(|m| m.1 == "fu:submit_us").unwrap().0;
    let presented = all
        .iter()
        .filter(|m| m.1 == "present" && m.0 > before && m.0 < fu)
        .count();
    assert!(
        presented >= empty,
        "the first use before the frames without a quad: {presented} presented, {empty} empty"
    );
    link.exit();
}

/// With the transition probe on, an array dropped before any frame drew
/// it ends its wait, whatever drops it: a selection skin replaced by the
/// built-in selection before any frame is marked `fu:discarded`, with the
/// selection skin's caller.
#[test]
fn the_probe_marks_an_array_dropped_before_its_first_use() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    turn_on();
    let (_dir, skin, script) = chart_skin();
    let (link, _, _) = lossy_spawn(None, &Arc::default(), Box::new(Frames::default()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    assert!(link.select(ScreenKind::Selection, None).wait().is_ok());
    let ends = || -> Vec<(&'static str, i64)> {
        marks()
            .iter()
            .filter(|m| matches!(m.1, "fu:caller" | "fu:superseded" | "fu:discarded"))
            .map(|m| (m.1, m.2))
            .collect()
    };
    assert!(
        wait_until(|| !ends().is_empty()),
        "the dropped array's wait not ended: {:?}",
        marks()
    );
    let skin = i64::from(render::probe::SELECT_SKIN);
    assert_eq!(ends(), [("fu:discarded", skin)]);
    link.exit();
}

/// With the transition probe on, each array awaited for its first use
/// ends once: a selection skin replaced before any frame drew it has its
/// array's wait ended by the replacement's upload (`fu:superseded`), so the
/// first use the next frame marks is the replacement's (`fu:caller`), each
/// with the caller it was uploaded for.
#[test]
fn the_probe_ends_each_awaited_first_use_once() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    turn_on();
    let (_dir, skin, script) = chart_skin();
    let (_other_dir, other, other_script) = chart_skin();
    let frames = Frames::default();
    let (link, _, _) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    for (bundle, script) in [(skin, script), (other, other_script)] {
        assert_eq!(
            link.select_with(ScreenKind::Selection, Some(bundle), script)
                .wait()
                .unwrap(),
            Preparation::Ready
        );
    }
    let ends = || -> Vec<(&'static str, i64)> {
        marks()
            .iter()
            .filter(|m| matches!(m.1, "fu:caller" | "fu:superseded" | "fu:discarded"))
            .map(|m| (m.1, m.2))
            .collect()
    };
    let uploads = marks()
        .iter()
        .filter(|m| m.1 == "up:selskin:total_us")
        .count();
    assert_eq!(uploads, 2, "{:?}", marks());
    link.enter_selection(1);
    link.show_selection(update(1, 1, "", CursorChart::Pending));
    frames.until(draws_none);
    assert!(
        wait_until(|| ends().iter().any(|e| e.0 == "fu:caller")),
        "no first use: {:?}",
        marks()
    );
    let skin = i64::from(render::probe::SELECT_SKIN);
    assert_eq!(ends(), [("fu:superseded", skin), ("fu:caller", skin)]);
    link.exit();
}

/// With the transition probe on, the selection's chart-image pool leaves
/// each mark at its event: the first frame that draws the selection skin
/// uploaded last is its first use (`fu:*`), though its pool binds that
/// array again before the commit, for the first skin and for each
/// replacement; a replacement's commit retires the old pool, whose drop
/// lies inside the memory readings of the drop that takes it
/// (`dr:pool_us`); a refused skin's release times its pool beside its slot
/// (`rel:pool_us`), with no pages phase (`rel:pages_us` is a result's).
#[test]
fn the_probe_marks_the_selections_pool_at_its_events() {
    use crate::dev::loop_probe::tests::{marks, now, turn_on};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    turn_on();
    let after = |tag: &str, at: u64| -> Vec<u64> {
        marks()
            .iter()
            .filter(|m| m.1 == tag && m.0 >= at)
            .map(|m| m.0)
            .collect()
    };
    // The first use after the upload marked last, once a frame draws it.
    let first_use = || {
        let uploaded = marks()
            .iter()
            .filter(|m| m.1.starts_with("up:") && m.1.ends_with(":total_us"))
            .map(|m| m.0)
            .max()
            .expect("the selection's upload is marked");
        assert!(
            wait_until(|| !after("fu:submit_us", uploaded).is_empty()),
            "no first use after the upload: {:?}",
            marks()
        );
    };
    let bad = Arc::new(AtomicBool::new(false));
    let r = rig_on(&bad);
    assert!(r.pool().layers > 0, "no pool to bind");
    first_use();
    // A replacement retires the old pool, which the next staging drops
    // (this rig's screen drains nothing).
    let replaced = now();
    let mut dirs = Vec::new();
    for revision in [2, 3] {
        let (dir, skin, script) = chart_skin();
        dirs.push(dir);
        assert_eq!(
            r.link
                .select_with(ScreenKind::Selection, Some(skin), script)
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        r.link
            .show_selection(update(1, revision, "", CursorChart::Pending));
        first_use();
    }
    assert!(
        wait_until(|| !after("dr:pool_us", replaced).is_empty()
            && !after("dr:l1", after("dr:pool_us", replaced)[0]).is_empty()),
        "the retired pool's drop not marked: {:?}",
        marks()
    );
    // Inside one drop's readings: its first before the pool's drop, none
    // of its last between them.
    let dropped = after("dr:pool_us", replaced)[0];
    let opened = after("dr:l0", replaced)
        .into_iter()
        .filter(|&t| t <= dropped)
        .max();
    let opened =
        opened.unwrap_or_else(|| panic!("no reading before the pool's drop: {:?}", marks()));
    assert!(
        after("dr:l1", opened).iter().all(|&t| t >= dropped),
        "the pool dropped after its drop's last reading: {:?}",
        marks()
    );
    // A refusal releases the slot and its pool.
    let refused = now();
    bad.store(true, Ordering::Release);
    assert!(wait_until(|| r.link.pool_now().0.is_none()));
    assert!(
        wait_until(|| !after("rel:pool_us", refused).is_empty()),
        "{:?}",
        marks()
    );
    let [slot, pool, pages] =
        ["rel:slot_us", "rel:pool_us", "rel:pages_us"].map(|t| after(t, refused));
    assert_eq!(pool.len(), 1, "the pool's release: {:?}", marks());
    assert!(
        slot.len() == 1 && slot.first() <= pool.first(),
        "{:?}",
        marks()
    );
    // A selection's release has no pages of the result's to time.
    assert_eq!(pages, [], "{:?}", marks());
    r.link.exit();
}

/// A chart refused before selection's departure (a practice the session
/// refuses) enters selection again with no departure: the entry is a new
/// cursor generation whatever row the cursor is on, so with the cursor
/// moved meanwhile to the all-charts row, the new visit's frames draw
/// nothing of the last chart's images, and 190 and 192 hold, 191 and 193
/// not (SX17, through the player's selection on the render thread).
#[test]
fn an_entry_with_no_departure_draws_nothing_of_the_last_visit() {
    use game::AppRig as _;

    use crate::select::MenuKey;
    use crate::selection::image_tests::{library, look_until, past_rest, press, quiet, resident};
    let _warp = crate::warp_turn::take();
    let _serial = crate::imports::tests::serial();
    let (_skin, skin, script) = chart_skin();
    let frames = Frames::default();
    let (link, ..) = lossy_spawn(None, &Arc::default(), Box::new(frames.clone()));
    assert_eq!(
        link.select_with(ScreenKind::Selection, Some(skin), script)
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut sel, _) = library(
        dir.path(),
        &[("a.bms", Some("hidden-a.png"), Some("hidden-n.png"))],
    );
    quiet(&mut sel);
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    crate::tests::on_rig_with(settings, link, |rig, _| {
        rig.adjust = None;
        rig.selection = Some(sel);
        rig.selection_chart = true;
        rig.entered_selection();
        frames.until(draws_none);
        // The cursor rests on the chart: its images are drawn.
        press(rig, MenuKey::Down);
        assert!(rig.choose().is_none());
        past_rest();
        look_until(rig, resident);
        // The update naming its slot.
        look_until(rig, |w| {
            matches!(w.sent_chart(), Some(CursorChart::Resident { .. }))
        });
        frames.clear();
        let last = frames.until(draws_images);
        let (last, _) = last.update.expect("a selection frame's update");
        // Refused before the departure: the chart left and selection
        // entered again, the cursor on the all-charts row meanwhile.
        assert!(rig.selection.as_mut().unwrap().screen.move_to(0));
        rig.left_chart().unwrap();
        rig.entered_selection();
        for _ in 0..3 {
            assert!(rig.choose().is_none());
        }
        // The new visit's frames: one the render thread drew before it
        // took the entry is the last visit's, however late it lands.
        let seen = frames.next_of(3, |s| s.update.is_some_and(|(lifetime, _)| lifetime > last));
        let drew = seen.iter().find(|s| !draws_none(s));
        assert!(drew.is_none(), "{drew:?}");
    });
}
