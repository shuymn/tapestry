//! The boundary's admission gate (design.md "Memory Budget", the
//! uploader's admission gate) over every order of its events: the swap
//! chain's grant, the frame's clock deadline, READY's cutoff, the
//! interval's allowance spent and the hold's bound, against a boundary's
//! asks, its heavy operation first and then its writes. An event stops
//! every ask from the one it precedes, the grant found kept so once taken;
//! the allowance stops only the heavy operation; the control, no event,
//! admits every ask.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use render::UploadGate;

use super::super::{Ask, FrameDue, GATE_NOW, GateState, PassGate, admits};

/// The boundary's asks, in order.
const ASKS: [Ask; 4] = [Ask::Heavy, Ask::Write, Ask::Write, Ask::Write];
/// An event's position: before the ask of that index, or never.
const NEVER: usize = ASKS.len();

/// The swap chain's grant (one test thread uses it).
static GRANT: AtomicBool = AtomicBool::new(false);

/// Where each event arrives: before which ask, or [`NEVER`]. READY's
/// cutoff is found only between passes (the render thread's phase is
/// running through a boundary, so the game cuts off only while it waits):
/// at the first ask or never.
#[derive(Clone, Copy, Debug)]
struct Order {
    grant: usize,
    deadline: usize,
    cutoff: usize,
    spent: usize,
    hold: usize,
}

/// What the gate answers for each ask of `order`: admitted or not.
fn run(order: Order) -> [bool; ASKS.len()] {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(1);
    GRANT.store(false, Ordering::SeqCst);
    GATE_NOW.with(|c| c.set(Some(start)));
    let hold = Cell::new(false);
    let cut_off = order.cutoff == 0;
    let mut granted = false;
    let mut gate = PassGate {
        frame: FrameDue::Grant(render::GrantProbe::Flag(&GRANT)),
        cut_off,
        hold: Some(|| hold.get()),
        chart: None,
        loading: None,
        granted: &mut granted,
    };
    // The offscreen frame's deadline, through a gate of its own: a pass
    // is paced by the grant or by the clock, never both.
    let mut granted_clock = false;
    let mut clock = PassGate::<fn() -> bool> {
        frame: FrameDue::At(deadline),
        cut_off,
        hold: None,
        chart: None,
        loading: None,
        granted: &mut granted_clock,
    };
    let mut spent = false;
    let mut grant_seen = false;
    let mut out = [false; ASKS.len()];
    for (i, ask) in ASKS.iter().enumerate() {
        if order.grant == i {
            GRANT.store(true, Ordering::SeqCst);
        }
        if order.deadline == i {
            GATE_NOW.with(|c| c.set(Some(deadline)));
        }
        if order.spent == i {
            spent = true;
        }
        if order.hold == i {
            hold.set(true);
        }
        out[i] = match ask {
            Ask::Heavy => {
                // The heavy operation's look at the grant takes it, as the
                // boundary's does.
                grant_seen |= GRANT.swap(false, Ordering::SeqCst);
                admits(
                    GateState {
                        frame_due: grant_seen
                            || GATE_NOW.with(Cell::get).is_some_and(|now| now >= deadline),
                        cut_off,
                        hold_past: hold.get(),
                        heavy_spent: spent,
                    },
                    Ask::Heavy,
                )
            }
            Ask::Write => {
                if grant_seen {
                    // A grant the heavy operation took is kept for the
                    // frame: the gate is built with it.
                    gate.frame = FrameDue::Now;
                }
                let swap = !gate.past();
                let paced = !clock.past();
                swap && paced
            }
        };
    }
    GATE_NOW.with(|c| c.set(None));
    out
}

/// The expected answers: an ask is admitted while no event that stops it
/// has arrived at or before it.
fn expected(order: Order) -> [bool; ASKS.len()] {
    let mut out = [false; ASKS.len()];
    for (i, ask) in ASKS.iter().enumerate() {
        let frame = order.grant <= i || order.deadline <= i;
        let cut = order.cutoff <= i;
        let hold = order.hold <= i;
        let spent = matches!(ask, Ask::Heavy) && order.spent <= i;
        out[i] = !(frame || cut || hold || spent);
    }
    out
}

/// Every order of the five events over the boundary's four asks (each
/// event before one of them or never; the cutoff from the first ask or
/// never): the gate answers as expected, and the control admits all.
// Revert: a grant found not kept (`PassGate::past` leaving `frame` at
// `Grant` after taking it) -> a write admitted after the grant; or the
// frame's due dropped from `admits` -> the heavy operation admitted
// with a grant found.
#[test]
fn the_gate_admits_by_its_order_in_every_order_of_events() {
    let control = Order {
        grant: NEVER,
        deadline: NEVER,
        cutoff: NEVER,
        spent: NEVER,
        hold: NEVER,
    };
    assert_eq!(run(control), [true; ASKS.len()], "the control");
    let mut orders = 0;
    for grant in 0..=NEVER {
        for deadline in 0..=NEVER {
            for cutoff in [0, NEVER] {
                for spent in 0..=NEVER {
                    for hold in 0..=NEVER {
                        let order = Order {
                            grant,
                            deadline,
                            cutoff,
                            spent,
                            hold,
                        };
                        assert_eq!(run(order), expected(order), "{order:?}");
                        orders += 1;
                    }
                }
            }
        }
    }
    assert_eq!(orders, 5 * 5 * 2 * 5 * 5);
}

/// The gate's single rule, case by case: each source alone stops a write
/// and a heavy operation, the allowance only the heavy operation.
#[test]
fn each_source_alone_stops_what_it_governs() {
    let none = GateState::default();
    assert!(admits(none, Ask::Heavy) && admits(none, Ask::Write));
    for s in [
        GateState {
            frame_due: true,
            ..none
        },
        GateState {
            cut_off: true,
            ..none
        },
        GateState {
            hold_past: true,
            ..none
        },
    ] {
        assert!(!admits(s, Ask::Heavy), "{s:?}");
        assert!(!admits(s, Ask::Write), "{s:?}");
    }
    let spent = GateState {
        heavy_spent: true,
        ..none
    };
    assert!(!admits(spent, Ask::Heavy));
    assert!(admits(spent, Ask::Write));
}
