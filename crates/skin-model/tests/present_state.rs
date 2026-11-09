//! The presentation program's timer callbacks and slots through
//! `Skin::new` and the play frame build (design.md "beatoraja Lua Skins",
//! the program and its state): a timer callback runs again only when its
//! first run was on, each run's stores kept (`SkinObject.java:350-357`,
//! `TimerProperty.java:8-10`, `SkinLuaAccessor.java:736-747` at `0.8.9`);
//! its elapsed time is the source's two truncating divisions, capped at
//! 4 h; and the verifier refuses what V5, V7 and V8 check.

mod lanes_support;
mod play_support;
mod present_support;
mod widget_support;

use bms_model::TimeUs;
use present_support::{Builder, Cb, load, read};
use skin_model::present::{
    Arith, Cmp, Conv, Domain, Insn, MAX_CALLBACK_OPS, MAX_SLOTS, OFF, OutTy, PresentError, SlotDef,
    Val, elapsed_ms,
};
use skin_model::{
    Ease, ElementDef, End, KeyDef, LoadError, MAX_ELAPSED_MS, MAX_KEY_US, MAX_SKIN_OPS, Place,
    Skin, Source, TimerRef, Track, Ty, WidgetDefs, timer,
};
use widget_support::{drawn, element, harness};

/// The integers 0 to 2^53, a counter's domain here.
const COUNT: Domain = Domain {
    off: false,
    ints: Some((0, 1 << 53)),
};

/// A slot counting the invocations that touch it, from 0.
fn counter(b: &mut Builder) {
    b.0.slots.push(SlotDef {
        init: Val::Int(0),
        domain: COUNT,
    });
    b.0.domains.push(COUNT);
}

/// A timer callback that counts its invocation in slot 0, then gives
/// `on` (a start in µs) when the count it saw compares with `n` by `cmp`,
/// else `other`.
fn counted(on: i64, cmp: Cmp, n: i64, other: Val) -> Cb {
    Cb::code(
        vec![
            Insn::LoadSlot { dst: 0, slot: 0 },
            Insn::Const { dst: 1, k: 0 },
            Insn::Arith {
                op: Arith::Add,
                dst: 1,
                a: 0,
                b: 1,
            },
            Insn::Const { dst: 2, k: 1 },
            Insn::Cmp {
                op: cmp,
                dst: 2,
                a: 0,
                b: 2,
            },
            Insn::Const { dst: 3, k: 2 },
            Insn::Test {
                cond: 2,
                when: true,
                to: 9,
            },
            Insn::Const { dst: 3, k: 3 },
            Insn::Jump { to: 9 },
            Insn::StoreSlot {
                slot: 0,
                src: 1,
                domain: 0,
            },
            Insn::Ret { src: 3 },
        ],
        vec![Val::Int(1), Val::Int(n), Val::Int(on), other],
        vec![],
        Conv::Timer,
    )
}

/// An image element on program timer `k` over `track`.
fn on_output(k: u16, track: Track) -> ElementDef {
    let mut e = element(Source::Image(0), [0, 0, 10, 10], Place::SCREEN);
    e.timer = TimerRef::Output(k);
    e.track = track;
    e
}

/// A track of one key holding from 0.
fn held() -> Track {
    key_track(&[0], End::Hold)
}

/// A track with a key at each of `at` (µs), ending `end`.
fn key_track(at: &[i64], end: End) -> Track {
    let mut props = [None; 9];
    props[2] = Some(10);
    props[3] = Some(10);
    let keys: Vec<KeyDef> = at
        .iter()
        .map(|&t| KeyDef {
            at: TimeUs(t),
            props,
        })
        .collect();
    Track::new(&keys, Ease::Linear, end).unwrap()
}

/// Program `b`, one element per timer callback `ks`.
fn timed(b: Builder, ks: &[u16], track: impl Fn() -> Track) -> Result<Skin, LoadError> {
    let elements = ks.iter().map(|&k| on_output(k, track())).collect();
    load(Some(b.0), Vec::new(), elements, WidgetDefs::default())
}

/// The slot of the `i`th program timer in a play without `when` timers.
fn program_timer(i: u16) -> u16 {
    timer::BUILTIN + i
}

fn count(h: &play_support::Harness) -> Val {
    h.play.bank().values()[0]
}

/// V5's cost for a slot-owning timer callback, written out by hand
/// (design.md "beatoraja Lua Skins", V5): per invocation its 11
/// instructions and the one slot it loads and stores (counted once), and
/// a timer callback invoked twice; it is the skin's only work.
#[test]
fn a_timer_callback_is_charged_for_both_runs_and_its_slot() {
    let mut b = Builder::default();
    counter(&mut b);
    let k = b.push(counted(0, Cmp::Lt, 0, OFF));
    let want = 2 * (11 + 1);
    let d = &b.0;
    let cb = d.callbacks[usize::from(k)];
    let span = &d.code[cb.start as usize..][..usize::from(cb.len)];
    assert_eq!(span.len(), 11);
    assert_eq!(
        skin_model::present::callback_cost(span, &d.reads, cb.conv, None),
        want
    );
    let skin = timed(b, &[k], held).unwrap();
    assert_eq!(skin.ops().total(), want);
}

/// A timer callback off at its first run runs once and is off; on at its
/// first and off at its second, it runs twice, both runs' stores kept,
/// and its start is the off value's, whose elapsed time is capped; two
/// objects sharing one closure run four times a frame while both are on.
#[test]
fn a_timer_runs_again_only_when_on() {
    // Off first: one run a frame, nothing drawn.
    let mut b = Builder::default();
    counter(&mut b);
    let k = b.push(counted(0, Cmp::Lt, 0, OFF));
    let mut h = harness(timed(b, &[k], held).unwrap());
    h.frame(1_000, &[]);
    assert_eq!(count(&h), Val::Int(1));
    assert_eq!(h.play.timer(program_timer(0)), None);
    assert!(drawn(&h).is_empty());
    h.frame(1_016, &[]);
    assert_eq!(count(&h), Val::Int(2));

    // On at the first run (it saw 0), off at the second (it saw 1).
    let mut b = Builder::default();
    counter(&mut b);
    let k = b.push(counted(0, Cmp::Eq, 0, OFF));
    let mut h = harness(timed(b, &[k], held).unwrap());
    h.state_us = 1_234_567;
    h.frame(1_000, &[]);
    assert_eq!(count(&h), Val::Int(2));
    let capped = 1_000_000 - i64::from(MAX_ELAPSED_MS) * 1_000;
    assert_eq!(h.play.timer(program_timer(0)), Some(TimeUs(capped)));
    assert_eq!(drawn(&h).len(), 1);
    // Then off at the first run: one more.
    h.frame(1_016, &[]);
    assert_eq!(count(&h), Val::Int(3));
    assert_eq!(h.play.timer(program_timer(0)), None);

    // Two objects of one closure, both on: four runs a frame.
    let mut b = Builder::default();
    counter(&mut b);
    let k = b.push(counted(0, Cmp::Le, 0, Val::Int(0)));
    let j = b.share(k);
    let mut h = harness(timed(b, &[k, j], held).unwrap());
    h.state_us = 5_000;
    h.frame(1_000, &[]);
    assert_eq!(count(&h), Val::Int(4));
    // Started at 0 µs on the state clock, 5 ms ago.
    for i in [0, 1] {
        assert_eq!(
            h.play.timer(program_timer(i)),
            Some(TimeUs(1_000_000 - 5_000))
        );
    }
    assert_eq!(drawn(&h).len(), 2);
}

/// The elapsed time is two truncating divisions: a start 1 µs before the
/// clock in another millisecond is 1 ms ago; the off value as a start is
/// `state_us / 1000 + 9223372036854775`.
#[test]
fn the_elapsed_time_is_the_sources() {
    assert_eq!(elapsed_ms(2_000, 1_999), 1);
    assert_eq!(elapsed_ms(1_999, 1_000), 0);
    assert_eq!(elapsed_ms(1_234_567, i64::MIN), 9_223_372_036_856_009);
    assert_eq!(elapsed_ms(1_000, -999), 1);
}

/// A start after the clock is not drawn (the source's `starttime > time`);
/// one at the clock is drawn at elapsed 0.
#[test]
fn a_start_after_the_clock_is_not_drawn() {
    for (start, shown) in [(3_000, false), (2_000, true), (1_999, true)] {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(start), Conv::Timer));
        let mut h = harness(timed(b, &[k], held).unwrap());
        h.state_us = 2_000;
        h.frame(1_000, &[]);
        assert_eq!(!drawn(&h).is_empty(), shown, "{start}");
    }
}

/// The elapsed time is capped at 4 h: a hidden end keyed at 4 h, which the
/// capped time would still show, is refused; one a millisecond before it
/// is accepted and hidden once the time passes it; a loop is refused.
#[test]
fn a_program_timers_track_ignores_the_cap() {
    let max = MAX_KEY_US;
    let one = |end: End, last: i64| {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(0), Conv::Timer));
        timed(b, &[k], || key_track(&[0, last], end))
    };
    assert!(matches!(
        one(End::Hide, max),
        Err(LoadError::ProgramTimerTrack { element: 0 })
    ));
    assert!(matches!(
        one(End::Loop { from: TimeUs(0) }, max),
        Err(LoadError::ProgramTimerTrack { element: 0 })
    ));
    assert!(one(End::Hold, max).is_ok());
    let mut h = harness(one(End::Hide, max - 1_000).unwrap());
    // Past 4 h on the state clock: hidden, as the source hides it.
    h.state_us = max + 5_000;
    h.frame(1_000, &[]);
    assert!(drawn(&h).is_empty());
    h.state_us = max - 1_000;
    h.frame(1_016, &[]);
    assert_eq!(drawn(&h).len(), 1);
}

/// An image choice's entry replaces the element's track on the element's
/// timer, so on a program timer each entry's track is held to the same
/// rule as the element's: a loop, or a hidden end at 4 h, is refused; a
/// held one is accepted.
#[test]
fn a_choice_entrys_track_on_a_program_timer_ignores_the_cap() {
    use skin_model::{ImageChoiceDef, ImageChoicePart, ImageSpan, PastTable};
    let max = MAX_KEY_US;
    let with_entry = |track: Track| {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(0), Conv::Timer));
        let mut e = on_output(k, held());
        e.source = Source::ImageChoice(0);
        let entry = ImageChoicePart {
            span: ImageSpan {
                image: 0,
                first: 0,
                frames: 1,
            },
            cycle: None,
            track: Some(track),
            native: None,
        };
        let widgets = WidgetDefs {
            image_choices: vec![ImageChoiceDef {
                value: widget_support::int("0"),
                past_table: PastTable::Hide,
                choices: vec![Some(entry)],
            }],
            ..WidgetDefs::default()
        };
        load(Some(b.0), Vec::new(), vec![e], widgets)
    };
    for end in [End::Loop { from: TimeUs(0) }, End::Hide] {
        assert!(
            matches!(
                with_entry(key_track(&[0, max], end)),
                Err(LoadError::ProgramTimerTrack { element: 0 })
            ),
            "{end:?}"
        );
    }
    assert!(with_entry(key_track(&[0, max], End::Hold)).is_ok());
    assert!(with_entry(key_track(&[0, max - 1_000], End::Hide)).is_ok());
}

/// A cover paint's gate track is sampled on the element's timer, so on a
/// program timer it is held to the same rule: a loop, or a hidden end at
/// 4 h, is refused; a held one is accepted.
#[test]
fn a_cover_gate_on_a_program_timer_ignores_the_cap() {
    use skin_model::{CoverImage, CoverMotion, CoverPaintDef, CoverTrim};
    let max = MAX_KEY_US;
    let with_gate = |gate: Track| {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(0), Conv::Timer));
        let mut e = on_output(k, held());
        e.source = Source::CoverPaint(0);
        let widgets = WidgetDefs {
            cover_paints: vec![CoverPaintDef {
                image: CoverImage::Image(0),
                slider: None,
                motion: CoverMotion::None,
                trim: CoverTrim::Fixed(960),
                gate_track: Some(gate),
            }],
            ..WidgetDefs::default()
        };
        load(Some(b.0), Vec::new(), vec![e], widgets)
    };
    for end in [End::Loop { from: TimeUs(0) }, End::Hide] {
        let r = with_gate(key_track(&[0, max], end));
        assert!(
            matches!(r, Err(LoadError::ProgramTimerTrack { element: 0 })),
            "{end:?}: {:?}",
            r.map(|_| ())
        );
    }
    let r = with_gate(key_track(&[0, max], End::Hold));
    assert!(r.is_ok(), "{:?}", r.map(|_| ()));
}

/// A timer gated by a program draw runs only while the draw is true.
#[test]
fn a_timer_after_a_false_draw_does_not_run() {
    for (combo, on) in [(0, false), (5, true)] {
        let mut b = Builder::default();
        counter(&mut b);
        let d = b.push(Cb::code(
            vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
            vec![],
            vec![read("combo > 3", Ty::Bool, None)],
            Conv::Truthy,
        ));
        let k = b.push(counted(0, Cmp::Le, 0, Val::Int(0)).after(d));
        let mut h = harness(timed(b, &[k], held).unwrap());
        h.tally.combo = combo;
        h.frame(1_000, &[]);
        assert_eq!(count(&h) == Val::Int(2), on, "{combo}");
        assert_eq!(h.play.timer(program_timer(0)).is_some(), on, "{combo}");
    }
}

fn refused(r: Result<Skin, LoadError>) -> PresentError {
    match r {
        Err(LoadError::Present(e)) => e,
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// V5 charges each slot a callback touches, and a timer callback for both
/// its runs; V7 refuses too many slots, an initial value or a store
/// outside its slot's domain and an unknown slot; V8 refuses an element
/// timer naming no timer callback and an expression reading a timer's
/// output.
#[test]
fn the_verifier_refuses_slots_and_timers() {
    let plain = |b: Builder| load(Some(b.0), Vec::new(), Vec::new(), WidgetDefs::default());
    // V5: `n` callbacks of 256 instructions fit; one slot more each, or a
    // second run each, does not.
    let n = MAX_SKIN_OPS / MAX_CALLBACK_OPS;
    let long = |touch: bool, conv: Conv| {
        let mut code = vec![Insn::Const { dst: 0, k: 0 }; MAX_CALLBACK_OPS - 1];
        if touch {
            code[0] = Insn::LoadSlot { dst: 0, slot: 0 };
        }
        code.push(Insn::Ret { src: 0 });
        let mut b = Builder::default();
        counter(&mut b);
        let k = b.push(Cb::code(code, vec![Val::Int(0)], vec![], conv));
        for _ in 1..n {
            b.share(k);
        }
        b
    };
    assert!(plain(long(false, Conv::Int)).is_ok());
    assert_eq!(refused(plain(long(true, Conv::Int))), PresentError::Cost);
    assert_eq!(refused(plain(long(false, Conv::Timer))), PresentError::Cost);

    // V7.
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Int(0), Conv::Timer));
    for _ in 0..=MAX_SLOTS {
        counter(&mut b);
    }
    assert_eq!(
        refused(plain(b)),
        PresentError::TooManySlots { n: MAX_SLOTS + 1 }
    );
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Int(0), Conv::Timer));
    b.0.slots.push(SlotDef {
        init: OFF,
        domain: COUNT,
    });
    assert_eq!(refused(plain(b)), PresentError::Slot { slot: 0 });
    let mut b = Builder::default();
    counter(&mut b);
    b.0.domains[0] = Domain {
        off: true,
        ints: None,
    };
    b.push(counted(0, Cmp::Lt, 0, OFF));
    assert_eq!(
        refused(plain(b)),
        PresentError::Store { callback: 0, pc: 9 }
    );
    let mut b = Builder::default();
    b.push(counted(0, Cmp::Lt, 0, OFF));
    assert_eq!(
        refused(plain(b)),
        PresentError::Operand { callback: 0, pc: 0 }
    );

    // V8: an element's program timer that is no timer callback.
    for conv in [Conv::Int, Conv::Truthy] {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(0), conv));
        assert!(matches!(
            timed(b, &[k], held),
            Err(LoadError::UnknownTimer { element: 0 })
        ));
    }
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Int(0), Conv::Timer));
    assert!(matches!(
        timed(b, &[1], held),
        Err(LoadError::UnknownTimer { element: 0 })
    ));
    // ... and a timer output read by an expression: it has no name there.
    let sig = [OutTy::Timer];
    for name in ["lua.0", "lua.0.ok"] {
        let scope = skin_model::Scope::new(&skin_model::PLAY).outputs(&sig);
        assert!(
            skin_model::compile(name, &scope, Ty::Bool).is_err(),
            "{name}"
        );
    }
}

/// A decision skin has no program, so no element timer of one.
#[test]
fn a_decision_skin_has_no_program_timer() {
    let def = skin_model::SkinDef {
        elements: vec![on_output(0, held())],
        ..play_support::def(Vec::new(), widget_support::sheet(), Vec::new())
    };
    assert!(matches!(
        Skin::new_decision(def, skin_model::DecisionDefs::default()),
        Err(LoadError::UnknownTimer { element: 0 })
    ));
}

/// The bank starts at the slots' initial values at each prepare, and a
/// rearm keeps it.
#[test]
fn slots_are_fresh_at_prepare_and_kept_by_a_rearm() {
    let mut b = Builder::default();
    counter(&mut b);
    let k = b.push(counted(0, Cmp::Lt, 0, OFF));
    let skin = timed(b, &[k], held).unwrap();
    let mut h = harness(skin);
    h.frame(1_000, &[]);
    h.frame(1_016, &[]);
    assert_eq!(count(&h), Val::Int(2));
    h.play.rearm();
    assert_eq!(count(&h), Val::Int(2));
    let fresh = h
        .skin
        .prepare(&widget_support::chart(), &play_support::full(0))
        .unwrap();
    assert_eq!(fresh.bank().values(), [Val::Int(0)]);
}
