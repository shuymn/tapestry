//! The phase-controlled revocation trial (docs/test-device.md "Revocation
//! Trial"): `revoke-test` makes the device hand a report it then revokes,
//! at a known point of the host's polling, so a run shows that a revoked
//! report never arrives, that the release after it does, and that nothing
//! plays before the host arms again.
//!
//! The device's phases are in [`crate::device`]; with `std`, [`judge`]
//! gives the host's verdict on one trial.

use crate::protocol::Control;
use crate::trace::Ep;

/// k1 is handed this long after C(k0), the host's poll phase.
pub const K1_AFTER_C0: u64 = 2_000;
/// The lease expires this long after C(k0): after k1's handoff and before
/// the host's next poll at 125 Hz, 8 ms after C(k0).
pub const DISARM_AFTER_C0: u64 = 4_000;
/// The probe item is due this long after k1's handoff, after the disarm.
pub const PROBE_AFTER_K1: u64 = 16_000;
/// The host withholds `arm` until this long after the probe's due time.
pub const REARM_AFTER_PROBE: u64 = 20_000;

/// The controls of a trial on one endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// Held down in k0 and k1; its release shows the neutral report
    /// arrived.
    pub sentinel: Control,
    /// Pressed only in k1, the revoked report.
    pub revoked: Control,
    /// The armed pattern's only item, due after the disarm.
    pub probe: Control,
}

/// F23, F24 and F22 on the keyboard (they type nothing); buttons 15, 16
/// and 14 on the pad.
#[must_use]
pub const fn controls(ep: Ep) -> Controls {
    match ep {
        Ep::Keyboard => Controls {
            sentinel: Control::Key(0x72),
            revoked: Control::Key(0x73),
            probe: Control::Key(0x71),
        },
        Ep::Pad => Controls {
            sentinel: Control::Button(15),
            revoked: Control::Button(16),
            probe: Control::Button(14),
        },
    }
}

#[cfg(feature = "std")]
pub use host::{Failure, Seen, edges, judge};

#[cfg(feature = "std")]
mod host {
    use core::fmt;

    use super::{REARM_AFTER_PROBE, controls};
    use crate::protocol::Control;
    use crate::report::{KEYBOARD_REPORT, KeyboardReport, PAD_REPORT, PadReport};
    use crate::trace::{Ep, Trace, TrialRun};

    /// One change the listener saw on the trial's endpoint, at a device time
    /// (the host maps its receive time onto the device's clock).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Seen {
        pub at: u64,
        pub control: Control,
        pub down: bool,
    }

    /// Why a trial does not pass.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Failure {
        /// The trace has no k1.
        Missing,
        /// k1 was not still queued at the abort: the host took it first.
        /// Not a failure of the device, but the trial shows nothing.
        NotExercised,
        /// The listener saw the revoked report's control.
        RevokedArrived,
        /// The next change after the sentinel's press is not its release,
        /// or the neutral report was not taken.
        SentinelBreakMissing,
        /// Something was handed, or the probe seen, before the host armed
        /// again.
        ItemBeforeRearm,
    }

    impl Failure {
        #[must_use]
        pub const fn message(self) -> &'static str {
            match self {
                Self::Missing => "k1 is not in the trace",
                Self::NotExercised => "not exercised",
                Self::RevokedArrived => "revoked report arrived",
                Self::SentinelBreakMissing => "sentinel break missing after revocation",
                Self::ItemBeforeRearm => "item before rearm",
            }
        }
    }

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message())
        }
    }

    const fn report_len(ep: Ep) -> u16 {
        match ep {
            #[expect(clippy::cast_possible_truncation, reason = "17 bytes")]
            Ep::Keyboard => KEYBOARD_REPORT as u16,
            #[expect(clippy::cast_possible_truncation, reason = "18 bytes")]
            Ep::Pad => PAD_REPORT as u16,
        }
    }

    /// The changes from report `before` to report `after` on `ep` (pad
    /// buttons, keyboard usages and modifiers; the axis is a position, not
    /// a press). Empty for bytes that are not such a report.
    #[must_use]
    pub fn edges(ep: Ep, before: &[u8], after: &[u8]) -> Vec<(Control, bool)> {
        match ep {
            Ep::Pad => {
                let (Some(b), Some(a)) = (PadReport::decode(before), PadReport::decode(after))
                else {
                    return Vec::new();
                };
                (1..=16_u8)
                    .filter_map(|n| {
                        let bit = 1_u16 << (n - 1);
                        let down = a.buttons & bit != 0;
                        (down != (b.buttons & bit != 0)).then_some((Control::Button(n), down))
                    })
                    .collect()
            }
            Ep::Keyboard => {
                let (Some(b), Some(a)) = (
                    KeyboardReport::from_bytes(before),
                    KeyboardReport::from_bytes(after),
                ) else {
                    return Vec::new();
                };
                (0..=0x7F_u8)
                    .chain(0xE0..=0xE7)
                    .filter_map(|u| {
                        let down = a.is_down(u);
                        (down != b.is_down(u)).then_some((Control::Key(u), down))
                    })
                    .collect()
            }
        }
    }

    /// The verdict on trial `run`, from the flushed trace and the
    /// listener's changes on its endpoint in order:
    ///
    /// 1. k1 was still queued at the abort: its `X` shows AVAILABLE and its
    ///    report's LENGTH ([`Failure::NotExercised`] otherwise).
    /// 2. The listener never saw the revoked control.
    /// 3. The next change after the sentinel's press is its release, and
    ///    the trace shows the report after k1 (the neutral one) taken.
    /// 4. Until [`REARM_AFTER_PROBE`] after the probe's due time, nothing
    ///    after the neutral report was handed on the endpoint and the
    ///    listener saw no probe.
    ///
    /// # Errors
    ///
    /// The first [`Failure`].
    pub fn judge(run: &TrialRun, trace: &Trace, seen: &[Seen]) -> Result<(), Failure> {
        let c = controls(run.ep);
        let k1 = trace
            .reports
            .get(&(run.ep, run.k1))
            .ok_or(Failure::Missing)?;
        if k1.revoked != Some((true, report_len(run.ep))) {
            return Err(Failure::NotExercised);
        }
        if seen.iter().any(|s| s.control == c.revoked && s.down) {
            return Err(Failure::RevokedArrived);
        }
        let make = seen.iter().position(|s| s.control == c.sentinel && s.down);
        let next = make.and_then(|i| seen.get(i + 1));
        let neutral = trace.reports.get(&(run.ep, run.k1.wrapping_add(1)));
        let released = next.is_some_and(|s| s.control == c.sentinel && !s.down);
        if !released || !neutral.is_some_and(|n| n.completion.is_some()) {
            return Err(Failure::SentinelBreakMissing);
        }
        let rearm = run.probe.saturating_add(REARM_AFTER_PROBE);
        let handed_early = trace.reports.iter().any(|(&(ep, seq), e)| {
            let after = seq.wrapping_sub(run.k1);
            ep == run.ep && (2..0x8000_0000).contains(&after) && e.handoff < rearm
        });
        let probe_early = seen
            .iter()
            .any(|s| s.control == c.probe && s.down && s.at < rearm);
        if handed_early || probe_early {
            return Err(Failure::ItemBeforeRearm);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Body, Check, Record, Trace, TrialRun};

    const RUN: TrialRun = TrialRun {
        ep: Ep::Keyboard,
        trial: 1,
        k0: 10,
        k1: 11,
        probe: 30_000,
    };

    /// A trial's trace: k0 taken, k1 revoked while queued, the neutral
    /// report taken, then `more` reports handed at the given times.
    fn trace(k1_available: bool, more: &[u64]) -> Trace {
        let ep = Ep::Keyboard;
        let mut bodies = vec![
            Body::Handoff {
                ep,
                seq: 10,
                ready: 1_000,
                handoff: 1_000,
            },
            Body::Completion {
                ep,
                seq: 10,
                at: 5_000,
            },
            Body::Handoff {
                ep,
                seq: 11,
                ready: 7_000,
                handoff: 7_000,
            },
            Body::Trial(RUN),
            Body::Revoked {
                ep,
                seq: 11,
                available: k1_available,
                len: 17,
                at: 9_000,
            },
            Body::Handoff {
                ep,
                seq: 12,
                ready: 9_100,
                handoff: 9_100,
            },
            Body::Completion {
                ep,
                seq: 12,
                at: 13_000,
            },
        ];
        for (seq, &at) in (13..).zip(more) {
            bodies.push(Body::Handoff {
                ep,
                seq,
                ready: at,
                handoff: at,
            });
            bodies.push(Body::Completion {
                ep,
                seq,
                at: at + 100,
            });
        }
        bodies.push(Body::Flushed);
        let mut check = Check::default();
        for (n, body) in (0..).zip(bodies) {
            check.feed(Record { n, body });
        }
        check.finish().unwrap()
    }

    fn seen(changes: &[(u64, Control, bool)]) -> Vec<Seen> {
        changes
            .iter()
            .map(|&(at, control, down)| Seen { at, control, down })
            .collect()
    }

    const F22: Control = Control::Key(0x71);
    const F23: Control = Control::Key(0x72);
    const F24: Control = Control::Key(0x73);

    #[test]
    fn a_trial_passes_and_each_failure_has_its_own_verdict() {
        let good = seen(&[
            (5_000, F23, true),
            (13_000, F23, false),
            (60_000, F22, true),
        ]);
        assert_eq!(judge(&RUN, &trace(true, &[59_900]), &good), Ok(()));
        assert_eq!(
            judge(&RUN, &trace(false, &[]), &good),
            Err(Failure::NotExercised)
        );
        let arrived = seen(&[(5_000, F23, true), (13_000, F24, true)]);
        assert_eq!(
            judge(&RUN, &trace(true, &[]), &arrived).map_err(Failure::message),
            Err("revoked report arrived")
        );
        let stuck = seen(&[(5_000, F23, true)]);
        assert_eq!(
            judge(&RUN, &trace(true, &[]), &stuck).map_err(Failure::message),
            Err("sentinel break missing after revocation")
        );
        assert_eq!(
            judge(&RUN, &trace(true, &[46_000]), &good).map_err(Failure::message),
            Err("item before rearm"),
            "a handoff inside the withheld window"
        );
        let early = seen(&[
            (5_000, F23, true),
            (13_000, F23, false),
            (46_000, F22, true),
        ]);
        assert_eq!(
            judge(&RUN, &trace(true, &[]), &early).map_err(Failure::message),
            Err("item before rearm"),
            "the probe seen inside the withheld window"
        );
    }

    #[test]
    fn edges_name_each_changed_control() {
        let mut before = crate::report::KeyboardReport::NEUTRAL;
        before.set(0x72, true);
        let mut after = before;
        after.set(0x72, false);
        after.set(0xE1, true);
        assert_eq!(
            edges(Ep::Keyboard, before.bytes(), after.bytes()),
            [(Control::Key(0x72), false), (Control::Key(0xE1), true)]
        );
        let pad = |buttons| {
            crate::report::PadReport {
                buttons,
                ..Default::default()
            }
            .encode()
        };
        assert_eq!(
            edges(Ep::Pad, &pad(1 << 14), &pad(1 << 15)),
            [(Control::Button(15), false), (Control::Button(16), true)]
        );
    }
}
