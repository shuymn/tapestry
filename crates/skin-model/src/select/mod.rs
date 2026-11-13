//! The selection screen's runtime: its fields, timers, text lines and
//! rows as of the last view and the last build. The player fills it from
//! each [`SelectionView`] ([`Skin::update_selection`](crate::Skin::update_selection))
//! and builds a frame from it ([`Skin::build_selection`](crate::Skin::build_selection)).
//!
//! A view change allocates, bounded by the skin (its text slots, its row
//! slots, at most [`MAX_TEXT_BYTES`] a string); a build allocates nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

pub(crate) mod schema;

use std::cell::Cell;

use bms_model::{Tempo, TimeUs};

use self::schema::{VALUE_MAX, field, timer};
use crate::chart_image::{ChartImage, ChartKey, ChartTable};
use crate::expr::{State, TIMER_OFF};
use crate::layout::Layout;
use crate::play::timers::app_fields;
use crate::pointer::{RowHit, SearchBounds, Target};
use crate::selection::{
    ChartLabels, Ratio, RowKind, RowScore, RowView, SelectionLabels, SelectionOrder,
    SelectionPlace, SelectionSettings, SelectionView, SpeedReference, SpeedView,
};
use crate::widget::Starts;
use crate::widget::glyphs::{self, GlyphLine};
use crate::widget::song_list::{LampKey, RowBind, RowStyle};
use crate::widget::text::{ChartText, MAX_TEXT_BYTES, TextTable};
use crate::{AppReadouts, BuildError, GaugeLook};

/// The starts of the selection screen's built-in timers this frame, each
/// `None` while it is off. The screen's lifetime and its UI events give
/// them (S10-09e).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionTimers {
    /// The screen's entry.
    pub screen: Option<TimeUs>,
    /// The cursor's latest move.
    pub change: Option<TimeUs>,
    /// Each panel's opening while it is open.
    pub panels: [Option<TimeUs>; 3],
}

/// What one selection frame gives the skin.
#[derive(Clone, Copy, Debug)]
pub struct SelectionFrame<'a> {
    /// The frame's time: timers age on it.
    pub now: TimeUs,
    /// The render target's size in pixels.
    pub target: [u32; 2],
    /// The player's layout.
    pub layout: &'a Layout,
    pub timers: SelectionTimers,
    /// The application's readouts at the frame, sampled at the reading
    /// `now` comes from (design.md "Rendering and Skins", play readouts):
    /// numbers 20 to 29 read them. A value outside its field's range
    /// refuses the frame.
    pub app: AppReadouts,
    /// The pointer, in target pixels; `None` while there is none over the
    /// window. A hover-gated element draws only while it is inside the
    /// element's hover rect.
    pub pointer: Option<[f32; 2]>,
}

/// Why a selection call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    /// The skin is not a selection skin, or the state is not its.
    #[error("the skin or the state is not a selection skin's")]
    Kind,
}

/// The strings a selection skin's text shows, as of the last view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Strings {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) artist: String,
    pub(crate) subartist: String,
    pub(crate) genre: String,
    /// The search box's text.
    pub(crate) query: String,
    /// The place's names, each followed by `" > "`.
    pub(crate) place: String,
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

/// The selection screen's state as of its last view and build.
/// [`Skin::prepare_selection`](crate::Skin::prepare_selection) makes a
/// fresh one; a state another skin made, or one a play skin made, draws
/// nothing.
#[derive(Clone, Debug)]
pub struct Selection {
    /// Whether a selection skin made it.
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
    /// Per song list slot.
    pub(crate) rows: Box<[RowBind]>,
    pub(crate) row_strings: Box<[String]>,
    pub(crate) row_text: TextTable,
    pub(crate) row_bitmap: Box<[Option<GlyphLine>]>,
    pub(crate) position: Option<Ratio>,
    pub(crate) ex_rate: Option<Ratio>,
    /// The best EX play's PERFECT to POOR over its own notes (graphs 140
    /// to 144); `None` without one or with no notes.
    pub(crate) judge_rates: [Option<Ratio>; 5],
    /// The best max combo over the chart's summary notes (graph 145), as
    /// the source divides it by the chart's notes for every chart
    /// (`FloatPropertyFactory.java:155-176`, `:308` at `0.8.9`); `None`
    /// without a summary or with no notes.
    pub(crate) combo_rate: Option<Ratio>,
    /// The system, key and background volumes, as of the last settings;
    /// `None` before any.
    pub(crate) system_volume: Option<Ratio>,
    pub(crate) key_volume: Option<Ratio>,
    pub(crate) background_volume: Option<Ratio>,
    /// Per pointer target in draw order (each changeable slider, action
    /// element, search text and song list element): as the last build
    /// drew it, `None` when it did not draw.
    pub(crate) targets: Box<[Cell<Option<Target>>]>,
    /// Per song list element and slot: the slot's bar as the last build
    /// drew it, `None` when it did not.
    pub(crate) row_hits: Box<[Cell<Option<RowHit>>]>,
    /// Per search text: its input box as the last build that sampled it
    /// placed it, kept across builds (a build that hides it keeps the
    /// bounds before); `None` until one does.
    pub(crate) search: Box<[Cell<Option<SearchBounds>>]>,
    /// The frame time [`Skin::prepare_selection_frame`](crate::Skin::prepare_selection_frame)
    /// last prepared the timers for, which that frame's build takes
    /// without refilling them.
    pub(crate) prepared: Option<TimeUs>,
    /// The cursor's chart's images published for this state's
    /// installation; `None` while they are not current.
    chart: Option<ChartTable>,
    /// The live key of the array its frames draw from, set by the frame's
    /// renderer before each build.
    chart_live: Option<ChartKey>,
}

impl Selection {
    /// A fresh state with `whens` `when` timers, `text_slots` text slots,
    /// `rows` song list slots, and `targets` pointer targets, row hit cells
    /// and `search` search texts' kept bounds; `live` when a selection skin
    /// makes it.
    pub(crate) fn new(
        skin: u64,
        live: bool,
        whens: usize,
        text_slots: usize,
        rows: usize,
        [targets, row_hits, search]: [usize; 3],
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
            rows: vec![RowBind::default(); rows].into_boxed_slice(),
            row_strings: vec![String::new(); rows].into_boxed_slice(),
            row_text: TextTable::new(vec![None; rows]),
            row_bitmap: vec![None; rows].into_boxed_slice(),
            position: None,
            ex_rate: None,
            judge_rates: [None; 5],
            combo_rate: None,
            system_volume: None,
            key_volume: None,
            background_volume: None,
            targets: vec![Cell::new(None); targets].into_boxed_slice(),
            row_hits: vec![Cell::new(None); row_hits].into_boxed_slice(),
            search: vec![Cell::new(None); search].into_boxed_slice(),
            prepared: None,
            chart: None,
            chart_live: None,
        }
    }

    /// A state with no timer, text, row or pointer slot, which a skin of
    /// another kind gives: nothing accepts it.
    pub(crate) fn inert(skin: u64) -> Self {
        let mut s = Self::new(skin, false, 0, 0, 0, [0, 0, 0], 0);
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

    /// The bytes this state holds, itself and every buffer it owns. For a
    /// memory witness; it allocates nothing.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let strings = [
            &self.strings.title,
            &self.strings.subtitle,
            &self.strings.artist,
            &self.strings.subartist,
            &self.strings.genre,
            &self.strings.query,
            &self.strings.place,
        ]
        .into_iter()
        .chain(self.row_strings.iter())
        .map(String::capacity)
        .sum::<usize>();
        let bytes = size_of::<Self>()
            + size_of_val(&*self.starts)
            + size_of_val(&*self.ms)
            + self.text.heap_bytes()
            + glyphs::lines_bytes(&self.bitmap)
            + size_of_val(&*self.rows)
            + size_of_val(&*self.row_strings)
            + strings
            + self.row_text.heap_bytes()
            + glyphs::lines_bytes(&self.row_bitmap)
            + size_of_val(&*self.targets)
            + size_of_val(&*self.row_hits)
            + size_of_val(&*self.search);
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    pub(crate) fn starts(&self) -> Starts<'_> {
        Starts(&self.starts)
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
    /// [`Skin::prepare_selection_frame`](crate::Skin::prepare_selection_frame)
    /// and the build (docs/stage3.md S7-07).
    pub fn runtime_parts(&mut self) -> (&mut [i32], &[i32]) {
        (&mut self.fields, &self.ms)
    }

    pub(crate) fn slots(&self) -> usize {
        self.ms.len()
    }

    /// Sets the cursor's chart's images its frames draw and its image
    /// fields read ([`write_chart_fields`](Self::write_chart_fields)):
    /// `None` while they are not current for the cursor, `Some` once
    /// they are, an absent entry drawing nothing and reading false.
    /// Allocates nothing.
    pub fn set_chart_images(&mut self, table: Option<ChartTable>) {
        self.chart = table;
    }

    /// The chart images set for it; `None` while not current.
    #[must_use]
    pub fn chart_images(&self) -> Option<&ChartTable> {
        self.chart.as_ref()
    }

    /// Sets the live key of the array its next frames draw from: the
    /// frame's renderer sets it before each build, from its own state.
    /// Allocates nothing.
    pub fn set_chart_live(&mut self, live: Option<ChartKey>) {
        self.chart_live = live;
    }

    /// The chart images its frames draw: the table set for it when it
    /// describes the live array ([`ChartTable::drawable`]); `None`, which
    /// draws nothing, otherwise.
    #[must_use]
    pub fn drawable_chart(&self) -> Option<&ChartTable> {
        self.chart.as_ref().filter(|t| t.drawable(self.chart_live))
    }

    /// Writes `selected.stagefile` and `selected.banner` from the table
    /// set ([`set_chart_images`](Self::set_chart_images)): whether it
    /// holds each image, both false while none is set. A view update
    /// resets them, so the player writes them after it and before the
    /// frame's callbacks. Allocates nothing.
    pub fn write_chart_fields(&mut self) {
        let [stage, banner] = [ChartImage::Stage, ChartImage::Banner]
            .map(|which| self.chart.is_some_and(|t| t.present(which)));
        for (slot, v) in [
            (field::SELECTED_STAGEFILE, stage),
            (field::SELECTED_BANNER, banner),
        ] {
            if let Some(f) = self.fields.get_mut(usize::from(slot)) {
                *f = i32::from(v);
            }
        }
    }

    /// Fills the fields, strings, rows and ratios from `view`, the rows
    /// placed around the cursor at `center`, each with its identity in the
    /// view's list. Allocates only to grow a
    /// string past its capacity, each string at most twice
    /// [`MAX_TEXT_BYTES`].
    pub(crate) fn update(&mut self, view: &SelectionView, labels: &SelectionLabels, center: usize) {
        let cursor = view.cursor.and_then(|c| view.rows.get(c));
        let mut fields = fields(view, cursor);
        // The settings' fields stay as the settings left them.
        let from = usize::from(field::SETTINGS);
        if let (Some(to), Some(kept)) =
            (fields.get_mut(from..), self.fields.get(from..field::COUNT))
        {
            to.copy_from_slice(kept);
        }
        if let Some(to) = self.fields.get_mut(..field::COUNT) {
            to.copy_from_slice(&fields);
        }
        self.strings_from(view, labels, cursor);
        let at = view.cursor;
        for (i, (bind, s)) in self
            .rows
            .iter_mut()
            .zip(self.row_strings.iter_mut())
            .enumerate()
        {
            let row = at
                .and_then(|c| (c + i).checked_sub(center))
                .and_then(|r| view.rows.get(r));
            *bind = row.map_or_else(RowBind::default, |r| RowBind::of(r, view.list_revision));
            s.clear();
            if let Some(row) = row {
                row_text(row, labels, s);
            }
        }
        self.position = view.position;
        self.ex_rate = view
            .selected
            .as_ref()
            .and_then(|c| match (c.score, c.max_ex) {
                // A largest of 0 makes no ratio.
                (RowScore::Best { ex, .. }, Some(max)) => Ratio::new(u64::from(ex), max),
                _ => None,
            });
        let selected = view.selected.as_ref();
        let play = selected.and_then(|c| c.best).map(|b| b.play);
        let counts = play.map(|p| judgments(&p.tally));
        for (i, rate) in self.judge_rates.iter_mut().enumerate() {
            *rate = play.zip(counts).and_then(|(p, counts)| {
                let count = counts.get(i).copied().unwrap_or(0);
                Ratio::new(u64::from(count), u64::from(p.notes))
            });
        }
        self.combo_rate = selected.and_then(|c| {
            let s = c.summary?;
            let combo = c.best.map_or(0, |b| b.max_combo);
            Ratio::new(u64::from(combo), u64::from(s.taps) + u64::from(s.longs))
        });
    }

    /// The text strings of the cursor's row and the place (docs/stage3.md
    /// S10-09d, D8).
    fn strings_from(
        &mut self,
        view: &SelectionView,
        labels: &SelectionLabels,
        cursor: Option<&RowView>,
    ) {
        let s = &mut self.strings;
        for f in [
            &mut s.title,
            &mut s.subtitle,
            &mut s.artist,
            &mut s.subartist,
            &mut s.genre,
            &mut s.query,
            &mut s.place,
        ] {
            f.clear();
        }
        match (cursor.map(|r| r.kind), view.selected.as_ref()) {
            (Some(RowKind::Chart), Some(c)) => {
                let ChartLabels {
                    title,
                    subtitle,
                    artist,
                    subartist,
                    genre,
                    ..
                } = &c.labels;
                push(&mut s.title, title);
                push(&mut s.subtitle, subtitle);
                push(&mut s.artist, artist);
                push(&mut s.subartist, subartist);
                push(&mut s.genre, genre);
            }
            (Some(RowKind::Folder { .. }), _) => {
                push(&mut s.title, cursor.map_or("", |r| r.title.as_str()));
            }
            (Some(RowKind::All { .. }), _) => push(&mut s.title, &labels.all_charts),
            _ => {}
        }
        push(&mut s.query, &view.query);
        match &view.place {
            SelectionPlace::Top => {}
            SelectionPlace::Folder(names) => {
                for name in names {
                    push(&mut s.place, name);
                    push(&mut s.place, " > ");
                }
            }
            SelectionPlace::All => {
                push(&mut s.place, &labels.all_charts);
                push(&mut s.place, " > ");
            }
            SelectionPlace::Results => {
                push(&mut s.place, &labels.search_results);
                push(&mut s.place, " > ");
            }
        }
    }

    /// Fills the settings' fields and the three volumes' ratios from `s`,
    /// leaving the view's as they are. Allocates nothing.
    pub(crate) fn settings(&mut self, s: &SelectionSettings) {
        let mut set = |slot: u16, v: i32| {
            if let Some(f) = self.fields.get_mut(usize::from(slot)) {
                *f = v;
            }
        };
        set(
            field::SETTINGS_DISPLAY_OFFSET,
            s.display_offset_ms.clamp(-VALUE_MAX, VALUE_MAX),
        );
        set(
            field::SETTINGS_VOLUME_SYSTEM,
            s.system_volume.number.clamp(0, 100),
        );
        let (floating, duration, green, fix) = match s.speed {
            SpeedView::Classic => (0, 0, 0, 0),
            SpeedView::Floating { reference, green } => {
                let g = i32::try_from(green).unwrap_or(VALUE_MAX).min(VALUE_MAX);
                // ⌊5 · G / 3⌋ ms: the duration whose green number is G
                // (`T = 5 · g / 3`).
                let ms = value(u64::from(green) * 5 / 3);
                let fix = match reference {
                    SpeedReference::Start => 1,
                    SpeedReference::Maximum => 2,
                    SpeedReference::Main => 3,
                    SpeedReference::Minimum => 4,
                };
                (1, ms, g, fix)
            }
        };
        set(field::SETTINGS_FLOATING, floating);
        set(field::SETTINGS_DURATION, duration);
        set(field::SETTINGS_GREEN, green);
        set(field::SETTINGS_FIX_HISPEED, fix);
        set(
            field::SETTINGS_ORDER,
            match s.order {
                SelectionOrder::Title => 0,
                SelectionOrder::Artist => 1,
                SelectionOrder::Level => 4,
                SelectionOrder::Modified => -1,
            },
        );
        set(
            field::SETTINGS_GAUGE,
            s.gauge.map_or(-1, GaugeLook::schema_value),
        );
        let [sudden, lift, hidden] = s.covers_on;
        set(field::SETTINGS_SUDDEN, i32::from(sudden));
        set(field::SETTINGS_LIFT, i32::from(lift));
        set(field::SETTINGS_HIDDEN, i32::from(hidden));
        set(field::SETTINGS_GAUGE_SHIFT, i32::from(s.play_to_end));
        set(
            field::SETTINGS_JUDGE_ALGORITHM,
            match s.assignment {
                // The source's `JudgeAlgorithm` order: Combo 0, Duration 1
                // (`play/JudgeAlgorithm.java:42` at `0.8.9`).
                Some(rules::Assignment::ComboPriority) => 0,
                Some(rules::Assignment::NearestTiesEarlier) => 1,
                None => -1,
            },
        );
        set(
            field::SETTINGS_VOLUME_KEY,
            s.key_volume.number.clamp(0, 100),
        );
        set(
            field::SETTINGS_VOLUME_BACKGROUND,
            s.background_volume.number.clamp(0, 100),
        );
        self.system_volume = Some(s.system_volume.ratio);
        self.key_volume = Some(s.key_volume.ratio);
        self.background_volume = Some(s.background_volume.ratio);
    }

    /// Fills this frame's application readouts from `app`, then its
    /// timers: the built-in ones from `timers`, then each `when` timer over
    /// the fields. Allocates nothing.
    ///
    /// # Errors
    ///
    /// [`BuildError::InvalidReadouts`] for a readout outside its field's
    /// range: nothing is filled.
    pub(crate) fn fill(
        &mut self,
        now: TimeUs,
        timers: &SelectionTimers,
        app: &AppReadouts,
        whens: &[crate::expr::Program],
    ) -> Result<(), BuildError> {
        let app = app_fields(app).ok_or(BuildError::InvalidReadouts)?;
        let at = usize::from(field::APP_FPS);
        if let Some(to) = self.fields.get_mut(at..at + app.len()) {
            to.copy_from_slice(&app);
        }
        let [p1, p2, p3] = timers.panels;
        for (start, at) in self
            .starts
            .iter_mut()
            .zip([timers.screen, timers.change, p1, p2, p3])
        {
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
        Ok(())
    }
}

/// `src` appended to `out` up to twice [`MAX_TEXT_BYTES`] in all, cut at a
/// character boundary: the text requests cut every string to
/// [`MAX_TEXT_BYTES`] anyway.
pub(crate) fn push(out: &mut String, src: &str) {
    let room = (MAX_TEXT_BYTES * 2).saturating_sub(out.len());
    let mut end = src.len().min(room);
    while !src.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(src.get(..end).unwrap_or_default());
}

/// A row's text: a chart's full title, a folder's name, or the "all
/// charts" label (docs/stage3.md S10-09d, D8; `select/bar/SongBar.java:61-63`,
/// `song/SongData.java:312-316`).
fn row_text(row: &RowView, labels: &SelectionLabels, out: &mut String) {
    match RowStyle::of(row.kind) {
        Some(RowStyle::Chart) => {
            push(out, &row.title);
            if !row.subtitle.is_empty() {
                push(out, " ");
                push(out, &row.subtitle);
            }
        }
        Some(RowStyle::Folder) => push(out, &row.title),
        Some(RowStyle::All) => push(out, &labels.all_charts),
        None => {}
    }
}

/// `v` as a field value: at most [`VALUE_MAX`].
pub(crate) fn value(v: u64) -> i32 {
    i32::try_from(v).map_or(VALUE_MAX, |v| v.min(VALUE_MAX))
}

/// A tempo in whole BPM, rounded down.
fn bpm(t: Tempo) -> i32 {
    value(u64::try_from(t.units() / Tempo::UNITS_PER_BPM).unwrap_or(u64::MAX))
}

/// The rank thresholds, in 27ths of the largest EX score
/// (`skin/property/BooleanPropertyFactory.java:280-288`).
const RANK_LOW: [u128; 8] = [0, 6, 9, 12, 15, 18, 21, 24];

/// The rank of `ex` over `max` (positive): the greatest `k` with
/// `27 · ex ≥ RANK_LOW[k] · max`.
pub(crate) fn rank(ex: u64, max: u64) -> i32 {
    let (ex, max) = (u128::from(ex), u128::from(max));
    let k = RANK_LOW
        .iter()
        .rposition(|&low| 27 * ex >= low * max)
        .unwrap_or(0);
    i32::try_from(k).unwrap_or(0)
}

/// The EX score from `ex` to the next rank of 3/27 steps over `max`
/// (positive), or to `max` past the last (`ScoreDataProperty.java:100-109`).
pub(crate) fn next_rank(ex: u64, max: u64) -> u64 {
    let (e, m) = (u128::from(ex), u128::from(max));
    let next = (0..27_u128)
        .step_by(3)
        .find(|&i| 27 * e < i * m)
        .map_or(m.saturating_sub(e), |i| {
            (i * m).div_ceil(27).saturating_sub(e)
        });
    u64::try_from(next).unwrap_or(u64::MAX)
}

/// The judgments of `t`, PERFECT to POOR.
const fn judgments(t: &eval::Tally) -> [u32; 5] {
    let c = &t.counts;
    [c.perfect, c.great, c.good, c.bad, c.poor]
}

/// The fields of `view`, its cursor's row `cursor`.
fn fields(view: &SelectionView, cursor: Option<&RowView>) -> [i32; field::COUNT] {
    let mut f = [0; field::COUNT];
    let mut set = |slot: u16, v: i32| {
        if let Some(s) = f.get_mut(usize::from(slot)) {
            *s = v;
        }
    };
    match cursor.map(|r| r.kind) {
        Some(RowKind::Chart) => set(field::CURSOR_CHART, 1),
        Some(RowKind::Folder { charts }) => {
            set(field::CURSOR_FOLDER, 1);
            set(field::CURSOR_CHARTS, value(u64::from(charts)));
        }
        Some(RowKind::All { charts }) => {
            set(field::CURSOR_ALL, 1);
            set(field::CURSOR_CHARTS, value(u64::from(charts)));
        }
        Some(RowKind::Heading) | None => {}
    }
    let Some(c) = view.selected.as_ref() else {
        return f;
    };
    match c.score {
        RowScore::Unplayed => {}
        RowScore::NoBest { plays } => {
            set(field::SELECTED_PLAYED, 1);
            set(field::SELECTED_PLAYS, value(plays));
        }
        RowScore::Best { plays, ex, bp, .. } => {
            set(field::SELECTED_PLAYED, 1);
            set(field::SELECTED_PLAYS, value(plays));
            set(field::SELECTED_BEST, 1);
            set(field::SELECTED_EX, value(u64::from(ex)));
            set(field::SELECTED_BP, value(u64::from(bp)));
        }
    }
    if let RowScore::NoBest { plays } | RowScore::Best { plays, .. } = c.score {
        set(field::SELECTED_CLEARS, value(c.clears));
        set(field::SELECTED_FAILS, value(plays.saturating_sub(c.clears)));
    }
    if let Some(b) = c.best {
        set(field::SELECTED_MAX_COMBO, value(u64::from(b.max_combo)));
        let p = &b.play;
        let t = &p.tally;
        set(field::SELECTED_POINT, value(p.point()));
        let [perfect, great, good, bad, poor] = judgments(t).map(u64::from);
        let empty = u64::from(t.empty_poor);
        let counts = [
            perfect,
            great,
            good,
            bad,
            poor,
            empty,
            u64::from(t.fast()),
            u64::from(t.slow()),
            u64::from(t.early.perfect),
            u64::from(t.late.perfect),
            u64::from(t.early.great),
            u64::from(t.late.great),
            u64::from(t.early.good),
            u64::from(t.late.good),
            u64::from(t.early.bad),
            u64::from(t.late.bad),
            u64::from(t.early.poor),
            u64::from(t.late.poor),
            u64::from(t.empty_poor_early),
            u64::from(t.empty_poor_late),
            bad + poor,
            poor + empty,
            bad + poor + empty,
        ];
        for (slot, v) in (field::SELECTED_COUNT_PERFECT..).zip(counts) {
            set(slot, value(v));
        }
        // Each count's whole percent of the play's notes, as the source's
        // `getJudgeCount(i) * 100 / getNotes()`
        // (`skin/property/IntegerPropertyFactory.java:503-509` at `0.8.9`).
        let notes = u64::from(p.notes);
        if notes > 0 {
            set(field::SELECTED_PLAY_NOTES_VALID, 1);
        }
        for (slot, n) in (field::SELECTED_RATE_PERFECT..).zip([perfect, great, good, bad, poor]) {
            if let Some(rate) = (n * 100).checked_div(notes) {
                set(slot, value(rate));
            }
        }
    }
    let lamp = LampKey::of(c.score).index();
    set(field::SELECTED_LAMP, i32::try_from(lamp).unwrap_or(0));
    if let Some(max) = c.max_ex {
        set(field::SELECTED_MAX_EX_VALID, 1);
        set(field::SELECTED_MAX_EX, value(max));
        if let RowScore::Best { ex, .. } = c.score
            && max > 0
        {
            let (e, m) = (u128::from(ex), u128::from(max));
            set(field::SELECTED_RATE_VALID, 1);
            set(
                field::SELECTED_RATE_PERCENT,
                value(u64::try_from(100 * e / m).unwrap_or(u64::MAX)),
            );
            set(
                field::SELECTED_RATE_HUNDREDTHS,
                i32::try_from(10_000 * e / m % 100).unwrap_or(0),
            );
            set(field::SELECTED_RANK, rank(u64::from(ex), max));
            set(
                field::SELECTED_NEXT_RANK,
                value(next_rank(u64::from(ex), max)),
            );
        }
    }
    if let Some(s) = c.summary {
        set(field::SELECTED_SUMMARY, 1);
        set(
            field::SELECTED_NOTES,
            value(u64::from(s.taps) + u64::from(s.longs)),
        );
        set(field::SELECTED_BPM_MAX, bpm(s.tempo_max));
        set(field::SELECTED_BPM_MIN, bpm(s.tempo_min));
        set(
            field::SELECTED_BPM_CHANGE,
            i32::from(s.tempo_min != s.tempo_max),
        );
        let secs = u64::try_from(s.length.0.max(0) / 1_000_000).unwrap_or(0);
        set(field::SELECTED_LENGTH_MIN, value(secs / 60));
        set(
            field::SELECTED_LENGTH_SEC,
            i32::try_from(secs % 60).unwrap_or(0),
        );
    }
    set(
        field::SELECTED_LEVEL,
        c.labels.level_value.clamp(-VALUE_MAX, VALUE_MAX),
    );
    if let Some(k) = c
        .judge_class
        .filter(|&k| i32::from(k) <= crate::JUDGE_CLASS_MAX)
    {
        set(field::SELECTED_JUDGE_CLASS, i32::from(k));
        set(field::SELECTED_JUDGE_CLASS_VALID, 1);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::{next_rank, rank};

    /// Every rank threshold, at it and one point below it.
    #[test]
    fn every_rank_threshold_is_exact() {
        // 27 notes at 2 a note: a 27th is 2 points.
        for (k, low) in [6_u64, 9, 12, 15, 18, 21, 24].into_iter().enumerate() {
            let k = i32::try_from(k).unwrap() + 1;
            assert_eq!(rank(2 * low, 54), k, "at {low}/27");
            assert_eq!(rank(2 * low - 1, 54), k - 1, "below {low}/27");
        }
        // Above the largest, as the source's rate is never clamped.
        assert_eq!(rank(81, 54), 7);
    }

    /// Ranks are exact over 27ths: each threshold at, just below and just
    /// above it.
    #[test]
    fn ranks_and_the_next_rank_are_exact() {
        // 27 notes at 2 a note: the largest is 54, a 27th is 2.
        let max = 54;
        assert_eq!(rank(0, max), 0);
        assert_eq!(rank(11, max), 0);
        assert_eq!(rank(12, max), 1);
        assert_eq!(rank(47, max), 6);
        assert_eq!(rank(48, max), 7);
        assert_eq!(rank(54, max), 7);
        // The next multiple of 3/27 above: 6/27 is 12.
        assert_eq!(next_rank(0, max), 6);
        assert_eq!(next_rank(11, max), 1);
        assert_eq!(next_rank(12, max), 6);
        // Past 24/27, to the largest.
        assert_eq!(next_rank(49, max), 5);
        // A threshold between two points rounds up.
        assert_eq!(next_rank(0, 10), 2);
    }
}
