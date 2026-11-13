//! The system voices' control step and mix neither allocate nor free
//! (design.md "Scope of No Allocation"): starts, refused starts, decision
//! sounds resolved both ways, stops, a waiting loop, installs merged with
//! starts through a cutoff, requests above it left queued, the empty
//! install, a blocked install, tone requests, retirements, returns, an
//! output failure and control-only steps, over 3 000 steps. Generations
//! are built before the counted window and dropped after it.

mod counting;
mod system_rig;

use std::sync::Arc;

use audio::{GenId, SystemGeneration, SystemKind, SystemVoices};
use system_rig::{RATE, Rig, constant};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const STEPS: u64 = 3_000;
/// A pick's install every this many steps.
const EVERY: u64 = 10;

#[test]
fn system_steps_neither_allocate_nor_free() {
    let scratch = constant(2_000, 2_400);
    let select = constant(1_000, 4_800);
    let decide = constant(3_000, 960);
    // Even ids have a decision sound, odd ones fall back to the tone.
    let picks: Vec<SystemGeneration> = (0..STEPS / EVERY + 2)
        .map(|k| {
            let g = SystemGeneration::new(GenId(1_000 * (k + 1)), RATE)
                .with(SystemKind::Scratch, Arc::clone(&scratch))
                .with(SystemKind::Select, Arc::clone(&select));
            if k % 2 == 0 {
                g.with(SystemKind::Decide, Arc::clone(&decide))
            } else {
                g
            }
        })
        .collect();
    let mut picks = picks.into_iter();
    let mut empty = Some(SystemGeneration::empty(GenId(u64::MAX - 1)));
    let mut returned: Vec<SystemGeneration> = Vec::with_capacity(picks.len() + 4);
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    let mut buf = vec![0.0_f32; 2 * 480];
    let mut seq = 0_u64;
    let mut current = GenId(0);
    let mut blocked = 0;
    counting::track(true);
    counting::self_check();
    let (allocations, frees) = (counting::allocations(), counting::frees());
    for step in 0..STEPS {
        let mut next = || {
            seq += 1;
            seq
        };
        if step % EVERY == 0 && step < STEPS - 400 {
            // A loop for the pick, waiting until it lands, a one-shot and
            // a decision sound before it, and one after it.
            let g = picks.next().unwrap();
            let id = g.id();
            let n = next();
            rig.looped(n, SystemKind::Select, id);
            let n = next();
            rig.start(n, SystemKind::Scratch);
            let n = next();
            rig.start(n, SystemKind::Decide);
            let n = next();
            rig.install(n, g);
            let n = next();
            rig.start(n, SystemKind::Decide);
            current = id;
        }
        if step % EVERY == 3 {
            // The loop kind stopped: the retired generation goes back.
            let n = next();
            rig.stop(n, SystemKind::Select);
            rig.stop(n, SystemKind::Scratch);
        }
        if step % EVERY == 5 {
            // A burst past the pool, and requests above the cutoff.
            for _ in 0..40 {
                let n = next();
                rig.start(n, SystemKind::Scratch);
            }
            rig.tone = true;
        }
        if step % EVERY == 6 {
            let n = next();
            rig.looped(n, SystemKind::Select, current);
        }
        if step % 500 == 250 && step < STEPS - 400 {
            // A second pick while the first retired one still plays: the
            // install waits behind the taken retired slot until a stop
            // frees it.
            let g = picks.next().unwrap();
            let n = next();
            rig.install(n, g);
            blocked += 1;
        }
        if step == STEPS - 300 {
            let n = next();
            rig.install(n, empty.take().unwrap());
            let n = next();
            rig.start(n, SystemKind::Decide);
        }
        if step == STEPS - 100 {
            voices.fail();
        }
        // The cutoff trails the requests by one on odd steps, so the last
        // waits in its ring for the next step.
        let cutoff = if step % 2 == 1 { seq - 1 } else { seq };
        rig.apply(&mut voices, cutoff);
        if step < STEPS - 100 {
            voices.mix_into(&mut buf, 0.5);
        }
        rig.sweep(&mut voices);
        while let Ok(g) = rig.returns_rx.pop() {
            returned.push(g);
        }
    }
    let counted = (
        counting::allocations() - allocations,
        counting::frees() - frees,
    );
    counting::track(false);
    assert_eq!(counted, (0, 0), "allocations and frees in the window");
    // What the window was meant to exercise did happen.
    assert!(voices.refused() > 0);
    assert!(voices.install_waits() > 0 && blocked > 0);
    assert!(returned.len() > 200, "{} returned", returned.len());
    assert_eq!(
        voices.current(),
        Some(GenId(u64::MAX - 1)),
        "the empty install"
    );
    drop(returned);
}
