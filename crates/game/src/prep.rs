//! A play's preparation as a handle begun and polled, and its bounded
//! final readiness (design.md "Play Lifecycle", preparing each attempt):
//! what the preparation is keyed by, so a change of an input it read is
//! seen at the final readiness and never repaired there.

use bms_model::Speed;
use rules::GaugeValue;

use crate::attempt::AttemptKind;

/// The screen's structural inputs a preparation was staged with: its
/// device, the target size it laid the attempt out for, and the play
/// skin's installation it prepared with. The rig reports them; the flow
/// only compares them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenKey {
    /// The screen's device: a screen built again has another.
    pub device: u64,
    /// The target's size in pixels.
    pub target: [u32; 2],
    /// The play skin's installation in use (a selection's identity).
    pub skin: u64,
}

/// What a preparation read (design.md "Play Lifecycle", preparing each
/// attempt): every input but the attempt's number and its adjustment
/// epoch, which only the final readiness takes. Two keys that differ mean
/// the preparation of one does not fit the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepKey {
    /// The loaded chart's identity: a new choice is another.
    pub(crate) chart: u64,
    /// The speed the chart is loaded at.
    pub(crate) speed: Speed,
    /// The whole chart with its lead-in, or a practice round's section.
    pub(crate) kind: AttemptKind,
    /// A practice round's initial gauges; none for a full play.
    pub(crate) gauges: Vec<GaugeValue>,
    /// The play preset's canonical bytes, hashed.
    pub(crate) preset: u64,
    /// The settings' version: it moves whenever they may have changed.
    pub(crate) settings: u64,
    /// The screen's structural inputs, once a preparation was staged with
    /// them; `None` before.
    pub(crate) screen: Option<ScreenKey>,
}

/// Which inputs of a [`PrepKey`] differ from another's: what a `Stale`
/// final readiness names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per input of the key, named as the key's fields"
)]
pub struct Changes {
    pub chart: bool,
    pub speed: bool,
    pub kind: bool,
    pub gauges: bool,
    pub preset: bool,
    pub settings: bool,
    pub screen: bool,
}

impl Changes {
    /// Whether nothing changed.
    #[must_use]
    pub const fn none(&self) -> bool {
        !(self.chart
            || self.speed
            || self.kind
            || self.gauges
            || self.preset
            || self.settings
            || self.screen)
    }
}

impl PrepKey {
    /// The screen's structural inputs it holds, if a preparation was
    /// staged with them.
    #[must_use]
    pub const fn screen(&self) -> Option<ScreenKey> {
        self.screen
    }

    /// This key, with the screen's structural inputs `screen`.
    #[must_use]
    pub fn with_screen(self, screen: ScreenKey) -> Self {
        Self {
            screen: Some(screen),
            ..self
        }
    }

    /// Which inputs differ in `other`.
    #[must_use]
    pub fn changes(&self, other: &Self) -> Changes {
        Changes {
            chart: self.chart != other.chart,
            speed: self.speed != other.speed,
            kind: self.kind != other.kind,
            gauges: self.gauges != other.gauges,
            preset: self.preset != other.preset,
            settings: self.settings != other.settings,
            screen: self.screen != other.screen,
        }
    }
}

/// A preparation begun ([`crate::Rig::begin_preparation`]): its key and
/// the rig's ticket for it. The rig keeps the work; whatever ends the
/// play before its commit discards it ([`crate::Rig::discard_attempt`]),
/// and a preparation begun again supersedes it, so a late answer installs
/// nothing.
#[derive(Debug)]
pub struct PrepHandle {
    key: PrepKey,
    ticket: u64,
}

impl PrepHandle {
    /// The handle of the rig's preparation `ticket` with `key`.
    #[must_use]
    pub const fn new(key: PrepKey, ticket: u64) -> Self {
        Self { key, ticket }
    }

    /// What it was begun with, and the screen's structural inputs once
    /// staged.
    #[must_use]
    pub const fn key(&self) -> &PrepKey {
        &self.key
    }

    /// The rig's ticket.
    #[must_use]
    pub const fn ticket(&self) -> u64 {
        self.ticket
    }

    /// Records the screen's structural inputs the preparation was staged
    /// with: the rig's, when its poll answers ready.
    pub fn staged_with(&mut self, screen: ScreenKey) {
        self.key.screen = Some(screen);
    }
}

/// A poll's answer: done, or not yet at its deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepPoll<T> {
    /// Not done at the deadline.
    Pending,
    /// Done.
    Done(T),
}

/// The final readiness's answer ([`crate::Rig::ready_attempt`]).
#[derive(Debug)]
pub enum Readiness<E> {
    /// The attempt's number and epoch are stamped on what was staged: the
    /// commit may follow.
    Ready,
    /// What was staged does not fit the screen's structural inputs any
    /// more, or was lost. Nothing is prepared here.
    Stale(Stale<E>),
}

/// A `Stale` final readiness: the screen's structural inputs now, and the
/// error the attempt ends with when they did not change (the staging was
/// lost without a structural change, which is never retried).
#[derive(Debug)]
pub struct Stale<E> {
    /// The screen's structural inputs now.
    pub screen: ScreenKey,
    /// What ends the run when nothing structural changed.
    pub lost: E,
}
