//! The system voices' control step and mix neither allocate nor free
//! (design.md "Scope of No Allocation"): starts, refused starts, decision
//! sounds resolved both ways, stops, a waiting loop, installs merged with
//! starts through a cutoff, requests above it left queued, the empty
//! install, a blocked install, tone requests, retirements, returns, an
//! output failure and control-only steps, over 3 000 steps; and a full
//! return ring holding installs and a sweep until the returns are taken.
//! Generations are built before the counted windows and dropped after
//! them.

mod counting;
mod system_rig;

use std::sync::Arc;

use audio::{GenId, SystemGeneration, SystemKind, SystemVoices};
use system_rig::{RATE, Rig, constant};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const STEPS: u64 = 3_000;
/// The tests count on global counters: one window at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// A generation `id` whose `select` is `sound`.
fn select(id: u64, sound: &Arc<audio::Sound>) -> SystemGeneration {
    SystemGeneration::new(GenId(id), RATE).with(SystemKind::Select, Arc::clone(sound))
}

/// A full return ring holds what would go back where it is: an install
/// that would send a generation back waits, counted, with the requests
/// above it, the tone held behind it; a pick's install that only retires
/// one still applies; a voiceless retired generation stays retired through
/// a sweep. Once the requesting side takes the returns, each goes back and
/// every generation comes back, and no step allocates or frees.
#[test]
fn a_full_return_ring_holds_every_generation() {
    let sound = constant(1_000, 48_000);
    let gens: Vec<SystemGeneration> = (1..=6).map(|id| select(id, &sound)).collect();
    let mut gens = gens.into_iter();
    let empty = SystemGeneration::empty(GenId(10));
    // Taken back in the window, dropped after it.
    let mut back: Vec<SystemGeneration> = Vec::with_capacity(8);
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    let mut buf = vec![0.0_f32; 2 * 480];
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    counting::track(true);
    counting::self_check();
    let (allocations, frees) = (counting::allocations(), counting::frees());
    // 1 to 4 installed in turn, voiceless: 1, 2 and 3 fill the ring.
    for seq in 1..=4 {
        rig.install(seq, gens.next().unwrap());
        rig.apply(&mut voices, seq);
    }
    assert_eq!(voices.current(), Some(GenId(4)));
    assert_eq!(rig.returns_tx.slots(), 0, "the ring is full");
    // 5's install would send 4 back: it waits.
    rig.install(5, gens.next().unwrap());
    let report = rig.apply(&mut voices, 5);
    assert!(report.blocked);
    assert_eq!((report.installed, voices.install_waits()), (None, 1));
    assert_eq!(voices.current(), Some(GenId(4)));
    assert_eq!(rig.installs_rx.slots(), 1, "5 still queued");
    // One return taken: 5 applies and 4 goes back.
    back.push(rig.returns_rx.pop().unwrap());
    let report = rig.apply(&mut voices, 5);
    assert_eq!(report.installed, Some(GenId(5)));
    assert_eq!(rig.returns_tx.slots(), 0, "full again");
    // A loop in 5, then 6: 5 retires, which takes no return slot.
    rig.looped(6, SystemKind::Select, GenId(5));
    rig.install(7, gens.next().unwrap());
    rig.apply(&mut voices, 7);
    voices.mix_into(&mut buf, 0.5);
    assert_eq!(
        (voices.current(), voices.retired()),
        (Some(GenId(6)), Some(GenId(5)))
    );
    // The loop stopped: 5 is voiceless but the sweep finds no room.
    rig.stop(8, SystemKind::Select);
    rig.apply(&mut voices, 8);
    rig.sweep(&mut voices);
    assert_eq!(voices.retired(), Some(GenId(5)), "kept, not lost");
    // The empty install, which sends 6 and 5 back, and the tone: held.
    rig.install(9, empty);
    for kind in SystemKind::ALL {
        rig.stop(9, kind);
    }
    rig.tone = true;
    let report = rig.apply(&mut voices, 9);
    assert!(report.blocked);
    assert!(voices.tone_held() && !voices.tone_sounding());
    assert_eq!(voices.install_waits(), 2);
    // The returns taken: 5 goes back at the sweep, then the empty install
    // applies, sends 6 back and starts the tone.
    while let Ok(g) = rig.returns_rx.pop() {
        back.push(g);
    }
    rig.sweep(&mut voices);
    let report = rig.apply(&mut voices, 9);
    assert!(!report.blocked);
    assert_eq!(report.installed, Some(GenId(10)));
    assert!(voices.tone_sounding());
    voices.mix_into(&mut buf, 0.5);
    rig.sweep(&mut voices);
    let counted = (
        counting::allocations() - allocations,
        counting::frees() - frees,
    );
    counting::track(false);
    assert_eq!(counted, (0, 0), "allocations and frees in the window");
    let mut ids: Vec<GenId> = back.iter().map(SystemGeneration::id).collect();
    ids.extend(rig.returned());
    ids.sort();
    assert_eq!(ids, (1..=6).map(GenId).collect::<Vec<_>>());
    assert_eq!(voices.current(), Some(GenId(10)));
}
