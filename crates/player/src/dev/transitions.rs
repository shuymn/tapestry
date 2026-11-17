//! A scenario run's transition record (dev-tools, design.md "Shortening
//! Launch-Based Checks", transition record).
//!
//! The render thread's frame sink appends fixed-size raw records to a book
//! allocated before the run starts: each present, with the stage it was
//! drawn for and the frames not presented before it; each mark that
//! annotates time (the pending presentation's hold, the sink's own capture
//! read-backs); the play's end; and the result taken. Nothing is derived
//! or printed during the run: once the render thread has ended, the main
//! thread derives the record from them ([`Transitions::lines`]): a
//! **transition** at each change of a frame's state or of that state's
//! lifetime, a **presentation** change when only what drew it changes, a
//! **span** summary closing each run of frames of one state and lifetime,
//! and the marks, the result's preparation among them, paired from the
//! play's end and the result taken.
//!
//! Every gap between two presents keeps its raw duration. Marks are
//! annotations, not exemptions: [`coverage`] clips every mark to the gap,
//! merges overlapping marks so no time is counted twice, and keeps what no
//! mark covers, its largest uncovered piece with its endpoints; a count of
//! frames not presented covers no time.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use bms_model::TimeUs;

use crate::measure::{HoldEnd, HoldStamp, Stage, State};

/// The raw records a scenario run keeps: its presents and marks
/// (provisional, sized for the 600 s render timeout at about 60 presents a
/// second, with room for the marks). Past it each record is counted as
/// lost and the run fails.
pub const MAX_RECORDS: usize = 65_536;

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

/// A change between two presented frames, as the render thread sees it
/// when the later one is presented (for its capture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// Whether only what drew the frames changed, not their state or its
    /// lifetime.
    pub presentation: bool,
    /// The last frame's stage before the change.
    pub from: Stage,
    /// The first frame's stage after it.
    pub to: Stage,
    /// The last present before the change.
    pub last: TimeUs,
    /// The first present after it.
    pub first: TimeUs,
}

/// A change between two presented frames, with its interval's coverage.
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

/// One record of the derived transition record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// A transition or a presentation change.
    Edge(Edge),
    /// A span's summary.
    Span(Span),
    /// A mark.
    Mark(Mark),
}

/// What the render thread appends to the book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Raw {
    /// A presented frame of `stage` at `at`, after `unpresented` frames
    /// not presented since the last present.
    Present {
        stage: Stage,
        at: TimeUs,
        unpresented: u32,
    },
    /// A hold or a read-back.
    Mark(Mark),
    /// The play's in-play interval ended.
    PlayEnded(TimeUs),
    /// The render thread took a result's presentation or the built-in
    /// result's page.
    ResultTaken(TimeUs),
}

/// The raw records of a run, kept in place.
#[derive(Debug)]
pub struct Book {
    raw: Vec<Raw>,
    cap: usize,
    lost: u64,
}

impl Book {
    /// Keeps `r`, or counts it lost past the cap.
    fn push(&mut self, r: Raw) {
        if self.raw.len() < self.cap {
            self.raw.push(r);
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
    /// A book of `cap` raw records, allocated now.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self(Arc::new(Mutex::new(Book {
            raw: Vec::with_capacity(cap),
            cap,
            lost: 0,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, r: Raw) {
        self.lock().push(r);
    }

    /// The record derived from the raw records kept so far (tests).
    #[cfg(test)]
    pub fn records(&self) -> Vec<Record> {
        derive(&self.lock().raw)
    }

    /// The raw records lost past the cap (tests).
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
    /// derived and formatted off the render thread once it has ended, and
    /// the run's failure if raw records were lost; an error when the book
    /// is still held (a render thread that did not end).
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
        let instrumented = b.raw.iter().any(|r| {
            matches!(
                r,
                Raw::Mark(Mark {
                    kind: MarkKind::ReadBack,
                    ..
                })
            )
        });
        let lines = format_records(&derive(&b.raw), instrumented, b.lost);
        let failure = (b.lost > 0).then(|| {
            format!(
                "{} transition records were lost past the cap of {}",
                b.lost, b.cap
            )
        });
        Ok((lines, failure))
    }
}

/// Whether the frame of `stage` after one of `last` is a change: `Some`
/// with whether only what drew it changed, `None` within a span drawn
/// alike.
fn change(last: Stage, stage: Stage) -> Option<bool> {
    if last.state != stage.state || last.lifetime != stage.lifetime {
        Some(false)
    } else if last.drew != stage.drew {
        Some(true)
    } else {
        None
    }
}

/// Every mark of the run, each with the raw record its line follows: the
/// recorded holds and read-backs, and each result's preparation, from the
/// play's end to the result taken. A play that ended without a result
/// marks nothing: a frame of another attempt (a retry withdrawing it), or
/// of a state outside the play other than the result (its practice page,
/// selection), withdraws its preparation; a change of phase within the
/// attempt (its stopped view, the tail or Closing, presented after its
/// end) keeps it.
fn marks_of(raw: &[Raw]) -> Vec<(usize, Mark)> {
    let mut marks = Vec::new();
    let mut preparing: Option<TimeUs> = None;
    let mut last: Option<Stage> = None;
    for (i, r) in raw.iter().enumerate() {
        match *r {
            Raw::Mark(m) => marks.push((i, m)),
            Raw::PlayEnded(at) => preparing = Some(at),
            Raw::Present { stage, .. } => {
                let other_attempt = last.is_some_and(|l| l.lifetime != stage.lifetime);
                if stage.state != State::Result && (!is_play(stage.state) || other_attempt) {
                    preparing = None;
                }
                last = Some(stage);
            }
            Raw::ResultTaken(at) => {
                if let Some(from) = preparing.take() {
                    marks.push((
                        i,
                        Mark {
                            kind: MarkKind::Preparing,
                            from,
                            to: at,
                        },
                    ));
                }
            }
        }
    }
    marks
}

/// The record derived from the raw records: spans, changes and marks in
/// the order they happened, every gap covered by every mark of the run.
fn derive(raw: &[Raw]) -> Vec<Record> {
    let marks = marks_of(raw);
    let all: Vec<Mark> = marks.iter().map(|&(_, m)| m).collect();
    let mut placed = marks.iter().peekable();
    let mut out = Vec::new();
    let mut last: Option<(Stage, TimeUs)> = None;
    let mut open: Option<Span> = None;
    for (i, r) in raw.iter().enumerate() {
        if let Raw::Present {
            stage,
            at,
            unpresented,
        } = *r
        {
            match (last, open) {
                (Some((l, l_at)), Some(mut s)) => {
                    let gap = Gap {
                        from: l_at,
                        to: at,
                        cover: coverage(l_at, at, &all),
                        unpresented,
                    };
                    match change(l, stage) {
                        Some(false) => {
                            out.push(Record::Span(s));
                            out.push(Record::Edge(Edge {
                                presentation: false,
                                from: l,
                                to: stage,
                                gap,
                            }));
                            open = Some(opened(stage, at));
                        }
                        presentation => {
                            if presentation.is_some() {
                                out.push(Record::Edge(Edge {
                                    presentation: true,
                                    from: l,
                                    to: stage,
                                    gap,
                                }));
                            }
                            s.frames = s.frames.saturating_add(1);
                            s.unpresented = s.unpresented.saturating_add(unpresented);
                            s.last = at;
                            if s.longest.is_none_or(|g| gap.len() > g.len()) {
                                s.longest = Some(gap);
                            }
                            if gap.cover.uncovered > 0
                                && s.uncovered
                                    .is_none_or(|u| gap.cover.uncovered > u.cover.uncovered)
                            {
                                s.uncovered = Some(gap);
                            }
                            open = Some(s);
                        }
                    }
                }
                _ => open = Some(opened(stage, at)),
            }
            last = Some((stage, at));
        }
        while let Some(&(_, m)) = placed.next_if(|&&(j, _)| j == i) {
            out.push(Record::Mark(m));
        }
    }
    // The run's end closes the open span.
    if let Some(s) = open {
        out.push(Record::Span(s));
    }
    out
}

/// A span opened by the frame of `stage` presented at `at`.
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

/// The record's lines, then its summary.
fn format_records(records: &[Record], instrumented: bool, lost: u64) -> Vec<String> {
    let tag = if instrumented { ", instrumented" } else { "" };
    let mut lines = Vec::with_capacity(records.len() + 1);
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
    for r in records {
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
        "dev: transitions: {} records, lost {lost}, largest uncovered {largest}{tag}",
        records.len(),
    ));
    lines
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
/// no mark covers.
#[must_use]
pub fn coverage(from: TimeUs, to: TimeUs, marks: &[Mark]) -> Coverage {
    let (g0, g1) = (from.0, to.0);
    let mut clipped: Vec<(i64, i64)> = marks
        .iter()
        .map(|m| (m.from.0.max(g0), m.to.0.min(g1)))
        .filter(|&(a, b)| a < b)
        .collect();
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
    for (a, b) in clipped {
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
/// appends raw records to the book; it allocates nothing.
#[derive(Debug)]
pub struct Recorder {
    book: Transitions,
    /// The last presented frame's stage and present.
    last: Option<(Stage, TimeUs)>,
    /// The frames not presented since the last present.
    unpresented: u32,
    /// The loading lifetime whose hold is marked.
    held: Option<u64>,
}

impl Recorder {
    /// A recorder writing into `book`.
    #[must_use]
    pub const fn new(book: Transitions) -> Self {
        Self {
            book,
            last: None,
            unpresented: 0,
            held: None,
        }
    }

    /// A frame that was not presented.
    pub const fn unpresented(&mut self) {
        self.unpresented = self.unpresented.saturating_add(1);
    }

    /// A presented frame of `stage` at `at`, with its loading lifetime's
    /// hold if the render thread held it. Returns the change it ends: a
    /// transition, or a presentation change.
    pub fn frame(&mut self, stage: Stage, at: TimeUs, hold: Option<HoldStamp>) -> Option<Change> {
        if let Some(h) = hold {
            self.hold(h);
        }
        let unpresented = std::mem::take(&mut self.unpresented);
        self.book.push(Raw::Present {
            stage,
            at,
            unpresented,
        });
        let ended = self.last.and_then(|(last, last_at)| {
            change(last, stage).map(|presentation| Change {
                presentation,
                from: last,
                to: stage,
                last: last_at,
                first: at,
            })
        });
        self.last = Some((stage, at));
        ended
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

    /// Records `m`.
    pub fn mark(&self, m: Mark) {
        self.book.push(Raw::Mark(m));
    }

    /// The play's in-play interval ended at `at`.
    pub fn play_ended(&self, at: TimeUs) {
        self.book.push(Raw::PlayEnded(at));
    }

    /// The render thread took a result's presentation or the built-in
    /// result's page at `at`: the result was prepared since the play ended.
    pub fn result_taken(&self, at: TimeUs) {
        self.book.push(Raw::ResultTaken(at));
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
        // The run's end closes the play's span too.
        let [s, _] = spans(&book)[..] else {
            panic!("two spans: {:?}", book.records());
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
        let c = coverage(TimeUs(20_100_000), TimeUs(20_500_000), &[hold]);
        assert_eq!((c.covered, c.uncovered, c.piece), (400_000, 0, None));
    }

    /// Every mark in a gap covers its part, however many: eight 1 µs
    /// read-backs and a ninth of 500 ms cover 500 008 µs of a 1 s gap.
    #[test]
    fn every_mark_in_a_gap_covers_its_part() {
        let book = Transitions::with_capacity(32);
        let mut r = Recorder::new(book.clone());
        let play = stage(State::Play, 7, Drew::Plain);
        r.frame(play, TimeUs(5_000_000), None);
        for k in 0..8 {
            r.mark(read_back(5_000_100 + k * 10, 5_000_101 + k * 10));
        }
        r.mark(read_back(5_200_000, 5_700_000));
        r.frame(play, TimeUs(6_000_000), None);
        let [s] = spans(&book)[..] else {
            panic!("one span: {:?}", book.records());
        };
        assert_eq!(s.longest.unwrap().cover.uncovered, 1_000_000 - 500_008);
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
        let r = Recorder::new(book.clone());
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
                "dev: transitions: 4 records, lost 0, largest uncovered 249800 us \
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
    /// presented, each hold ending and an unended hold, a preparation, and
    /// a summary with no uncovered time.
    #[test]
    fn every_line_form_is_pinned() {
        use super::{Coverage, Edge, Gap, format_records};
        let gap = |from: i64, to: i64, cover: Coverage, unpresented: u32| Gap {
            from: TimeUs(from),
            to: TimeUs(to),
            cover,
            unpresented,
        };
        let mut records = vec![
            Record::Edge(Edge {
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
            }),
            Record::Edge(Edge {
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
            }),
        ];
        for kind in [
            MarkKind::Hold(Some(HoldEnd::Bound)),
            MarkKind::Hold(Some(HoldEnd::Skin)),
            MarkKind::Hold(Some(HoldEnd::Image)),
            MarkKind::Hold(Some(HoldEnd::DeviceLost)),
            MarkKind::Hold(None),
            MarkKind::Preparing,
        ] {
            records.push(Record::Mark(Mark {
                kind,
                from: TimeUs(10),
                to: TimeUs(20),
            }));
        }
        assert_eq!(
            format_records(&records, false, 0),
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
                "dev: transitions: 8 records, lost 0, largest uncovered none",
            ]
        );
    }
}
