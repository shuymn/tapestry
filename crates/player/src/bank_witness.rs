//! The live-bank witness (dev-tools; design.md "Load Pipeline", the
//! release barrier): every sound bank the chosen chart's load decodes,
//! and every decode in flight, so that a load can check, before its bank
//! decode begins, that no earlier bank is still held anywhere.
//!
//! A decode is accounted by a [`DecodeToken`] taken on the thread that
//! starts it, before its job exists, and held through the job's admission,
//! its decode, its return and the result's trip back; the token ends at
//! the bank's registration, which records the bank first and only then
//! drops the count, or with the result when no bank is kept. So a bank is
//! never unaccounted between the decode that makes it and its owner.
//!
//! The background measurement's decodes and `tapestry play`'s take no
//! token: they are not the chosen chart's load.

use std::sync::{Arc, Mutex, PoisonError, Weak};

/// The witness: the banks registered and the decodes in flight, and what
/// its checks found.
#[derive(Debug, Default)]
pub struct BankWitness {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    in_flight: usize,
    banks: Vec<Weak<audio::SoundBank>>,
    checks: usize,
    violations: Vec<String>,
}

impl State {
    /// The registered banks still alive; the dead ones are forgotten.
    fn alive(&mut self) -> usize {
        self.banks.retain(|b| b.strong_count() > 0);
        self.banks.len()
    }
}

/// A decode in flight, until its bank is registered or it is dropped.
#[derive(Debug)]
pub struct DecodeToken {
    witness: Arc<BankWitness>,
}

impl BankWitness {
    /// A witness with nothing registered.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accounts a decode about to start.
    #[must_use]
    pub fn decode(self: &Arc<Self>) -> DecodeToken {
        self.state().in_flight += 1;
        DecodeToken {
            witness: Arc::clone(self),
        }
    }

    /// The decodes in flight and the registered banks alive.
    #[cfg(test)]
    pub fn now(&self) -> (usize, usize) {
        let mut s = self.state();
        let alive = s.alive();
        (s.in_flight, alive)
    }

    /// The release barrier's check, before a bank decode begins or a kept
    /// bank is installed: no decode in flight and no registered bank alive
    /// but `allowed`, the practice page's 1× bank a Start at 1× installs;
    /// a violation is recorded and logged.
    pub fn check(&self, allowed: Option<&Arc<audio::SoundBank>>) {
        let mut s = self.state();
        s.checks += 1;
        let alive = s.alive()
            - allowed.map_or(0, |a| {
                s.banks
                    .iter()
                    .filter(|b| std::ptr::eq(b.as_ptr(), Arc::as_ptr(a)))
                    .count()
            });
        if s.in_flight != 0 || alive != 0 {
            let line = format!(
                "bank witness: at a decode's start {} decodes in flight and {alive} banks alive",
                s.in_flight
            );
            s.violations.push(line.clone());
            drop(s);
            log::warn!("{line}");
            eprintln!("{line}");
        }
    }

    /// The checks made so far and the violations they recorded.
    #[cfg(test)]
    pub fn record(&self) -> (usize, Vec<String>) {
        let s = self.state();
        (s.checks, s.violations.clone())
    }
}

impl DecodeToken {
    /// The decode made `bank`, now owned by the load: it is recorded, and
    /// only then is the decode no longer in flight. More than two banks
    /// alive (this one and the one installed) is a violation.
    pub fn register(self, bank: &Arc<audio::SoundBank>) {
        let mut s = self.witness.state();
        s.banks.push(Arc::downgrade(bank));
        let alive = s.alive();
        if alive > 2 {
            let line = format!("bank witness: {alive} banks alive at a registration");
            s.violations.push(line.clone());
            drop(s);
            log::warn!("{line}");
            eprintln!("{line}");
        }
    }
}

impl Drop for DecodeToken {
    fn drop(&mut self) {
        let mut s = self.witness.state();
        s.in_flight = s.in_flight.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::BankWitness;

    fn bank() -> Arc<audio::SoundBank> {
        Arc::new(audio::SoundBank::from_sounds(48_000, Vec::new()))
    }

    /// A decode is in flight from its token to its registration, with no
    /// gap, and a check then finds it; its bank is alive while held, and a
    /// check finds that too; with neither, a check finds nothing.
    #[test]
    fn a_decode_is_in_flight_until_its_bank_is_registered() {
        let w = BankWitness::new();
        let token = w.decode();
        assert_eq!(w.now(), (1, 0));
        w.check(None);
        assert_eq!(w.record().1.len(), 1, "a decode in flight");
        let b = bank();
        assert_eq!(w.now(), (1, 0), "a bank made, not yet registered");
        token.register(&b);
        assert_eq!(w.now(), (0, 1));
        w.check(None);
        drop(b);
        assert_eq!(w.now(), (0, 0));
        w.check(None);
        let (checks, violations) = w.record();
        assert_eq!(checks, 3);
        assert_eq!(violations.len(), 2, "{violations:?}");
    }

    /// A token dropped with no bank kept ends its decode.
    #[test]
    fn a_dropped_token_ends_its_decode() {
        let w = BankWitness::new();
        drop(w.decode());
        assert_eq!(w.now(), (0, 0));
        w.check(None);
        assert!(w.record().1.is_empty());
    }

    /// The allowed bank alone alive passes a check; another bank alive
    /// beside it does not, and neither does it once not allowed.
    #[test]
    fn only_the_allowed_bank_may_be_alive_at_a_check() {
        let w = BankWitness::new();
        let (kept, other) = (bank(), bank());
        w.decode().register(&kept);
        w.check(Some(&kept));
        assert!(w.record().1.is_empty(), "{:?}", w.record().1);
        w.check(Some(&other));
        assert_eq!(w.record().1.len(), 1, "the kept bank, not allowed");
        w.decode().register(&other);
        w.check(Some(&kept));
        assert_eq!(w.record().1.len(), 2, "another bank beside the allowed one");
        w.check(None);
        assert_eq!(w.record().1.len(), 3);
    }

    /// A third bank alive at a registration is a violation.
    #[test]
    fn three_banks_alive_at_a_registration_is_a_violation() {
        let w = BankWitness::new();
        let banks: Vec<_> = (0..3).map(|_| bank()).collect();
        for (k, b) in banks.iter().enumerate() {
            w.decode().register(b);
            assert_eq!(w.record().1.len(), usize::from(k == 2));
        }
    }
}
