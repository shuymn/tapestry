//! Properties of expression compilation and evaluation over generated
//! well-typed trees, checked against an independent reference interpreter.
//!
//! - P1 agreement: a compiled program evaluates to the reference's value.
//! - P2 soundness: when a tree compiles, no node's reference value leaves
//!   `i32` (the range check is sound).
//! - P3 no spurious errors: a generated tree is refused only for its range,
//!   depth, length or stack depth; a tree of the bounded subset compiles.
//! - P4 clamping: arbitrary state slices evaluate as their clamped state.
//! - P5 folding is invisible: under every option configuration a tree
//!   compiles the same way and agrees with the reference.
//! - P6 totality: arbitrary text never panics, and error spans lie inside
//!   the source.
//!
//! `generators_reach_the_fragile_spots` checks that the generators still
//! reach the cases the properties need.

// A crate root's modules sit beside it, so the paths name the directory
// that keeps them out of Cargo's test discovery.
mod common;
#[path = "props/reference.rs"]
mod reference;
#[path = "props/strategy.rs"]
mod strategy;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use skin_model::{
    CompileError, EnumId, ErrorKind, MAX_SOURCE_BYTES, Program, Scope, State, Ty, compile,
};

use common::{SCHEMA, options};
use reference::{Options, Reference, Seen};
use strategy::{
    Arith, E, InRange, RawState, RawTree, Root, build, build_state, clamp_state, print,
};

fn raw_tree() -> impl Strategy<Value = RawTree> {
    (any::<u8>(), vec(any::<u32>(), 0..96)).prop_map(|(root, choices)| RawTree { root, choices })
}

fn raw_state() -> impl Strategy<Value = RawState> {
    (any::<[u32; 7]>(), any::<[u32; 2]>()).prop_map(|(fields, timers)| RawState { fields, timers })
}

fn want(root: Root) -> Ty {
    match root {
        Root::Int => Ty::Int,
        Root::Bool => Ty::Bool,
        Root::Enum(e) => Ty::Enum(EnumId(u16::try_from(e).unwrap())),
    }
}

fn compile_under(src: &str, root: Root, opts: Options) -> Result<Program, CompileError> {
    let defs = options(opts.cover, opts.style);
    compile(src, &Scope::new(&SCHEMA).options(&defs), want(root))
}

fn eval(p: &Program, s: &InRange) -> i32 {
    p.eval(&State {
        fields: &s.fields,
        timers: &s.timers,
    })
}

/// The refusals a generated well-typed tree may meet.
fn allowed(kind: &ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::OutOfRange { .. }
            | ErrorKind::TooDeep
            | ErrorKind::TooManyOps { .. }
            | ErrorKind::StackTooDeep { .. }
    )
}

const ALL_OPTIONS: [Options; 6] = [
    Options {
        cover: false,
        style: 0,
    },
    Options {
        cover: false,
        style: 1,
    },
    Options {
        cover: false,
        style: 2,
    },
    Options {
        cover: true,
        style: 0,
    },
    Options {
        cover: true,
        style: 1,
    },
    Options {
        cover: true,
        style: 2,
    },
];

/// Checks P1 to P3 for one tree, state and option configuration; returns
/// whether the tree compiled.
fn check_one(e: &E, root: Root, s: &InRange, opts: Options) -> Result<bool, TestCaseError> {
    let src = print(e);
    prop_assert!(
        src.len() <= MAX_SOURCE_BYTES,
        "generated source too long: {}",
        src.len()
    );
    let mut r = Reference::new(s, opts);
    let expected = r.eval(e);
    match compile_under(&src, root, opts) {
        Ok(p) => {
            prop_assert!(r.all_in_i32(), "{src}: compiled, but a value leaves i32");
            prop_assert_eq!(i128::from(eval(&p, s)), expected, "{}", src);
            Ok(true)
        }
        Err(err) => {
            prop_assert!(allowed(&err.kind), "{src}: {err:?}");
            Ok(false)
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// P1, P2 and P3 over the whole generator.
    #[test]
    fn compiled_programs_agree_with_the_reference(raw in raw_tree(), state in raw_state()) {
        let (e, root) = build(&raw, false);
        check_one(&e, root, &build_state(&state), Options { cover: true, style: 1 })?;
    }

    /// P1 over the bounded subset, every tree of which must compile.
    #[test]
    fn bounded_trees_compile_and_agree(raw in raw_tree(), state in raw_state()) {
        let (e, root) = build(&raw, true);
        let compiled = check_one(&e, root, &build_state(&state), Options { cover: false, style: 2 })?;
        prop_assert!(compiled, "{} was refused", print(&e));
    }

    /// P4: arbitrary slices of any length read as their clamped state.
    #[test]
    fn reads_clamp_arbitrary_state(
        raw in raw_tree(),
        fields in vec(any::<i32>(), 0..10),
        timers in vec(any::<i32>(), 0..4),
    ) {
        let (e, root) = build(&raw, raw.root % 8 < 4);
        let opts = Options { cover: true, style: 0 };
        if let Ok(p) = compile_under(&print(&e), root, opts) {
            let clamped = clamp_state(&fields, &timers);
            let expected = Reference::new(&clamped, opts).eval(&e);
            let got = p.eval(&State { fields: &fields, timers: &timers });
            prop_assert_eq!(i128::from(got), expected);
        }
    }

    /// P5: compiling does not depend on the options, and each configuration
    /// agrees with the reference.
    #[test]
    fn folding_is_invisible(raw in raw_tree(), state in raw_state()) {
        let (e, root) = build(&raw, raw.root % 8 < 4);
        let s = build_state(&state);
        let mut outcomes = Vec::new();
        for opts in ALL_OPTIONS {
            outcomes.push(check_one(&e, root, &s, opts)?);
        }
        prop_assert!(
            outcomes.iter().all(|&o| o == outcomes[0]),
            "{}: compiles under some options only: {outcomes:?}",
            print(&e)
        );
    }

    /// P6: arbitrary ASCII never panics, and spans lie inside the source.
    #[test]
    fn arbitrary_ascii_is_total(bytes in vec(0x20u8..0x7f, 0..MAX_SOURCE_BYTES + 16)) {
        let src = String::from_utf8(bytes).unwrap();
        check_total(&src)?;
    }

    /// P6 over token soup, which reaches deeper than random characters.
    #[test]
    fn token_soup_is_total(tokens in vec(0usize..SOUP.len(), 0..400), any_char in any::<char>()) {
        let mut src: String = tokens.iter().map(|&t| SOUP[t]).collect();
        if tokens.len() % 7 == 0 {
            src.push(any_char);
        }
        check_total(&src)?;
    }
}

const SOUP: &[&str] = &[
    "(",
    ")",
    "if ",
    " then ",
    " else ",
    "min(",
    "max(",
    "clamp(",
    "abs(",
    "on(",
    "elapsed(",
    "option(",
    "-",
    "!",
    "+",
    "*",
    "/",
    "%",
    "==",
    "!=",
    "<",
    ">=",
    "&&",
    "||",
    ",",
    " ",
    "small",
    "full",
    "flag",
    "grade",
    "great",
    "play",
    "judge.great",
    "cover",
    "style",
    "modern",
    "1",
    "0",
    "2147483648",
    "1.5s",
    "120ms",
    "true",
    "x",
];

fn check_total(src: &str) -> Result<(), TestCaseError> {
    let len = u32::try_from(src.len()).unwrap();
    let defs = options(true, 0);
    let scope = Scope::new(&SCHEMA).options(&defs);
    for want in [Ty::Int, Ty::Bool, Ty::Enum(EnumId(0))] {
        if let Err(e) = compile(src, &scope, want) {
            prop_assert!(
                e.span.start <= e.span.end && e.span.end <= len,
                "{src:?}: {e:?}"
            );
        }
    }
    Ok(())
}

/// Whether `e` holds an operator `op` whose divisor is exactly `small`,
/// whose interval `[-10, 10]` is mixed-sign and holds 0.
fn has_mixed_divisor(e: &E, op: Arith) -> bool {
    let mut found = false;
    visit(e, &mut |n| {
        if let E::Arith(o, _, b) = n {
            found |= *o == op && matches!(**b, E::Field(strategy::SMALL));
        }
    });
    found
}

/// The arithmetic operators in `e` with a dynamic operand (one reading a
/// field), which folding keeps.
fn dynamic_ops(e: &E, out: &mut Vec<&'static str>) {
    visit(e, &mut |n| {
        let name = match n {
            E::Neg(_) => "neg",
            E::Abs(_) => "abs",
            E::Clamp(..) => "clamp",
            E::Arith(op, ..) => match op {
                Arith::Add => "+",
                Arith::Sub => "-",
                Arith::Mul => "*",
                Arith::Div => "/",
                Arith::Rem => "%",
                Arith::Min => "min",
                Arith::Max => "max",
            },
            _ => return,
        };
        if reads_field(n) {
            out.push(name);
        }
    });
}

fn reads_field(e: &E) -> bool {
    let mut found = false;
    visit(e, &mut |n| found |= matches!(n, E::Field(_)));
    found
}

fn visit<'e>(e: &'e E, f: &mut impl FnMut(&'e E)) {
    f(e);
    match e {
        E::Neg(a) | E::Abs(a) | E::Not(a) => visit(a, f),
        E::Arith(_, a, b) | E::Cmp(_, a, b) | E::And(a, b) | E::Or(a, b) => {
            visit(a, f);
            visit(b, f);
        }
        E::Clamp(a, b, c) | E::If(a, b, c) => {
            visit(a, f);
            visit(b, f);
            visit(c, f);
        }
        _ => {}
    }
}

/// The generators' own checks: a generator change cannot make a property
/// vacuous.
#[test]
fn generators_reach_the_fragile_spots() {
    let mut runner = TestRunner::deterministic();
    let mut seen = Seen::default();
    let mut out_of_range = false;
    let mut accepted_full = 0;
    let mut bounded_ops = Vec::new();
    let (mut mixed_div, mut mixed_rem) = (false, false);
    let (mut full_positive, mut full_negative) = (false, false);
    for i in 0..4000 {
        let raw = raw_tree().new_tree(&mut runner).unwrap().current();
        let state = build_state(&raw_state().new_tree(&mut runner).unwrap().current());
        let full = state.fields[strategy::FULL];
        full_positive |= (2..i32::MAX - 1).contains(&full);
        full_negative |= (i32::MIN + 1..-2).contains(&full);
        let bounded = i % 2 == 0;
        let (e, root) = build(&raw, bounded);
        let opts = ALL_OPTIONS[i % ALL_OPTIONS.len()];
        let mut r = Reference::new(&state, opts);
        r.eval(&e);
        let s = r.seen;
        seen.zero_divisor |= s.zero_divisor;
        seen.negative_divisor |= s.negative_divisor;
        seen.off_timer |= s.off_timer;
        for k in 0..2 {
            seen.cover[k] |= s.cover[k];
            seen.choice[k] |= s.choice[k];
        }
        match compile_under(&print(&e), root, opts) {
            Ok(p) if bounded => {
                if p.op_count() > 1 {
                    dynamic_ops(&e, &mut bounded_ops);
                    mixed_div |= has_mixed_divisor(&e, Arith::Div);
                    mixed_rem |= has_mixed_divisor(&e, Arith::Rem);
                }
            }
            Ok(_) => accepted_full += 1,
            Err(err) => out_of_range |= matches!(err.kind, ErrorKind::OutOfRange { .. }),
        }
    }
    assert!(seen.zero_divisor, "no zero divisor");
    assert!(seen.negative_divisor, "no negative divisor");
    assert!(seen.off_timer, "no off timer");
    assert_eq!(seen.cover, [true, true], "toggle states");
    assert_eq!(seen.choice, [true, true], "choice states");
    assert!(out_of_range, "no OutOfRange refusal");
    assert!(full_positive, "no mid-range positive `full`");
    assert!(full_negative, "no mid-range negative `full`");
    assert!(
        accepted_full > 100,
        "only {accepted_full} unbounded trees compiled"
    );
    for op in ["neg", "abs", "clamp", "+", "-", "*", "/", "%", "min", "max"] {
        assert!(bounded_ops.contains(&op), "no accepted dynamic `{op}`");
    }
    assert!(
        mixed_div,
        "no accepted `/` by a mixed-sign divisor holding 0"
    );
    assert!(
        mixed_rem,
        "no accepted `%` by a mixed-sign divisor holding 0"
    );
}

fn node_count(e: &E) -> u32 {
    let mut n = 0;
    visit(e, &mut |_| n += 1);
    n
}

/// Deterministic sweeps of raw choices, including a case that once grew
/// past the node budget (pending siblings were not reserved): a tree stays
/// within `MAX_NODES` and its source within `MAX_SOURCE_BYTES`.
#[test]
fn generated_trees_stay_within_their_budget() {
    let mut cases = vec![RawTree {
        root: 2,
        choices: [22, 15, 0].repeat(32),
    }];
    for root in 0..8 {
        for c in 0..256 {
            cases.push(RawTree {
                root,
                choices: vec![c; 96],
            });
            for (a, b) in [(15, 0), (0, 15), (3, 6), (22, 1)] {
                cases.push(RawTree {
                    root,
                    choices: [c, a, b].repeat(32),
                });
            }
        }
    }
    for raw in &cases {
        for bounded in [false, true] {
            let (e, _) = build(raw, bounded);
            let src = print(&e);
            let n = node_count(&e);
            assert!(n <= strategy::MAX_NODES, "{raw:?}: {n} nodes");
            assert!(
                src.len() <= MAX_SOURCE_BYTES,
                "{raw:?}: {} bytes",
                src.len()
            );
        }
    }
}
