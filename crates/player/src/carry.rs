//! What a play state's attempts take over from one another on the render
//! thread (design.md "beatoraja Lua Skins", the program's state): the
//! source keeps its skin's state and its cached point for as long as its
//! play state lasts (`ScoreDataProperty` of the `MainState`, not reset at
//! `BMSPlayer.java:582` at `0.8.9`), so a practice session's rounds carry
//! both.
//!
//! The record lives beside the committed installations, not in the
//! screen, so a screen built again keeps it. It is written only here:
//! at an attempt's commit, at a play installation's commit, when the
//! committed attempt starts, and when it is settled from the game's final
//! tally. Its slots are valid only through an unbroken chain of runs that
//! ran the callbacks with slots on a complete view; anything else
//! invalidates them for the rest of the play state, and the point stays.

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

/// Where the committed attempt of the record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Committed, not started: it changed nothing the next attempt reads.
    Idle,
    /// Started as the committed attempt of this request.
    Started(u64),
    /// Its final tally taken.
    Settled,
}

/// One play state's record.
#[derive(Debug)]
struct Record {
    key: CarryKey,
    point: i32,
    bank: Option<Bank>,
    /// The play installation the slots belong to; `None` for the simple
    /// skin.
    bundle: Option<Weak<SkinBundle>>,
    /// The committed attempt's target count, which its point is over.
    targets: u32,
    phase: Phase,
}

/// The render thread's record of the play state's carry.
#[derive(Debug, Default)]
pub struct Carries {
    record: Option<Record>,
}

/// Whether `a` and `b` name the same installation: the very allocation.
fn same(a: Option<&Weak<SkinBundle>>, b: Option<&Arc<SkinBundle>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::ptr::eq(a.as_ptr(), Arc::as_ptr(b)),
        _ => false,
    }
}

impl Carries {
    /// What an attempt of `key` prepared with `bundle` takes over: fresh
    /// for a play state with no record; else the cached point, with the
    /// slots when they are valid and belong to `bundle`. A record whose
    /// attempt started and was never settled has invalid slots.
    pub fn offer(&mut self, key: Option<CarryKey>, bundle: Option<&Arc<SkinBundle>>) -> Offer {
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
        if matches!(r.phase, Phase::Started(_)) {
            r.bank = None;
        }
        let valid = r.key.chart == key.chart && same(r.bundle.as_ref(), bundle);
        Offer::Carried {
            point: r.point,
            bank: r.bank.clone().filter(|_| valid),
        }
    }

    /// The attempt of `key` offered `offer` was committed with `bundle` as
    /// request `request`'s, the skin's committed attempt giving `carry`: a
    /// fresh one starts the record, a carried one keeps its point and
    /// takes its slots only when the offer's were valid.
    pub fn committed(
        &mut self,
        key: Option<CarryKey>,
        offer: &Offer,
        bundle: Option<&Arc<SkinBundle>>,
        carry: Option<Carry<'_>>,
    ) {
        let Some(key) = key else {
            return;
        };
        let bank = || Some(carry.map_or_else(Bank::default, |c| c.bank.clone()));
        let targets = carry.map_or(0, |c| c.targets);
        let point = match (offer, &self.record) {
            (Offer::Carried { .. }, Some(r)) if r.key.play_state == key.play_state => r.point,
            _ => 0,
        };
        self.record = Some(Record {
            key,
            point,
            bank: if offer.valid() { bank() } else { None },
            bundle: bundle.map(Arc::downgrade),
            targets,
            phase: Phase::Idle,
        });
    }

    /// A play installation was committed: slots of another installation
    /// are invalid from now on; the point stays.
    pub fn installed(&mut self, bundle: Option<&Arc<SkinBundle>>) {
        if let Some(r) = &mut self.record
            && !same(r.bundle.as_ref(), bundle)
        {
            r.bank = None;
        }
    }

    /// The committed attempt of request `request` started.
    pub fn started(&mut self, request: u64) {
        if let Some(r) = &mut self.record
            && r.phase == Phase::Idle
        {
            r.phase = Phase::Started(request);
        }
    }

    /// The started attempt ended with the final tally `tally`; `held`
    /// is the screen's committed attempt's request and `carry` what it
    /// gives. The point is the final tally's (kept for the zero tally);
    /// the slots stay valid only when they were, the attempt is still the
    /// screen's, and its last build ran the callbacks with slots on a
    /// complete view.
    pub fn settled(&mut self, tally: &Tally, held: Option<u64>, carry: Option<Carry<'_>>) {
        let Some(r) = &mut self.record else {
            return;
        };
        let Phase::Started(request) = r.phase else {
            return;
        };
        if let Some(p) = skin_model::settled_point(tally, r.targets) {
            r.point = p;
        }
        let witnessed = carry.filter(|c| held == Some(request) && c.slots_run && c.terminal);
        match (&mut r.bank, witnessed) {
            (Some(b), Some(c)) => b.copy_from(c.bank),
            (b, _) => *b = None,
        }
        r.phase = Phase::Settled;
    }
}
