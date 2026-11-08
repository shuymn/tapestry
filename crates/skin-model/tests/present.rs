//! The presentation program through `Skin::new` and the play frame build
//! (design.md "beatoraja Lua Skins", the program): the verifier refuses
//! each rule it breaks (V1 to V6), the callbacks run in the source's
//! schedule (`SkinObject.java:587-596`, `SkinNumber.java:134-150` at
//! `0.8.9`), and each consumer receives the source's conversion
//! (`SkinLuaAccessor.java:612-651`).

mod lanes_support;
mod play_support;
mod present_support;
mod widget_support;

use present_support::{Builder, Cb, expr, load, read, slot};
use skin_model::present::{
    Arith, Conv, Insn, MAX_CALLBACK_OPS, MAX_CALLBACKS, MAX_REGISTERS, MathFn, OutTy, PresentDef,
    PresentError, Val,
};
use skin_model::{
    Align, ElementDef, GlyphSet, LoadError, MAX_SKIN_OPS, NumberDef, Pad, Place, Skin, Source,
    TimerRef, Ty, WidgetDefs, timer,
};
use widget_support::{TEN, digits, drawn, element, harness};

/// An image element drawn while `visible`, on `ready` unless `timer`.
fn image(visible: &str, sig: &[OutTy], timer: Option<u16>) -> ElementDef {
    let mut e = element(Source::Image(0), [0, 0, 10, 10], Place::SCREEN);
    e.visible = Some(expr(visible, sig, Ty::Bool));
    if let Some(t) = timer {
        e.timer = TimerRef::Builtin(t);
    }
    e
}

/// A number widget showing `value`, and its element, drawn while
/// `visible`.
fn number(value: &str, visible: Option<&str>, sig: &[OutTy]) -> (ElementDef, NumberDef) {
    let mut e = element(Source::Number(0), [100, 100, 20, 30], Place::SCREEN);
    e.visible = visible.map(|v| expr(v, sig, Ty::Bool));
    let n = NumberDef {
        digits: digits(TEN, GlyphSet::Ten, 4, Align::Right, Pad::Blank),
        value: expr(value, sig, Ty::Int),
    };
    (e, n)
}

/// A skin of `b`'s program with `elements` and the number `n`.
fn skin(b: Builder, elements: Vec<ElementDef>, n: Option<NumberDef>) -> Result<Skin, LoadError> {
    let widgets = WidgetDefs {
        numbers: n.into_iter().collect(),
        ..WidgetDefs::default()
    };
    load(Some(b.0), Vec::new(), elements, widgets)
}

/// Program `b` whose every output is read by one image, loaded.
fn plain(b: Builder) -> Result<Skin, LoadError> {
    let sig = b.signature();
    let elements = (0..sig.len())
        .map(|k| image(&format!("lua.{k}.ok"), &sig, None))
        .collect();
    skin(b, elements, None)
}

/// The outputs, `[value, ok]` per callback, after one frame of `combo`.
fn outputs(skin: Skin, n: usize, combo: u32) -> Vec<[i32; 2]> {
    let mut h = harness(skin);
    h.tally.combo = combo;
    h.frame(1_000, &[]);
    let f = h.play.state().fields;
    (0..n).map(|k| [f[slot(k)], f[slot(k) + 1]]).collect()
}

fn refused(r: Result<Skin, LoadError>) -> PresentError {
    match r {
        Err(LoadError::Present(e)) => e,
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// A minimal program a callback long, accepted.
fn one() -> Builder {
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy));
    b
}

#[test]
fn a_minimal_program_is_accepted() {
    assert_eq!(outputs(plain(one()).unwrap(), 1, 0), [[1, 1]]);
}

// Covers: T08
#[test]
fn the_verifier_refuses_each_rule() {
    // V1: too many callbacks, a signature that does not match, too many
    // constants, too much code.
    let mut b = one();
    for _ in 0..MAX_CALLBACKS {
        b.share(0);
    }
    assert!(matches!(
        refused(load(Some(b.0), vec![], vec![], WidgetDefs::default())),
        PresentError::TooManyCallbacks { .. }
    ));
    let mut b = one();
    b.0.signature[0] = OutTy::Int;
    assert_eq!(refused(plain_raw(b.0)), PresentError::Signature);
    let mut b = one();
    b.0.consts.resize(usize::from(u16::MAX) + 2, Val::Nil);
    assert_eq!(refused(plain_raw(b.0)), PresentError::TooManyEntries);
    let mut b = one();
    b.0.code.resize(MAX_SKIN_OPS + 1, Insn::Ret { src: 0 });
    assert!(matches!(
        refused(plain_raw(b.0)),
        PresentError::TooLong { .. }
    ));
    // V2: an empty span, one past the code, one too long, too many
    // registers.
    for (start, len) in [(0, 0), (1, 2), (0, MAX_CALLBACK_OPS as u16 + 1)] {
        let mut b = one();
        b.0.code.resize(MAX_CALLBACK_OPS + 1, Insn::Ret { src: 0 });
        b.0.callbacks[0].start = start;
        b.0.callbacks[0].len = len;
        if start == 1 {
            b.0.code.truncate(2);
        }
        assert_eq!(refused(plain_raw(b.0)), PresentError::Span { callback: 0 });
    }
    let mut b = one();
    b.0.callbacks[0].regs = MAX_REGISTERS as u8 + 1;
    assert_eq!(
        refused(plain_raw(b.0)),
        PresentError::Registers { callback: 0 }
    );
    // V3: a register past the count, a constant and a read that do not
    // exist, a backward jump, one past the end, a jump to itself.
    for code in [
        vec![Insn::Ret { src: 8 }],
        vec![Insn::Const { dst: 0, k: 1 }, Insn::Ret { src: 0 }],
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
    ] {
        let mut b = Builder::default();
        b.push(Cb::code(code, vec![Val::Nil], vec![], Conv::Truthy));
        assert!(matches!(
            refused(plain_raw(b.0)),
            PresentError::Operand { .. }
        ));
    }
    for code in [
        vec![Insn::Const { dst: 0, k: 0 }, Insn::Jump { to: 0 }],
        vec![Insn::Jump { to: 3 }, Insn::Ret { src: 0 }],
        vec![Insn::Jump { to: 0 }, Insn::Ret { src: 0 }],
        vec![Insn::Test {
            cond: 0,
            when: true,
            to: 0,
        }],
    ] {
        let mut b = Builder::default();
        b.push(Cb::code(code, vec![Val::Nil], vec![], Conv::Truthy));
        assert!(matches!(refused(plain_raw(b.0)), PresentError::Jump { .. }));
    }
    // V4: a read of another type, a guard that is not a boolean, a read
    // of an output, a gate that is not a boolean, an `after` that points
    // forward or at a draw.
    let mut b = Builder::default();
    let skin_model::FieldTy::Enum(kind) = skin_model::PLAY.field("gauge.kind").unwrap().1.ty else {
        panic!("the gauge's kind is an enum");
    };
    let gauge = skin_model::present::ReadDef {
        value: expr("gauge.kind", &[], Ty::Enum(kind)),
        guard: None,
    };
    b.push(Cb::code(
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
        vec![],
        vec![gauge],
        Conv::Int,
    ));
    assert_eq!(refused(plain_raw(b.0)), PresentError::ReadType { read: 0 });
    let mut b = Builder::default();
    let mut r = read("combo", Ty::Int, None);
    r.guard = Some(expr("combo", &[], Ty::Int));
    b.push(Cb::code(
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
        vec![],
        vec![r],
        Conv::Int,
    ));
    assert_eq!(refused(plain_raw(b.0)), PresentError::ReadType { read: 0 });
    let mut b = one();
    let sig = b.signature();
    b.push(Cb::code(
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
        vec![],
        vec![skin_model::present::ReadDef {
            value: expr("lua.0", &sig, Ty::Bool),
            guard: None,
        }],
        Conv::Truthy,
    ));
    assert_eq!(refused(plain_raw(b.0)), PresentError::ReadSet { read: 0 });
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).gate(read("combo", Ty::Int, None)));
    assert_eq!(refused(plain_raw(b.0)), PresentError::Gate { callback: 0 });
    let mut b = Builder::default();
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).after(1));
    b.push(Cb::ret(Val::Int(1), Conv::Int));
    assert_eq!(refused(plain_raw(b.0)), PresentError::After { callback: 0 });
    let mut b = one();
    b.push(Cb::ret(Val::Bool(true), Conv::Truthy).after(0));
    assert_eq!(refused(plain_raw(b.0)), PresentError::After { callback: 1 });
    // V5: callbacks whose per-frame cost passes the skin's total.
    let mut b = Builder::default();
    let mut code = vec![Insn::Const { dst: 0, k: 0 }; MAX_CALLBACK_OPS - 1];
    code.push(Insn::Ret { src: 0 });
    let k = b.push(Cb::code(code, vec![Val::Nil], vec![], Conv::Truthy));
    for _ in 0..MAX_SKIN_OPS / MAX_CALLBACK_OPS {
        b.share(k);
    }
    assert_eq!(refused(plain_raw(b.0)), PresentError::Cost);
    // V6: a consumer reading an output the program lacks, one at another
    // type, and a `when` timer reading one.
    let b = one();
    let wide = [OutTy::Bool, OutTy::Int];
    let e = image("lua.1 > 0", &wide, None);
    assert!(matches!(skin(b, vec![e], None), Err(LoadError::OutputRead)));
    let b = one();
    let e = image("lua.0 > 0", &[OutTy::Int], None);
    assert!(matches!(skin(b, vec![e], None), Err(LoadError::OutputRead)));
    let b = one();
    let sig = b.signature();
    let when = expr("lua.0", &sig, Ty::Bool);
    assert!(matches!(
        load(Some(b.0), vec![when], vec![], WidgetDefs::default()),
        Err(LoadError::OutputRead)
    ));
    // Without a program, an output read is refused too.
    let e = image("lua.0", &[OutTy::Bool], None);
    assert!(matches!(
        load(None, vec![], vec![e], WidgetDefs::default()),
        Err(LoadError::OutputRead)
    ));
}

/// `def` loaded with no consumer.
fn plain_raw(def: PresentDef) -> Result<Skin, LoadError> {
    load(Some(def), vec![], vec![], WidgetDefs::default())
}

/// An object whose timer is off still runs its draw callback: the source
/// evaluates `dstdraw` before it checks the timer.
#[test]
fn a_timer_that_is_off_does_not_gate_a_draw() {
    let b = one();
    let sig = b.signature();
    let s = skin(b, vec![image("lua.0", &sig, Some(timer::FAILED))], None).unwrap();
    let mut h = harness(s);
    h.frame(1_000, &[]);
    let f = h.play.state().fields;
    assert_eq!([f[slot(0)], f[slot(0) + 1]], [1, 1]);
    assert!(drawn(&h).is_empty());
}

/// A number's value of `i32::MIN` or `i32::MAX` draws nothing and skips
/// the draw callbacks after it; any other value runs them.
#[test]
fn a_sentinel_value_skips_its_draws() {
    for (v, want) in [
        (i64::from(i32::MAX), [[i32::MAX, 0], [0, 0]]),
        (i64::from(i32::MIN), [[i32::MIN, 0], [0, 0]]),
        (5, [[5, 1], [1, 1]]),
    ] {
        let mut b = Builder::default();
        let k = b.push(Cb::ret(Val::Int(v), Conv::Int));
        b.push(Cb::ret(Val::Bool(true), Conv::Truthy).after(k));
        let sig = b.signature();
        let (e, n) = number("lua.0", Some("lua.0.ok && lua.1"), &sig);
        let s = skin(b, vec![e], Some(n)).unwrap();
        let mut h = harness(s);
        h.frame(1_000, &[]);
        let f = h.play.state().fields;
        assert_eq!(
            [[f[slot(0)], f[slot(0) + 1]], [f[slot(1)], f[slot(1) + 1]]],
            want,
            "{v}"
        );
        assert_eq!(drawn(&h).is_empty(), want[1][1] == 0, "{v}");
    }
}

/// A false condition before a draw callback skips it.
#[test]
fn a_false_condition_before_skips_the_callback() {
    for (combo, want) in [(0, [0, 0]), (5, [1, 1])] {
        let mut b = Builder::default();
        b.push(Cb::ret(Val::Bool(true), Conv::Truthy).gate(read("combo > 3", Ty::Bool, None)));
        assert_eq!(outputs(plain(b).unwrap(), 1, combo), [want], "{combo}");
    }
}

/// Two objects sharing one closure are two callbacks, each invoked.
#[test]
fn a_shared_closure_runs_once_per_object() {
    let mut b = Builder::default();
    let k = b.push(Cb::code(
        vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
        vec![],
        vec![read("combo", Ty::Int, None)],
        Conv::Int,
    ));
    b.share(k);
    let s = plain(b).unwrap();
    #[cfg(feature = "dev-tools")]
    let before = skin_model::present::invocations();
    assert_eq!(outputs(s, 2, 7), [[7, 1], [7, 1]]);
    #[cfg(feature = "dev-tools")]
    assert_eq!(skin_model::present::invocations() - before, 2);
}

/// An expression value with a program draw, and a program value with an
/// expression draw: each consumer shows what the source's would.
#[test]
fn mixed_backends_give_the_source_results() {
    // An expression value (the combo) whose draw is the program's.
    for (combo, shown) in [(2, false), (9, true)] {
        let mut b = Builder::default();
        b.push(Cb::code(
            vec![
                Insn::Read { dst: 0, read: 0 },
                Insn::Const { dst: 1, k: 0 },
                Insn::Cmp {
                    op: skin_model::present::Cmp::Lt,
                    dst: 0,
                    a: 1,
                    b: 0,
                },
                Insn::Ret { src: 0 },
            ],
            vec![Val::Int(5)],
            vec![read("combo", Ty::Int, None)],
            Conv::Truthy,
        ));
        let sig = b.signature();
        let (e, n) = number("combo", Some("lua.0"), &sig);
        let mut h = harness(skin(b, vec![e], Some(n)).unwrap());
        h.tally.combo = combo;
        h.frame(1_000, &[]);
        assert_eq!(!drawn(&h).is_empty(), shown, "{combo}");
    }
    // A program value (the combo times 3) under an expression condition.
    for (combo, digits) in [(0, 0), (4, 2)] {
        let mut b = Builder::default();
        b.push(Cb::code(
            vec![
                Insn::Read { dst: 0, read: 0 },
                Insn::Const { dst: 1, k: 0 },
                Insn::Arith {
                    op: Arith::Mul,
                    dst: 0,
                    a: 0,
                    b: 1,
                },
                Insn::Ret { src: 0 },
            ],
            vec![Val::Int(3)],
            vec![read("combo", Ty::Int, None)],
            Conv::Int,
        ));
        let sig = b.signature();
        let (e, n) = number("lua.0", Some("lua.0.ok && combo > 0"), &sig);
        let mut h = harness(skin(b, vec![e], Some(n)).unwrap());
        h.tally.combo = combo;
        h.frame(1_000, &[]);
        assert_eq!(drawn(&h).len(), digits, "{combo}");
    }
}

/// Each result reaches its consumer as the source converts it: a draw by
/// truthiness, a value by `toint()`; a caught error gives false or 0; an
/// unavailable read, an overflow or a sentinel gives no value.
#[test]
fn results_convert_as_the_source() {
    let nan = Val::Float(f64::NAN);
    let error = |conv| {
        Cb::code(
            vec![
                Insn::Const { dst: 0, k: 0 },
                Insn::Const { dst: 1, k: 1 },
                Insn::Arith {
                    op: Arith::Add,
                    dst: 0,
                    a: 0,
                    b: 1,
                },
                Insn::Ret { src: 0 },
            ],
            vec![Val::Nil, Val::Int(1)],
            vec![],
            conv,
        )
    };
    let unavailable = |conv| {
        Cb::code(
            vec![Insn::Read { dst: 0, read: 0 }, Insn::Ret { src: 0 }],
            vec![],
            vec![read("combo", Ty::Int, Some("combo > 100"))],
            conv,
        )
    };
    let overflow = Cb::code(
        vec![
            Insn::Const { dst: 0, k: 0 },
            Insn::Const { dst: 1, k: 1 },
            Insn::Arith {
                op: Arith::Add,
                dst: 0,
                a: 0,
                b: 1,
            },
            Insn::Ret { src: 0 },
        ],
        vec![Val::Int(i64::MAX), Val::Int(1)],
        vec![],
        Conv::Int,
    );
    let floor = Cb::code(
        vec![
            Insn::Const { dst: 0, k: 0 },
            Insn::Math {
                f: MathFn::Floor,
                dst: 0,
                a: 0,
            },
            Insn::Ret { src: 0 },
        ],
        vec![Val::Float(-2.5)],
        vec![],
        Conv::Int,
    );
    let cases = [
        (Cb::ret(Val::Float(4_294_967_296.0), Conv::Int), [0, 1]),
        (Cb::ret(nan, Conv::Int), [0, 1]),
        (Cb::ret(Val::Float(-0.5), Conv::Int), [0, 1]),
        (Cb::ret(Val::Float(99.9), Conv::Int), [99, 1]),
        (
            Cb::ret(Val::Float(2_147_483_648.0), Conv::Int),
            [i32::MIN, 0],
        ),
        (Cb::ret(Val::Nil, Conv::Int), [0, 1]),
        (Cb::ret(Val::Bool(true), Conv::Int), [0, 1]),
        (Cb::ret(Val::Int((1 << 32) + 7), Conv::Int), [7, 1]),
        (Cb::ret(Val::Float(0.0), Conv::Truthy), [1, 1]),
        (Cb::ret(Val::Int(0), Conv::Truthy), [1, 1]),
        (Cb::ret(Val::Nil, Conv::Truthy), [0, 1]),
        (Cb::ret(Val::Bool(false), Conv::Truthy), [0, 1]),
        (error(Conv::Int), [0, 1]),
        (error(Conv::Truthy), [0, 1]),
        (unavailable(Conv::Int), [0, 0]),
        (unavailable(Conv::Truthy), [0, 0]),
        (overflow, [0, 0]),
        (floor, [-3, 1]),
        // The end of a span with no `Ret` returns nil.
        (
            Cb::code(
                vec![Insn::Const { dst: 0, k: 0 }],
                vec![Val::Int(4)],
                vec![],
                Conv::Int,
            ),
            [0, 1],
        ),
    ];
    let n = cases.len();
    let mut b = Builder::default();
    let mut want = Vec::new();
    for (cb, w) in cases {
        b.push(cb);
        want.push(w);
    }
    assert_eq!(outputs(plain(b).unwrap(), n, 0), want);
}

/// V6 keeps a read's type: a schema field at the first output's slot,
/// an integer of range 0 to 1, read where the program's boolean output is,
/// is refused, though its range is a boolean's.
#[test]
fn an_output_slot_read_as_a_field_is_refused() {
    let mut fields = skin_model::PLAY.fields.to_vec();
    let mut forged = fields[0];
    forged.name = "forged";
    forged.ty = skin_model::FieldTy::Int { lo: 0, hi: 1 };
    fields.push(forged);
    let schema: &'static skin_model::Schema = Box::leak(Box::new(skin_model::Schema {
        fields: Box::leak(fields.into_boxed_slice()),
        ..skin_model::PLAY
    }));
    let mut e = element(Source::Image(0), [0, 0, 10, 10], Place::SCREEN);
    e.visible = Some(
        skin_model::compile("forged == 1", &skin_model::Scope::new(schema), Ty::Bool).unwrap(),
    );
    assert!(matches!(
        skin(one(), vec![e], None),
        Err(LoadError::OutputRead)
    ));
    // The same slot read by its output name loads.
    let b = one();
    let sig = b.signature();
    assert!(skin(b, vec![image("lua.0", &sig, None)], None).is_ok());
}

/// A number whose value is `i32::MIN` or `i32::MAX` draws nothing, on play
/// too, as the source's number does.
#[test]
fn a_reserved_value_draws_no_number() {
    for (value, shown) in [
        ("2147483647", false),
        ("-2147483647 - 1", false),
        ("5", true),
    ] {
        let (e, n) = number(value, None, &[]);
        let s = load(
            None,
            vec![],
            vec![e],
            WidgetDefs {
                numbers: vec![n],
                ..WidgetDefs::default()
            },
        )
        .unwrap();
        let mut h = harness(s);
        h.frame(1_000, &[]);
        assert_eq!(!drawn(&h).is_empty(), shown, "{value}");
    }
}
