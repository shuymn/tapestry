//! The retirement list (design.md "Memory Budget"): twelve entries made
//! with the renderer and never grown, each free, reserved (empty, held for
//! a committed installation or an operation in flight) or filled with a
//! retired array. Nothing is retired into an entry that was not reserved
//! first, so a removal never waits and nothing is dropped inline.
//!
//! A filled entry holds the array's device resources (its GPU part, a
//! pool beside it in a part of its own), its CPU part until it is handed
//! to the render thread's worker, the pages' identity, the kind it was
//! retired from, its charge, its last use and the order it was retired
//! in. It is destroyed only once a completion covering its last use was
//! seen: the pool part first, then the array, each a heavy operation but a
//! layout's buffers, and the entry frees with the array.
//!
//! Nothing here allocates: the entries are filled, emptied and searched in
//! place.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use skin_model::{AtlasId, PackedPages, ScreenKind};

use super::Resources;
use crate::PlaySkin;
use crate::chart_pool::RetiredPool;
use crate::decl::{DeclSkin, Installation, LoadingPresentation, SkinBundle, pages_bytes};
use crate::skin_atlas::AtlasTexture;
use crate::staging::Staging;
use crate::text_cache::{Holder, TextCache};

/// The entries of the list.
pub(super) const ENTRIES: usize = 12;

/// The destructions the development record keeps, the newest last.
#[cfg(any(test, feature = "dev-tools"))]
pub const RECORD_LEN: usize = 64;

/// What an entry holds.
#[expect(
    clippy::large_enum_variant,
    reason = "the list's entries are made once and filled in place, which boxing would make allocate"
)]
pub(super) enum Entry {
    /// Nothing, and held for nothing.
    Free,
    /// Nothing yet, held for what will retire into it.
    Reserved(Hold),
    /// A retired array, until its destruction.
    Filled(Filled),
}

/// What a reserved entry is held for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    /// A kind's committed installation, filled at its removal, release or
    /// replacement.
    Standing(ScreenKind),
    /// A staging of the kind, filled with its own array when it is
    /// replaced, withdrawn or refused after its creation; its installation's
    /// standing entry after its commit.
    Staging(ScreenKind),
    /// The chart-image pool's operation: the pool a reduction or a
    /// regrowth retires.
    PoolOp,
}

/// How a filled entry is ordered for its destruction, and whether
/// admission charges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A commit's replaced installation: destroyed before the others.
    Replaced,
    /// Any other retirement: first in, first out.
    Other,
    /// A pool a reduction or a regrowth retired: first in, first out
    /// with the others, left out of admission's sum and reported apart.
    RetiredPool,
}

/// Which part of an entry a destruction took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The pool beside the array (a heavy operation).
    Pool,
    /// The array, a texture (a heavy operation).
    Array,
    /// A layout's staging and instance buffer: no texture, not heavy.
    Light,
}

/// A filled entry.
pub(super) struct Filled {
    pub(super) array: Array,
    pub(super) pool: Option<PoolPart>,
    pub(super) cpu: Option<CpuParts>,
    /// The pages' identity, while the array holds them (a layout's
    /// buffers and a pool hold none).
    pub(super) id: Option<AtlasId>,
    /// The kind it was retired from.
    pub(super) kind: ScreenKind,
    /// What admission counts for it until its destruction.
    pub(super) charge: u64,
    /// The number of the last submission that uses any part of it.
    pub(super) last_use: u64,
    /// Its place in the order of retirement, given when it is filled.
    pub(super) seq: u64,
    pub(super) class: Class,
}

impl Filled {
    /// A filled entry of `array` retired from `kind`, its order given at
    /// [`Retirements::fill`].
    pub(super) const fn new(array: Array, kind: ScreenKind, class: Class) -> Self {
        Self {
            array,
            pool: None,
            cpu: None,
            id: None,
            kind,
            charge: 0,
            last_use: 0,
            seq: 0,
            class,
        }
    }

    /// Whether a part of it is a heavy destruction still to come.
    fn heavy(&self) -> bool {
        self.pool.is_some() || self.array.heavy()
    }

    /// Whether its last use is covered by the completion `seen`.
    const fn due(&self, seen: u64) -> bool {
        self.last_use <= seen
    }

    /// Its device resources' bytes, the pool beside it included.
    pub(super) fn gpu_bytes(&self) -> u64 {
        self.array
            .bytes()
            .saturating_add(self.pool.as_ref().map_or(0, |p| p.pool.bytes()))
    }
}

/// A retired pool's device resources, and the bind group that bound it
/// when the array's own does not (a layout's replaced bind group, or the
/// one a reduction or a regrowth replaced): it goes with the pool.
pub(super) struct PoolPart {
    pub(super) pool: RetiredPool,
    #[expect(
        dead_code,
        reason = "held until the pool's destruction, which drops it"
    )]
    pub(super) bind: Option<wgpu::BindGroup>,
}

/// A filled entry's GPU part.
#[expect(
    clippy::large_enum_variant,
    reason = "held in the list's preallocated entries and moved in place, which boxing would make allocate"
)]
pub(super) enum Array {
    /// An installation's resources: another kind's slot, or play's
    /// replaced ones.
    Resources(Resources),
    /// A staging's own resources.
    Own(Box<Resources>),
    /// A pages-only installation's array.
    Pages {
        atlas: AtlasTexture,
        #[expect(
            dead_code,
            reason = "held until the entry's destruction, which drops it"
        )]
        atlas_bind: wgpu::BindGroup,
        text_cache: TextCache,
    },
    /// An array whose upload the device refused after its texture was
    /// made and its writes submitted.
    Refused {
        atlas: AtlasTexture,
        #[expect(
            dead_code,
            reason = "held until the entry's destruction, which drops it"
        )]
        atlas_bind: wgpu::BindGroup,
        #[expect(
            dead_code,
            reason = "held until the entry's destruction, which drops it"
        )]
        instances: Option<wgpu::Buffer>,
    },
    /// A layout's staging and instance buffer, and a bind group it held,
    /// with no texture of its own.
    Layout {
        staging: Staging,
        #[expect(
            dead_code,
            reason = "held until the entry's destruction, which drops it"
        )]
        instances: wgpu::Buffer,
        #[expect(
            dead_code,
            reason = "held until the entry's destruction, which drops it"
        )]
        bind: Option<wgpu::BindGroup>,
    },
    /// Nothing beside the pool part.
    None,
    /// Unit tests: a part with no device resource, heavy or not.
    #[cfg(test)]
    Fake { heavy: bool },
}

impl Array {
    /// Whether its destruction is a heavy operation: a texture's.
    const fn heavy(&self) -> bool {
        match self {
            Self::Resources(_) | Self::Own(_) | Self::Pages { .. } | Self::Refused { .. } => true,
            Self::Layout { .. } | Self::None => false,
            #[cfg(test)]
            Self::Fake { heavy } => *heavy,
        }
    }

    /// Development builds: the transition probe's `dr:kind` for its
    /// destruction, and the mark that times it.
    #[cfg(feature = "dev-tools")]
    const fn probe_kind(&self) -> (i64, &'static str) {
        match self {
            Self::Pages { .. } => (1, "dr:pages_us"),
            Self::Resources(_) => (2, "dr:slot_us"),
            Self::Layout { .. } => (3, "dr:layout_us"),
            Self::Own(_) => (4, "dr:own_us"),
            Self::Refused { .. } => (5, "dr:refused_us"),
            Self::None => (6, "dr:cpu_us"),
            #[cfg(test)]
            Self::Fake { .. } => (-1, "dr:fake_us"),
        }
    }

    /// The texture it holds.
    pub(super) fn atlas(&self) -> Option<&AtlasTexture> {
        match self {
            Self::Resources(r) => Some(&r.atlas),
            Self::Own(r) => Some(&r.atlas),
            Self::Pages { atlas, .. } | Self::Refused { atlas, .. } => Some(atlas),
            Self::Layout { .. } | Self::None => None,
            #[cfg(test)]
            Self::Fake { .. } => None,
        }
    }

    /// Its bytes as the witness counts them: the array's levels, a
    /// layout's staging and instances, and the text layer's cache.
    pub(super) fn bytes(&self) -> u64 {
        match self {
            Self::Resources(r) => r
                .atlas
                .bytes()
                .saturating_add(r.staging.bytes())
                .saturating_add(r.text_cache.bytes()),
            Self::Own(r) => r
                .atlas
                .bytes()
                .saturating_add(r.staging.bytes())
                .saturating_add(r.text_cache.bytes()),
            Self::Pages {
                atlas, text_cache, ..
            } => atlas.bytes().saturating_add(text_cache.bytes()),
            Self::Refused { atlas, .. } => atlas.bytes(),
            Self::Layout { staging, .. } => staging.bytes(),
            Self::None => 0,
            #[cfg(test)]
            Self::Fake { .. } => 0,
        }
    }
}

/// A retired array's CPU side, held by its entry until it is handed to
/// the worker, which drops it: the installation or the pages, play's
/// replaced skin, a staging's skin and its lines' holders. Dropping it
/// frees the pages when it holds their last reference.
#[derive(Default)]
pub struct CpuParts {
    pub(super) installation: Option<Installation>,
    pub(super) pages: Option<Arc<PackedPages>>,
    pub(super) skin: Option<Box<dyn PlaySkin>>,
    pub(super) staged: Option<DeclSkin>,
    pub(super) holders: [Holder; 2],
}

// The worker drops what it is handed on its own thread.
const _: fn() = || {
    fn sent<T: Send>() {}
    sent::<CpuParts>();
};

impl std::fmt::Debug for CpuParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpuParts")
            .field("id", &self.id())
            .field("bytes", &self.pages_bytes())
            .finish_non_exhaustive()
    }
}

impl CpuParts {
    /// The pages' identity it holds, if any.
    #[must_use]
    pub fn id(&self) -> Option<AtlasId> {
        self.installation
            .as_ref()
            .map(|i| i.bundle().atlas_id())
            .or_else(|| self.pages.as_ref().map(|p| p.id()))
            .or_else(|| self.staged.as_ref().map(|s| s.bundle().atlas_id()))
    }

    /// The bytes of the pages it holds on the CPU.
    #[must_use]
    pub fn pages_bytes(&self) -> u64 {
        self.installation
            .as_ref()
            .map(|i| i.bundle().shared_bytes())
            .or_else(|| self.pages.as_ref().map(|p| pages_bytes(p)))
            .or_else(|| self.staged.as_ref().map(|s| s.bundle().shared_bytes()))
            .unwrap_or(0)
    }

    /// The bundle of the installation it holds, or of its staged skin.
    pub(super) fn bundle(&self) -> Option<&Arc<SkinBundle>> {
        self.installation
            .as_ref()
            .map(Installation::bundle)
            .or_else(|| self.staged.as_ref().map(DeclSkin::bundle))
    }

    /// The loading presentation of the installation it holds, or of its
    /// staged skin.
    pub(super) fn loading(&self) -> Option<&LoadingPresentation> {
        self.installation
            .as_ref()
            .map(Installation::loading)
            .or_else(|| self.staged.as_ref().map(DeclSkin::loading))
    }

    /// What else of it the witness counts in its entry: the bundle's own
    /// bytes, the skins' prepared attempts, the holders, and with
    /// `loading` the presentation (the result's are counted apart, each
    /// distinct one once).
    pub(super) fn own_bytes(&self, loading: bool) -> u64 {
        let bytes = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let presentation = self
            .loading()
            .filter(|_| loading)
            .map_or(0, LoadingPresentation::bytes);
        let bundle = self.bundle().map_or(0, |b| b.resident_bytes());
        let staged = self.staged.as_ref().map_or(0, DeclSkin::attempt_bytes);
        let skin = self.skin.as_ref().map_or(0, |s| s.attempt_bytes());
        let holders = self
            .holders
            .iter()
            .fold(0u64, |sum, h| sum.saturating_add(bytes(h.bytes())));
        presentation
            .saturating_add(bundle)
            .saturating_add(staged)
            .saturating_add(skin)
            .saturating_add(holders)
    }
}

/// A completed entry's CPU part, taken to be handed to the worker
/// ([`Renderer::take_handoff`](super::Renderer::take_handoff)). The entry
/// keeps its array, its charge and its identity until its destruction;
/// a part the worker cannot take goes back to it
/// ([`Renderer::return_handoff`](super::Renderer::return_handoff)).
#[derive(Debug)]
pub struct Handoff {
    /// The entry it came from.
    pub entry: usize,
    /// The pages' identity it holds, if any.
    pub id: Option<AtlasId>,
    /// The kind the entry was retired from.
    pub kind: ScreenKind,
    /// The pages' bytes on the CPU.
    pub bytes: u64,
    /// The part itself.
    pub cpu: CpuParts,
}

/// One part's destruction ([`Renderer::destroy_due`](super::Renderer::destroy_due)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Destruction {
    /// The kind its entry was retired from.
    pub kind: ScreenKind,
    /// The pages' identity its entry held, if any.
    pub id: Option<AtlasId>,
    /// The part destroyed.
    pub part: Part,
    /// Its entry's CPU part was still in it and was dropped with the
    /// array, on the render thread (no handoff took it).
    pub cpu_dropped: bool,
    /// The entry is free.
    pub freed: bool,
}

/// What one [`Renderer::destroy_due`](super::Renderer::destroy_due) did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Destroyed {
    /// The heavy part destroyed, if one was.
    pub heavy: Option<Destruction>,
    /// The entries holding only a layout's buffers destroyed.
    pub light: u32,
    /// The CPU parts dropped with their arrays, on the render thread.
    pub cpu_dropped: u32,
}

/// The heavy device operations the renderer ran
/// ([`Renderer::heavy_ops`](super::Renderer::heavy_ops)): array and pool
/// creations, destructions of a texture, BGA begins and drops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeavyOps {
    /// How many ran since the renderer was made.
    pub count: u64,
    /// When the last one ended.
    pub last_end: Option<Instant>,
}

/// One destruction as the development record keeps it.
#[cfg(any(test, feature = "dev-tools"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestroyRecord {
    /// The kind its entry was retired from.
    pub kind: ScreenKind,
    /// The pages' identity its entry held.
    pub id: Option<AtlasId>,
    /// Its entry's class.
    pub class: Class,
    /// The part destroyed.
    pub part: Part,
    /// Its entry's last use.
    pub last_use: u64,
    /// The newest completion seen at its destruction.
    pub seen: u64,
    /// The thread it ran on.
    pub thread: std::thread::ThreadId,
    /// A CPU part was dropped with it.
    pub cpu_dropped: bool,
    /// It ran inside a staging's call (the intermediate rule), not at a
    /// boundary.
    pub in_call: bool,
    /// How long it took.
    pub took: std::time::Duration,
}

/// The development record: the last [`RECORD_LEN`] destructions.
#[cfg(any(test, feature = "dev-tools"))]
struct Record {
    at: [Option<DestroyRecord>; RECORD_LEN],
    total: usize,
}

/// The list itself.
pub(super) struct Retirements {
    entries: Box<[Entry; ENTRIES]>,
    /// The last order of retirement given.
    seq: u64,
    /// Bumped at every fill, destruction and handoff of an entry of the
    /// result kind.
    result_changes: u64,
    #[cfg(any(test, feature = "dev-tools"))]
    record: Box<Record>,
    /// The device whose memory the transition probe reads around each
    /// destruction (dev-tools).
    #[cfg(feature = "dev-tools")]
    probe: Option<wgpu::Device>,
}

impl Retirements {
    /// Every entry free: allocated now, never again.
    pub(super) fn new() -> Self {
        Self {
            entries: Box::new(std::array::from_fn(|_| Entry::Free)),
            seq: 0,
            result_changes: 0,
            #[cfg(any(test, feature = "dev-tools"))]
            record: Box::new(Record {
                at: [None; RECORD_LEN],
                total: 0,
            }),
            #[cfg(feature = "dev-tools")]
            probe: None,
        }
    }

    /// Development builds: `d`, whose memory the transition probe reads
    /// around each destruction.
    #[cfg(feature = "dev-tools")]
    pub(super) fn set_probe(&mut self, d: wgpu::Device) {
        self.probe = Some(d);
    }

    /// Reserves a free entry for `hold`: its index, or `None` when none is
    /// free.
    pub(super) fn reserve(&mut self, hold: Hold) -> Option<usize> {
        let (i, e) = self
            .entries
            .iter_mut()
            .enumerate()
            .find(|(_, e)| matches!(e, Entry::Free))?;
        *e = Entry::Reserved(hold);
        Some(i)
    }

    /// Whether an entry is free to reserve.
    pub(super) fn has_free(&self) -> bool {
        self.entries.iter().any(|e| matches!(e, Entry::Free))
    }

    /// The entry reserved for `hold`, if one is.
    pub(super) fn reserved(&self, hold: Hold) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| matches!(e, Entry::Reserved(h) if *h == hold))
    }

    /// Whether entry `i` is reserved, for anything.
    pub(super) fn is_reserved(&self, i: usize) -> bool {
        matches!(self.entries.get(i), Some(Entry::Reserved(_)))
    }

    /// What entry `i` is reserved for.
    pub(super) fn hold_of(&self, i: usize) -> Option<Hold> {
        match self.entries.get(i) {
            Some(Entry::Reserved(h)) => Some(*h),
            _ => None,
        }
    }

    /// Frees entry `i` if it is reserved: an operation withdrawn before
    /// it made anything, or a staging committed with nothing replaced.
    pub(super) fn release(&mut self, i: usize) {
        if let Some(e) = self.entries.get_mut(i)
            && matches!(e, Entry::Reserved(_))
        {
            *e = Entry::Free;
        }
    }

    /// Holds reserved entry `i` for `hold` from now on: a staging's own
    /// entry becomes its installation's standing entry at its commit.
    pub(super) fn rehold(&mut self, i: usize, hold: Hold) {
        if let Some(Entry::Reserved(h)) = self.entries.get_mut(i) {
            *h = hold;
        }
    }

    /// Fills reserved entry `i` with `f`, its order of retirement given
    /// now; `f` back when `i` is not reserved.
    #[expect(
        clippy::result_large_err,
        reason = "the entry comes back to its caller in place; boxing it would allocate"
    )]
    pub(super) fn fill(&mut self, i: usize, mut f: Filled) -> Result<(), Filled> {
        let Some(e) = self.entries.get_mut(i) else {
            return Err(f);
        };
        if !matches!(e, Entry::Reserved(_)) {
            return Err(f);
        }
        self.seq += 1;
        f.seq = self.seq;
        if f.kind == ScreenKind::Result {
            self.result_changes += 1;
        }
        *e = Entry::Filled(f);
        Ok(())
    }

    /// The filled entries, with their indices.
    pub(super) fn filled_entries(&self) -> impl Iterator<Item = (usize, &Filled)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Entry::Filled(f) => Some((i, f)),
                _ => None,
            })
    }

    /// How many entries are filled, retired pools included.
    pub(super) fn filled(&self) -> u32 {
        let n = self.filled_entries().count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// What admission counts for the filled entries: every one's charge
    /// but a retired pool's.
    pub(super) fn charged(&self) -> u64 {
        self.filled_entries()
            .filter(|(_, f)| f.class != Class::RetiredPool)
            .fold(0, |sum, (_, f)| sum.saturating_add(f.charge))
    }

    /// The retired pools' charges, reported apart.
    pub(super) fn retired_pool(&self) -> u64 {
        self.filled_entries()
            .filter(|(_, f)| f.class == Class::RetiredPool)
            .fold(0, |sum, (_, f)| sum.saturating_add(f.charge))
    }

    /// The entry holding a pool a reduction or a regrowth retired, while
    /// it is undestroyed.
    pub(super) fn retired_pool_entry(&self) -> Option<usize> {
        self.filled_entries()
            .find(|(_, f)| f.class == Class::RetiredPool)
            .map(|(i, _)| i)
    }

    /// Whether a filled entry still holds a pool array: a pool a reduction
    /// or a regrowth retired, or a replaced or removed selection's, or a
    /// dropped selection candidate's, beside its atlas. No pool array is
    /// made while one does (design.md "Memory Budget": at most one pool
    /// array beyond the live one).
    pub(super) fn pool_held(&self) -> bool {
        self.filled_entries()
            .any(|(_, f)| f.class == Class::RetiredPool || f.pool.is_some())
    }

    /// The oldest filled entry, of those admission charges when
    /// `charged` (a retired pool left out), of any otherwise.
    pub(super) fn oldest(&self, charged: bool) -> Option<usize> {
        self.filled_entries()
            .filter(|(_, f)| !charged || f.class != Class::RetiredPool)
            .min_by_key(|(_, f)| f.seq)
            .map(|(i, _)| i)
    }

    /// Entry `i`'s last use, when it is filled.
    pub(super) fn last_use(&self, i: usize) -> Option<u64> {
        match self.entries.get(i) {
            Some(Entry::Filled(f)) => Some(f.last_use),
            _ => None,
        }
    }

    /// Whether a filled entry with a heavy part is due under `seen`.
    pub(super) fn due_heavy(&self, seen: u64) -> bool {
        self.filled_entries().any(|(_, f)| f.heavy() && f.due(seen))
    }

    /// Whether a filled entry waits for a completion past `seen`.
    pub(super) fn pending_completion(&self, seen: u64) -> bool {
        self.filled_entries().any(|(_, f)| !f.due(seen))
    }

    /// The ids of the entries retired from `kind` that hold one, in the
    /// list's order.
    pub(super) fn ids(&self, kind: ScreenKind) -> [Option<AtlasId>; ENTRIES] {
        let mut out = [None; ENTRIES];
        for (o, e) in out.iter_mut().zip(self.entries.iter()) {
            if let Entry::Filled(f) = e
                && f.kind == kind
            {
                *o = f.id;
            }
        }
        out
    }

    /// For each entry by its index, whether it is filled and still holds
    /// its CPU part (development builds: the result's holders).
    #[cfg(any(test, feature = "dev-tools"))]
    pub(super) fn holding_cpu(&self) -> [bool; ENTRIES] {
        let mut out = [false; ENTRIES];
        for (o, e) in out.iter_mut().zip(self.entries.iter()) {
            *o = matches!(e, Entry::Filled(f) if f.cpu.is_some());
        }
        out
    }

    /// The counter of the result kind's entries' changes.
    pub(super) const fn result_changes(&self) -> u64 {
        self.result_changes
    }

    /// The heavy entry due under `seen` destroyed first: a commit's
    /// replaced installation before any other, then first in, first out.
    fn next_heavy(&self, seen: u64) -> Option<usize> {
        self.filled_entries()
            .filter(|(_, f)| f.heavy() && f.due(seen))
            .min_by_key(|(_, f)| (f.class != Class::Replaced, f.seq))
            .map(|(i, _)| i)
    }

    /// Destroys entry `i`'s next part: its pool, else its array, its CPU
    /// part with the array, the entry freed then (and with the pool when
    /// nothing else is left). The caller made sure its last use is
    /// covered by `seen`.
    pub(super) fn destroy_part(
        &mut self,
        i: usize,
        seen: u64,
        in_call: bool,
    ) -> Option<Destruction> {
        let e = self.entries.get_mut(i)?;
        let Entry::Filled(f) = e else {
            return None;
        };
        // The probe on: the part's destruction between memory readings
        // (`dr:*`), none of them in its time.
        #[cfg(feature = "dev-tools")]
        let probe = self.probe.as_ref().filter(|_| crate::probe::on()).map(|d| {
            let (kind, timed) = match f.pool {
                Some(_) => (0, "dr:pool_us"),
                None => f.array.probe_kind(),
            };
            crate::probe::mark("dr:site", if in_call { 2 } else { 1 });
            crate::probe::mem(d, "dr:l0", "dr:n0", "dr:r0");
            crate::probe::mark("dr:kind", kind);
            (d, timed)
        });
        let began = Instant::now();
        // The probe's own work inside the destruction (an array's first
        // use given up) left out of its mark.
        #[cfg(feature = "dev-tools")]
        let p_span = crate::probe::Span::at(began);
        let (kind, id, class, last_use) = (f.kind, f.id, f.class, f.last_use);
        let (part, cpu_dropped, freed) = match f.pool.take() {
            Some(pool) => {
                drop(pool);
                let freed = matches!(f.array, Array::None) && f.cpu.is_none();
                if freed {
                    *e = Entry::Free;
                }
                (Part::Pool, false, freed)
            }
            None => {
                let heavy = f.array.heavy();
                let cpu = f.cpu.is_some();
                // The array, its CPU part and the entry go together.
                *e = Entry::Free;
                (if heavy { Part::Array } else { Part::Light }, cpu, true)
            }
        };
        let took = began.elapsed();
        #[cfg(feature = "dev-tools")]
        if let Some((d, timed)) = probe {
            crate::probe::mark(timed, crate::probe::us(p_span.elapsed()));
            crate::probe::mark("dr:with_cpu", i64::from(cpu_dropped));
            crate::probe::mem(d, "dr:l1", "dr:n1", "dr:r1");
            crate::probe::arm_post(8);
        }
        if kind == ScreenKind::Result {
            self.result_changes += 1;
        }
        #[cfg(any(test, feature = "dev-tools"))]
        self.note(DestroyRecord {
            kind,
            id,
            class,
            part,
            last_use,
            seen,
            thread: std::thread::current().id(),
            cpu_dropped,
            in_call,
            took,
        });
        #[cfg(not(any(test, feature = "dev-tools")))]
        let _ = (class, last_use, seen, in_call, took);
        Some(Destruction {
            kind,
            id,
            part,
            cpu_dropped,
            freed,
        })
    }

    /// At a boundary: every due entry holding only light parts destroyed,
    /// and with `heavy` one heavy part first, by priority.
    pub(super) fn destroy_due(&mut self, seen: u64, heavy: bool) -> (Destroyed, bool) {
        let mut out = Destroyed::default();
        let mut ran = false;
        if heavy
            && let Some(i) = self.next_heavy(seen)
            && let Some(d) = self.destroy_part(i, seen, false)
        {
            ran = true;
            out.cpu_dropped += u32::from(d.cpu_dropped);
            out.heavy = Some(d);
        }
        for i in 0..ENTRIES {
            let light = matches!(
                self.entries.get(i),
                Some(Entry::Filled(f)) if !f.heavy() && f.due(seen)
            );
            if light && let Some(d) = self.destroy_part(i, seen, false) {
                out.light += 1;
                out.cpu_dropped += u32::from(d.cpu_dropped);
            }
        }
        (out, ran)
    }

    /// Takes the CPU part of the oldest entry due under `seen` that still
    /// holds one.
    pub(super) fn take_handoff(&mut self, seen: u64) -> Option<Handoff> {
        let i = self
            .filled_entries()
            .filter(|(_, f)| f.cpu.is_some() && f.due(seen))
            .min_by_key(|(_, f)| f.seq)
            .map(|(i, _)| i)?;
        let Some(Entry::Filled(f)) = self.entries.get_mut(i) else {
            return None;
        };
        let cpu = f.cpu.take()?;
        let kind = f.kind;
        if kind == ScreenKind::Result {
            self.result_changes += 1;
        }
        Some(Handoff {
            entry: i,
            id: cpu.id(),
            kind,
            bytes: cpu.pages_bytes(),
            cpu,
        })
    }

    /// Puts `cpu` back into entry `entry`, which a handoff took it from;
    /// `cpu` back when that entry no longer waits for it.
    #[expect(
        clippy::result_large_err,
        reason = "the part comes back to its caller in place; boxing it would allocate"
    )]
    pub(super) fn return_handoff(&mut self, entry: usize, cpu: CpuParts) -> Result<(), CpuParts> {
        match self.entries.get_mut(entry) {
            Some(Entry::Filled(f)) if f.cpu.is_none() => {
                if f.kind == ScreenKind::Result {
                    self.result_changes += 1;
                }
                f.cpu = Some(cpu);
                Ok(())
            }
            _ => Err(cpu),
        }
    }

    #[cfg(any(test, feature = "dev-tools"))]
    fn note(&mut self, r: DestroyRecord) {
        let at = self.record.total % RECORD_LEN;
        if let Some(slot) = self.record.at.get_mut(at) {
            *slot = Some(r);
        }
        self.record.total += 1;
    }

    /// The destructions recorded, oldest first: the last [`RECORD_LEN`].
    #[cfg(any(test, feature = "dev-tools"))]
    pub(super) fn record(&self) -> impl Iterator<Item = &DestroyRecord> {
        let total = self.record.total;
        let first = total.saturating_sub(RECORD_LEN);
        (first..total).filter_map(move |n| self.record.at.get(n % RECORD_LEN)?.as_ref())
    }

    /// How many destructions were recorded since the renderer was made.
    #[cfg(any(test, feature = "dev-tools"))]
    pub(super) const fn recorded(&self) -> usize {
        self.record.total
    }
}

#[cfg(test)]
mod tests {
    use skin_model::ScreenKind;

    use super::{Array, Class, ENTRIES, Entry, Filled, Hold, Part, Retirements};

    fn fake(heavy: bool, kind: ScreenKind, class: Class, last_use: u64) -> Filled {
        Filled {
            last_use,
            charge: 10,
            ..Filled::new(Array::Fake { heavy }, kind, class)
        }
    }

    /// Fills a fresh reservation of `hold` with `f`.
    fn retire(l: &mut Retirements, hold: Hold, f: Filled) -> usize {
        let i = l.reserve(hold).unwrap();
        assert!(l.fill(i, f).is_ok());
        i
    }

    /// Reservation: a free entry each time, none past the twelfth; a
    /// filled entry is never filled again, nor a free one; a withdrawal
    /// before anything is made frees its entry, and a filled one is kept.
    // Revert: `fill` accepting an entry that is not reserved -> the
    // second fill of entry 0 succeeds.
    #[test]
    fn a_reservation_takes_a_free_entry_and_a_fill_needs_one() {
        let mut l = Retirements::new();
        let mut held = Vec::new();
        for _ in 0..ENTRIES {
            held.push(l.reserve(Hold::Staging(ScreenKind::Play)).unwrap());
        }
        held.sort_unstable();
        held.dedup();
        assert_eq!(held.len(), ENTRIES);
        assert_eq!(l.reserve(Hold::PoolOp), None, "past the twelfth");
        let f = fake(true, ScreenKind::Play, Class::Other, 1);
        assert!(l.fill(0, f).is_ok());
        let again = fake(true, ScreenKind::Play, Class::Other, 1);
        assert!(l.fill(0, again).is_err(), "filled twice");
        l.release(0);
        assert_eq!(l.filled(), 1, "a filled entry is not withdrawn");
        l.release(1);
        assert_eq!(l.reserve(Hold::PoolOp), Some(1));
        assert!(matches!(l.entries.get(2), Some(Entry::Reserved(_))));
    }

    /// A staging's entry is held as its installation's standing entry
    /// after its commit; the old standing entry, filled with what it
    /// replaced, is destroyed before any other kind's.
    // Revert: `next_heavy` ordering by retirement alone (no priority for
    // a replaced installation) -> the decision's entry is destroyed first.
    #[test]
    fn a_commit_converts_the_staging_entry_and_fills_the_standing_one() {
        let mut l = Retirements::new();
        let standing = l.reserve(Hold::Standing(ScreenKind::Result)).unwrap();
        let op = l.reserve(Hold::Staging(ScreenKind::Result)).unwrap();
        let other = retire(
            &mut l,
            Hold::Staging(ScreenKind::Decision),
            fake(true, ScreenKind::Decision, Class::Other, 1),
        );
        let replaced = fake(true, ScreenKind::Result, Class::Replaced, 1);
        assert!(l.fill(standing, replaced).is_ok());
        l.rehold(op, Hold::Standing(ScreenKind::Result));
        assert_eq!(l.reserved(Hold::Standing(ScreenKind::Result)), Some(op));
        assert_eq!(l.reserved(Hold::Staging(ScreenKind::Result)), None);
        // The replaced installation first, though filled after the other.
        let (d, ran) = l.destroy_due(1, true);
        assert!(ran);
        assert_eq!(d.heavy.unwrap().kind, ScreenKind::Result);
        assert_eq!(l.filled(), 1);
        assert!(matches!(l.entries.get(other), Some(Entry::Filled(_))));
    }

    /// Others go first in, first out, a retired pool among them, one heavy
    /// part a call; light entries all go in any call once due.
    // Revert: `next_heavy` taking the newest retirement first -> the
    // selection's entry, retired after the decision's, is destroyed first.
    #[test]
    fn others_go_first_in_first_out_one_heavy_part_a_call() {
        let mut l = Retirements::new();
        let a = retire(
            &mut l,
            Hold::PoolOp,
            fake(true, ScreenKind::Selection, Class::RetiredPool, 3),
        );
        let b = retire(
            &mut l,
            Hold::Staging(ScreenKind::Play),
            fake(true, ScreenKind::Play, Class::Other, 1),
        );
        let light = retire(
            &mut l,
            Hold::Standing(ScreenKind::Result),
            fake(false, ScreenKind::Result, Class::Replaced, 1),
        );
        assert_eq!(l.charged(), 20, "a retired pool is not charged");
        assert_eq!(l.retired_pool(), 10);
        // Only `b` and the light one are due under 2.
        let (d, _) = l.destroy_due(2, false);
        assert_eq!((d.heavy, d.light), (None, 1), "no heavy part without leave");
        assert!(matches!(l.entries.get(light), Some(Entry::Free)));
        let (d, _) = l.destroy_due(2, true);
        assert_eq!(d.heavy.unwrap().kind, ScreenKind::Play);
        assert!(matches!(l.entries.get(b), Some(Entry::Free)));
        let (d, ran) = l.destroy_due(2, true);
        assert!(!ran && d.heavy.is_none(), "the pool is not due yet");
        let (d, _) = l.destroy_due(3, true);
        assert_eq!(d.heavy.unwrap().part, Part::Array);
        assert!(matches!(l.entries.get(a), Some(Entry::Free)));
        // First in, first out among equals.
        let x = retire(
            &mut l,
            Hold::Staging(ScreenKind::Play),
            fake(true, ScreenKind::Decision, Class::Other, 1),
        );
        let _y = retire(
            &mut l,
            Hold::Staging(ScreenKind::Play),
            fake(true, ScreenKind::Selection, Class::Other, 1),
        );
        assert_eq!(l.oldest(true), Some(x));
        let (d, _) = l.destroy_due(5, true);
        assert_eq!(d.heavy.unwrap().kind, ScreenKind::Decision);
        assert_eq!(l.filled(), 1, "one heavy part a call");
    }

    /// Nothing is destroyed before its completion is seen, whatever the
    /// leave; the CPU part is taken only once due, from the oldest, and
    /// goes back to its entry when the worker cannot take it.
    // Revert: `take_handoff` without its `due` filter -> a handoff is taken
    // under 6, before the entry's last use 7 is seen.
    #[test]
    fn nothing_is_destroyed_or_handed_before_its_completion() {
        let mut l = Retirements::new();
        let i = retire(
            &mut l,
            Hold::Standing(ScreenKind::Result),
            Filled {
                cpu: Some(super::CpuParts::default()),
                ..fake(true, ScreenKind::Result, Class::Replaced, 7)
            },
        );
        assert!(l.pending_completion(6));
        assert!(!l.due_heavy(6));
        let (d, ran) = l.destroy_due(6, true);
        assert!(!ran && d == super::Destroyed::default());
        assert!(l.take_handoff(6).is_none());
        let changes = l.result_changes();
        let h = l.take_handoff(7).unwrap();
        assert_eq!((h.entry, h.kind), (i, ScreenKind::Result));
        assert!(l.result_changes() > changes);
        assert!(l.return_handoff(h.entry, h.cpu).is_ok());
        let (d, _) = l.destroy_due(7, true);
        assert!(d.heavy.unwrap().cpu_dropped, "dropped with its array");
        assert_eq!(l.filled(), 0);
        let record: Vec<_> = l.record().collect();
        assert_eq!(record.len(), 1);
        assert!(record.iter().all(|r| r.last_use <= r.seen));
    }

    /// Capacity: with every kind's standing entry, a staging's entry and
    /// the one it replaces, and the pool's operation entry reserved at
    /// once, five places remain for filled entries; an entry holding
    /// nothing but metadata is light and goes in any call.
    // Revert: `ENTRIES` lowered to 11 -> four places remain, not five.
    #[test]
    fn five_places_remain_beside_every_reservation() {
        let mut l = Retirements::new();
        for kind in [
            ScreenKind::Play,
            ScreenKind::Selection,
            ScreenKind::Decision,
            ScreenKind::Result,
        ] {
            l.reserve(Hold::Standing(kind)).unwrap();
        }
        l.reserve(Hold::Staging(ScreenKind::Play)).unwrap();
        l.reserve(Hold::Staging(ScreenKind::Result)).unwrap();
        l.reserve(Hold::PoolOp).unwrap();
        let mut filled = Vec::new();
        while let Some(i) = l.reserve(Hold::Staging(ScreenKind::Decision)) {
            let mut f = fake(false, ScreenKind::Decision, Class::Other, 1);
            f.array = Array::None;
            assert!(l.fill(i, f).is_ok());
            filled.push(i);
        }
        assert_eq!(filled.len(), 5);
        let (d, ran) = l.destroy_due(1, false);
        assert!(!ran);
        assert_eq!(d.light, 5);
        assert!(
            filled
                .iter()
                .all(|&i| matches!(l.entries.get(i), Some(Entry::Free)))
        );
    }
}
