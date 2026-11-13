//! The loading state's first frame held while it would be the pending
//! presentation (design.md "The pending presentation"), and the validity
//! of the image it holds. Each test sets the hold's bound; one no test
//! reaches leaves the hold to end only by what the test does, so no
//! verdict waits on time. The frame gates step one frame at a time, from
//! before its token to its stamp.

use std::sync::atomic::Ordering::SeqCst;

use super::*;
use crate::render_side::{Extent, FIRST_FRAME_HOLD, HELD_PACE, held_wait};

const MS: i64 = 1_000;

/// A bound no test reaches.
const NEVER: i64 = 3_600_000 * MS;

/// A render thread on new installers, play's skin pending, following a
/// window of 1920×1080, the hold's bound `bound` µs, a page presented and
/// stamped valid.
fn rig(
    bound: i64,
) -> (
    Arc<Chart>,
    Installers,
    super::super::RenderLink,
    Arc<Extent>,
) {
    let (chart, screens, link) = pending_link();
    link.shared.hold_bound.store(bound, SeqCst);
    let extent = Arc::new(Extent::default());
    extent.publish(1_920, 1_080);
    link.follow(&extent);
    show_page(&link);
    (chart, screens, link, extent)
}

/// Shows a page and waits until a frame of it is stamped valid.
fn show_page(link: &super::super::RenderLink) {
    link.send(RenderMsg::Menu(Box::new(one_line("page"))));
    let from = stamps(link).0;
    assert!(wait_until(|| {
        let (count, valid) = stamps(link);
        count >= from + 2 && valid
    }));
}

/// The stamps so far, and whether the last left the image valid.
fn stamps(link: &super::super::RenderLink) -> (u64, bool) {
    let [count, last] = &link.shared.stamps;
    let count = count.load(SeqCst);
    (count, last.load(SeqCst) == 1)
}

/// The held iterations' waits so far.
fn waits(link: &super::super::RenderLink) -> u64 {
    link.shared.held[1].load(SeqCst)
}

/// The held iterations begun so far.
fn entries(link: &super::super::RenderLink) -> u64 {
    link.shared.held[0].load(SeqCst)
}

fn pending(screens: &Installers) -> u32 {
    screens.pending.load(SeqCst)
}

/// Shows a loading lifetime and waits until it is held.
fn show_held(link: &super::super::RenderLink) {
    let from = waits(link);
    link.loading().show(one_line("loading"));
    assert!(wait_until(|| waits(link) >= from + 3));
}

/// With a frame waiting before its token (gate 0 held), runs exactly
/// that frame to its stamp and holds the next one before its token:
/// whether its stamp left the image valid.
fn step(link: &super::super::RenderLink) -> bool {
    let [before_token, _, before_stamp] = &link.shared.frame_gates;
    assert!(wait_until(|| before_token.waiting()));
    before_stamp.hold(true);
    before_token.hold(false);
    assert!(wait_until(|| before_stamp.waiting()));
    before_token.hold(true);
    let count = stamps(link).0;
    before_stamp.hold(false);
    assert!(wait_until(|| stamps(link).0 > count));
    let valid = stamps(link).1;
    assert!(wait_until(|| before_token.waiting()));
    valid
}

/// Every gate let go.
fn release(link: &super::super::RenderLink) {
    for gate in &link.shared.frame_gates {
        gate.hold(false);
    }
}

/// TB1: a lifetime shown over a valid image is held, nothing presented
/// or acknowledged, until its staging is answered: its first presented
/// frame is the staged skin's, acknowledged then; the placeholder is never
/// drawn.
#[test]
fn the_first_frame_waits_for_the_skin() {
    let (chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    assert_eq!(pending(&screens), 0);
    assert_eq!(screens.loading.load(SeqCst), 0);
    assert_eq!(link.loading_ack(), None);
    let answered = link.clock.now();
    assert_eq!(
        specialized(&link, &chart, &bundle()).unwrap(),
        Preparation::Ready
    );
    assert!(wait_until(|| link.loading_ack().is_some()));
    let (lifetime, at) = link.loading_ack().unwrap();
    assert!(at >= answered, "{at:?} {answered:?}");
    let staged = screens.staged_frames.lock().unwrap().clone();
    assert_eq!(staged.first().map(|&(l, _)| l), Some(lifetime));
    assert_eq!(pending(&screens), 0, "the placeholder never drawn");
    link.end_loading().unwrap();
    link.exit();
}

/// TB2: a loading frame returned not presented acknowledges nothing and
/// ends the hold: the staging discarded, the pending presentation is
/// drawn at once, never held again.
#[test]
fn an_unpresented_frame_ends_the_hold() {
    let (chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    screens.unpresented.store(true, SeqCst);
    assert_eq!(
        specialized(&link, &chart, &bundle()).unwrap(),
        Preparation::Ready
    );
    assert!(wait_until(|| !screens
        .staged_frames
        .lock()
        .unwrap()
        .is_empty()));
    assert_eq!(link.loading_ack(), None, "nothing presented");
    let held = entries(&link);
    link.discard_attempt();
    assert!(wait_until(|| pending(&screens) >= 2));
    screens.unpresented.store(false, SeqCst);
    assert!(wait_until(|| link.loading_ack().is_some()));
    assert_eq!(entries(&link), held, "no hold after it");
    link.end_loading().unwrap();
    link.exit();
}

/// TB3: a play skin that is not pending is never held: a selected
/// declarative skin and the built-in presentation draw the first loading
/// frame at once.
#[test]
fn an_ordinary_skin_is_not_held() {
    for selected in [true, false] {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn_bare();
        link.shared.hold_bound.store(NEVER, SeqCst);
        link.load(Arc::clone(&chart), None).unwrap();
        if selected {
            assert_eq!(
                link.select(ScreenKind::Play, Some(bundle()))
                    .wait()
                    .unwrap(),
                Preparation::Ready
            );
        }
        show_page(&link);
        link.loading().show(one_line("loading"));
        assert!(
            wait_until(|| screens.loading.load(SeqCst) >= 2),
            "{selected}"
        );
        assert!(link.loading_ack().is_some());
        assert_eq!(entries(&link), 0, "{selected}");
        link.end_loading().unwrap();
        link.exit();
    }
}

/// TB4, TB13: the held waits' schedule, shown at 1 ms with the first
/// check at 2 ms, off the waits' phase: checks every 4 ms to 498 ms, the
/// last wait clamped to the 3 ms left, the hold ending at 501 ms, never
/// later.
#[test]
fn the_held_waits_stop_at_the_bound() {
    let (shown, bound) = (TimeUs(MS), FIRST_FRAME_HOLD);
    let mut now = TimeUs(2 * MS);
    let mut checks = Vec::new();
    while let Some(wait) = held_wait(now, shown, bound) {
        assert!(wait <= HELD_PACE, "{wait:?}");
        checks.push(now);
        now = TimeUs(now.0 + wait.0);
    }
    assert_eq!(now, TimeUs(501 * MS), "the hold ends at the bound");
    assert_eq!(checks.len(), 125);
    assert_eq!(checks[..2], [TimeUs(2 * MS), TimeUs(6 * MS)]);
    assert_eq!(checks.last(), Some(&TimeUs(498 * MS)));
    assert_eq!(
        held_wait(TimeUs(498 * MS), shown, bound),
        Some(TimeUs(3 * MS))
    );
    assert_eq!(held_wait(TimeUs(501 * MS), shown, bound), None);
}

/// The hold reads `elapsed` µs since its showing in place of the clock,
/// from now on.
fn hold_at(link: &super::super::RenderLink, elapsed: i64) {
    link.shared.hold_elapsed.store(elapsed, SeqCst);
}

/// TB4: with no staging the hold ends at its bound from the showing, not
/// a microsecond before: held at the bound less 1 µs, the pending
/// presentation drawn and acknowledged once the bound is reached. The
/// hold reads its time from the test, so a render thread descheduled
/// anywhere changes no verdict.
#[test]
fn the_hold_is_bounded() {
    let (_chart, screens, link, _extent) = rig(FIRST_FRAME_HOLD.0);
    hold_at(&link, 0);
    show_held(&link);
    hold_at(&link, FIRST_FRAME_HOLD.0 - 1);
    let from = waits(&link);
    assert!(wait_until(|| waits(&link) >= from + 3));
    assert_eq!(pending(&screens), 0);
    assert_eq!(link.loading_ack(), None);
    hold_at(&link, FIRST_FRAME_HOLD.0);
    assert!(wait_until(|| link.loading_ack().is_some()));
    assert!(pending(&screens) >= 1);
    link.end_loading().unwrap();
    link.exit();
}

/// TB13: the hold waits on the clock between its looks instead of
/// spinning, and its iterations take no frame of the pacing: a frame's
/// wait is asked neither while held nor twice after it, so the pending
/// presentation drawn at the bound takes one wait. Its looks progress
/// (asserted apart), and no more of them run than one per pace.
#[test]
fn the_hold_paces() {
    let (_chart, screens, link, _extent) = rig(NEVER);
    hold_at(&link, 0);
    screens.permits.store(0, SeqCst);
    screens.pace.store(true, SeqCst);
    assert!(wait_until(|| screens.held.load(SeqCst)));
    let asked = screens.waits.load(SeqCst);
    link.loading().show(one_line("loading"));
    // The page's frame waiting, then the frame drawn after the hold.
    screens.permits.store(2, SeqCst);
    let from = waits(&link);
    assert!(
        wait_until(|| waits(&link) >= from + 3),
        "the hold progresses"
    );
    assert_eq!(screens.waits.load(SeqCst), asked, "a wait asked while held");
    // At most one look per pace over a window. Both counts are read
    // inside the window, so every look between them lies in it; with the
    // time since the showing at 0 each wait is the whole pace, never a
    // clamped one, and a sleep never ends early, so k looks need k - 1
    // whole paces between them. A thread descheduled anywhere only lowers
    // the count: this fails a spin, never the hold.
    let began = Instant::now();
    let first = entries(&link);
    std::thread::sleep(Duration::from_millis(100));
    let last = entries(&link);
    let span = began.elapsed();
    let paces = u64::try_from(span.as_micros()).unwrap() / HELD_PACE.0.unsigned_abs();
    assert!(
        last - first <= paces + 1,
        "{} looks in {span:?}",
        last - first
    );
    hold_at(&link, NEVER);
    assert!(wait_until(|| link.loading_ack().is_some()));
    assert!(wait_until(|| screens.held.load(SeqCst)));
    assert_eq!(pending(&screens), 1);
    assert_eq!(
        screens.waits.load(SeqCst),
        asked + 2,
        "one wait for the frame"
    );
    screens.pace.store(false, SeqCst);
    link.end_loading().unwrap();
    link.exit();
}

/// TB13: on a target paced by the clock (no frame waitable), a held
/// iteration takes only its own wait, never the frame's 16 ms on top.
#[test]
fn an_unpaced_hold_takes_no_frame_pacing() {
    let (_chart, _screens, link, _extent) = rig(NEVER);
    hold_at(&link, 0);
    show_held(&link);
    let (paced, from) = (link.shared.clock_paced.load(SeqCst), entries(&link));
    assert!(wait_until(|| entries(&link) >= from + 5));
    assert_eq!(link.shared.clock_paced.load(SeqCst), paced);
    link.end_loading().unwrap();
    link.exit();
}

/// The lifetime whose first frame was visited last, and its showing.
fn first_shown(link: &super::super::RenderLink) -> Option<(u64, TimeUs)> {
    *link.shared.first_shown.lock().unwrap()
}

/// TB5: the hold counts from the showing: a held lifetime's from its
/// reveal, however long it was held unshown, and a lifetime begun unheld
/// (a practice Start) from its beginning.
#[test]
fn the_hold_counts_from_the_showing() {
    let (_chart, _screens, link, _extent) = rig(NEVER);
    link.begin_loading_held(&chosen("One"), None);
    let lifetime = link.loading_lifetime();
    // Held unshown for a while.
    std::thread::sleep(Duration::from_millis(50));
    let revealed = link.clock.now();
    link.reveal_loading();
    assert!(wait_until(
        || first_shown(&link).is_some_and(|(l, _)| l == lifetime)
    ));
    let (_, shown) = first_shown(&link).unwrap();
    assert_eq!(link.shared.current_origin(), (lifetime, Some(shown)));
    assert!(shown >= revealed, "{shown:?} {revealed:?}");
    assert!(waits(&link) > 0 || wait_until(|| waits(&link) > 0));
    link.end_loading().unwrap();

    show_page(&link);
    let began = link.clock.now();
    link.begin_loading(&chosen("Two"), None);
    let lifetime = link.loading_lifetime();
    link.loading()
        .report(LoadInput::Stage(Stage::Chart), Some(one_line("two")));
    assert!(wait_until(
        || first_shown(&link).is_some_and(|(l, _)| l == lifetime)
    ));
    let (_, shown) = first_shown(&link).unwrap();
    assert_eq!(link.shared.current_origin(), (lifetime, Some(shown)));
    assert!(shown >= began, "{shown:?} {began:?}");
    link.end_loading().unwrap();
    link.exit();
}

/// TB6: a size notice during the hold ends it, whatever the size: another
/// one, the same one (a scale change keeping the physical size), and a
/// minimize then a restore to the same size; the pending presentation is
/// drawn then.
#[test]
fn a_size_notice_ends_the_hold() {
    let notices: [(&str, &[(u32, u32)]); 3] = [
        ("another size", &[(1_280, 720)]),
        ("the same size", &[(1_920, 1_080)]),
        ("minimized and restored", &[(0, 0), (1_920, 1_080)]),
    ];
    for (what, sizes) in notices {
        let (_chart, screens, link, extent) = rig(NEVER);
        show_held(&link);
        for &(w, h) in sizes {
            extent.publish(w, h);
        }
        assert!(wait_until(|| pending(&screens) >= 1), "{what}");
        assert!(wait_until(|| link.loading_ack().is_some()), "{what}");
        link.end_loading().unwrap();
        link.exit();
    }
}

/// TB6: a lost device during the hold, with no frame result and no
/// notice, ends it by the screen's failure path: the screen dropped and
/// the failure flagged for the game, which builds it again.
#[test]
fn a_lost_device_ends_the_hold() {
    let (_chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    screens.lost.store(true, SeqCst);
    assert!(wait_until(|| link.failed()));
    let held = entries(&link);
    screens.lost.store(false, SeqCst);
    link.prepare().unwrap();
    assert_eq!(screens.builds.load(SeqCst), 2);
    assert_eq!(entries(&link), held);
    link.exit();
}

/// TB6: an image a frame left broken before the showing is not held: the
/// page's frames returned not presented (an outdated swap chain), the
/// loading state is shown with no successful redraw, and its pending
/// presentation is drawn at once.
#[test]
fn a_broken_image_is_not_held() {
    let (_chart, screens, link, _extent) = rig(NEVER);
    screens.unpresented.store(true, SeqCst);
    let from = stamps(&link).0;
    assert!(wait_until(|| {
        let (count, valid) = stamps(&link);
        count > from + 1 && !valid
    }));
    link.loading().show(one_line("loading"));
    assert!(wait_until(|| pending(&screens) >= 2));
    assert_eq!(entries(&link), 0);
    screens.unpresented.store(false, SeqCst);
    link.end_loading().unwrap();
    link.exit();
}

/// TB6: a same-size notice between a frame's token and its presentation
/// leaves the image it presents invalid, whether the notice comes before
/// the draw (the frame past its extent read) or after the present
/// (before its stamp); the loading state shown next, its message taken
/// before the notice's wake, is not held.
#[test]
fn a_notice_around_the_stamp_breaks_the_image() {
    for gate in [1, 2] {
        let (_chart, screens, link, extent) = rig(NEVER);
        let held = &link.shared.frame_gates[gate];
        held.hold(true);
        assert!(wait_until(|| held.waiting()), "{gate}");
        link.loading().show(one_line("loading"));
        extent.publish(1_920, 1_080);
        held.hold(false);
        assert!(wait_until(|| pending(&screens) >= 1), "{gate}");
        assert_eq!(entries(&link), 0, "{gate}");
        link.end_loading().unwrap();
        link.exit();
    }
}

/// TB6: a size published with its notice uncounted yet (the publication
/// paused between its size and its count), read by a frame that took its
/// token there, leaves that frame's image invalid once the count lands:
/// checked right after its stamp, before any later frame reads the size.
#[test]
fn a_size_comes_with_its_notice() {
    let (_chart, _screens, link, extent) = rig(NEVER);
    let [before_token, _, before_stamp] = &link.shared.frame_gates;
    before_token.hold(true);
    assert!(wait_until(|| before_token.waiting()));
    extent.counting.hold(true);
    let publisher = {
        let extent = Arc::clone(&extent);
        std::thread::spawn(move || extent.publish(1_280, 720))
    };
    assert!(wait_until(|| extent.counting.waiting()));
    // The frame takes its token and the new size, and presents.
    before_stamp.hold(true);
    before_token.hold(false);
    assert!(wait_until(|| before_stamp.waiting()));
    before_token.hold(true);
    // The count lands before the frame's stamp.
    extent.counting.hold(false);
    publisher.join().unwrap();
    let count = stamps(&link).0;
    before_stamp.hold(false);
    assert!(wait_until(|| stamps(&link).0 > count));
    assert!(
        !stamps(&link).1,
        "stamped valid on a notice it did not count"
    );
    release(&link);
    link.exit();
}

/// TB6: a resize applied and a frame presented in the same iteration
/// leaves the image invalid; the next frame on the stable target makes
/// it valid again, and a loading state shown then is held.
#[test]
fn a_redraw_restores_the_image() {
    let (_chart, screens, link, extent) = rig(NEVER);
    let before_token = &link.shared.frame_gates[0];
    before_token.hold(true);
    assert!(wait_until(|| before_token.waiting()));
    extent.publish(1_280, 720);
    assert!(!step(&link), "resized and presented in one frame");
    assert!(step(&link), "presented again on the stable target");
    release(&link);
    show_held(&link);
    assert_eq!(pending(&screens), 0);
    link.end_loading().unwrap();
    link.exit();
}

/// TB7: a successor lifetime holds on its own: lifetime 1 cleared while
/// held with nothing presented, lifetime 2 is held from its own showing;
/// and once a lifetime's frame is presented it is never held again, its
/// staging dropped or not.
#[test]
fn a_successor_lifetime_holds_on_its_own() {
    let (chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    let first = link.loading_lifetime();
    link.clear_loading();
    let shown = link.clock.now();
    link.loading().show(one_line("two"));
    let second = link.loading_lifetime();
    assert_ne!(first, second);
    assert!(wait_until(
        || first_shown(&link).is_some_and(|(l, _)| l == second)
    ));
    let (_, at) = first_shown(&link).unwrap();
    assert_eq!(link.shared.current_origin(), (second, Some(at)));
    assert!(at >= shown, "its own showing: {at:?} {shown:?}");
    let from = waits(&link);
    assert!(wait_until(|| waits(&link) >= from + 3), "held on its own");
    assert_eq!(link.loading_ack(), None);
    link.end_loading().unwrap();

    show_page(&link);
    show_held(&link);
    assert_eq!(
        specialized(&link, &chart, &bundle()).unwrap(),
        Preparation::Ready
    );
    let third = link.loading_lifetime();
    assert!(wait_until(|| link
        .loading_ack()
        .is_some_and(|(l, _)| l == third)));
    let held = entries(&link);
    let from = pending(&screens);
    link.discard_attempt();
    assert!(wait_until(|| pending(&screens) >= from + 2));
    assert_eq!(entries(&link), held, "never held again");
    link.end_loading().unwrap();
    link.exit();
}

/// TB1, TB2: a staging answered during the hold and dropped before any
/// frame drew it (its discard taken right after the answer) ends
/// nothing: no frame is built, the hold goes on over the valid image, and
/// nothing is acknowledged.
#[test]
fn a_staging_dropped_before_its_frame_keeps_the_hold() {
    let (chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    let answered = &link.shared.answer_gate;
    answered.hold(true);
    assert_eq!(
        specialized(&link, &chart, &bundle()).unwrap(),
        Preparation::Ready
    );
    assert!(wait_until(|| answered.waiting()));
    link.discard_attempt();
    answered.hold(false);
    let from = waits(&link);
    assert!(wait_until(|| waits(&link) >= from + 3), "still held");
    assert!(screens.staged_frames.lock().unwrap().is_empty());
    assert_eq!(pending(&screens), 0);
    assert_eq!(link.loading_ack(), None);
    link.end_loading().unwrap();
    link.exit();
}

/// TB6: a same-size notice after the outgoing frame's stamp and before
/// the lifetime's first visit (the loading state's message taken by the
/// next iteration, held before its token) leaves that frame's image
/// invalid: the first visit is not held.
#[test]
fn a_notice_before_the_first_visit_breaks_the_image() {
    let (_chart, screens, link, extent) = rig(NEVER);
    let [before_token, _, before_stamp] = &link.shared.frame_gates;
    before_stamp.hold(true);
    assert!(wait_until(|| before_stamp.waiting()));
    link.loading().show(one_line("loading"));
    before_token.hold(true);
    let count = stamps(&link).0;
    before_stamp.hold(false);
    assert!(wait_until(|| stamps(&link).0 > count));
    assert!(stamps(&link).1, "the outgoing frame stamped valid");
    assert!(wait_until(|| before_token.waiting()));
    extent.publish(1_920, 1_080);
    before_token.hold(false);
    assert!(wait_until(|| pending(&screens) >= 1));
    assert_eq!(entries(&link), 0);
    link.end_loading().unwrap();
    link.exit();
}

/// TB8: a lifetime cleared during the hold, a stale update of it queued
/// before the clear, takes no frame's pacing on a target paced by the
/// clock: the held iteration that finds it ended goes idle at once. The
/// iteration is held past its extent read while the update and the clear
/// are queued, and the next one, which takes the stale update, is held
/// before its pacing.
#[test]
fn a_cleared_hold_takes_no_frame_pacing() {
    let (_chart, _screens, link, _extent) = rig(NEVER);
    hold_at(&link, 0);
    show_held(&link);
    let [before_token, after_extent, _] = &link.shared.frame_gates;
    after_extent.hold(true);
    assert!(wait_until(|| after_extent.waiting()));
    let paced = link.shared.clock_paced.load(SeqCst);
    link.loading()
        .report(LoadInput::Stage(Stage::Chart), Some(one_line("stale")));
    link.clear_loading();
    before_token.hold(true);
    after_extent.hold(false);
    assert!(wait_until(|| before_token.waiting()));
    assert_eq!(link.shared.clock_paced.load(SeqCst), paced);
    release(&link);
    let _ = link.end_loading();
    link.exit();
}

/// TB8, TB9: the game's ways out are served during the hold, none
/// waiting for a presentation: the end, a clear followed by the page
/// again, and the thread's exit.
#[test]
fn exits_during_the_hold_are_served() {
    let (_chart, _screens, link, _extent) = rig(NEVER);
    show_held(&link);
    link.end_loading().unwrap();
    assert_eq!(link.loading_ack(), None);

    show_page(&link);
    show_held(&link);
    link.clear_loading();
    show_page(&link);

    show_held(&link);
    assert_eq!(link.exit(), super::super::RenderEnd::Joined);
}

/// TB10: a first frame the skin refuses ends the hold with no
/// acknowledgement: the refusal is latched as before and nothing of the
/// lifetime is held or drawn again.
#[test]
fn a_refused_first_frame_ends_the_hold() {
    let (chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    let lifetime = link.loading_lifetime();
    screens.refuse_loading.store(true, SeqCst);
    assert_eq!(
        specialized(&link, &chart, &bundle()).unwrap(),
        Preparation::Ready
    );
    assert!(wait_until(|| link.loading_refused(lifetime).is_some()));
    let held = entries(&link);
    let drawn = screens.loading.load(SeqCst);
    show_page(&link);
    assert_eq!(link.loading_ack(), None);
    assert_eq!(entries(&link), held);
    assert_eq!(screens.loading.load(SeqCst), drawn);
    assert!(link.end_loading().is_err());
    link.exit();
}

/// TB14: whole held iterations, the loop's top included, allocate nothing
/// on the render thread, read by the thread itself as each begins; the
/// page's frames before them, which note their lines, did (the positive
/// control).
#[test]
fn the_hold_alloc() {
    let (_chart, screens, link, _extent) = rig(NEVER);
    show_held(&link);
    let read = || link.shared.held_allocations.load(SeqCst);
    let from = entries(&link);
    let before = read();
    assert!(before > 0, "the page's frames counted");
    assert!(wait_until(|| entries(&link) >= from + 20));
    assert_eq!(read() - before, 0);
    assert_eq!(pending(&screens), 0);
    link.end_loading().unwrap();
    link.exit();
}
