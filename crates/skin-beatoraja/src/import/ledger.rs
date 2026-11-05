//! The conversion report's destination ledger (dev-tools; docs/stage3.md §3,
//! the conversion report): what an import made of each destination, in
//! the destination list's order, however far the import got. It names what
//! the skin wrote, so only a survey asked for developer-local detail arms
//! it; an import it does not arm records nothing.

use std::cell::{Cell, RefCell};
use std::ops::Range;

use super::Cx;
use super::report::{self, NoteKind, Severity};

thread_local! {
    /// A survey asked for the ledger of this thread's next import.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// The ledger of this thread's last armed import.
    static LAST: RefCell<Vec<DestinationEntry>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f`, an import, and returns the ledger it left: empty if it stopped
/// before the destinations.
pub(crate) fn recording<T>(f: impl FnOnce() -> T) -> (T, Vec<DestinationEntry>) {
    ARMED.with(|a| a.set(true));
    LAST.with(|l| l.borrow_mut().clear());
    let out = f();
    ARMED.with(|a| a.set(false));
    (out, LAST.with(RefCell::take))
}

/// What the import made of a destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Became {
    /// It has elements.
    Retained,
    /// A condition folded false under the configuration: never drawn.
    Folded,
    /// Dropped with a note: `Dropped` severity.
    Dropped,
    /// It names nothing the source makes an object of: `Ignored` severity.
    Ignored,
    /// No element and no note saying why.
    NoElement,
    /// The import refused here.
    Refused,
    /// The import ended before it.
    NotReached,
}

/// A note of a destination, located.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationNote {
    /// The definition file, and the line and column there; a converted
    /// Lua value has line 0 and its ordinal as the column.
    pub at: Option<(String, u32, u32)>,
    pub severity: Severity,
    pub kind: NoteKind,
}

/// One destination of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationEntry {
    /// Its index in the destination list.
    pub index: usize,
    /// Its `id`, when it has one and the import read it.
    pub id: Option<String>,
    /// The kind of object its `id` names, when it was looked up.
    pub kind: Option<String>,
    pub became: Became,
    /// Its elements in the skin's draw order.
    pub elements: Range<usize>,
    pub notes: Vec<DestinationNote>,
    /// Its notes past the report's cap, counted only.
    pub suppressed: u32,
}

/// One destination read, before its notes are located.
struct Read {
    id: Option<String>,
    kind: Option<String>,
    refused: bool,
    folded: bool,
    elements: Range<usize>,
    notes: Range<usize>,
    suppressed: u32,
}

/// Follows an import's walk of the destinations. Inert unless armed.
pub(super) struct Recorder {
    armed: bool,
    total: usize,
    read: Vec<Read>,
    elements_at: usize,
    notes_at: usize,
    suppressed_at: u32,
}

impl Recorder {
    pub(super) fn new(total: usize) -> Self {
        Self {
            armed: ARMED.with(Cell::get),
            total,
            read: Vec::new(),
            elements_at: 0,
            notes_at: 0,
            suppressed_at: 0,
        }
    }

    /// Before `cx` reads the next destination.
    pub(super) fn begin(&mut self, cx: &mut Cx<'_>) {
        if self.armed {
            self.elements_at = cx.elements.len();
            self.notes_at = cx.notes.list.len();
            self.suppressed_at = cx.notes.suppressed;
            cx.folded = false;
        }
    }

    /// After `cx` read destination `index`, `refused` if it refused it:
    /// then this is the import's end, as the destinations' end is.
    pub(super) fn end(&mut self, cx: &Cx<'_>, index: usize, refused: bool) {
        if !self.armed {
            return;
        }
        // The current element is set only once the destination reads.
        let e = cx.element.as_ref().filter(|e| e.index == index);
        self.read.push(Read {
            id: e.and_then(|e| e.id.clone()),
            kind: e.and_then(|e| e.kind.clone()),
            refused,
            folded: cx.folded,
            elements: self.elements_at..cx.elements.len(),
            notes: self.notes_at..cx.notes.list.len(),
            suppressed: cx.notes.suppressed.saturating_sub(self.suppressed_at),
        });
        if refused {
            self.publish(cx);
        }
    }

    /// Locates the notes, once, and leaves the ledger for [`recording`].
    pub(super) fn publish(&mut self, cx: &Cx<'_>) {
        if !self.armed {
            return;
        }
        let located = report::locate(&cx.def.arena, &cx.notes.list);
        let sources = cx.def.arena.sources();
        let mut entries: Vec<DestinationEntry> = self
            .read
            .drain(..)
            .zip(0..)
            .map(|(r, index)| {
                let notes: Vec<DestinationNote> = located
                    .get(r.notes)
                    .unwrap_or_default()
                    .iter()
                    .map(|n| DestinationNote {
                        at: n.at.map(|at| {
                            let file = sources
                                .get(usize::from(at.source))
                                .cloned()
                                .unwrap_or_default();
                            (file, at.line, at.col)
                        }),
                        severity: n.severity,
                        kind: n.kind,
                    })
                    .collect();
                let has = |s| notes.iter().any(|n| n.severity == s);
                let became = if r.refused {
                    Became::Refused
                } else if !r.elements.is_empty() {
                    Became::Retained
                } else if r.folded {
                    Became::Folded
                } else if has(Severity::Dropped) {
                    Became::Dropped
                } else if has(Severity::Ignored) {
                    Became::Ignored
                } else {
                    Became::NoElement
                };
                DestinationEntry {
                    index,
                    id: r.id,
                    kind: r.kind,
                    became,
                    elements: r.elements,
                    notes,
                    suppressed: r.suppressed,
                }
            })
            .collect();
        let end = cx.elements.len();
        for index in entries.len()..self.total {
            entries.push(DestinationEntry {
                index,
                id: None,
                kind: None,
                became: Became::NotReached,
                elements: end..end,
                notes: Vec::new(),
                suppressed: 0,
            });
        }
        LAST.with(|l| *l.borrow_mut() = entries);
        self.armed = false;
    }
}
