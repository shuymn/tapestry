//! The text layer's cache (design.md "Rendering and Skins"): which lines
//! are placed where in a skin's text layer, which preparations hold them,
//! and the placement of one preparation's lines in rounds. One cache
//! belongs to each set of resources (an atlas array with its text layer)
//! and starts empty with it.
//!
//! **Entries.** A line is keyed by its font, its string and its em, the
//! request's capped by the round ([`LineKey`]). Its rectangle is the one
//! `etagere` returned, never the size asked for: it is at most the side
//! wide and 1 096 tall, and every texel of it is written when the line is
//! uploaded, so a reused rectangle shows nothing stale. An entry is
//! unpublished from its allocation until its upload has finished on the
//! GPU; only then does it draw.
//!
//! **Aliases.** A request (font, string, requested em) remembers the key
//! it was last placed under, so a request refitted to a smaller em finds
//! its line again: several aliases may name one entry, and an alias dies
//! with its entry. At most [`MAX_ALIASES`]; past them the least recently
//! used one that no holder and no current request uses is pruned.
//!
//! **Pins.** A [`Holder`] (an installed or a staged attempt) holds the
//! entries its table draws and the aliases its requests used. Nothing
//! writes to or frees an entry a holder or the current round holds.
//!
//! **Rounds.** Each request takes a hit or a new entry; a failed
//! allocation evicts unpinned published entries, least recently used
//! first, and retries; a second failure releases the round's new entries,
//! and the next round asks for every request at half the em cap. After
//! [`MAX_ROUNDS`], requests that hit published entries keep them and the
//! others get no line ([`TextNote::LayerFull`]).
//!
//! Preparation work: it allocates.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use etagere::{AllocId, AllocatorOptions, AtlasAllocator, size2};
use skin_model::{FontRef, FrameUv, GUTTER, MAX_TEXT_EM_PX, TextImage, TextRequest};

use crate::outline::{LineBounds, LineFit, LineLayout, TextNote};

/// The most entries one cache holds.
pub const MAX_TEXT_ENTRIES: usize = 320;
/// The most aliases one cache holds.
pub const MAX_ALIASES: usize = 320;
/// The most rounds one preparation places its lines in.
pub const MAX_ROUNDS: u32 = 8;
/// Every rectangle's origin and size are multiples of this: the mips'
/// alignment.
const ALIGN: u32 = skin_model::ALIGN;

/// A line's key: its font, its string and the em it was laid out at, as
/// `f32` bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineKey {
    font: FontRef,
    text: String,
    em: u32,
}

impl LineKey {
    fn of(req: &TextRequest, em: f32) -> Self {
        Self {
            font: req.font,
            text: req.text.clone(),
            em: em.to_bits(),
        }
    }
}

/// One line in the layer.
#[derive(Debug)]
struct Entry {
    key: LineKey,
    id: AllocId,
    /// The returned rectangle: left, top, width, height.
    rect: [u32; 4],
    /// The line's pixels, `GUTTER` texels inside the rectangle.
    size: [u32; 2],
    ink: [f32; 4],
    advance: f32,
    published: bool,
    used: u64,
}

/// A request's key and the line key it was placed under.
#[derive(Debug)]
struct Alias {
    key: LineKey,
    line: LineKey,
    used: u64,
}

/// What a preparation holds in a cache: the entries its table draws and
/// the aliases its requests used, at most one per request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Holder {
    entries: Vec<LineKey>,
    aliases: Vec<LineKey>,
}

impl Holder {
    /// The bytes it holds on the heap: its lists and their strings.
    pub(crate) fn bytes(&self) -> usize {
        let strings: usize = self
            .entries
            .iter()
            .chain(&self.aliases)
            .map(|k| k.text.capacity())
            .sum();
        (self.entries.capacity() + self.aliases.capacity()) * size_of::<LineKey>() + strings
    }

    fn holds(&self, key: &LineKey) -> bool {
        self.entries.contains(key)
    }

    fn uses(&self, alias: &LineKey) -> bool {
        self.aliases.contains(alias)
    }
}

/// A preparation's counts, for its report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextCounts {
    /// The rounds placement took.
    pub rounds: u32,
    /// The published entries evicted to make room.
    pub evictions: u32,
    /// The requests that found their line.
    pub hits: u32,
    /// The new entries placed.
    pub placements: u32,
}

/// One request's outcome.
#[derive(Debug)]
enum Slot {
    /// An entry, by key.
    Line(LineKey),
    /// No line, with a note or none (nothing to draw).
    None(Option<TextNote>),
}

/// A preparation's placement: per request its outcome, the new entries to
/// upload with their layouts, and the counts.
#[derive(Debug)]
pub(crate) struct Placement {
    slots: Vec<(u16, Slot)>,
    /// The new entries' rectangles, the place of their line's pixels and
    /// their layouts.
    pub(crate) new: Vec<NewLine>,
    pub(crate) counts: TextCounts,
    /// The aliases the requests used: (alias, line).
    aliases: Vec<(LineKey, LineKey)>,
}

/// A new entry to upload: its returned rectangle and its layout.
#[derive(Debug)]
pub(crate) struct NewLine {
    key: LineKey,
    /// Left, top, width, height in texels.
    pub(crate) rect: [u32; 4],
    pub(crate) layout: LineLayout,
}

/// The text layer's cache.
pub(crate) struct TextCache {
    alloc: Option<AtlasAllocator>,
    side: u32,
    layer: u16,
    entries: Vec<Entry>,
    aliases: Vec<Alias>,
    clock: u64,
}

impl std::fmt::Debug for TextCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextCache")
            .field("side", &self.side)
            .field("layer", &self.layer)
            .field("entries", &self.entries)
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

impl TextCache {
    /// A cache with no layer: nothing places.
    pub(crate) fn none() -> Self {
        Self {
            alloc: None,
            side: 0,
            layer: 0,
            entries: Vec::new(),
            aliases: Vec::new(),
            clock: 0,
        }
    }

    /// An empty cache of layer `layer`, `side` texels square.
    pub(crate) fn new(side: u32, layer: u16) -> Self {
        let s = i32::try_from(side).unwrap_or(0);
        let a = i32::try_from(ALIGN).unwrap_or(1);
        let alloc = AtlasAllocator::with_options(
            size2(s, s),
            &AllocatorOptions {
                alignment: size2(a, a),
                vertical_shelves: false,
                num_columns: 1,
            },
        );
        Self {
            alloc: Some(alloc),
            side,
            layer,
            ..Self::none()
        }
    }

    /// Closes the cache after a device failure: nothing places, hits or
    /// is published again, and the renderer is built again with a fresh
    /// one.
    pub(crate) fn abandon(&mut self) {
        let side = self.side;
        *self = Self::none();
        self.side = side;
    }

    /// Whether it places nothing: no layer, or abandoned.
    #[cfg(test)]
    pub(crate) fn closed(&self) -> bool {
        self.alloc.is_none()
    }

    /// Its new entries, not yet published.
    #[cfg(test)]
    pub(crate) fn unpublished(&self) -> usize {
        self.entries.iter().filter(|e| !e.published).count()
    }

    /// The layer's side; 0 without a layer.
    pub(crate) const fn side(&self) -> u32 {
        self.side
    }

    /// The layer's free texels, by its allocator.
    #[cfg(test)]
    pub(crate) fn free(&self) -> i64 {
        self.alloc.as_ref().map_or(0, |a| i64::from(a.free_space()))
    }

    /// Its entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Each entry's string, returned rectangle and line size.
    #[cfg(test)]
    pub(crate) fn rects(&self) -> Vec<(String, [u32; 4], [u32; 2])> {
        self.entries
            .iter()
            .map(|e| (e.key.text.clone(), e.rect, e.size))
            .collect()
    }

    /// Its aliases.
    #[cfg(test)]
    pub(crate) fn aliases(&self) -> usize {
        self.aliases.len()
    }

    /// The bytes it holds: its lists' retained capacities and their
    /// strings.
    pub(crate) fn bytes(&self) -> u64 {
        let lists = self.entries.capacity() * size_of::<Entry>()
            + self.aliases.capacity() * size_of::<Alias>();
        let strings: usize = self
            .entries
            .iter()
            .map(|e| e.key.text.capacity())
            .chain(
                self.aliases
                    .iter()
                    .map(|a| a.key.text.capacity() + a.line.text.capacity()),
            )
            .sum();
        u64::try_from(lists + strings).unwrap_or(u64::MAX)
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn entry(&self, key: &LineKey) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.key == key)
    }

    /// Places `requests` (at most 64, in slot order) within `bounds`,
    /// laying out what it does not find with `layout`, never writing to
    /// or freeing an entry a holder of `held` holds. The new entries are
    /// allocated and unpublished: [`publish`](Self::publish) or
    /// [`rollback`](Self::rollback) them after their upload.
    pub(crate) fn place(
        &mut self,
        requests: &[TextRequest],
        bounds: LineBounds,
        held: &[&Holder],
        mut layout: impl FnMut(&TextRequest, f32, LineBounds) -> LineFit,
    ) -> Placement {
        let mut counts = TextCounts::default();
        if self.alloc.is_none() {
            let slots = requests
                .iter()
                .map(|r| (r.slot, Slot::None(Some(TextNote::LayerFull))));
            return Placement {
                slots: slots.collect(),
                new: Vec::new(),
                counts,
                aliases: Vec::new(),
            };
        }
        for round in 0..MAX_ROUNDS {
            counts.rounds = round + 1;
            let cap = MAX_TEXT_EM_PX / f32::from(1_u16 << round);
            if let Some(p) = self.round(
                requests,
                bounds,
                held,
                cap,
                round == 0,
                &mut layout,
                &mut counts,
            ) {
                return p;
            }
        }
        // Every round failed: published hits stay, the rest get nothing.
        let mut slots = Vec::with_capacity(requests.len());
        let mut aliases = Vec::new();
        for r in requests {
            let alias = LineKey::of(r, r.em_px);
            let line = self
                .aliases
                .iter()
                .find(|a| a.key == alias)
                .map(|a| a.line.clone())
                .unwrap_or_else(|| alias.clone());
            let hit = self.entry(&line).is_some_and(|e| e.published);
            if hit {
                counts.hits += 1;
                aliases.push((alias, line.clone()));
                slots.push((r.slot, Slot::Line(line)));
            } else {
                slots.push((r.slot, Slot::None(Some(TextNote::LayerFull))));
            }
        }
        Placement {
            slots,
            new: Vec::new(),
            counts,
            aliases,
        }
    }

    /// One round at em cap `cap`; `None`, with its new entries released,
    /// when the layer has no room.
    #[expect(
        clippy::too_many_arguments,
        reason = "one round's inputs, private to `place`"
    )]
    fn round(
        &mut self,
        requests: &[TextRequest],
        bounds: LineBounds,
        held: &[&Holder],
        cap: f32,
        first: bool,
        layout: &mut impl FnMut(&TextRequest, f32, LineBounds) -> LineFit,
        counts: &mut TextCounts,
    ) -> Option<Placement> {
        let mut slots = Vec::with_capacity(requests.len());
        let mut new: Vec<NewLine> = Vec::new();
        let mut aliases = Vec::with_capacity(requests.len());
        // This round's pins: its hits and its new entries.
        let mut pins: Vec<LineKey> = Vec::new();
        let hits_before = counts.hits;
        for r in requests {
            let alias = LineKey::of(r, r.em_px);
            let em = r.em_px.min(cap);
            let mut key = LineKey::of(r, em);
            if first
                && let Some(a) = self.aliases.iter().find(|a| a.key == alias)
                && self.entry(&a.line).is_some_and(|e| e.published)
            {
                key = a.line.clone();
            }
            let found = self.entry(&key).is_some();
            if found {
                counts.hits += 1;
                if !pins.contains(&key) {
                    pins.push(key.clone());
                }
                aliases.push((alias, key.clone()));
                slots.push((r.slot, Slot::Line(key)));
                continue;
            }
            let line = match layout(r, em, bounds) {
                LineFit::Fits(l) => l,
                LineFit::Empty => {
                    slots.push((r.slot, Slot::None(None)));
                    continue;
                }
                LineFit::Refused(note) => {
                    slots.push((r.slot, Slot::None(Some(note))));
                    continue;
                }
            };
            let Some(rect) = self.allocate(&key, &line, held, &pins, counts) else {
                // No room: release this round's new entries; the next
                // round sets every request smaller.
                for n in new.drain(..) {
                    self.release(&n.key);
                }
                counts.hits = hits_before;
                return None;
            };
            pins.push(key.clone());
            aliases.push((alias, key.clone()));
            slots.push((r.slot, Slot::Line(key.clone())));
            new.push(NewLine {
                key,
                rect,
                layout: line,
            });
        }
        // Count the placements of the round that succeeded only.
        counts.placements = u32::try_from(new.len()).unwrap_or(u32::MAX);
        Some(Placement {
            slots,
            new,
            counts: *counts,
            aliases,
        })
    }

    /// A rectangle for a line of `size`, its gutter included, as a new
    /// unpublished entry; published entries nobody holds are evicted,
    /// least recently used first, until it fits. `None` when it does not.
    fn allocate(
        &mut self,
        key: &LineKey,
        line: &LineLayout,
        held: &[&Holder],
        pins: &[LineKey],
        counts: &mut TextCounts,
    ) -> Option<[u32; 4]> {
        let [w, h] = line.size;
        let pad = |v: u32| v.checked_add(2 * GUTTER)?.checked_next_multiple_of(ALIGN);
        let (pw, ph) = (pad(w)?, pad(h)?);
        let size = size2(i32::try_from(pw).ok()?, i32::try_from(ph).ok()?);
        loop {
            if self.entries.len() < MAX_TEXT_ENTRIES
                && let Some(a) = self.alloc.as_mut()?.allocate(size)
            {
                let r = a.rectangle;
                let rect = [r.min.x, r.min.y, r.width(), r.height()].map(|v| u32::try_from(v).ok());
                let [Some(x), Some(y), Some(rw), Some(rh)] = rect else {
                    self.alloc.as_mut()?.deallocate(a.id);
                    return None;
                };
                let aligned = [x, y, rw, rh].iter().all(|v| v % ALIGN == 0);
                if !aligned || rw < pw || rh < ph || x + rw > self.side || y + rh > self.side {
                    self.alloc.as_mut()?.deallocate(a.id);
                    return None;
                }
                let used = self.tick();
                self.entries.push(Entry {
                    key: key.clone(),
                    id: a.id,
                    rect: [x, y, rw, rh],
                    size: [w, h],
                    ink: line.ink,
                    advance: line.advance,
                    published: false,
                    used,
                });
                return Some([x, y, rw, rh]);
            }
            // Evict the least recently used published entry nobody holds.
            let victim = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.published)
                .filter(|(_, e)| !pins.contains(&e.key))
                .filter(|(_, e)| !held.iter().any(|h| h.holds(&e.key)))
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i)?;
            let key = self.entries.get(victim)?.key.clone();
            self.release(&key);
            counts.evictions += 1;
        }
    }

    /// Frees `key`'s entry and the aliases naming it.
    fn release(&mut self, key: &LineKey) {
        if let Some(i) = self.entries.iter().position(|e| &e.key == key) {
            let e = self.entries.swap_remove(i);
            if let Some(a) = self.alloc.as_mut() {
                a.deallocate(e.id);
            }
        }
        self.aliases.retain(|a| &a.line != key);
    }

    /// Settles `placement` after its upload finished on the GPU: its new
    /// entries are published, its aliases recorded (the least recently
    /// used unprotected ones pruned past [`MAX_ALIASES`]), and the table's
    /// lines and the preparation's holder returned, with one note per
    /// request without a line.
    pub(crate) fn publish(
        &mut self,
        placement: Placement,
        slots: usize,
        held: &[&Holder],
    ) -> (Vec<Option<TextImage>>, Holder, Vec<(u16, TextNote)>) {
        for n in &placement.new {
            if let Some(e) = self.entries.iter_mut().find(|e| e.key == n.key) {
                e.published = true;
            }
        }
        let now = self.tick();
        let mut holder = Holder::default();
        for (alias, line) in &placement.aliases {
            match self.aliases.iter_mut().find(|a| &a.key == alias) {
                Some(a) => {
                    a.line = line.clone();
                    a.used = now;
                }
                None => self.aliases.push(Alias {
                    key: alias.clone(),
                    line: line.clone(),
                    used: now,
                }),
            }
            if !holder.aliases.contains(alias) {
                holder.aliases.push(alias.clone());
            }
        }
        while self.aliases.len() > MAX_ALIASES {
            let current = &holder;
            let Some(i) = self
                .aliases
                .iter()
                .enumerate()
                .filter(|(_, a)| !current.uses(&a.key) && !held.iter().any(|h| h.uses(&a.key)))
                .min_by_key(|(_, a)| a.used)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.aliases.swap_remove(i);
        }
        let mut lines = vec![None; slots];
        let mut notes = Vec::new();
        for (slot, s) in placement.slots {
            match s {
                Slot::Line(key) => {
                    let image = self.entry(&key).and_then(|e| self.image(e));
                    if let Some(e) = self.entries.iter_mut().find(|e| e.key == key) {
                        e.used = now;
                    }
                    if let Some(l) = lines.get_mut(usize::from(slot)) {
                        *l = image;
                    }
                    if !holder.entries.contains(&key) {
                        holder.entries.push(key);
                    }
                }
                Slot::None(Some(note)) => notes.push((slot, note)),
                Slot::None(None) => {}
            }
        }
        (lines, holder, notes)
    }

    /// Releases `placement`'s new entries, unpublished: the preparation
    /// failed after they were allocated, and the GPU has finished every
    /// write to them.
    pub(crate) fn rollback(&mut self, placement: &Placement) {
        for n in &placement.new {
            self.release(&n.key);
        }
    }

    /// The image of `e`: its line's pixels inside its rectangle on the
    /// layer, with its metrics.
    fn image(&self, e: &Entry) -> Option<TextImage> {
        let side = self.side as f32;
        let [x, y, ..] = e.rect;
        let [w, h] = e.size;
        let (l, t) = (x + GUTTER, y + GUTTER);
        let uv = FrameUv {
            uv: [l, t, l + w, t + h].map(|v| v as f32 / side),
            layer: self.layer,
        };
        TextImage::new(uv, e.ink, e.advance)
    }
}

#[cfg(test)]
mod tests {
    use skin_model::{FontRef, TextImage, TextRequest};

    use super::{Holder, MAX_ALIASES, MAX_ROUNDS, TextCache, TextCounts};
    use crate::outline::{LineBounds, LineFit, LineLayout, TextNote};

    fn req(slot: u16, text: &str, em: f32) -> TextRequest {
        TextRequest {
            slot,
            text: text.to_owned(),
            font: FontRef::Default,
            em_px: em,
        }
    }

    /// A line of `w` × `h` pixels.
    fn line(w: u32, h: u32, em: f32) -> LineFit {
        LineFit::Fits(LineLayout {
            font: FontRef::Default,
            size: [w, h],
            ink: [0.0, 0.0, 1.0, 1.0],
            advance: 1.0,
            raster_em: em,
            glyphs: Box::new([]),
        })
    }

    const SIDE: u32 = 2_048;

    fn bounds() -> LineBounds {
        LineBounds::for_side(SIDE)
    }

    type Settled = (
        Vec<Option<TextImage>>,
        Holder,
        Vec<(u16, TextNote)>,
        TextCounts,
    );

    /// Places and publishes `reqs` with `layout`.
    fn settle(
        c: &mut TextCache,
        reqs: &[TextRequest],
        held: &[&Holder],
        layout: impl FnMut(&TextRequest, f32, LineBounds) -> LineFit,
    ) -> Settled {
        let p = c.place(reqs, bounds(), held, layout);
        let counts = p.counts;
        let (lines, holder, notes) = c.publish(p, 64, held);
        (lines, holder, notes, counts)
    }

    #[test]
    fn own_pins_hold_within_a_round() {
        let mut c = TextCache::new(SIDE, 2);
        // Two lines of half the layer each fill it: A first, so A is the
        // least recently used; nobody holds either.
        let half = |_: &TextRequest, em: f32, _: LineBounds| line(2_032, 1_000, em);
        settle(&mut c, &[req(0, "A", 64.0)], &[], half);
        settle(&mut c, &[req(0, "C", 64.0)], &[], half);
        let a = c.entries.iter().find(|e| e.key.text == "A").unwrap().rect;
        // A again, then B: A is this round's hit, so B evicts C, though A
        // is older.
        let reqs = [req(0, "A", 64.0), req(1, "B", 64.0)];
        let (lines, _, notes, counts) = settle(&mut c, &reqs, &[], half);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!((counts.rounds, counts.evictions, counts.hits), (1, 1, 1));
        assert!(lines[0].is_some() && lines[1].is_some());
        let kept = c.entries.iter().find(|e| e.key.text == "A").unwrap();
        assert_eq!(kept.rect, a);
        assert!(!c.entries.iter().any(|e| e.key.text == "C"));
    }

    #[test]
    fn rollback_releases_only_new() {
        let mut c = TextCache::new(SIDE, 2);
        let small = |_: &TextRequest, em: f32, _: LineBounds| line(100, 50, em);
        let (_, outside, _, _) = settle(&mut c, &[req(0, "A", 100.0)], &[], small);
        let free = c.free();
        let reqs = [
            req(0, "A", 100.0),
            req(1, "B", 100.0),
            req(2, "B", 100.0),
            req(3, "C", 100.0),
        ];
        let p = c.place(&reqs, bounds(), &[&outside], small);
        assert_eq!(p.new.len(), 2, "duplicate keys share one entry");
        assert_eq!(c.len(), 3);
        c.rollback(&p);
        assert_eq!(c.len(), 1);
        assert_eq!(c.free(), free);
        assert!(c.entries.iter().all(|e| e.key.text == "A" && e.published));
    }

    #[test]
    fn exhaustion_with_pinned_space() {
        let mut c = TextCache::new(SIDE, 2);
        // Four lines of 2032 × 496 (512 with the gutter) fill the layer.
        let full = |_: &TextRequest, em: f32, _: LineBounds| line(2_032, 496, em);
        let reqs: Vec<_> = (0..4).map(|i| req(i, &format!("{i}"), 64.0)).collect();
        let (_, held, notes, _) = settle(&mut c, &reqs, &[], full);
        assert!(notes.is_empty(), "{notes:?}");
        // Held, they cannot be evicted: another line has no room in any
        // round, and the held lines stay.
        let reqs = [req(0, "0", 64.0), req(1, "x", 64.0)];
        let (lines, _, notes, counts) = settle(&mut c, &reqs, &[&held], full);
        assert_eq!(counts.rounds, MAX_ROUNDS);
        assert!(lines[0].is_some(), "a published hit keeps its line");
        assert_eq!(lines[1], None);
        assert_eq!(notes, [(1, TextNote::LayerFull)]);
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn alias_cap_and_protection() {
        let mut c = TextCache::new(SIDE, 2);
        // Above an em of 128 a line takes half the layer.
        let big = |_: &TextRequest, em: f32, _: LineBounds| {
            if em > 128.0 {
                line(2_032, 1_000, em)
            } else {
                line(64, 32, em)
            }
        };
        let reqs = [req(0, "T", 200.0), req(1, "T", 220.0), req(2, "U", 230.0)];
        let p = c.place(&reqs, bounds(), &[], big);
        // Round 0 fills the layer with the two T lines and U has no room;
        // round 1 sets all three at 128.
        assert_eq!(p.counts.rounds, 2);
        let (_, protected, _) = c.publish(p, 64, &[]);
        assert_eq!(c.len(), 2, "200 and 220 alias one entry");
        assert_eq!(c.aliases(), 3);
        // Their aliases find it again in the first round.
        let p = c.place(&reqs[..2], bounds(), &[], big);
        assert_eq!((p.counts.hits, p.new.len(), p.counts.rounds), (2, 0, 1));
        c.rollback(&p);
        // 320 more aliases: the held ones survive, the oldest others go.
        let small = |_: &TextRequest, em: f32, _: LineBounds| line(8, 8, em);
        for i in 0..MAX_ALIASES {
            let r = req(0, &format!("h{}", i % 8), 10.0 + (i / 8) as f32);
            let p = c.place(std::slice::from_ref(&r), bounds(), &[&protected], small);
            c.publish(p, 64, &[&protected]);
            assert!(c.aliases() <= MAX_ALIASES);
        }
        assert_eq!(c.aliases(), MAX_ALIASES);
        for a in &protected.aliases {
            assert!(c.aliases.iter().any(|x| &x.key == a), "{a:?} pruned");
        }
        let oldest = 10.0_f32.to_bits();
        assert!(
            !c.aliases
                .iter()
                .any(|x| x.key.text == "h0" && x.key.em == oldest)
        );
    }
}
