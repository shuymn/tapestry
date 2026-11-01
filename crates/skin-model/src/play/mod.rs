//! The play screen's state: the per-attempt [`Play`] a skin fills each
//! frame from a [`SkinFrame`], the play schema and the reducer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod reducer;
pub(crate) mod schema;
pub(crate) mod timers;

use bms_model::{Chart, LANE_COUNT, Lane, LnMode, Note, Position, ScrollMap, TimeUs};
use eval::{GaugeState, JudgeEvent, Section, Tally};
use rules::GaugeValue;

use self::reducer::Reducer;
use self::schema::{field, timer};
use crate::expr::{State, TIMER_OFF};
use crate::layout::Layout;
use crate::widget::Starts;
use crate::widget::bar::BarScratch;
use crate::widget::gauge::GaugeLook;
use crate::widget::glyphs::GlyphLine;
use crate::widget::lanes::{LaneView, Side};
use crate::widget::text::TextTable;

/// What the skin knows of one of the preset's gauges, in the preset's
/// order ([`GaugeDesc::from_rule`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeDesc {
    /// The value a whole-chart attempt starts at.
    pub start: GaugeValue,
    /// The value `gauge_max` turns on at.
    pub max: GaugeValue,
    /// The images a `gauge` widget draws it with.
    pub look: GaugeLook,
    /// Where a `gauge` widget's segments change from below the border to
    /// at or above it.
    pub border: GaugeValue,
}

/// What one attempt plays.
#[derive(Clone, Copy, Debug)]
pub enum Attempt<'a> {
    /// The whole chart: `ready` starts `lead_in` before chart time 0 and
    /// `play` at 0, every note is a target and the gauges start at their
    /// descriptors' `start`.
    Full { lead_in: TimeUs },
    /// A section: a practice round, or a start mid-chart (a section whose
    /// end is past the chart). `ready` starts `lead_in` before the
    /// section's start and `play` at it, the targets are the section's and
    /// the gauges start at its `initial_gauges`.
    Section(&'a Section),
}

/// What is fixed for one attempt.
#[derive(Clone, Copy, Debug)]
pub struct PlayContext<'a> {
    /// The preset's gauges, in its order.
    pub gauges: &'a [GaugeDesc],
    /// The attempt.
    pub attempt: Attempt<'a>,
    /// Whether an empty POOR breaks the combo (the preset's
    /// `empty_poor.breaks_combo`), which turns `combo.up` off.
    pub empty_poor_breaks_combo: bool,
    /// Whether an empty POOR is counted as a POOR (the preset's
    /// `empty_poor.counted`): the snapshot's POOR count then includes its
    /// empty POORs, which the score rates do not count as judged targets.
    pub empty_poor_counted: bool,
    /// The player's scratch side, which the lanes are laid out for.
    pub scratch: Side,
    /// The preset's long-note mode for a chart that declares none
    /// (`rule.ln_mode.default`).
    pub ln_default: rules::LnMode,
    /// The end of the attempt's notes on the play clock, from which
    /// `last_note` is sampled: the last note's time in a normal play, the
    /// last timeline's (background sounds included) in autoplay, and in
    /// practice the section's end plus 1 s, scaled by the play speed
    /// (`play/BMSPlayer.java:199, 587`).
    pub last_note_end: TimeUs,
    /// The original 1× chart's BPM range, its initial BPM included, over
    /// its whole timeline (not narrowed to a practice section, not scaled
    /// by the play speed), which `bpm.min_x100` and `bpm.max_x100` read;
    /// `None` when the caller has no exact range, which hides them.
    pub bpm_range: Option<BpmRange>,
}

/// A chart's BPM range in floored hundredths of a BPM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BpmRange {
    /// The lowest BPM, at most `max_x100`.
    pub min_x100: u32,
    /// The highest BPM, at most [`READOUT_MAX`](crate::READOUT_MAX).
    pub max_x100: u32,
}

/// An exact nonnegative duration in ms, `numerator / denominator`; the
/// denominator is not 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurationRatio {
    pub numerator: u128,
    pub denominator: u128,
}

/// The player's cover settings as stored: an amount is kept while its
/// cover is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverSettings {
    /// The configured lane cover, in ‰ (at most 1 000).
    pub cover: u16,
    /// The configured lift, in ‰ (at most 1 000).
    pub lift: u16,
    /// The configured hidden cover, in ‰ (at most 1 000).
    pub hidden: u16,
    /// Whether the lane cover is on.
    pub cover_enabled: bool,
    /// Whether the lift is on.
    pub lift_enabled: bool,
    /// Whether the hidden cover is on.
    pub hidden_enabled: bool,
    /// Whether the cover-adjustment mode is active; not whether an amount
    /// changed this frame.
    pub adjusting: bool,
}

/// What one frame reports of the presentation besides the lanes'
/// effective amounts ([`LaneView`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameReadouts {
    /// The playback tempo at the display head (`now + display_offset`), in
    /// floored hundredths of a BPM: the initial tempo before chart time 0,
    /// the musical tempo through a STOP; `None` when unknown. 0 is a tempo
    /// below 0.01, not a STOP.
    pub bpm_current_x100: Option<u32>,
    /// The scroll policy's explicit multiplicative hi-speed, in floored
    /// hundredths; `None` when it has none (it is never solved back from
    /// the duration).
    pub hispeed_x100: Option<u32>,
    /// The scroll policy's nominal current travel duration, STOP waiting
    /// excluded and unrounded; `None` when unknown.
    pub duration: Option<DurationRatio>,
    /// The stored cover settings.
    pub covers: CoverSettings,
}

/// What one frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct SkinFrame<'a> {
    /// The frame's chart time: timers age on it.
    pub now: TimeUs,
    /// Added to `now` for the display clock, which `beat` follows.
    pub display_offset: TimeUs,
    /// The play view's running totals.
    pub tally: &'a Tally,
    /// The play view's gauges, in the preset's order.
    pub gauges: &'a [GaugeState],
    /// The shown gauge's index into `gauges`; it may change between
    /// frames.
    pub gauge: usize,
    /// The effects drained since the last frame, in emission order.
    pub effects: &'a [JudgeEvent],
    /// Whether evaluation is complete (the play view's `complete`): a
    /// section attempt then rings out without notes.
    pub complete: bool,
    /// Whether this frame's update entered the play in its playing state:
    /// false during the countdown and after a stop, the finish or the fade
    /// out. A gauge failure during the frame's own update leaves it true.
    pub playing: bool,
    /// The render target's size in pixels; the skin computes its canvas
    /// from it.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a Layout,
    /// The hi-speed and the covers.
    pub lanes: LaneView,
    /// The chart's load while the play screen waits for it, from the
    /// chart's choice to the countdown: the `loading` timer runs and
    /// `load.percent` reads its progress; `None` once it is loaded.
    pub loading: Option<LoadState>,
    /// The tempo, hi-speed, duration and stored covers the readouts show.
    /// Each effective amount in `lanes` must be its stored setting while
    /// that cover is on and 0 while it is off, else the build is refused.
    pub readouts: FrameReadouts,
}

/// The chart's load as the play screen shows it before the countdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadState {
    /// When the load began, on the frame's clock.
    pub since: TimeUs,
    /// How far it is, in %, clamped to 0..=100.
    pub percent: i32,
}

/// Why [`Skin::prepare`](crate::Skin::prepare) refused an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    #[error("the attempt's lead-in is negative")]
    NegativeLeadIn,
    #[error("the attempt gives {got} initial gauges for {expected} gauges")]
    InitialGauges { expected: usize, got: usize },
    #[error("the attempt's countdown starts outside the time range")]
    Overflow,
    #[error("the attempt's section ends before it starts")]
    InvalidSectionRange,
    #[error(
        "the BPM range's minimum is above its maximum, or its maximum above the readout maximum"
    )]
    InvalidBpmRange,
}

/// Why [`Skin::build`](crate::Skin::build) refused a frame. The frame's
/// effects were not applied and the [`Play`] is poisoned: every later
/// build returns the same error until a fresh
/// [`prepare`](crate::Skin::prepare), so the caller stops the attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The snapshot counts more empty POORs as POORs than it has POORs,
    /// which the evaluator never produces.
    #[error("the snapshot counts more empty POORs than POORs")]
    InconsistentTally,
    /// A readout is out of range (a BPM or hi-speed above
    /// [`READOUT_MAX`](crate::READOUT_MAX), a cover setting above 1 000, a
    /// zero duration denominator, a rounded duration above
    /// [`READOUT_MAX`](crate::READOUT_MAX)), or an effective amount in
    /// [`SkinFrame::lanes`] is not its stored cover setting.
    #[error("the frame's readouts are out of range or disagree with the lanes")]
    InvalidReadouts,
}

/// One attempt's skin state: the reducer, every timer's start and reading,
/// and the fields, as of the last build. [`Skin::prepare`](crate::Skin::prepare)
/// makes a fresh one per attempt, so a retry starts clean.
#[derive(Clone, Debug)]
pub struct Play {
    reducer: Reducer,
    /// The context's [`PlayContext::empty_poor_counted`].
    empty_poor_counted: bool,
    /// The error of a refused build, which every later build returns.
    poisoned: Option<BuildError>,
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    fields: [i32; field::COUNT],
    scroll: ScrollMap,
    /// The display beat last seen and the µs it was crossed at.
    beat: Option<(i128, i64)>,
    ready_at: TimeUs,
    play_at: TimeUs,
    /// The context's [`PlayContext::last_note_end`].
    last_note_end: TimeUs,
    /// Where `time.remaining_s` counts to: the chart's last resolved
    /// object ([`last_object`]) or the section's end.
    readout_end: TimeUs,
    /// The context's [`PlayContext::bpm_range`], validated.
    bpm_range: Option<BpmRange>,
    scratch: Side,
    /// Whether a long note's release is judged on its own (CN and HCN).
    separate_end_judgment: bool,
    notes: Option<Box<Notes>>,
    /// The context's gauge descriptors.
    gauges: Box<[GaugeDesc]>,
    /// The attempt's rasterized lines, empty until one is attached.
    text: TextTable,
    /// The attempt's bitmap lines by text slot, empty until laid out.
    bitmap: Box<[Option<GlyphLine>]>,
    /// The bar groups' per-frame work, for a skin with lanes and groups;
    /// a build lends it to the lanes.
    bars: Option<BarScratch>,
}

/// The chart's notes and bar lines, kept for a skin with lanes.
#[derive(Clone, Debug)]
pub(crate) struct Notes {
    pub(crate) lanes: [Box<[Note]>; LANE_COUNT],
    pub(crate) bars: Box<[Position]>,
    /// Per lane: the indices of the notes from the section's start to its
    /// end (every note without one); the last may still not be a target.
    pub(crate) range: [(usize, usize); LANE_COUNT],
    /// The section's start and end.
    pub(crate) section: Option<(TimeUs, TimeUs)>,
}

/// The chart's last resolved object: the latest note end (mines included)
/// or background sound start, at least 0. A sound's tail is not an object.
fn last_object(chart: &Chart) -> TimeUs {
    let notes = chart.lanes.iter().flatten().map(Note::end);
    let bgm = chart.bgm.iter().map(|b| b.time);
    notes.chain(bgm).fold(TimeUs(0), TimeUs::max)
}

impl Play {
    /// The bytes this attempt's state holds, itself and every buffer it
    /// owns: the reducer, the timers, the chart's scroll segments and
    /// notes copied for it, the gauges, the attached and bitmap lines, and
    /// the bar groups' scratch. For a memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let notes = self.notes.as_deref().map_or(0, |n| {
            let lanes: usize = n.lanes.iter().map(|l| size_of_val(&**l)).sum();
            size_of::<Notes>() + lanes + size_of_val(&*n.bars)
        });
        let bitmap: usize = self
            .bitmap
            .iter()
            .flatten()
            .map(|l| size_of_val(&*l.quads))
            .sum();
        let bytes = size_of::<Self>()
            + self.reducer.heap_bytes()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + size_of_val(self.scroll.segments())
            + notes
            + size_of_val(&*self.gauges)
            + self.text.heap_bytes()
            + size_of_val(&*self.bitmap)
            + bitmap
            + self.bars.as_ref().map_or(0, BarScratch::heap_bytes);
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    /// A fresh attempt with `whens` `when` timers, keeping the chart's
    /// notes and the work of `bar_groups` checked groups when the skin has
    /// `lanes`; grades up to `bomb_up_to` give a bomb.
    pub(crate) fn new(
        chart: &Chart,
        ctx: &PlayContext<'_>,
        whens: usize,
        lanes: bool,
        bar_groups: usize,
        bomb_up_to: Option<schema::Grade>,
    ) -> Result<Self, PrepareError> {
        let (play_at, lead_in, section) = match ctx.attempt {
            Attempt::Full { lead_in } => (TimeUs(0), lead_in, None),
            Attempt::Section(s) => (s.start, s.lead_in, Some(s)),
        };
        if lead_in.0 < 0 {
            return Err(PrepareError::NegativeLeadIn);
        }
        let ready_at = play_at.checked_sub(lead_in).ok_or(PrepareError::Overflow)?;
        if let Some(s) = section
            && s.initial_gauges.len() != ctx.gauges.len()
        {
            return Err(PrepareError::InitialGauges {
                expected: ctx.gauges.len(),
                got: s.initial_gauges.len(),
            });
        }
        if section.is_some_and(|s| s.end < s.start) {
            return Err(PrepareError::InvalidSectionRange);
        }
        if ctx
            .bpm_range
            .is_some_and(|r| r.min_x100 > r.max_x100 || r.max_x100 > schema::READOUT_MAX_U32)
        {
            return Err(PrepareError::InvalidBpmRange);
        }
        let readout_end = section.map_or_else(|| last_object(chart), |s| s.end);
        let initial = ctx.gauges.iter().enumerate().map(|(i, g)| {
            let value = section
                .and_then(|s| s.initial_gauges.get(i).copied())
                .unwrap_or(g.start);
            (value, g.max)
        });
        let reducer = Reducer::new(
            chart,
            section,
            initial,
            play_at,
            ctx.empty_poor_breaks_combo,
            bomb_up_to,
        );
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        let bounds = section.map(|s| (s.start, s.end));
        let notes = lanes.then(|| {
            let range = std::array::from_fn(|l| {
                chart.lanes.get(l).map_or((0, 0), |notes| match bounds {
                    None => (0, notes.len()),
                    Some((start, end)) => (
                        notes.partition_point(|n| n.time < start),
                        notes.partition_point(|n| n.time < end),
                    ),
                })
            });
            Box::new(Notes {
                lanes: chart.lanes.clone().map(Vec::into_boxed_slice),
                bars: chart.bar_lines.clone().into_boxed_slice(),
                range,
                section: bounds,
            })
        });
        Ok(Self {
            reducer,
            empty_poor_counted: ctx.empty_poor_counted,
            poisoned: None,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            fields: [0; field::COUNT],
            scroll: chart.scroll.clone(),
            beat: None,
            ready_at,
            play_at,
            last_note_end: ctx.last_note_end,
            readout_end,
            bpm_range: ctx.bpm_range,
            scratch: ctx.scratch,
            // As eval decides the chart's mode (evaluator.rs, `ln_mode`):
            // a new preset mode must be placed here.
            separate_end_judgment: match (ctx.ln_default, chart.meta.ln_mode) {
                (rules::LnMode::Ln, None | Some(LnMode::Ln)) => false,
                (rules::LnMode::Ln, Some(LnMode::Cn | LnMode::Hcn)) => true,
            },
            notes,
            gauges: ctx.gauges.into(),
            text: TextTable::default(),
            bitmap: Box::new([]),
            bars: (lanes && bar_groups > 0).then(|| BarScratch::new(bar_groups)),
        })
    }

    /// The bar groups the play holds work for.
    pub(crate) fn bar_capacity(&self) -> usize {
        self.bars.as_ref().map_or(0, |b| b.capacity())
    }

    /// Lends out the bar groups' work; allocates nothing.
    pub(crate) fn take_bars(&mut self) -> Option<BarScratch> {
        self.bars.take()
    }

    /// Takes back the work [`take_bars`](Self::take_bars) lent.
    pub(crate) fn restore_bars(&mut self, bars: Option<BarScratch>) {
        self.bars = bars;
    }

    /// The number of timer slots, built-in and `when`.
    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Timer `slot`'s start as of the last build, `None` while off.
    #[must_use]
    pub fn timer(&self, slot: u16) -> Option<TimeUs> {
        self.timer_at(usize::from(slot))
    }

    pub(crate) fn timer_at(&self, slot: usize) -> Option<TimeUs> {
        self.starts().at(slot)
    }

    /// Every timer slot's start as of the last build.
    pub(crate) fn starts(&self) -> Starts<'_> {
        Starts(&self.starts)
    }

    /// The fields and timer readings as of the last build.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &self.fields,
            timers: &self.ms,
        }
    }

    /// The attempt's rasterized lines.
    pub(crate) fn text(&self) -> &TextTable {
        &self.text
    }

    pub(crate) fn set_text(&mut self, table: TextTable) {
        self.text = table;
    }

    /// The attempt's bitmap lines, by text slot.
    pub(crate) fn bitmap(&self) -> &[Option<GlyphLine>] {
        &self.bitmap
    }

    pub(crate) fn set_bitmap(&mut self, lines: Box<[Option<GlyphLine>]>) {
        self.bitmap = lines;
    }

    /// The descriptor of the context's gauge `i`.
    pub(crate) fn gauge_desc(&self, i: usize) -> Option<&GaugeDesc> {
        self.gauges.get(i)
    }

    /// The latest judgment's grade as of the last build, `None` before
    /// the first.
    pub(crate) fn judge(&self) -> Option<schema::Grade> {
        self.reducer.judge.map(|(g, _)| g)
    }

    /// The attempt's scratch side.
    pub(crate) fn scratch(&self) -> Side {
        self.scratch
    }

    /// Whether a long note's release is judged on its own: the chart's
    /// mode, or the preset's default when it declares none, is CN or HCN.
    pub(crate) fn separate_end_judgment(&self) -> bool {
        self.separate_end_judgment
    }

    /// The chart's notes, kept when the skin has lanes.
    pub(crate) fn notes(&self) -> Option<&Notes> {
        self.notes.as_deref()
    }

    /// The chart's scroll map.
    pub(crate) fn scroll(&self) -> &ScrollMap {
        &self.scroll
    }

    /// Whether `note` on lane `l` is no longer drawn: a judged tap or a hit
    /// mine, as of the last build.
    pub(crate) fn done(&self, l: usize, note: usize) -> bool {
        self.reducer.done(l, note)
    }

    /// `lane`'s visual hold, as of the last build.
    fn hold(&self, lane: Lane) -> Option<reducer::Hold> {
        self.reducer.hold.get(lane.index()).copied().flatten()
    }

    /// The long note whose visual hold `lane` shows, as of the last build.
    #[cfg(test)]
    fn hold_note(&self, lane: Lane) -> Option<usize> {
        self.hold(lane).map(|h| h.note)
    }

    /// Whether the lanes draw `note`'s body on `lane` active, as of the
    /// last build: by the LN mode, from whether the lane's visual hold is
    /// on it.
    pub(crate) fn body_active(&self, lane: Lane, note: usize) -> bool {
        let held = self.hold(lane).is_some_and(|h| h.note == note);
        reducer::body_active(self.reducer.ln_mode, held)
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{ChartBuilder, Lane, TimeUs};
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, Tally};
    use rules::{GaugeValue, Judgment, PerJudgment};

    use super::{Attempt, CoverSettings, FrameReadouts, GaugeDesc, PlayContext, SkinFrame};
    use crate::{
        AtlasLimits, Fit, LaneView, Layout, QuadBatch, Region, Sheet, Side, Skin, SkinDef, Variant,
    };

    const MS: i64 = 1_000;

    /// No tempo, hi-speed or duration, every cover off at 0: the readouts
    /// of a frame whose lanes draw no cover.
    pub(crate) const OFF: FrameReadouts = FrameReadouts {
        bpm_current_x100: None,
        hispeed_x100: None,
        duration: None,
        covers: CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled: false,
            lift_enabled: false,
            hidden_enabled: false,
            adjusting: false,
        },
    };

    fn lane(n: u8) -> Lane {
        Lane::new(n).unwrap()
    }

    fn ev(ms: i64, kind: JudgeEventKind) -> JudgeEvent {
        JudgeEvent {
            seq: 0,
            time: TimeUs(ms * MS),
            origin: None,
            kind,
        }
    }

    /// The done bits and hold notes the lanes widget reads, after each
    /// frame of effects.
    #[test]
    fn done_bits_and_hold_notes_follow_the_effects() {
        let chart = ChartBuilder::new(150_000)
            .long(lane(2), TimeUs(1_000 * MS), TimeUs(1_300 * MS))
            .long(lane(2), TimeUs(2_000 * MS), TimeUs(2_300 * MS))
            .long(lane(5), TimeUs(1_500 * MS), TimeUs(1_800 * MS))
            .tap(lane(5), TimeUs(3_000 * MS))
            .mine(lane(6), TimeUs(3_000 * MS), 10)
            .build()
            .unwrap();
        let region = crate::RegionDef {
            anchor: crate::Anchor::TopLeft,
            optional: false,
        };
        let skin = Skin::new(SkinDef {
            fit: Fit::Adapt,
            regions: [region; Region::COUNT],
            whens: Vec::new(),
            sheet: Sheet::empty(),
            lanes: None,
            elements: Vec::new(),
            widgets: crate::WidgetDefs::default(),
            bomb_up_to: Some(crate::Grade::Bad),
        })
        .unwrap();
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 2,
            reserved_layers: 1,
        };
        let (_, uvs) = skin.atlas(Variant::Lo, limits).unwrap().into_parts();
        let gauges = [GaugeDesc {
            start: GaugeValue(0),
            max: GaugeValue::FULL,
            look: crate::GaugeLook::Normal,
            border: GaugeValue(80 * GaugeValue::PERCENT),
        }];
        let ctx = PlayContext {
            gauges: &gauges,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(3_000 * MS),
            bpm_range: None,
        };
        let mut play = skin.prepare(&chart, &ctx).unwrap();
        let zero = PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        };
        let tally = Tally {
            counts: zero,
            ex_score: 0,
            bp: 0,
            combo: 0,
            max_combo: 0,
            empty_poor: 0,
        };
        let snapshot = [GaugeState {
            value: GaugeValue(0),
            reference: GaugeValue(0),
            failed_at: None,
        }];
        let layout = Layout::AUTHOR;
        let mut batch = QuadBatch::with_capacity(0, 0);
        let mut frame = |play: &mut super::Play, now_ms: i64, effects: &[JudgeEvent]| {
            let frame = SkinFrame {
                now: TimeUs(now_ms * MS),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &snapshot,
                gauge: 0,
                effects,
                complete: false,
                playing: true,
                target: [1_920, 1_080],
                layout: &layout,
                lanes: LaneView {
                    visible_time: TimeUs(600 * MS),
                    cover: 0,
                    lift: 0,
                    hidden: 0,
                },
                loading: None,
                readouts: OFF,
            };
            skin.build(play, &frame, &uvs, &mut batch).unwrap();
        };
        let held = |n: u8, note: usize, kind: Judgment| JudgeEventKind::LnHeld {
            lane: lane(n),
            note,
            judgment: kind,
            deviation: TimeUs(0),
        };
        let ended = |n: u8, note: usize| JudgeEventKind::LnHoldEnded {
            lane: lane(n),
            note,
        };
        let judged = |n: u8, note: usize, judgment, deviation: Option<i64>, release| {
            JudgeEventKind::Judged {
                lane: lane(n),
                note,
                judgment,
                deviation: deviation.map(|d| TimeUs(d * MS)),
                release,
                combo: 1,
            }
        };
        frame(&mut play, 1_000, &[ev(1_000, held(2, 0, Judgment::Great))]);
        assert_eq!(play.hold_note(lane(2)), Some(0));
        assert!(play.body_active(lane(2), 0));
        frame(
            &mut play,
            1_250,
            &[
                ev(1_250, ended(2, 0)),
                ev(
                    1_250,
                    judged(2, 0, Judgment::Good, Some(0), Some(TimeUs(-50 * MS))),
                ),
            ],
        );
        assert_eq!(play.hold_note(lane(2)), None);
        // A judged long note stays drawn: it is never done.
        assert!(!play.done(2, 0));
        // A BAD head holds without `LnHeld`, its body active; it stays
        // drawn.
        frame(
            &mut play,
            1_400,
            &[ev(1_400, judged(5, 0, Judgment::Bad, Some(-100), None))],
        );
        assert_eq!(play.hold_note(lane(5)), Some(0));
        assert!(play.body_active(lane(5), 0));
        assert!(!play.done(5, 0));
        // Another note's end leaves it; its own ends it.
        frame(&mut play, 1_420, &[ev(1_420, ended(5, 1))]);
        assert_eq!(play.hold_note(lane(5)), Some(0));
        frame(&mut play, 1_450, &[ev(1_450, ended(5, 0))]);
        assert_eq!(play.hold_note(lane(5)), None);
        // A held note whose end event is lost stops at its end.
        frame(
            &mut play,
            2_000,
            &[ev(2_000, held(2, 1, Judgment::Perfect))],
        );
        assert_eq!(play.hold_note(lane(2)), Some(1));
        frame(&mut play, 2_300, &[]);
        assert_eq!(play.hold_note(lane(2)), None);
        // A judged tap and a hit mine are done; a tap's `LnHeld` is no
        // hold, and stray indices are ignored.
        assert!(!play.done(5, 1));
        frame(
            &mut play,
            3_000,
            &[
                ev(3_000, held(5, 1, Judgment::Great)),
                ev(3_000, judged(5, 1, Judgment::Poor, None, None)),
                ev(3_000, judged(5, 99, Judgment::Great, Some(0), None)),
                ev(
                    3_000,
                    JudgeEventKind::MineHit {
                        lane: lane(6),
                        note: 0,
                    },
                ),
                ev(
                    3_000,
                    JudgeEventKind::MineHit {
                        lane: lane(6),
                        note: 70,
                    },
                ),
            ],
        );
        assert_eq!(play.hold_note(lane(5)), None);
        assert!(play.done(5, 1));
        assert!(play.done(6, 0));
        assert!(!play.done(5, 99));
        assert!(!play.done(6, 70));
        assert!(!play.done(1, 0));

        // A refused frame applies none of its effects: the hold its end
        // would close stays.
        let counted = PlayContext {
            empty_poor_counted: true,
            ..ctx
        };
        let mut play = skin.prepare(&chart, &counted).unwrap();
        frame(&mut play, 1_000, &[ev(1_000, held(2, 0, Judgment::Great))]);
        assert_eq!(play.hold_note(lane(2)), Some(0));
        let inconsistent = Tally {
            empty_poor: 1,
            ..tally
        };
        let refused = SkinFrame {
            now: TimeUs(1_250 * MS),
            display_offset: TimeUs(0),
            tally: &inconsistent,
            gauges: &snapshot,
            gauge: 0,
            effects: &[ev(1_250, ended(2, 0))],
            complete: false,
            playing: true,
            target: [1_920, 1_080],
            layout: &layout,
            lanes: LaneView {
                visible_time: TimeUs(600 * MS),
                cover: 0,
                lift: 0,
                hidden: 0,
            },
            loading: None,
            readouts: OFF,
        };
        assert_eq!(
            skin.build(&mut play, &refused, &uvs, &mut batch),
            Err(super::BuildError::InconsistentTally)
        );
        assert_eq!(play.hold_note(lane(2)), Some(0));
        assert_eq!(play.poisoned, Some(super::BuildError::InconsistentTally));
    }

    /// The loading state: the `loading` timer runs from the load's start
    /// and `load.percent` reads its progress, clamped, while the frame
    /// carries one; once it is loaded the timer is off and the field 100.
    #[test]
    fn the_loading_timer_and_progress_follow_the_frame() {
        use super::LoadState;
        use crate::{field, timer};
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let region = crate::RegionDef {
            anchor: crate::Anchor::TopLeft,
            optional: false,
        };
        let skin = Skin::new(SkinDef {
            fit: Fit::Adapt,
            regions: [region; Region::COUNT],
            whens: Vec::new(),
            sheet: Sheet::empty(),
            lanes: None,
            elements: Vec::new(),
            widgets: crate::WidgetDefs::default(),
            bomb_up_to: None,
        })
        .unwrap();
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 2,
            reserved_layers: 1,
        };
        let (_, uvs) = skin.atlas(Variant::Lo, limits).unwrap().into_parts();
        let ctx = PlayContext {
            gauges: &[],
            attempt: Attempt::Full {
                lead_in: TimeUs(1_000 * MS),
            },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(1_000 * MS),
            bpm_range: None,
        };
        let mut play = skin.prepare(&chart, &ctx).unwrap();
        let zero = PerJudgment {
            perfect: 0,
            great: 0,
            good: 0,
            bad: 0,
            poor: 0,
        };
        let tally = Tally {
            counts: zero,
            ex_score: 0,
            bp: 0,
            combo: 0,
            max_combo: 0,
            empty_poor: 0,
        };
        let layout = Layout::AUTHOR;
        let mut batch = QuadBatch::with_capacity(0, 0);
        let mut frame = |play: &mut super::Play, loading: Option<LoadState>| {
            let frame = SkinFrame {
                now: TimeUs(-5_000 * MS),
                display_offset: TimeUs(0),
                tally: &tally,
                gauges: &[],
                gauge: 0,
                effects: &[],
                complete: false,
                target: [1_920, 1_080],
                layout: &layout,
                lanes: LaneView {
                    visible_time: TimeUs(600 * MS),
                    cover: 0,
                    lift: 0,
                    hidden: 0,
                },
                loading,
                readouts: OFF,
                playing: false,
            };
            skin.build(play, &frame, &uvs, &mut batch).unwrap();
        };
        let percent = |play: &super::Play| {
            play.state()
                .fields
                .get(usize::from(field::LOAD_PERCENT))
                .copied()
        };
        let since = TimeUs(-5_250 * MS);
        frame(&mut play, Some(LoadState { since, percent: 40 }));
        assert_eq!(play.timer(timer::LOADING), Some(since));
        assert_eq!(
            play.state().timers.get(usize::from(timer::LOADING)),
            Some(&250)
        );
        assert_eq!(percent(&play), Some(40));
        frame(
            &mut play,
            Some(LoadState {
                since,
                percent: 140,
            }),
        );
        assert_eq!(percent(&play), Some(100));
        // Before the countdown, `ready` stays off while it loads.
        assert_eq!(play.timer(timer::READY), None);
        frame(&mut play, None);
        assert_eq!(play.timer(timer::LOADING), None);
        assert_eq!(percent(&play), Some(100));
    }
}
