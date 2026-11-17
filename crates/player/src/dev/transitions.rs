//! A scenario run's transition record (dev-tools, design.md "Shortening
//! Launch-Based Checks", transition record).
//!
//! The render thread's frame sink keeps fixed-size records of the presented
//! frames in a book allocated before the run starts: a **transition** at
//! each change of a frame's state or of that state's lifetime, a
//! **presentation** change when only what drew it changes, a **span**
//! summary closing each run of frames of one state and lifetime, and the
//! **marks** that annotate time: the pending presentation's hold, the
//! result's preparation and the sink's own capture read-backs. Nothing is
//! printed during the run: once the render thread has ended, the main
//! thread formats the book ([`Transitions::lines`]).
//!
//! Every gap between two presents keeps its raw duration. Marks are
//! annotations, not exemptions: [`coverage`] clips each mark to the gap,
//! merges overlapping marks so no time is counted twice, and keeps what no
//! mark covers, its largest uncovered piece with its endpoints; a count of
//! frames not presented covers no time.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use bms_model::TimeUs;

use crate::measure::{HoldEnd, HoldStamp, Stage, State};

/// The records a scenario run keeps (provisional: about 170 for the exit
/// scenario, 20 to 50 for one gate chain). Past it each record is counted
/// as lost and the run fails.
pub const MAX_RECORDS: usize = 4096;

/// The marks that may annotate one gap; a mark past them is counted as
/// dropped and covers nothing.
pub const MAX_MARKS: usize = 8;

/// What a mark annotates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    /// The render thread held a loading lifetime's first frame (design.md
    /// "The pending presentation"), and why the hold ended.
    Hold(Option<HoldEnd>),
    /// The result was prepared: from the play's end to the render thread's
    /// taking its presentation (design.md "State Model", ResultPreparing).
    Preparing,
    /// The frame sink read a captured frame back.
    ReadBack,
}

/// A marked interval of time, on the clock of the presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// What it annotates.
    pub kind: MarkKind,
    /// Its start.
    pub from: TimeUs,
    /// Its end.
    pub to: TimeUs,
}

/// How much of a gap marks cover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    /// The µs some mark covers, overlaps counted once.
    pub covered: i64,
    /// The µs no mark covers.
    pub uncovered: i64,
    /// The largest piece no mark covers, with its endpoints.
    pub piece: Option<(TimeUs, TimeUs)>,
}

/// The time between two presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    /// The earlier present.
    pub from: TimeUs,
    /// The later present.
    pub to: TimeUs,
    /// What marks cover of it.
    pub cover: Coverage,
    /// The frames not presented inside it, which cover nothing.
    pub unpresented: u32,
}

impl Gap {
    /// Its raw duration, µs.
    #[must_use]
    pub const fn len(&self) -> i64 {
        self.to.0 - self.from.0
    }
}

/// A change between two presented frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// Whether the state or its lifetime changed, or only what drew it.
    pub presentation: bool,
    /// The last frame's stage before the change.
    pub from: Stage,
    /// The first frame's stage after it.
    pub to: Stage,
    /// The interval from the last present before it to the first after.
    pub gap: Gap,
}

/// A run of presented frames of one state and lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Its stage, as its first frame drew it.
    pub stage: Stage,
    /// Its presented frames.
    pub frames: u32,
    /// The frames not presented between its first and last.
    pub unpresented: u32,
    /// Its first present.
    pub first: TimeUs,
    /// Its last present.
    pub last: TimeUs,
    /// Its longest gap between presents, raw.
    pub longest: Option<Gap>,
    /// Its gap with the most time no mark covers, when any is uncovered.
    pub uncovered: Option<Gap>,
}

/// One record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// A transition or a presentation change.
    Edge(Edge),
    /// A span's summary.
    Span(Span),
    /// A mark.
    Mark(Mark),
}

/// The records of a run, kept in place.
#[derive(Debug)]
pub struct Book {
    records: Vec<Record>,
    cap: usize,
    lost: u64,
    readbacks: u64,
    dropped: u64,
}

impl Book {
    fn push(&mut self, r: Record) {
        if self.records.len() < self.cap {
            self.records.push(r);
        } else {
            self.lost = self.lost.saturating_add(1);
        }
    }
}

/// The run's book, shared by the render thread's sink and the main
/// thread, which reads it once the render thread has ended.
#[derive(Clone, Debug)]
pub struct Transitions(Arc<Mutex<Book>>);

impl Default for Transitions {
    fn default() -> Self {
        Self::with_capacity(MAX_RECORDS)
    }
}

impl Transitions {
    /// A book of `cap` records, allocated now.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self(Arc::new(Mutex::new(Book {
            records: Vec::with_capacity(cap),
            cap,
            lost: 0,
            readbacks: 0,
            dropped: 0,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, r: Record) {
        let mut b = self.lock();
        if let Record::Mark(Mark {
            kind: MarkKind::ReadBack,
            ..
        }) = r
        {
            b.readbacks = b.readbacks.saturating_add(1);
        }
        b.push(r);
    }

    fn dropped_mark(&self) {
        let mut b = self.lock();
        b.dropped = b.dropped.saturating_add(1);
    }

    /// The records kept so far (tests).
    #[cfg(test)]
    pub fn records(&self) -> Vec<Record> {
        self.lock().records.clone()
    }

    /// The records lost past the cap (tests).
    #[cfg(test)]
    pub fn lost(&self) -> u64 {
        self.lock().lost
    }

    /// Holds the book, as a render thread that never ended would (tests).
    #[cfg(test)]
    pub fn hold(&self) -> MutexGuard<'_, Book> {
        self.lock()
    }

    /// The book's lines (design.md "Shortening Launch-Based Checks"),
    /// formatted off the render thread once it has ended, and the run's
    /// failure if records were lost; an error when the book is still held
    /// (a render thread that did not end).
    ///
    /// # Errors
    ///
    /// The book is held by another thread.
    pub fn lines(&self) -> Result<(Vec<String>, Option<String>), String> {
        let b = match self.0.try_lock() {
            Ok(b) => b,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err("the transition record is held by the render thread".to_owned());
            }
        };
        Ok(format_book(&b))
    }
}

/// The book's lines, then its failure if records were lost.
fn format_book(b: &Book) -> (Vec<String>, Option<String>) {
    let tag = if b.readbacks > 0 {
        ", instrumented"
    } else {
        ""
    };
    let mut lines = Vec::with_capacity(b.records.len() + 1);
    // The largest uncovered time of any gap or interval, and where.
    let mut worst: Option<(Gap, String)> = None;
    let mut consider = |gap: Gap, at: &dyn Fn() -> String| {
        if gap.cover.uncovered > 0
            && worst
                .as_ref()
                .is_none_or(|(w, _)| gap.cover.uncovered > w.cover.uncovered)
        {
            worst = Some((gap, at()));
        }
    };
    for r in &b.records {
        let line = match r {
            Record::Edge(e) if e.presentation => format!(
                "dev: presentation {} {}: {} -> {}: {} us, last {} us, first {} us{}{tag}",
                e.to.state.name(),
                e.to.lifetime,
                e.from.drew.name(),
                e.to.drew.name(),
                e.gap.len(),
                e.gap.from.0,
                e.gap.to.0,
                gap_cover(&e.gap),
            ),
            Record::Edge(e) => {
                consider(e.gap, &|| {
                    format!(
                        "transition {} -> {}",
                        e.from.state.name(),
                        e.to.state.name()
                    )
                });
                format!(
                    "dev: transition {} -> {}: {} us, last {} us, first {} us{}{tag}",
                    e.from.state.name(),
                    e.to.state.name(),
                    e.gap.len(),
                    e.gap.from.0,
                    e.gap.to.0,
                    gap_cover(&e.gap),
                )
            }
            Record::Span(s) => {
                if let Some(g) = s.uncovered {
                    consider(g, &|| {
                        format!("span {} {}", s.stage.state.name(), s.stage.lifetime)
                    });
                }
                span_line(s, tag)
            }
            Record::Mark(m) => {
                let ended = match m.kind {
                    MarkKind::Hold(Some(why)) => format!(" (ended by {})", why.name()),
                    _ => String::new(),
                };
                format!(
                    "dev: mark {} {} to {} us{ended}{tag}",
                    mark_name(m.kind),
                    m.from.0,
                    m.to.0
                )
            }
        };
        lines.push(line);
    }
    let largest = worst.map_or_else(
        || "none".to_owned(),
        |(g, at)| match g.cover.piece {
            Some((a, b)) => format!("{} us ({} to {}) in {at}", g.cover.uncovered, a.0, b.0),
            None => format!("{} us in {at}", g.cover.uncovered),
        },
    );
    lines.push(format!(
        "dev: transitions: {} records, lost {}, marks dropped {}, largest uncovered {largest}{tag}",
        b.records.len(),
        b.lost,
        b.dropped
    ));
    let failure = (b.lost > 0).then(|| {
        format!(
            "{} transition records were lost past the cap of {}",
            b.lost, b.cap
        )
    });
    (lines, failure)
}

/// A gap's covered and uncovered time, and its frames not presented.
fn gap_cover(g: &Gap) -> String {
    let mut out = format!(
        ", covered {} us, uncovered {} us",
        g.cover.covered, g.cover.uncovered
    );
    if let Some((a, b)) = g.cover.piece {
        let _ = write!(out, " ({} to {})", a.0, b.0);
    }
    if g.unpresented > 0 {
        let _ = write!(out, ", {} not presented", g.unpresented);
    }
    out
}

/// A span's summary line.
fn span_line(s: &Span, tag: &str) -> String {
    // The longest raw gap, its uncovered time and that time's largest
    // piece.
    let longest = s.longest.map_or_else(
        || "none".to_owned(),
        |g| {
            let piece = g
                .cover
                .piece
                .map_or_else(String::new, |(a, b)| format!(", {} to {}", a.0, b.0));
            format!(
                "{} us ({} to {}, uncovered {} us{piece})",
                g.len(),
                g.from.0,
                g.to.0,
                g.cover.uncovered
            )
        },
    );
    let uncovered = s.uncovered.map_or_else(
        || "none".to_owned(),
        |g| {
            let piece = g
                .cover
                .piece
                .map_or_else(String::new, |(a, b)| format!(", {} to {}", a.0, b.0));
            format!(
                "{} us of {} us ({} to {}{piece})",
                g.cover.uncovered,
                g.len(),
                g.from.0,
                g.to.0
            )
        },
    );
    format!(
        "dev: span {} {}: frames {}, not presented {}, first {} us, last {} us, longest gap {longest}, largest uncovered gap {uncovered}{tag}",
        s.stage.state.name(),
        s.stage.lifetime,
        s.frames,
        s.unpresented,
        s.first.0,
        s.last.0,
    )
}

/// A mark kind's name in its line.
const fn mark_name(k: MarkKind) -> &'static str {
    match k {
        MarkKind::Hold(_) => "hold",
        MarkKind::Preparing => "preparing",
        MarkKind::ReadBack => "read-back",
    }
}

/// How much of the gap from `from` to `to` the marks cover: each clipped to
/// the gap, overlaps merged so no time counts twice, and the largest piece
/// no mark covers. Allocates nothing.
#[must_use]
pub fn coverage(from: TimeUs, to: TimeUs, marks: &[Option<Mark>]) -> Coverage {
    let (g0, g1) = (from.0, to.0);
    // The clipped intervals; empty slots sort last.
    let mut clipped = [(i64::MAX, i64::MAX); MAX_MARKS];
    for (slot, m) in clipped.iter_mut().zip(marks.iter().flatten()) {
        let (a, b) = (m.from.0.max(g0), m.to.0.min(g1));
        if a < b {
            *slot = (a, b);
        }
    }
    clipped.sort_unstable();
    let mut cursor = g0;
    let mut uncovered = 0;
    let mut piece: Option<(TimeUs, TimeUs)> = None;
    let mut open = |a: i64, b: i64, uncovered: &mut i64| {
        if b > a {
            *uncovered += b - a;
            if piece.is_none_or(|(p0, p1)| b - a > p1.0 - p0.0) {
                piece = Some((TimeUs(a), TimeUs(b)));
            }
        }
    };
    for &(a, b) in clipped.iter().take_while(|&&(a, _)| a != i64::MAX) {
        open(cursor, a, &mut uncovered);
        cursor = cursor.max(b);
    }
    open(cursor, g1, &mut uncovered);
    Coverage {
        covered: (g1 - g0) - uncovered,
        uncovered,
        piece,
    }
}

/// The render thread's recorder: it sees each frame the sink sees and
/// writes finished records into the book; it allocates nothing.
#[derive(Debug)]
pub struct Recorder {
    book: Transitions,
    /// The last presented frame's stage and present.
    last: Option<(Stage, TimeUs)>,
    /// The span the last presented frame belongs to.
    open: Option<Span>,
    /// The marks that may still lie in a gap to come.
    marks: [Option<Mark>; MAX_MARKS],
    /// The frames not presented since the last present.
    unpresented: u32,
    /// The play's end, while its result is prepared.
    preparing: Option<TimeUs>,
    /// The open span's longest and largest uncovered gaps as they stand
    /// if the preparation ends in a result: each gap since the play's end
    /// covered by the preparation's mark from its start, which is recorded
    /// only once the result is taken (design.md "State Model",
    /// ResultPreparing). A preparation withdrawn drops them.
    prepared: Option<(Option<Gap>, Option<Gap>)>,
    /// The loading lifetime whose hold is marked.
    held: Option<u64>,
}

impl Recorder {
    /// A recorder writing into `book`.
    #[must_use]
    pub fn new(book: Transitions) -> Self {
        Self {
            book,
            last: None,
            open: None,
            marks: [None; MAX_MARKS],
            unpresented: 0,
            preparing: None,
            prepared: None,
            held: None,
        }
    }

    /// A frame that was not presented.
    pub const fn unpresented(&mut self) {
        self.unpresented = self.unpresented.saturating_add(1);
    }

    /// A presented frame of `stage` at `at`, with its loading lifetime's
    /// hold if the render thread held it. Returns the change it recorded:
    /// a transition, or a presentation change.
    pub fn frame(&mut self, stage: Stage, at: TimeUs, hold: Option<HoldStamp>) -> Option<Edge> {
        if let Some(h) = hold {
            self.hold(h);
        }
        // A play that ended without a result: its preparation is no
        // result's, and marks nothing. So does one a change to another
        // state than the result's leaves (a retry withdrawing it).
        let changed = self
            .last
            .is_some_and(|(l, _)| l.state != stage.state || l.lifetime != stage.lifetime);
        if self.preparing.is_some()
            && stage.state != State::Result
            && (changed || !is_play(stage.state))
        {
            self.preparing = None;
            self.prepared = None;
        }
        let unpresented = std::mem::take(&mut self.unpresented);
        let mut change = None;
        match (self.last, self.open) {
            (Some((last, last_at)), Some(mut s)) => {
                let gap = Gap {
                    from: last_at,
                    to: at,
                    cover: coverage(last_at, at, &self.marks),
                    unpresented,
                };
                if last.state != stage.state || last.lifetime != stage.lifetime {
                    self.book.push(Record::Span(s));
                    let edge = Edge {
                        presentation: false,
                        from: last,
                        to: stage,
                        gap,
                    };
                    self.book.push(Record::Edge(edge));
                    change = Some(edge);
                    self.open = Some(Self::opened(stage, at));
                } else {
                    if last.drew != stage.drew {
                        let edge = Edge {
                            presentation: true,
                            from: last,
                            to: stage,
                            gap,
                        };
                        self.book.push(Record::Edge(edge));
                        change = Some(edge);
                    }
                    s.frames = s.frames.saturating_add(1);
                    s.unpresented = s.unpresented.saturating_add(unpresented);
                    s.last = at;
                    if let Some(from) = self.preparing {
                        let with = Gap {
                            cover: coverage(last_at, at, &self.with_preparation(from, at)),
                            ..gap
                        };
                        if let Some((longest, uncovered)) = &mut self.prepared {
                            Self::widen(longest, uncovered, with);
                        }
                    }
                    Self::widen(&mut s.longest, &mut s.uncovered, gap);
                    self.open = Some(s);
                }
            }
            _ => self.open = Some(Self::opened(stage, at)),
        }
        self.last = Some((stage, at));
        // A mark that ended by this present lies in no later gap.
        for slot in &mut self.marks {
            if slot.is_some_and(|m| m.to.0 <= at.0) {
                *slot = None;
            }
        }
        change
    }

    /// `gap` into a span's longest and largest uncovered gaps.
    fn widen(longest: &mut Option<Gap>, uncovered: &mut Option<Gap>, gap: Gap) {
        if longest.is_none_or(|l| gap.len() > l.len()) {
            *longest = Some(gap);
        }
        if gap.cover.uncovered > 0
            && uncovered.is_none_or(|u| gap.cover.uncovered > u.cover.uncovered)
        {
            *uncovered = Some(gap);
        }
    }

    /// The kept marks with the preparation's from `from` to `at`, in a free
    /// slot; with none free, its mark would be dropped and covers nothing.
    fn with_preparation(&self, from: TimeUs, at: TimeUs) -> [Option<Mark>; MAX_MARKS] {
        let mut marks = self.marks;
        if let Some(slot) = marks.iter_mut().find(|s| s.is_none()) {
            *slot = Some(Mark {
                kind: MarkKind::Preparing,
                from,
                to: at,
            });
        }
        marks
    }

    const fn opened(stage: Stage, at: TimeUs) -> Span {
        Span {
            stage,
            frames: 1,
            unpresented: 0,
            first: at,
            last: at,
            longest: None,
            uncovered: None,
        }
    }

    /// The hold of loading lifetime `h.lifetime`, marked once per lifetime.
    pub fn hold(&mut self, h: HoldStamp) {
        if self.held != Some(h.lifetime) {
            self.held = Some(h.lifetime);
            self.mark(Mark {
                kind: MarkKind::Hold(h.ended_by),
                from: h.from,
                to: h.to,
            });
        }
    }

    /// Records `m` and keeps it for the gaps it may lie in.
    pub fn mark(&mut self, m: Mark) {
        self.book.push(Record::Mark(m));
        if let Some(slot) = self.marks.iter_mut().find(|s| s.is_none()) {
            *slot = Some(m);
        } else {
            self.book.dropped_mark();
        }
    }

    /// The play's in-play interval ended at `at`.
    pub fn play_ended(&mut self, at: TimeUs) {
        self.preparing = Some(at);
        self.prepared = Some(self.open.map_or((None, None), |s| (s.longest, s.uncovered)));
    }

    /// The render thread took a result's presentation or the built-in
    /// result's page at `at`: the result was prepared since the play ended.
    pub fn result_taken(&mut self, at: TimeUs) {
        if let Some(from) = self.preparing.take() {
            if let (Some((longest, uncovered)), Some(s)) = (self.prepared.take(), &mut self.open) {
                s.longest = longest;
                s.uncovered = uncovered;
            }
            self.mark(Mark {
                kind: MarkKind::Preparing,
                from,
                to: at,
            });
        }
    }

    /// The run ended: the open span is recorded.
    pub fn close(&mut self) {
        if let Some(open) = self.open.take() {
            self.book.push(Record::Span(open));
        }
    }
}

/// Whether `s` is one of a play's states.
const fn is_play(s: State) -> bool {
    matches!(
        s,
        State::PlayPending | State::Ready | State::Play | State::Tail | State::Closing
    )
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{Mark, MarkKind, Record, Recorder, Span, Transitions, coverage};
    use crate::measure::{Drew, HoldEnd, HoldStamp, Stage, State};

    fn stage(state: State, lifetime: u64, drew: Drew) -> Stage {
        Stage {
            state,
            lifetime,
            drew,
        }
    }

    fn read_back(from: i64, to: i64) -> Mark {
        Mark {
            kind: MarkKind::ReadBack,
            from: TimeUs(from),
            to: TimeUs(to),
        }
    }

    fn spans(t: &Transitions) -> Vec<Span> {
        t.records()
            .into_iter()
            .filter_map(|r| match r {
                Record::Span(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// A 3 s gap holding a 400 ms read-back, a 500 ms hold that overlaps it
    /// by 100 ms and a frame not presented: the marks cover 800 ms, never
    /// 900, the frame covers nothing, and the 2.2 s after the hold stay
    /// uncovered, with their endpoints.
    #[test]
    fn coverage_excuses_only_the_marked_part() {
        let book = Transitions::with_capacity(32);
        let mut r = Recorder::new(book.clone());
        let ready = stage(State::Ready, 41, Drew::Plain);
        r.frame(ready, TimeUs(9_983_300), None);
        r.frame(ready, TimeUs(10_000_000), None);
        r.mark(read_back(10_000_000, 10_400_000));
        r.unpresented();
        r.frame(
            ready,
            TimeUs(13_000_000),
            Some(HoldStamp {
                lifetime: 9,
                from: TimeUs(10_300_000),
                to: TimeUs(10_800_000),
                ended_by: Some(HoldEnd::Bound),
            }),
        );
        r.frame(
            stage(State::Play, 41, Drew::Plain),
            TimeUs(13_016_700),
            None,
        );
        let [s] = spans(&book)[..] else {
            panic!("one span: {:?}", book.records());
        };
        assert_eq!((s.frames, s.unpresented), (3, 1));
        let g = s.longest.unwrap();
        assert_eq!(
            (g.from, g.to, g.unpresented),
            (TimeUs(10_000_000), TimeUs(13_000_000), 1)
        );
        assert_eq!((g.cover.covered, g.cover.uncovered), (800_000, 2_200_000));
        assert_eq!(
            g.cover.piece,
            Some((TimeUs(10_800_000), TimeUs(13_000_000)))
        );
        assert_eq!(s.uncovered, Some(g));
    }

    /// A gap inside a hold is covered whole: nothing uncovered, no piece.
    #[test]
    fn a_gap_inside_a_hold_is_covered_whole() {
        let hold = Mark {
            kind: MarkKind::Hold(Some(HoldEnd::Skin)),
            from: TimeUs(20_000_000),
            to: TimeUs(20_600_000),
        };
        let c = coverage(TimeUs(20_100_000), TimeUs(20_500_000), &[Some(hold)]);
        assert_eq!((c.covered, c.uncovered, c.piece), (400_000, 0, None));
    }

    /// A ninth mark in one gap is counted as dropped and covers nothing:
    /// eight 1 µs read-backs cover 8 µs of a 1 s gap, and the ninth's
    /// 500 ms stay uncovered.
    #[test]
    fn a_mark_past_the_slots_covers_nothing() {
        let book = Transitions::with_capacity(32);
        let mut r = Recorder::new(book.clone());
        let play = stage(State::Play, 7, Drew::Plain);
        r.frame(play, TimeUs(5_000_000), None);
        for k in 0..8 {
            r.mark(read_back(5_000_100 + k * 10, 5_000_101 + k * 10));
        }
        r.mark(read_back(5_200_000, 5_700_000));
        r.frame(play, TimeUs(6_000_000), None);
        r.close();
        let [s] = spans(&book)[..] else {
            panic!("one span: {:?}", book.records());
        };
        assert_eq!(s.longest.unwrap().cover.uncovered, 1_000_000 - 8);
        let (lines, _) = book.lines().unwrap();
        assert!(
            lines.last().unwrap().contains("marks dropped 1"),
            "{lines:?}"
        );
    }

    /// A change of what draws a state is a presentation record, not a span
    /// of its own: the pending presentation giving way to the skin on
    /// loading lifetime 7.
    #[test]
    fn presentation_changes_are_not_spans() {
        let book = Transitions::with_capacity(32);
        let mut r = Recorder::new(book.clone());
        r.frame(
            stage(State::Decision, 3, Drew::Skin),
            TimeUs(1_000_000),
            None,
        );
        r.frame(
            stage(State::Loading, 7, Drew::Pending),
            TimeUs(1_516_000),
            None,
        );
        r.frame(
            stage(State::Loading, 7, Drew::Pending),
            TimeUs(1_532_700),
            None,
        );
        r.frame(
            stage(State::Loading, 7, Drew::Skin),
            TimeUs(1_549_300),
            None,
        );
        r.frame(
            stage(State::Loading, 7, Drew::Skin),
            TimeUs(1_566_000),
            None,
        );
        r.close();
        let presentations: Vec<_> = book
            .records()
            .into_iter()
            .filter_map(|r| match r {
                Record::Edge(e) if e.presentation => Some(e),
                _ => None,
            })
            .collect();
        let [p] = presentations[..] else {
            panic!("one presentation change: {:?}", book.records());
        };
        assert_eq!((p.from.drew, p.to.drew), (Drew::Pending, Drew::Skin));
        assert_eq!(
            (p.gap.from, p.gap.to),
            (TimeUs(1_532_700), TimeUs(1_549_300))
        );
        let loading: Vec<_> = spans(&book)
            .into_iter()
            .filter(|s| s.stage.state == State::Loading)
            .map(|s| s.frames)
            .collect();
        assert_eq!(loading, [4], "{:?}", book.records());
    }

    /// Past its cap the book counts each record as lost, and the run fails
    /// naming how many; a book within its cap gives no failure.
    #[test]
    fn overflow_counts_lost_records_and_fails_the_run() {
        let book = Transitions::with_capacity(4);
        let mut r = Recorder::new(book.clone());
        for k in 0..6 {
            r.mark(read_back(k * 1_000, k * 1_000 + 500));
        }
        assert_eq!((book.records().len(), book.lost()), (4, 2));
        let (_, failure) = book.lines().unwrap();
        assert_eq!(
            failure.as_deref(),
            Some("2 transition records were lost past the cap of 4")
        );
        let within = Transitions::with_capacity(4);
        Recorder::new(within.clone()).mark(read_back(0, 1));
        assert_eq!(within.lines().unwrap().1, None);
    }

    /// The book's lines, formatted after the run: each record kind in
    /// order, a read-back making the run instrumented, the summary naming
    /// the largest uncovered time and where; a book still held is the
    /// run's failure.
    #[test]
    fn the_record_formats_each_line_and_refuses_while_held() {
        let book = Transitions::with_capacity(16);
        let mut r = Recorder::new(book.clone());
        r.frame(
            stage(State::Loading, 7, Drew::Skin),
            TimeUs(2_000_300),
            None,
        );
        r.frame(
            stage(State::Ready, 90, Drew::Plain),
            TimeUs(2_250_100),
            None,
        );
        r.mark(read_back(2_250_200, 2_300_200));
        r.frame(
            stage(State::Ready, 90, Drew::Plain),
            TimeUs(2_400_100),
            None,
        );
        r.close();
        let (lines, failure) = book.lines().unwrap();
        assert_eq!(failure, None);
        assert_eq!(
            lines,
            [
                "dev: span loading 7: frames 1, not presented 0, first 2000300 us, last 2000300 us, \
                 longest gap none, largest uncovered gap none, instrumented",
                "dev: transition loading -> ready: 249800 us, last 2000300 us, first 2250100 us, \
                 covered 0 us, uncovered 249800 us (2000300 to 2250100), instrumented",
                "dev: mark read-back 2250200 to 2300200 us, instrumented",
                "dev: span ready 90: frames 2, not presented 0, first 2250100 us, last 2400100 us, \
                 longest gap 150000 us (2250100 to 2400100, uncovered 100000 us, 2300200 to 2400100), \
                 largest uncovered gap 100000 us of 150000 us (2250100 to 2400100, 2300200 to 2400100), \
                 instrumented",
                "dev: transitions: 4 records, lost 0, marks dropped 0, largest uncovered 249800 us \
                 (2000300 to 2250100) in transition loading -> ready, instrumented",
            ]
        );
        let held = book.hold();
        assert_eq!(
            book.lines().unwrap_err(),
            "the transition record is held by the render thread"
        );
        drop(held);
    }

    /// Every line form, from records of fixed values: a fully covered
    /// transition (no piece), a presentation change with frames not
    /// presented, each hold ending and an unended hold, a preparation, a
    /// dropped mark, and a summary with no uncovered time.
    #[test]
    fn every_line_form_is_pinned() {
        use super::{Coverage, Edge, Gap};
        let book = Transitions::with_capacity(16);
        let gap = |from: i64, to: i64, cover: Coverage, unpresented: u32| Gap {
            from: TimeUs(from),
            to: TimeUs(to),
            cover,
            unpresented,
        };
        book.push(Record::Edge(Edge {
            presentation: false,
            from: stage(State::Loading, 7, Drew::Skin),
            to: stage(State::Ready, 90, Drew::Plain),
            gap: gap(
                1_000_000,
                1_200_000,
                Coverage {
                    covered: 200_000,
                    uncovered: 0,
                    piece: None,
                },
                0,
            ),
        }));
        book.push(Record::Edge(Edge {
            presentation: true,
            from: stage(State::Decision, 3, Drew::Skin),
            to: stage(State::Decision, 3, Drew::Fallback),
            gap: gap(
                2_000_000,
                2_016_700,
                Coverage {
                    covered: 0,
                    uncovered: 16_700,
                    piece: Some((TimeUs(2_000_000), TimeUs(2_016_700))),
                },
                2,
            ),
        }));
        for kind in [
            MarkKind::Hold(Some(HoldEnd::Bound)),
            MarkKind::Hold(Some(HoldEnd::Skin)),
            MarkKind::Hold(Some(HoldEnd::Image)),
            MarkKind::Hold(Some(HoldEnd::DeviceLost)),
            MarkKind::Hold(None),
            MarkKind::Preparing,
        ] {
            book.push(Record::Mark(Mark {
                kind,
                from: TimeUs(10),
                to: TimeUs(20),
            }));
        }
        book.dropped_mark();
        let (lines, failure) = book.lines().unwrap();
        assert_eq!(failure, None);
        assert_eq!(
            lines,
            [
                "dev: transition loading -> ready: 200000 us, last 1000000 us, first 1200000 us, \
                 covered 200000 us, uncovered 0 us",
                "dev: presentation decision 3: skin -> fallback: 16700 us, last 2000000 us, \
                 first 2016700 us, covered 0 us, uncovered 16700 us (2000000 to 2016700), \
                 2 not presented",
                "dev: mark hold 10 to 20 us (ended by bound)",
                "dev: mark hold 10 to 20 us (ended by skin)",
                "dev: mark hold 10 to 20 us (ended by image)",
                "dev: mark hold 10 to 20 us (ended by device-lost)",
                "dev: mark hold 10 to 20 us",
                "dev: mark preparing 10 to 20 us",
                "dev: transitions: 8 records, lost 0, marks dropped 1, largest uncovered none",
            ]
        );
    }
}
