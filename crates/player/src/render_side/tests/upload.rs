//! The uploader's steps on a real screen, on WARP through the render
//! thread (design.md "Memory Budget", the uploader; V10f): a play
//! staging whose own array takes several atlas steps, each step's
//! completion held 2 ms, is stepped by the passes that present nothing
//! with no message wait, no `CANCEL_POLL` and no `HELD_PACE` sleep while
//! a step is ready or in flight, and its answer leaves in the pass whose
//! boundary saw its last step complete. The staged attempt's chart
//! images step the same way, one band a step, and a publication made
//! stale after its first band (a renewal, a discard, a lost device) is
//! answered Unavailable with nothing more written (V10f (iv), V3).

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use super::result::Rig;
use super::*;
use crate::render_decl::{Look, solid_bundle};
use crate::render_side::WaitKind;

/// A play skin whose own array takes three atlas steps: one 3 500² cell
/// on a 3 584-texel page, 65 MiB of writes.
fn big(target: &render::Renderer) -> Arc<render::SkinBundle> {
    let b = solid_bundle(target, &Look::left([0, 255, 0, 255]), [3_500, 3_500]);
    assert_eq!(b.side(), 3_584);
    b
}

/// The loop's waits by kind when the last attempt was taken (`0`) and
/// when it was answered (`1`).
fn waits(rig: &Rig, at: usize) -> [u64; 4] {
    rig.link.shared.attempt_waits[at]
        .each_ref()
        .map(|w| w.load(SeqCst))
}

/// V10f (i): after a menu page (the wait page, Idle), a staging of three
/// atlas steps, each completion delayed 2 ms: between the request and its
/// answer no message or `CANCEL_POLL` wait, every wait a completion wait,
/// and the answer in the pass that saw the last step complete.
#[test]
fn an_idle_staging_steps_without_sleeping_and_answers_in_its_last_pass() {
    let rig = Rig::new();
    rig.faults.step_delay_us.store(2_000, SeqCst);
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let target = crate::render_common::offscreen(1_920, 1_080);
    let staged = big(&target);
    rig.link.send(RenderMsg::Menu(Box::new(one_line("wait"))));
    let answer = rig
        .link
        .prepare_attempt_with(
            super::titled(&chart),
            super::super::AttemptSkin::Specialized(staged),
            || false,
        )
        .unwrap();
    assert_eq!(answer, Preparation::Ready);
    let (before, after) = (waits(&rig, 0), waits(&rig, 1));
    let kind = |k: WaitKind| after[k as usize] - before[k as usize];
    assert_eq!(kind(WaitKind::CancelPoll), 0, "no CANCEL_POLL wait");
    assert_eq!(kind(WaitKind::Message), 0, "no message wait");
    assert_eq!(kind(WaitKind::HeldPace), 0);
    assert!(
        kind(WaitKind::Completion) >= 3,
        "a completion wait a step: {after:?} from {before:?}"
    );
    let (staged, answered) = (
        rig.link.shared.staged_pass.load(SeqCst),
        rig.link.shared.answered_pass.load(SeqCst),
    );
    assert!(staged > 0);
    assert_eq!(
        answered, staged,
        "answered in the pass that saw its last step"
    );
    rig.exit();
}

/// V10f (iii): the staging of (i) under a held lifetime, the hold's
/// clock 1 ms before its bound as the staging is taken and moved 400 µs at
/// each look of the uploader's after a pass's first: the first atlas step
/// is closed with the writes it had when its writes reached the bound,
/// fewer than a step's eight; the hold then ends, its pending
/// presentation acknowledged while the staging still steps, and the
/// remaining writes go in later steps.
#[test]
fn a_held_staging_stops_writing_at_the_holds_bound() {
    use crate::render_side::FIRST_FRAME_HOLD;
    let rig = Rig::new();
    rig.faults.step_delay_us.store(2_000, SeqCst);
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    assert_eq!(
        rig.link.pend(ScreenKind::Play).wait().unwrap(),
        Preparation::Ready
    );
    let target = crate::render_common::offscreen(1_920, 1_080);
    let staged = big(&target);
    // A page presented and stamped valid, then the lifetime shown over it
    // and held on the test's time.
    rig.link.send(RenderMsg::Menu(Box::new(one_line("page"))));
    let [stamps, valid] = &rig.link.shared.stamps;
    let from = stamps.load(SeqCst);
    assert!(wait_until(
        || stamps.load(SeqCst) >= from + 2 && valid.load(SeqCst) == 1
    ));
    rig.link.shared.hold_elapsed.store(0, SeqCst);
    let from = rig.link.shared.held[1].load(SeqCst);
    rig.link.loading().show(one_line("loading"));
    assert!(wait_until(
        || rig.link.shared.held[1].load(SeqCst) >= from + 3
    ));
    assert_eq!(rig.link.loading_ack(), None, "held");
    rig.link
        .shared
        .hold_elapsed
        .store(FIRST_FRAME_HOLD.0 - 1_000, SeqCst);
    rig.link.shared.write_clock_step.store(400, SeqCst);
    let answer = rig
        .link
        .prepare_attempt_with(
            super::titled(&chart),
            super::super::AttemptSkin::Specialized(staged),
            || false,
        )
        .unwrap();
    assert_eq!(answer, Preparation::Ready);
    let (ack, answered) = (rig.link.loading_ack(), rig.link.clock.now());
    let log = rig.faults.step_log.lock().unwrap().clone();
    assert!(log.len() >= 3, "{log:?}");
    let first = log[0];
    assert!(
        (1..8).contains(&first),
        "the first step closed at the bound: {log:?}"
    );
    assert!(log[1..].iter().all(|&n| n > 0), "{log:?}");
    let (_, at) = ack.expect("the pending presentation acknowledged");
    assert!(at <= answered, "{at:?} {answered:?}");
    rig.exit();
}

/// A green play skin keeping a chart layer, over its own atlas.
fn charted(target: &render::Renderer) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let dir = tempfile::tempdir().unwrap();
    let look = Look {
        chart: true,
        ..Look::left([0, 255, 0, 255])
    };
    let (b, _) = crate::render_decl::bundle(target, dir.path(), &look);
    assert!(b.chart_layer().is_some());
    (dir, b)
}

/// A stage file and a back image as large as their slots of a chart layer
/// of side `side`, read and fitted.
fn chart_set(side: u32) -> (tempfile::TempDir, Arc<crate::chart_images::ChartImages>) {
    use crate::chart_images::{Want, derive_set, read_sources};
    let d = tempfile::tempdir().unwrap();
    let s = side / 2 - 8;
    for (name, rgb) in [("s.png", [200, 10, 10]), ("b.png", [10, 10, 200])] {
        let file = std::fs::File::create(d.path().join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), s, s);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let [r, g, b] = rgb;
        let texels = [r, g, b, 255].repeat(usize::try_from(s * s).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
    }
    let want = Want {
        dir: d.path().to_owned(),
        names: ["s.png".to_owned(), "b.png".to_owned(), String::new()],
        play: None,
        result: None,
    };
    let sources = read_sources(&want, None, &|| false);
    let set = derive_set(&sources, side, &|| false).unwrap();
    assert!(set.fitted()[..2].iter().all(Option::is_some));
    (d, Arc::new(set))
}

/// After the wait page (Idle), an attempt staged with a play skin keeping
/// a chart layer: its request, answered Ready.
fn staged_attempt(rig: &Rig, skin: Arc<render::SkinBundle>) -> u64 {
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    rig.link.send(RenderMsg::Menu(Box::new(one_line("wait"))));
    let mut pending = rig.link.begin_attempt(
        super::titled(&chart),
        super::super::AttemptSkin::Specialized(skin),
    );
    let request = pending.request();
    let answer = loop {
        if let Some(a) = pending.wait_until(None, || false) {
            break a.unwrap().preparation;
        }
    };
    assert_eq!(answer, Preparation::Ready);
    request
}

/// Publishes `set` for the staged attempt `request` in the current
/// generation, answering on the receiver returned.
fn publish(
    rig: &Rig,
    request: u64,
    set: &Arc<crate::chart_images::ChartImages>,
) -> std::sync::mpsc::Receiver<crate::chart_images::Ack> {
    let generation = rig.link.chart_generation().load(SeqCst);
    rig.link
        .publish_chart_images(generation, Some(request), Some(Arc::clone(set)))
}

/// V10f (iv), M5: in Idle, the staged attempt's chart images, each band's
/// completion delayed 2 ms: between the publication and its answer no
/// message, `CANCEL_POLL` or `HELD_PACE` wait, a completion wait a band,
/// and the table published.
#[test]
fn idle_chart_images_step_without_sleeping_and_answer_in_their_last_pass() {
    let rig = Rig::new();
    rig.faults.step_delay_us.store(2_000, SeqCst);
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_d, skin) = charted(&target);
    let (_i, set) = chart_set(skin.side());
    let request = staged_attempt(&rig, skin);
    let ack = publish(&rig, request, &set)
        .recv_timeout(Duration::from_secs(20))
        .unwrap();
    assert!(ack.answer.is_ok(), "{ack:?}");
    let bands = rig.faults.chart_bands.load(SeqCst);
    assert!(bands >= 4, "{bands} bands");
    let (before, after) = (waits(&rig, 2), waits(&rig, 3));
    let kind = |k: WaitKind| after[k as usize] - before[k as usize];
    assert_eq!(kind(WaitKind::CancelPoll), 0, "no CANCEL_POLL wait");
    assert_eq!(kind(WaitKind::Message), 0, "no message wait");
    assert_eq!(kind(WaitKind::HeldPace), 0);
    assert!(
        kind(WaitKind::Completion) >= bands,
        "a completion wait a band: {after:?} from {before:?}, {bands} bands"
    );
    rig.exit();
}

/// V3: a publication whose first band is in flight (each completion held
/// 300 ms) and then made stale by `stale`: answered Unavailable at the
/// next pass, no band written after it.
fn ended_after_the_first_band(stale: impl FnOnce(&Rig)) {
    let rig = Rig::new();
    rig.faults.step_delay_us.store(300_000, SeqCst);
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_d, skin) = charted(&target);
    let (_i, set) = chart_set(skin.side());
    let request = staged_attempt(&rig, skin);
    let rx = publish(&rig, request, &set);
    assert!(super::wait_until(
        || rig.faults.chart_bands.load(SeqCst) >= 1
    ));
    stale(&rig);
    let ack = rx.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(
        ack.answer,
        Err(render::ChartOutcome::Unavailable),
        "{ack:?}"
    );
    assert_eq!(
        rig.faults.chart_bands.load(SeqCst),
        1,
        "a band written after it went stale"
    );
    rig.exit();
}

/// V3: a new generation (a renewal) after the first band.
#[test]
fn a_renewal_after_the_first_band_answers_unavailable() {
    ended_after_the_first_band(|rig| {
        rig.link.chart_generation().fetch_add(1, SeqCst);
    });
}

/// V3: the attempt's staging discarded after the first band.
#[test]
fn a_discard_after_the_first_band_answers_unavailable() {
    ended_after_the_first_band(|rig| rig.link.discard_attempt());
}

/// V3: the device lost at the first band's boundary; the screen is built
/// again.
#[test]
fn a_loss_after_the_first_band_answers_unavailable() {
    let rig = Rig::new();
    rig.faults.step_delay_us.store(300_000, SeqCst);
    rig.faults.lose_after_band.store(true, SeqCst);
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_d, skin) = charted(&target);
    let (_i, set) = chart_set(skin.side());
    let request = staged_attempt(&rig, skin);
    let ack = publish(&rig, request, &set)
        .recv_timeout(Duration::from_secs(20))
        .unwrap();
    assert_eq!(
        ack.answer,
        Err(render::ChartOutcome::Unavailable),
        "{ack:?}"
    );
    assert!(!rig.faults.lose_after_band.load(SeqCst), "lost at a band");
    rig.exit();
}

/// H2 (design.md "Memory Budget", the uploader's passes between frames):
/// offscreen, a staging stepped by loading's presenting passes ends
/// mid-interval and its frame keeps the deadline those passes waited for:
/// the pacing after the last stepping pass is that deadline, kept or
/// reached, never a fresh pace.
// Revert: the deadline dropped once no upload steps (`Between::Frame`
// for every pass not stepping) -> "a fresh pace after the upload".
#[test]
fn an_upload_ended_mid_interval_keeps_its_frames_deadline() {
    use crate::render_side::Pace;
    let rig = Rig::new();
    rig.faults.step_delay_us.store(2_000, SeqCst);
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let target = crate::render_common::offscreen(1_920, 1_080);
    let staged = big(&target);
    // Loading frames presented by the clock: the hold never holds.
    rig.link.shared.hold_bound.store(0, SeqCst);
    rig.link.send(RenderMsg::Menu(Box::new(one_line("page"))));
    rig.link.loading().show(one_line("loading"));
    let paced = &rig.link.shared.clock_paced;
    let from = paced.load(SeqCst);
    assert!(wait_until(|| paced.load(SeqCst) >= from + 2));
    rig.link.record_pacing(4_096);
    let answer = rig
        .link
        .prepare_attempt_with(
            super::titled(&chart),
            super::super::AttemptSkin::Specialized(staged),
            || false,
        )
        .unwrap();
    assert_eq!(answer, Preparation::Ready);
    let from = paced.load(SeqCst);
    assert!(wait_until(|| paced.load(SeqCst) >= from + 2));
    let pacing = rig.link.pacing();
    let last = pacing
        .iter()
        .rposition(|p| matches!(p, Pace::Step(_)))
        .expect("stepped by the clock");
    let Some(Pace::Step(deadline)) = pacing.get(last).copied() else {
        unreachable!()
    };
    assert!(
        matches!(
            pacing.get(last + 1),
            Some(Pace::Kept(d) | Pace::StepDue(d)) if *d == deadline
        ),
        "a fresh pace after the upload: {:?}",
        &pacing[last..]
    );
    rig.exit();
}
