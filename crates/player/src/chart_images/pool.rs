//! The selection's chart-image entries and the worker's map of the live
//! pool's slots (design.md "Rendering and Skins", chart images), on the
//! game thread.
//!
//! - **Entries** ([`Entries`]): every cursor row read, resident in a slot
//!   or absent, with the version of its read, the bindings it was read
//!   from and the uses its checks validated. Resident and absent entries
//!   share one least-recently-used bound of [`MAX_ENTRIES`] entries and
//!   [`MAX_ENTRY_BYTES`] of accounted heap bytes, each entry charged its
//!   own when inserted.
//! - **Slots** ([`Slots`]): one live pool generation's slots, each free,
//!   being written or resident, with its recency. A slot is chosen for a
//!   write only when no pin holds it: a free one first, else the least
//!   recently used resident one, whose entry the caller drops.

use skin_model::chart_image::Binding;

use super::CursorId;

/// The most entries kept.
pub(super) const MAX_ENTRIES: usize = 256;

/// The most accounted heap bytes the entries keep.
pub(super) const MAX_ENTRY_BYTES: usize = 4 << 20;

/// Where an entry's images are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryState {
    /// In slot `slot` of pool generation `generation`.
    Resident { generation: u64, slot: u16 },
    /// Neither image is drawable: the entry takes no slot.
    Absent,
}

/// One row read.
#[derive(Debug)]
pub(super) struct Entry {
    /// Its read's number, fresh at each read.
    pub version: u64,
    /// The stage file's and the banner's bindings as read, which its
    /// checks compare ([`Binding::same_as`]).
    pub bindings: [Binding; 2],
    pub state: EntryState,
    /// The last use stamped on it, and the last use a check validated.
    pub last_use: u64,
    pub validated: u64,
    /// Its accounted heap bytes.
    bytes: usize,
    /// When it was last used or inserted, for the least-recently-used
    /// bound.
    recency: u64,
}

impl Entry {
    /// A row's entry of read `version` from `bindings`, in `state`.
    pub(super) const fn new(version: u64, bindings: [Binding; 2], state: EntryState) -> Self {
        Self {
            version,
            bindings,
            state,
            last_use: 0,
            validated: 0,
            bytes: 0,
            recency: 0,
        }
    }
}

/// The heap bytes an entry for `id` read from `bindings` keeps: its
/// folder, its names, each binding's name and resolved path, and its
/// place in the list.
fn accounted(id: &CursorId, bindings: &[Binding; 2]) -> usize {
    let name = |n: &library::Name| match n {
        library::Name::Named(s) => s.capacity(),
        library::Name::None | library::Name::Overlong => 0,
    };
    let binding = |b: &Binding| match b {
        Binding::NoHeader => 0,
        Binding::Unresolved { name, .. } => name.capacity(),
        Binding::Resolved { name, path, .. } => name.capacity() + path.capacity(),
    };
    std::mem::size_of::<(CursorId, Entry)>()
        + id.dir.capacity()
        + id.names.iter().map(name).sum::<usize>()
        + bindings.iter().map(binding).sum::<usize>()
}

/// The entries, least recently used first to go.
#[derive(Debug, Default)]
pub(super) struct Entries {
    list: Vec<(CursorId, Entry)>,
    bytes: usize,
    clock: u64,
}

impl Entries {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    pub(super) fn get(&self, id: &CursorId) -> Option<&Entry> {
        self.list.iter().find(|(k, _)| k == id).map(|(_, e)| e)
    }

    pub(super) fn get_mut(&mut self, id: &CursorId) -> Option<&mut Entry> {
        self.list.iter_mut().find(|(k, _)| k == id).map(|(_, e)| e)
    }

    /// `id`'s entry was used: it goes last.
    pub(super) fn touch(&mut self, id: &CursorId) {
        let now = self.tick();
        if let Some(e) = self.get_mut(id) {
            e.recency = now;
        }
    }

    /// Inserts `entry` for `id`, replacing its own, the least recently
    /// used going first while either bound would be passed; one larger
    /// than the whole byte bound is not kept. Returns whether it was kept
    /// and the entries that went, its own replaced one included, whose
    /// slots the caller frees.
    pub(super) fn insert(&mut self, id: CursorId, mut entry: Entry) -> (bool, Vec<Entry>) {
        let mut gone: Vec<Entry> = self.remove(&id).into_iter().collect();
        entry.bytes = accounted(&id, &entry.bindings);
        if entry.bytes > MAX_ENTRY_BYTES {
            return (false, gone);
        }
        while self.list.len() >= MAX_ENTRIES || self.bytes + entry.bytes > MAX_ENTRY_BYTES {
            let Some(oldest) = self
                .list
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, e))| e.recency)
                .map(|(i, _)| i)
            else {
                break;
            };
            let (_, e) = self.list.swap_remove(oldest);
            self.bytes -= e.bytes;
            gone.push(e);
        }
        entry.recency = self.tick();
        self.bytes += entry.bytes;
        self.list.push((id, entry));
        (true, gone)
    }

    /// Removes `id`'s entry.
    pub(super) fn remove(&mut self, id: &CursorId) -> Option<Entry> {
        let i = self.list.iter().position(|(k, _)| k == id)?;
        let (_, e) = self.list.swap_remove(i);
        self.bytes -= e.bytes;
        Some(e)
    }

    /// Drops every resident entry: their pool generation ended. Absent
    /// ones need no slot and stay.
    pub(super) fn drop_resident(&mut self) {
        self.list
            .retain(|(_, e)| matches!(e.state, EntryState::Absent));
        self.bytes = self.list.iter().map(|(_, e)| e.bytes).sum();
    }

    /// The row whose entry is of read `version`.
    pub(super) fn by_version(&self, version: u64) -> Option<&CursorId> {
        self.list
            .iter()
            .find(|(_, e)| e.version == version)
            .map(|(k, _)| k)
    }

    pub(super) fn len(&self) -> usize {
        self.list.len()
    }

    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn clear(&mut self) {
        self.list.clear();
        self.bytes = 0;
    }
}

/// What a slot of the live pool holds, by the worker's bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Nothing: a write may take it.
    Free,
    /// A read of version `version` was admitted to it, its write not yet
    /// answered.
    Writing { version: u64 },
    /// The entry of read `version` is in it.
    Resident { version: u64 },
}

/// The live pool generation's slots.
#[derive(Debug)]
pub(super) struct Slots {
    layout: render::PoolLayout,
    slots: Vec<(SlotState, u64)>,
    clock: u64,
}

impl Slots {
    /// The slots of `layout`, every one free.
    pub(super) fn new(layout: render::PoolLayout) -> Self {
        Self {
            layout,
            slots: vec![(SlotState::Free, 0); usize::from(layout.slots)],
            clock: 0,
        }
    }

    pub(super) const fn layout(&self) -> render::PoolLayout {
        self.layout
    }

    /// A slot for a write, none of `pins`: a free one first, else the
    /// least recently used resident one, with the version of the entry it
    /// held, which the caller drops. `None` when every slot is pinned or
    /// being written.
    pub(super) fn admit(&self, pins: [Option<u16>; 2]) -> Option<(u16, Option<u64>)> {
        let open = (0_u16..).zip(&self.slots).filter(|(i, (s, _))| {
            !pins.contains(&Some(*i)) && !matches!(s, SlotState::Writing { .. })
        });
        let mut best: Option<(u16, Option<u64>, u64)> = None;
        for (i, &(state, recency)) in open {
            match state {
                SlotState::Free => return Some((i, None)),
                SlotState::Resident { version } => {
                    if best.is_none_or(|(_, _, r)| recency < r) {
                        best = Some((i, Some(version), recency));
                    }
                }
                SlotState::Writing { .. } => {}
            }
        }
        best.map(|(i, v, _)| (i, v))
    }

    /// Sets slot `slot`'s state; a write or a residency makes it the most
    /// recently used.
    pub(super) fn set(&mut self, slot: u16, state: SlotState) {
        self.clock += 1;
        let now = self.clock;
        if let Some(s) = self.slots.get_mut(usize::from(slot)) {
            *s = (state, if state == SlotState::Free { s.1 } else { now });
        }
    }

    /// Slot `slot` was carried by an update sent: the most recently used.
    pub(super) fn touch(&mut self, slot: u16) {
        self.clock += 1;
        let now = self.clock;
        if let Some(s) = self.slots.get_mut(usize::from(slot)) {
            s.1 = now;
        }
    }

    /// Frees slot `slot` if it holds `version` (being written or
    /// resident): no later write's slot is freed by an earlier read.
    pub(super) fn free(&mut self, slot: u16, version: u64) {
        if let Some(s) = self.slots.get_mut(usize::from(slot))
            && matches!(s.0, SlotState::Writing { version: v } | SlotState::Resident { version: v } if v == version)
        {
            s.0 = SlotState::Free;
        }
    }

    /// Every slot's state (tests).
    #[cfg(test)]
    pub(super) fn states(&self) -> Vec<SlotState> {
        self.slots.iter().map(|(s, _)| *s).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use skin_model::chart_image::{Binding, ChartCells};

    use super::{
        CursorId, Entries, Entry, EntryState, MAX_ENTRIES, MAX_ENTRY_BYTES, SlotState, Slots,
    };

    fn row(dir: impl Into<PathBuf>) -> CursorId {
        CursorId {
            dir: dir.into(),
            names: [
                library::Name::Named("s.png".to_owned()),
                library::Name::None,
            ],
        }
    }

    fn entry(version: u64, state: EntryState) -> Entry {
        Entry::new(version, [Binding::NoHeader, Binding::NoHeader], state)
    }

    const RESIDENT: EntryState = EntryState::Resident {
        generation: 1,
        slot: 0,
    };

    /// 257 entries keep the 256 most recent, absences included, across two
    /// folders: the least recently used goes first, and a use keeps one
    /// (AC-S16).
    #[test]
    fn the_257th_entry_evicts_the_least_recent() {
        let mut e = Entries::default();
        for i in 0..MAX_ENTRIES {
            let folder = if i % 2 == 0 { "one" } else { "two" };
            let state = if i % 3 == 0 {
                EntryState::Absent
            } else {
                RESIDENT
            };
            let (kept, gone) = e.insert(
                row(format!("{folder}/{i}")),
                entry(u64::try_from(i).unwrap(), state),
            );
            assert!(kept && gone.is_empty(), "{i}");
        }
        assert_eq!(e.len(), MAX_ENTRIES);
        // The first, an absence, used again: the second goes instead.
        e.touch(&row("one/0"));
        let (kept, gone) = e.insert(row("two/new"), entry(1_000, EntryState::Absent));
        assert!(kept);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].version, 1);
        assert_eq!(e.len(), MAX_ENTRIES);
        assert!(e.get(&row("one/0")).is_some());
        assert!(e.get(&row("two/1")).is_none());
        // The next goes the third, an absence (0 used, 1 gone).
        let (_, gone) = e.insert(row("one/new"), entry(1_001, RESIDENT));
        assert_eq!(gone[0].version, 2);
        let (_, gone) = e.insert(row("two/newer"), entry(1_002, RESIDENT));
        assert_eq!(gone[0].version, 3);
        assert_eq!(gone[0].state, EntryState::Absent);
    }

    /// Entries with long folder paths pass 4 MiB of accounted bytes before
    /// 256 entries: the least recent go until the new one fits, absences
    /// included; one larger than the whole bound is not kept (AC-S16).
    #[test]
    fn long_folder_paths_are_bounded_by_their_bytes() {
        let mut e = Entries::default();
        // Each path's capacity its length: an entry accounts a little
        // over 64 KiB.
        let long = |i: usize| {
            let mut dir = format!("{}/{i}", "x".repeat(64 << 10));
            dir.shrink_to_fit();
            row(dir)
        };
        let mut n = 0;
        loop {
            let state = if n % 2 == 0 {
                EntryState::Absent
            } else {
                RESIDENT
            };
            let (kept, gone) = e.insert(long(n), entry(u64::try_from(n).unwrap(), state));
            assert!(kept);
            if !gone.is_empty() {
                assert_eq!(gone.len(), 1);
                assert_eq!(gone[0].version, 0, "the least recent goes");
                break;
            }
            n += 1;
            assert!(n < MAX_ENTRIES);
        }
        assert!(n > 1 && n < 70, "{n}");
        assert!(e.bytes() <= MAX_ENTRY_BYTES);
        assert!(e.bytes() > MAX_ENTRY_BYTES - (66 << 10));
        assert!(e.get(&long(0)).is_none());
        let huge = row("y".repeat(MAX_ENTRY_BYTES + 1));
        let (kept, gone) = e.insert(huge.clone(), entry(9_999, EntryState::Absent));
        assert!(!kept && gone.is_empty());
        assert!(e.get(&huge).is_none());
    }

    /// A generation's end drops the resident entries and keeps the absent
    /// ones.
    #[test]
    fn a_generations_end_keeps_only_absences() {
        let mut e = Entries::default();
        e.insert(row("a"), entry(1, RESIDENT));
        e.insert(row("b"), entry(2, EntryState::Absent));
        e.drop_resident();
        assert!(e.get(&row("a")).is_none());
        assert!(e.get(&row("b")).is_some());
        assert_eq!(e.len(), 1);
    }

    fn layout(slots: u16) -> render::PoolLayout {
        render::PoolLayout {
            generation: 1,
            slots,
            cells: ChartCells::NONE,
            side: 2_048,
            layers: 1,
        }
    }

    /// A write takes a free slot first, else the least recently used
    /// resident one; never a pinned one or one being written.
    #[test]
    fn a_slot_is_free_first_then_least_recent_and_never_pinned() {
        let mut s = Slots::new(layout(3));
        assert_eq!(s.admit([None, None]), Some((0, None)));
        s.set(0, SlotState::Resident { version: 10 });
        assert_eq!(s.admit([Some(1), None]), Some((2, None)));
        s.set(1, SlotState::Resident { version: 11 });
        s.set(2, SlotState::Writing { version: 12 });
        // 0 is the least recent.
        assert_eq!(s.admit([None, None]), Some((0, Some(10))));
        s.touch(0);
        assert_eq!(s.admit([None, None]), Some((1, Some(11))));
        assert_eq!(s.admit([Some(1), Some(0)]), None);
        // An earlier read frees nothing of a later one.
        s.free(2, 99);
        assert_eq!(s.states()[2], SlotState::Writing { version: 12 });
        s.free(2, 12);
        assert_eq!(s.admit([None, None]), Some((2, None)));
    }
}
