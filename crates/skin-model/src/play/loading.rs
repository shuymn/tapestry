//! The loading runtime: the play screen's state from a chart's choice to
//! its READY state, isolated from any attempt (stage3.md R3, "The loading
//! runtime"). It holds its own fields, timer starts and readings and text
//! lines, and no chart, notes, reducer or effects; a frame fills it from a
//! [`LoadingFrame`] and draws it with a play's own evaluation and drawing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use bms_model::TimeUs;
use rules::GaugeValue;

use super::schema::{field, timer};
use super::timers::{chart_fields, covers, level_fields, read_timers};
use super::{BuildError, ChartMetadata, CoverSettings, GaugeDesc, LoadState};
use crate::expr::{Program, State, TIMER_OFF};
use crate::layout::Layout;
use crate::widget::Starts;
use crate::widget::gauge::GaugeLook;
use crate::widget::glyphs::{self, GlyphLine};
use crate::widget::lanes::{LaneView, Side};
use crate::widget::ratio_bar::ScoreRate;
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
    /// Available while the frame gives this datum of the chosen chart:
    /// its play level, its difficulty or whether it has a BGA.
    Chosen,
    /// Known before the play starts: it reads its READY value, which a
    /// program may read: `time.progress_ppm`, 0 (the source's
    /// `TIMER_PLAY` is off).
    Before,
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
    use LoadingField::{Before, Chosen, Descriptor, Live, Setting, Unavailable};
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
        Unavailable, // count.fast
        Unavailable, // count.slow
        Unavailable, // count.early.perfect
        Unavailable, // count.late.perfect
        Unavailable, // count.early.great
        Unavailable, // count.late.great
        Unavailable, // count.early.good
        Unavailable, // count.late.good
        Unavailable, // count.early.bad
        Unavailable, // count.late.bad
        Unavailable, // count.early.poor
        Unavailable, // count.late.poor
        Unavailable, // count.early.empty_poor
        Unavailable, // count.late.empty_poor
        // The duration variants, 51 to 74: each needs a tempo (current or
        // a reference) and `H`, and a loading frame has neither (no gain,
        // no tempo, no presentation snapshot), so all 24 are unavailable,
        // their validities included, which would only ever read false.
        Unavailable, // duration.current.covered.ms
        Unavailable, // duration.current.covered.green
        Unavailable, // duration.current.covered.valid
        Unavailable, // duration.current.uncovered.ms
        Unavailable, // duration.current.uncovered.green
        Unavailable, // duration.current.uncovered.valid
        Unavailable, // duration.main.covered.ms
        Unavailable, // duration.main.covered.green
        Unavailable, // duration.main.covered.valid
        Unavailable, // duration.main.uncovered.ms
        Unavailable, // duration.main.uncovered.green
        Unavailable, // duration.main.uncovered.valid
        Unavailable, // duration.min.covered.ms
        Unavailable, // duration.min.covered.green
        Unavailable, // duration.min.covered.valid
        Unavailable, // duration.min.uncovered.ms
        Unavailable, // duration.min.uncovered.green
        Unavailable, // duration.min.uncovered.valid
        Unavailable, // duration.max.covered.ms
        Unavailable, // duration.max.covered.green
        Unavailable, // duration.max.covered.valid
        Unavailable, // duration.max.uncovered.ms
        Unavailable, // duration.max.uncovered.green
        Unavailable, // duration.max.uncovered.valid
        // The native block, 75 to 85: the native HUD's values come from
        // the presentation snapshot and the lanes' gain, which no loading
        // frame carries: no `H`, duration or target, and no mode to show
        // as `native.floating`. The whites take the effective lift with the
        // configured covers; they are part of that HUD, not settings, so
        // they are unavailable too, and a loading skin reads the stored
        // covers through the `Setting` fields above instead.
        Unavailable, // native.hispeed.x100
        Unavailable, // native.hispeed.valid
        Unavailable, // native.duration.ms
        Unavailable, // native.duration.green
        Unavailable, // native.duration.valid
        Unavailable, // native.target.green
        Unavailable, // native.target.valid
        Unavailable, // native.sudden.white
        Unavailable, // native.lift.white
        Unavailable, // native.hidden.white
        Unavailable, // native.floating
        // The BPM-change fact, 86 and 87: a play context's, which no
        // loading frame carries.
        Unavailable, // bpm.change
        Unavailable, // bpm.change_valid
        // The play level, 88 and 89: the chosen chart's, from the frame.
        Chosen, // chart.level
        Chosen, // chart.level_valid
        // Whether the attempt is autoplay: the frame's.
        Live, // autoplay
        // The recent timing, 91 and 92: the reducer's, which a loading
        // state does not have.
        Unavailable, // judge.recent_ms
        Unavailable, // judge.recent_imperfect
        // The lanes' visual states, 93 to 100: the reducer's too.
        Unavailable, // judge.lane.0
        Unavailable, // judge.lane.1
        Unavailable, // judge.lane.2
        Unavailable, // judge.lane.3
        Unavailable, // judge.lane.4
        Unavailable, // judge.lane.5
        Unavailable, // judge.lane.6
        Unavailable, // judge.lane.7
        // The progress, 101: 0 until the play starts, as in READY.
        Before, // time.progress_ppm
        // The chart's metadata, 102 to 109: its difficulty and whether it
        // has a BGA are the chosen chart's, from the frame; its total and
        // its class come with the attempt, from the parsed chart, which a
        // loading frame does not have.
        Chosen,      // chart.difficulty
        Chosen,      // chart.difficulty_valid
        Unavailable, // chart.total
        Unavailable, // chart.total_valid
        Chosen,      // chart.bga
        Chosen,      // chart.bga_valid
        Unavailable, // chart.judge_class
        Unavailable, // chart.judge_class_valid
        // The point, 110: from the counts, unavailable as they are.
        Unavailable, // point
        // The best and the target, 111 to 120: read at load-ready, but not
        // shown while the play screen loads, so they do not appear partway
        // through its loading lifetime; with them their rates, the flags
        // that compare against them, and whether the chart declares
        // `#RANDOM`, which only the parsed chart knows.
        Unavailable, // best.ex
        Unavailable, // target.ex
        // The paced differences: 0 until the first judgment, as in READY.
        Before,      // best.diff
        Before,      // target.diff
        Unavailable, // best.rate_bp
        Unavailable, // target.rate_bp
        Unavailable, // best.valid
        Unavailable, // best.rate_ppm
        Unavailable, // best.reached
        Unavailable, // target.reached
        // The ranks, 121 to 136: false until the first judgment.
        Before, // rank.current.0
        Before, // rank.current.1
        Before, // rank.current.2
        Before, // rank.current.3
        Before, // rank.current.4
        Before, // rank.current.5
        Before, // rank.current.6
        Before, // rank.current.7
        Before, // rank.whole.0
        Before, // rank.whole.1
        Before, // rank.whole.2
        Before, // rank.whole.3
        Before, // rank.whole.4
        Before, // rank.whole.5
        Before, // rank.whole.6
        Before, // rank.whole.7
    ]
};

/// Score rate `rate`'s class while loading, as a ratio bar reads it: the
/// best's and the target's own rates are [`LoadingField::Unavailable`] as
/// their fields are; the others are [`LoadingField::Before`] at 0, their
/// READY value.
#[must_use]
pub const fn loading_rate(rate: ScoreRate) -> LoadingField {
    match rate {
        ScoreRate::Best | ScoreRate::Target => LoadingField::Unavailable,
        ScoreRate::Current | ScoreRate::Judged | ScoreRate::PacedBest | ScoreRate::PacedTarget => {
            LoadingField::Before
        }
    }
}

/// Field `slot`'s class while loading; `None` past the PLAY schema.
#[must_use]
pub fn loading_field(slot: u16) -> Option<LoadingField> {
    FIELDS.get(usize::from(slot)).copied()
}

/// The chosen chart's data a loading frame may give, as bits of
/// [`Need::chosen`]: its play level, its difficulty, whether it has a BGA.
const CHOSEN_LEVEL: u8 = 1;
const CHOSEN_DIFFICULTY: u8 = 2;
const CHOSEN_BGA: u8 = 4;

/// The chosen chart's datum [`LoadingField::Chosen`] field `slot` reads.
const fn chosen_bit(slot: u16) -> u8 {
    match slot {
        field::CHART_LEVEL | field::CHART_LEVEL_VALID => CHOSEN_LEVEL,
        field::CHART_DIFFICULTY | field::CHART_DIFFICULTY_VALID => CHOSEN_DIFFICULTY,
        _ => CHOSEN_BGA,
    }
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
/// It reads only live fields and settings when it needs nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Need {
    /// It reads a descriptor field: evaluated while a descriptor is given.
    descriptor: bool,
    /// The chosen chart's data it reads ([`CHOSEN_LEVEL`] and the others):
    /// evaluated while the frame gives each.
    chosen: u8,
    /// It reads an unavailable field: never evaluated while loading.
    never: bool,
}

impl Need {
    /// What `p` needs; nothing for no program.
    pub(crate) fn of(p: Option<&Program>) -> Self {
        p.into_iter()
            .flat_map(Program::fields)
            .fold(Self::default(), |need, slot| {
                need.and(Self::of_class(loading_field(slot), chosen_bit(slot)))
            })
    }

    /// What reading a value of `class` needs, `chosen` the chosen chart's
    /// datum a [`LoadingField::Chosen`] one reads; never met for no class.
    pub(crate) const fn of_class(class: Option<LoadingField>, chosen: u8) -> Self {
        let none = Self {
            descriptor: false,
            chosen: 0,
            never: false,
        };
        match class {
            Some(LoadingField::Live | LoadingField::Setting | LoadingField::Before) => none,
            Some(LoadingField::Descriptor) => Self {
                descriptor: true,
                ..none
            },
            Some(LoadingField::Chosen) => Self { chosen, ..none },
            Some(LoadingField::Unavailable) | None => Self {
                never: true,
                ..none
            },
        }
    }

    /// What it and `other` need together.
    pub(crate) const fn and(self, other: Self) -> Self {
        Self {
            descriptor: self.descriptor || other.descriptor,
            chosen: self.chosen | other.chosen,
            never: self.never || other.never,
        }
    }

    /// Whether it is met in a frame that gives a descriptor or not, and
    /// the chosen chart's data `chosen` ([`LoadingFrame::chosen`]).
    pub(crate) const fn met(self, descriptor: bool, chosen: u8) -> bool {
        !self.never && (descriptor || !self.descriptor) && self.chosen & !chosen == 0
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
    /// The chosen chart's play level, never a previous attempt's; `None`
    /// leaves what depends on it undrawn.
    pub play_level: Option<i32>,
    /// The chosen chart's difficulty, from 1 to 5 and 0 for any other or
    /// none, as [`ChartMetadata::difficulty`]; `None` leaves what depends on
    /// it undrawn.
    pub difficulty: Option<u8>,
    /// Whether the chosen chart has a BGA, as [`ChartMetadata::bga`];
    /// `None` leaves what depends on it undrawn.
    pub bga: Option<bool>,
    /// Whether the attempt being loaded is autoplay.
    pub autoplay: bool,
    /// The turntable's angle, as [`SkinFrame::turntable`](crate::SkinFrame::turntable).
    pub turntable: i32,
}

impl LoadingFrame<'_> {
    /// The chosen chart's data this frame gives, as [`Need`] reads them.
    pub(crate) const fn chosen(&self) -> u8 {
        let [_, difficulty, _, _, _, bga, _, _] = chart_fields(self.meta());
        let level = if level_fields(self.play_level).1 != 0 {
            CHOSEN_LEVEL
        } else {
            0
        };
        level
            | if difficulty != 0 {
                CHOSEN_DIFFICULTY
            } else {
                0
            }
            | if bga != 0 { CHOSEN_BGA } else { 0 }
    }

    /// The chosen chart's metadata as the frame gives it: no total and no
    /// class, which come with the attempt.
    const fn meta(&self) -> ChartMetadata {
        ChartMetadata {
            difficulty: self.difficulty,
            bga: self.bga,
            ..ChartMetadata::UNKNOWN
        }
    }
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
    /// A state with no timer and no text slot, which a selection skin
    /// gives: every build of it is refused.
    pub(crate) fn inert(scratch: Side) -> Self {
        Self {
            poisoned: None,
            starts: Box::default(),
            ms: Box::default(),
            fields: [0; field::COUNT],
            scratch,
            text: TextTable::new(Vec::new()),
            bitmap: Box::default(),
        }
    }

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

    /// Starts the state afresh in place, as
    /// [`Skin::prepare_loading`](crate::Skin::prepare_loading) makes it:
    /// no refusal, every timer off, every field 0 and every text slot
    /// empty, its lanes still laid out for the same scratch side. Its
    /// buffers are kept, so it allocates nothing.
    pub fn reset(&mut self) {
        self.poisoned = None;
        self.starts.iter_mut().for_each(|s| *s = None);
        self.ms.iter_mut().for_each(|m| *m = TIMER_OFF);
        self.fields = [0; field::COUNT];
        self.text.clear();
        self.bitmap.iter_mut().for_each(|b| *b = None);
    }

    /// The bytes this state holds, itself and every buffer it owns. For a
    /// memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let bytes = size_of::<Self>()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + self.text.heap_bytes()
            + glyphs::lines_bytes(&self.bitmap);
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
        let (level, level_valid) = level_fields(frame.play_level);
        set(field::CHART_LEVEL, level);
        set(field::CHART_LEVEL_VALID, level_valid);
        set(field::AUTOPLAY, i32::from(frame.autoplay));
        for (slot, v) in (field::CHART_DIFFICULTY..).zip(chart_fields(frame.meta())) {
            set(slot, v);
        }
        self.fields = fields;
        for (slot, start) in (0..timer::BUILTIN).zip(self.starts.iter_mut()) {
            *start = match loading_timer(slot) {
                Some(LoadingTimer::Live) => Some(frame.load.since),
                Some(LoadingTimer::Off) | None => None,
            };
        }
        let descriptor = gauge.is_some();
        let chosen = frame.chosen();
        read_timers(
            frame.now,
            &self.fields,
            &mut self.starts,
            &mut self.ms,
            whens,
            |i| whens_need.get(i).is_some_and(|n| n.met(descriptor, chosen)),
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
        for name in ["load.percent", "autoplay"] {
            assert_eq!(class(name), Some(LoadingField::Live), "{name}");
        }
        let chosen = [
            "chart.level",
            "chart.level_valid",
            "chart.difficulty",
            "chart.difficulty_valid",
            "chart.bga",
            "chart.bga_valid",
        ];
        for name in chosen {
            assert_eq!(class(name), Some(LoadingField::Chosen), "{name}");
        }
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
        assert_eq!(class("time.progress_ppm"), Some(LoadingField::Before));
        // The values that wait for the first judgment read their READY
        // value; the best, the target, their rates and what compares
        // against them are unavailable (stage 3, S10-10, G1).
        let mut before = vec!["best.diff".to_owned(), "target.diff".to_owned()];
        for k in 0..8 {
            before.push(format!("rank.current.{k}"));
            before.push(format!("rank.whole.{k}"));
        }
        for name in &before {
            assert_eq!(class(name), Some(LoadingField::Before), "{name}");
        }
        for name in [
            "best.ex",
            "target.ex",
            "best.rate_bp",
            "target.rate_bp",
            "best.valid",
            "best.rate_ppm",
            "best.reached",
            "target.reached",
        ] {
            assert_eq!(class(name), Some(LoadingField::Unavailable), "{name}");
        }
        for f in PLAY.fields {
            let named = [
                "load.percent",
                "autoplay",
                "gauge.kind",
                "gauge.valid",
                "time.progress_ppm",
            ]
            .contains(&f.name)
                || before.iter().any(|b| b == f.name)
                || chosen.contains(&f.name)
                || settings.contains(&f.name);
            if !named {
                assert_eq!(class(f.name), Some(LoadingField::Unavailable), "{}", f.name);
            }
        }
        assert_eq!(loading_field(field::GAUGE), Some(LoadingField::Unavailable));
        for slot in [
            field::BPM_CHANGE,
            field::BPM_CHANGE_VALID,
            field::JUDGE_RECENT_MS,
            field::JUDGE_RECENT_IMPERFECT,
            field::CHART_TOTAL,
            field::CHART_TOTAL_VALID,
            field::CHART_JUDGE_CLASS,
            field::CHART_JUDGE_CLASS_VALID,
        ] {
            assert_eq!(loading_field(slot), Some(LoadingField::Unavailable));
        }
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

    /// A ratio bar's score rate reads as its fields do while loading: the
    /// best's and the target's own rates are unavailable, the others are
    /// 0 until the first judgment.
    #[test]
    fn score_rates_have_their_fields_loading_classes() {
        use crate::widget::ratio_bar::ScoreRate;
        for (rate, want) in [
            (ScoreRate::Current, LoadingField::Before),
            (ScoreRate::Judged, LoadingField::Before),
            (ScoreRate::PacedBest, LoadingField::Before),
            (ScoreRate::Best, LoadingField::Unavailable),
            (ScoreRate::PacedTarget, LoadingField::Before),
            (ScoreRate::Target, LoadingField::Unavailable),
        ] {
            assert_eq!(super::loading_rate(rate), want, "{rate:?}");
        }
    }
}
