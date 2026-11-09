//! What a play state's attempts take over from one another on the render
//! thread (design.md "beatoraja Lua Skins", the program's state): the
//! source keeps its skin's state and its cached point for as long as its
//! play state lasts (`ScoreDataProperty` of the `MainState`, not reset at
//! `BMSPlayer.java:582` at `0.8.9`), so a practice session's rounds carry
//! both.
//!
//! The record lives beside the committed installations, not in the
//! screen, so a screen built again keeps it. It keeps three identities
//! apart, each changed only by its own events, every one through
//! [`Carries::on`]:
//! - the committed attempt: its target count, over which its final tally
//!   gives the point whatever the skin, and whether it is committed,
//!   started or settled;
//! - whether the screen still holds that attempt's `Play`, the only place
//!   its slots can be read from;
//! - the play installation the slots belong to.
//!
//! The slots are valid only through an unbroken chain of runs that ran the
//! callbacks with slots on a complete view, on the installation they
//! belong to; anything else invalidates them for the rest of the play
//! state, and the point stays.

use std::sync::{Arc, Weak};

use eval::Tally;
use render::SkinBundle;
use skin_model::Carry;
use skin_model::present::Bank;

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
    /// A later attempt: the cached point and the slots, `None` when they
    /// are not valid.
    Carried { point: i32, bank: Option<Bank> },
}

impl Offer {
    /// The skin's view of it.
    pub fn continuation(&self) -> skin_model::Continuation<'_> {
        match self {
            Self::Fresh => skin_model::Continuation::Fresh,
            Self::Carried { point, bank } => skin_model::Continuation::Carried {
                point: *point,
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

/// Every event that changes the record or one of its identities.
#[derive(Debug)]
pub enum Event<'a> {
    /// An attempt of `key` offered `offer` was committed: it draws with
    /// `installed`, has `targets` targets, and the screen holds its `Play`,
    /// whose slots are `bank` (`None` for the built-in skin's).
    Committed {
        key: Option<CarryKey>,
        offer: &'a Offer,
        installed: Installed,
        targets: u32,
        bank: Option<&'a Bank>,
    },
    /// A play installation was committed: a selection's bundle, or the
    /// pending marker.
    Installed(Installed),
    /// The screen no longer holds the committed attempt's `Play`: a
    /// chart's load, an in-place preparation, the screen dropped.
    PlayGone,
    /// The committed attempt started (`RenderMsg::Play`).
    Started,
    /// The started attempt ended with the final tally `tally`; `held` is
    /// what the screen's committed attempt's `Play` gives, when the screen
    /// holds it.
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
    point: i32,
    bank: Option<Bank>,
    /// The installation the slots belong to.
    installed: Installed,
    /// The committed attempt's target count, which its point is over.
    targets: u32,
    phase: Phase,
    /// Whether the screen holds the committed attempt's `Play`.
    held: bool,
}

/// The render thread's record of the play state's carry.
#[derive(Debug, Default)]
pub struct Carries {
    record: Option<Record>,
}

impl Carries {
    /// What an attempt of `key` drawing with `installed` takes over: fresh
    /// for a play state with no record; else the cached point, with the
    /// slots when they are valid and belong to `installed`. A record whose
    /// attempt started and was never settled has invalid slots.
    pub fn offer(&mut self, key: Option<CarryKey>, installed: &Installed) -> Offer {
        let Some(key) = key else {
            return Offer::Fresh;
        };
        let Some(r) = self
            .record
            .as_mut()
            .filter(|r| r.key.play_state == key.play_state)
        else {
            return Offer::Fresh;
        };
        if r.phase == Phase::Started {
            r.bank = None;
            r.phase = Phase::Settled;
        }
        let valid = r.key.chart == key.chart && r.installed.same(installed);
        Offer::Carried {
            point: r.point,
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
                targets,
                bank,
            } => self.committed(key, offer, installed, targets, bank),
            Event::Installed(installed) => {
                if let Some(r) = &mut self.record
                    && !r.installed.same(&installed)
                {
                    r.bank = None;
                    r.installed = installed;
                }
            }
            Event::PlayGone => {
                if let Some(r) = &mut self.record {
                    r.held = false;
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

    /// A fresh attempt starts the record; a carried one keeps its point
    /// and takes its slots only when the offer's were valid and its
    /// installation is the record's.
    fn committed(
        &mut self,
        key: Option<CarryKey>,
        offer: &Offer,
        installed: Installed,
        targets: u32,
        bank: Option<&Bank>,
    ) {
        let Some(key) = key else {
            return;
        };
        let (point, same) = match (offer, &self.record) {
            (Offer::Carried { .. }, Some(r)) if r.key.play_state == key.play_state => {
                (r.point, r.installed.same(&installed))
            }
            _ => (0, true),
        };
        let bank = (offer.valid() && same).then(|| bank.cloned().unwrap_or_default());
        self.record = Some(Record {
            key,
            point,
            bank,
            installed,
            targets,
            phase: Phase::Idle,
            held: true,
        });
    }

    /// The point from the final tally over the attempt's targets (kept for
    /// the zero tally); the slots stay valid only when they were, the
    /// screen still holds the attempt's `Play`, and its last build ran the
    /// callbacks with slots on a complete view.
    fn settled(&mut self, tally: &Tally, held: Option<Carry<'_>>) {
        let Some(r) = &mut self.record else {
            return;
        };
        if r.phase != Phase::Started {
            return;
        }
        if let Some(p) = skin_model::settled_point(tally, r.targets) {
            r.point = p;
        }
        let witnessed = held.filter(|c| r.held && c.slots_run && c.terminal);
        match (&mut r.bank, witnessed) {
            (Some(b), Some(c)) => b.copy_from(c.bank),
            (b, _) => *b = None,
        }
        r.phase = Phase::Settled;
    }
}

#[cfg(test)]
mod tests {
    use super::{Carries, CarryKey, Event, Installed, Offer};

    const KEY: CarryKey = CarryKey {
        play_state: 1,
        chart: 1,
    };

    /// A round with the built-in skin, which gives no program state, settles
    /// the point over the attempt's target count; a declarative skin
    /// installed after it in the play state reads that point (code review
    /// r1, finding 2), its slots invalid.
    #[test]
    fn a_simple_skin_round_settles_the_point() {
        let mut c = Carries::default();
        let offer = c.offer(Some(KEY), &Installed::Simple);
        assert_eq!(offer, Offer::Fresh);
        c.on(Event::Committed {
            key: Some(KEY),
            offer: &offer,
            installed: Installed::Simple,
            targets: 4,
            bank: None,
        });
        c.on(Event::Started);
        let mut t = eval::Tally::default();
        t.counts.perfect = 4;
        t.max_combo = 4;
        t.combo = 4;
        c.on(Event::Settled {
            tally: &t,
            held: None,
        });
        let skin = crate::render_side::tests::bundle();
        let installed = Installed::of(Some(&skin));
        c.on(Event::Installed(installed.clone()));
        assert_eq!(
            c.offer(Some(KEY), &installed),
            Offer::Carried {
                point: 200_000,
                bank: None
            }
        );
    }
}
