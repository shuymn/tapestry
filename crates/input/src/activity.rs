//! Tuning activity as the loading state reads it (design.md "Play
//! Lifecycle", the loading state before a play and its opening): one
//! record the window side and the input thread publish into, read as one
//! coherent snapshot.
//!
//! The record has two sections, each written by one writer at a time:
//! the window side's (the settings keys and focus, written under the
//! tuning step lock) and the input side's (the tuning controls the
//! adjustment tap sees, written by the input thread, which never waits).
//! Each section has a sequence counter that is odd while a write is in
//! progress ([`Section::begin`]). A read ([`ActivityRecord::read`]) takes
//! both sections between two reads of both counters and accepts it only
//! when neither was odd nor moved; after [`READ_TRIES`] attempts it gives
//! up, and the reader treats the record as changed and not quiet. Writing
//! and reading allocate nothing and never wait.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};

use bms_model::TimeUs;

/// The most attempts one read makes before it gives up: a reader under
/// the tuning step lock never loops on a busy writer.
pub const READ_TRIES: u32 = 4;

/// One section's content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionValue {
    /// Moves with every activity the section's writer counts.
    pub revision: u64,
    /// The latest activity's time stamp; `None` before any.
    pub last: Option<TimeUs>,
    /// A tuning control is held, or may be.
    pub held: bool,
    /// A press did not fit the held set (the window side).
    pub overflow: bool,
    /// Focus is away (the window side).
    pub unfocused: bool,
    /// Focus losses so far (the window side's focus-loss generation).
    pub focus_losses: u64,
}

const HELD: u64 = 1;
const OVERFLOW: u64 = 2;
const UNFOCUSED: u64 = 4;

/// One writer's section of the record.
#[derive(Debug)]
pub struct Section {
    /// Odd while a write is in progress.
    seq: AtomicU64,
    revision: AtomicU64,
    /// `i64::MIN` for none.
    last: AtomicI64,
    flags: AtomicU64,
    focus_losses: AtomicU64,
}

impl Default for Section {
    fn default() -> Self {
        Self {
            seq: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            last: AtomicI64::new(i64::MIN),
            flags: AtomicU64::new(0),
            focus_losses: AtomicU64::new(0),
        }
    }
}

/// A write of a [`Section`] in progress: its sequence is odd until it is
/// committed or dropped.
#[derive(Debug)]
pub struct Writing<'a>(&'a Section);

impl Writing<'_> {
    /// Stores `value` and ends the write.
    pub fn commit(self, value: SectionValue) {
        let s = self.0;
        s.revision.store(value.revision, Ordering::Relaxed);
        s.last
            .store(value.last.map_or(i64::MIN, |t| t.0), Ordering::Relaxed);
        let flags = (if value.held { HELD } else { 0 })
            | (if value.overflow { OVERFLOW } else { 0 })
            | (if value.unfocused { UNFOCUSED } else { 0 });
        s.flags.store(flags, Ordering::Relaxed);
        s.focus_losses.store(value.focus_losses, Ordering::Relaxed);
        // `drop` ends it.
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.seq.fetch_add(1, Ordering::Release);
    }
}

impl Section {
    /// Begins a write: until it is committed (or dropped), a read of the
    /// record retries and then gives up. Only the section's one writer
    /// calls it.
    pub fn begin(&self) -> Writing<'_> {
        self.seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        Writing(self)
    }

    /// Publishes `value`: [`begin`](Self::begin), then commit.
    pub fn publish(&self, value: SectionValue) {
        self.begin().commit(value);
    }

    /// The focus losses as last written, read alone (the input thread's
    /// opening check).
    pub fn focus_losses(&self) -> u64 {
        self.focus_losses.load(Ordering::Acquire)
    }

    fn fields(&self) -> SectionValue {
        let last = self.last.load(Ordering::Relaxed);
        let flags = self.flags.load(Ordering::Relaxed);
        SectionValue {
            revision: self.revision.load(Ordering::Relaxed),
            last: (last != i64::MIN).then_some(TimeUs(last)),
            held: flags & HELD != 0,
            overflow: flags & OVERFLOW != 0,
            unfocused: flags & UNFOCUSED != 0,
            focus_losses: self.focus_losses.load(Ordering::Relaxed),
        }
    }
}

/// Both sections, read together ([`ActivityRecord::read`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityRead {
    /// The window side's.
    pub window: SectionValue,
    /// The input side's.
    pub input: SectionValue,
}

/// The ingress word as one observation was stamped with, or as it is now
/// (design.md "Play Lifecycle", the decision screen): a generation and
/// whether tuning observations act. The decision screen's choice closes
/// it and every exit opens it, each a new generation, so an observation
/// acts only when the word it was stamped with is the current one and
/// open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ingress(u64);

impl Ingress {
    /// The word before any choice: open, generation 0.
    pub const INITIAL: Self = Self(1);

    /// Whether observations stamped with it act while it is current.
    #[must_use]
    pub const fn is_open(self) -> bool {
        self.0 & 1 != 0
    }

    /// The next generation, `open` or closed.
    #[must_use]
    pub const fn next(self, open: bool) -> Self {
        Self((self.0 & !1).wrapping_add(2) | open as u64)
    }

    /// Whether an observation stamped `stamp` acts under `self`, the word
    /// now: the same generation, open.
    #[must_use]
    pub fn admits(self, stamp: Self) -> bool {
        self == stamp && self.is_open()
    }
}

/// The record: the window side's section, the input side's, the
/// candidate generation whose recording the input thread opened last, and
/// the ingress word.
#[derive(Debug)]
pub struct ActivityRecord {
    /// Written under the tuning step lock.
    pub window: Section,
    /// Written by the input thread's adjustment tap.
    pub input: Section,
    opened: AtomicU32,
    /// Written only under the tuning step lock; read lock-free by the
    /// stamps.
    ingress: AtomicU64,
}

impl Default for ActivityRecord {
    fn default() -> Self {
        Self {
            window: Section::default(),
            input: Section::default(),
            opened: AtomicU32::new(0),
            ingress: AtomicU64::new(Ingress::INITIAL.0),
        }
    }
}

impl ActivityRecord {
    /// Both sections as one snapshot: `None` when a write was in progress
    /// or completed during each of [`READ_TRIES`] attempts.
    pub fn read(&self) -> Option<ActivityRead> {
        self.read_racing(|| {})
    }

    /// [`read`](Self::read), with `between` run in each attempt after the
    /// fields are read and before the sequences are read again: a test
    /// completes a write there.
    fn read_racing(&self, mut between: impl FnMut()) -> Option<ActivityRead> {
        for _ in 0..READ_TRIES {
            let w = self.window.seq.load(Ordering::Acquire);
            let i = self.input.seq.load(Ordering::Acquire);
            if w.is_multiple_of(2) && i.is_multiple_of(2) {
                let read = ActivityRead {
                    window: self.window.fields(),
                    input: self.input.fields(),
                };
                between();
                fence(Ordering::Acquire);
                if self.window.seq.load(Ordering::Relaxed) == w
                    && self.input.seq.load(Ordering::Relaxed) == i
                {
                    return Some(read);
                }
            }
            std::hint::spin_loop();
        }
        None
    }

    /// The candidate generation whose recording the input thread opened
    /// last; 0 before any.
    pub fn opened(&self) -> u32 {
        self.opened.load(Ordering::Acquire)
    }

    /// Candidate `generation`'s recording opened (the input thread).
    pub fn set_opened(&self, generation: u32) {
        self.opened.store(generation, Ordering::Release);
    }

    /// The ingress word now: what an observation is stamped with.
    pub fn ingress(&self) -> Ingress {
        Ingress(self.ingress.load(Ordering::Acquire))
    }

    /// The ingress word becomes `word`. The caller holds the tuning step
    /// lock, under which every observation is validated.
    pub fn set_ingress(&self, word: Ingress) {
        self.ingress.store(word.0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{ActivityRecord, READ_TRIES, SectionValue};

    /// R22: a write in progress on either section (its sequence odd) makes
    /// every attempt of a read fail: the read gives up rather than return
    /// a mix. Committed, the same read returns what was written.
    #[test]
    fn a_torn_read_is_rejected() {
        let record = ActivityRecord::default();
        let value = SectionValue {
            revision: 3,
            last: Some(TimeUs(7)),
            held: true,
            ..SectionValue::default()
        };
        let writing = record.input.begin();
        assert_eq!(record.read(), None, "the input side's write in progress");
        writing.commit(value);
        let read = record.read().unwrap();
        assert_eq!(read.input, value);
        let writing = record.window.begin();
        assert_eq!(record.read(), None, "the window side's write in progress");
        drop(writing);
        assert_eq!(record.read().unwrap().input, value, "dropped: unchanged");
        record.window.publish(SectionValue {
            unfocused: true,
            focus_losses: 1,
            ..SectionValue::default()
        });
        let read = record.read().unwrap();
        assert!(read.window.unfocused);
        assert_eq!(record.window.focus_losses(), 1);
    }

    /// R22: a write that begins and completes while a read takes the
    /// fields leaves both sequences even, and only their second reading
    /// shows it moved: a read with such a write in every attempt gives up
    /// on each section, and one with it in the first attempt only returns
    /// the written value whole from the next.
    #[test]
    fn a_write_completed_during_a_read_is_rejected() {
        for section in ["input", "window"] {
            let record = ActivityRecord::default();
            let written = |n: u64| SectionValue {
                revision: n,
                last: Some(TimeUs(i64::try_from(n).unwrap())),
                held: true,
                ..SectionValue::default()
            };
            let publish = |n| match section {
                "input" => record.input.publish(written(n)),
                _ => record.window.publish(written(n)),
            };
            let mut n = 0;
            let read = record.read_racing(|| {
                n += 1;
                publish(n);
            });
            assert_eq!(read, None, "{section}: written in every attempt");
            assert_eq!(n, u64::from(READ_TRIES), "{section}");
            let mut once = false;
            let read = record
                .read_racing(|| {
                    if !once {
                        once = true;
                        publish(100);
                    }
                })
                .unwrap();
            let got = match section {
                "input" => read.input,
                _ => read.window,
            };
            assert_eq!(got, written(100), "{section}: read whole after it");
        }
    }
}
