//! The timing trace (docs/test-device.md "Timing trace"): per endpoint, when
//! each report was handed to the USB controller and when the host took it,
//! as numbered records over the control link.

use core::fmt;

/// An HID endpoint of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ep {
    Keyboard,
    Pad,
}

impl Ep {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Keyboard => "k",
            Self::Pad => "p",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "k" => Some(Self::Keyboard),
            "p" => Some(Self::Pad),
            _ => None,
        }
    }
}

/// Report k's handoff and completion, both on the device's µs timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    pub seq: u32,
    pub handoff: u64,
    pub complete: u64,
}

/// One endpoint's association of handoffs with completions.
///
/// Whether a report is pending is its own state: "a previous report
/// exists" is never read off a nonzero sequence number, which would skip
/// the completion at every wrap.
#[derive(Clone, Copy, Debug, Default)]
pub struct Association {
    pending: Option<(u32, u64)>,
    next: u32,
}

impl Association {
    /// An association whose first report is numbered `first`.
    #[must_use]
    pub const fn starting_at(first: u32) -> Self {
        Self {
            pending: None,
            next: first,
        }
    }

    /// Whether a handed report has not been seen complete yet.
    #[must_use]
    pub const fn pending(&self) -> Option<u32> {
        match self.pending {
            Some((seq, _)) => Some(seq),
            None => None,
        }
    }

    /// The pending report completed at `at`.
    pub const fn completed(&mut self, at: u64) -> Option<Done> {
        match self.pending.take() {
            Some((seq, handoff)) => Some(Done {
                seq,
                handoff,
                complete: at,
            }),
            None => None,
        }
    }

    /// The pending report was revoked before the host took it.
    pub fn revoked(&mut self) -> Option<u32> {
        self.pending.take().map(|(seq, _)| seq)
    }

    /// The number the next report gets.
    #[must_use]
    pub const fn next_seq(&self) -> u32 {
        self.next
    }

    /// The next report was handed at `at`; returns its number and the
    /// report that was still pending, if any (a report handed over a
    /// pending one is a firmware fault).
    pub const fn handed(&mut self, at: u64) -> (u32, Option<(u32, u64)>) {
        let seq = self.next;
        let old = self.pending.replace((seq, at));
        self.next = seq.wrapping_add(1);
        (seq, old)
    }
}

/// A trace record's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
    /// `T`: report `seq` was fixed at `ready` and handed at `handoff`.
    Handoff {
        ep: Ep,
        seq: u32,
        ready: u64,
        handoff: u64,
    },
    /// `C`: the host took report `seq`; the firmware saw it at `at`.
    Completion { ep: Ep, seq: u32, at: u64 },
    /// `X`: report `seq` was revoked at `at`; the buffer control read at
    /// `EP_ABORT` had AVAILABLE `available` and LENGTH `len`.
    Revoked {
        ep: Ep,
        seq: u32,
        available: bool,
        len: u16,
        at: u64,
    },
    /// `L`: `count` records were dropped because the ring was full.
    Lost { count: u32 },
    /// `P`: report `seq` was still pending when `flush` gave up.
    Pending { ep: Ep, seq: u32 },
    /// `F`: the end of a `flush`.
    Flushed,
}

/// A numbered trace record. Numbers are consecutive, so a gap is a loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub n: u32,
    pub body: Body,
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.n;
        match self.body {
            Body::Handoff {
                ep,
                seq,
                ready,
                handoff,
            } => {
                write!(f, "T {n} {} {seq} R {ready} H {handoff}", ep.name())
            }
            Body::Completion { ep, seq, at } => write!(f, "C {n} {} {seq} {at}", ep.name()),
            Body::Revoked {
                ep,
                seq,
                available,
                len,
                at,
            } => {
                write!(
                    f,
                    "X {n} {} {seq} {} {len} {at}",
                    ep.name(),
                    u8::from(available)
                )
            }
            Body::Lost { count } => write!(f, "L {n} lost {count}"),
            Body::Pending { ep, seq } => write!(f, "P {n} {} {seq}", ep.name()),
            Body::Flushed => write!(f, "F {n}"),
        }
    }
}

impl Record {
    /// The record a line holds (without its line ending).
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut w = line.split_ascii_whitespace();
        let kind = w.next()?;
        let n = w.next()?.parse().ok()?;
        let body = match kind {
            "T" => {
                let ep = Ep::parse(w.next()?)?;
                let seq = w.next()?.parse().ok()?;
                (w.next()? == "R").then_some(())?;
                let ready = w.next()?.parse().ok()?;
                (w.next()? == "H").then_some(())?;
                let handoff = w.next()?.parse().ok()?;
                Body::Handoff {
                    ep,
                    seq,
                    ready,
                    handoff,
                }
            }
            "C" => Body::Completion {
                ep: Ep::parse(w.next()?)?,
                seq: w.next()?.parse().ok()?,
                at: w.next()?.parse().ok()?,
            },
            "X" => Body::Revoked {
                ep: Ep::parse(w.next()?)?,
                seq: w.next()?.parse().ok()?,
                available: match w.next()? {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                },
                len: w.next()?.parse().ok()?,
                at: w.next()?.parse().ok()?,
            },
            "L" => {
                (w.next()? == "lost").then_some(())?;
                Body::Lost {
                    count: w.next()?.parse().ok()?,
                }
            }
            "P" => Body::Pending {
                ep: Ep::parse(w.next()?)?,
                seq: w.next()?.parse().ok()?,
            },
            "F" => Body::Flushed,
            _ => return None,
        };
        w.next().is_none().then_some(Self { n, body })
    }
}

/// The firmware's trace buffer: `N` records, numbered as they are made.
/// When it is full a record is dropped, but its number is still used, and
/// the next record that fits is preceded by an `L` record counting the
/// drops.
#[derive(Debug)]
pub struct Ring<const N: usize> {
    buf: [Record; N],
    head: usize,
    len: usize,
    next_n: u32,
    lost: u32,
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Ring<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [Record {
                n: 0,
                body: Body::Flushed,
            }; N],
            head: 0,
            len: 0,
            next_n: 0,
            lost: 0,
        }
    }

    fn put(&mut self, body: Body) -> bool {
        let n = self.next_n;
        self.next_n = n.wrapping_add(1);
        if self.len == N {
            self.lost = self.lost.saturating_add(1);
            return false;
        }
        if let Some(slot) = self.buf.get_mut((self.head + self.len) % N) {
            *slot = Record { n, body };
            self.len += 1;
        }
        true
    }

    /// Numbers `body` and buffers it, or drops it when the ring is full.
    pub fn push(&mut self, body: Body) {
        if self.lost > 0 && self.len < N {
            let count = core::mem::take(&mut self.lost);
            self.put(Body::Lost { count });
        }
        self.put(body);
    }

    /// The oldest buffered record.
    pub fn pop(&mut self) -> Option<Record> {
        if self.len == 0 {
            return None;
        }
        let r = self.buf.get(self.head).copied();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        r
    }

    /// Whether no record waits.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[cfg(feature = "std")]
pub use check::{Check, Entry, Problem, Trace};

#[cfg(feature = "std")]
mod check {
    use std::collections::BTreeMap;

    use super::{Body, Ep, Record};
    use crate::report::PadReport;

    /// One report as the trace shows it.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Entry {
        pub ready: u64,
        pub handoff: u64,
        pub completion: Option<u64>,
        /// The `X` record's AVAILABLE and LENGTH, when it was revoked.
        pub revoked: Option<(bool, u16)>,
    }

    /// Why a trace cannot be used: any of these fails the row.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Problem {
        /// Record numbers jumped: records were lost.
        Gap { expected: u32, got: u32 },
        /// The firmware dropped `count` records.
        Lost { n: u32, count: u32 },
        /// `flush` ended with this report still pending.
        Pending { ep: Ep, seq: u32 },
        /// The trace does not end with `F`.
        NoFlush,
        /// A completion or revocation of a report never handed.
        Unhanded { ep: Ep, seq: u32 },
        /// A report handed but neither taken nor revoked by the flush.
        Unresolved { ep: Ep, seq: u32 },
        /// A report handed twice.
        Repeated { ep: Ep, seq: u32 },
    }

    /// A complete trace: every report of each endpoint by number.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Trace {
        pub reports: BTreeMap<(Ep, u32), Entry>,
    }

    /// Reads records in order and checks them (docs/test-device.md "Timing
    /// trace"): consecutive numbers, no `L`, a terminal `F` with no `P`,
    /// and every handed report taken or revoked.
    #[derive(Debug, Default)]
    pub struct Check {
        next_n: Option<u32>,
        trace: Trace,
        problems: Vec<Problem>,
        flushed: bool,
    }

    impl Check {
        pub fn feed(&mut self, r: Record) {
            if let Some(expected) = self.next_n
                && r.n != expected
            {
                self.problems.push(Problem::Gap { expected, got: r.n });
            }
            self.next_n = Some(r.n.wrapping_add(1));
            self.flushed = false;
            let reports = &mut self.trace.reports;
            match r.body {
                Body::Handoff {
                    ep,
                    seq,
                    ready,
                    handoff,
                } => {
                    let new = Entry {
                        ready,
                        handoff,
                        ..Entry::default()
                    };
                    if reports.insert((ep, seq), new).is_some() {
                        self.problems.push(Problem::Repeated { ep, seq });
                    }
                }
                Body::Completion { ep, seq, at } => match reports.get_mut(&(ep, seq)) {
                    Some(e) => e.completion = Some(at),
                    None => self.problems.push(Problem::Unhanded { ep, seq }),
                },
                Body::Revoked {
                    ep,
                    seq,
                    available,
                    len,
                    ..
                } => match reports.get_mut(&(ep, seq)) {
                    Some(e) => e.revoked = Some((available, len)),
                    None => self.problems.push(Problem::Unhanded { ep, seq }),
                },
                Body::Lost { count } => self.problems.push(Problem::Lost { n: r.n, count }),
                Body::Pending { ep, seq } => self.problems.push(Problem::Pending { ep, seq }),
                Body::Flushed => self.flushed = true,
            }
        }

        /// The trace, or every problem found.
        ///
        /// # Errors
        ///
        /// The problems, in the order found.
        pub fn finish(mut self) -> Result<Trace, Vec<Problem>> {
            if !self.flushed {
                self.problems.push(Problem::NoFlush);
            }
            for (&(ep, seq), e) in &self.trace.reports {
                let pending = self.problems.contains(&Problem::Pending { ep, seq });
                if e.completion.is_none() && e.revoked.is_none() && !pending {
                    self.problems.push(Problem::Unresolved { ep, seq });
                }
            }
            if self.problems.is_empty() {
                Ok(self.trace)
            } else {
                Err(self.problems)
            }
        }
    }

    impl Trace {
        /// Checks a received pad report's own fields against the trace:
        /// its sequence number and R(k), and H(k−1) and C(k−1) when it
        /// carries them.
        ///
        /// # Errors
        ///
        /// Which field disagrees.
        pub fn check_pad_report(&self, seq: u32, report: &PadReport) -> Result<(), String> {
            let low16 = |v: u32| v & 0xFFFF;
            let low32 = |v: u64| v & 0xFFFF_FFFF;
            let this = self
                .reports
                .get(&(Ep::Pad, seq))
                .ok_or_else(|| format!("report {seq} is not in the trace"))?;
            if u32::from(report.seq) != low16(seq) {
                return Err(format!(
                    "report {seq}: sequence {} in the report",
                    report.seq
                ));
            }
            if u64::from(report.ready) != low32(this.ready) {
                return Err(format!(
                    "report {seq}: R {} vs trace {}",
                    report.ready, this.ready
                ));
            }
            let before = seq
                .checked_sub(1)
                .and_then(|s| self.reports.get(&(Ep::Pad, s)));
            match (report.previous, before) {
                (None, None) => Ok(()),
                (Some((h, c)), Some(prev)) => {
                    let ok = u64::from(h) == low32(prev.handoff)
                        && prev.completion.is_some_and(|pc| u64::from(c) == low32(pc));
                    if ok {
                        Ok(())
                    } else {
                        Err(format!("report {seq}: H/C {h}/{c} vs trace {prev:?}"))
                    }
                }
                (carried, traced) => Err(format!(
                    "report {seq}: previous report {} in the report, {} in the trace",
                    if carried.is_some() {
                        "present"
                    } else {
                        "absent"
                    },
                    if traced.is_some() {
                        "present"
                    } else {
                        "absent"
                    },
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands and completes reports the way the firmware's writer does,
    /// starting at `first`, and returns what each completion reported.
    fn run(first: u32, reports: u32) -> Vec<Done> {
        let mut a = Association::starting_at(first);
        let mut done = Vec::new();
        let mut t = 1_000;
        for _ in 0..reports {
            if a.pending().is_some() {
                done.extend(a.completed(t));
            }
            let (_, old) = a.handed(t + 10);
            assert_eq!(old, None, "handed over a pending report");
            t += 1_000;
        }
        done.extend(a.completed(t));
        done
    }

    #[test]
    fn completions_follow_their_reports_across_the_wrap() {
        let done = run(u32::MAX - 2, 6);
        let seqs: Vec<u32> = done.iter().map(|d| d.seq).collect();
        assert_eq!(seqs, [u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2]);
        assert!(done.iter().all(|d| d.complete > d.handoff));
    }

    #[test]
    fn the_first_report_has_no_predecessor() {
        let mut a = Association::default();
        assert_eq!(a.pending(), None);
        assert_eq!(a.completed(5), None);
        assert_eq!(a.handed(6), (0, None));
        assert_eq!(
            a.completed(9),
            Some(Done {
                seq: 0,
                handoff: 6,
                complete: 9
            })
        );
    }

    #[test]
    fn report_zero_after_the_wrap_completes_the_last_before_it() {
        let mut a = Association::starting_at(u32::MAX);
        assert_eq!(a.handed(1).0, u32::MAX);
        assert_eq!(a.completed(2).map(|d| d.seq), Some(u32::MAX));
        assert_eq!(a.handed(3).0, 0);
        assert_eq!(a.completed(4).map(|d| d.seq), Some(0));
    }

    #[test]
    fn a_revoked_report_leaves_nothing_pending() {
        let mut a = Association::default();
        a.handed(1);
        assert_eq!(a.revoked(), Some(0));
        assert_eq!(a.completed(2), None);
        assert_eq!(a.handed(3).0, 1);
    }

    #[test]
    fn records_round_trip_through_their_lines() {
        let bodies = [
            Body::Handoff {
                ep: Ep::Pad,
                seq: 7,
                ready: 10,
                handoff: u64::MAX,
            },
            Body::Completion {
                ep: Ep::Keyboard,
                seq: u32::MAX,
                at: 99,
            },
            Body::Revoked {
                ep: Ep::Pad,
                seq: 3,
                available: true,
                len: 18,
                at: 5,
            },
            Body::Lost { count: 4 },
            Body::Pending {
                ep: Ep::Keyboard,
                seq: 1,
            },
            Body::Flushed,
        ];
        for (n, body) in (0..).zip(bodies) {
            let r = Record { n, body };
            assert_eq!(Record::parse(&r.to_string()), Some(r), "{r}");
        }
        assert_eq!(Record::parse("C 1 p 2 3 4"), None, "trailing field");
        assert_eq!(Record::parse("T 1 q 2 R 3 H 4"), None, "unknown endpoint");
    }

    #[test]
    fn a_full_ring_numbers_its_drops_and_counts_them() {
        let mut ring = Ring::<2>::new();
        for seq in 0..4 {
            ring.push(Body::Pending { ep: Ep::Pad, seq });
        }
        let mut check = Check::default();
        let mut got = Vec::new();
        while let Some(r) = ring.pop() {
            got.push(r);
        }
        ring.push(Body::Flushed);
        while let Some(r) = ring.pop() {
            got.push(r);
        }
        let ns: Vec<u32> = got.iter().map(|r| r.n).collect();
        // 0 and 1 kept, 2 and 3 dropped, then L as 4 and F as 5.
        assert_eq!(ns, [0, 1, 4, 5]);
        assert_eq!(got[2].body, Body::Lost { count: 2 });
        for r in got {
            check.feed(r);
        }
        let problems = check.finish().unwrap_err();
        assert!(problems.contains(&Problem::Gap {
            expected: 2,
            got: 4
        }));
        assert!(problems.contains(&Problem::Lost { n: 4, count: 2 }));
    }

    fn records(bodies: &[Body]) -> Check {
        let mut check = Check::default();
        for (n, &body) in (0..).zip(bodies) {
            check.feed(Record { n, body });
        }
        check
    }

    #[test]
    fn a_trace_needs_its_flush_and_every_report_resolved() {
        let t = |seq, h| Body::Handoff {
            ep: Ep::Keyboard,
            seq,
            ready: h,
            handoff: h,
        };
        let c = |seq, at| Body::Completion {
            ep: Ep::Keyboard,
            seq,
            at,
        };
        let ok = records(&[t(0, 1), c(0, 2), t(1, 3), c(1, 4), Body::Flushed])
            .finish()
            .unwrap();
        assert_eq!(ok.reports.len(), 2);
        assert_eq!(
            records(&[t(0, 1), c(0, 2)]).finish().unwrap_err(),
            [Problem::NoFlush]
        );
        assert_eq!(
            records(&[t(0, 1), Body::Flushed]).finish().unwrap_err(),
            [Problem::Unresolved {
                ep: Ep::Keyboard,
                seq: 0
            }]
        );
        assert_eq!(
            records(&[
                t(0, 1),
                Body::Pending {
                    ep: Ep::Keyboard,
                    seq: 0
                },
                Body::Flushed
            ])
            .finish()
            .unwrap_err(),
            [Problem::Pending {
                ep: Ep::Keyboard,
                seq: 0
            }]
        );
    }

    #[test]
    fn a_pad_report_must_agree_with_the_trace() {
        let p = |seq, h| Body::Handoff {
            ep: Ep::Pad,
            seq,
            ready: h - 5,
            handoff: h,
        };
        let c = |seq, at| Body::Completion {
            ep: Ep::Pad,
            seq,
            at,
        };
        let trace = records(&[p(0, 100), c(0, 900), p(1, 1100), c(1, 1900), Body::Flushed])
            .finish()
            .unwrap();
        let first = crate::report::PadReport {
            seq: 0,
            ready: 95,
            ..Default::default()
        };
        assert_eq!(trace.check_pad_report(0, &first), Ok(()));
        let second = crate::report::PadReport {
            seq: 1,
            ready: 1095,
            previous: Some((100, 900)),
            ..Default::default()
        };
        assert_eq!(trace.check_pad_report(1, &second), Ok(()));
        let wrong = crate::report::PadReport {
            previous: Some((100, 901)),
            ..second
        };
        assert!(trace.check_pad_report(1, &wrong).is_err());
        let missing = crate::report::PadReport {
            previous: None,
            ..second
        };
        assert!(trace.check_pad_report(1, &missing).is_err());
    }
}
