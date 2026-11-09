//! A preparation handle's lifecycle on the player's rig (design.md "Play
//! Lifecycle", preparing each attempt): its replacement and discard
//! settled through the attempt-quit path, the run's end bounding that
//! settle ([`settle`]), and each stage's adapter, event by event
//! ([`adapter`]). Real jobs on self-made skins, held by the jobs' test
//! gates (the exit gate, the VM hooks), on a render thread of
//! [`Installers`]. A watchdog (`TAPESTRY_TEST_WATCHDOG`, seconds) panics a
//! wait that never ends.

use std::sync::Arc;
use std::time::{Duration, Instant};

use game::Rig as _;
use skin_beatoraja::{JudgeClass, vm_hooks};

use super::{PlayerRig, PrepStage};
use crate::imports::STOP_WAIT;
use crate::imports::facts::tests::loaded;
use crate::imports::tests as imports;
use crate::render_side::tests::{Installers, bundle};
use crate::result_skin::tests as result;
use crate::result_skin::{ResultSkin, ResultStart};

mod settle;

/// How long a test waits for what must come; `TAPESTRY_TEST_WATCHDOG`
/// (seconds) shortens it for mutation runs.
fn watchdog() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Waits until `done`, looking every 1 ms; panics past the watchdog,
/// which only catches what never happens.
#[track_caller]
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < watchdog(), "{what} never happened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// How long a held thread is kept past the settle's or the signal's
/// start: past the stop bound, so a wait bounded by it would have
/// returned before the release.
const PAST: Duration = Duration::from_millis(STOP_WAIT.as_millis() as u64 + 1_000);

/// How late past [`STOP_WAIT`] a wait that gives up at the bound may
/// return on a loaded machine.
const LATE: Duration = Duration::from_millis(750);

/// A play skin reading the play level: level 12 loops in metered Lua
/// until cancelled; any other level builds at once.
const LEVEL_SKIN: &str = "local lv = ms.number(96)\nif lv == 12 then while true do end end";

/// A result skin's script reading the play level: level 13 loops until
/// cancelled; any other level's probe ends at once.
const LEVEL_RESULT: &str = "local lv = ms.number(96)\nif lv == 13 then while true do end end";

/// A chart at play level `level`.
fn at_level(level: u32) -> game::Loaded {
    loaded(&format!("#PLAYLEVEL {level}\n"), "tapestry")
}

fn settings() -> game::Settings {
    game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    }
}

/// A render thread on `screens` with `chart` loaded and play's skin
/// marked pending, so each attempt stages its specialization, when
/// `pending`; a fixed play skin chosen otherwise.
fn render_for(
    chart: &bms_model::Chart,
    screens: &Installers,
    pending: bool,
) -> crate::render_side::RenderLink {
    if !pending {
        return screens.spawn(&Arc::new(chart.clone()), &bundle());
    }
    let render = screens.spawn_bare();
    render.load(Arc::new(chart.clone()), None).unwrap();
    assert_eq!(
        render.pend(skin_model::ScreenKind::Play).wait().unwrap(),
        game::Preparation::Ready
    );
    render
}

/// `f` on a player's rig over [`render_for`]'s render thread. Every held
/// job thread and VM is let go before the rig drops, a failing test's
/// included.
fn on_rig<R>(
    chart: &bms_model::Chart,
    screens: &Installers,
    pending: bool,
    f: impl FnOnce(&mut PlayerRig<'_>) -> R,
) -> R {
    let render = render_for(chart, screens, pending);
    crate::tests::on_rig_with(settings(), render, |rig, _| {
        let _let_go = LetGo;
        f(rig)
    })
    .0
}

/// Lets every held job thread and VM go when dropped.
struct LetGo;

impl Drop for LetGo {
    fn drop(&mut self) {
        imports::hold_exit(false);
        vm_hooks::release();
    }
}

/// A pending play skin built from `body`.
fn pending_play(body: &str) -> (tempfile::TempDir, crate::imports::PendingSkin) {
    let r = crate::render_common::offscreen(1_920, 1_080);
    let t = imports::target(&r);
    let dir = imports::lua_skin(body);
    let (p, _) = imports::pending(imports::play(
        &imports::choice(dir.path(), "play.luaskin"),
        &t,
    ));
    (dir, crate::imports::PendingSkin::new(p))
}

/// A pending result skin running `pre` as it loads, its images staged at
/// play level 12.
fn staged_result(pre: &str) -> (tempfile::TempDir, ResultSkin) {
    let d = result::skin(
        pre,
        "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }",
    );
    let p = result::pending(d.path(), result::LIMITS);
    let mut rs = ResultSkin::new(Some(ResultStart::Pending(p)));
    result::staged_at(&mut rs, 12, JudgeClass::Normal);
    assert!(rs.image().is_some(), "the probe's images");
    (d, rs)
}

/// The stage the rig's preparation stands at, by name.
fn stage(rig: &PlayerRig<'_>) -> &'static str {
    match rig.prep.as_ref().map(|p| &p.stage) {
        None => "none",
        Some(PrepStage::Skin(_)) => "skin",
        Some(PrepStage::Image(..)) => "image",
        Some(PrepStage::Screen(_)) => "screen",
        Some(PrepStage::Ended(_)) => "ended",
        Some(PrepStage::Answered(_)) => "answered",
    }
}

/// Polls `handle` to its end, in 10 ms slices as the flow polls it.
fn polled(
    rig: &mut PlayerRig<'_>,
    handle: &mut game::PrepHandle,
) -> Result<game::Preparation, crate::render_side::ScreenError> {
    let began = Instant::now();
    loop {
        assert!(began.elapsed() < watchdog(), "the preparation never ended");
        let until = Instant::now() + Duration::from_millis(10);
        if let game::PrepPoll::Done(ended) = rig.poll_preparation(handle, until)? {
            return Ok(ended);
        }
    }
}

/// Begins `loaded`'s preparation with its own key.
fn begin(rig: &mut PlayerRig<'_>, loaded: &game::Loaded) -> game::PrepHandle {
    rig.begin_preparation(loaded, loaded.prep_key())
}

/// The run's end a test signals, from another thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// The window's close.
    Close,
    /// The input thread's failure.
    Input,
    /// The audio output's failure.
    Audio,
}

impl Signal {
    const ALL: [Self; 3] = [Self::Close, Self::Input, Self::Audio];

    /// What gives `self` on `rig`, from any thread.
    fn giver<'a>(self, rig: &PlayerRig<'a>) -> impl Fn() + Send + Sync + use<'a> {
        let controls = rig.controls;
        let input = rig.input.failer();
        let audio = Arc::clone(&rig.audio_lost);
        move || match self {
            Self::Close => controls.close(),
            Self::Input => input(),
            Self::Audio => audio.store(true, std::sync::atomic::Ordering::Release),
        }
    }

    /// Whether `rig` shows `self` as the run's end.
    fn shown(self, rig: &PlayerRig<'_>) -> bool {
        match self {
            Self::Close => rig.controls.quitting(),
            Self::Input => rig.input_failure().is_some(),
            Self::Audio => rig.audio_failure().is_some(),
        }
    }
}

/// Runs `settle`, a call that settles a held job, while another thread
/// lets the job go with `release` [`PAST`] after `settle` began: what
/// `settle` returned, and whether it returned only after the release.
fn held_past<R>(release: impl FnOnce() + Send, settle: impl FnOnce() -> R) -> (R, bool) {
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::channel::<Instant>();
        let helper = s.spawn(move || {
            let began = rx.recv().unwrap();
            std::thread::sleep((began + PAST).saturating_duration_since(Instant::now()));
            let at = Instant::now();
            release();
            at
        });
        tx.send(Instant::now()).unwrap();
        let out = settle();
        let returned = Instant::now();
        let released = helper.join().unwrap();
        (out, returned >= released)
    })
}

/// The jobs this test started and joined since `from`.
fn jobs_since(from: (u64, u64)) -> (u64, u64) {
    let now = imports::jobs_counted();
    (now.0 - from.0, now.1 - from.1)
}

/// Lets VMs go and waits until every job thread ended, an abandoned one
/// included.
fn release_workers() {
    vm_hooks::release();
    imports::hold_exit(false);
    until("every job thread ended", || imports::workers() == 0);
}
