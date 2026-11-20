//! The uploader's steps on a real screen, on WARP through the render
//! thread (design.md "Memory Budget", the uploader; V10f): a play
//! staging whose own array takes several atlas steps, each step's
//! completion held 2 ms, is stepped by the passes that present nothing
//! with no message wait, no `CANCEL_POLL` and no `HELD_PACE` sleep while
//! a step is ready or in flight, and its answer leaves in the pass whose
//! boundary saw its last step complete.

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

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
