//! Generators: well-typed expression trees over the test schema, printed
//! to source, and states to evaluate them in.
//!
//! A tree is built from a list of raw choices, so shrinking the list
//! shrinks the tree and every case stays well typed. An exhausted list
//! picks leaves.

use std::fmt::Write as _;

use skin_model::{MAX_ELAPSED_MS, MAX_SOURCE_BYTES, TIMER_OFF};

use crate::common::{FIELDS, GRADES, KINDS, STYLES};

/// Field slots of the test schema.
pub const FULL: usize = 0;
pub const SMALL: usize = 1;
pub const MS: usize = 2;
pub const ZERO: usize = 3;
pub const FLAG: usize = 4;
pub const GRADE: usize = 5;
pub const GAUGE: usize = 6;
pub const TIMER_NAMES: [&str; 2] = ["play", "judge.great"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Min,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

/// A well-typed expression.
#[derive(Clone, Debug)]
pub enum E {
    Int(i32),
    /// A duration in ms, printed in seconds when `secs`.
    Dur {
        ms: i32,
        secs: bool,
    },
    Bool(bool),
    Field(usize),
    /// Variant `index` of enum `e`, valid only as an operand of `==`/`!=`.
    Variant {
        e: usize,
        index: usize,
    },
    Neg(Box<E>),
    Abs(Box<E>),
    Not(Box<E>),
    Arith(Arith, Box<E>, Box<E>),
    Clamp(Box<E>, Box<E>, Box<E>),
    Cmp(Cmp, Box<E>, Box<E>),
    And(Box<E>, Box<E>),
    Or(Box<E>, Box<E>),
    If(Box<E>, Box<E>, Box<E>),
    On(usize),
    Elapsed(usize),
    Toggle,
    Choice(usize),
}

/// The root type of a generated tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    Int,
    Bool,
    Enum(usize),
}

/// Raw choices: the root type, then the tree's choices.
#[derive(Clone, Debug)]
pub struct RawTree {
    pub root: u8,
    pub choices: Vec<u32>,
}

/// The deepest generated tree.
const MAX_DEPTH: u32 = 8;
/// The most nodes per tree.
pub const MAX_NODES: u32 = 40;
/// The most bytes one node prints besides its children:
/// `option(style, classic)`.
const MAX_NODE_BYTES: usize = 22;
// Every generated tree's source fits.
const _: () = assert!(MAX_NODES as usize * MAX_NODE_BYTES <= MAX_SOURCE_BYTES);
/// In the bounded subset: integer leaves lie in `[-10, 10]` and at most
/// three `+ - *` nest on any path, so every value is at most `10^8` in
/// magnitude and every tree compiles.
const BOUNDED_GROWTH: u32 = 3;

struct Builder<'a> {
    choices: &'a [u32],
    pos: usize,
    /// Nodes built or reserved for pending children: a node reserves its
    /// (at most 3) children before building the first, so the tree never
    /// passes `MAX_NODES`.
    reserved: u32,
    bounded: bool,
}

impl Builder<'_> {
    fn next(&mut self) -> u32 {
        let v = self.choices.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        v
    }

    /// Whether this (already reserved) node must be a leaf; otherwise
    /// reserves its children.
    fn leaf(&mut self, depth: u32) -> bool {
        let leaf =
            depth >= MAX_DEPTH || self.reserved + 3 > MAX_NODES || self.pos >= self.choices.len();
        if !leaf {
            self.reserved += 3;
        }
        leaf
    }

    fn int(&mut self, depth: u32, growth: u32) -> E {
        let leaf = self.leaf(depth);
        let c = self.next();
        if leaf || c.is_multiple_of(3) {
            return self.int_leaf(c / 3);
        }
        let d = depth + 1;
        let b = Box::new;
        match (c / 3) % 13 {
            0 => E::Neg(b(self.int(d, growth))),
            1 => E::Abs(b(self.int(d, growth))),
            n @ 2..=4 if !self.bounded || growth > 0 => {
                let op = [Arith::Add, Arith::Sub, Arith::Mul][usize::try_from(n - 2).unwrap()];
                let g = growth.saturating_sub(1);
                E::Arith(op, b(self.int(d, g)), b(self.int(d, g)))
            }
            5 | 6 => {
                let op = if (c / 3) % 13 == 5 {
                    Arith::Div
                } else {
                    Arith::Rem
                };
                E::Arith(op, b(self.int(d, growth)), b(self.int(d, growth)))
            }
            7 => E::Arith(Arith::Min, b(self.int(d, growth)), b(self.int(d, growth))),
            8 => E::Arith(Arith::Max, b(self.int(d, growth)), b(self.int(d, growth))),
            9 => E::Clamp(
                b(self.int(d, growth)),
                b(self.int(d, growth)),
                b(self.int(d, growth)),
            ),
            10 | 11 => E::If(
                b(self.boolean(d, growth)),
                b(self.int(d, growth)),
                b(self.int(d, growth)),
            ),
            _ => self.int_leaf(c / 39),
        }
    }

    fn int_leaf(&mut self, c: u32) -> E {
        let v = self.next();
        if self.bounded {
            return match c % 4 {
                0 => E::Field(SMALL),
                1 => E::Field(ZERO),
                2 => E::Dur {
                    ms: i32::try_from(v % 11).unwrap(),
                    secs: false,
                },
                _ => E::Int(i32::try_from(v % 21).unwrap() - 10),
            };
        }
        match c % 7 {
            0 => E::Field([FULL, SMALL, MS, ZERO][usize::try_from(v % 4).unwrap()]),
            1 => E::Elapsed(usize::try_from(v % 2).unwrap()),
            2 => E::Dur {
                ms: i32::try_from(v % 100_000).unwrap(),
                secs: v.is_multiple_of(2),
            },
            3 => E::Field(SMALL),
            // Literals near the corners, and anywhere.
            4 => {
                E::Int([0, 1, -1, 2, -2, i32::MAX, i32::MIN, 1000][usize::try_from(v % 8).unwrap()])
            }
            5 => E::Int(i32::try_from(v % 201).unwrap() - 100),
            _ => E::Int(v.cast_signed()),
        }
    }

    fn boolean(&mut self, depth: u32, growth: u32) -> E {
        let leaf = self.leaf(depth);
        let c = self.next();
        if leaf || c.is_multiple_of(3) {
            let v = self.next();
            return match (c / 3) % 6 {
                0 => E::Bool(v.is_multiple_of(2)),
                1 | 2 => E::Field(FLAG),
                3 => E::On(usize::try_from(v % 2).unwrap()),
                4 => E::Toggle,
                _ => E::Choice(usize::try_from(v % 3).unwrap()),
            };
        }
        let d = depth + 1;
        let b = Box::new;
        match (c / 3) % 8 {
            0 => E::Not(b(self.boolean(d, growth))),
            1 => E::And(b(self.boolean(d, growth)), b(self.boolean(d, growth))),
            2 => E::Or(b(self.boolean(d, growth)), b(self.boolean(d, growth))),
            3 | 4 => {
                let op = [Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge, Cmp::Eq, Cmp::Ne]
                    [usize::try_from(self.next() % 6).unwrap()];
                E::Cmp(op, b(self.int(d, growth)), b(self.int(d, growth)))
            }
            5 => {
                let op = if self.next().is_multiple_of(2) {
                    Cmp::Eq
                } else {
                    Cmp::Ne
                };
                E::Cmp(op, b(self.boolean(d, growth)), b(self.boolean(d, growth)))
            }
            6 => {
                let v = self.next();
                let e = usize::try_from(v % 2).unwrap();
                let op = if v % 4 < 2 { Cmp::Eq } else { Cmp::Ne };
                let enum_side = self.enumeration(e, d, growth);
                let other = if self.next().is_multiple_of(2) {
                    let n = [GRADES.len(), KINDS.len()][e];
                    E::Variant {
                        e,
                        index: usize::try_from(self.next()).unwrap() % n,
                    }
                } else {
                    self.enumeration(e, d, growth)
                };
                if v % 8 < 4 {
                    E::Cmp(op, b(enum_side), b(other))
                } else {
                    E::Cmp(op, b(other), b(enum_side))
                }
            }
            _ => E::If(
                b(self.boolean(d, growth)),
                b(self.boolean(d, growth)),
                b(self.boolean(d, growth)),
            ),
        }
    }

    fn enumeration(&mut self, e: usize, depth: u32, growth: u32) -> E {
        let leaf = self.leaf(depth);
        let c = self.next();
        let field = E::Field([GRADE, GAUGE][e]);
        if leaf || !c.is_multiple_of(3) {
            return field;
        }
        let d = depth + 1;
        E::If(
            Box::new(self.boolean(d, growth)),
            Box::new(self.enumeration(e, d, growth)),
            Box::new(self.enumeration(e, d, growth)),
        )
    }
}

/// Builds a tree from raw choices; `bounded` selects the subset that always
/// compiles.
pub fn build(raw: &RawTree, bounded: bool) -> (E, Root) {
    let mut b = Builder {
        choices: &raw.choices,
        pos: 0,
        reserved: 1,
        bounded,
    };
    let growth = BOUNDED_GROWTH;
    match raw.root % 4 {
        0 | 1 => (b.int(0, growth), Root::Int),
        2 => (b.boolean(0, growth), Root::Bool),
        _ => {
            let e = usize::from(raw.root / 4 % 2);
            (b.enumeration(e, 0, growth), Root::Enum(e))
        }
    }
}

/// Prints `e` as source, parenthesizing every compound node.
pub fn print(e: &E) -> String {
    let mut out = String::new();
    write(e, &mut out);
    out
}

fn write(e: &E, out: &mut String) {
    let bin = |out: &mut String, a: &E, op: &str, b: &E| {
        out.push('(');
        write(a, out);
        let _ = write!(out, " {op} ");
        write(b, out);
        out.push(')');
    };
    let call = |out: &mut String, name: &str, args: &[&E]| {
        out.push_str(name);
        out.push('(');
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write(a, out);
        }
        out.push(')');
    };
    match e {
        E::Int(v) if *v < 0 => {
            let _ = write!(out, "(-{})", v.unsigned_abs());
        }
        E::Int(v) => {
            let _ = write!(out, "{v}");
        }
        E::Dur { ms, secs: true } => {
            let _ = write!(out, "{}.{:03}s", ms / 1000, ms % 1000);
        }
        E::Dur { ms, secs: false } => {
            let _ = write!(out, "{ms}ms");
        }
        E::Bool(v) => {
            let _ = write!(out, "{v}");
        }
        E::Field(slot) => out.push_str(FIELDS[*slot].name),
        E::Variant { e, index } => out.push_str([GRADES, KINDS][*e][*index]),
        E::Neg(a) => {
            out.push_str("(-");
            write(a, out);
            out.push(')');
        }
        E::Not(a) => {
            out.push_str("(!");
            write(a, out);
            out.push(')');
        }
        E::Abs(a) => call(out, "abs", &[a]),
        E::Arith(op, a, b) => match op {
            Arith::Add => bin(out, a, "+", b),
            Arith::Sub => bin(out, a, "-", b),
            Arith::Mul => bin(out, a, "*", b),
            Arith::Div => bin(out, a, "/", b),
            Arith::Rem => bin(out, a, "%", b),
            Arith::Min => call(out, "min", &[a, b]),
            Arith::Max => call(out, "max", &[a, b]),
        },
        E::Clamp(x, l, h) => call(out, "clamp", &[x, l, h]),
        E::Cmp(op, a, b) => {
            let op = match op {
                Cmp::Lt => "<",
                Cmp::Le => "<=",
                Cmp::Gt => ">",
                Cmp::Ge => ">=",
                Cmp::Eq => "==",
                Cmp::Ne => "!=",
            };
            bin(out, a, op, b);
        }
        E::And(a, b) => bin(out, a, "&&", b),
        E::Or(a, b) => bin(out, a, "||", b),
        E::If(c, a, b) => {
            out.push_str("(if ");
            write(c, out);
            out.push_str(" then ");
            write(a, out);
            out.push_str(" else ");
            write(b, out);
            out.push(')');
        }
        E::On(t) => {
            let _ = write!(out, "on({})", TIMER_NAMES[*t]);
        }
        E::Elapsed(t) => {
            let _ = write!(out, "elapsed({})", TIMER_NAMES[*t]);
        }
        E::Toggle => out.push_str("option(cover)"),
        E::Choice(k) => {
            let _ = write!(out, "option(style, {})", STYLES[*k]);
        }
    }
}

/// Raw state values: each maps to a value inside its slot's range, biased
/// toward the ends and `{0, ±1}`, where interval extremes sit, and
/// otherwise uniform over the range.
#[derive(Clone, Debug)]
pub struct RawState {
    pub fields: [u32; 7],
    pub timers: [u32; 2],
}

/// An in-range state: field values by slot and timer readings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InRange {
    pub fields: [i32; 7],
    pub timers: [i32; 2],
}

fn pick(raw: u32, lo: i32, hi: i32) -> i32 {
    let v = match raw % 8 {
        0 => i64::from(lo),
        1 => i64::from(hi),
        2 => 0,
        3 => 1,
        4 => -1,
        // Uniform over the whole range: scale the raw value into it.
        _ => {
            let size = i128::from(hi) - i128::from(lo) + 1;
            let offset = (i128::from(raw >> 3) * size) >> 29;
            i64::try_from(i128::from(lo) + offset).unwrap()
        }
    };
    i32::try_from(v.clamp(i64::from(lo), i64::from(hi))).unwrap()
}

pub fn build_state(raw: &RawState) -> InRange {
    let mut fields = [0; 7];
    for (slot, f) in fields.iter_mut().enumerate() {
        let (lo, hi) = range(slot);
        *f = pick(raw.fields[slot], lo, hi);
    }
    let timers = raw.timers.map(|t| match t % 4 {
        0 => TIMER_OFF,
        1 => 0,
        2 => MAX_ELAPSED_MS,
        _ => {
            i32::try_from(u64::from(t / 4) % (u64::try_from(MAX_ELAPSED_MS).unwrap() + 1)).unwrap()
        }
    });
    InRange { fields, timers }
}

/// The declared range of field `slot`'s encoding.
pub fn range(slot: usize) -> (i32, i32) {
    match slot {
        FULL => (i32::MIN, i32::MAX),
        SMALL => (-10, 10),
        MS => (0, 1000),
        ZERO => (0, 0),
        FLAG => (0, 1),
        GRADE => (0, i32::try_from(GRADES.len()).unwrap() - 1),
        _ => (0, i32::try_from(KINDS.len()).unwrap() - 1),
    }
}

/// The in-range state the evaluator must see for arbitrary slices: each
/// field clamped into its range (the low end when missing), each timer off
/// when negative or missing and at most `MAX_ELAPSED_MS`.
pub fn clamp_state(fields: &[i32], timers: &[i32]) -> InRange {
    let mut out = InRange {
        fields: [0; 7],
        timers: [TIMER_OFF; 2],
    };
    for (slot, f) in out.fields.iter_mut().enumerate() {
        let (lo, hi) = range(slot);
        *f = fields.get(slot).map_or(lo, |&v| v.clamp(lo, hi));
    }
    for (slot, t) in out.timers.iter_mut().enumerate() {
        *t = match timers.get(slot) {
            Some(&v) if v >= 0 => v.min(MAX_ELAPSED_MS),
            _ => TIMER_OFF,
        };
    }
    out
}
