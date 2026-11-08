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
use crate::widget::Starts;
use crate::widget::glyphs::GlyphLine;
use crate::widget::text::TextTable;

/// The starts of the decision screen's built-in timers this frame, each
/// `None` while it is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecisionTimers {
    /// The choice: the screen's start.
    pub screen: Option<TimeUs>,
    /// The fade-out's start: the choice plus the declared scene time.
    pub fadeout: Option<TimeUs>,
}

/// What a decision skin declares for its screen's timing, in µs: the
/// source's `scene`, `fadeout` and `input`
/// (`skin/json/JsonSkin.java:14-16`), each 0 when not declared. The
/// fade-out starts at `scene` from the choice and the screen ends at
/// `scene + fadeout`; `input` is kept and changes nothing (design.md
/// "Play Lifecycle", the decision screen).
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

/// The decision screen's state as of its last labels and build.
/// [`Skin::prepare_decision`](crate::Skin::prepare_decision) makes a fresh
/// one; a state another skin made, or one a skin of another kind made,
/// draws nothing.
#[derive(Clone, Debug)]
pub struct DecisionScreen {
    /// Whether a decision skin made it.
    pub(crate) live: bool,
    /// The identity of the skin that made it.
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
    /// slots; `live` when a decision skin makes it.
    pub(crate) fn new(skin: u64, live: bool, whens: usize, text_slots: usize) -> Self {
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Self {
            live,
            skin,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            strings: Strings::default(),
            text: TextTable::new(vec![None; text_slots]),
            bitmap: vec![None; text_slots].into_boxed_slice(),
        }
    }

    /// A state with no timer or text slot, which a skin of another kind
    /// gives: nothing accepts it.
    pub(crate) fn inert(skin: u64) -> Self {
        Self::new(skin, false, 0, 0)
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
        for (start, at) in self.starts.iter_mut().zip([timers.screen, timers.fadeout]) {
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
