//! The play adjustments' reducer (design.md "Play Adjustments"): one writer
//! of the hi-speed, the lane covers and the target green number, fed the
//! adjustment controls' observations in receive order and publishing one
//! complete [`PresentationSnapshot`] per drain.
//!
//! - **Dispatch**: START and SELECT are modifiers; under them the scratch,
//!   the white and black lane keys and the auxiliary keys adjust, F1 and F2
//!   step the speed in every state but both held, and pressing both only
//!   switches the LIFT/HIDDEN+ target.
//! - **Contexts**: a modifier change, a phase or READY generation, the
//!   display offset, the tempo becoming known, a target change, an
//!   ownership change ([`Event::Ownership`]) and a loss or resynchronization
//!   each end a dispatch context. A control held across the end of its
//!   context takes no further action until released and pressed afresh,
//!   and an axis drops its remainder.
//! - **Transactions**: the requests at one receive time in one context form
//!   a group, closed by the first observation at a later time or by the end
//!   of the context; opposite requests in it cancel, the rest apply as
//!   speed, then target green, then cover, with a single solve at the end.
//!   How the drains split the observations changes nothing.
//! - **Repeats**: a held digital scratch acts on its press and then every
//!   [`REPEAT_US`] on the producer's ticks ([`Event::Tick`]): a tick that
//!   passed several deadlines acts once, so a stalled producer leaves no
//!   backlog, and every tick produced is processed.
//!
//! Tempos come from the caller ([`TempoMap`]); the reducer applies the
//! practice speed once. It allocates nothing: its state is fixed-size.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Speed, TimeUs};

use crate::hispeed::{
    CoverKind, GREEN_TARGETS, MAX_COVER, PlayOptions, Ratio, References, SpeedMode,
    Unrepresentable, solve, step_hispeed,
};
use crate::tuning::OFFSET_LIMIT_US;

/// The most physical controls the reducer tracks.
pub const MAX_CONTROLS: usize = 128;
/// Two START activations closer than this toggle SUDDEN+, µs.
pub const DOUBLE_PRESS_US: i64 = 500_000;
/// A held digital scratch acts again after this, µs.
pub const REPEAT_US: i64 = 50_000;
/// A digital scratch held this long in one direction steps a cover by
/// [`FAST_COVER_STEP`], µs.
pub const ACCELERATE_US: i64 = 500_000;
/// A digital scratch's cover step once accelerated, ‰.
pub const FAST_COVER_STEP: i64 = 10;
/// An auxiliary key's cover step, ‰.
pub const AUX_COVER_STEP: i64 = 10;

/// A physical control, numbered below [`MAX_CONTROLS`] by the bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ControlId(pub u8);

/// What a physical control does for the adjustments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The START modifier.
    Start,
    /// The SELECT modifier.
    Select,
    /// The auxiliary negative key.
    AuxNegative,
    /// The auxiliary positive key.
    AuxPositive,
    /// The scratch's positive digital direction.
    ScratchPositive,
    /// The scratch's negative digital direction.
    ScratchNegative,
    /// A white lane key (1, 3, 5, 7).
    White,
    /// A black lane key (2, 4, 6).
    Black,
    /// F1: slower.
    Slower,
    /// F2: faster.
    Faster,
}

/// Each control's role, fixed for an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRoles([Option<Role>; MAX_CONTROLS]);

impl Default for ControlRoles {
    fn default() -> Self {
        Self([None; MAX_CONTROLS])
    }
}

impl ControlRoles {
    /// Gives `control` the role `role`; `false` when it is out of range.
    pub fn set(&mut self, control: ControlId, role: Role) -> bool {
        match self.0.get_mut(usize::from(control.0)) {
            Some(slot) => {
                *slot = Some(role);
                true
            }
            None => false,
        }
    }

    /// The role of `control`.
    #[must_use]
    pub fn get(&self, control: ControlId) -> Option<Role> {
        self.0.get(usize::from(control.0)).copied().flatten()
    }
}

/// The controls verified held, one bit each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeldSet(pub u128);

impl HeldSet {
    /// Whether `control` is in the set.
    #[must_use]
    pub const fn contains(self, control: ControlId) -> bool {
        (control.0 as usize) < MAX_CONTROLS && self.0 & (1 << control.0) != 0
    }

    /// The set with `control` in it.
    #[must_use]
    pub const fn with(self, control: ControlId) -> Self {
        if (control.0 as usize) < MAX_CONTROLS {
            Self(self.0 | (1 << control.0))
        } else {
            self
        }
    }
}

/// How many axis counts make how many adjustment steps: a wrapping axis
/// steps at 9/2000 of a revolution, any other at a positive quantum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisQuantum {
    counts: u64,
    steps: u64,
}

impl AxisQuantum {
    /// A wrapping axis of `range` counts per revolution.
    #[must_use]
    pub fn wrapping(range: u32) -> Option<Self> {
        (range > 0).then(|| Self {
            counts: 9 * u64::from(range),
            steps: 2000,
        })
    }

    /// One step every `counts` counts.
    #[must_use]
    pub fn per_step(counts: u32) -> Option<Self> {
        (counts > 0).then_some(Self {
            counts: u64::from(counts),
            steps: 1,
        })
    }
}

/// Where the presentation head's base is (design.md "Play Adjustments",
/// tempo): the head is the base plus the display offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    /// Frozen at `opens_at()`: loading and preparing, or waiting after a
    /// cancelled READY.
    Frozen {
        /// The chart time the attempt opens at.
        opens_at: TimeUs,
    },
    /// The session clock: session time `chart` at receive time `at`, both
    /// real time, so it advances one µs per µs at any practice speed.
    Clock {
        /// A receive time.
        at: TimeUs,
        /// The chart time at `at`.
        chart: TimeUs,
    },
}

/// One observation's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A control went down.
    Press(ControlId),
    /// A control went up.
    Release(ControlId),
    /// The keyboard repeated a held key (F1 and F2 act on it).
    Repeat(ControlId),
    /// An axis moved by `delta` counts, its sign resolved.
    Axis {
        /// The axis.
        control: ControlId,
        /// Signed counts since its last report.
        delta: i64,
        /// Its counts per step.
        quantum: AxisQuantum,
    },
    /// The producer's time passed: held scratches repeat.
    Tick,
    /// The display offset changed to this, µs.
    DisplayOffset(i64),
    /// The scene, the result's input gate or another owner changed.
    Ownership,
    /// The lifecycle phase: the head's base and the READY generation.
    Phase {
        /// The head's base.
        head: Head,
        /// The READY generation.
        generation: u32,
    },
    /// The chart's tempo became known, with the practice speed.
    Prepare {
        /// The chart's reference tempos.
        references: References,
        /// The practice speed.
        speed: Speed,
    },
    /// Observations were lost, or focus was: nothing acts until
    /// [`Event::Resync`].
    Lost,
    /// The controls held after a loss; each stays without action until
    /// released. While `verified` is false the held set may miss a control,
    /// so the loss stands: nothing acts until a verified one.
    Resync {
        /// The controls held.
        held: HeldSet,
        /// Whether `held` is known to be complete.
        verified: bool,
    },
}

/// An observation: an event at a receive time, with its packet sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The receive time.
    pub time: TimeUs,
    /// The packet sequence, ordering observations at one time.
    pub seq: u64,
    /// What happened.
    pub event: Event,
}

/// The chart's tempo at a presentation head. The head is a session time,
/// real time with the practice speed already in it (design.md "Practice
/// Speed Changes"); the tempo is the chart's own 1× BPM, which the reducer
/// scales by the speed once.
pub trait TempoMap {
    /// The tempo at the session time `head`; `None` when unknown.
    fn bpm_at(&self, head: TimeUs) -> Option<Ratio>;
}

/// No tempo known.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTempo;

impl TempoMap for NoTempo {
    fn bpm_at(&self, _: TimeUs) -> Option<Ratio> {
        None
    }
}

impl PresentationSnapshot {
    /// The record's starting tuning: these options, the multiplier `H`
    /// and the cover target ([`replay::StartTuning`]).
    #[must_use]
    pub fn start_tuning(&self) -> replay::StartTuning {
        self.options.start_tuning(self.hispeed, self.target)
    }
}

/// What the reducer publishes: everything a frame and the readouts need,
/// from one drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentationSnapshot {
    /// The settings and covers, with every accepted change.
    pub options: PlayOptions,
    /// The active multiplier `H`, once known.
    pub hispeed: Option<Ratio>,
    /// The cover the controls adjust.
    pub target: Option<CoverKind>,
    /// The display offset, µs.
    pub display_offset_us: i64,
    /// Focused START or SELECT held (`cover.adjusting`).
    pub adjusting: bool,
    /// The READY generation.
    pub generation: u32,
    /// The drain that published it, counting from 1.
    pub sequence: u64,
    /// The presentation epoch: which attempt's chart `hispeed` was solved
    /// for, 0 before any. A frame of another attempt reads no gain from it.
    pub epoch: u64,
}

/// Why [`Adjustment::attach`] left a Floating `H` unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    /// The chart's reference tempos are unavailable: an exact tempo does
    /// not fit.
    #[error("the chart's reference tempos are unavailable")]
    NoReferences,
    /// The solve's result does not fit.
    #[error("the hi-speed for the chart's reference tempo does not fit")]
    Unrepresentable,
}

/// One item of the augmented input stream (design.md "Play Adjustments",
/// the display offset): an observation, or a display-offset change cut in
/// where the drain that first read it took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Augmented {
    /// An observation.
    Obs(Observation),
    /// The display offset changed to this, µs.
    Cut(i64),
}

/// Counts of what the reducer refused, outside counted processing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Transactions left unapplied because a value did not fit.
    pub unrepresentable: u64,
    /// Observations before the previous one, taken at its time.
    pub out_of_order: u64,
    /// Observations of a control without a role or out of range.
    pub unknown_controls: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Up,
    Held {
        role: Role,
        /// The context its press was accepted in.
        context: u64,
        /// Opposite digital directions held together.
        suppressed: bool,
        pressed: TimeUs,
        /// The next repeat, for a digital scratch.
        next: TimeUs,
        /// A digital scratch's request in the open group, with its time; it
        /// was made in `context`. An opposite press at that time in that
        /// context takes it back. Once its context ends, the group it was
        /// in has applied, and nothing is taken back.
        pending: Option<(TimeUs, Request)>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AxisState {
    context: u64,
    remainder: i128,
}

/// One time's requests in one context, before they apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Group {
    open: bool,
    time: TimeUs,
    /// Mode steps: + faster. Every sum is wide enough that requests within
    /// `i64` cancel exactly; only the resulting setting is clamped.
    coarse: i128,
    /// Hundredths: + faster.
    fine: i128,
    /// Green units: + longer.
    green: i128,
    /// `Cover(δ)`, ‰.
    cover: i128,
}

/// One request, signed toward faster, a longer green number or `Cover(δ)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Coarse(i128),
    Fine(i128),
    Green(i128),
    Cover(i128),
}

impl Request {
    /// The two requests together; one control's requests at one time are
    /// of one kind, its context's.
    const fn plus(self, other: Self) -> Self {
        match (self, other) {
            (Self::Coarse(a), Self::Coarse(b)) => Self::Coarse(a.saturating_add(b)),
            (Self::Fine(a), Self::Fine(b)) => Self::Fine(a.saturating_add(b)),
            (Self::Green(a), Self::Green(b)) => Self::Green(a.saturating_add(b)),
            (Self::Cover(a), Self::Cover(b)) => Self::Cover(a.saturating_add(b)),
            (_, other) => other,
        }
    }
}

impl Group {
    /// Takes `request` back out of the group open at `time`.
    const fn retract(&mut self, time: TimeUs, request: Request) {
        if !self.open || self.time.0 != time.0 {
            return;
        }
        let (field, by) = match request {
            Request::Coarse(by) => (&mut self.coarse, by),
            Request::Fine(by) => (&mut self.fine, by),
            Request::Green(by) => (&mut self.green, by),
            Request::Cover(by) => (&mut self.cover, by),
        };
        *field = field.saturating_sub(by);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mods {
    Neither,
    Start,
    Select,
    Both,
}

/// The adjustments' state for an attempt and its READY candidates.
#[derive(Clone, Debug)]
pub struct Adjustment {
    options: PlayOptions,
    hispeed: Option<Ratio>,
    selected: CoverKind,
    roles: ControlRoles,
    holds: [Hold; MAX_CONTROLS],
    axes: [AxisState; MAX_CONTROLS],
    starts: u32,
    selects: u32,
    pending_start: Option<TimeUs>,
    context: u64,
    lost: bool,
    head: Head,
    generation: u32,
    display_offset_us: i64,
    /// The practice speed, once [`Event::Prepare`] makes the tempo known.
    speed: Option<Speed>,
    group: Group,
    last: Option<(TimeUs, u64)>,
    diagnostics: Diagnostics,
    published: u64,
    epoch: u64,
    /// The state the last opening started with, taken once its phase was
    /// reduced ([`Self::opened`]).
    opened: Option<PresentationSnapshot>,
}

impl Adjustment {
    /// The reducer for an attempt with `options` and the bindings' `roles`,
    /// its head frozen at `opens_at` and the display offset
    /// `display_offset_us`. A Classic multiplier is known at once; a
    /// Floating one once [`Event::Prepare`] gives the tempo.
    #[must_use]
    pub fn new(
        options: PlayOptions,
        roles: ControlRoles,
        opens_at: TimeUs,
        display_offset_us: i64,
    ) -> Self {
        let mut a = Self {
            options,
            hispeed: None,
            selected: CoverKind::Lift,
            roles,
            holds: [Hold::Up; MAX_CONTROLS],
            axes: [AxisState::default(); MAX_CONTROLS],
            starts: 0,
            selects: 0,
            pending_start: None,
            context: 0,
            lost: false,
            head: Head::Frozen { opens_at },
            generation: 0,
            display_offset_us: 0,
            speed: None,
            group: Group::default(),
            last: None,
            diagnostics: Diagnostics::default(),
            published: 0,
            epoch: 0,
            opened: None,
        };
        a.hispeed = match options.hispeed().mode {
            SpeedMode::Classic => Some(Ratio::from_hundredths(
                options.hispeed().classic_hispeed_x100,
            )),
            SpeedMode::Floating => None,
        };
        a.display_offset_us = clamp_offset(display_offset_us);
        a
    }

    /// The entry of a new attempt (a first play, a retry, the next chart
    /// or round), before its chart is known to the reducer: the open
    /// group closes under the head and tempo in force; the holds carry
    /// over suppressed (a control held across it acts only after a release
    /// and a fresh press, while a held START or SELECT still counts as
    /// held), and the target selection, the double-press history, the
    /// axes' remainders and the repeat deadlines start afresh; the chart
    /// is forgotten: Floating's `H` is unknown and no tempo is known until
    /// [`attach`](Self::attach), so an edit until then solves nothing and
    /// is never rolled back for want of a tempo. The settings, Classic's
    /// multiplier, the display offset and a loss stay. The head freezes at
    /// `opens_at` where the coming attempt's opening is known. Allocates
    /// nothing.
    pub fn enter_attempt<T: TempoMap>(&mut self, opens_at: Option<TimeUs>, tempo: &T) {
        self.close(tempo);
        let context = self.context;
        for hold in &mut self.holds {
            if let Hold::Held {
                context: c,
                suppressed,
                next,
                pending,
                ..
            } = hold
            {
                *c = context.wrapping_sub(1);
                *suppressed = true;
                *next = TimeUs(i64::MAX);
                *pending = None;
            }
        }
        self.context = context.wrapping_add(1);
        self.selected = CoverKind::Lift;
        self.axes = [AxisState::default(); MAX_CONTROLS];
        self.pending_start = None;
        if self.options.hispeed().mode == SpeedMode::Floating {
            self.hispeed = None;
        }
        self.speed = None;
        if let Some(opens_at) = opens_at {
            self.head = Head::Frozen { opens_at };
        }
    }

    /// The attempt's chart becomes known: the head frozen at `opens_at`,
    /// the practice `speed`, and in Floating one solve from the reference
    /// tempo of `references` with the target and the covers in force.
    ///
    /// # Errors
    ///
    /// [`AttachError::NoReferences`] when Floating has no references, and
    /// [`AttachError::Unrepresentable`] when its solve does not fit: `H`
    /// stays unknown, and the attempt cannot be played in Floating.
    pub fn attach<T: TempoMap>(
        &mut self,
        opens_at: TimeUs,
        references: Option<References>,
        speed: Speed,
        tempo: &T,
    ) -> Result<(), AttachError> {
        self.end_context(tempo);
        self.head = Head::Frozen { opens_at };
        self.prepare(references, speed, tempo)
    }

    /// The tempo becomes known at the practice `speed`: in Floating, one
    /// solve from `references` ([`Event::Prepare`]).
    fn prepare<T: TempoMap>(
        &mut self,
        references: Option<References>,
        speed: Speed,
        tempo: &T,
    ) -> Result<(), AttachError> {
        self.end_context(tempo);
        self.speed = Some(speed);
        if self.options.hispeed().mode != SpeedMode::Floating {
            return Ok(());
        }
        let Some(references) = references else {
            self.hispeed = None;
            return Err(AttachError::NoReferences);
        };
        let reference = references.get(self.options.hispeed().reference);
        match playback(reference, speed).and_then(|b| self.solve_at(b)) {
            Ok(h) => {
                self.hispeed = Some(h);
                Ok(())
            }
            Err(Unrepresentable) => {
                self.diagnostics.unrepresentable += 1;
                self.hispeed = None;
                Err(AttachError::Unrepresentable)
            }
        }
    }

    /// The latest observation taken: its receive time and sequence.
    #[must_use]
    pub const fn last(&self) -> Option<(TimeUs, u64)> {
        self.last
    }

    /// Marks what is published from now on as of presentation epoch
    /// `epoch` ([`PresentationSnapshot::epoch`]).
    pub const fn set_epoch(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    /// The settings with every accepted change: what is saved.
    #[must_use]
    pub const fn options(&self) -> PlayOptions {
        self.options
    }

    /// What was refused so far.
    #[must_use]
    pub const fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
    }

    /// Whether START or SELECT is held as the observations tell, or their
    /// state is unknown after a loss. Not the start gate's latch, which the
    /// input thread keeps.
    #[must_use]
    pub const fn modifiers_held(&self) -> bool {
        self.lost || self.starts > 0 || self.selects > 0
    }

    /// The state the last opening started with (design.md "Input Log
    /// Format", the starting tuning): taken where its phase is reduced in
    /// the ordered stream, so every adjustment ordered before it is in it
    /// and none after; replaced by the next opening, a rearm's included.
    #[must_use]
    pub const fn opened(&self) -> Option<&PresentationSnapshot> {
        self.opened.as_ref()
    }

    /// The state to publish. A group still open at the latest time is not
    /// in it until a later observation closes it.
    #[must_use]
    pub fn snapshot(&self) -> PresentationSnapshot {
        PresentationSnapshot {
            options: self.options,
            hispeed: self.hispeed,
            target: self.target(),
            display_offset_us: self.display_offset_us,
            adjusting: !self.lost && (self.starts > 0 || self.selects > 0),
            generation: self.generation,
            sequence: self.published,
            epoch: self.epoch,
        }
    }

    /// Takes a drain's observations, in receive order, then publishes one
    /// snapshot through `writer`.
    pub fn drain<T: TempoMap>(
        &mut self,
        observations: impl IntoIterator<Item = Observation>,
        tempo: &T,
        writer: &mut PresentationWriter,
    ) {
        self.feed(observations.into_iter().map(Augmented::Obs), tempo, writer);
    }

    /// Takes a drain's items of the augmented stream in order, a cut as
    /// [`Event::DisplayOffset`] at the latest observation's stamp, then
    /// publishes one snapshot through `writer`.
    pub fn feed<T: TempoMap>(
        &mut self,
        items: impl IntoIterator<Item = Augmented>,
        tempo: &T,
        writer: &mut PresentationWriter,
    ) {
        for item in items {
            match item {
                Augmented::Obs(o) => self.observe(o, tempo),
                Augmented::Cut(us) => self.cut(us, tempo),
            }
        }
        self.published = self.published.wrapping_add(1);
        writer.0.write(self.snapshot());
    }

    /// The display offset changed to `us`, taken at the latest
    /// observation's stamp, so it is never out of order.
    pub fn cut<T: TempoMap>(&mut self, us: i64, tempo: &T) {
        let (time, seq) = self.last.unwrap_or((TimeUs(i64::MIN), 0));
        self.observe(
            Observation {
                time,
                seq,
                event: Event::DisplayOffset(us),
            },
            tempo,
        );
    }

    /// Takes one observation.
    pub fn observe<T: TempoMap>(&mut self, observation: Observation, tempo: &T) {
        let Observation {
            mut time,
            seq,
            event,
        } = observation;
        if let Some(last) = self.last
            && (time, seq) < last
        {
            self.diagnostics.out_of_order += 1;
            time = last.0;
        }
        self.last = Some((time, seq));
        if self.group.open && self.group.time != time {
            self.close(tempo);
        }
        match event {
            Event::Press(c) => self.press(c, time, tempo),
            Event::Release(c) => self.release(c, tempo),
            Event::Repeat(c) => self.repeat(c, time),
            Event::Axis {
                control,
                delta,
                quantum,
            } => self.axis(control, delta, quantum, time),
            Event::Tick => self.tick(time),
            Event::DisplayOffset(us) => {
                self.end_context(tempo);
                self.display_offset_us = clamp_offset(us);
            }
            Event::Ownership => self.end_context(tempo),
            Event::Phase { head, generation } => {
                // A new READY generation ends the dispatch context. A cancel
                // (the head frozen, the generation kept) only closes the open
                // group, under the candidate's clock, before the head
                // freezes: a gesture accepted in or after the batch whose
                // hold caused it, such as a scratch held with START, keeps
                // its context and goes on repeating. The tap sends the cancel
                // after that batch, so the batch's requests are made under
                // the candidate's clock too.
                if generation == self.generation {
                    self.close(tempo);
                } else {
                    self.end_context(tempo);
                }
                self.head = head;
                self.generation = generation;
                // An opening (the head on the clock): its starting tuning,
                // with every adjustment ordered before it reduced and its
                // open group closed by the new generation's context end.
                if matches!(head, Head::Clock { .. }) {
                    self.opened = Some(self.snapshot());
                }
            }
            Event::Prepare { references, speed } => {
                let _ = self.prepare(Some(references), speed, tempo);
            }
            Event::Lost => {
                self.end_context(tempo);
                self.lost = true;
                self.pending_start = None;
            }
            Event::Resync { held, verified } => {
                self.end_context(tempo);
                self.resync(held);
                self.lost = !verified;
            }
        }
    }

    fn mods(&self) -> Mods {
        match (self.starts > 0, self.selects > 0) {
            (false, false) => Mods::Neither,
            (true, false) => Mods::Start,
            (false, true) => Mods::Select,
            (true, true) => Mods::Both,
        }
    }

    /// The cover the controls adjust: SUDDEN+ when on, else the selected
    /// one of LIFT and HIDDEN+ when on, else the one of them that is on.
    fn target(&self) -> Option<CoverKind> {
        let c = self.options.covers();
        if c.sudden.on {
            return Some(CoverKind::Sudden);
        }
        if c.get(self.selected).on {
            return Some(self.selected);
        }
        [CoverKind::Lift, CoverKind::Hidden]
            .into_iter()
            .find(|&k| c.get(k).on)
    }

    fn end_context<T: TempoMap>(&mut self, tempo: &T) {
        self.close(tempo);
        self.context = self.context.wrapping_add(1);
    }

    fn role(&mut self, control: ControlId) -> Option<Role> {
        let role = self.roles.get(control);
        if role.is_none() {
            self.diagnostics.unknown_controls += 1;
        }
        role
    }

    fn press<T: TempoMap>(&mut self, control: ControlId, time: TimeUs, tempo: &T) {
        let Some(role) = self.role(control) else {
            return;
        };
        let context = self.context;
        let Some(hold) = self.holds.get_mut(usize::from(control.0)) else {
            return;
        };
        if *hold != Hold::Up || (self.lost && !matches!(role, Role::Start | Role::Select)) {
            // A press without a release, or one the loss left unverified.
            return;
        }
        *hold = Hold::Held {
            role,
            context,
            suppressed: false,
            pressed: time,
            next: TimeUs(time.0.saturating_add(REPEAT_US)),
            pending: None,
        };
        match role {
            Role::Start | Role::Select => {
                let before = self.mods();
                if role == Role::Start {
                    self.starts += 1;
                } else {
                    self.selects += 1;
                }
                if !self.lost {
                    self.modifiers_changed(before, time, tempo);
                }
            }
            Role::ScratchPositive | Role::ScratchNegative => {
                let opposite = if role == Role::ScratchPositive {
                    Role::ScratchNegative
                } else {
                    Role::ScratchPositive
                };
                let mut clash = false;
                let group = &mut self.group;
                for h in &mut self.holds {
                    if let Hold::Held {
                        role: r,
                        context: c,
                        suppressed,
                        pending,
                        ..
                    } = h
                        && *r == opposite
                    {
                        // What it asked at this time, press or repeat, is
                        // taken back: neither direction acts at this time.
                        if let Some((at, request)) = pending.take()
                            && at == time
                            && *c == context
                        {
                            group.retract(time, request);
                        }
                        *suppressed = true;
                        clash = true;
                    }
                }
                if clash {
                    if let Some(Hold::Held { suppressed, .. }) =
                        self.holds.get_mut(usize::from(control.0))
                    {
                        *suppressed = true;
                    }
                } else {
                    let request = self.act(role, 1, time);
                    self.note(control, time, request);
                }
            }
            _ => {
                self.act(role, AUX_COVER_STEP, time);
            }
        }
    }

    fn release<T: TempoMap>(&mut self, control: ControlId, tempo: &T) {
        let Some(hold) = self.holds.get_mut(usize::from(control.0)) else {
            self.diagnostics.unknown_controls += 1;
            return;
        };
        let Hold::Held { role, .. } = *hold else {
            return;
        };
        *hold = Hold::Up;
        let before = self.mods();
        match role {
            Role::Start => self.starts = self.starts.saturating_sub(1),
            Role::Select => self.selects = self.selects.saturating_sub(1),
            _ => return,
        }
        if self.mods() != before {
            self.end_context(tempo);
        }
    }

    /// A modifier went down, from `before`: the context ends, a START-only
    /// activation counts toward the double press, and entering both held
    /// switches the LIFT/HIDDEN+ target and nothing else.
    fn modifiers_changed<T: TempoMap>(&mut self, before: Mods, time: TimeUs, tempo: &T) {
        let after = self.mods();
        if after == before {
            return;
        }
        self.end_context(tempo);
        match (before, after) {
            (Mods::Neither, Mods::Start) => match self.pending_start {
                Some(first) if time.0.saturating_sub(first.0) < DOUBLE_PRESS_US => {
                    self.pending_start = None;
                    self.toggle_sudden(time, tempo);
                }
                _ => self.pending_start = Some(time),
            },
            (_, Mods::Both) => {
                self.pending_start = None;
                let c = self.options.covers();
                if c.lift.on && c.hidden.on {
                    self.selected = match self.selected {
                        CoverKind::Lift => CoverKind::Hidden,
                        _ => CoverKind::Lift,
                    };
                }
            }
            _ => {}
        }
    }

    /// Toggles SUDDEN+ as one transaction: shown again, Floating solves to
    /// the saved target at the current tempo, and Classic keeps `H`. A solve
    /// that does not fit leaves the switch and `H` as they were.
    fn toggle_sudden<T: TempoMap>(&mut self, time: TimeUs, tempo: &T) {
        let mut options = self.options;
        let shown = {
            let (_, covers) = options.parts_mut();
            covers.sudden.on = !covers.sudden.on;
            covers.sudden.on
        };
        let mut hispeed = self.hispeed;
        if shown && options.hispeed().mode == SpeedMode::Floating {
            let solved = self.bpm_at(time, tempo).and_then(|b| {
                b.map(|b| solve(b, options.hispeed().green_target, &options.covers()))
                    .transpose()
            });
            match solved {
                Ok(Some(h)) => hispeed = Some(h),
                Ok(None) => {}
                Err(Unrepresentable) => {
                    self.diagnostics.unrepresentable += 1;
                    return;
                }
            }
        }
        self.options = options;
        self.hispeed = hispeed;
        // The target changed.
        self.context = self.context.wrapping_add(1);
    }

    fn repeat(&mut self, control: ControlId, time: TimeUs) {
        if self.lost {
            return;
        }
        if let Some(&Hold::Held {
            role: role @ (Role::Slower | Role::Faster),
            context,
            ..
        }) = self.holds.get(usize::from(control.0))
            && context == self.context
        {
            self.act(role, 1, time);
        }
    }

    fn tick(&mut self, time: TimeUs) {
        if self.lost {
            return;
        }
        let context = self.context;
        // A step only adds to the group and to its own hold's request, so
        // each hold steps as it is reached.
        for i in 0..self.holds.len() {
            let Ok(control) = u8::try_from(i).map(ControlId) else {
                break;
            };
            let Some(Hold::Held {
                role: role @ (Role::ScratchPositive | Role::ScratchNegative),
                context: c,
                suppressed: false,
                pressed,
                next,
                ..
            }) = self.holds.get_mut(i)
            else {
                continue;
            };
            if *c != context || time < *next {
                continue;
            }
            let role = *role;
            // One step however many deadlines passed.
            let missed = (time.0 - next.0) / REPEAT_US + 1;
            next.0 = next.0.saturating_add(missed.saturating_mul(REPEAT_US));
            let q = if time.0.saturating_sub(pressed.0) >= ACCELERATE_US {
                FAST_COVER_STEP
            } else {
                1
            };
            let request = self.act(role, q, time);
            self.note(control, time, request);
        }
    }

    /// Keeps a digital scratch's `request` at `time` with its control, to
    /// take back if an opposite press comes at the same time.
    fn note(&mut self, control: ControlId, time: TimeUs, request: Option<Request>) {
        let Some(request) = request else {
            return;
        };
        if let Some(Hold::Held { pending, .. }) = self.holds.get_mut(usize::from(control.0)) {
            // Only a hold in the current context acts, so an earlier
            // request at this time was made in this context too.
            *pending = Some(match *pending {
                Some((at, earlier)) if at == time => (time, earlier.plus(request)),
                _ => (time, request),
            });
        }
    }

    fn axis(&mut self, control: ControlId, delta: i64, quantum: AxisQuantum, time: TimeUs) {
        if self.lost {
            return;
        }
        let context = self.context;
        let Some(state) = self.axes.get_mut(usize::from(control.0)) else {
            self.diagnostics.unknown_controls += 1;
            return;
        };
        if state.context != context {
            // A new baseline: earlier travel is never reinterpreted.
            *state = AxisState {
                context,
                remainder: 0,
            };
            return;
        }
        let counts = i128::from(quantum.counts);
        state.remainder = state
            .remainder
            .saturating_add(i128::from(delta) * i128::from(quantum.steps));
        let n = state.remainder / counts;
        state.remainder -= n * counts;
        if n != 0 {
            self.act_steps(Role::ScratchPositive, n, 1, time);
        }
    }

    fn act(&mut self, role: Role, cover_step: i64, time: TimeUs) -> Option<Request> {
        self.act_steps(role, 1, cover_step, time)
    }

    /// `n` steps of `role` under the current modifiers, into the group at
    /// `time`: the request added, if any.
    fn act_steps(&mut self, role: Role, n: i128, cover_step: i64, time: TimeUs) -> Option<Request> {
        let mods = self.mods();
        let floating = self.options.hispeed().mode == SpeedMode::Floating;
        let target = self.target().is_some();
        let sign = match role {
            Role::Black
            | Role::Faster
            | Role::AuxPositive
            | Role::ScratchPositive
            | Role::Start
            | Role::Select => 1,
            Role::White | Role::Slower | Role::AuxNegative | Role::ScratchNegative => -1,
        };
        let request = match (mods, role) {
            (Mods::Both, _) | (_, Role::Start | Role::Select) => return None,
            (_, Role::Slower | Role::Faster) | (Mods::Start, Role::White | Role::Black) => {
                Request::Coarse(n.saturating_mul(sign))
            }
            (Mods::Select, _) if floating => Request::Green(n.saturating_mul(sign)),
            (
                Mods::Start,
                Role::AuxNegative
                | Role::AuxPositive
                | Role::ScratchPositive
                | Role::ScratchNegative,
            ) => {
                if target {
                    Request::Cover(
                        n.saturating_mul(i128::from(cover_step))
                            .saturating_mul(sign),
                    )
                } else {
                    Request::Fine(n.saturating_mul(sign))
                }
            }
            _ => return None,
        };
        let g = &mut self.group;
        if !g.open {
            *g = Group {
                open: true,
                time,
                ..Group::default()
            };
        }
        let (field, by) = match request {
            Request::Coarse(by) => (&mut g.coarse, by),
            Request::Fine(by) => (&mut g.fine, by),
            Request::Green(by) => (&mut g.green, by),
            Request::Cover(by) => (&mut g.cover, by),
        };
        *field = field.saturating_add(by);
        Some(request)
    }

    /// Applies the open group as one transaction: speed, then the target
    /// green number, then the cover, and one solve at the end where the
    /// transitions call for it. A value that does not fit leaves everything
    /// as it was.
    fn close<T: TempoMap>(&mut self, tempo: &T) {
        let group = std::mem::take(&mut self.group);
        if !group.open {
            return;
        }
        let mut options = self.options;
        let mut hispeed = self.hispeed;
        let target = self.target();
        let result = (|| -> Result<(), Unrepresentable> {
            let (h, covers) = options.parts_mut();
            let floating = h.mode == SpeedMode::Floating;
            let mut solve_needed = false;
            if (group.coarse != 0 || group.fine != 0)
                && let Some(current) = hispeed
            {
                let step = i64::from(if floating {
                    h.floating_step_x100
                } else {
                    h.classic_step_x100
                });
                // Past `i64` any step reaches an end already.
                let by = group
                    .coarse
                    .saturating_mul(i128::from(step))
                    .saturating_add(group.fine);
                let by = i64::try_from(by).unwrap_or(if by < 0 { i64::MIN } else { i64::MAX });
                let next = step_hispeed(current, by)?;
                if !floating {
                    h.classic_hispeed_x100 = next.hundredths().ok_or(Unrepresentable)?;
                }
                hispeed = Some(next);
            }
            if group.green != 0 {
                let g = i128::from(h.green_target).saturating_add(group.green);
                let g = g.clamp(i128::from(GREEN_TARGETS.0), i128::from(GREEN_TARGETS.1));
                h.green_target = u32::try_from(g).map_err(|_| Unrepresentable)?;
                solve_needed = true;
            }
            if group.cover != 0
                && let Some(kind) = target
            {
                let cover = covers.get_mut(kind);
                let by = if kind == CoverKind::Sudden {
                    group.cover
                } else {
                    group.cover.saturating_neg()
                };
                let amount = i128::from(cover.amount)
                    .saturating_add(by)
                    .clamp(0, i128::from(MAX_COVER));
                cover.amount = u16::try_from(amount).map_err(|_| Unrepresentable)?;
                solve_needed |= floating && kind != CoverKind::Hidden;
            }
            if solve_needed && let Some(b) = self.bpm_at(group.time, tempo)? {
                hispeed = Some(solve(b, h.green_target, covers)?);
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.options = options;
                self.hispeed = hispeed;
            }
            Err(Unrepresentable) => self.diagnostics.unrepresentable += 1,
        }
    }

    /// The playback tempo at the presentation head for `time`: the phase's
    /// base head plus the display offset, the practice speed applied once.
    ///
    /// `Ok(None)` when no tempo is known; an error when the playback tempo
    /// does not fit, which fails the transaction that needed it.
    fn bpm_at<T: TempoMap>(
        &self,
        time: TimeUs,
        tempo: &T,
    ) -> Result<Option<Ratio>, Unrepresentable> {
        let Some(speed) = self.speed else {
            return Ok(None);
        };
        let base = match self.head {
            Head::Frozen { opens_at } => opens_at.0,
            Head::Clock { at, chart } => chart.0.saturating_add(time.0.saturating_sub(at.0)),
        };
        let head = TimeUs(base.saturating_add(self.display_offset_us));
        let Some(bpm) = tempo.bpm_at(head) else {
            return Ok(None);
        };
        playback(bpm, speed).map(Some)
    }

    fn solve_at(&self, bpm: Ratio) -> Result<Ratio, Unrepresentable> {
        let covers = self.options.covers();
        solve(bpm, self.options.hispeed().green_target, &covers)
    }

    fn resync(&mut self, held: HeldSet) {
        let context = self.context;
        self.starts = 0;
        self.selects = 0;
        for (i, hold) in self.holds.iter_mut().enumerate() {
            let control = ControlId(u8::try_from(i).unwrap_or(u8::MAX));
            if !held.contains(control) {
                *hold = Hold::Up;
                continue;
            }
            let Some(role) = self.roles.get(control) else {
                *hold = Hold::Up;
                continue;
            };
            match role {
                Role::Start => self.starts += 1,
                Role::Select => self.selects += 1,
                _ => {}
            }
            // Held across the loss: no action until released.
            *hold = Hold::Held {
                role,
                context: context.wrapping_sub(1),
                suppressed: true,
                pressed: TimeUs(0),
                next: TimeUs(i64::MAX),
                pending: None,
            };
        }
        self.lost = false;
        self.pending_start = None;
    }
}

/// A display offset held within its limit.
fn clamp_offset(us: i64) -> i64 {
    us.clamp(-OFFSET_LIMIT_US, OFFSET_LIMIT_US)
}

/// The playback tempo: `bpm` at the practice `speed`.
pub(crate) fn playback(bpm: Ratio, speed: Speed) -> Result<Ratio, Unrepresentable> {
    let (k, d) = speed.ratio();
    let speed = Ratio::new(u64::from(k), u64::from(d)).ok_or(Unrepresentable)?;
    bpm.times(speed)
}

/// The reducer's end of the presentation triple buffer.
#[derive(Debug)]
pub struct PresentationWriter(triple_buffer::Input<PresentationSnapshot>);

impl PresentationWriter {
    /// Publishes `snapshot`, as a drain does: what a frame stands in for
    /// the reducer with. Allocates nothing.
    pub fn write(&mut self, snapshot: PresentationSnapshot) {
        self.0.write(snapshot);
    }
}

/// The frame's end of the presentation triple buffer.
#[derive(Debug)]
pub struct PresentationReader(triple_buffer::Output<PresentationSnapshot>);

impl PresentationReader {
    /// The newest snapshot.
    pub fn read(&mut self) -> &PresentationSnapshot {
        self.0.read()
    }
}

/// The presentation triple buffer, prepared before the attempt: writing a
/// snapshot allocates nothing.
#[must_use]
pub fn presentation_link(
    initial: &PresentationSnapshot,
) -> (PresentationWriter, PresentationReader) {
    let (input, output) = triple_buffer::triple_buffer(initial);
    (PresentationWriter(input), PresentationReader(output))
}
