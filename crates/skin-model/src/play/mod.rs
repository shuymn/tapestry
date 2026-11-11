//! The play screen's state: the per-attempt [`Play`] a skin fills each
//! frame from a [`SkinFrame`], the play schema and the reducer.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod loading;
pub(crate) mod playhead;
pub(crate) mod reducer;
pub(crate) mod schema;
pub(crate) mod timers;

use bms_model::{
    Chart, ChartProfile, LANE_COUNT, Lane, LnMode, NOTE_CATEGORIES, Note, Position,
    PresentationError, Speed, Tempo, TimeUs, TimelineId,
};
use eval::{GaugeState, InputFacts, JudgeEvent, NoteFacts, NoteHold, Section, Tally};
use rules::GaugeValue;

use self::playhead::ExactPlayhead;
use self::reducer::Reducer;
use self::schema::{field, timer};
use self::timers::Settled;
use crate::chart_image::{ChartKey, ChartTable};
use crate::expr::{State, TIMER_OFF};
use crate::layout::Layout;
use crate::selection::Ratio;
use crate::skin::GraphPlan;
use crate::widget::Starts;
use crate::widget::bar::BarScratch;
use crate::widget::bpm_graph::{BpmInput, BpmRaster};
use crate::widget::compose::Run;
use crate::widget::gauge::GaugeLook;
use crate::widget::glyphs::{self, GlyphLine};
use crate::widget::hit_error::HitErrorState;
use crate::widget::lanes::{LaneView, Side};
use crate::widget::ratio_bar::ScoreRate;
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

/// What an attempt takes over from the earlier attempts of its play state
/// (design.md "beatoraja Lua Skins", the program's state): the source keeps
/// its skin's state and its score data (the point, the EX score and the
/// score rates) for as long as its play state lasts (`ScoreDataProperty`
/// of the `MainState`, not reset at `BMSPlayer.java:582` at `0.8.9`).
#[derive(Clone, Copy, Debug, Default)]
pub enum Continuation<'a> {
    /// The play state's first attempt: the slots at their initial values,
    /// the score data the attempt's own, the program run.
    #[default]
    Fresh,
    /// A later attempt of the play state: the score data the play state's
    /// last judgment left, read until this attempt's first judgment
    /// (`None` before any judgment: the attempt's own, as a fresh one's),
    /// and the
    /// slots as the last witnessed run left them, `None` when no such run
    /// carries them (then the callbacks with slots do not run, their
    /// timers off). A program that is not
    /// [`carry_safe`](crate::present::Present::carry_safe) runs no
    /// callback with slots either.
    Carried {
        score: Option<Settled>,
        bank: Option<&'a crate::present::Bank>,
    },
}

/// The target count of `attempt` over `chart`: what its point is over,
/// whatever the skin (the reducer's rule).
#[must_use]
pub fn attempt_targets(chart: &Chart, attempt: &Attempt<'_>) -> u32 {
    let section = match attempt {
        Attempt::Full { .. } => None,
        Attempt::Section(s) => Some((s.start, s.end)),
    };
    reducer::count_targets(chart, section)
}

/// What a play's committed attempt gives the next attempt of its play
/// state: its slots, whether its callbacks with slots ran, whether its last
/// build ran them on a complete view, and its target count.
#[derive(Clone, Copy, Debug)]
pub struct Carry<'a> {
    pub bank: &'a crate::present::Bank,
    pub slots_run: bool,
    pub terminal: bool,
    pub targets: u32,
}

/// A play's best and target EX scores, read once when the play began and
/// kept by each READY rearm of its attempt (stage 3, S10-10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScoreBasis {
    /// The best EX score before this play: 0 when known empty, unavailable
    /// (no library, a failed read) or in a practice round.
    pub best: u32,
    /// Whether the best was read: `false` when unavailable and in a
    /// practice round.
    pub best_known: bool,
    /// The target EX score: 0 in a practice round.
    pub target: u32,
}

/// What an attempt's score figures and graphs read: the play's best and
/// target, the chart's 1× profile and its whole MAIN tempo.
#[derive(Clone, Copy, Debug)]
pub struct PlayScore<'a> {
    /// The play's best and target.
    pub basis: ScoreBasis,
    /// The chart's notes by second and tempo rows over its 1× times;
    /// `None` when the caller has no chart profile.
    pub profile: Option<&'a ChartProfile>,
    /// The whole chart's MAIN tempo, not a practice section's; `None`
    /// when the caller has none.
    pub main: Option<Tempo>,
    /// The preset's key judge areas for the chart's rank, in ms, early
    /// positive: PERFECT, GREAT, GOOD and BAD as `[−late, early]`, the
    /// empty POOR range as `[−behind, ahead]`, which the visualizers band
    /// their offsets by; `None` when the caller has none, which draws every
    /// band empty.
    pub areas: Option<[[i16; 2]; 5]>,
    /// The speed the chart is loaded at: its times are 1× times divided by
    /// it, so a 1× time is a play time times it.
    pub speed: Speed,
}

impl PlayScore<'static> {
    /// No best read, no target, no profile and no MAIN tempo.
    pub const NONE: Self = Self {
        basis: ScoreBasis {
            best: 0,
            best_known: false,
            target: 0,
        },
        profile: None,
        main: None,
        areas: None,
        speed: Speed::NORMAL,
    };
}

/// How many judgment offsets a frame's ring holds.
pub const OFFSET_SLOTS: usize = 100;

/// The play view's newest judgment offsets as a frame gives them, in whole
/// ms, early positive (note time minus press, truncated toward 0), one per
/// judgment PERFECT to BAD: `slots` as the ring stands, the next one going
/// to `count % OFFSET_SLOTS`, and `count` the offsets recorded since the
/// attempt's start or its last rearm, the overwritten ones included.
#[derive(Clone, Copy, Debug)]
pub struct Offsets<'a> {
    pub slots: &'a [i32; OFFSET_SLOTS],
    pub count: u32,
}

/// [`OFFSET_SLOTS`] as a count.
#[expect(clippy::cast_possible_truncation, reason = "100")]
const SLOTS_U32: u32 = OFFSET_SLOTS as u32;

impl Offsets<'static> {
    /// Nothing recorded.
    pub const NONE: Self = Self {
        slots: &[0; OFFSET_SLOTS],
        count: 0,
    };
}

impl Offsets<'_> {
    /// The offsets recorded after the first `seen` still in the ring,
    /// oldest first: the last `min(count − seen, OFFSET_SLOTS)`.
    pub(crate) fn since(self, seen: u32) -> impl Iterator<Item = i32> {
        let new = self.count.wrapping_sub(seen).min(SLOTS_U32);
        let first = self.count.wrapping_sub(new);
        let slots = self.slots;
        (0..new).filter_map(move |i| {
            let at = first.wrapping_add(i) % SLOTS_U32;
            slots.get(usize::try_from(at).ok()?).copied()
        })
    }

    /// The offset recorded `age` judgments ago (0 the newest), if one is
    /// still in the ring.
    pub(crate) fn back(self, age: u32) -> Option<i32> {
        if age >= self.count.min(SLOTS_U32) {
            return None;
        }
        let at = self.count.wrapping_sub(1).wrapping_sub(age) % SLOTS_U32;
        self.slots.get(usize::try_from(at).ok()?).copied()
    }
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
    /// The end of the attempt's notes on the play clock, where
    /// `last_note` starts: the last note's time for the whole chart,
    /// played or autoplayed, and the section's end in a practice round,
    /// both at the play speed.
    pub last_note_end: TimeUs,
    /// The original 1× chart's BPM range, its initial BPM included, over
    /// its whole timeline (not narrowed to a practice section, not scaled
    /// by the play speed), which `bpm.min_x100` and `bpm.max_x100` read;
    /// `None` when the caller has no exact range, which hides them.
    pub bpm_range: Option<BpmRange>,
    /// Whether the same chart's whole BPM extremes differ, uncapped:
    /// `w(min) < w(max)` with `w(t) = min(⌊t⌋, 2³¹ − 1)`, which
    /// `bpm.change` reads; `None` when the caller has no chart, which
    /// clears `bpm.change_valid`.
    pub bpm_whole_change: Option<bool>,
    /// The chart's play level, which `chart.level` reads; `None` when the
    /// caller has no chart, which clears `chart.level_valid`.
    pub play_level: Option<i32>,
    /// Whether the attempt is autoplay, which `autoplay` reads.
    pub autoplay: bool,
    /// The chart's metadata the `chart.*` fields read.
    pub meta: ChartMetadata,
    /// The play's best and target, the chart's profile and its MAIN tempo.
    pub score: PlayScore<'a>,
}

/// A chart's metadata as the `chart.*` fields read it, each `None` when
/// the caller does not know it, which clears its validity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChartMetadata {
    /// Its difficulty, `chart.difficulty`: `#DIFFICULTY` from 1 to 5, 0
    /// for any other or none. A value above 5 is unknown.
    pub difficulty: Option<u8>,
    /// Its judgment class, `chart.judge_class`: 0 the hardest to 4 the
    /// easiest, from its own rank as the reference computes it; `None`
    /// also for a rank in no class. A value above 4 is none.
    pub judge_class: Option<u8>,
    /// Its gauge total, whole, `chart.total`; a negative value or one a
    /// number widget reserves (`i32::MAX`) is unknown.
    pub total: Option<i32>,
    /// Whether it defines a BGA image, `chart.bga`.
    pub bga: Option<bool>,
}

impl ChartMetadata {
    /// Nothing known.
    pub const UNKNOWN: Self = Self {
        difficulty: None,
        judge_class: None,
        total: None,
        bga: None,
    };
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
/// effective amounts ([`LaneView`]): the source projections, in the
/// source's coordinates for imported numbers, and the native block for
/// the player's own HUD, all from one presentation snapshot (design.md
/// "Play readouts"). The producer marks a value past
/// [`READOUT_MAX`](crate::READOUT_MAX) unavailable; a build refuses one
/// as malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameReadouts {
    /// The playback tempo at the display head (`now + display_offset`), in
    /// floored hundredths of a BPM: the initial tempo before chart time 0,
    /// the musical tempo through a STOP; `None` when unknown. 0 is a tempo
    /// below 0.01, not a STOP.
    pub bpm_current_x100: Option<u32>,
    /// The source hi-speed `⌊100 · H / (1 − l)⌋` for the effective lift
    /// `l`, in floored hundredths and not clamped to 20; `None` while `H`
    /// is unknown or no field is left above the lift.
    pub hispeed_x100: Option<u32>,
    /// The source travel duration `240000 · (1 − c) / (B · Hs)` ms at the
    /// current tempo with the effective lane cover, STOP waiting excluded
    /// and unrounded; `None` with the source hi-speed or the tempo
    /// unknown.
    pub duration: Option<DurationRatio>,
    /// The stored cover settings.
    pub covers: CoverSettings,
    /// The eight duration variants, unrounded, in slot order (slots 51 to
    /// 74): the current, MAIN, minimum and maximum tempos, each first
    /// with the configured lane cover, counted while it is off, then with
    /// none; `None` where unavailable.
    pub duration_variants: [Option<DurationRatio>; 8],
    /// The native block's inputs (slots 75 to 85).
    pub native: NativeReadouts,
    /// The speed policy's hi-speed fix (`settings.fix_hispeed`), as the
    /// selection screen's: 0 Classic; Floating 1 from the start tempo, 2
    /// the maximum, 3 the main and 4 the minimum. A value above
    /// [`FIX_HISPEED_MAX`](crate::FIX_HISPEED_MAX) is
    /// malformed.
    pub fix_hispeed: u8,
}

impl FrameReadouts {
    /// Every readout unavailable and every cover off at 0, not being
    /// adjusted: a frame without a presentation snapshot.
    pub const UNAVAILABLE: Self = Self {
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
        duration_variants: [None; 8],
        native: NativeReadouts {
            duration: None,
            target_green: None,
            floating: false,
        },
        fix_hispeed: 0,
    };
}

/// The native block's inputs: the active `H` itself is the lanes' gain
/// ([`LaneView::gain`]) and the whites follow from the stored covers and
/// the lanes' effective lift ([`native_values`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeReadouts {
    /// The exact current travel duration `T = 240000 · v / (B · H)` ms at
    /// the lanes' gain and the effective covers, 0 where no field is
    /// left; `None` with the gain or the tempo unknown.
    pub duration: Option<DurationRatio>,
    /// The target green number G, 1 to 6 000, given in Floating only.
    pub target_green: Option<u32>,
    /// Whether the speed mode is Floating.
    pub floating: bool,
}

/// A duration rounded for the readouts: whole ms, and the green number
/// from the exact ratio × 3/5, each rounded half up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundedDuration {
    /// Whole ms, at most [`READOUT_MAX`](crate::READOUT_MAX).
    pub ms: u32,
    /// The green number, at most [`READOUT_MAX`](crate::READOUT_MAX).
    pub green: u32,
}

/// The native block (slots 75 to 85) as values, for the native HUD and
/// the fields ([`native_values`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeValues {
    /// The lanes' gain in hundredths, rounded half up; `None` without a
    /// gain.
    pub hispeed_x100: Option<u32>,
    /// The native duration, rounded; `None` while unavailable.
    pub duration: Option<RoundedDuration>,
    /// The target green number, Floating only.
    pub target_green: Option<u32>,
    /// SUDDEN+'s white number, `⌊(1 000 − l) · cover / 1 000⌋` of the
    /// configured lane cover and the effective lift `l`.
    pub sudden_white: u16,
    /// LIFT's white number: the configured lift.
    pub lift_white: u16,
    /// HIDDEN+'s white number, `⌊(1 000 − l) · hidden / 1 000⌋` of the
    /// configured hidden cover and the effective lift `l`.
    pub hidden_white: u16,
    /// Whether the speed mode is Floating.
    pub floating: bool,
}

/// What one frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct SkinFrame<'a> {
    /// The frame's chart time: timers age on it.
    pub now: TimeUs,
    /// Added to `now` for the display clock, which `beat` follows.
    pub display_offset: TimeUs,
    /// The state clock: µs since the play state was entered, sampled once
    /// a frame on the monotonic clock (design.md "beatoraja Lua Skins").
    /// Real time, never chart time: it advances through loading, READY, a
    /// paused chart time and a practice session's page.
    pub state_us: i64,
    /// The play view's running totals.
    pub tally: &'a Tally,
    /// The play view's gauges, in the preset's order.
    pub gauges: &'a [GaugeState],
    /// The shown gauge's index into `gauges`; it may change between
    /// frames.
    pub gauge: usize,
    /// The effects drained since the last frame, in emission order, of
    /// the play view's candidate and within its events: they start
    /// animations only.
    pub effects: &'a [JudgeEvent],
    /// The play view's note facts: each note's state and each lane's
    /// visual long-note hold, from which the lanes and the `hold` timers
    /// read, whatever effects were dropped.
    pub notes: &'a NoteFacts,
    /// The play view's judgment offsets, which the visualizers draw.
    pub offsets: Offsets<'a>,
    /// The play view's lane input: each lane held or not, with its latest
    /// press and release, from which the `press` and `release` timers
    /// read.
    pub input: &'a InputFacts,
    /// Whether evaluation is complete (the play view's `complete`): a
    /// section attempt then rings out without notes.
    pub complete: bool,
    /// Whether this frame's update entered the play in its playing state:
    /// false during READY and after a stop, the finish or the fade
    /// out. A gauge failure during the frame's own update leaves it true.
    pub playing: bool,
    /// Where the play entered Closing, on its play clock (the play view's
    /// `closing`): the `failed` timer's start, for the displayed survival
    /// gauge's failure and the quit key alike; `None` outside Closing.
    pub closing: Option<TimeUs>,
    /// The render target's size in pixels; the skin computes its canvas
    /// from it.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a Layout,
    /// The hi-speed and the covers.
    pub lanes: LaneView,
    /// The tempo, hi-speed, duration and stored covers the readouts show.
    /// Each effective amount in `lanes` must be its stored setting while
    /// that cover is on and 0 while it is off, else the build is refused.
    pub readouts: FrameReadouts,
    /// The turntable's angle, clockwise on y-down, in
    /// 1/[`TURNTABLE_UNITS_PER_DEGREE`](crate::TURNTABLE_UNITS_PER_DEGREE)
    /// degree: what a turntable follower adds
    /// ([`AngleFollow::Turntable`](crate::AngleFollow::Turntable)).
    pub turntable: i32,
    /// The application's readouts at the frame: presents per second,
    /// local time and uptime, whatever the attempt's state.
    pub app: AppReadouts,
}

/// The application's readouts at one frame, which the player samples once
/// per frame whatever the screen (design.md "Rendering and Skins", play
/// readouts): the frames presented in the last second, the local date and
/// time, and the uptime on the clock started at launch. Values outside
/// their fields' ranges are malformed: a build refuses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppReadouts {
    /// The frames presented in the last 1 000 000 µs, at most
    /// [`FPS_MAX`](crate::FPS_MAX).
    pub fps: u16,
    /// µs since the player's clock started at launch, at least 0.
    pub uptime_us: i64,
    /// The local date and time.
    pub local: LocalTime,
}

impl AppReadouts {
    /// No present, the launch instant, at 1970-01-01 00:00:00: a frame
    /// that has no application clock, such as a tool's or a test's.
    pub const LAUNCH: Self = Self {
        fps: 0,
        uptime_us: 0,
        local: LocalTime {
            year: 1_970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        },
    };
}

/// A local date and time: month 1 to 12, day 1 to 31, hour 0 to 23,
/// minute and second 0 to 59, year at most
/// [`YEAR_MAX`](crate::YEAR_MAX).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// The chart's load as the play screen shows it before READY
/// ([`LoadingFrame::load`](crate::LoadingFrame::load)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadState {
    /// The loading lifetime's origin on the loading clock: the moment the
    /// play screen began showing its loading state.
    pub since: TimeUs,
    /// How far it is, in %, clamped to 0..=100.
    pub percent: i32,
}

/// Why [`Skin::prepare`](crate::Skin::prepare) refused an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrepareError {
    /// The skin is not a play skin.
    #[error("the skin is not a play skin")]
    Kind,
    #[error("the attempt's lead-in is negative")]
    NegativeLeadIn,
    #[error("the attempt gives {got} initial gauges for {expected} gauges")]
    InitialGauges { expected: usize, got: usize },
    #[error("the attempt's READY state starts outside the time range")]
    Overflow,
    #[error("the attempt's section ends before it starts")]
    InvalidSectionRange,
    #[error(
        "the BPM range's minimum is above its maximum, or its maximum above the readout maximum"
    )]
    InvalidBpmRange,
    /// The chart cannot be presented: its links are broken, or two of its
    /// note endpoints sit at one µs at different positions
    /// ([`Chart::validate_presentation`]). An admitted chart never is.
    #[error("the chart cannot be presented: {0}")]
    Presentation(PresentationError),
    /// A timeline row lies past the scroll map's domain (`±DOMAIN` µs),
    /// where the playhead cannot follow it: a chart built in code with
    /// such a time; the loader refuses one.
    #[error("timeline row {} at {} µs is outside the playhead's range", .row.0, .at.0)]
    TimeOutOfDomain {
        /// The first row past the domain.
        row: TimelineId,
        /// Its time.
        at: TimeUs,
    },
    /// A carried bank that does not fit the program's slots: another
    /// length, or a value outside its slot's domain.
    #[error("the carried slots do not fit the skin's program")]
    Continuation,
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
    /// zero duration denominator, a duration or variant whose rounded ms
    /// or green number is above [`READOUT_MAX`](crate::READOUT_MAX), a
    /// target green number outside 1 to 6 000 or given outside Floating),
    /// or an effective amount in [`SkinFrame::lanes`] is not its stored
    /// cover setting.
    #[error("the frame's readouts are out of range or disagree with the lanes")]
    InvalidReadouts,
    /// The skin is not made for the screen built: a play or loading frame
    /// of a selection skin, or a selection frame of a play skin.
    #[error("the skin is not made for this screen")]
    Kind,
}

/// One attempt's skin state: the reducer, every timer's start and reading,
/// each lane's visual hold and the fields, as of the last build. [`Skin::prepare`](crate::Skin::prepare)
/// makes a fresh one per attempt, so a retry starts clean.
#[derive(Clone, Debug)]
pub struct Play {
    reducer: Reducer,
    /// Per lane, the note facts' visual hold as of the last build, off
    /// once its long note's end is at or before the frame's `now`.
    holds: [Option<NoteHold>; LANE_COUNT],
    /// The context's [`PlayContext::empty_poor_counted`].
    empty_poor_counted: bool,
    /// The error of a refused build, which every later build returns.
    poisoned: Option<BuildError>,
    /// Per timer slot (built-in, then `when`, then the program's timer
    /// callbacks): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    /// The fields, then the presentation program's outputs, two a
    /// callback.
    fields: Box<[i32]>,
    /// The presentation program's slots: fresh at each prepare, kept by
    /// a rearm.
    bank: crate::present::Bank,
    /// Whether the program's callbacks with slots run: a fresh attempt's,
    /// or a carried one's with its slots carried and a
    /// [`carry_safe`](crate::present::Present::carry_safe) program.
    slots_run: bool,
    /// The score data the play state's last judgment left (`None` before
    /// any), which the score fields read until this attempt's first
    /// judgment.
    cached: Option<Settled>,
    /// Whether the attempt has had a judgment: a frame's tally was not the
    /// zero tally. A rearm clears it.
    judged: bool,
    /// Whether the last build ran the callbacks with slots on a complete
    /// view, so the slots as it left them are the round's last.
    terminal: bool,
    /// The attempt's exact playhead, which notes, bar lines and the beat
    /// sample; a rearm keeps it.
    playhead: ExactPlayhead,
    /// The playhead at the last build's display time, sampled once per
    /// frame and read by the beat timer and the lanes.
    display: Position,
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
    /// The context's [`PlayContext::bpm_whole_change`].
    bpm_whole_change: Option<bool>,
    /// The context's [`PlayContext::play_level`].
    play_level: Option<i32>,
    /// The `chart.*` fields of the context's [`PlayContext::meta`], from
    /// `field::CHART_DIFFICULTY` on.
    chart_fields: [i32; timers::CHART_FIELDS],
    /// The context's [`PlayContext::autoplay`].
    pub(crate) autoplay: bool,
    /// The context's play best and target ([`PlayScore::basis`]), which a
    /// rearm keeps.
    score: ScoreBasis,
    /// Whether the chart declares `#RANDOM` or `#SETRANDOM`: the values
    /// measured against the best are then not shown (design.md "Personal
    /// Bests", skin projections).
    random: bool,
    /// The score ratios as of the last build, in
    /// [`ScoreRate::ALL`]'s order; each fill computes them afresh, so a
    /// fresh attempt and a rearm start from their "until updated" values.
    ratios: [Option<Ratio>; 6],
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
    /// The chart images its frames draw and whose fields they give:
    /// absent until set ([`set_chart_images`](Self::set_chart_images)).
    chart: ChartTable,
    /// The live key of the array its frames draw from, set by the frame's
    /// renderer before each build ([`set_chart_live`](Self::set_chart_live)).
    chart_live: Option<ChartKey>,
    /// The bar groups' per-frame work, for a skin with lanes and groups;
    /// a build lends it to the lanes.
    bars: Option<BarScratch>,
    /// The graphs' and visualizers' prepared data and state.
    graphs: Graphs,
}

/// A play's graphs and visualizers: the notes graph's bins and cursor
/// clock, each BPM graph element's pixmap, each timing visualizer
/// element's band and each hit-error element's state, prepared with the
/// attempt.
#[derive(Clone, Debug, Default)]
struct Graphs {
    /// The chart's notes by category, a bin each; empty without a notes
    /// graph or a profile.
    bins: Box<[[u16; NOTE_CATEGORIES]]>,
    /// The chart's 1× seconds the bins span.
    seconds: u32,
    /// The speed the chart is loaded at, as `(num, den)`: a 1× time is a
    /// play time times `num / den`.
    speed: (u32, u32),
    /// A section attempt's start and end in 1× µs.
    section: Option<[i64; 2]>,
    bpm: Box<[BpmRaster]>,
    bands: Box<[Box<[Run]>]>,
    hits: Box<[HitErrorState]>,
    /// The key judge areas the hit-error visualizers band by; all 0
    /// without any.
    areas: [[i16; 2]; 5],
}

impl Graphs {
    /// The graphs of `plan` for `chart` under `ctx`.
    fn new(plan: &GraphPlan, chart: &Chart, ctx: &PlayContext<'_>) -> Self {
        let score = ctx.score;
        let profile = score.profile;
        let (bins, seconds) = match profile.filter(|_| plan.notes) {
            Some(p) => (
                p.bins().into(),
                u32::try_from(p.seconds().len()).unwrap_or(u32::MAX),
            ),
            None => (Box::default(), 0),
        };
        let speed = score.speed.ratio();
        let to_1x = |t: TimeUs| {
            let (n, d) = speed;
            let v = i128::from(t.0) * i128::from(n) / i128::from(d.max(1));
            i64::try_from(v).unwrap_or(0)
        };
        let section = match ctx.attempt {
            Attempt::Section(s) => Some([to_1x(s.start), to_1x(s.end)]),
            Attempt::Full { .. } => None,
        };
        // A chart without a note draws no BPM graph, as the source has no
        // MAIN for it (`skin/SkinBPMGraph.java:169`).
        let any_note = chart
            .lanes
            .iter()
            .flatten()
            .any(|n| !matches!(n.kind, bms_model::NoteKind::Mine { .. }));
        let bpm = plan
            .bpm
            .iter()
            .map(|(def, [cols, rows])| match (profile, score.main) {
                (Some(p), Some(main)) if any_note => {
                    let input = BpmInput {
                        rows: p.speeds(),
                        main,
                        length_ms: chart.meta.length.0 / 1_000,
                    };
                    def.prepare(&input, *cols, *rows)
                }
                _ => BpmRaster::default(),
            })
            .collect();
        Self {
            bins,
            seconds,
            speed,
            section,
            bpm,
            bands: plan.timing.iter().map(|t| t.band(score.areas)).collect(),
            hits: plan.hit.iter().map(|&h| HitErrorState::new(h)).collect(),
            areas: score.areas.unwrap_or([[0; 2]; 5]),
        }
    }

    /// The bytes its buffers hold.
    fn heap_bytes(&self) -> usize {
        size_of_val(&*self.bins)
            + self
                .bpm
                .iter()
                .map(|b| size_of::<BpmRaster>() + size_of_val(&*b.runs))
                .sum::<usize>()
            + self
                .bands
                .iter()
                .map(|b| size_of::<Box<[Run]>>() + size_of_val(&**b))
                .sum::<usize>()
            + self
                .hits
                .iter()
                .map(|h| size_of::<HitErrorState>() + h.heap_bytes())
                .sum::<usize>()
    }
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
    /// owns: the reducer, the timers, the playhead's representatives and
    /// the notes copied for it (the timeline is the chart's, shared), the gauges, the attached and bitmap lines, and
    /// the bar groups' scratch. For a memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let notes = self.notes.as_deref().map_or(0, |n| {
            let lanes: usize = n.lanes.iter().map(|l| size_of_val(&**l)).sum();
            size_of::<Notes>() + lanes + size_of_val(&*n.bars)
        });
        let bytes = size_of::<Self>()
            + self.reducer.heap_bytes()
            + size_of_val(&*self.fields)
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + size_of_val(self.bank.values())
            + self.playhead.heap_bytes()
            + notes
            + size_of_val(&*self.gauges)
            + self.text.heap_bytes()
            + glyphs::lines_bytes(&self.bitmap)
            + self.bars.as_ref().map_or(0, BarScratch::heap_bytes)
            + self.graphs.heap_bytes();
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    /// A fresh attempt with the program `present` (its output slots, its
    /// timers and its slots at their initial values) and `whens` `when`
    /// timers, keeping the chart's
    /// notes and the work of `bar_groups` checked groups when the skin has
    /// `lanes`; grades up to `bomb_up_to` give a bomb.
    #[expect(
        clippy::too_many_arguments,
        reason = "the skin's parts an attempt is prepared from"
    )]
    pub(crate) fn new(
        chart: &Chart,
        ctx: &PlayContext<'_>,
        carry: Continuation<'_>,
        present: Option<&crate::present::Present>,
        whens: usize,
        lanes: bool,
        bar_groups: usize,
        bomb_up_to: Option<schema::Grade>,
        plan: &GraphPlan,
    ) -> Result<Self, PrepareError> {
        let outputs = present.map_or(0, |p| crate::output::slots(p.len()));
        let timers = whens.saturating_add(present.map_or(0, crate::present::Present::timers));
        let mut bank =
            present.map_or_else(crate::present::Bank::default, crate::present::Present::bank);
        let (slots_run, cached) = match carry {
            Continuation::Fresh => (true, None),
            Continuation::Carried { score, bank: None } => (false, score),
            Continuation::Carried {
                score,
                bank: Some(b),
            } => {
                if !present.map_or(b.values().is_empty(), |p| p.fits(b)) {
                    return Err(PrepareError::Continuation);
                }
                bank.copy_from(b);
                (
                    present.is_none_or(crate::present::Present::carry_safe),
                    score,
                )
            }
        };
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
        let bounds = section.map(|s| (s.start, s.end));
        let playhead = ExactPlayhead::prepare(chart, bounds)?;
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
        let slots = usize::from(timer::BUILTIN).saturating_add(timers);
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
            holds: [None; LANE_COUNT],
            empty_poor_counted: ctx.empty_poor_counted,
            poisoned: None,
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            fields: vec![0; field::COUNT.saturating_add(outputs)].into_boxed_slice(),
            bank,
            slots_run,
            cached,
            judged: false,
            terminal: false,
            playhead,
            display: Position(0),
            beat: None,
            ready_at,
            play_at,
            last_note_end: ctx.last_note_end,
            readout_end,
            bpm_range: ctx.bpm_range,
            bpm_whole_change: ctx.bpm_whole_change,
            play_level: ctx.play_level,
            chart_fields: timers::chart_fields(ctx.meta),
            autoplay: ctx.autoplay,
            score: ctx.score.basis,
            random: !chart.identity.random_results.is_empty(),
            ratios: [None; 6],
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
            chart: ChartTable::ABSENT,
            chart_live: None,
            bars: (lanes && bar_groups > 0).then(|| BarScratch::new(bar_groups)),
            graphs: Graphs::new(plan, chart, ctx),
        })
    }

    /// Starts the attempt's runtime state over for a rearmed READY
    /// candidate (design.md "Play Lifecycle", the start gate), in place:
    /// the reducer, the holds, every timer, the beat and the first
    /// judgment's latch, as a fresh attempt has them. The chart's notes,
    /// the prepared playhead, the gauges, the attached and bitmap text, the
    /// program's slots, the cached score data and a refused build stay, and
    /// nothing is allocated.
    pub fn rearm(&mut self) {
        self.reducer.rearm();
        self.judged = false;
        self.terminal = false;
        self.holds = [None; LANE_COUNT];
        self.starts.fill(None);
        self.ms.fill(TIMER_OFF);
        self.beat = None;
        self.graphs.hits.iter_mut().for_each(HitErrorState::reset);
    }

    /// Reads the frame's ring into each hit-error element's state.
    pub(crate) fn read_offsets(&mut self, offsets: Offsets<'_>) {
        let areas = self.graphs.areas;
        for h in &mut self.graphs.hits {
            h.update(offsets, areas);
        }
    }

    /// The notes graph's bins and its cursors at `now`: the play cursor
    /// while the play runs; `None` without bins.
    pub(crate) fn notes_graph(
        &self,
        now: TimeUs,
    ) -> Option<(
        &[[u16; NOTE_CATEGORIES]],
        crate::widget::judge_graph::Cursors,
    )> {
        let g = &self.graphs;
        if g.bins.is_empty() {
            return None;
        }
        let (n, d) = g.speed;
        let play = (now >= self.play_at).then(|| {
            let v = i128::from(now.0) * i128::from(n) / i128::from(d.max(1));
            i64::try_from(v).unwrap_or(0)
        });
        let cursors = crate::widget::judge_graph::Cursors {
            seconds: g.seconds,
            play,
            section: g.section,
        };
        Some((&g.bins, cursors))
    }

    /// BPM graph element `slot`'s prepared pixmap.
    pub(crate) fn bpm_raster(&self, slot: usize) -> Option<&BpmRaster> {
        self.graphs.bpm.get(slot)
    }

    /// Timing visualizer element `slot`'s band.
    pub(crate) fn timing_band(&self, slot: usize) -> Option<&[Run]> {
        self.graphs.bands.get(slot).map(|b| &**b)
    }

    /// Hit-error element `slot`'s state.
    pub(crate) fn hit_error(&self, slot: usize) -> Option<&HitErrorState> {
        self.graphs.hits.get(slot)
    }

    /// The play's best and target, as the context gave them: fixed for
    /// the attempt, a rearm included.
    #[must_use]
    pub const fn score(&self) -> ScoreBasis {
        self.score
    }

    /// Score ratio `rate` as of the last build: `None` before the first
    /// and for the best's two on a `#RANDOM` chart.
    pub(crate) fn score_ratio(&self, rate: ScoreRate) -> Option<Ratio> {
        self.ratios.get(rate.index()).copied().flatten()
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

    /// The field slots, the program's outputs included.
    pub(crate) fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The program's slots as of the last build.
    #[must_use]
    pub fn bank(&self) -> &crate::present::Bank {
        &self.bank
    }

    /// What the next attempt of the play state takes over from this one.
    #[must_use]
    pub fn carry(&self) -> Carry<'_> {
        Carry {
            bank: &self.bank,
            slots_run: self.slots_run,
            terminal: self.terminal,
            targets: self.reducer.targets(),
        }
    }

    /// The number of timer slots: built-in, `when` and the program's.
    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Timer `slot`'s start as of the last build, `None` while off.
    #[must_use]
    pub fn timer(&self, slot: u16) -> Option<TimeUs> {
        self.starts().at(usize::from(slot))
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

    /// Sets the chart images its frames draw and whose fields
    /// (`chart.stagefile`, `chart.backbmp`) they give, from the next
    /// frame on; kept by [`rearm`](Self::rearm). The caller sets them
    /// before the attempt opens and never during it (S10-12 design §5.3).
    /// Allocates nothing.
    pub fn set_chart_images(&mut self, table: ChartTable) {
        self.chart = table;
    }

    /// The chart images set for it.
    #[must_use]
    pub fn chart_images(&self) -> &ChartTable {
        &self.chart
    }

    /// Sets the live key of the array its next frames draw from: the
    /// frame's renderer sets it before each build, from its own state.
    /// Allocates nothing.
    pub fn set_chart_live(&mut self, live: Option<ChartKey>) {
        self.chart_live = live;
    }

    /// The chart images its frames draw: the table set for it when it
    /// describes the live array ([`ChartTable::drawable`]), else none.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(crate) fn drawable_chart(&self) -> &ChartTable {
        if self.chart.drawable(self.chart_live) {
            &self.chart
        } else {
            &ChartTable::ABSENT
        }
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

    /// The attempt's exact playhead.
    #[must_use]
    pub fn playhead(&self) -> &ExactPlayhead {
        &self.playhead
    }

    /// The playhead at the last build's display time (`now` plus the
    /// display offset): the frame's one sample of it.
    #[must_use]
    pub const fn display_position(&self) -> Position {
        self.display
    }

    /// `lane`'s visual hold, as of the last build.
    fn hold(&self, lane: Lane) -> Option<NoteHold> {
        self.holds.get(lane.index()).copied().flatten()
    }

    /// Takes each lane's visual hold from `notes`, off once its long
    /// note's end is at or before `now`, even while the facts, which
    /// follow the finalized watermark, still hold it.
    pub(crate) fn take_holds(&mut self, notes: &NoteFacts, now: TimeUs) {
        for (lane, slot) in Lane::all().zip(&mut self.holds) {
            *slot = notes.hold(lane).filter(|h| h.end > now);
        }
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
    use eval::{GaugeState, JudgeEvent, JudgeEventKind, NoteFacts, NoteHold, Tally};
    use rules::{GaugeValue, PerJudgment};

    use super::{Attempt, FrameReadouts, GaugeDesc, PlayContext, SkinFrame};
    use crate::{
        AtlasLimits, Fit, Gain, LaneView, Layout, QuadBatch, Region, Sheet, Side, Skin, SkinDef,
        Variant,
    };

    const MS: i64 = 1_000;

    /// No tempo, hi-speed or duration, every cover off at 0: the readouts
    /// of a frame whose lanes draw no cover.
    const OFF: FrameReadouts = FrameReadouts::UNAVAILABLE;

    /// The lanes at H 1, no cover.
    const VIEW: LaneView = LaneView {
        gain: Some(Gain::ONE),
        cover: 0,
        lift: 0,
        hidden: 0,
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

    /// The holds the lanes widget and the `hold` timers read come from
    /// each frame's note facts, never from its effects: a dropped
    /// `LnHeld` or a BAD head's judgment lost still holds, a late
    /// `LnHoldEnded` of the held note ends nothing, and a hold reads off
    /// once its long note's end is at the frame's `now`, even while the
    /// facts, which follow the finalized watermark, still have it.
    #[test]
    fn holds_follow_the_note_facts() {
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
            close: None,
            loadend: None,
            present: None,
        })
        .unwrap();
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 2,
            reserved_layers: 1,
        };
        let (_, uvs) = skin
            .atlas(Variant::Lo, limits, &|| false)
            .unwrap()
            .into_parts();
        let gauges = [GaugeDesc {
            start: GaugeValue(0),
            max: GaugeValue::FULL,
            look: crate::GaugeLook::Normal,
            border: GaugeValue(80 * GaugeValue::PERCENT),
        }];
        let ctx = PlayContext {
            meta: crate::ChartMetadata::UNKNOWN,
            autoplay: false,
            gauges: &gauges,
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(3_000 * MS),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
            score: crate::PlayScore::NONE,
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
            ..Default::default()
        };
        let snapshot = [GaugeState {
            value: GaugeValue(0),
            reference: GaugeValue(0),
            failed_at: None,
        }];
        let layout = Layout::AUTHOR;
        let mut batch = QuadBatch::with_capacity(0, 0);
        let mut frame =
            |play: &mut super::Play, now_ms: i64, effects: &[JudgeEvent], notes: &NoteFacts| {
                let frame = SkinFrame {
                    app: crate::AppReadouts::LAUNCH,
                    state_us: 0,
                    now: TimeUs(now_ms * MS),
                    display_offset: TimeUs(0),
                    tally: &tally,
                    gauges: &snapshot,
                    gauge: 0,
                    effects,
                    input: &eval::InputFacts::NONE,
                    notes,
                    offsets: crate::Offsets::NONE,
                    complete: false,
                    playing: true,
                    closing: None,
                    target: [1_920, 1_080],
                    layout: &layout,
                    turntable: 0,
                    lanes: VIEW,
                    readouts: OFF,
                };
                skin.build(play, &frame, &uvs, &mut batch).unwrap();
            };
        let ended = |n: u8, note: usize| JudgeEventKind::LnHoldEnded {
            lane: lane(n),
            note,
        };
        let hold = |note, start: i64, end: i64| NoteHold {
            note,
            start: TimeUs(start * MS),
            end: TimeUs(end * MS),
        };
        let hold_timer = |play: &super::Play, n: u8| {
            play.timer(super::schema::lane_timer(
                super::schema::LaneTimer::Hold,
                lane(n),
            ))
        };
        let mut notes = NoteFacts::new(&chart);
        // Held with its `LnHeld` dropped: the facts hold it.
        notes.set_done(lane(2), 0);
        notes.set_hold(lane(2), Some(hold(0, 1_000, 1_300)));
        frame(&mut play, 1_000, &[], &notes);
        assert_eq!(play.hold_note(lane(2)), Some(0));
        assert!(play.body_active(lane(2), 0));
        assert_eq!(hold_timer(&play, 2), Some(TimeUs(1_000 * MS)));
        // A late `LnHoldEnded` of it changes nothing while the facts hold.
        frame(&mut play, 1_100, &[ev(1_050, ended(2, 0))], &notes);
        assert_eq!(play.hold_note(lane(2)), Some(0));
        // Released: the facts end it.
        notes.set_hold(lane(2), None);
        frame(&mut play, 1_250, &[], &notes);
        assert_eq!(play.hold_note(lane(2)), None);
        assert!(!play.body_active(lane(2), 0));
        assert_eq!(hold_timer(&play, 2), None);
        // A BAD head's hold, its judgment lost.
        notes.set_done(lane(5), 0);
        notes.set_hold(lane(5), Some(hold(0, 1_400, 1_800)));
        frame(&mut play, 1_400, &[], &notes);
        assert_eq!(play.hold_note(lane(5)), Some(0));
        assert!(play.body_active(lane(5), 0));
        // A hold whose end passed reads off though the facts lag.
        notes.set_hold(lane(2), Some(hold(1, 2_000, 2_300)));
        frame(&mut play, 2_299, &[], &notes);
        assert_eq!(play.hold_note(lane(2)), Some(1));
        frame(&mut play, 2_300, &[], &notes);
        assert_eq!(play.hold_note(lane(2)), None);
        assert_eq!(hold_timer(&play, 2), None);
        // A rearm clears the holds until the next frame's facts.
        play.rearm();
        assert_eq!(play.hold_note(lane(5)), None);

        // A refused frame applies none of its effects: the hold its end
        // would close stays.
        let counted = PlayContext {
            empty_poor_counted: true,
            ..ctx
        };
        let mut play = skin.prepare(&chart, &counted).unwrap();
        let mut notes = NoteFacts::new(&chart);
        notes.set_hold(lane(2), Some(hold(0, 1_000, 1_300)));
        frame(&mut play, 1_000, &[], &notes);
        assert_eq!(play.hold_note(lane(2)), Some(0));
        notes.set_hold(lane(2), None);
        let inconsistent = Tally {
            empty_poor: 1,
            ..tally
        };
        let refused = SkinFrame {
            app: crate::AppReadouts::LAUNCH,
            state_us: 0,
            now: TimeUs(1_250 * MS),
            display_offset: TimeUs(0),
            tally: &inconsistent,
            gauges: &snapshot,
            gauge: 0,
            effects: &[ev(1_250, ended(2, 0))],
            notes: &notes,
            offsets: crate::Offsets::NONE,
            input: &eval::InputFacts::NONE,
            complete: false,
            playing: true,
            closing: None,
            target: [1_920, 1_080],
            layout: &layout,
            turntable: 0,
            lanes: VIEW,
            readouts: OFF,
        };
        assert_eq!(
            skin.build(&mut play, &refused, &uvs, &mut batch),
            Err(super::BuildError::InconsistentTally)
        );
        assert_eq!(play.hold_note(lane(2)), Some(0));
        assert_eq!(play.poisoned, Some(super::BuildError::InconsistentTally));
    }

    /// C12 (stage 3, S10-10): the play's best and target, as the context
    /// gave them, stay through a READY rearm, which starts the rest of the
    /// attempt's runtime state over.
    #[test]
    fn a_rearm_keeps_the_score_basis() {
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        let basis = super::ScoreBasis {
            best: 7,
            best_known: true,
            target: 9,
        };
        let ctx = PlayContext {
            meta: crate::ChartMetadata::UNKNOWN,
            autoplay: false,
            gauges: &[],
            attempt: Attempt::Full { lead_in: TimeUs(0) },
            empty_poor_breaks_combo: false,
            empty_poor_counted: false,
            scratch: Side::Left,
            ln_default: rules::LnMode::Ln,
            last_note_end: TimeUs(1_000 * MS),
            bpm_range: None,
            bpm_whole_change: None,
            play_level: None,
            score: super::PlayScore {
                basis,
                ..crate::PlayScore::NONE
            },
        };
        let mut play = super::Play::new(
            &chart,
            &ctx,
            super::Continuation::Fresh,
            None,
            0,
            false,
            0,
            None,
            &crate::skin::GraphPlan::default(),
        )
        .unwrap();
        assert_eq!(play.score(), basis);
        play.rearm();
        assert_eq!(play.score(), basis);
    }
}
