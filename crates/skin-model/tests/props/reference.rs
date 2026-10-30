//! The reference interpreter: walks a generated tree in `i128`, without
//! folding, and records every node's value.
//!
//! Its floor division is written independently of the crate's: for `b > 0`
//! it is `a.div_euclid(b)`, for `b < 0` `(-a).div_euclid(-b)`, for `b == 0`
//! 0; modulo is `a - b * q`, with its own zero-divisor case. Values that
//! leave `i128` saturate; they are far outside `i32` either way.

use skin_model::TIMER_OFF;

use crate::strategy::{Arith, Cmp, E, InRange};

/// The option configuration a tree is evaluated under.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub cover: bool,
    pub style: usize,
}

/// What evaluation passed through, for the generator checks.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub zero_divisor: bool,
    pub negative_divisor: bool,
    pub off_timer: bool,
    pub cover: [bool; 2],
    /// A choice test that was true, and one that was false.
    pub choice: [bool; 2],
}

pub struct Reference<'a> {
    pub state: &'a InRange,
    pub options: Options,
    /// Every node's value, in evaluation order.
    pub values: Vec<i128>,
    pub seen: Seen,
}

fn floor_div(a: i128, b: i128) -> i128 {
    match b.signum() {
        1 => a.div_euclid(b),
        -1 => a.saturating_neg().div_euclid(b.saturating_neg()),
        _ => 0,
    }
}

fn floor_mod(a: i128, b: i128) -> i128 {
    if b == 0 {
        return 0;
    }
    a.saturating_sub(b.saturating_mul(floor_div(a, b)))
}

impl<'a> Reference<'a> {
    pub fn new(state: &'a InRange, options: Options) -> Self {
        Self {
            state,
            options,
            values: Vec::new(),
            seen: Seen::default(),
        }
    }

    /// Whether every recorded value lies in `i32`.
    pub fn all_in_i32(&self) -> bool {
        self.values
            .iter()
            .all(|&v| i128::from(i32::MIN) <= v && v <= i128::from(i32::MAX))
    }

    fn timer(&mut self, t: usize) -> i128 {
        let v = self.state.timers[t];
        if v == TIMER_OFF {
            self.seen.off_timer = true;
        }
        i128::from(v)
    }

    pub fn eval(&mut self, e: &E) -> i128 {
        let v = match e {
            E::Int(v) | E::Dur { ms: v, .. } => i128::from(*v),
            E::Bool(b) => i128::from(*b),
            E::Field(slot) => i128::from(self.state.fields[*slot]),
            E::Variant { index, .. } => i128::try_from(*index).unwrap(),
            E::Neg(a) => self.eval(a).saturating_neg(),
            E::Abs(a) => self.eval(a).saturating_abs(),
            E::Not(a) => i128::from(self.eval(a) == 0),
            E::Arith(op, a, b) => {
                let (x, y) = (self.eval(a), self.eval(b));
                match op {
                    Arith::Add => x.saturating_add(y),
                    Arith::Sub => x.saturating_sub(y),
                    Arith::Mul => x.saturating_mul(y),
                    Arith::Div | Arith::Rem => {
                        self.seen.zero_divisor |= y == 0;
                        self.seen.negative_divisor |= y < 0;
                        if *op == Arith::Div {
                            floor_div(x, y)
                        } else {
                            floor_mod(x, y)
                        }
                    }
                    Arith::Min => x.min(y),
                    Arith::Max => x.max(y),
                }
            }
            E::Clamp(x, l, h) => {
                let (x, l, h) = (self.eval(x), self.eval(l), self.eval(h));
                x.max(l).min(h)
            }
            E::Cmp(op, a, b) => {
                let (x, y) = (self.eval(a), self.eval(b));
                i128::from(match op {
                    Cmp::Lt => x < y,
                    Cmp::Le => x <= y,
                    Cmp::Gt => x > y,
                    Cmp::Ge => x >= y,
                    Cmp::Eq => x == y,
                    Cmp::Ne => x != y,
                })
            }
            E::And(a, b) => {
                let (x, y) = (self.eval(a), self.eval(b));
                i128::from(x != 0 && y != 0)
            }
            E::Or(a, b) => {
                let (x, y) = (self.eval(a), self.eval(b));
                i128::from(x != 0 || y != 0)
            }
            E::If(c, a, b) => {
                let (c, a, b) = (self.eval(c), self.eval(a), self.eval(b));
                if c != 0 { a } else { b }
            }
            E::On(t) => i128::from(self.timer(*t) >= 0),
            E::Elapsed(t) => self.timer(*t),
            E::Toggle => {
                self.seen.cover[usize::from(self.options.cover)] = true;
                i128::from(self.options.cover)
            }
            E::Choice(k) => {
                let hit = *k == self.options.style;
                self.seen.choice[usize::from(hit)] = true;
                i128::from(hit)
            }
        };
        self.values.push(v);
        v
    }
}
