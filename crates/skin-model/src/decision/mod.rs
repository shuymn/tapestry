//! The decision screen's runtime: its timers and text lines. The player
//! fills its strings from the chosen chart's labels
//! ([`Skin::update_decision`](crate::Skin::update_decision)) and builds a
//! frame from it ([`Skin::build_decision`](crate::Skin::build_decision)).
//!
//! The decision screen may allocate (design.md "Play Lifecycle", the
//! decision screen): its strings grow as the result's do, at most twice
//! [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) each; a build allocates
//! nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod schema;

use bms_model::TimeUs;

use self::schema::timer;
use crate::expr::{State, TIMER_OFF};
use crate::result::Strings;
use crate::result::view::ResultLabels;
use crate::widget::Starts;
use crate::widget::glyphs::{BitmapLines, GlyphLine};
use crate::widget::text::{ChartText, TextTable};

/// The starts of the decision screen's built-in timers this frame, each
/// `None` while it is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionTimers {
    /// The choice: the screen's start.
    pub screen: Option<TimeUs>,
    /// The fade-out's start: the choice plus the declared scene time.
    pub fadeout: Option<TimeUs>,
    /// The input declaration's end: the choice plus the declared input
    /// time, on from that instant.
    pub input: Option<TimeUs>,
}

impl DecisionTimers {
    /// The timers at `now` of a screen chosen at `choice` with `timing`:
    /// the screen's from the choice, the fade-out's from the choice plus
    /// `scene` once `now` reaches it, and the input's from the choice plus
    /// `input` once `now` reaches it. Allocates nothing.
    #[must_use]
    pub const fn at(choice: TimeUs, timing: &DecisionTiming, now: TimeUs) -> Self {
        let fadeout = TimeUs(choice.0.saturating_add(timing.scene.0));
        let input = TimeUs(choice.0.saturating_add(timing.input.0));
        Self {
            screen: Some(choice),
            fadeout: if now.0 >= fadeout.0 {
                Some(fadeout)
            } else {
                None
            },
            input: if now.0 >= input.0 { Some(input) } else { None },
        }
    }
}

/// The longest scene, and the longest fade-out, a decision skin's
/// declaration is honoured for: 10 s each, so the screen lasts at most
/// 20 s (a provisional interaction limit; design.md "Play Lifecycle", the
/// decision screen). A longer declaration is clamped to it.
pub const MAX_DECISION_PHASE: TimeUs = TimeUs(10_000_000);

/// What a decision skin declares for its screen's timing, in µs: the
/// source's `scene`, `fadeout` and `input`
/// (`skin/json/JsonSkin.java:14-16`), each 0 when not declared. The
/// fade-out starts at `scene` from the choice and the screen ends at
/// `scene + fadeout`; `input` starts the input timer and changes nothing
/// else (design.md "Play Lifecycle", the decision screen).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionTiming {
    /// From the choice to the fade-out's start.
    pub scene: TimeUs,
    /// How long the fade-out plays before the screen ends.
    pub fadeout: TimeUs,
    /// The declared input-disabled time.
    pub input: TimeUs,
}

/// What one decision frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct DecisionFrame<'a> {
    /// The frame's time: timers age on it.
    pub now: TimeUs,
    /// The render target's size in pixels.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a crate::layout::Layout,
    pub timers: DecisionTimers,
}

/// Why a decision call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecisionError {
    /// The skin is not a decision skin, or the state is not its.
    #[error("the skin or the state is not a decision skin's")]
    Kind,
}

/// The chosen chart's strings a decision preparation lays its lines out
/// from, owned, so a worker thread can lay them out while the labels'
/// owner goes on ([`Skin::decision_text_requests_for`],
/// [`Skin::decision_bitmap_lines`]): each label as
/// [`update_decision`](crate::Skin::update_decision) keeps it.
///
/// [`Skin::decision_text_requests_for`]: crate::Skin::decision_text_requests_for
/// [`Skin::decision_bitmap_lines`]: crate::Skin::decision_bitmap_lines
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecisionStrings(Strings);

impl DecisionStrings {
    /// The strings of `labels`. Preparation work: it allocates.
    #[must_use]
    pub fn new(labels: &ResultLabels) -> Self {
        let mut s = Strings::default();
        s.set(labels);
        Self(s)
    }

    pub(crate) fn chart(&self) -> ChartText<'_> {
        self.0.chart()
    }

    /// The bytes the strings hold.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.0.capacity()
    }
}

/// A decision frame's labels, borrowed from their owner for the build
/// ([`Skin::build_decision_with`](crate::Skin::build_decision_with)): the
/// rasterized lines and the bitmap lines, one per text slot each.
#[derive(Clone, Copy, Debug)]
pub struct DecisionLabels<'a> {
    /// The rasterized lines.
    pub text: &'a TextTable,
    /// The lines in bitmap fonts.
    pub bitmap: &'a BitmapLines,
}

/// The decision screen's state as of its last labels and build.
/// [`Skin::prepare_decision`](crate::Skin::prepare_decision) makes a fresh
/// one; a state another skin made, or one a skin of another kind made,
/// draws nothing.
#[derive(Clone, Debug)]
pub struct DecisionScreen {
    /// The identity of the skin that made it: a state of another kind's
    /// skin carries that skin's, which no decision skin accepts.
    pub(crate) skin: u64,
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    pub(crate) strings: Strings,
    pub(crate) text: TextTable,
    pub(crate) bitmap: Box<[Option<GlyphLine>]>,
}

impl DecisionScreen {
    /// A fresh state with `whens` `when` timers and `text_slots` text
    /// slots.
    pub(crate) fn new(skin: u64, whens: usize, text_slots: usize) -> Self {
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Self {
            skin,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            strings: Strings::default(),
            text: TextTable::new(vec![None; text_slots]),
            bitmap: vec![None; text_slots].into_boxed_slice(),
        }
    }

    /// The state a skin of another kind gives: its built-in timers off,
    /// no `when` timer and no text slot. No decision skin accepts it, since
    /// it carries the other skin's identity.
    pub(crate) fn inert(skin: u64) -> Self {
        Self::new(skin, 0, 0)
    }

    /// The timer readings as of the last build; there is no field.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &[],
            timers: &self.ms,
        }
    }

    pub(crate) fn starts(&self) -> Starts<'_> {
        Starts(&self.starts)
    }

    /// The timer slots: built-in, then `when`.
    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Fills this frame's timers: the built-in ones from `timers`, then
    /// each `when` timer. Allocates nothing.
    pub(crate) fn fill(
        &mut self,
        now: TimeUs,
        timers: &DecisionTimers,
        whens: &[crate::expr::Program],
    ) {
        for (start, at) in self
            .starts
            .iter_mut()
            .zip([timers.screen, timers.fadeout, timers.input])
        {
            *start = at;
        }
        crate::play::timers::read_screen_timers(
            usize::from(timer::BUILTIN),
            now,
            &[],
            &mut self.starts,
            &mut self.ms,
            whens,
            |_| true,
        );
    }
}
