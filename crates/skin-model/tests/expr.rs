//! Expression compilation and evaluation through the public API: the
//! lexer, the parser, the floor operators, intervals, types, options,
//! clamped reads, and every bound at its limit and one past it.

mod common;

use common::{SCHEMA, options};
use skin_model::{
    CompileError, EnumId, ErrorKind, MAX_ELAPSED_MS, MAX_OPS, MAX_SKIN_OPS, MAX_SOURCE_BYTES,
    Program, Scope, SkinOps, Span, State, TIMER_OFF, Ty, compile,
};

const I32_TOP: i64 = 1 << 31;

fn compile_opts(src: &str, want: Ty, cover: bool, style: usize) -> Result<Program, CompileError> {
    let opts = options(cover, style);
    compile(src, &Scope::new(&SCHEMA).options(&opts), want)
}

fn ok(src: &str, want: Ty) -> Program {
    compile_opts(src, want, true, 1).unwrap_or_else(|e| panic!("{src:?}: {e:?}"))
}

fn err(src: &str, want: Ty) -> CompileError {
    match compile_opts(src, want, true, 1) {
        Ok(p) => panic!("{src:?} compiled to {p:?}"),
        Err(e) => e,
    }
}

fn kind(src: &str) -> ErrorKind {
    err(src, Ty::Int).kind
}

fn bool_kind(src: &str) -> ErrorKind {
    err(src, Ty::Bool).kind
}

/// Evaluates `src` with every field 0 and every timer off.
fn int(src: &str) -> i32 {
    ok(src, Ty::Int).eval(&State {
        fields: &[0; 7],
        timers: &[TIMER_OFF; 2],
    })
}

fn boolean(src: &str) -> bool {
    ok(src, Ty::Bool).eval_bool(&State {
        fields: &[0; 7],
        timers: &[TIMER_OFF; 2],
    })
}

fn eval_with(src: &str, want: Ty, fields: &[i32], timers: &[i32]) -> i32 {
    ok(src, want).eval(&State { fields, timers })
}

/// `small` (slot 1) set to `v`.
fn small(v: i32) -> [i32; 7] {
    [0, v, 0, 0, 0, 0, 0]
}

fn span(start: u32, end: u32) -> Span {
    Span { start, end }
}

fn out_of_range(lo: i64, hi: i64) -> ErrorKind {
    ErrorKind::OutOfRange { lo, hi }
}

// Lexer

#[test]
fn durations_are_integer_ms() {
    assert_eq!(int("120ms"), 120);
    assert_eq!(int("1.5s"), 1500);
    assert_eq!(int("1.005s"), 1005);
    assert_eq!(int("2s + 1ms"), 2001);
}

#[test]
fn malformed_literals_are_refused_with_their_spans() {
    for (src, span_, kind_) in [
        ("1 + 1.5ms", span(4, 9), ErrorKind::BadDuration),
        ("1.0005s", span(0, 7), ErrorKind::BadDuration),
        ("1.5", span(0, 3), ErrorKind::BadDuration),
        ("12msx", span(0, 5), ErrorKind::BadDuration),
        ("2147483648ms", span(0, 12), ErrorKind::IntTooLarge),
        ("2147483648", span(0, 10), ErrorKind::IntTooLarge),
        ("1 - 2147483648", span(4, 14), ErrorKind::IntTooLarge),
        ("-(2147483648)", span(2, 12), ErrorKind::IntTooLarge),
        (
            "99999999999999999999999",
            span(0, 23),
            ErrorKind::IntTooLarge,
        ),
        ("1 + é", span(4, 6), ErrorKind::UnexpectedChar('é')),
        ("1 & 2", span(2, 3), ErrorKind::UnexpectedChar('&')),
        ("Small", span(0, 1), ErrorKind::UnexpectedChar('S')),
    ] {
        assert_eq!(
            err(src, Ty::Int),
            CompileError {
                span: span_,
                kind: kind_
            },
            "{src:?}"
        );
    }
}

#[test]
fn the_most_negative_literal_is_only_a_direct_negation() {
    assert_eq!(int("-2147483648"), i32::MIN);
    assert_eq!(int("- 2147483648"), i32::MIN);
    assert_eq!(int("2147483647"), i32::MAX);
}

#[test]
fn names_are_bounded() {
    let at_limit = "a".repeat(64);
    assert!(matches!(kind(&at_limit), ErrorKind::UnknownName { .. }));
    let past = "a".repeat(65);
    assert_eq!(
        err(&past, Ty::Int),
        CompileError {
            span: span(0, 65),
            kind: ErrorKind::NameTooLong
        }
    );
}

// Parser

#[test]
fn precedence_follows_the_table() {
    // `if`: the else branch extends as far as it can.
    assert_eq!(int("if flag then 1 else 2 + 3"), 5);
    assert_eq!(int("1 + if flag then 1 else 2 * 3"), 7);
    // `||` below `&&`.
    assert!(boolean("true || false && false"));
    // `&&` below `==`.
    assert!(!boolean("false && false == false"));
    // `==` below `<`.
    assert!(boolean("1 < 2 == true"));
    // `<` below `+`.
    assert!(boolean("1 + 2 < 4"));
    assert!(boolean("4 >= 1 + 3"));
    // `+` below `*`; both left-associative.
    assert_eq!(int("2 + 3 * 4"), 14);
    assert_eq!(int("7 - 2 - 1"), 4);
    assert_eq!(int("16 / 4 / 2"), 2);
    assert_eq!(int("2 * 7 % 4"), 2);
    // Unary binds tighter than `*`: (-7) / 2, not -(7 / 2).
    assert_eq!(int("- 7 / 2"), -4);
    assert!(!boolean("!true && true"));
    // Comparisons by slot values.
    assert!(!boolean("small > ms"));
    assert!(boolean("small >= ms"));
    assert!(!boolean("small >= ms + 1"));
}

#[test]
fn comparisons_do_not_chain() {
    assert_eq!(
        err("1 < 2 < 3", Ty::Bool),
        CompileError {
            span: span(6, 7),
            kind: ErrorKind::NonAssociative
        }
    );
    assert_eq!(bool_kind("1 == 1 == true"), ErrorKind::NonAssociative);
    assert_eq!(bool_kind("flag == flag != flag"), ErrorKind::NonAssociative);
    assert!(boolean("(1 < 2) == (2 < 3)"));
}

#[test]
fn syntax_errors() {
    assert_eq!(
        kind("if flag then 1"),
        ErrorKind::Expected { what: "`else`" }
    );
    assert_eq!(
        kind("if flag 1 else 2"),
        ErrorKind::Expected { what: "`then`" }
    );
    assert_eq!(err("1 2", Ty::Int).span, span(2, 3));
    assert!(matches!(kind("1 2"), ErrorKind::Expected { .. }));
    assert_eq!(kind("(1"), ErrorKind::Expected { what: "`)`" });
    assert_eq!(
        kind("1 +"),
        ErrorKind::Expected {
            what: "an expression"
        }
    );
    assert_eq!(kind("min(1 2)"), ErrorKind::Expected { what: "`,` or `)`" });
    assert_eq!(
        kind("min(1)"),
        ErrorKind::Arity {
            func: "min",
            expected: 2,
            found: 1
        }
    );
    assert_eq!(
        kind("abs(1, 2)"),
        ErrorKind::Arity {
            func: "abs",
            expected: 1,
            found: 2
        }
    );
    assert_eq!(
        kind("clamp()"),
        ErrorKind::Arity {
            func: "clamp",
            expected: 3,
            found: 0
        }
    );
    assert_eq!(
        kind("foo(1)"),
        ErrorKind::UnknownFunction { name: "foo".into() }
    );
}

#[test]
fn arity_counts_any_number_of_arguments() {
    let src = format!("min({})", ["1"; 300].join(","));
    assert!(src.len() <= MAX_SOURCE_BYTES);
    assert_eq!(
        kind(&src),
        ErrorKind::Arity {
            func: "min",
            expected: 2,
            found: 300
        }
    );
}

// Floor division and modulo

#[test]
fn division_and_modulo_floor_and_give_zero_for_a_zero_divisor() {
    // Rows a in {-7, -1, 0, 1, 7}; columns b in {-2, -1, 0, 1, 2}.
    let a = [-7, -1, 0, 1, 7];
    let b = [-2, -1, 0, 1, 2];
    let div = [
        [3, 7, 0, -7, -4],
        [0, 1, 0, -1, -1],
        [0, 0, 0, 0, 0],
        [-1, -1, 0, 1, 0],
        [-4, -7, 0, 7, 3],
    ];
    let rem = [
        [-1, 0, 0, 0, 1],
        [-1, 0, 0, 0, 1],
        [0, 0, 0, 0, 0],
        [-1, 0, 0, 0, 1],
        [-1, 0, 0, 0, 1],
    ];
    for (i, &x) in a.iter().enumerate() {
        for (j, &y) in b.iter().enumerate() {
            for (op, table) in [("/", div), ("%", rem)] {
                let want = table[i][j];
                // Folded at compile time.
                assert_eq!(int(&format!("({x}) {op} ({y})")), want, "{x} {op} {y}");
                // Evaluated: a dynamic dividend, then a dynamic divisor.
                let dividend = format!("small {op} ({y})");
                assert_eq!(eval_with(&dividend, Ty::Int, &small(x), &[]), want);
                let divisor = format!("({x}) {op} small");
                assert_eq!(eval_with(&divisor, Ty::Int, &small(y), &[]), want);
            }
        }
    }
    assert_eq!(int("7 % 0"), 0);
    assert_eq!(eval_with("small % zero", Ty::Int, &small(7), &[]), 0);
    assert_eq!(eval_with("small / zero", Ty::Int, &small(7), &[]), 0);
}

#[test]
fn the_one_overflowing_division_is_refused_and_modulo_never_overflows() {
    assert_eq!(
        kind("(-2147483647 - 1) / -1"),
        out_of_range(I32_TOP, I32_TOP)
    );
    assert_eq!(int("(-2147483647 - 1) % -1"), 0);
    let mut fields = [0; 7];
    fields[0] = i32::MIN;
    assert_eq!(eval_with("full % -1", Ty::Int, &fields, &[]), 0);
    assert_eq!(eval_with("full % small", Ty::Int, &fields, &[]), 0);
}

// Intervals

#[test]
fn each_operator_is_accepted_at_the_boundary_and_refused_past_it() {
    let max = i64::from(i32::MAX);
    let min = i64::from(i32::MIN);
    // `+` and `-`.
    assert_eq!(int("ms + 2147482647"), 2_147_482_647);
    assert_eq!(
        kind("ms + 2147482648"),
        out_of_range(2_147_482_648, I32_TOP)
    );
    assert_eq!(int("-2147482648 - ms"), -2_147_482_648);
    assert_eq!(
        kind("-2147482649 - ms"),
        out_of_range(min - 1, -2_147_482_649)
    );
    // `*`: the extremes come from the corners, including the two negative
    // ends.
    ok("clamp(small, -8, 7) * 268435456", Ty::Int);
    assert_eq!(
        kind("clamp(small, -8, 7) * -268435456"),
        out_of_range(-7 * (1 << 28), I32_TOP)
    );
    assert_eq!(
        kind("clamp(small, -8, 8) * 268435456"),
        out_of_range(min, I32_TOP)
    );
    // `/`.
    ok("full / 1", Ty::Int);
    assert_eq!(kind("full / -1"), out_of_range(-max, I32_TOP));
    assert_eq!(kind("full / small"), out_of_range(min, I32_TOP));
    // `%` cannot leave its operands' range.
    ok("full % full", Ty::Int);
    // `abs` and unary `-`.
    ok("abs(max(full, -2147483647))", Ty::Int);
    assert_eq!(kind("abs(full)"), out_of_range(0, I32_TOP));
    ok("-max(full, -2147483647)", Ty::Int);
    assert_eq!(kind("-full"), out_of_range(-max, I32_TOP));
    // Elapsed time is bounded by 4 h, so `* 100` fits and `* 1000` does not.
    ok("elapsed(play) * 100", Ty::Int);
    assert!(matches!(
        kind("elapsed(play) * 1000"),
        ErrorKind::OutOfRange { .. }
    ));
}

#[test]
fn clamp_is_total_when_the_bounds_cross() {
    for v in [-10, 0, 10] {
        assert_eq!(
            eval_with("clamp(small, 5, -5)", Ty::Int, &small(v), &[]),
            -5
        );
    }
    assert_eq!(eval_with("clamp(small, -3, 3)", Ty::Int, &small(9), &[]), 3);
}

#[test]
fn dead_branches_are_range_checked() {
    assert!(matches!(
        kind("if option(cover) then 1 else full + 1"),
        ErrorKind::OutOfRange { .. }
    ));
    assert!(matches!(
        kind("if true then 1 else full * 2"),
        ErrorKind::OutOfRange { .. }
    ));
    assert!(matches!(
        bool_kind("false && full * 2 > 0"),
        ErrorKind::OutOfRange { .. }
    ));
}

// Types

#[test]
fn type_errors() {
    assert_eq!(kind("flag + 1"), ErrorKind::NotOrdered { found: Ty::Bool });
    assert_eq!(
        bool_kind("1 && flag"),
        ErrorKind::Type {
            expected: Ty::Bool,
            found: Ty::Int
        }
    );
    assert_eq!(
        kind("if flag then 1 else true"),
        ErrorKind::Type {
            expected: Ty::Int,
            found: Ty::Bool
        }
    );
    assert_eq!(
        bool_kind("grade == gauge"),
        ErrorKind::Type {
            expected: Ty::Enum(EnumId(0)),
            found: Ty::Enum(EnumId(1))
        }
    );
    assert_eq!(
        bool_kind("grade == fast"),
        ErrorKind::UnknownVariant {
            name: "fast".into()
        }
    );
    assert_eq!(
        bool_kind("small == great"),
        ErrorKind::UnknownName {
            name: "great".into()
        }
    );
    assert_eq!(
        bool_kind("grade < grade"),
        ErrorKind::NotOrdered {
            found: Ty::Enum(EnumId(0))
        }
    );
    assert_eq!(
        err("1", Ty::Bool),
        CompileError {
            span: span(0, 1),
            kind: ErrorKind::Type {
                expected: Ty::Bool,
                found: Ty::Int
            }
        }
    );
    assert_eq!(
        bool_kind("on(small)"),
        ErrorKind::NotATimer {
            name: "small".into()
        }
    );
    assert_eq!(
        bool_kind("on(nothing)"),
        ErrorKind::UnknownTimer {
            name: "nothing".into()
        }
    );
    assert_eq!(
        bool_kind("on(1)"),
        ErrorKind::Expected {
            what: "a timer name"
        }
    );
    assert_eq!(
        kind("play"),
        ErrorKind::UnknownName {
            name: "play".into()
        }
    );
}

#[test]
fn enum_variants_compare_on_either_side() {
    let mut fields = [0; 7];
    fields[5] = 1;
    assert_eq!(eval_with("grade == great", Ty::Bool, &fields, &[]), 1);
    assert_eq!(eval_with("great == grade", Ty::Bool, &fields, &[]), 1);
    assert_eq!(eval_with("grade != perfect", Ty::Bool, &fields, &[]), 1);
    assert_eq!(eval_with("grade", ok_enum(), &fields, &[]), 1);
    assert_eq!(
        eval_with(
            "(if flag then grade else grade) == poor",
            Ty::Bool,
            &fields,
            &[]
        ),
        0
    );
}

fn ok_enum() -> Ty {
    Ty::Enum(EnumId(0))
}

// Options

#[test]
fn options_fold_to_constants() {
    assert!(boolean("option(cover)"));
    assert!(
        !compile_opts("option(cover)", Ty::Bool, false, 0)
            .unwrap()
            .eval_bool(&State {
                fields: &[],
                timers: &[]
            })
    );
    assert!(boolean("option(style, modern)"));
    assert!(!boolean("option(style, mini)"));
    assert_eq!(
        bool_kind("option(nope)"),
        ErrorKind::UnknownOption {
            name: "nope".into()
        }
    );
    assert_eq!(
        bool_kind("option(style, huge)"),
        ErrorKind::UnknownChoice {
            option: "style".into(),
            choice: "huge".into()
        }
    );
    assert_eq!(
        bool_kind("option(style)"),
        ErrorKind::Arity {
            func: "option",
            expected: 2,
            found: 1
        }
    );
    assert_eq!(
        bool_kind("option(cover, modern)"),
        ErrorKind::Arity {
            func: "option",
            expected: 1,
            found: 2
        }
    );
    assert_eq!(
        ok("if option(cover) then 2 * 3 else 4", Ty::Int).op_count(),
        1
    );
}

#[test]
fn folding_leaves_one_operation_around_a_dynamic_leaf() {
    let fields = [0, 7, 300, 0, 1, 0, 0];
    let state = State {
        fields: &fields,
        timers: &[],
    };
    for (cover, want) in [(true, 7), (false, 300)] {
        let p = compile_opts("if option(cover) then small else ms", Ty::Int, cover, 0).unwrap();
        assert_eq!(p.op_count(), 1, "cover {cover}");
        assert_eq!(p.eval(&state), want);
    }
    for (src, want) in [
        ("true && flag", 1),
        ("false && flag", 0),
        ("flag || true", 1),
        ("flag || false", 1),
        ("flag && true", 1),
        ("false || flag", 1),
    ] {
        let p = ok(src, Ty::Bool);
        assert_eq!(p.op_count(), 1, "{src}");
        assert_eq!(p.eval(&state), want, "{src}");
    }
    // A dynamic operation stays.
    assert_eq!(ok("small + 1", Ty::Int).op_count(), 3);
}

// Clamped reads

#[test]
fn reads_are_clamped_into_their_declared_ranges() {
    let read = |src: &str, want: Ty, fields: &[i32]| eval_with(src, want, fields, &[]);
    assert_eq!(read("small", Ty::Int, &small(100)), 10);
    assert_eq!(read("small", Ty::Int, &small(-100)), -10);
    let mut fields = [0; 7];
    fields[4] = 7;
    fields[5] = 99;
    assert_eq!(read("flag", Ty::Bool, &fields), 1);
    assert_eq!(read("!flag", Ty::Bool, &fields), 0);
    assert_eq!(read("grade", ok_enum(), &fields), 4);
    fields[5] = -3;
    assert_eq!(read("grade", ok_enum(), &fields), 0);
    // A missing slot reads the low end.
    assert_eq!(read("small", Ty::Int, &[]), -10);
    assert_eq!(read("ms", Ty::Int, &[]), 0);
    assert_eq!(read("flag", Ty::Bool, &[]), 0);
    assert_eq!(read("grade", ok_enum(), &[0, 0]), 0);
}

#[test]
fn timers_read_clamped_or_off() {
    for (raw, elapsed) in [
        (-5, TIMER_OFF),
        (-1, TIMER_OFF),
        (0, 0),
        (500, 500),
        (i32::MAX, MAX_ELAPSED_MS),
        (i32::MIN, TIMER_OFF),
    ] {
        let timers = [raw, 0];
        assert_eq!(eval_with("elapsed(play)", Ty::Int, &[], &timers), elapsed);
        assert_eq!(
            eval_with("on(play)", Ty::Bool, &[], &timers),
            i32::from(elapsed >= 0)
        );
    }
    assert_eq!(
        eval_with("elapsed(judge.great)", Ty::Int, &[], &[5]),
        TIMER_OFF
    );
    assert_eq!(eval_with("on(judge.great)", Ty::Bool, &[], &[5, 9]), 1);
}

// Bounds

#[test]
fn source_length_is_bounded() {
    let at = format!("1{}", " ".repeat(MAX_SOURCE_BYTES - 1));
    assert_eq!(int(&at), 1);
    let past = format!("{at} ");
    assert_eq!(
        err(&past, Ty::Int),
        CompileError {
            span: span(0, 1025),
            kind: ErrorKind::TooLong { len: 1025 }
        }
    );
}

#[test]
fn nesting_is_bounded_at_64_levels() {
    let parens = |n: usize| format!("{}small{}", "(".repeat(n), ")".repeat(n));
    ok(&parens(64), Ty::Int);
    assert_eq!(kind(&parens(65)), ErrorKind::TooDeep);
    let nots = |n: usize| format!("{}flag", "!".repeat(n));
    ok(&nots(64), Ty::Bool);
    assert_eq!(bool_kind(&nots(65)), ErrorKind::TooDeep);
    let negs = |n: usize| format!("{}small", "- ".repeat(n));
    ok(&negs(64), Ty::Int);
    assert_eq!(kind(&negs(65)), ErrorKind::TooDeep);
    let calls = |n: usize| format!("{}small{}", "abs(".repeat(n), ")".repeat(n));
    ok(&calls(64), Ty::Int);
    assert_eq!(kind(&calls(65)), ErrorKind::TooDeep);
    // A left chain is built without recursion; 64 operators join 65 terms.
    let chain = |terms: usize| format!("small{}", " + zero".repeat(terms - 1));
    ok(&chain(65), Ty::Int);
    assert_eq!(kind(&chain(66)), ErrorKind::TooDeep);
}

/// A sum of 16 products of 8 `zero`s (255 operations), with `extra` unary
/// minuses in front.
fn ops_source(extra: usize) -> String {
    let product = ["zero"; 8].join("*");
    format!("{}{}", "-".repeat(extra), [product.as_str(); 16].join("+"))
}

#[test]
fn operations_are_bounded() {
    let at = ok(&ops_source(1), Ty::Int);
    assert_eq!(at.op_count(), MAX_OPS);
    assert_eq!(
        kind(&ops_source(2)),
        ErrorKind::TooManyOps { ops: MAX_OPS + 1 }
    );
}

#[test]
fn stack_depth_is_bounded() {
    // Each `if` leaves its condition and then-branch on the stack while
    // its else-branch runs; the innermost else needs two more slots.
    let chain = |n: usize| format!("{}small + zero", "if flag then small else ".repeat(n));
    assert_eq!(ok(&chain(15), Ty::Int).stack_depth(), 32);
    assert_eq!(kind(&chain(16)), ErrorKind::StackTooDeep { depth: 34 });
}

#[test]
fn skin_operations_are_bounded() {
    let p = ok(&ops_source(1), Ty::Int);
    let mut skin = SkinOps::default();
    for _ in 0..MAX_SKIN_OPS / MAX_OPS {
        skin.charge(&p).unwrap();
    }
    assert_eq!(skin.total(), MAX_SKIN_OPS);
    let one = ok("1", Ty::Int);
    assert_eq!(
        skin.charge(&one).unwrap_err().kind,
        ErrorKind::TooManySkinOps
    );
    assert_eq!(skin.total(), MAX_SKIN_OPS);
}

#[test]
fn the_test_schema_is_valid() {
    assert_eq!(SCHEMA.check(), Ok(()));
}
