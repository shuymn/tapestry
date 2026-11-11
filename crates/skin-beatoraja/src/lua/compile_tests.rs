//! The compiler's interval pass on residuals built by hand (design.md
//! "beatoraja Lua Skins", Numbers): an integer `//` reaches no closed
//! candidate, since any `//` opens it, so the divisor refusal is pinned
//! here, below the import.

use super::Unsupported;
use super::callback::Analysis;
use super::compile::{Iv, Leaf, Leaves, intervals};
use super::residual::{MathOp, Node, R, Residual, Stop, Val};
use super::syntax::Bin;

/// Every read an integer in `range`.
struct Reads {
    range: (i64, i64),
}

impl Leaves for Reads {
    fn leaf(&mut self, _: &Residual, _: u32) -> R<Option<Leaf>> {
        Ok(None)
    }

    fn range(&self, _: u32) -> Option<(i64, i64)> {
        Some(self.range)
    }

    fn guards(&self, _: &Residual) -> Vec<Vec<&'static str>> {
        Vec::new()
    }
}

/// `main_state.option` and `main_state.number`, by their indices among
/// the host's APIs.
const OPTION: u16 = 0;
const NUMBER: u16 = 1;

/// A residual under construction, each node given its kinds as an
/// evaluation gives them.
#[derive(Default)]
struct Build(Vec<Node>);

impl Build {
    fn push(&mut self, node: Node) -> u32 {
        self.0.push(node);
        u32::try_from(self.0.len() - 1).unwrap()
    }

    fn read(&mut self, id: i64) -> u32 {
        self.push(Node::Read {
            api: NUMBER,
            arg: Some(id),
        })
    }

    fn int(&mut self, n: i64) -> u32 {
        self.push(Node::Known(Val::Int(n)))
    }

    fn arith(&mut self, op: Bin, a: u32, b: u32) -> u32 {
        self.push(Node::Arith(op, a, b))
    }

    fn math(&mut self, f: MathOp, a: u32, b: u32) -> u32 {
        self.push(Node::Math(f, a, b))
    }

    /// The bounds `intervals` gives, its reads in `range`.
    fn intervals(&self, range: (i64, i64)) -> R<Vec<Iv>> {
        intervals(
            &Analysis::default(),
            &Reads { range },
            &Residual::of(&self.0),
        )
    }
}

const TWO_53: i64 = 1 << 53;

/// `1 // math.floor(wrap(r * r - 2^53 - 2^53))`, `r` a read in
/// [0, 2^31 - 2]. Its divisor's bounds, clamped, exclude 0 (each of
/// `wrap`'s results below is bounded at -2^53 or at 2^53 alone), but at
/// r = 2^27 the product is the double 2^54, the differences 2^53 and 0,
/// so the floor is 0: only the mark the product's bounds leave, carried
/// through `wrap`, refuses it.
fn divided(wrap: impl Fn(&mut Build, u32) -> u32) -> R<Vec<Iv>> {
    let mut b = Build::default();
    let r = b.read(312);
    let sq = b.arith(Bin::Mul, r, r);
    let big = b.int(TWO_53);
    let once = b.arith(Bin::Sub, sq, big);
    let twice = b.arith(Bin::Sub, once, big);
    let x = wrap(&mut b, twice);
    let floor = b.math(MathOp::Floor, x, x);
    let one = b.int(1);
    b.arith(Bin::IDiv, one, floor);
    b.intervals((0, i64::from(i32::MAX) - 1))
}

/// The bounds given, or a panic at a refusal.
fn bounds(r: R<Vec<Iv>>) -> Vec<Iv> {
    r.unwrap_or_else(|_| panic!("refused"))
}

/// What a case puts around the divisor's operand.
type Wrap = dyn Fn(&mut Build, u32) -> u32;

fn refused(r: &R<Vec<Iv>>) -> bool {
    matches!(
        r,
        Err(Stop::Unsupported(Unsupported::Construct(
            "an integer // whose divisor can be 0"
        )))
    )
}

#[test]
fn a_promoted_divisor_of_an_integer_floor_division_is_refused() {
    let cases: [(&str, &Wrap); 5] = [
        ("plain", &|_, x| x),
        ("negation", &|b, x| b.push(Node::Neg(x))),
        ("abs", &|b, x| b.math(MathOp::Abs, x, x)),
        ("max", &|b, x| {
            let low = b.int(-TWO_53);
            b.math(MathOp::Max, x, low)
        }),
        ("select", &|b, x| {
            let c = b.push(Node::Read {
                api: OPTION,
                arg: Some(271),
            });
            let low = b.int(-TWO_53);
            b.push(Node::Select {
                cond: c,
                then: x,
                other: low,
            })
        }),
    ];
    for (name, wrap) in cases {
        let r = divided(wrap);
        assert!(refused(&r), "{name}: {:?}", r.as_ref().ok());
    }
}

/// The operand of each case above, its bounds clamped to one end of
/// ±2^53, without 0: the refusal is the mark's.
#[test]
fn the_promoted_divisor_s_own_bounds_exclude_0() {
    let mut b = Build::default();
    let r = b.read(312);
    let sq = b.arith(Bin::Mul, r, r);
    let big = b.int(TWO_53);
    let once = b.arith(Bin::Sub, sq, big);
    let twice = b.arith(Bin::Sub, once, big);
    let neg = b.push(Node::Neg(twice));
    let abs = b.math(MathOp::Abs, twice, twice);
    let ivs = bounds(b.intervals((0, i64::from(i32::MAX) - 1)));
    let at = |n: u32| ivs[n as usize];
    let e = i128::from(TWO_53);
    assert_eq!(at(sq), Some((0, e)));
    assert_eq!(at(twice), Some((-e, -e)));
    assert_eq!(at(neg), Some((e, e)));
    assert_eq!(at(abs), Some((e, e)));
}

/// `x // math.floor(a + b)` over reads in [1, 100]: the sum of two
/// integers is of the integer kind and within its bounds, so the floor
/// passes them through and the divisor is proven nonzero.
#[test]
fn an_in_range_divisor_is_bounded() {
    let mut b = Build::default();
    let x = b.read(1);
    let a = b.read(2);
    let c = b.read(3);
    let sum = b.arith(Bin::Add, a, c);
    let floor = b.math(MathOp::Floor, sum, sum);
    let q = b.arith(Bin::IDiv, x, floor);
    let ivs = bounds(b.intervals((1, 100)));
    assert_eq!(ivs[floor as usize], Some((2, 200)));
    assert_eq!(ivs[q as usize], Some((0, 50)));
}
