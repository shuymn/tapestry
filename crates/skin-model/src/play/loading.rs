//! The loading runtime: the play screen's state from a chart's choice to
//! its countdown, isolated from any attempt (stage3.md R3, "The loading
//! runtime"). It holds its own fields, timer starts and readings and text
//! lines, and no chart, notes, reducer or effects; a frame fills it from a
//! [`LoadingFrame`] and draws it with a play's own evaluation and drawing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::TimeUs;
use rules::GaugeValue;

use super::schema::{field, timer};
use super::timers::{covers, read_timers};
use super::{BuildError, CoverSettings, GaugeDesc, LoadState};
use crate::expr::{Program, State, TIMER_OFF};
use crate::layout::Layout;
use crate::widget::Starts;
use crate::widget::gauge::GaugeLook;
use crate::widget::glyphs::GlyphLine;
use crate::widget::lanes::{LaneView, Side};
use crate::widget::text::TextTable;

/// A field's class while the play screen loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadingField {
    /// Read from the loading frame: `load.percent`.
    Live,
    /// The frame's stored covers.
    Setting,
    /// Available while the frame gives a gauge descriptor.
    Descriptor,
    /// Not read: a program reading it is rejected for loading.
    Unavailable,
}

/// A built-in timer's class while the play screen loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadingTimer {
    /// Started at the loading lifetime's origin: `loading` and `screen`.
    Live,
    /// Known off: it reads as off, which a program may read.
    Off,
}

/// Every PLAY field's class while loading, in slot order (`field::*`).
const FIELDS: [LoadingField; field::COUNT] = {
    use LoadingField::{Descriptor, Live, Setting, Unavailable};
    [
        Unavailable, // combo
        Unavailable, // max_combo
        Unavailable, // ex_score
        Unavailable, // bp
        Unavailable, // count.perfect
        Unavailable, // count.great
        Unavailable, // count.good
        Unavailable, // count.bad
        Unavailable, // count.poor
        Unavailable, // count.empty_poor
        Unavailable, // notes
        Unavailable, // gauge: its value, whatever `gauge.valid` says
        Live,        // load.percent
        Unavailable, // rate.current_bp
        Unavailable, // rate.whole_bp
        Descriptor,  // gauge.kind
        Descriptor,  // gauge.valid
        Unavailable, // bpm.current_x100
        Unavailable, // bpm.min_x100
        Unavailable, // bpm.max_x100
        Unavailable, // bpm.current_valid
        Unavailable, // bpm.range_valid
        Unavailable, // time.remaining_s
        Unavailable, // time.elapsed_s
        Setting,     // cover.permille
        Setting,     // lift.permille
        Setting,     // hidden.permille
        Setting,     // cover.absolute_permille
        Setting,     // cover.enabled
        Setting,     // lift.enabled
        Setting,     // hidden.enabled
        Setting,     // cover.adjusting
        Unavailable, // hispeed.x100
        Unavailable, // hispeed.valid
        Unavailable, // duration.ms
        Unavailable, // duration.green
        Unavailable, // duration.valid
    ]
};

/// Field `slot`'s class while loading; `None` past the PLAY schema.
#[must_use]
pub fn loading_field(slot: u16) -> Option<LoadingField> {
    FIELDS.get(usize::from(slot)).copied()
}

/// Built-in timer `slot`'s class while loading: `loading` and `screen`
/// run from the lifetime's origin, every other built-in is off; `None`
/// past the built-in timers (a `when` timer is live).
#[must_use]
pub const fn loading_timer(slot: u16) -> Option<LoadingTimer> {
    match slot {
        timer::LOADING | timer::SCREEN => Some(LoadingTimer::Live),
        s if s < timer::BUILTIN => Some(LoadingTimer::Off),
        _ => None,
    }
}

/// What a program needs to be evaluated while loading, from its read set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Need {
    /// It reads only live fields and settings.
    Always,
    /// It reads a descriptor field: evaluated while a descriptor is given.
    Descriptor,
    /// It reads an unavailable field: never evaluated while loading.
    Never,
}

impl Need {
    /// What `p` needs; nothing for no program.
    pub(crate) fn of(p: Option<&Program>) -> Self {
        p.into_iter()
            .flat_map(Program::fields)
            .map(|slot| match loading_field(slot) {
                Some(LoadingField::Live | LoadingField::Setting) => Self::Always,
                Some(LoadingField::Descriptor) => Self::Descriptor,
                Some(LoadingField::Unavailable) | None => Self::Never,
            })
            .max()
            .unwrap_or(Self::Always)
    }

    /// Whether it is met in a frame that gives a descriptor or not.
    pub(crate) fn met(self, descriptor: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Descriptor => descriptor,
            Self::Never => false,
        }
    }
}

/// The shown gauge as the loading state knows it, from the chosen play
/// preset: no start and no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadingGauge {
    /// Which of the preset's gauges it is: the selection's identity.
    pub index: usize,
    /// The images a `gauge` widget draws it with, and `gauge.kind`.
    pub look: GaugeLook,
    /// Its maximum, against which its segments are cut.
    pub max: GaugeValue,
    /// Where its segments change from below the border to at or above it.
    pub border: GaugeValue,
}

impl LoadingGauge {
    /// Gauge `index` of a preset, from its descriptor without its start.
    #[must_use]
    pub const fn of(index: usize, desc: &GaugeDesc) -> Self {
        Self {
            index,
            look: desc.look,
            max: desc.max,
            border: desc.border,
        }
    }
}

/// What one loading frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct LoadingFrame<'a> {
    /// The frame's time on the loading clock: timers age on it.
    pub now: TimeUs,
    /// The load: its origin, from which `loading` and `screen` run, and
    /// its progress in %, clamped to 0..=100.
    pub load: LoadState,
    /// The render target's size in pixels.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a Layout,
    /// The hi-speed and the effective covers the lanes and paint follow.
    pub lanes: LaneView,
    /// The stored cover settings: each effective amount in `lanes` must be
    /// its setting while that cover is on and 0 while it is off, else the
    /// build is refused.
    pub covers: CoverSettings,
    /// The shown gauge's descriptor; `None` leaves what depends on it
    /// undrawn.
    pub gauge: Option<LoadingGauge>,
}

/// The loading runtime's state as of its last build: every timer's start
/// and reading, the fields, and the installed text lines.
/// [`Skin::prepare_loading`](crate::Skin::prepare_loading) makes a fresh
/// one.
#[derive(Clone, Debug)]
pub struct Loading {
    /// The error of a refused build, which every later build returns.
    poisoned: Option<BuildError>,
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    fields: [i32; field::COUNT],
    scratch: Side,
    /// The installed rasterized lines, one per text slot.
    text: TextTable,
    /// The installed bitmap lines, one per text slot.
    bitmap: Box<[Option<GlyphLine>]>,
}

impl Loading {
    /// A fresh state with `whens` `when` timers and `text_slots` empty
    /// text slots, its lanes laid out for `scratch`.
    pub(crate) fn new(whens: usize, text_slots: usize, scratch: Side) -> Self {
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Self {
            poisoned: None,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            fields: [0; field::COUNT],
            scratch,
            text: TextTable::new(vec![None; text_slots]),
            bitmap: vec![None; text_slots].into_boxed_slice(),
        }
    }

    /// The bytes this state holds, itself and every buffer it owns. For a
    /// memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let bitmap: usize = self
            .bitmap
            .iter()
            .flatten()
            .map(|l| size_of_val(&*l.quads))
            .sum();
        let bytes = size_of::<Self>()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + self.text.heap_bytes()
            + size_of_val(&*self.bitmap)
            + bitmap;
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    /// Timer `slot`'s start as of the last build, `None` while off.
    #[must_use]
    pub fn timer(&self, slot: u16) -> Option<TimeUs> {
        self.starts().at(usize::from(slot))
    }

    /// The fields and timer readings as of the last build.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &self.fields,
            timers: &self.ms,
        }
    }

    /// The error a build refused with, which every later build returns.
    #[must_use]
    pub fn refused(&self) -> Option<BuildError> {
        self.poisoned
    }

    pub(crate) fn starts(&self) -> Starts<'_> {
        Starts(&self.starts)
    }

    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    pub(crate) fn scratch(&self) -> Side {
        self.scratch
    }

    pub(crate) fn text(&self) -> &TextTable {
        &self.text
    }

    pub(crate) fn text_mut(&mut self) -> &mut TextTable {
        &mut self.text
    }

    pub(crate) fn bitmap(&self) -> &[Option<GlyphLine>] {
        &self.bitmap
    }

    pub(crate) fn bitmap_mut(&mut self) -> &mut [Option<GlyphLine>] {
        &mut self.bitmap
    }

    /// Fills this frame's fields and timers: the frame's progress, stored
    /// covers and descriptor into the fields, `loading` and `screen` from
    /// the origin with every other built-in off, then each `when` timer,
    /// whose condition reads false while `whens_need` is not met.
    ///
    /// A refused state, or covers [`Play::build`](crate::Skin::build)
    /// would refuse, is refused before anything changes, and the state is
    /// refused with that error from then on.
    pub(crate) fn fill(
        &mut self,
        frame: &LoadingFrame<'_>,
        whens: &[Program],
        whens_need: &[Need],
    ) -> Result<(), BuildError> {
        if let Some(e) = self.poisoned {
            return Err(e);
        }
        let Some(c) = covers(&frame.lanes, &frame.covers) else {
            self.poisoned = Some(BuildError::InvalidReadouts);
            return Err(BuildError::InvalidReadouts);
        };
        let gauge = frame.gauge;
        // Unavailable fields are never read (`Need`); they hold 0.
        let mut fields = [0; field::COUNT];
        let mut set = |slot: u16, v: i32| {
            if let Some(f) = fields.get_mut(usize::from(slot)) {
                *f = v;
            }
        };
        set(field::LOAD_PERCENT, frame.load.percent.clamp(0, 100));
        let look = gauge.map_or(GaugeLook::Normal, |g| g.look);
        set(field::GAUGE_KIND, look.schema_value());
        set(field::GAUGE_VALID, i32::from(gauge.is_some()));
        set(field::COVER_PERMILLE, c.cover);
        set(field::LIFT_PERMILLE, c.lift);
        set(field::HIDDEN_PERMILLE, c.hidden);
        set(field::COVER_ABSOLUTE_PERMILLE, c.absolute);
        set(field::COVER_ENABLED, c.cover_enabled);
        set(field::LIFT_ENABLED, c.lift_enabled);
        set(field::HIDDEN_ENABLED, c.hidden_enabled);
        set(field::COVER_ADJUSTING, c.adjusting);
        self.fields = fields;
        for (slot, start) in (0..timer::BUILTIN).zip(self.starts.iter_mut()) {
            *start = match loading_timer(slot) {
                Some(LoadingTimer::Live) => Some(frame.load.since),
                Some(LoadingTimer::Off) | None => None,
            };
        }
        let descriptor = gauge.is_some();
        read_timers(
            frame.now,
            &self.fields,
            &mut self.starts,
            &mut self.ms,
            whens,
            |i| whens_need.get(i).is_some_and(|n| n.met(descriptor)),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FIELDS, LoadingField, LoadingTimer, loading_field, loading_timer};
    use crate::play::schema::{PLAY, field, timer};

    /// The table classes every field and timer of the schema, and the
    /// classes are the ones stage3.md R3 lists.
    #[test]
    fn every_field_and_timer_has_a_loading_class() {
        assert_eq!(FIELDS.len(), PLAY.fields.len());
        let class = |name: &str| PLAY.field(name).and_then(|(s, _)| loading_field(s));
        assert_eq!(class("load.percent"), Some(LoadingField::Live));
        for name in ["gauge.kind", "gauge.valid"] {
            assert_eq!(class(name), Some(LoadingField::Descriptor), "{name}");
        }
        let settings = [
            "cover.permille",
            "lift.permille",
            "hidden.permille",
            "cover.absolute_permille",
            "cover.enabled",
            "lift.enabled",
            "hidden.enabled",
            "cover.adjusting",
        ];
        for name in settings {
            assert_eq!(class(name), Some(LoadingField::Setting), "{name}");
        }
        for f in PLAY.fields {
            let named = ["load.percent", "gauge.kind", "gauge.valid"].contains(&f.name)
                || settings.contains(&f.name);
            if !named {
                assert_eq!(class(f.name), Some(LoadingField::Unavailable), "{}", f.name);
            }
        }
        assert_eq!(loading_field(field::GAUGE), Some(LoadingField::Unavailable));
        assert_eq!(loading_field(u16::try_from(field::COUNT).unwrap()), None);
        for (i, t) in PLAY.timers.iter().enumerate() {
            let slot = u16::try_from(i).unwrap();
            let want = if matches!(t.name, "loading" | "screen") {
                LoadingTimer::Live
            } else {
                LoadingTimer::Off
            };
            assert_eq!(loading_timer(slot), Some(want), "{}", t.name);
        }
        assert_eq!(loading_timer(timer::BUILTIN), None);
    }
}
