//! The render thread's retirement worker and its handoff table (design.md
//! "Memory Budget", the worker handoff): twelve places made with the
//! render loop, never grown, each holding the identity, kind and bytes of
//! a retired array's CPU part the worker was handed, until the worker's
//! release of it is read. The worker drops each part it is sent, then
//! sets that place's release flag; the render thread reads the flags
//! outside READY's cutoff and frees the places it finds set.
//!
//! The table belongs to the render loop, not to the renderer, so a
//! renderer dropped on a device loss leaves it and the parts the worker
//! still holds in place, and the result's atlas report keeps naming them.
//! A send moves the part and a place's index through a channel bounded at
//! the table's size, and a release is one store: nothing here allocates
//! once the table is made.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};

use skin_model::{AtlasId, ScreenKind};

/// The table's places: one for each retirement entry.
pub const PLACES: usize = render::RETIREMENT_ENTRIES;

/// What the worker is handed: moved through the channel, never boxed.
#[cfg_attr(
    test,
    expect(
        clippy::large_enum_variant,
        reason = "a part moves into the channel's preallocated slot without allocating"
    )
)]
enum Handed {
    /// A retired entry's CPU part.
    Parts(render::CpuParts),
    /// Pages a test seeded into a place, with no entry behind them
    /// (tests).
    #[cfg(test)]
    Pages(
        #[expect(dead_code, reason = "held until the worker drops it")]
        Arc<skin_model::PackedPages>,
    ),
}

/// A place in use: what it holds until its release is read.
#[derive(Clone, Copy, Debug)]
struct Place {
    id: Option<AtlasId>,
    kind: ScreenKind,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests read the memory witness")
    )]
    bytes: u64,
}

/// What one read of the release flags freed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Released {
    /// The places freed.
    pub freed: u32,
    /// Whether one of them held the result kind's pages.
    pub result: bool,
}

/// The handoff table and the worker's end of it.
pub struct Handoffs {
    places: [Option<Place>; PLACES],
    released: Arc<[AtomicBool; PLACES]>,
    /// The worker's channel; `None` once a send found the worker gone, or
    /// when it could not be started: parts stay in their entries, dropped
    /// with their arrays.
    tx: Option<SyncSender<(usize, Handed)>>,
}

impl std::fmt::Debug for Handoffs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handoffs")
            .field("places", &self.places)
            .field("worker", &self.tx.is_some())
            .finish()
    }
}

impl Handoffs {
    /// The table, every place free, and its worker thread started; with
    /// `gate` the worker waits at it before each drop (tests). A worker
    /// that cannot start leaves every part in its entry.
    pub fn start(#[cfg(test)] gate: Arc<crate::render_side::TestGate>) -> Self {
        let released: Arc<[AtomicBool; PLACES]> =
            Arc::new(std::array::from_fn(|_| AtomicBool::new(false)));
        let (tx, rx) = mpsc::sync_channel::<(usize, Handed)>(PLACES);
        let flags = Arc::clone(&released);
        let started = std::thread::Builder::new()
            .name("retire".into())
            .spawn(move || {
                for (place, part) in rx {
                    #[cfg(test)]
                    gate.pass();
                    drop(part);
                    if let Some(flag) = flags.get(place) {
                        flag.store(true, Ordering::Release);
                    }
                }
            });
        if let Err(e) = &started {
            crate::render_report::report(format_args!(
                "render: the retirement worker did not start, retired pages are dropped with their arrays: {e}"
            ));
        }
        Self {
            places: [None; PLACES],
            released,
            tx: started.is_ok().then_some(tx),
        }
    }

    /// Reads the release flags and frees each place the worker released.
    pub fn read_releases(&mut self) -> Released {
        let mut out = Released::default();
        for (place, flag) in self.places.iter_mut().zip(self.released.iter()) {
            if place.is_some() && flag.swap(false, Ordering::Acquire) {
                out.freed += 1;
                out.result |= place.is_some_and(|p| p.kind == ScreenKind::Result && p.id.is_some());
                *place = None;
            }
        }
        out
    }

    /// Whether a send could take a place now.
    pub fn has_free(&self) -> bool {
        self.tx.is_some() && self.places.iter().any(Option::is_none)
    }

    /// Whether a place awaits its release.
    pub fn awaiting(&self) -> bool {
        self.places.iter().any(Option::is_some)
    }

    /// Hands `handoff`'s CPU part to the worker in a free place, which
    /// records its identity, kind and bytes. With no place free or the
    /// worker gone, the part comes back with its entry's index, for the
    /// entry to drop with its array.
    ///
    /// # Errors
    ///
    /// The entry and the part, when nothing took it.
    #[expect(
        clippy::result_large_err,
        reason = "a part comes back by value, without allocating"
    )]
    pub fn send(&mut self, handoff: render::Handoff) -> Result<(), (usize, render::CpuParts)> {
        let render::Handoff {
            entry,
            id,
            kind,
            bytes,
            cpu,
        } = handoff;
        match self.put(Place { id, kind, bytes }, Handed::Parts(cpu)) {
            Ok(()) => Ok(()),
            Err(Handed::Parts(cpu)) => Err((entry, cpu)),
            #[cfg(test)]
            Err(Handed::Pages(_)) => Ok(()),
        }
    }

    /// Puts `part` in a free place recorded as `place`.
    #[expect(
        clippy::result_large_err,
        reason = "a part comes back by value, without allocating"
    )]
    fn put(&mut self, place: Place, part: Handed) -> Result<(), Handed> {
        let (Some(tx), Some((i, slot))) = (
            &self.tx,
            self.places
                .iter_mut()
                .enumerate()
                .find(|(_, p)| p.is_none()),
        ) else {
            return Err(part);
        };
        match tx.try_send((i, part)) {
            Ok(()) => {
                *slot = Some(place);
                Ok(())
            }
            Err(TrySendError::Full((_, part))) => Err(part),
            Err(TrySendError::Disconnected((_, part))) => {
                crate::render_report::report(format_args!(
                    "render: the retirement worker ended, retired pages are dropped with their arrays"
                ));
                self.tx = None;
                Err(part)
            }
        }
    }

    /// The result kind's pages' identities the places hold, by place.
    pub fn result_ids(&self) -> [Option<AtlasId>; PLACES] {
        self.places.map(|p| {
            p.filter(|p| p.kind == ScreenKind::Result)
                .and_then(|p| p.id)
        })
    }

    /// The pages the worker holds, each place's identity and bytes, into
    /// `out`, for the memory witness
    /// ([`render::Renderer::memory_with`], read by tests alone): how many
    /// it wrote.
    #[cfg(test)]
    pub fn handed(&self, out: &mut [(AtlasId, u64); PLACES]) -> usize {
        let mut n = 0;
        for (id, bytes) in self
            .places
            .iter()
            .flatten()
            .filter_map(|p| p.id.map(|id| (id, p.bytes)))
        {
            if let Some(o) = out.get_mut(n) {
                *o = (id, bytes);
                n += 1;
            }
        }
        n
    }

    /// Puts `pages` of `kind` in a free place, with no retirement entry
    /// behind them (tests): whether one took them.
    #[cfg(test)]
    pub fn seed(&mut self, kind: ScreenKind, pages: Arc<skin_model::PackedPages>) -> bool {
        let place = Place {
            id: Some(pages.id()),
            kind,
            bytes: render::pages_bytes(&pages),
        };
        self.put(place, Handed::Pages(pages)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pages of their own, each call a new identity.
    #[cfg(feature = "dev-tools")]
    fn pages() -> Arc<skin_model::PackedPages> {
        Arc::clone(crate::render_side::tests::bundle().atlas_pages())
    }

    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_seeded_place_frees_once_the_worker_released_it() {
        let gate = Arc::new(crate::render_side::TestGate::default());
        let mut t = Handoffs::start(Arc::clone(&gate));
        let p = pages();
        let id = p.id();
        let weak = Arc::downgrade(&p);
        assert!(t.seed(ScreenKind::Result, p));
        assert_eq!(
            t.result_ids().iter().flatten().copied().collect::<Vec<_>>(),
            [id]
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut released = Released::default();
        while released.freed == 0 && std::time::Instant::now() < deadline {
            released = t.read_releases();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            released,
            Released {
                freed: 1,
                result: true
            }
        );
        assert!(weak.upgrade().is_none(), "the worker dropped the pages");
        assert!(!t.awaiting());
        assert_eq!(gate.passed(), 1);
    }

    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_full_table_takes_nothing_more() {
        let gate = Arc::new(crate::render_side::TestGate::default());
        gate.hold(true);
        let mut t = Handoffs::start(Arc::clone(&gate));
        for _ in 0..PLACES {
            assert!(t.seed(ScreenKind::Play, pages()));
        }
        assert!(!t.has_free());
        assert!(!t.seed(ScreenKind::Play, pages()), "no place is free");
        assert_eq!(t.read_releases().freed, 0, "nothing released while held");
        gate.hold(false);
    }

    /// The witness counts the pages two places hold once, until their
    /// release is read.
    #[test]
    fn the_witness_counts_handed_pages_once() {
        use crate::render_decl::{Look, solid_bundle};
        let r = crate::render_common::offscreen(64, 64);
        let p = Arc::clone(solid_bundle(&r, &Look::left([255, 0, 0, 255]), [16, 16]).atlas_pages());
        let bytes = render::pages_bytes(&p);
        assert!(bytes > 0);
        let mut t = Handoffs::start(Arc::default());
        assert!(t.seed(ScreenKind::Result, Arc::clone(&p)));
        assert!(t.seed(ScreenKind::Play, p));
        let mut out = [(AtlasId(0), 0); PLACES];
        let n = t.handed(&mut out);
        assert_eq!(n, 2);
        let base = r.memory().shared_cpu;
        let with = r.memory_with(None, out.get(..n).unwrap_or_default());
        assert_eq!(with.shared_cpu - base, bytes, "the same pages counted once");
    }
}
