//! The harness's model of the player's carry (s7-06e-2-design.md §4): the
//! player's `Carries` rule (`crates/player/src/carry.rs`, `offer` and
//! `settled`), restated for one chart and one installation, as design.md
//! "The program" states it. It is inside the trusted boundary: the
//! player's carry itself is not run.

use eval::Tally;
use skin_model::present::Bank;
use skin_model::{Carry, Continuation, Settled};

/// What a round took over, as the comparator reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carried {
    /// The play state's first round.
    Fresh,
    /// A later round: whether its incoming bank was valid, and whether
    /// the program is carry-safe.
    Carried { bank_valid: bool, carry_safe: bool },
}

impl Carried {
    /// Why the round's slots are refused, if they are: an invalid bank
    /// first, then a program that is not carry-safe.
    pub fn refused(self) -> Option<&'static str> {
        match self {
            Self::Fresh => None,
            Self::Carried {
                bank_valid: false, ..
            } => Some("invalid bank"),
            Self::Carried {
                carry_safe: false, ..
            } => Some("not carry-safe"),
            Self::Carried { .. } => None,
        }
    }
}

/// The record of one play state: nothing before its first round, then
/// the cached score data (`None` before any judgment) and the slots,
/// `None` once invalid.
#[derive(Debug, Default)]
pub struct Model {
    record: Option<(Option<Settled>, Option<Bank>)>,
}

impl Model {
    /// What the next round takes over.
    pub fn offer(&self) -> Continuation<'_> {
        match &self.record {
            None => Continuation::Fresh,
            Some((score, bank)) => Continuation::Carried {
                score: *score,
                bank: bank.as_ref(),
            },
        }
    }

    /// The offer as the comparator reads it, under `carry_safe`.
    pub fn carried(&self, carry_safe: bool) -> Carried {
        match &self.record {
            None => Carried::Fresh,
            Some((_, bank)) => Carried::Carried {
                bank_valid: bank.is_some(),
                carry_safe,
            },
        }
    }

    /// The cached score data: the last settled judgment's, `None` before
    /// any.
    pub fn score(&self) -> Option<Settled> {
        self.record.as_ref().and_then(|r| r.0)
    }

    /// Settles the round that took the current offer and ended with
    /// `tally`, its `Play` showing `carry`, under its preset's empty-POOR
    /// rule: the score data from the final tally (kept for the zero tally
    /// and a refused one); the slots copied only when the incoming
    /// ones were valid (or the round fresh) and the round's last build ran
    /// them on a complete view, else `None` from then on.
    pub fn settle(&mut self, tally: &Tally, carry: &Carry<'_>, empty_poor_counted: bool) {
        let (score, valid) = match &self.record {
            None => (None, true),
            Some((s, b)) => (*s, b.is_some()),
        };
        let score = skin_model::settled(tally, carry.targets, empty_poor_counted).or(score);
        let witnessed = carry.slots_run && carry.terminal;
        let bank = (valid && witnessed).then(|| carry.bank.clone());
        self.record = Some((score, bank));
    }
}

#[cfg(test)]
mod tests {
    use eval::Tally;
    use skin_model::Carry;
    use skin_model::present::Bank;

    use super::{Carried, Model};

    fn carry(bank: &Bank, witnessed: bool) -> Carry<'_> {
        Carry {
            bank,
            slots_run: witnessed,
            terminal: witnessed,
            targets: 7,
        }
    }

    /// A fresh round's unwitnessed end gives no slots; a `None` bank stays
    /// `None` after a witnessed round; the zero tally keeps the point.
    #[test]
    fn the_model_follows_the_carry_rule() {
        let bank = Bank::default();
        let mut tally = Tally::default();
        tally.counts.perfect = 7;
        let mut m = Model::default();
        assert_eq!(m.carried(true), Carried::Fresh);
        m.settle(&tally, &carry(&bank, false), false);
        let score = m.score();
        assert!(score.is_some_and(|s| s.point > 0 && s.ex_score == 0));
        assert_eq!(
            m.carried(true),
            Carried::Carried {
                bank_valid: false,
                carry_safe: true
            }
        );
        m.settle(&Tally::default(), &carry(&bank, true), false);
        assert_eq!(m.score(), score, "the zero tally keeps the score data");
        assert_eq!(m.carried(true).refused(), Some("invalid bank"));
        let mut w = Model::default();
        w.settle(&tally, &carry(&bank, true), false);
        assert_eq!(w.carried(false).refused(), Some("not carry-safe"));
        assert_eq!(w.carried(true).refused(), None);
    }
}
