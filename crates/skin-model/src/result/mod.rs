//! The result screen's runtime: its fields, timers, text lines, timing
//! statistics and timelines as of the last view and the last build. The
//! player fills it from each [`ResultView`]
//! ([`Skin::update_result`](crate::Skin::update_result)) and builds a frame
//! from it ([`Skin::build_result`](crate::Skin::build_result)).
//!
//! A view change allocates, bounded by the skin (its text slots, at most
//! twice [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) a string) and the timelines (at most
//! [`MAX_TIMELINE_BINS`](view::MAX_TIMELINE_BINS) bins); the gauge's
//! history and the deviations are copied into storage the state makes
//! once; a build allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod schema;
pub(crate) mod view;

use bms_model::TimeUs;

use self::schema::{HIDDEN, VALUE_MAX, field, timer};
use self::view::{
    BestView, DEVIATION_SLOTS, GRAPH_COLUMNS, GaugeColumn, GaugeHistory, ResultLabels,
    ResultNumber, ResultStatus, ResultView, Timelines, TimingStats,
};
use crate::JUDGE_CLASS_MAX;
use crate::chart_image::{ChartKey, ChartTable};
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

/// The chart strings a result or decision skin's text shows, as of the
/// last view or labels.
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

    /// Sets the strings to `labels`, each as [`kept`] keeps it.
    /// Allocates only to replace a string too small for its new text, with
    /// one of exactly that text's length, so each keeps at most twice
    /// [`MAX_TEXT_BYTES`] however often it grows.
    pub(crate) fn set(&mut self, labels: &ResultLabels) {
        for (to, from) in [
            (&mut self.title, &labels.title),
            (&mut self.subtitle, &labels.subtitle),
            (&mut self.artist, &labels.artist),
            (&mut self.subartist, &labels.subartist),
            (&mut self.genre, &labels.genre),
        ] {
            let len = kept(from).map(char::len_utf8).sum();
            if to.capacity() < len {
                *to = String::with_capacity(len);
            }
            to.clear();
            to.extend(kept(from));
        }
    }

    /// The bytes the strings hold.
    pub(crate) fn capacity(&self) -> usize {
        [
            &self.title,
            &self.subtitle,
            &self.artist,
            &self.subartist,
            &self.genre,
        ]
        .into_iter()
        .map(String::capacity)
        .sum()
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
    /// The schema's fields, then the callbacks' output slots.
    fields: Box<[i32]>,
    /// Per timer slot (built-in, then `when`): its start, `None` while off.
    starts: Box<[Option<TimeUs>]>,
    /// Per timer slot: elapsed ms, or [`TIMER_OFF`].
    ms: Box<[i32]>,
    pub(crate) strings: Strings,
    pub(crate) text: TextTable,
    pub(crate) bitmap: Box<[Option<GlyphLine>]>,
    timing: Option<TimingStats>,
    timelines: Option<Timelines>,
    /// The view's emphasized gauge, valid while `has_gauge`.
    gauge: Box<GaugeHistory>,
    has_gauge: bool,
    /// The view's deviations, valid while `has_deviations`.
    deviations: Box<[u32; DEVIATION_SLOTS]>,
    has_deviations: bool,
    areas: Option<[[i16; 2]; 5]>,
    /// The frame time [`Skin::prepare_result_frame`](crate::Skin::prepare_result_frame)
    /// last prepared the timers for, which that frame's build takes
    /// without refilling them.
    pub(crate) prepared: Option<TimeUs>,
    /// The chart images its frames draw (−100 and −101), of the array the
    /// view's staging wrote them into, and the live key of the array its
    /// frames draw from: they draw only while equal.
    chart: ChartTable,
    chart_live: Option<ChartKey>,
}

/// An empty history, the gauge storage's first contents.
const NO_GAUGE: GaugeHistory = GaugeHistory {
    columns: [GaugeColumn {
        low: rules::GaugeValue(0),
        high: rules::GaugeValue(0),
        last: rules::GaugeValue(0),
    }; GRAPH_COLUMNS],
    max: rules::GaugeValue(0),
    border: rules::GaugeValue(0),
    look: crate::GaugeLook::Normal,
    reached: 0,
    no_input_from: None,
};

impl ResultScreen {
    /// A fresh state with `whens` `when` timers and `text_slots` text
    /// slots; `live` when a result skin makes it.
    pub(crate) fn new(
        skin: u64,
        live: bool,
        whens: usize,
        text_slots: usize,
        outputs: usize,
    ) -> Self {
        let slots = usize::from(timer::BUILTIN).saturating_add(whens);
        Self {
            live,
            skin,
            fields: vec![0; field::COUNT.saturating_add(crate::output::slots(outputs))]
                .into_boxed_slice(),
            starts: vec![None; slots].into_boxed_slice(),
            ms: vec![TIMER_OFF; slots].into_boxed_slice(),
            strings: Strings::default(),
            text: TextTable::new(vec![None; text_slots]),
            bitmap: vec![None; text_slots].into_boxed_slice(),
            timing: None,
            timelines: None,
            gauge: Box::new(NO_GAUGE),
            has_gauge: false,
            deviations: Box::new([0; DEVIATION_SLOTS]),
            has_deviations: false,
            areas: None,
            prepared: None,
            chart: ChartTable::ABSENT,
            chart_live: None,
        }
    }

    /// Sets the chart images its frames draw, keyed to the array its
    /// view's staging wrote them into. Their fields are the attempt's
    /// resolved availability, set with the view, not this table.
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
    /// frame's renderer sets it before each build. Allocates nothing.
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

    /// A state with no timer or text slot, which a skin of another kind
    /// gives: nothing accepts it.
    pub(crate) fn inert(skin: u64) -> Self {
        let mut s = Self::new(skin, false, 0, 0, 0);
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

    /// The view's emphasized gauge; `None` without one or a result.
    pub(crate) fn gauge(&self) -> Option<&GaugeHistory> {
        self.has_gauge.then_some(&*self.gauge)
    }

    /// The view's deviations; `None` without them or a result.
    pub(crate) fn deviations(&self) -> Option<&[u32; DEVIATION_SLOTS]> {
        self.has_deviations.then_some(&*self.deviations)
    }

    /// The view's judge areas; `None` without them or a result.
    pub(crate) fn areas(&self) -> Option<[[i16; 2]; 5]> {
        self.areas
    }

    /// The callbacks' output slots, after the schema's fields: what the
    /// screen's runtime writes each frame before the build reads them
    /// (docs/stage3.md S7-07).
    pub fn outputs_mut(&mut self) -> &mut [i32] {
        self.fields.get_mut(field::COUNT..).unwrap_or_default()
    }

    /// The fields (the schema's, then the callbacks' output slots) to
    /// write, and the timer readings of the frame last prepared: what a
    /// runtime frame reads and writes between
    /// [`Skin::prepare_result_frame`](crate::Skin::prepare_result_frame)
    /// and the build (docs/stage3.md S7-07).
    pub fn runtime_parts(&mut self) -> (&mut [i32], &[i32]) {
        (&mut self.fields, &self.ms)
    }

    /// Whether the view is ready: its result's fields are valid.
    pub(crate) fn ready(&self) -> bool {
        self.fields
            .get(usize::from(field::RESULT_VALID))
            .is_some_and(|&v| v != 0)
    }

    /// The bytes this state holds, itself and every buffer it owns. For a
    /// memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let strings = self.strings.capacity();
        let bytes = size_of::<Self>()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + size_of_val(&*self.gauge)
            + size_of_val(&*self.deviations)
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

    /// Fills the fields, strings, timing statistics, timelines, gauge
    /// history, deviations and judge areas from `view`. Allocates only to replace a string too small for its new
    /// text, with one of exactly that text's length, so each keeps at most
    /// twice [`MAX_TEXT_BYTES`](crate::MAX_TEXT_BYTES) however often it
    /// grows; and to copy the timelines, at most
    /// [`MAX_TIMELINE_BINS`](view::MAX_TIMELINE_BINS) bins.
    pub(crate) fn update(&mut self, view: &ResultView, labels: &ResultLabels) {
        if let Some(to) = self.fields.get_mut(..field::COUNT) {
            to.copy_from_slice(&ResultFields::of(view).0);
        }
        let ready = view.status == ResultStatus::Ready;
        self.timing = view.timing.filter(|_| ready);
        self.timelines = view.timelines.clone().filter(|_| ready);
        self.has_gauge = false;
        if let Some(g) = view.gauge.as_deref().filter(|_| ready) {
            *self.gauge = *g;
            self.has_gauge = true;
        }
        self.has_deviations = false;
        if let Some(d) = view.deviations.as_deref().filter(|_| ready) {
            *self.deviations = *d;
            self.has_deviations = true;
        }
        self.areas = view.timing_areas.filter(|_| ready);
        self.strings.set(labels);
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

/// The characters of `src` a label keeps: its characters but control
/// characters, in order, while they fit twice [`MAX_TEXT_BYTES`]. The text
/// drops control characters before it cuts a line to [`MAX_TEXT_BYTES`]
/// (`text::clean`), so dropping them first and keeping more than a line
/// gives the lines play gives. A label of control characters alone keeps
/// its first one, which the text drops: its field stays not empty as
/// written, which a joined field reads (`TextValue::resolve`).
fn kept(src: &str) -> impl Iterator<Item = char> + '_ {
    let mut len = 0_usize;
    let shown = src
        .chars()
        .filter(|c| !c.is_control())
        .take_while(move |c| {
            len += c.len_utf8();
            len <= MAX_TEXT_BYTES * 2
        });
    let only_control = !src.is_empty() && src.chars().all(char::is_control);
    shown.chain(src.chars().next().filter(|_| only_control))
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
    if let Some(k) = c.judge_class.filter(|&k| i32::from(k) <= JUDGE_CLASS_MAX) {
        set(field::CHART_JUDGE_CLASS, i32::from(k));
        set(field::CHART_JUDGE_CLASS_VALID, 1);
    }
    set(field::CHART_STAGEFILE, i32::from(c.images.stage));
    set(field::CHART_BACKBMP, i32::from(c.images.back));
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
    if let Some(d) = s.duration_us {
        set(field::DURATION_VALID, 1);
        set(field::DURATION_10US, value(u64::from(d / 10)));
    }
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
