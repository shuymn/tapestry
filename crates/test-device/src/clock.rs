//! Clock bounds (docs/test-device.md "Clock bounds", F4).
//!
//! **Model:** host time = a + b·(device time), assuming both clocks keep
//! constant rates over a run (the device's crystal and the host's counter,
//! over minutes). Every ping gives s_i ≤ a + b·d_i ≤ r_i: s_i when the host
//! sent it, d_i the device's µs in the pong, r_i when the host received the
//! pong. The (a, b) satisfying all of them, with b within [`RATE_PPM`] of 1,
//! is a convex polygon, computed exactly: each vertex is the intersection
//! of two of the constraint lines, held as i128 fractions. A device time
//! maps to the interval of a + b·t over the polygon's vertices. An empty
//! polygon means the model or an observation is wrong: no ping is dropped,
//! and the error names the pings whose removal alone would restore it.

/// How far either clock's rate may be from the other's, in ppm.
pub const RATE_PPM: i128 = 1000;

/// One round trip: the host's send and receive times (µs) around the
/// device time (µs) the pong carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
    pub sent: i64,
    pub device: u64,
    pub received: i64,
}

impl Ping {
    /// A ping from host counter ticks at `freq` Hz, rounded outwards to µs
    /// so the bounds stay conservative.
    #[must_use]
    pub fn from_ticks(sent: i64, device: u64, received: i64, freq: i64) -> Self {
        let us = |t: i64| i128::from(t) * 1_000_000;
        let f = i128::from(freq.max(1));
        let to_i64 = |v: i128| i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX });
        Self {
            sent: to_i64(us(sent).div_euclid(f)),
            device,
            received: to_i64(-((-us(received)).div_euclid(f))),
        }
    }
}

/// Why no bounds exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NoPings,
    /// Ping `index` is not later on the device than the one before it, or
    /// was received before it was sent.
    Order {
        index: usize,
    },
    /// No offset and rate fit every ping. `culprits` are the pings whose
    /// removal alone makes the rest consistent (empty when no single ping
    /// does).
    Inconsistent {
        culprits: Vec<usize>,
    },
}

/// α·a + β·b = γ, with a in host µs (relative to the first ping's send)
/// and b the rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    alpha: i128,
    beta: i128,
    gamma: i128,
}

/// The intersection of two lines: (a, b) = (num_a, num_b) / den, den > 0.
#[derive(Clone, Copy, Debug)]
struct Vertex {
    num_a: i128,
    num_b: i128,
    den: i128,
}

fn meet(l: Line, m: Line) -> Option<Vertex> {
    let det = l.alpha * m.beta - m.alpha * l.beta;
    if det == 0 {
        return None;
    }
    let s = det.signum();
    Some(Vertex {
        num_a: s * (l.gamma * m.beta - m.gamma * l.beta),
        num_b: s * (l.alpha * m.gamma - m.alpha * l.gamma),
        den: det.abs(),
    })
}

/// A half-plane: α·a + β·b ≥ γ, or ≤ γ when `!ge`.
#[derive(Clone, Copy, Debug)]
struct Half {
    line: Line,
    ge: bool,
}

impl Half {
    fn holds(self, v: Vertex) -> bool {
        let l = self.line;
        let lhs = l.alpha * v.num_a + l.beta * v.num_b - l.gamma * v.den;
        if self.ge { lhs >= 0 } else { lhs <= 0 }
    }
}

/// A convex polygon as its edges' lines in order; vertex k is edge k
/// meeting edge k+1.
#[derive(Clone, Debug)]
struct Polygon {
    edges: Vec<Line>,
}

impl Polygon {
    fn vertices(&self) -> Option<Vec<Vertex>> {
        let n = self.edges.len();
        (0..n)
            .map(|k| meet(*self.edges.get(k)?, *self.edges.get((k + 1) % n)?))
            .collect()
    }

    /// Cuts the polygon by `h`; `None` when nothing is left.
    fn clip(self, h: Half) -> Option<Self> {
        let vertices = self.vertices()?;
        let inside: Vec<bool> = vertices.iter().map(|&v| h.holds(v)).collect();
        if inside.iter().all(|&i| i) {
            return Some(self);
        }
        if !inside.iter().any(|&i| i) {
            return None;
        }
        let n = self.edges.len();
        let mut edges = Vec::with_capacity(n + 1);
        for k in 0..n {
            // Edge k runs from vertex k-1 to vertex k.
            let start = *inside.get((k + n - 1) % n)?;
            let end = *inside.get(k)?;
            if start || end {
                edges.push(*self.edges.get(k)?);
            }
            if start && !end {
                edges.push(h.line);
            }
        }
        Some(Self { edges })
    }
}

/// The feasible offsets and rates of a run's pings.
#[derive(Clone, Debug)]
pub struct Bounds {
    vertices: Vec<Vertex>,
    sent0: i64,
    device0: u64,
}

fn constraints(pings: &[Ping], skip: Option<usize>) -> Result<Option<Bounds>, Error> {
    let mut kept = pings.iter().enumerate().filter(|&(i, _)| Some(i) != skip);
    let Some((_, first)) = kept.next() else {
        return Err(Error::NoPings);
    };
    let (sent0, device0) = (first.sent, first.device);
    let rel = |p: &Ping| {
        (
            i128::from(p.sent) - i128::from(sent0),
            i128::from(p.device) - i128::from(device0),
            i128::from(p.received) - i128::from(sent0),
        )
    };
    let (_, _, r0) = rel(first);
    let scale = 1_000_000;
    let b_line = |ppm: i128| Line {
        alpha: 0,
        beta: scale,
        gamma: scale + ppm,
    };
    // With d = 0 the first ping bounds a alone: a rectangle.
    let mut poly = Some(Polygon {
        edges: vec![
            b_line(-RATE_PPM),
            Line {
                alpha: 1,
                beta: 0,
                gamma: r0,
            },
            b_line(RATE_PPM),
            Line {
                alpha: 1,
                beta: 0,
                gamma: 0,
            },
        ],
    });
    for (_, p) in kept {
        let (s, d, r) = rel(p);
        let line = |gamma| Line {
            alpha: 1,
            beta: d,
            gamma,
        };
        poly = poly
            .and_then(|q| {
                q.clip(Half {
                    line: line(s),
                    ge: true,
                })
            })
            .and_then(|q| {
                q.clip(Half {
                    line: line(r),
                    ge: false,
                })
            });
    }
    Ok(poly.and_then(|q| q.vertices()).map(|vertices| Bounds {
        vertices,
        sent0,
        device0,
    }))
}

impl Bounds {
    /// The bounds of `pings`, in the order sent.
    ///
    /// # Errors
    ///
    /// [`Error`].
    pub fn fit(pings: &[Ping]) -> Result<Self, Error> {
        if pings.is_empty() {
            return Err(Error::NoPings);
        }
        for (i, w) in pings.windows(2).enumerate() {
            if let [a, b] = w
                && b.device <= a.device
            {
                return Err(Error::Order { index: i + 1 });
            }
        }
        if let Some(i) = pings.iter().position(|p| p.received < p.sent) {
            return Err(Error::Order { index: i });
        }
        if let Some(b) = constraints(pings, None)? {
            return Ok(b);
        }
        let culprits = (0..pings.len())
            .filter(|&i| matches!(constraints(pings, Some(i)), Ok(Some(_))))
            .collect();
        Err(Error::Inconsistent { culprits })
    }

    /// The host times (µs) device time `t` may map to: the least and the
    /// greatest a + b·t over the feasible set, rounded outwards.
    #[must_use]
    pub fn map(&self, t: u64) -> (i64, i64) {
        let dt = i128::from(t) - i128::from(self.device0);
        // Each value as a fraction over its vertex's positive denominator.
        let values = self
            .vertices
            .iter()
            .map(|v| (v.num_a + v.num_b * dt, v.den));
        let less = |x: &(i128, i128), y: &(i128, i128)| (x.0 * y.1).cmp(&(y.0 * x.1));
        let lo = values
            .clone()
            .min_by(less)
            .map_or(0, |(n, d)| n.div_euclid(d));
        let hi = values
            .max_by(less)
            .map_or(0, |(n, d)| -((-n).div_euclid(d)));
        let to_i64 = |v: i128| {
            i64::try_from(v + i128::from(self.sent0)).unwrap_or(if v < 0 {
                i64::MIN
            } else {
                i64::MAX
            })
        };
        (to_i64(lo), to_i64(hi))
    }

    /// The mapped interval's width at device time `t`, in µs.
    #[must_use]
    pub fn width(&self, t: u64) -> i64 {
        let (lo, hi) = self.map(t);
        hi.saturating_sub(lo)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A run's true clocks: host = HOST0 + (device − DEVICE0)·(5000 + k)/5000.
    pub(crate) struct Truth {
        pub k: i64,
    }

    pub(crate) const HOST0: i64 = 1_000_000_000;
    pub(crate) const DEVICE0: u64 = 7_000_000;
    pub(crate) const STEP: u64 = 100_000;

    impl Truth {
        /// The host µs of device time `d`; exact for multiples of 5000 µs.
        pub(crate) fn host(&self, d: u64) -> i64 {
            let dt = i64::try_from(d - DEVICE0).unwrap();
            HOST0 + dt * (5000 + self.k) / 5000
        }

        /// `count` pings every 100 ms (1200 make 120 s), with the outward
        /// and return delays of ping i.
        pub(crate) fn pings(
            &self,
            count: usize,
            delays: impl Fn(usize) -> (i64, i64),
        ) -> Vec<Ping> {
            (0..count)
                .map(|i| {
                    let d = DEVICE0 + STEP * u64::try_from(i).unwrap();
                    let (out, back) = delays(i);
                    Ping {
                        sent: self.host(d) - out,
                        device: d,
                        received: self.host(d) + back,
                    }
                })
                .collect()
        }
    }

    /// 3 ms out and 0.1 ms back for the first half, then the reverse.
    fn step_delays(i: usize) -> (i64, i64) {
        if i < 600 { (3_000, 100) } else { (100, 3_000) }
    }

    /// Every mapped interval holds the true time. Inside the run its width
    /// stays near one round trip (3.1 ms); 10 s past the last ping the rate's
    /// freedom widens it.
    fn assert_contains_truth(bounds: &Bounds, truth: &Truth) {
        for (i, width) in [
            (0, 3_200),
            (1, 3_200),
            (599, 3_200),
            (600, 3_200),
            (1199, 3_200),
            (1300, 6_000),
        ] {
            // Between pings as well as at them.
            let t = DEVICE0 + STEP * i + 45_000;
            let (lo, hi) = bounds.map(t);
            let want = truth.host(t);
            assert!(lo <= want && want <= hi, "{t}: {want} not in [{lo}, {hi}]");
            assert!(hi - lo < width, "{t}: width {}", hi - lo);
        }
    }

    #[test]
    fn asymmetric_delays_and_rate_errors_keep_the_truth() {
        // ±200 ppm: (5000 ± 1)/5000.
        for k in [1, -1, 0] {
            let truth = Truth { k };
            let reversed = |i| {
                let (out, back) = step_delays(i);
                (back, out)
            };
            for pings in [truth.pings(1200, step_delays), truth.pings(1200, reversed)] {
                let bounds = Bounds::fit(&pings).unwrap();
                assert_contains_truth(&bounds, &truth);
            }
        }
    }

    /// The method this replaced: fit the rate to the round trips'
    /// midpoints by least squares, then intersect the offsets that rate
    /// allows. A step in the delay asymmetry biases the fitted rate, and
    /// the offsets then exclude the truth.
    #[test]
    fn a_fitted_rate_then_offset_excludes_the_truth_where_the_set_does_not() {
        let truth = Truth { k: 0 };
        let pings = truth.pings(1200, step_delays);
        // Exact least squares over i128: slope = Sxy / Sxx of midpoints.
        let n = i128::try_from(pings.len()).unwrap();
        let x = |p: &Ping| i128::from(p.device) - i128::from(DEVICE0);
        let y = |p: &Ping| i128::from(p.sent) + i128::from(p.received);
        let (sx, sy) = pings.iter().fold((0, 0), |(a, b), p| (a + x(p), b + y(p)));
        let sxy: i128 = pings
            .iter()
            .map(|p| (n * x(p) - sx) * (n * y(p) - sy))
            .sum();
        let sxx: i128 = pings.iter().map(|p| (n * x(p) - sx).pow(2)).sum();
        // b = sxy / (2·sxx), since y is twice the midpoint.
        let (bn, bd) = (sxy, 2 * sxx);
        assert!(
            bn * 1_000_000 > bd * 1_000_030,
            "the fit is biased above 30 ppm"
        );
        // The offsets at device time DEVICE0 that rate allows, in host µs.
        let off = |t: i64, p: &Ping| i128::from(t) * bd - bn * x(p);
        let lo = pings.iter().map(|p| off(p.sent, p)).max().unwrap();
        let hi = pings.iter().map(|p| off(p.received, p)).min().unwrap();
        let want = i128::from(truth.host(DEVICE0)) * bd;
        assert!(
            !(lo <= want && want <= hi),
            "the fitted method kept the truth"
        );
        let bounds = Bounds::fit(&pings).unwrap();
        let (l, h) = bounds.map(DEVICE0);
        assert!(l <= HOST0 && HOST0 <= h);
    }

    #[test]
    fn one_late_pong_makes_the_set_empty_and_is_named() {
        let truth = Truth { k: 1 };
        let mut pings = truth.pings(200, |_| (100, 100));
        pings[100].device += 5_000;
        assert_eq!(
            Bounds::fit(&pings).unwrap_err(),
            Error::Inconsistent {
                culprits: vec![100]
            }
        );
    }

    #[test]
    fn out_of_order_pings_are_refused() {
        let truth = Truth { k: 0 };
        let mut pings = truth.pings(10, |_| (100, 100));
        pings[3].device = pings[2].device;
        assert_eq!(Bounds::fit(&pings).unwrap_err(), Error::Order { index: 3 });
        assert_eq!(Bounds::fit(&[]).unwrap_err(), Error::NoPings);
    }

    #[test]
    fn ticks_round_outwards() {
        let p = Ping::from_ticks(15, 0, 15, 10_000_000);
        assert_eq!((p.sent, p.received), (1, 2));
        let n = Ping::from_ticks(-15, 0, -15, 10_000_000);
        assert_eq!((n.sent, n.received), (-2, -1));
    }
}
