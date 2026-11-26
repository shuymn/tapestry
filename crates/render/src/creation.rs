//! A play staging's and the result pages upload's array created off the
//! frame (design.md "Screen Transitions and Loading", the frame-budgeted
//! uploads; `crate::residency`): the ticket's deadline runs from its
//! request; at the boundary that takes it the array is created not
//! resident and its residency asked for (`Creating`), the boundary's heavy
//! operation; each pass polls the fence without waiting; the first call
//! that sees it complete imports the array and the staging or the upload
//! goes on as before. A ticket past its deadline is answered once with a
//! timeout; a withdrawn or timed-out ticket's array is kept, charged, as a
//! `Residing` retirement entry until its fence completes.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use bms_model::{Deadline, TimeUs};
use skin_model::{PackedPages, ScreenKind};

use super::retire::{Array, Class, Filled};
use super::{PagesStep, RenderError, Renderer};
use crate::decl::{SkinBundle, page_levels};
use crate::residency::{
    self, CREATE_BOUND, Completion, Native, NativeCreate, Path, Refusal, ResidencyError,
    ResidencyEvent, ResidencyLine, Residing, TimeoutPhase, timeout,
};
use crate::skin_atlas::{self, AtlasPages, UploadError};

/// A ticket whose array's residency is in flight.
pub(super) struct Ticket {
    /// The staging's request, or the pages upload's ticket.
    pub(super) id: u64,
    pub(super) key: NativeCreate,
    native: Native,
    value: u64,
    /// The level-0 texels admission charged for it.
    charge: u64,
    requested: TimeUs,
    deadline: Deadline,
}

/// The play staging whose array is being created: its request, kind,
/// bundle and the entry it reserved.
pub(super) struct StagingCreation {
    pub(super) kind: ScreenKind,
    pub(super) bundle: Arc<SkinBundle>,
    pub(super) entry: Option<usize>,
    pub(super) t: Ticket,
}

/// The result pages upload whose array is being created: its pages and
/// the entry it reserved.
pub(super) struct PagesCreation {
    pub(super) pages: Arc<PackedPages>,
    pub(super) entry: usize,
    pub(super) t: Ticket,
    /// When it began (G1's timing).
    #[cfg(any(test, feature = "dev-tools"))]
    pub(super) began: std::time::Instant,
}

/// How a ticket stands at a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    Pending,
    Ready,
    TimedOut,
    Removed,
}

/// How a creation's start went.
pub(super) enum Started {
    /// Created not resident, its residency asked for.
    Async(Ticket),
    /// The `Sync` path takes it.
    Sync,
}

impl Renderer {
    /// The clock deadlines are read on: the one their owner reads them on
    /// ([`set_deadline_clock`](Self::set_deadline_clock)), else the slice
    /// clock.
    pub(super) fn creation_now(&self) -> TimeUs {
        if let Some(c) = &self.creation_clock {
            return c.now();
        }
        self.slice_clock.now()
    }

    /// The clock its owner reads ticket deadlines on
    /// ([`set_ticket_deadline`](Self::set_ticket_deadline)): every deadline
    /// is compared on it, whatever clock the uploader's slice reads.
    pub fn set_deadline_clock(&mut self, clock: Arc<dyn bms_model::TimeSource>) {
        self.creation_clock = Some(clock);
    }

    /// The deadline the next ticket (a play staging's first call or a
    /// result pages begin) runs to, from its request, as its owner reads it
    /// on the render thread's clock; without one, the ticket's deadline
    /// runs from that call. The next staging or pages begin takes it,
    /// whichever path creates its array.
    pub fn set_ticket_deadline(&mut self, deadline: Deadline) {
        self.ticket_deadline = Some(deadline);
    }

    /// How skin arrays are created: the path's name and its reason
    /// (`render: array creation: <path> (<reason>)`).
    #[must_use]
    pub const fn array_creation(&self) -> (&'static str, &'static str) {
        self.creation_path.describe()
    }

    /// The residency lines since the last call, oldest first.
    pub fn take_residency_line(&mut self) -> Option<ResidencyLine> {
        let first = self.residency_lines.iter().position(Option::is_some)?;
        self.residency_lines.get_mut(first).and_then(Option::take)
    }

    /// The render loop's holder of what a renderer leaves in flight on a
    /// healthy rebuild: this renderer counts what it holds in its
    /// admission, releases each at a boundary once its fence completes,
    /// and leaves its own there when it is dropped on a device that was
    /// not removed.
    pub fn set_residency_holder(&mut self, holder: residency::ResidencyHolder) {
        self.holder = holder;
    }

    /// Development builds: the fake's path, its fence held until the
    /// caller completes it (the player's allocation guard of the poll).
    #[cfg(feature = "dev-tools")]
    pub fn use_fake_residency(&mut self) -> crate::FakeResidency {
        let f = residency::fake::Fake::default();
        self.creation_path = Path::Async(f.residency());
        f
    }

    /// Development builds: one poll of the creations in flight, as a pass
    /// outside READY's cutoff makes it. Whether the pages creation's
    /// residency completed.
    #[cfg(feature = "dev-tools")]
    pub fn poll_creations_now(&mut self) -> bool {
        let mut out = crate::Uploaded {
            bga: crate::BgaStep::Idle,
            staged: None,
            text: None,
            chart: None,
            pages: None,
        };
        self.poll_creations(&mut out)
    }

    /// Tests: the fake's path.
    #[cfg(test)]
    pub(crate) fn use_residency(&mut self, r: Box<dyn residency::Residency>) {
        self.creation_path = Path::Async(r);
    }

    /// Tests: the device's own path, as production takes it.
    #[cfg(test)]
    pub(crate) fn use_device_path(&mut self) {
        self.creation_path = residency::path(self.gpu.device(), None);
    }

    /// Tests: deadlines read on `clock`.
    #[cfg(test)]
    pub(crate) fn set_creation_clock(&mut self, clock: Arc<bms_model::VirtualClock>) {
        self.creation_clock = Some(clock as Arc<dyn bms_model::TimeSource>);
    }

    /// Tests: the fence values from `next`.
    #[cfg(test)]
    pub(crate) const fn set_fence_next(&mut self, next: u64) {
        self.fence_values.set_next(next);
    }

    /// Tests: arrays the `Sync` path created.
    #[cfg(test)]
    pub(crate) const fn sync_creations(&self) -> u32 {
        self.sync_creations
    }

    fn line(&mut self, event: ResidencyEvent, key: NativeCreate) {
        let Path::Async(r) = &self.creation_path else {
            return;
        };
        let line = ResidencyLine {
            event,
            native: r.native_size(key),
            logical: key.bytes(),
            budget: r.budget(),
        };
        // The oldest line gives way to the newest when the owner has not
        // read them.
        if let Some(free) = self.residency_lines.iter_mut().find(|l| l.is_none()) {
            *free = Some(line);
        } else {
            self.residency_lines.rotate_left(1);
            if let Some(last) = self.residency_lines.last_mut() {
                *last = Some(line);
            }
        }
    }

    /// The shape of the array of `pages`, as the upload checks it.
    pub(super) fn native_key(&self, pages: &PackedPages) -> Option<NativeCreate> {
        let levels = page_levels(pages);
        let upload = AtlasPages {
            side: pages.side(),
            pages: &levels,
            text_layer: pages.text_layer().is_some(),
            chart_layer: pages.chart_layer().is_some(),
        };
        let layers = skin_atlas::check_pages(&upload, &self.gpu.device().limits()).ok()?;
        Some(NativeCreate {
            side: pages.side(),
            layers,
            levels: u32::try_from(skin_model::MIP_LEVELS).unwrap_or(1),
        })
    }

    /// Starts the creation of `key`'s array for ticket `id`, charged at
    /// `charge` texels, by `deadline` (from this call without one): created not resident and its residency asked for,
    /// the boundary's heavy operation; or the `Sync` path, where the
    /// device lacks the path, a request is refused as unsupported, or no
    /// fence value is left for its residency.
    ///
    /// # Errors
    ///
    /// The device's refusal of either call; [`RenderError::DeviceLost`],
    /// latched, when either fails on a device whose removal is confirmed.
    pub(super) fn start_creation(
        &mut self,
        key: NativeCreate,
        id: u64,
        charge: u64,
        deadline: Option<Deadline>,
    ) -> Result<Started, RenderError> {
        let now = self.creation_now();
        let (requested, deadline) = match deadline {
            Some(d) => (TimeUs(d.at().0.saturating_sub(CREATE_BOUND.0)), d),
            None => (now, Deadline::after(now, CREATE_BOUND)),
        };
        if !matches!(self.creation_path, Path::Async(_)) || !self.fence_values.left() {
            return Ok(Started::Sync);
        }
        self.line(ResidencyEvent::Request, key);
        #[cfg(feature = "dev-tools")]
        let p_native = crate::probe::Span::start();
        let Path::Async(r) = &mut self.creation_path else {
            return Ok(Started::Sync);
        };
        let native = match r.create(key) {
            Ok(n) => n,
            Err(Refusal::Unsupported) => return Ok(Started::Sync),
            Err(Refusal::Failed) => {
                return Err(RenderError::Atlas(UploadError::Residency(
                    ResidencyError::Refused,
                )));
            }
            Err(Refusal::Removed) => {
                self.gpu.mark_lost();
                return Err(RenderError::DeviceLost);
            }
        };
        let Some(value) = self.fence_values.take() else {
            return Ok(Started::Sync);
        };
        match r.enqueue(&native, value, key.residency_flags()) {
            Ok(()) => {}
            // No residency was asked: nothing can use it, so it goes now.
            Err(Refusal::Unsupported) => return Ok(Started::Sync),
            Err(Refusal::Failed) => {
                return Err(RenderError::Atlas(UploadError::Residency(
                    ResidencyError::Refused,
                )));
            }
            // Its resource goes with the removed device.
            Err(Refusal::Removed) => {
                self.gpu.mark_lost();
                return Err(RenderError::DeviceLost);
            }
        }
        #[cfg(feature = "dev-tools")]
        crate::probe::cmark!("cr", "native_us", crate::probe::us(p_native.elapsed()));
        self.note_heavy();
        Ok(Started::Async(Ticket {
            id,
            key,
            native,
            value,
            charge,
            requested,
            deadline,
        }))
    }

    /// How `t` stands at `now`: its fence read once, without waiting.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(super) fn seen(&self, t: &Ticket, now: TimeUs) -> Seen {
        let Path::Async(r) = &self.creation_path else {
            return Seen::Ready;
        };
        match residency::completion(r.fence().completed(), t.value) {
            Completion::Done => Seen::Ready,
            Completion::Removed => Seen::Removed,
            Completion::Pending if t.deadline.passed(now) => Seen::TimedOut,
            Completion::Pending => Seen::Pending,
        }
    }

    /// Imports ready ticket `t`'s array, zeroed and in `COPY_DEST`, for
    /// the next upload of its shape.
    pub(super) fn import(&mut self, t: Ticket) {
        debug_assert!(t.key.zeroed(), "only a zeroed array is imported");
        let desc = skin_atlas::descriptor(t.key.side, t.key.layers, t.key.levels);
        debug_assert!(
            t.key.matches(&desc),
            "the native description matches wgpu's"
        );
        let key = t.key;
        #[cfg(feature = "dev-tools")]
        crate::probe::cmark!(
            "cr",
            "ready_us",
            (self.creation_now().0 - t.requested.0).max(0)
        );
        #[cfg(feature = "dev-tools")]
        let p_import = crate::probe::Span::start();
        let device = self.gpu.device().clone();
        let Path::Async(r) = &mut self.creation_path else {
            return;
        };
        let texture = r.import(&device, t.native, &desc);
        #[cfg(feature = "dev-tools")]
        crate::probe::cmark!("cr", "import_us", crate::probe::us(p_import.elapsed()));
        self.import_texture = Some((key, texture));
        self.line(ResidencyEvent::Ready, key);
    }

    /// Keeps ticket `t`'s array, its residency in flight, charged as a
    /// `Residing` entry in `entry`, retired from `kind`, until its fence
    /// completes; with no entry to take it, the holder keeps it, charged.
    pub(super) fn retire_ticket(&mut self, t: Ticket, entry: Option<usize>, kind: ScreenKind) {
        let Path::Async(r) = &self.creation_path else {
            return;
        };
        let held = Residing {
            native: t.native,
            fence: Arc::clone(r.fence()),
            value: t.value,
            bytes: t.key.bytes(),
            charge: t.charge,
            requested: t.requested,
            deadline: t.deadline,
            deferred: std::sync::atomic::AtomicBool::new(false),
        };
        let mut f = Filled::new(Array::Residing(held), kind, Class::Other);
        f.charge = t.charge;
        let left = match entry {
            Some(i) => self.retire.fill(i, f).err(),
            None => Some(f),
        };
        if let Some(f) = left
            && let Array::Residing(held) = f.array
        {
            self.holder.hold(held);
        }
    }

    /// The play staging's creation withdrawn: its array kept charged until
    /// its fence completes.
    pub(super) fn withdraw_staging_creation(&mut self) {
        if let Some(c) = self.staging_creation.take() {
            self.retire_ticket(c.t, c.entry, c.kind);
        }
    }

    /// The result pages upload's creation withdrawn: its array kept
    /// charged until its fence completes. Its ticket.
    pub(super) fn withdraw_pages_creation(&mut self) -> Option<u64> {
        let c = self.pages_creation.take()?;
        let ticket = c.t.id;
        self.retire_ticket(c.t, Some(c.entry), ScreenKind::Result);
        Some(ticket)
    }

    /// Once a pass, outside READY's cutoff: every ticket in flight read
    /// once, without waiting. One past its deadline is answered once with a
    /// timeout, its array kept charged as a `Residing` entry; a removed
    /// device is latched lost and its ticket answered so. Allocates
    /// nothing. Whether the pages creation's residency completed, for its
    /// import at this boundary.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub(super) fn poll_creations(&mut self, out: &mut crate::Uploaded) -> bool {
        let now = self.creation_now();
        let lost = self.gpu.lost();
        if let Some(seen) = self.staging_creation.as_ref().map(|c| self.seen(&c.t, now))
            && let Some(e) = self.creation_end(seen, lost)
            && let Some(request) = self.end_staging_creation(e)
        {
            out.staged = Some(crate::StagingEnd::Refused(request));
        }
        let pages = self.pages_creation.as_ref().map(|c| self.seen(&c.t, now));
        if let Some(seen) = pages
            && let Some(e) = self.creation_end(seen, lost)
        {
            if let Some(c) = self.pages_creation.take() {
                let ticket = c.t.id;
                self.timed_out_line(&c.t, &e, now);
                self.retire_ticket(c.t, Some(c.entry), ScreenKind::Result);
                self.pages_report = Some(PagesStep::Refused(ticket, e));
            }
            return false;
        }
        pages == Some(Seen::Ready)
    }

    /// The answer that ends a ticket seen as `seen`, if one does: the
    /// device's removal (its loss latched where the device confirms it) or
    /// loss, or its deadline passed.
    pub(super) fn creation_end(&self, seen: Seen, lost: bool) -> Option<RenderError> {
        match seen {
            Seen::Removed => {
                self.removed();
                Some(RenderError::DeviceLost)
            }
            _ if lost => Some(RenderError::DeviceLost),
            Seen::TimedOut => Some(timeout(TimeoutPhase::Residency)),
            Seen::Pending | Seen::Ready => None,
        }
    }

    /// The play staging's creation ended with `e`, answered once when its
    /// request is asked again: its array kept charged until its fence
    /// completes. Its request.
    pub(super) fn end_staging_creation(&mut self, e: RenderError) -> Option<u64> {
        let c = self.staging_creation.take()?;
        let request = c.t.id;
        self.timed_out_line(&c.t, &e, self.creation_now());
        self.retire_ticket(c.t, c.entry, c.kind);
        self.stage_failed = Some((request, e));
        Some(request)
    }

    /// The report's line for `t` answered with `e` at `now`, when `e` is
    /// its timeout.
    fn timed_out_line(&mut self, t: &Ticket, e: &RenderError, now: TimeUs) {
        if matches!(
            e,
            RenderError::Atlas(UploadError::Residency(ResidencyError::Timeout { .. }))
        ) {
            let elapsed = TimeUs(now.0.saturating_sub(t.requested.0));
            self.line(ResidencyEvent::TimedOut { elapsed }, t.key);
        }
    }

    /// The fence read its removal: the loss latched where the device
    /// confirms it.
    fn removed(&self) {
        if let Path::Async(r) = &self.creation_path
            && r.fence().removed()
        {
            self.gpu.mark_lost();
        }
    }

    /// Whether a creation is in flight: the pass stays awake for it, so
    /// its deadline is seen with no frame presented.
    #[must_use]
    pub fn creating(&self) -> bool {
        self.staging_creation.is_some() || self.pages_creation.is_some()
    }

    /// The charge of the play staging in creation, by its kind.
    pub(super) fn creation_charge(&self) -> Option<(ScreenKind, u64)> {
        self.staging_creation.as_ref().map(|c| (c.kind, c.t.charge))
    }

    /// The charge of the result pages upload in creation.
    pub(super) fn pages_creation_charge(&self) -> u64 {
        self.pages_creation.as_ref().map_or(0, |c| c.t.charge)
    }

    /// The texels the holder keeps charged.
    pub(super) fn held_charge(&self) -> u64 {
        self.holder.charge()
    }

    /// At a boundary with the heavy allowance: one resource the holder
    /// keeps whose fence completed released, the boundary's heavy
    /// operation. Whether one was.
    pub(super) fn release_held(&self) -> bool {
        let released = self.holder.release_one();
        if released {
            self.note_heavy();
        }
        released
    }

    /// How many resources wait for their residency, held in this
    /// renderer's entries or the holder, and when the oldest's ticket was
    /// taken.
    #[must_use]
    pub fn residency_pending(&self) -> Option<(usize, TimeUs)> {
        let entries = self.retire.residing();
        let held = self.holder.oldest();
        match (entries, held) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some((n, a)), Some((m, b))) => Some((n.saturating_add(m), TimeUs(a.0.min(b.0)))),
        }
    }

    /// What this renderer leaves in flight, handed to the holder when it
    /// is dropped on a device that was not removed; released with it once
    /// the device's removal is confirmed.
    pub(super) fn hand_over_residing(&mut self) {
        let removed = matches!(&self.creation_path, Path::Async(r) if r.fence().removed());
        let mut left = self.retire.take_residing();
        if let Some(c) = self.staging_creation.take()
            && let Path::Async(r) = &self.creation_path
        {
            left.push(ticket_residing(c.t, r.fence()));
        }
        if let Some(c) = self.pages_creation.take()
            && let Path::Async(r) = &self.creation_path
        {
            left.push(ticket_residing(c.t, r.fence()));
        }
        for held in left {
            if removed && held.device_removed() {
                drop(held);
            } else {
                self.holder.hold(held);
            }
        }
    }
}

fn ticket_residing(t: Ticket, fence: &residency::Fence) -> Residing {
    Residing {
        native: t.native,
        fence: Arc::clone(fence),
        value: t.value,
        bytes: t.key.bytes(),
        charge: t.charge,
        requested: t.requested,
        deadline: t.deadline,
        deferred: std::sync::atomic::AtomicBool::new(false),
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.hand_over_residing();
    }
}
