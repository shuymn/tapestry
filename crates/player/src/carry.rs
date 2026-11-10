//! What a play state's attempts take over from one another on the render
//! thread (design.md "beatoraja Lua Skins", the program's state): the
//! source keeps its skin's state and its score data (the point, the EX
//! score and the score rates) for as long as its play state lasts
//! (`ScoreDataProperty` of the `MainState`, not reset at
//! `BMSPlayer.java:582` at `0.8.9`), so a practice session's rounds carry
//! both.
//!
//! The record lives beside the committed installations, not in the
//! screen, so a screen built again keeps it. It keeps three identities
//! apart, each changed only by its own events, every one through
//! [`Carries::on`]:
//! - the committed attempt: its target count and empty-POOR rule, over
//!   which its final tally gives the score data whatever the skin, and
//!   whether it is committed, started or settled;
//! - what the screen's `Play` gives at the settlement, the only place the
//!   attempt's slots can be read from (none once the screen lost it);
//! - the play installation the slots belong to.
//!
//! The slots are valid only through an unbroken chain of runs that ran the
//! callbacks with slots on a complete view, on the installation they
//! belong to; anything else invalidates them for the rest of the play
//! state, and the score data stays.

use std::sync::{Arc, Weak};

use eval::Tally;
use render::SkinBundle;
use skin_model::present::Bank;
use skin_model::{Carry, Settled};

/// Which play state and chart an attempt belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarryKey {
    /// The play state's number (`game::PlayState::number`).
    pub play_state: u64,
    /// The loaded chart's identity, which a practice's speed change keeps.
    pub chart: u64,
}

/// What an attempt is offered at its preparation.
#[derive(Clone, Debug, PartialEq)]
pub enum Offer {
    /// The play state's first attempt.
    Fresh,
    /// A later attempt: the cached score data (`None` before the play
    /// state's first judgment) and the slots, `None` when they are not
    /// valid.
    Carried {
        score: Option<Settled>,
        bank: Option<Bank>,
    },
}

impl Offer {
    /// The skin's view of it.
    pub fn continuation(&self) -> skin_model::Continuation<'_> {
        match self {
            Self::Fresh => skin_model::Continuation::Fresh,
            Self::Carried { score, bank } => skin_model::Continuation::Carried {
                score: *score,
                bank: bank.as_ref(),
            },
        }
    }

    /// Whether it carries valid slots, or starts them fresh.
    const fn valid(&self) -> bool {
        matches!(self, Self::Fresh | Self::Carried { bank: Some(_), .. })
    }
}

/// A play installation as the slots belong to it.
#[derive(Clone, Debug)]
pub enum Installed {
    /// The built-in skin: no program, no slots.
    Simple,
    /// A declarative skin's bundle, the very allocation.
    Bundle(Weak<SkinBundle>),
    /// The pending marker: no skin until a specialization commits.
    Pending,
}

impl Installed {
    /// `bundle`'s, or the built-in skin's.
    pub fn of(bundle: Option<&Arc<SkinBundle>>) -> Self {
        bundle.map_or(Self::Simple, |b| Self::Bundle(Arc::downgrade(b)))
    }

    /// Whether `self` and `other` are the same installation: the built-in
    /// skin, or the very bundle; the pending marker is none.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Simple, Self::Simple) => true,
            (Self::Bundle(a), Self::Bundle(b)) => Weak::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// What a committed attempt's score data is over: its target count and
/// whether its preset counts an empty POOR among the POORs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Basis {
    pub targets: u32,
    pub empty_poor_counted: bool,
}

/// Every event that changes the record or one of its identities.
#[derive(Debug)]
pub enum Event<'a> {
    /// An attempt of `key` offered `offer` was committed: it draws with
    /// `installed`, its score data is over `basis`, and the screen holds
    /// its `Play`, whose slots are `bank` (`None` for the built-in
    /// skin's).
    Committed {
        key: Option<CarryKey>,
        offer: &'a Offer,
        installed: Installed,
        basis: Basis,
        bank: Option<&'a Bank>,
    },
    /// A play installation was committed: a selection's bundle, or the
    /// pending marker.
    Installed(Installed),
    /// The committed attempt started (`RenderMsg::Play`).
    Started,
    /// The started attempt ended with the final tally `tally`; `held` is
    /// what the screen's `Play` gives, when it holds one. It can only be
    /// the committed attempt's: the attempt's settlement comes before the
    /// next preparation on the one channel from the game thread, and every
    /// other change of the screen's `Play` (a chart's load, a screen built
    /// again) leaves none.
    Settled {
        tally: &'a Tally,
        held: Option<Carry<'a>>,
    },
}

/// Where the committed attempt is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Committed, not started: it changed nothing the next attempt reads.
    Idle,
    Started,
    /// Its final tally taken.
    Settled,
}

/// One play state's record.
#[derive(Debug)]
struct Record {
    key: CarryKey,
    /// The score data the play state's last judgment left, `None` before
    /// any.
    score: Option<Settled>,
    bank: Option<Bank>,
    /// The installation the slots belong to.
    installed: Installed,
    /// What the committed attempt's score data is over.
    basis: Basis,
    phase: Phase,
}

/// The render thread's record of the play state's carry.
#[derive(Debug, Default)]
pub struct Carries {
    record: Option<Record>,
}

impl Carries {
    /// What an attempt of `key` drawing with `installed` takes over: fresh
    /// for a play state with no record; else the cached score data, with the
    /// slots when they are valid and belong to `installed`. A record whose
    /// attempt started and was never settled offers no slots, and the
    /// attempt's commit records none ([`Event::Committed`]). It changes
    /// nothing: every change goes through [`on`](Self::on).
    pub fn offer(&self, key: Option<CarryKey>, installed: &Installed) -> Offer {
        let Some(key) = key else {
            return Offer::Fresh;
        };
        let Some(r) = self
            .record
            .as_ref()
            .filter(|r| r.key.play_state == key.play_state)
        else {
            return Offer::Fresh;
        };
        let valid =
            r.phase != Phase::Started && r.key.chart == key.chart && r.installed.same(installed);
        Offer::Carried {
            score: r.score,
            bank: r.bank.clone().filter(|_| valid),
        }
    }

    /// Takes `event`: the one entry point of every change (the design
    /// note's identity table).
    pub fn on(&mut self, event: Event<'_>) {
        match event {
            Event::Committed {
                key,
                offer,
                installed,
                basis,
                bank,
            } => self.committed(key, offer, installed, basis, bank),
            Event::Installed(installed) => {
                if let Some(r) = &mut self.record
                    && !r.installed.same(&installed)
                {
                    r.bank = None;
                    r.installed = installed;
                }
            }
            Event::Started => {
                if let Some(r) = &mut self.record
                    && r.phase == Phase::Idle
                {
                    r.phase = Phase::Started;
                }
            }
            Event::Settled { tally, held } => self.settled(tally, held),
        }
    }

    /// A fresh attempt starts the record; a carried one keeps its score data
    /// and takes its slots only when the offer's were valid and its
    /// installation is the record's.
    fn committed(
        &mut self,
        key: Option<CarryKey>,
        offer: &Offer,
        installed: Installed,
        basis: Basis,
        bank: Option<&Bank>,
    ) {
        let Some(key) = key else {
            return;
        };
        let (score, same) = match (offer, &self.record) {
            (Offer::Carried { .. }, Some(r)) if r.key.play_state == key.play_state => {
                (r.score, r.installed.same(&installed))
            }
            _ => (None, true),
        };
        let bank = (offer.valid() && same).then(|| bank.cloned().unwrap_or_default());
        self.record = Some(Record {
            key,
            score,
            bank,
            installed,
            basis,
            phase: Phase::Idle,
        });
    }

    /// The score data from the final tally over the attempt's basis (kept,
    /// the point included, for the zero tally and for a tally the skin
    /// refuses); the slots stay valid only when they were, the
    /// screen still holds the attempt's `Play`, and its last build ran the
    /// callbacks with slots on a complete view.
    fn settled(&mut self, tally: &Tally, held: Option<Carry<'_>>) {
        let Some(r) = &mut self.record else {
            return;
        };
        if r.phase != Phase::Started {
            return;
        }
        let b = r.basis;
        if let Some(s) = skin_model::settled(tally, b.targets, b.empty_poor_counted) {
            r.score = Some(s);
        }
        let witnessed = held.filter(|c| c.slots_run && c.terminal);
        match (&mut r.bank, witnessed) {
            (Some(b), Some(c)) => b.copy_from(c.bank),
            (b, _) => *b = None,
        }
        r.phase = Phase::Settled;
    }
}

#[cfg(test)]
mod tests {
    use super::{Basis, Carries, CarryKey, Event, Installed, Offer};

    const KEY: CarryKey = CarryKey {
        play_state: 1,
        chart: 1,
    };

    /// The carry tests' attempts: 4 targets, empty POORs uncounted.
    const BASIS: Basis = Basis {
        targets: 4,
        empty_poor_counted: false,
    };

    /// 4 PGREATs over 4 targets but one EX short of the maximum (a custom
    /// weighting): point 200 000, EX 7, both rates 87.5 %.
    fn seven() -> eval::Tally {
        let mut t = eval::Tally::default();
        t.counts.perfect = 4;
        t.ex_score = 7;
        t.max_combo = 4;
        t.combo = 4;
        t
    }

    /// The score data [`seven`] settles over 4 targets.
    const SEVEN: skin_model::Settled = skin_model::Settled {
        point: 200_000,
        ex_score: 7,
        rate_current_bp: 8_750,
        rate_whole_bp: 8_750,
    };

    /// A played round of the play state: committed with `offer`, started
    /// and settled with `tally`.
    fn round(c: &mut Carries, offer: &Offer, tally: &eval::Tally) {
        c.on(Event::Committed {
            key: Some(KEY),
            offer,
            installed: Installed::Simple,
            basis: BASIS,
            bank: None,
        });
        c.on(Event::Started);
        c.on(Event::Settled { tally, held: None });
    }

    /// s7-06s S4: the zero tally (a round with no judgment) keeps the play
    /// state's score data, the point included.
    #[test]
    fn the_zero_tally_keeps_the_score() {
        let mut c = Carries::default();
        round(&mut c, &Offer::Fresh, &seven());
        let offer = c.offer(Some(KEY), &Installed::Simple);
        round(&mut c, &offer, &eval::Tally::default());
        assert_eq!(
            c.offer(Some(KEY), &Installed::Simple),
            Offer::Carried {
                score: Some(SEVEN),
                bank: None
            }
        );
    }

    /// s7-06s S5 (O7): a final tally the skin refuses (more counted empty
    /// POORs than POORs) keeps all four previous values, the point
    /// included, and no score data before any valid settlement.
    #[test]
    fn a_refused_tally_keeps_the_score() {
        let mut refused = eval::Tally::default();
        refused.counts.perfect = 1;
        refused.empty_poor = 1;
        refused.ex_score = 2;
        refused.max_combo = 1;
        let counted = Basis {
            empty_poor_counted: true,
            ..BASIS
        };
        let committed = |c: &mut Carries, offer: &Offer| {
            c.on(Event::Committed {
                key: Some(KEY),
                offer,
                installed: Installed::Simple,
                basis: counted,
                bank: None,
            });
            c.on(Event::Started);
        };
        let mut c = Carries::default();
        committed(&mut c, &Offer::Fresh);
        c.on(Event::Settled {
            tally: &refused,
            held: None,
        });
        let none = Offer::Carried {
            score: None,
            bank: None,
        };
        assert_eq!(c.offer(Some(KEY), &Installed::Simple), none);
        let mut c = Carries::default();
        round(&mut c, &Offer::Fresh, &seven());
        let offer = c.offer(Some(KEY), &Installed::Simple);
        committed(&mut c, &offer);
        c.on(Event::Settled {
            tally: &refused,
            held: None,
        });
        assert_eq!(
            c.offer(Some(KEY), &Installed::Simple),
            Offer::Carried {
                score: Some(SEVEN),
                bank: None
            }
        );
    }

    /// A round with the built-in skin, which gives no program state, settles
    /// the score data over the attempt's target count; a declarative skin
    /// installed after it in the play state reads it (code review r1,
    /// finding 2), its slots invalid.
    #[test]
    fn a_simple_skin_round_settles_the_score() {
        let mut c = Carries::default();
        let offer = c.offer(Some(KEY), &Installed::Simple);
        assert_eq!(offer, Offer::Fresh);
        c.on(Event::Committed {
            key: Some(KEY),
            offer: &offer,
            installed: Installed::Simple,
            basis: BASIS,
            bank: None,
        });
        c.on(Event::Started);
        c.on(Event::Settled {
            tally: &seven(),
            held: None,
        });
        let skin = crate::render_side::tests::bundle();
        let installed = Installed::of(Some(&skin));
        c.on(Event::Installed(installed.clone()));
        assert_eq!(
            c.offer(Some(KEY), &installed),
            Offer::Carried {
                score: Some(SEVEN),
                bank: None
            }
        );
    }

    /// A round that started and was never settled offers the next attempt
    /// no slots and its point, and the offer changes nothing: the next
    /// attempt's commit records no slots, and an offer before it is the
    /// same.
    #[test]
    fn an_unsettled_round_offers_no_slots() {
        use skin_model::present::{Bank, Val};
        let mut c = Carries::default();
        let bank = Bank::of(vec![Val::Int(7)]);
        c.on(Event::Committed {
            key: Some(KEY),
            offer: &Offer::Fresh,
            installed: Installed::Simple,
            basis: BASIS,
            bank: Some(&bank),
        });
        assert_eq!(
            c.offer(Some(KEY), &Installed::Simple),
            Offer::Carried {
                score: None,
                bank: Some(bank.clone())
            }
        );
        c.on(Event::Started);
        let unsettled = Offer::Carried {
            score: None,
            bank: None,
        };
        assert_eq!(c.offer(Some(KEY), &Installed::Simple), unsettled);
        assert_eq!(c.offer(Some(KEY), &Installed::Simple), unsettled);
        c.on(Event::Committed {
            key: Some(KEY),
            offer: &unsettled,
            installed: Installed::Simple,
            basis: BASIS,
            bank: Some(&bank),
        });
        assert_eq!(c.offer(Some(KEY), &Installed::Simple), unsettled);
    }
}
