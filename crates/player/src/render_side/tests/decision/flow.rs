//! The decision skin through the player's flow (design.md "Play
//! Lifecycle", the decision screen): the choice pins the chart's ready
//! labels through `PlayerRig::decision_shown` without waiting, the entry
//! tells the render thread what it claimed, every end of the lifetime
//! tells the label book, and the built-in presentation takes over when
//! the skin cannot draw. The game's App is played by the test, one hook at
//! a time, as `App::run` calls them, on the render thread's clock; every
//! choice's origin is past the clock's start.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bms_model::{TimeSource as _, TimeUs};
use game::{
    AppRig as _, Decision, DecisionLifetime, DecisionShown, Origin, Pick, Rig as _, Verdict,
};
use render::{LabelBook, LabelState, lock};
use tempfile::TempDir;

use super::{Rig, Seen, skin_dir};
use crate::decision_side::decision_frame_of;
use crate::play::PlayerRig;
use crate::render_side::Extent;
use crate::render_side::tests::wait_until;

const MS: i64 = 1_000;

/// [`skin_dir`]'s skin declaring `scene`, `input` and `fadeout` in ms.
fn timed(scene: i64, input: i64, fadeout: i64) -> TempDir {
    let dir = skin_dir();
    let path = dir.path().join("s.json");
    let json = std::fs::read_to_string(&path).unwrap();
    let timing = format!(r#""type": 6, "scene": {scene}, "input": {input}, "fadeout": {fadeout},"#);
    std::fs::write(&path, json.replacen(r#""type": 6,"#, &timing, 1)).unwrap();
    dir
}

/// The 2,500 / 500 / 500 ms skin of stage3's list.
fn listed() -> TempDir {
    timed(2_500, 500, 500)
}

/// What a flow test sees of the render thread and its chart.
struct Flow {
    seen: Arc<Seen>,
    book: Arc<Mutex<LabelBook>>,
    extent: Arc<Extent>,
    /// The chart under the cursor, as the library lists it.
    chart: PathBuf,
}

impl Flow {
    /// Waits until the chart's labels are ready in a slot.
    fn prepared(&self) {
        assert!(
            wait_until(|| self.ready().is_some()),
            "never ready: {:?}",
            lock(&self.book)
        );
    }

    /// The ready slot holding the chart's labels and its generation.
    fn ready(&self) -> Option<(usize, u64)> {
        let book = lock(&self.book);
        (0..render::LABEL_SLOTS).find_map(|i| match book.state(i) {
            Some(LabelState::Ready {
                generation, key, ..
            }) if key.chart == self.chart => Some((i, *generation)),
            _ => None,
        })
    }

    /// The pinned slot, its generation and its lifetime.
    fn pinned(&self) -> Option<(usize, u64, u64)> {
        let book = lock(&self.book);
        (0..render::LABEL_SLOTS).find_map(|i| match book.state(i) {
            Some(LabelState::Pinned {
                generation,
                lifetime,
                ..
            }) => Some((i, *generation, *lifetime)),
            _ => None,
        })
    }

    /// The pick of the chart, to play or to practice.
    fn pick(&self, practice: bool) -> Pick {
        if practice {
            Pick::Practice(self.chart.clone())
        } else {
            Pick::Play(self.chart.clone())
        }
    }
}

/// `f` on a player rig whose render thread draws on [`Recorder`]s with
/// the decision skin of `dir` committed, sharing the rig's clock, the
/// select screen entered with its cursor on one chart.
///
/// [`Recorder`]: super::Recorder
fn flow<R>(dir: &TempDir, f: impl FnOnce(&mut PlayerRig<'_>, &Flow) -> R) -> R {
    flow_with(|clock| Rig::on(dir, clock), f)
}

/// [`flow`] on the render thread `rig` makes on the clock it is given,
/// its decision skin committed as it chose.
fn flow_with<R>(
    rig: impl FnOnce(input::QpcClock) -> Rig,
    f: impl FnOnce(&mut PlayerRig<'_>, &Flow) -> R,
) -> R {
    let clock = input::QpcClock::start();
    let Rig {
        link,
        seen,
        book,
        extent,
        _turn: turn,
        ..
    } = rig(clock);
    let charts = tempfile::tempdir().unwrap();
    let (mut sel, roots) = crate::selection::tests::tree(charts.path(), &[&["a.bms"]]);
    assert!(sel.screen.select_path(&roots[0].join("a.bms")));
    let chart = sel.screen.selected().unwrap().path.clone();
    let flow = Flow {
        seen,
        book,
        extent,
        chart,
    };
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let (out, _) = crate::tests::on_rig_hooked(settings, link, clock, |rig, _, _| {
        // No adjustment waits on the input thread, which none serves.
        rig.adjust = None;
        rig.controls.quit_leaves();
        rig.selection = Some(sel);
        rig.entered_selection();
        f(rig, &flow)
    });
    drop(turn);
    out
}

/// The choice of `pick` in `lifetime`, as App makes it: the lifetime
/// first, the presentation chosen for it, the screen begun from now and
/// entered.
fn choose(rig: &mut PlayerRig<'_>, pick: &Pick, lifetime: u64) -> Decision {
    let now = rig.now();
    let lifetime = DecisionLifetime(lifetime);
    let shown = rig.decision_shown(pick, lifetime);
    let d = Decision::begin(now, lifetime, shown, matches!(pick, Pick::Practice(_)));
    rig.entered_decision(pick, d)
}

/// Waits until the rig's clock reads `t`.
fn until_clock(rig: &PlayerRig<'_>, t: TimeUs) {
    while rig.clock.now().0 < t.0 {
        std::thread::yield_now();
    }
}

/// The skin's frames drawn after the `from`th frame.
fn skins_after(seen: &Seen, from: usize) -> usize {
    seen.skins(from)
}

/// stage3's list, scene / input / fade-out 2,500 / 500 / 500 ms through
/// the player: the choice pins the ready slot and its screen lasts
/// 3,000 ms from the choice; the render thread is told the choice, the
/// timing and the slot; every skin frame drawn runs its timers from the
/// choice, timer 1 from 500 ms (off 1 µs before, on at 500,000 µs) and
/// timer 2 from 2,500 ms; the transition comes at 3,000 ms, not
/// before; the step ends the lifetime and the slot is retired.
#[test]
fn skin_decision_timeline_through_the_player() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let (slot, generation) = f.ready().unwrap();
        let mut d = choose(rig, &f.pick(false), 3);
        let origin = d.origin();
        assert!(origin.0 > 0, "a nonzero origin");
        let at = |us: i64| TimeUs(origin.0 + us);
        assert_eq!(
            d.shown(),
            Some(DecisionShown::Skin {
                slot: u8::try_from(slot).unwrap(),
                generation,
                scene: TimeUs(2_500 * MS),
                fadeout: TimeUs(500 * MS),
            })
        );
        assert_eq!(d.deadline(), Some(at(3_000 * MS)));
        assert_eq!(f.pinned(), Some((slot, generation, 3)));
        let sent = rig.render.decision_sent().expect("a skinned presentation");
        assert_eq!(
            (sent.lifetime, sent.origin, sent.slot),
            (3, origin, Some((slot, generation)))
        );
        assert_eq!(sent.timing.input, TimeUs(500 * MS));
        let input = |us| decision_frame_of(&sent, at(us), [1_920, 1_080]).timers;
        assert_eq!(input(500 * MS - 1).input, None, "timer 1 before 500 ms");
        assert_eq!(input(500 * MS).input, Some(at(500 * MS)));
        assert_eq!(input(2_500 * MS - 1).fadeout, None);
        assert_eq!(input(2_500 * MS).fadeout, Some(at(2_500 * MS)));
        // The frames drawn through the fade-out run their timers from the
        // choice.
        assert!(wait_until(|| {
            f.seen
                .timers
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.0.0 > at(2_600 * MS).0)
        }));
        for &(now, input, fadeout) in f.seen.timers.lock().unwrap().iter() {
            let late = now.0 - origin.0;
            assert_eq!(input, (late >= 500 * MS).then_some(at(500 * MS)), "{late}");
            assert_eq!(fadeout, (late >= 2_500 * MS).then_some(at(2_500 * MS)));
        }
        assert_eq!(
            d.classify(Origin::Ready, at(3_000 * MS - 1), false, false, None),
            Verdict::Continue
        );
        assert_eq!(
            d.classify(Origin::Ready, at(3_000 * MS), false, false, None),
            Verdict::Step
        );
        rig.deadline_step();
        assert_eq!(lock(&f.book).active(), None, "the step ended the lifetime");
        assert!(wait_until(|| f.pinned().is_none()), "never retired");
    });
}

/// R3.1 (stage3's list): commands and a settings key pressed at 100 and
/// 600 ms from the choice, before and after the skin's `input`, through
/// the controls' queues, are discarded, the looks of the screen at both
/// times included: nothing taken, nothing pending, no setting stepped.
#[test]
fn skin_decision_discards_commands_before_and_after_input() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let d = choose(rig, &f.pick(false), 3);
        assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
        let origin = d.origin();
        let values = rig.controls.tuning().snapshot().values;
        for ms in [100, 600] {
            until_clock(rig, TimeUs(origin.0 + ms * MS));
            rig.decision_tick(Origin::Load).unwrap();
            rig.controls.command(game::Command::Quit);
            rig.controls.command(game::Command::Retry);
            rig.controls
                .settings_key(game::Adjust::VolumeDown, crate::tuning::KeyEvent::Press);
            rig.controls
                .settings_key(game::Adjust::VolumeDown, crate::tuning::KeyEvent::Release);
            rig.decision_tick(Origin::Load).unwrap();
            assert!(!rig.controls.ingress().is_open(), "{ms} ms: the epoch");
            assert!(!rig.controls.leaving(), "{ms} ms");
            assert_eq!(rig.controls.take(), None, "{ms} ms");
            assert_eq!(rig.controls.tuning().snapshot().values, values, "{ms} ms");
        }
        assert!(!rig.controls.quitting());
    });
}

/// R3.1 (stage3's list): after the skinned screen's epoch, a press made
/// in it does nothing: a quit and a retry are gone and a settings key
/// held through the step steps nothing; fresh presses act.
#[test]
fn fresh_press_needed_after_skin_decision_epoch() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let mut d = choose(rig, &f.pick(false), 3);
        assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
        let values = rig.controls.tuning().snapshot().values;
        rig.controls.command(game::Command::Quit);
        rig.controls.command(game::Command::Retry);
        rig.controls
            .settings_key(game::Adjust::VolumeDown, crate::tuning::KeyEvent::Press);
        let end = d.deadline().unwrap();
        assert_eq!(
            d.classify(Origin::Ready, end, false, false, None),
            Verdict::Step
        );
        rig.deadline_step();
        assert!(rig.controls.ingress().is_open());
        assert_eq!(rig.controls.take(), None, "a press in the epoch acted");
        rig.controls
            .settings_key(game::Adjust::VolumeDown, crate::tuning::KeyEvent::Repeat);
        assert_eq!(rig.controls.tuning().snapshot().values, values);
        // Fresh presses act.
        rig.controls
            .settings_key(game::Adjust::VolumeDown, crate::tuning::KeyEvent::Release);
        rig.controls.command(game::Command::Quit);
        assert_eq!(rig.controls.take(), Some(game::Command::Quit));
    });
}

/// R4.1, R4.3 (stage3's list): the skin's build refused at 1,000 ms gives
/// the built-in presentation at once, the deadline at 1,500 ms from the
/// original choice, no presentation sent again and no second tone; at
/// 2,000 ms the screen ends at once on its route: the loading state on
/// the play path, the practice page once its chart is read.
#[test]
fn takeover_restarts_no_clock_and_plays_no_tone() {
    for (refused_ms, practice) in [(1_000, false), (2_000, false), (2_000, true)] {
        let dir = listed();
        flow(&dir, |rig, f| {
            f.prepared();
            let tones = rig.audio.control.failure_probe();
            let sent = rig.render.decisions_sent();
            let mut d = choose(rig, &f.pick(practice), 3);
            let origin = d.origin();
            assert!(wait_until(|| skins_after(&f.seen, 0) > 0));
            until_clock(rig, TimeUs(origin.0 + refused_ms * MS));
            f.seen.refuse_frame.store(true, Ordering::Release);
            // As App's look takes it, through the rig.
            let began = std::time::Instant::now();
            let t = loop {
                if let Some(t) = rig.take_decision_takeover() {
                    break t;
                }
                assert!(began.elapsed().as_secs() < 60, "no takeover");
                std::thread::yield_now();
            };
            assert_eq!(t.lifetime, DecisionLifetime(3));
            assert!(t.at.0 >= origin.0 + refused_ms * MS);
            let now = rig.now();
            assert!(d.apply_takeover(t, now));
            assert_eq!(d.origin(), origin, "a clock restarted");
            assert_eq!(d.deadline(), Some(TimeUs(origin.0 + 1_500 * MS)));
            assert_eq!(d.shown(), Some(DecisionShown::BuiltIn));
            // The page draws at once.
            let from = f.seen.len();
            assert!(wait_until(|| f.seen.len() > from + 2));
            assert_eq!(skins_after(&f.seen, from), 0);
            assert_eq!(sent(), 1, "a presentation sent again");
            assert_eq!(tones.tone_requests(), 1, "a second tone");
            if refused_ms == 1_000 {
                let at = |us| TimeUs(origin.0 + us);
                assert_eq!(
                    d.classify(Origin::Ready, at(1_500 * MS - 1), false, false, None),
                    Verdict::Continue
                );
                assert_eq!(
                    d.classify(Origin::Ready, at(1_500 * MS), false, false, None),
                    Verdict::Step
                );
            } else if practice {
                // Practice past its time waits for its read, then opens.
                assert_eq!(
                    d.classify(Origin::Read, now, false, false, None),
                    Verdict::Continue
                );
                assert!(d.waiting());
                assert_eq!(
                    d.classify(Origin::Jobs, now, false, false, None),
                    Verdict::Step
                );
            } else {
                assert_eq!(
                    d.classify(Origin::Ready, now, false, false, None),
                    Verdict::Step
                );
            }
        });
    }
}

/// O6: the idle wait the game thread parks in is unparked by the takeover's
/// producer: the skin's build refused on the render thread, after the
/// test thread parked for 60 s, wakes it long before then, and the
/// producer's own wake count rose.
#[test]
fn takeover_unparks_the_idle_wait() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let d = choose(rig, &f.pick(false), 3);
        assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
        assert!(wait_until(|| skins_after(&f.seen, 0) > 0));
        rig.controls.set_game(std::thread::current());
        let game = std::thread::current();
        rig.render.set_game_wake(Arc::new(move || game.unpark()));
        let probe = Arc::clone(&rig.park_probe);
        probe.store(false, Ordering::SeqCst);
        let far = TimeUs(rig.now().0 + 60_000 * MS);
        let seen = Arc::clone(&f.seen);
        std::thread::scope(|s| {
            s.spawn(|| {
                assert!(wait_until(|| probe.load(Ordering::SeqCst)));
                seen.refuse_frame.store(true, Ordering::Release);
            });
            // A wake by anything else parks again.
            while rig.render.takeovers_woken() == 0 && rig.now().0 < far.0 {
                rig.idle_wait(far);
            }
        });
        assert!(
            rig.now().0 < far.0,
            "the park was not ended by the takeover"
        );
        assert_eq!(rig.render.takeovers_woken(), 1);
    });
}

/// R1.5: a render thread that takes the choice's presentation only after
/// a short screen's step, with the target changed meanwhile, keeps the
/// choice: no refusal at its entry, the kind not disabled, and the slot
/// retired by its lifetime at a later boundary.
#[test]
fn delayed_render_short_screen_keeps_its_choice() {
    let dir = timed(200, 0, 0);
    flow(&dir, |rig, f| {
        f.prepared();
        let gate = rig.render.decision_gate();
        gate.hold(true);
        let mut d = choose(rig, &f.pick(false), 3);
        assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
        assert!(wait_until(|| gate.waiting()));
        let (slot, generation, _) = f.pinned().unwrap();
        let end = d.deadline().unwrap();
        assert_eq!(
            d.classify(Origin::Ready, end, false, false, None),
            Verdict::Step
        );
        rig.deadline_step();
        lock(&f.book).invalidate([1_280, 720]);
        gate.hold(false);
        assert!(wait_until(|| f.pinned().is_none()), "never retired");
        assert!(rig.render.fence(std::time::Duration::from_secs(60)));
        assert_eq!(rig.render.take_decision_refusal(), None, "refused at entry");
        assert_eq!(rig.render.take_decision_takeover(), None);
        assert!(
            matches!(
                lock(&f.book).state(slot),
                Some(LabelState::Cooling { generation: g, .. }
                    | LabelState::Ready { generation: g, .. }) if *g == generation
            ) || matches!(lock(&f.book).state(slot), Some(LabelState::Free)),
            "{:?}",
            lock(&f.book)
        );
    });
}

/// R1.5: a render thread that takes the presentation late, with the target
/// changed meanwhile, while the lifetime is still active, draws the slot
/// the choice claimed: skin frames, no takeover, the kind not disabled,
/// the slot still pinned by the lifetime.
#[test]
fn delayed_render_active_lifetime_keeps_its_claim() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let gate = rig.render.decision_gate();
        gate.hold(true);
        let d = choose(rig, &f.pick(false), 3);
        assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
        assert!(wait_until(|| gate.waiting()));
        let pinned = f.pinned().unwrap();
        lock(&f.book).invalidate([1_280, 720]);
        let from = f.seen.len();
        gate.hold(false);
        assert!(wait_until(|| skins_after(&f.seen, from) >= 3));
        assert_eq!(rig.render.take_decision_takeover(), None);
        assert_eq!(rig.render.take_decision_refusal(), None);
        assert_eq!(f.pinned(), Some(pinned));
    });
}

/// R0.3, R1.4: a choice made while the chart's labels are written but
/// their completion has not come draws the built-in presentation for
/// 1,500 ms; the slot is published only after it.
#[test]
fn choice_before_prep_completes_draws_builtin_for_1500() {
    let dir = listed();
    flow(&dir, |rig, f| {
        // The selection was entered before this hold: wait for the hold
        // to catch the fill, or the labels to be ready already.
        f.seen.hold_settle.store(true, Ordering::Release);
        assert!(wait_until(|| f
            .seen
            .skipped_settles
            .load(Ordering::Acquire)
            > 0
            || f.ready().is_some()));
        if f.ready().is_some() {
            // Published before the hold: ask for the labels again.
            f.extent.publish(1_280, 720);
            assert!(wait_until(|| f
                .seen
                .skipped_settles
                .load(Ordering::Acquire)
                > 0));
        }
        assert_eq!(f.ready(), None, "published before its completion");
        let d = choose(rig, &f.pick(false), 3);
        assert_eq!(d.shown(), Some(DecisionShown::BuiltIn));
        assert_eq!(d.deadline(), Some(TimeUs(d.origin().0 + 1_500 * MS)));
        assert_eq!(rig.render.decision_sent().and_then(|s| s.slot), None);
        assert_eq!(lock(&f.book).active(), Some(3));
        f.seen.hold_settle.store(false, Ordering::Release);
        assert!(wait_until(|| f.ready().is_some()));
    });
}

/// Repeat: the same chart chosen again after its screen ended, the cursor
/// not moved, pins the same generation again (the retired slot ready
/// again at its last frame's completion, its content kept) and the skin
/// draws.
#[test]
fn same_chart_chosen_again_without_cursor_move_draws_skin() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let mut d = choose(rig, &f.pick(false), 3);
        let first = f.pinned().unwrap();
        assert!(wait_until(|| skins_after(&f.seen, 0) > 0));
        let end = d.deadline().unwrap();
        assert_eq!(
            d.classify(Origin::Ready, end, false, false, None),
            Verdict::Step
        );
        rig.deadline_step();
        rig.entered_selection();
        assert!(wait_until(|| f.ready() == Some((first.0, first.1))));
        let from = f.seen.len();
        let d = choose(rig, &f.pick(false), 4);
        assert!(matches!(
            d.shown(),
            Some(DecisionShown::Skin { generation, .. }) if generation == first.1
        ));
        assert_eq!(f.pinned(), Some((first.0, first.1, 4)));
        assert!(wait_until(|| skins_after(&f.seen, from) >= 3));
        assert_eq!(rig.render.take_decision_refusal(), None);
    });
}

/// The labels' strings: a choice whose chart and labels' revision match a
/// ready slot prepared from other strings (equal digests of other labels)
/// does not pin it: the built-in presentation for 1,500 ms, the lifetime
/// active all the same and the slot still ready.
#[test]
fn choice_of_other_strings_under_an_equal_digest_draws_builtin() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let (slot, generation) = f.ready().unwrap();
        // The slot's generation as if prepared from other labels whose
        // digest is the chart's.
        {
            let prepared = rig.render.prepared_strings();
            let mut p = prepared.lock().unwrap();
            let other = crate::decision_side::decision_strings(&skin_model::ChartLabels {
                title: "Other".to_owned(),
                ..skin_model::ChartLabels::default()
            });
            p[slot] = Some((generation, other));
        }
        let d = choose(rig, &f.pick(false), 3);
        assert_eq!(
            d.shown(),
            Some(DecisionShown::BuiltIn),
            "pinned other labels"
        );
        assert_eq!(d.deadline(), Some(TimeUs(d.origin().0 + 1_500 * MS)));
        assert_eq!(f.pinned(), None);
        assert_eq!(f.ready(), Some((slot, generation)));
        assert_eq!(lock(&f.book).active(), Some(3));
    });
}

/// R5.1 (stage3's list) on the play path: the skinned screen follows its
/// own time whether the chart's load reached its last stage before the
/// scene ended or after the step: no loading frame before the step at
/// 3,000 ms, loading frames after it and no skin frame.
#[test]
fn chart_ready_before_and_after_scene_both_reach_loading() {
    use crate::loading_text::Stage;
    for before in [true, false] {
        let dir = listed();
        flow(&dir, |rig, f| {
            f.prepared();
            let mut d = choose(rig, &f.pick(false), 3);
            assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
            let origin = d.origin();
            let page = rig.selection.as_ref().unwrap().loading.clone().unwrap();
            let loading = rig.render.loading_drawn();
            assert!(wait_until(|| skins_after(&f.seen, 0) > 0));
            if before {
                until_clock(rig, TimeUs(origin.0 + 1_000 * MS));
                crate::selection::show_stage(&rig.render, Some(&page), Stage::Screen);
                let from = f.seen.skins(0);
                assert!(wait_until(|| skins_after(&f.seen, 0) > from + 2));
                assert_eq!(
                    rig.render.loading_drawn(),
                    loading,
                    "{before}: loading early"
                );
            }
            let end = d.deadline().unwrap();
            assert_eq!(end, TimeUs(origin.0 + 3_000 * MS));
            assert_eq!(
                d.classify(Origin::Ready, end, false, false, None),
                Verdict::Step
            );
            rig.deadline_step();
            if !before {
                crate::selection::show_stage(&rig.render, Some(&page), Stage::Screen);
            }
            assert!(
                wait_until(|| rig.render.loading_drawn() > loading + 2),
                "{before}: no loading"
            );
            let from = f.seen.len();
            let drawn = rig.render.loading_drawn();
            assert!(wait_until(|| rig.render.loading_drawn() > drawn + 2));
            assert_eq!(
                skins_after(&f.seen, from),
                0,
                "{before}: the skin after the step"
            );
        });
    }
}

/// Startup's kinds on `rig`'s render thread, the decision kind's from
/// `choice` through production's build and every other kind's a built
/// bundle: the full configured set of song selection.
fn configured(rig: &Rig, choice: &game::SkinChoice) -> crate::imports::Started {
    use crate::imports::{Build, KindOutcome, Launch, Summary, build, startup};
    use skin_model::ScreenKind;
    let built: Build = Arc::new(|kind, choice, autoplay, target, cancel| match kind {
        ScreenKind::Decision => build(kind, choice, autoplay, target, cancel),
        _ => KindOutcome::Bundle(
            crate::render_side::tests::bundle(),
            Summary {
                sources: vec!["s.json".to_owned()],
                files: Vec::new(),
                usage: skin_beatoraja::Usage::default(),
                notes: Vec::new(),
                suppressed: 0,
                chart_layer: None,
            },
            crate::screens::ScriptSlot::default(),
        ),
    });
    let choices = game::SkinChoices {
        play: Some(choice.clone()),
        selection: Some(choice.clone()),
        decision: Some(choice.clone()),
        result: Some(choice.clone()),
    };
    startup(
        &rig.link,
        &choices,
        Launch::Selection,
        false,
        &built,
        || false,
    )
    .unwrap()
}

/// A Lua decision skin with [`skin_dir`]'s font and labels, declaring the
/// 2,500 / 500 / 500 ms timing.
fn lua_skin_dir() -> TempDir {
    let dir = skin_dir();
    let flip = |y: i32, h: i32| 1_080 - y - h;
    let entry = format!(
        "local skin = {{ type = 6, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.scene = 2500 skin.input = 500 skin.fadeout = 500\n\
         skin.font = {{ {{ id = \"f\", path = \"ttf.ttf\" }} }}\n\
         skin.text = {{ {{ id = \"t0\", font = \"f\", size = 18, ref = 10 }},\n\
         {{ id = \"t1\", font = \"f\", size = 18, ref = 16 }} }}\n\
         skin.destination = {{\n\
         {{ id = \"t0\", dst = {{ {{ x = 100, y = {t0}, w = 1200, h = 18 }} }} }},\n\
         {{ id = \"t1\", dst = {{ {{ x = 100, y = {t1}, w = 1200, h = 18 }} }} }} }}\n\
         return skin\n",
        t0 = flip(100, 18),
        t1 = flip(600, 18),
    );
    std::fs::write(dir.path().join("s.luaskin"), entry).unwrap();
    dir
}

/// R6.3: self-made JSON and Lua decision skins, each installed by startup
/// through production's build beside test bundles of the other kinds (the
/// full configured set's run is `integrated`'s), draw through production:
/// the set installed with no notice, the result kind's skin fixed, the
/// choice pinning the chart's labels with the declared timing and the
/// title drawn lit from them. A decision skin the render thread refuses
/// (its labels' rows do not fit the region) leaves the run's notice, and
/// its choice draws the built-in presentation for 1,500 ms.
#[test]
fn decision_skin_json_and_lua_draw_through_production() {
    use crate::render_outline::{Value, decision_folder, text, ttf};
    // Each flow holds WARP's turn, then the import serial lock (the Lua
    // skin's VMs count, and are held, with the hooks' tests'), in the
    // order `warp_turn` documents.
    let held = || (crate::warp_turn::take(), crate::imports::tests::serial());
    let json = listed();
    let lua = lua_skin_dir();
    for (dir, entry) in [(&json, "s.json"), (&lua, "s.luaskin")] {
        let choice = crate::imports::tests::choice(dir.path(), entry);
        let started = std::cell::RefCell::new(None);
        let (turn, _serial) = held();
        flow_with(
            |clock| {
                let rig = Rig::bare(turn, clock);
                *started.borrow_mut() = Some(configured(&rig, &choice));
                rig
            },
            |rig, f| {
                let started = started.borrow_mut().take().unwrap();
                assert_eq!(started.notice, None, "{entry}");
                assert!(
                    matches!(
                        started.result,
                        Some(crate::result_skin::ResultStart::Fixed(..))
                    ),
                    "{entry}"
                );
                assert_eq!(rig.render.take_decision_refusal(), None, "{entry}");
                f.prepared();
                let d = choose(rig, &f.pick(false), 3);
                assert!(
                    matches!(
                        d.shown(),
                        Some(DecisionShown::Skin { scene, fadeout, .. })
                            if scene == TimeUs(2_500 * MS) && fadeout == TimeUs(500 * MS)
                    ),
                    "{entry}: {:?}",
                    d.shown()
                );
                assert!(wait_until(|| skins_after(&f.seen, 0) > 0), "{entry}");
                f.seen.probe.store(true, Ordering::Release);
                assert!(wait_until(|| !f.seen.lit.lock().unwrap().is_empty()));
                let lit = f.seen.lit.lock().unwrap()[0].1;
                assert!(lit > 0, "{entry}: the title not drawn");
            },
        );
    }
    // A real render refusal: three labels at em 256 do not fit the region.
    let misfit = decision_folder(
        &[(
            "ttf.ttf",
            crate::font_support::build(&crate::font_support::latin("Skin")),
        )],
        &[ttf()],
        &[
            text("f", 256, Value::Ref(10), [0, 0, 100, 256]),
            text("f", 256, Value::Ref(11), [0, 300, 100, 256]),
            text("f", 256, Value::Ref(16), [0, 600, 100, 256]),
        ],
    );
    let choice = crate::imports::tests::choice(misfit.path(), "s.json");
    let (turn, _serial) = held();
    flow_with(
        |clock| {
            let rig = Rig::bare(turn, clock);
            let started = configured(&rig, &choice);
            assert_eq!(started.notice, None);
            rig
        },
        |rig, f| {
            let notice = rig.render.take_decision_refusal().expect("the notice");
            assert!(notice.contains("rows"), "{notice}");
            let d = choose(rig, &f.pick(false), 3);
            assert_eq!(d.shown(), Some(DecisionShown::BuiltIn));
            assert_eq!(d.deadline(), Some(TimeUs(d.origin().0 + 1_500 * MS)));
            let from = f.seen.len();
            assert!(wait_until(|| f.seen.len() > from + 2));
            assert_eq!(skins_after(&f.seen, 0), 0);
        },
    );
}

/// O5, O7: the practice page's entry and a refused load's return
/// (`closed_epoch`, which App runs for every leave of a screen shown)
/// end the lifetime in the label book, and its slot is retired.
#[test]
fn page_entry_and_leave_end_the_lifetime() {
    for practice in [true, false] {
        let dir = listed();
        flow(&dir, |rig, f| {
            f.prepared();
            let d = choose(rig, &f.pick(practice), 3);
            assert!(matches!(d.shown(), Some(DecisionShown::Skin { .. })));
            assert_eq!(lock(&f.book).active(), Some(3));
            rig.closed_epoch();
            assert_eq!(lock(&f.book).active(), None, "{practice}: still active");
            assert!(
                wait_until(|| f.pinned().is_none()),
                "{practice}: never retired"
            );
        });
    }
}

/// O7 inside a rig's wait: the play's deadline step taken by a look of
/// the controls (the step latched there, so App's own look runs no step)
/// ends the lifetime in the label book, and its slot is retired.
#[test]
fn step_inside_a_rig_wait_ends_the_lifetime() {
    let dir = listed();
    flow(&dir, |rig, f| {
        f.prepared();
        let d = choose(rig, &f.pick(false), 3);
        let end = d.deadline().unwrap();
        rig.controls.watch_decision(crate::controls::DecisionWatch {
            decision: d,
            origin: Origin::Load,
            reveal: Box::new(|| {}),
            failures: rig.failures.clone(),
            screen: Some(rig.render.recovery_handle()),
        });
        until_clock(rig, end);
        assert_eq!(rig.controls.look(), Some(Verdict::Step));
        let d = rig.controls.unwatch_decision().unwrap();
        assert!(d.stepped());
        assert_eq!(lock(&f.book).active(), None, "the step left it active");
        assert!(wait_until(|| f.pinned().is_none()), "never retired");
    });
}
