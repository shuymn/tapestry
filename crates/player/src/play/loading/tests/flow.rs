//! `game::load_and_open` on the player's rig with its threads (S10 item
//! 9.4a plan §2.3, §2.4, §2.8): a selection's loading state on a render
//! thread of [`Installers`], an input thread serving the rig's link, and
//! the opener's steps met at the rig's test gates (the epoch's refresh,
//! the revalidation), the screen's held commit and the held end of the
//! loading lifetime, never by timing. A watchdog
//! (`TAPESTRY_TEST_WATCHDOG`, seconds) panics a wait that never ends.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bms_model::{TimeSource as _, TimeUs};
use game::{Adjust, AppRig as _, Command, CoverKind, Leave, LiveError};

use super::{Keyboard, accepting, covers, fenced, on_input_rig_with, watchdog};
use crate::imports::tests as imports;
use crate::play::PlayerRig;
use crate::render_side::ScreenError;
use crate::render_side::tests::{Installers, bundle};
use crate::tuning::{Acceptance, KeyEvent};

/// How the opener's run ended.
type Ended = Result<Leave, LiveError<ScreenError>>;

/// Waits until `done`, looking every 1 ms; panics past the watchdog.
#[track_caller]
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < watchdog(), "{what} never happened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The chart `text` (its notes included) loaded under `settings`, its
/// file kept in the directory returned.
fn chart(text: &str, settings: game::Settings) -> (tempfile::TempDir, game::Loaded) {
    let dir = tempfile::tempdir().unwrap();
    let root = game::DataRoot::new(dir.path().join("root"));
    let path = dir.path().join("a.bms");
    std::fs::write(&path, text).unwrap();
    let config = game::SessionConfig {
        lead_in: TimeUs(1_000_000),
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: 64,
        seed: 1,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: replay::BuildProvenance::Development,
        receive: replay::ReceiveMethod::Scripted,
        autoplay: None,
    };
    let parts = game::Parts::unsaved(&settings.player);
    let loaded = game::Loaded::load(root, settings, &path, config, parts).unwrap();
    (dir, loaded)
}

/// A chart at 120 BPM with one note, LIFT and HIDDEN on.
fn plain() -> (tempfile::TempDir, game::Loaded) {
    chart("#PLAYLEVEL 13\n#BPM 120\n#00111:01\n", covers())
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

/// What a test reaches beside the game thread: the controls, the
/// keyboard, the screens and the render thread's holds, and the opener's
/// gates on the rig.
struct Side<'a> {
    controls: &'a crate::controls::Controls,
    keys: &'a Keyboard,
    screens: &'a Installers,
    loading_end: Arc<crate::render_side::TestGate>,
    ack: crate::render_side::AckHold,
    gates: Arc<crate::play::loading::OpeningGates>,
    clock: input::QpcClock,
    tuning: Arc<crate::tuning::LiveTuning>,
    /// Set once `load_and_open` returned.
    returned: std::sync::atomic::AtomicBool,
}

impl Side<'_> {
    /// Lets every hold go.
    fn let_go(&self) {
        self.loading_end.hold(false);
        self.ack.hold(false);
        self.gates.refresh.hold(false);
        self.gates.revalidate.hold(false);
        self.screens.hold_commits(false);
        skin_beatoraja::vm_hooks::release();
    }

    /// A settings key pressed and released, from the window thread's side.
    fn tap(&self, a: Adjust) {
        self.controls.settings_key(a, KeyEvent::Press);
        self.controls.settings_key(a, KeyEvent::Release);
    }

    /// Waits until recording opened after the `ends`-th end of loading
    /// (acceptance, closed for the candidate, reads open once the input
    /// thread opened it), then presses the quit key until the run returns:
    /// the play aborts, and its result is left.
    fn quit_the_play(&self, ends: u64) {
        until("recording opened", || {
            self.loading_end.passed() >= ends && self.tuning.acceptance() == Acceptance::Open
        });
        self.quit_until_returned();
    }

    /// Presses the quit key until the run returns.
    fn quit_until_returned(&self) {
        let began = Instant::now();
        while !self.returned.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(began.elapsed() < watchdog(), "the run never returned");
            self.controls.command(Command::Quit);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the end of loading's answer held (O13 or a deferral's
    /// second end), quits, and lets it go: the next look leaves.
    fn quit_at_the_end_of_loading(&self) {
        until("the end of loading held", || self.loading_end.waiting());
        self.controls.command(Command::Quit);
        self.loading_end.hold(false);
    }
}

/// `f` on a player's rig in a selection's loading state of `loaded`'s
/// chart, the chart's load done as `load_chart` leaves it (its attachment,
/// a refusal ignored), with a fixed play skin or `pending`, and the quit
/// key leaving rather than ending the run, as with selection. With
/// `held_ack`, the loading lifetime's first presented frame is held
/// unacknowledged from before the lifetime begins.
fn on_loading<R>(
    loaded: &mut game::Loaded,
    pending: Option<crate::imports::PendingSkin>,
    held_ack: bool,
    f: impl FnOnce(&mut PlayerRig<'_>, &Side<'_>, &mut game::Loaded) -> R,
) -> R {
    let _serial = imports::serial();
    let screens = Installers::default();
    let chart = Arc::new(loaded.chart().clone());
    let render = if pending.is_some() {
        let render = screens.spawn_bare();
        render.load(chart, None).unwrap();
        assert_eq!(
            render.pend(skin_model::ScreenKind::Play).wait().unwrap(),
            game::Preparation::Ready
        );
        render
    } else {
        screens.spawn(&chart, &bundle())
    };
    on_input_rig_with(covers(), render, |rig, keys| {
        let dir = tempfile::tempdir().unwrap();
        let (sel, _) = crate::selection::tests::tree(dir.path(), &[&["b.bms"]]);
        rig.selection = Some(sel);
        rig.pending = pending;
        rig.controls.quit_leaves();
        rig.render.hold_loading_ack(held_ack);
        rig.entered_selection();
        rig.entered_loading(loaded.path());
        if let Some((driver, mut pump)) = rig.adjustments() {
            let _ = driver.attach_loaded(loaded, &mut pump);
        }
        let side = Side {
            controls: rig.controls,
            keys,
            screens: &screens,
            loading_end: rig.render.loading_end_gate(),
            ack: rig.render.ack_hold(),
            gates: Arc::clone(&rig.opening_gates),
            clock: rig.clock,
            tuning: Arc::clone(rig.controls.tuning()),
            returned: std::sync::atomic::AtomicBool::new(false),
        };
        let out = f(rig, &side, loaded);
        side.let_go();
        out
    })
}

/// `load_and_open` on the game thread while `helper` runs beside it. A
/// helper that panics lets every hold go and closes the window, so the
/// run ends and the panic is reported.
fn open_with(
    loaded: &mut game::Loaded,
    rig: &mut PlayerRig<'_>,
    side: &Side<'_>,
    helper: impl FnOnce() + Send,
) -> Ended {
    std::thread::scope(|s| {
        let helper = s.spawn(|| {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(helper));
            if ran.is_err() {
                side.let_go();
                side.controls.close();
            }
            ran
        });
        let ended = game::load_and_open(loaded, rig);
        side.returned
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(panic) = helper.join().unwrap() {
            std::panic::resume_unwind(panic);
        }
        ended
    })
}

/// SELECT and START together on the keyboard: HIDDEN selected (LIFT is
/// after the attempt's entry), one accepted adjustment.
fn select_hidden(rig: &mut PlayerRig<'_>, side: &Side<'_>) -> u64 {
    let base = fenced(rig);
    side.keys.both();
    assert_eq!(fenced(rig), base + 1);
    assert_eq!(selected(rig), Some(CoverKind::Hidden));
    base + 1
}

/// The cover the reducer has selected.
fn selected(rig: &PlayerRig<'_>) -> Option<CoverKind> {
    rig.adjust.as_ref().and_then(|d| d.snapshot().target)
}

/// After the opener returned to the page or selection: acceptance open on
/// both sides, a display offset key steps and START with SELECT act.
#[track_caller]
fn tuning_works(rig: &mut PlayerRig<'_>, side: &Side<'_>) {
    assert_eq!(rig.controls.tuning().acceptance(), Acceptance::Open);
    assert!(accepting(rig));
    let before = rig.controls.tuning().get().display_offset_us;
    side.tap(Adjust::DisplayOffsetUp);
    assert_eq!(
        rig.controls.tuning().get().display_offset_us,
        before + 1_000
    );
    let base = fenced(rig);
    side.keys.both();
    assert_eq!(fenced(rig), base + 1);
}

/// E1 (M13): a fixed play skin and no result skin, so the preparation
/// begun at load-ready sends its screen request with the load's epoch at
/// once; the epoch's refresh at O5 names the next one, which the final
/// readiness stamps and the commit installs. HIDDEN, selected before,
/// stays selected through the refresh. (The held repeating control of the
/// row cannot be down through the wait here: every repeating adjustment
/// holds START, which holds the wait; its half is the driver's
/// `the_epoch_refresh_keeps_the_selected_cover_and_holds`.)
#[test]
fn the_final_readiness_stamps_the_attempts_epoch() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let accepted = select_hidden(rig, side);
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || side.quit_at_the_end_of_loading());
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let (staged, committed) = rig.render.epochs();
        assert!(staged > 0);
        assert_eq!(committed, staged + 1, "e2, named at O5, not e1");
        assert_eq!(side.gates.refresh.passed(), 1);
        assert_eq!(fenced(rig), accepted, "the refresh is no change");
        assert_eq!(selected(rig), Some(CoverKind::Hidden));
    });
}

/// E1b: a pending play skin whose specialization still runs at O5: the
/// load's epoch is still pending, so the refresh attaches nothing and
/// names no epoch; the specialization's screen request then takes the
/// load's epoch, which the final readiness stamps.
#[test]
fn a_pending_skin_keeps_the_loads_epoch() {
    let (_p, skin) = pending_play("local lv = ms.number(96)");
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, Some(skin), false, |rig, side, loaded| {
        // The specialization's VM is held at its destruction: the job
        // does not end until let go.
        skin_beatoraja::vm_hooks::hold();
        side.gates.refresh.hold(true);
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || {
            until("the specialization held", || {
                skin_beatoraja::vm_hooks::waiting() > 0
            });
            until("the refresh", || side.gates.refresh.waiting());
            // Still specializing at O5: let it go only now.
            skin_beatoraja::vm_hooks::release();
            side.gates.refresh.hold(false);
            side.quit_at_the_end_of_loading();
        });
        skin_beatoraja::vm_hooks::release();
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let (staged, committed) = rig.render.epochs();
        assert!(staged > 0);
        assert_eq!(committed, staged, "e1, the load's");
        assert_eq!(side.gates.refresh.passed(), 1);
    });
}

/// E3 (and A11's player half): a settings key pressed after the opening's
/// copy is found at the revalidation: the opening retries locally, the
/// preparation kept, and its second refresh names e3, which the final
/// readiness stamps. HIDDEN stays selected through the retry.
#[test]
fn a_local_retry_stamps_the_new_epoch() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let accepted = select_hidden(rig, side);
        let offset = rig.controls.tuning().get().display_offset_us;
        side.gates.revalidate.hold(true);
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || {
            until("the revalidation", || side.gates.revalidate.waiting());
            side.tap(Adjust::DisplayOffsetUp);
            side.gates.revalidate.hold(false);
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let (staged, committed) = rig.render.epochs();
        assert_eq!(committed, staged + 2, "e3: e2 was the retried attempt's");
        assert_eq!(side.gates.refresh.passed(), 2);
        assert_eq!(side.gates.revalidate.passed(), 2);
        assert_eq!(rig.preps, 1, "the preparation was kept");
        assert_eq!(fenced(rig), accepted);
        assert_eq!(selected(rig), Some(CoverKind::Hidden));
        assert_eq!(
            loaded.settings().device.display_offset_us,
            offset + 1_000,
            "the retry's copy took the press"
        );
    });
}

/// A10c, the production half: Floating without references. The load's
/// attachment was refused and ignored; the refresh at O5 attaches again
/// and reports the refusal, the preparation begun at load-ready is
/// discarded, nothing is committed and recording does not open; tuning
/// acceptance is open after.
#[test]
fn a_failed_load_attachment_is_refused_at_o5() {
    // A tempo with no exact ratio: no START reference.
    let (_dir, mut loaded) = chart(
        "#PLAYLEVEL 13\n#BPM 150.000000000000000001\n#00111:01\n",
        covers(),
    );
    assert!(loaded.attempt_tempo().references().is_err());
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let ended = open_with(loaded, rig, side, || {});
        let Err(LiveError::Preparation(why)) = &ended else {
            panic!("{ended:?}");
        };
        assert!(
            why.reason.contains("Floating hi-speed cannot be solved"),
            "{why:?}"
        );
        assert_eq!(side.gates.refresh.passed(), 1);
        assert_eq!(rig.render.epochs().1, 0, "nothing committed");
        // The discard is the render thread's to note.
        until("the staging discarded", || {
            side.screens
                .log()
                .last()
                .is_some_and(|l| l.ends_with(" discard"))
        });
        let log = side.screens.log();
        let staged = log
            .iter()
            .position(|l| l.contains(" prepare "))
            .expect("prepared at load-ready");
        assert_eq!(log[staged + 1..], ["1 discard"], "{log:?}");
        assert!(rig.prep.is_none(), "the handle disposed of");
        assert_eq!(side.gates.revalidate.passed(), 0);
        assert_eq!(rig.controls.tuning().acceptance(), Acceptance::Open);
    });
}

/// A13, the flow's half: a settings key and START with SELECT pressed
/// while the commit's reply is held, or while the end of loading's
/// answer is held, act on nothing; once the last look's quit returned,
/// a fresh press acts again.
#[test]
fn a_press_in_the_closed_gap_is_discarded_and_rearms() {
    for at_commit in [true, false] {
        let (_dir, mut loaded) = plain();
        on_loading(&mut loaded, None, false, |rig, side, loaded| {
            let accepted = fenced(rig);
            let offset = rig.controls.tuning().get().display_offset_us;
            side.screens.hold_commits(at_commit);
            side.loading_end.hold(true);
            let ended = open_with(loaded, rig, side, || {
                if at_commit {
                    until("the commit held", || side.screens.commit_held());
                } else {
                    until("the end held", || side.loading_end.waiting());
                }
                side.tap(Adjust::DisplayOffsetUp);
                side.keys.both();
                side.screens.hold_commits(false);
                side.quit_at_the_end_of_loading();
            });
            assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
            assert_eq!(
                rig.controls.tuning().get().display_offset_us,
                offset,
                "{at_commit}"
            );
            assert!(rig.controls.tuning().held().is_empty(), "{at_commit}");
            assert_eq!(fenced(rig), accepted, "{at_commit}");
            tuning_works(rig, side);
        });
    }
}

/// A14: a quit at the last look, after the commit and the end of
/// loading: the run is left for the page, the committed installation
/// stays, and acceptance is open again on both sides: on the practice
/// page a settings key steps and START with SELECT act.
#[test]
fn a_quit_at_the_last_look_reopens_tuning_on_the_page() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || side.quit_at_the_end_of_loading());
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        assert!(rig.render.epochs().1 > 0, "committed");
        assert!(
            side.screens.log().iter().any(|l| l.contains(" commit ")),
            "the installation stays"
        );
        let c = bms_model::ChartBuilder::new(120_000).build().unwrap();
        rig.entered_practice(&game::PracticeScreen::new(&c, &rules::tapestry(), 0));
        tuning_works(rig, side);
    });
}

/// L1 and L3, the opener's side: the minimum counts from the render
/// thread's acknowledgement of the lifetime's first presented frame, held
/// here past the lifetime's beginning; the loading state ends no earlier
/// than a second after it.
#[test]
fn the_opener_counts_the_minimum_from_the_render_ack() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, true, |rig, side, loaded| {
        let lifetime = rig.render.loading_lifetime();
        side.loading_end.hold(true);
        let mut passed = TimeUs(0);
        let mut ended_at = TimeUs(0);
        let ended = open_with(loaded, rig, side, || {
            until("the first frame held", || side.ack.held());
            // The frame stays unacknowledged a while.
            std::thread::sleep(Duration::from_millis(300));
            passed = side.clock.now();
            side.ack.hold(false);
            side.ack.pass();
            until("the end held", || side.loading_end.waiting());
            ended_at = side.clock.now();
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let (acked, at) = rig.render.loading_ack().unwrap();
        assert_eq!(acked, lifetime);
        assert!(at >= passed, "{at:?} {passed:?}");
        assert!(ended_at.0 >= at.0 + 1_000_000, "{ended_at:?} {at:?}");
    });
}

/// A18f, on the player's rig: focus lost while the commit's reply is
/// held. The opening defers for focus, reopening acceptance; it is asked
/// again only once focus is back and the tuning quiet for a second from
/// the return.
#[test]
fn focus_lost_during_the_commit_reply_waits_a_quiet_second_after_return() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        side.screens.hold_commits(true);
        side.loading_end.hold(true);
        let mut returned = TimeUs(0);
        let mut asked = TimeUs(0);
        let tuning = Arc::clone(rig.controls.tuning());
        let ended = open_with(loaded, rig, side, || {
            until("the commit held", || side.screens.commit_held());
            side.controls.focus(false);
            side.screens.hold_commits(false);
            until("the end held", || side.loading_end.waiting());
            side.loading_end.hold(false);
            // Deferred for focus: acceptance reopened.
            until("the deferral", || {
                side.loading_end.passed() == 1 && tuning.acceptance() == Acceptance::Open
            });
            side.loading_end.hold(true);
            returned = side.clock.now();
            side.controls.focus(true);
            until("the second end held", || side.loading_end.waiting());
            asked = side.clock.now();
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let waited = asked.0 - returned.0;
        assert!(
            (1_000_000..2_500_000).contains(&waited),
            "{waited} us from the return"
        );
        assert_eq!(side.loading_end.passed(), 2);
    });
}

/// The display offset the one ended play recorded.
#[track_caller]
fn recorded_offset(rig: &PlayerRig<'_>) -> TimeUs {
    let [(_, record)] = rig.ended_plays.as_slice() else {
        panic!("{:?}", rig.ended_plays.len());
    };
    record.as_ref().expect("a record").devices.display_offset
}

/// A23: an opening deferred by START held (pressed in the closed gap,
/// while the commit's reply is held): acceptance reopens, a display
/// offset key steps, START is released, acceptance closes again on a
/// fresh copy, and recording opens with it: the record's display offset
/// is the one set during the deferral.
#[test]
fn a_display_offset_set_during_a_deferral_is_recorded() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let offset = rig.controls.tuning().get().display_offset_us;
        side.screens.hold_commits(true);
        let ended = open_with(loaded, rig, side, || {
            until("the commit held", || side.screens.commit_held());
            side.keys.key(super::START, 1);
            side.screens.hold_commits(false);
            // Deferred for START: acceptance reopened.
            until("the deferral", || {
                side.loading_end.passed() == 1 && side.tuning.acceptance() == Acceptance::Open
            });
            side.tap(Adjust::DisplayOffsetUp);
            side.keys.key(super::START, 0);
            side.quit_the_play(2);
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        assert_eq!(side.loading_end.passed(), 2, "deferred once");
        assert_eq!(recorded_offset(rig), TimeUs(offset + 1_000));
    });
}

/// O-20, the player's half: settings keys pressed after the epoch's
/// refresh (O5) and before the revalidation (O11) each send the opening
/// back to the wait; the copy it finally opens with has both, and so does
/// the record. HIDDEN, selected before, stays selected.
#[test]
fn an_adjustment_at_each_opening_step_reaches_the_record() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        select_hidden(rig, side);
        let offset = rig.controls.tuning().get().display_offset_us;
        side.gates.refresh.hold(true);
        let ended = open_with(loaded, rig, side, || {
            until("the refresh", || side.gates.refresh.waiting());
            side.tap(Adjust::DisplayOffsetUp);
            side.gates.revalidate.hold(true);
            side.gates.refresh.hold(false);
            until("the revalidation", || side.gates.revalidate.waiting());
            side.tap(Adjust::DisplayOffsetUp);
            side.gates.revalidate.hold(false);
            side.quit_the_play(1);
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        // Both presses are found at the first revalidation: one local
        // retry, then the opening.
        assert_eq!(side.gates.refresh.passed(), 2);
        assert_eq!(side.gates.revalidate.passed(), 2);
        assert_eq!(recorded_offset(rig), TimeUs(offset + 2_000));
        assert_eq!(loaded.settings().device.display_offset_us, offset + 2_000);
        assert_eq!(selected(rig), Some(CoverKind::Hidden));
    });
}

/// A18p: focus lost once recording opened out of the loading state
/// aborts the play through the focus gate, as for any opening.
#[test]
fn focus_lost_after_a_loading_opening_aborts_the_play() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let ended = open_with(loaded, rig, side, || {
            until("recording opened", || {
                side.loading_end.passed() >= 1 && side.tuning.acceptance() == Acceptance::Open
            });
            side.controls.focus(false);
            // The gate looks at its latch before the next batch.
            side.keys.key(super::SELECT, 0);
            until("the result", || side.tuning.end_screen_shown());
            side.controls.focus(true);
            side.quit_until_returned();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let [(end, _)] = rig.ended_plays.as_slice() else {
            panic!("{:?}", rig.ended_plays.len());
        };
        assert_eq!(*end, game::PlayEnd::Aborted(game::AbortCause::FocusLoss));
    });
}

/// F2 on the keyboard: hi-speed faster through the play adjustments, a
/// role that is not latched (no START or SELECT), so it moves neither the
/// window's revision nor the input side's.
const FASTER: u32 = game::SPEED_KEYS[1];

/// A11 through the reducer alone: F2 pressed after the opening's copy and
/// before the revalidation is a change only the reducer's count shows (no
/// settings key, no START or SELECT, nothing held at O11). The opening
/// retries locally and opens with the faster hi-speed at a new epoch.
#[test]
fn a_reducer_change_before_the_revalidation_retries() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let accepted = fenced(rig);
        let hispeed = rig.adjust.as_ref().unwrap().snapshot().hispeed;
        let window = rig.controls.tuning().snapshot().revision;
        side.gates.revalidate.hold(true);
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || {
            until("the revalidation", || side.gates.revalidate.waiting());
            side.keys.key(FASTER, 1);
            side.keys.key(FASTER, 0);
            side.gates.revalidate.hold(false);
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let now = rig.controls.tuning().snapshot().revision;
        assert_eq!(
            (now.window, now.input),
            (window.window, window.input),
            "only the reducer saw it"
        );
        assert_eq!(fenced(rig), accepted + 1);
        assert_ne!(rig.adjust.as_ref().unwrap().snapshot().hispeed, hispeed);
        assert_eq!(side.gates.revalidate.passed(), 2, "one local retry");
        let (staged, committed) = rig.render.epochs();
        assert_eq!(committed, staged + 2);
    });
}

/// A11's input half: after a local retry, the reducer's close at the
/// revalidation was reversed, so START with SELECT pressed in the next
/// wait acts (HIDDEN selected).
#[test]
fn a_local_retry_reopens_the_reducer() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        let accepted = fenced(rig);
        side.gates.revalidate.hold(true);
        side.loading_end.hold(true);
        let ended = open_with(loaded, rig, side, || {
            until("the revalidation", || side.gates.revalidate.waiting());
            side.tap(Adjust::DisplayOffsetUp);
            side.gates.revalidate.hold(false);
            until("the retry", || side.gates.revalidate.passed() == 1);
            side.gates.revalidate.hold(true);
            // In the wait again, before the next revalidation.
            until("the second revalidation", || {
                side.gates.revalidate.waiting()
            });
            side.keys.both();
            side.gates.revalidate.hold(false);
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        assert_eq!(fenced(rig), accepted + 1, "START with SELECT acted");
        assert_eq!(selected(rig), Some(CoverKind::Hidden));
        assert_eq!(side.gates.revalidate.passed(), 3, "two local retries");
    });
}

/// A18f by the latch alone: focus lost and back while the commit's reply
/// is held, so it is back when the deferral is read. The opening defers
/// for the loss, and asks again only a quiet second after the return.
#[test]
fn focus_lost_and_back_during_the_commit_reply_waits_a_quiet_second() {
    let (_dir, mut loaded) = plain();
    on_loading(&mut loaded, None, false, |rig, side, loaded| {
        side.screens.hold_commits(true);
        side.loading_end.hold(true);
        let mut returned = TimeUs(0);
        let mut asked = TimeUs(0);
        let ended = open_with(loaded, rig, side, || {
            until("the commit held", || side.screens.commit_held());
            side.controls.focus(false);
            returned = side.clock.now();
            side.controls.focus(true);
            side.screens.hold_commits(false);
            until("the end held", || side.loading_end.waiting());
            side.loading_end.hold(false);
            until("the deferral", || {
                side.loading_end.passed() == 1 && side.tuning.acceptance() == Acceptance::Open
            });
            side.loading_end.hold(true);
            until("the second end held", || side.loading_end.waiting());
            asked = side.clock.now();
            side.quit_at_the_end_of_loading();
        });
        assert!(matches!(ended, Ok(Leave::Back)), "{ended:?}");
        let waited = asked.0 - returned.0;
        assert!(
            (1_000_000..2_500_000).contains(&waited),
            "{waited} us from the return"
        );
        assert_eq!(side.loading_end.passed(), 2);
    });
}
