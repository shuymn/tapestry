//! The result screen's runtime: its fields, timers, text lines, timing
//! statistics and timelines as of the last view and the last build. The
//! player fills it from each [`ResultView`]
//! ([`Skin::update_result`](crate::Skin::update_result)) and builds a frame
//! from it ([`Skin::build_result`](crate::Skin::build_result)).
//!
//! A view change allocates, bounded by the skin (its text slots, at most
//! twice [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) a string) and the timelines (at most
//! [`MAX_TIMELINE_BINS`](view::MAX_TIMELINE_BINS) bins); a build
//! allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod schema;
pub(crate) mod view;

use bms_model::TimeUs;

use self::schema::{HIDDEN, VALUE_MAX, field, timer};
use self::view::{
    BestView, ResultLabels, ResultNumber, ResultStatus, ResultView, Timelines, TimingStats,
};
use crate::expr::{State, TIMER_OFF};
use crate::select::value;
use crate::widget::Starts;
use crate::widget::glyphs::{self, GlyphLine};
use crate::widget::text::MAX_TEXT_BYTES;
use crate::widget::text::{ChartText, TextTable};

/// The starts of the result screen's built-in timers this frame, each
/// `None` while it is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultTimers {
    /// The screen's entry.
    pub screen: Option<TimeUs>,
    /// The fade-out's start.
    pub fadeout: Option<TimeUs>,
}

/// The longest outro a result skin's declaration is honoured for: 10 s
/// (a provisional interaction limit; design.md "State Model", Result). A
/// longer declaration is clamped to it.
pub const MAX_RESULT_OUTRO: TimeUs = TimeUs(10_000_000);

/// What a result skin declares for its screen's timing (design.md "State
/// Model", Result), in µs: the source's `fadeout`, `input` and `scene`
/// (`skin/json/JsonSkin.java:14-16`), each 0 when not declared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultTiming {
    /// The outro: how long the fade-out (timer 2) plays before retry, back
    /// or the quit key takes effect; 0 to [`MAX_RESULT_OUTRO`].
    pub outro: TimeUs,
    /// How long from the screen's start commands are not taken; 0 or more.
    pub input: TimeUs,
    /// When the screen leaves on its own (back, with its outro), from its
    /// start; `None` when the skin declares no positive time.
    pub scene: Option<TimeUs>,
}

/// What one result frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct ResultFrame<'a> {
    /// The frame's time: timers age on it.
    pub now: TimeUs,
    /// The render target's size in pixels.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a crate::layout::Layout,
    pub timers: ResultTimers,
}

/// Why a result call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResultError {
    /// The skin is not a result skin, or the state is not its.
    #[error("the skin or the state is not a result skin's")]
    Kind,
}

/// The chart strings a result skin's text shows, as of the last view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Strings {
    title: String,
    subtitle: String,
    artist: String,
    subartist: String,
    genre: String,
}

impl Strings {
    pub(crate) fn chart(&self) -> ChartText<'_> {
        ChartText {
            title: &self.title,
            subtitle: &self.subtitle,
            artist: &self.artist,
            subartist: &self.subartist,
            genre: &self.genre,
        }
    }
}

/// The result screen's state as of its last view and build.
/// [`Skin::prepare_result`](crate::Skin::prepare_result) makes a fresh one;
/// a state another skin made, or one a skin of another kind made, draws
/// nothing.
#[derive(Clone, Debug)]
pub struct ResultScreen {
    /// Whether a result skin made it.
    pub(crate) live: bool,
    /// The identity of the skin that made it.
    pub(crate) skin: u64,
    fields: [i32; field::COUNT],
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    pub(crate) strings: Strings,
    pub(crate) text: TextTable,
    pub(crate) bitmap: Box<[Option<GlyphLine>]>,
    timing: Option<TimingStats>,
    timelines: Option<Timelines>,
}

impl ResultScreen {
    /// A fresh state with `whens` `when` timers and `text_slots` text
    /// slots; `live` when a result skin makes it.
    pub(crate) fn new(skin: u64, live: bool, whens: usize, text_slots: usize) -> Self {
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Self {
            live,
            skin,
            fields: [0; field::COUNT],
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            strings: Strings::default(),
            text: TextTable::new(vec![None; text_slots]),
            bitmap: vec![None; text_slots].into_boxed_slice(),
            timing: None,
            timelines: None,
        }
    }

    /// A state with no timer or text slot, which a skin of another kind
    /// gives: nothing accepts it.
    pub(crate) fn inert(skin: u64) -> Self {
        let mut s = Self::new(skin, false, 0, 0);
        s.starts = Box::default();
        s.ms = Box::default();
        s
    }

    /// The fields and timer readings as of the last view and build.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &self.fields,
            timers: &self.ms,
        }
    }

    /// The view's timing statistics; `None` without a sample or a result.
    #[must_use]
    pub fn timing(&self) -> Option<TimingStats> {
        self.timing
    }

    /// The view's timelines; `None` without them or a result.
    #[must_use]
    pub fn timelines(&self) -> Option<&Timelines> {
        self.timelines.as_ref()
    }

    /// The bytes this state holds, itself and every buffer it owns. For a
    /// memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let s = &self.strings;
        let strings = [&s.title, &s.subtitle, &s.artist, &s.subartist, &s.genre]
            .into_iter()
            .map(String::capacity)
            .sum::<usize>();
        let bytes = size_of::<Self>()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + self.text.heap_bytes()
            + glyphs::lines_bytes(&self.bitmap)
            + strings
            + self
                .timelines
                .as_ref()
                .map_or(0, |t| size_of_val(t.bins()) + size_of_val(t.notes()));
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    pub(crate) fn starts(&self) -> Starts<'_> {
        Starts(&self.starts)
    }

    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Fills the fields, strings, timing statistics and timelines from
    /// `view`. Allocates only to replace a string too small for its new
    /// text, with one of exactly that text's length, so each keeps at most
    /// twice [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) however often it
    /// grows; and to copy the timelines, at most
    /// [`MAX_TIMELINE_BINS`](view::MAX_TIMELINE_BINS) bins.
    pub(crate) fn update(&mut self, view: &ResultView, labels: &ResultLabels) {
        self.fields = ResultFields::of(view).0;
        let ready = view.status == ResultStatus::Ready;
        self.timing = view.timing.filter(|_| ready);
        self.timelines = view.timelines.clone().filter(|_| ready);
        let s = &mut self.strings;
        for (to, from) in [
            (&mut s.title, &labels.title),
            (&mut s.subtitle, &labels.subtitle),
            (&mut s.artist, &labels.artist),
            (&mut s.subartist, &labels.subartist),
            (&mut s.genre, &labels.genre),
        ] {
            let from = cut(from);
            if to.capacity() < from.len() {
                *to = String::with_capacity(from.len());
            }
            to.clear();
            to.push_str(from);
        }
    }

    /// Fills this frame's timers: the built-in ones from `timers`, then
    /// each `when` timer over the fields. Allocates nothing.
    pub(crate) fn fill(
        &mut self,
        now: TimeUs,
        timers: &ResultTimers,
        whens: &[crate::expr::Program],
    ) {
        for (start, at) in self.starts.iter_mut().zip([timers.screen, timers.fadeout]) {
            *start = at;
        }
        crate::play::timers::read_screen_timers(
            usize::from(timer::BUILTIN),
            now,
            &self.fields,
            &mut self.starts,
            &mut self.ms,
            whens,
            |_| true,
        );
    }
}

/// `src` cut to twice [`MAX_TEXT_BYTES`] at a character boundary: the
/// text requests cut every string to [`MAX_TEXT_BYTES`] anyway.
fn cut(src: &str) -> &str {
    let mut end = src.len().min(MAX_TEXT_BYTES * 2);
    while !src.is_char_boundary(end) {
        end -= 1;
    }
    src.get(..end).unwrap_or_default()
}

/// `v` as a signed field value, within ±[`VALUE_MAX`].
fn signed(v: i64) -> i32 {
    i32::try_from(v.clamp(-i64::from(VALUE_MAX), i64::from(VALUE_MAX))).unwrap_or(0)
}

/// A best value as its field reads it: shown within ±[`VALUE_MAX`], or
/// [`HIDDEN`].
fn number(n: ResultNumber) -> i32 {
    match n {
        ResultNumber::Shown(v) => signed(i64::from(v)),
        ResultNumber::Hidden => HIDDEN,
    }
}

/// The [`RESULT`](schema::RESULT) fields of one view, by slot: what a
/// result skin's expressions read, in frames ([`Skin::update_result`](crate::Skin::update_result)
/// fills its state with them) and while a skin is imported for that view,
/// so the two cannot differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultFields(pub [i32; field::COUNT]);

impl ResultFields {
    /// The fields of `view`: the chart's under every status; the
    /// result's, the timing's and the best's only while it is ready, each
    /// group's validity false otherwise and its values 0; a hidden best
    /// value `i32::MIN`.
    #[must_use]
    pub fn of(view: &ResultView) -> Self {
        Self(fields(view))
    }

    /// The state an expression over [`RESULT`](schema::RESULT) reads:
    /// these fields, with no timer.
    #[must_use]
    pub fn state(&self) -> State<'_> {
        State {
            fields: &self.0,
            timers: &[],
        }
    }
}

/// The fields of `view`, as [`ResultFields::of`] gives them.
fn fields(view: &ResultView) -> [i32; field::COUNT] {
    let mut f = [0; field::COUNT];
    let mut set = |slot: u16, v: i32| {
        if let Some(s) = f.get_mut(usize::from(slot)) {
            *s = v;
        }
    };
    let c = &view.chart;
    if let Some(level) = c.level {
        set(field::CHART_LEVEL, signed(i64::from(level)));
        set(field::CHART_LEVEL_VALID, 1);
    }
    if let Some(total) = c.total {
        set(field::CHART_TOTAL, value(u64::from(total)));
        set(field::CHART_TOTAL_VALID, 1);
    }
    if let Some(b) = c.bpm {
        set(field::CHART_BPM_MIN_X100, value(u64::from(b.min_x100)));
        set(field::CHART_BPM_MAX_X100, value(u64::from(b.max_x100)));
        set(field::CHART_BPM_CHANGES, i32::from(b.changes));
        set(field::CHART_BPM_VALID, 1);
    }
    set(
        field::CHART_DIFFICULTY,
        c.difficulty
            .filter(|d| (1..=5).contains(d))
            .map_or(0, i32::from),
    );
    if let Some(length) = c.length {
        set(
            field::CHART_LENGTH_MS,
            value(u64::try_from(length.0 / 1_000).unwrap_or(0)),
        );
        set(field::CHART_LENGTH_VALID, 1);
    }
    if let Some(notes) = c.notes {
        set(field::CHART_NOTES, value(u64::from(notes)));
        set(field::CHART_NOTES_VALID, 1);
    }
    if view.status != ResultStatus::Ready {
        return f;
    }
    let s = &view.score;
    let t = &s.tally;
    set(field::RESULT_VALID, 1);
    for (slot, v) in (field::EX_SCORE..).zip([t.ex_score, t.max_combo, t.bp]) {
        set(slot, value(u64::from(v)));
    }
    let counts = [
        t.counts.perfect,
        t.counts.great,
        t.counts.good,
        t.counts.bad,
        t.counts.poor,
        t.empty_poor,
        t.fast(),
        t.slow(),
        t.early.perfect,
        t.late.perfect,
        t.early.great,
        t.late.great,
        t.early.good,
        t.late.good,
        t.early.bad,
        t.late.bad,
        t.early.poor,
        t.late.poor,
        t.empty_poor_early,
        t.empty_poor_late,
    ];
    for (slot, v) in (field::COUNT_PERFECT..).zip(counts) {
        set(slot, value(u64::from(v)));
    }
    set(field::GAUGE, i32::from(s.gauge_permille.min(1_000)));
    if let Some(r) = s.rate {
        set(field::RESULT_RATE_VALID, 1);
        set(field::RATE_CURRENT_BP, value(u64::from(r.rate_bp)));
        set(field::RESULT_GRADE, i32::from(r.grade.min(7)));
    }
    set(field::RESULT_NEXT_RANK, value(u64::from(s.next_rank)));
    set(field::RESULT_CLEAR, s.clear.clear_type());
    set(field::RESULT_CLEARED, i32::from(s.cleared));
    if let Some(timing) = view.timing {
        set(field::TIMING_VALID, 1);
        set(field::TIMING_MEAN_X100, signed(i64::from(timing.mean_x100)));
        set(field::TIMING_SD_X100, value(u64::from(timing.sd_x100)));
    }
    if let BestView::Known(b) = view.best {
        set(field::BEST_KNOWN, 1);
        set(field::BEST_EX, value(u64::from(b.ex)));
        set(field::BEST_EX_DIFF, signed(i64::from(b.ex_diff)));
        set(field::BEST_BP, number(b.bp));
        set(field::BEST_BP_DIFF, number(b.bp_diff));
        set(field::BEST_CLEAR, b.clear.clear_type());
        set(field::BEST_EX_UPDATED, i32::from(b.ex_updated));
        set(field::BEST_BP_UPDATED, i32::from(b.bp_updated));
        if let Some(r) = b.rate {
            set(field::BEST_RATE_VALID, 1);
            set(field::BEST_RATE_BP, value(u64::from(r.rate_bp)));
            set(field::BEST_GRADE, i32::from(r.grade.min(7)));
            set(field::BEST_RATE_UPDATED, i32::from(r.updated));
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::{number, signed};
    use crate::result::view::ResultNumber;

    /// A shown value never reads as a number's reserved values; a hidden
    /// one reads `i32::MIN`.
    #[test]
    fn shown_values_stay_inside_the_reserved_bounds() {
        assert_eq!(number(ResultNumber::Shown(i32::MIN)), -(i32::MAX - 1));
        assert_eq!(number(ResultNumber::Shown(i32::MAX)), i32::MAX - 1);
        assert_eq!(number(ResultNumber::Shown(-3)), -3);
        assert_eq!(number(ResultNumber::Hidden), i32::MIN);
        assert_eq!(signed(i64::MIN), -(i32::MAX - 1));
    }
}
