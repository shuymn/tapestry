//! The result pages upload (design.md "Memory Budget", the uploader): the
//! result kind's prepared pages uploaded in steps beside the play
//! staging, on an ownership path of their own. Its begin reserves an
//! entry, releases the result kind's committed installation, admits the
//! pages and creates their array with nothing written; each boundary
//! writes one step of at most [`UPLOAD_STEP`](crate::UPLOAD_STEP) after
//! the BGA batch's; the boundary that sees its last step complete reports
//! it written, and its owner then publishes it as the result kind's
//! pages-only installation or withdraws it. No staging's begin, drop,
//! discard or commit touches it; the result kind's own operations withdraw
//! it first.

use std::sync::Arc;
use std::time::{Duration, Instant};

use skin_model::PackedPages;

use super::{
    Hold, Holder, PagesRes, Pending, PendingRes, PreparedToken, RenderError, Renderer, ScreenKind,
    UploadError, pages_texels, skin_atlas,
};

/// The result pages upload in hand: its ticket, its array under
/// construction with the pages still to write, and the entry it reserved,
/// which its publication makes the result kind's standing entry.
pub(super) struct PagesUpload {
    pub(super) ticket: u64,
    pub(super) res: PagesRes,
    writes: skin_atlas::AtlasWrites,
    entry: Option<usize>,
    /// Every page written and the last step's completion seen.
    pub(super) written: bool,
    /// When it began and when its last step was submitted (G1's timing).
    #[cfg(any(test, feature = "dev-tools"))]
    timing: (Instant, Instant),
}

/// How a result pages begin was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagesBegun {
    /// Its pages are the result kind's committed atlas: nothing reserved,
    /// released, admitted or created; its owner publishes at once.
    Installed,
    /// Its array is created and its steps follow at the boundaries.
    Begun,
}

/// How the result pages upload in hand ended, reported at a boundary.
#[derive(Debug)]
pub enum PagesStep {
    /// Every page of the ticket is written and its last step's completion
    /// seen: its owner publishes or withdraws it.
    Written(u64),
    /// A step of the ticket was refused: its array is retired.
    Refused(u64, RenderError),
    /// The renderer withdrew the ticket itself (a newer begin, or the
    /// result kind's own operation): its array is retired.
    Withdrawn(u64),
}

impl Renderer {
    /// Begins the result pages upload of `pages` for `ticket` (design.md
    /// "Memory Budget", the uploader), withdrawing one in hand first. Pages
    /// that are the result kind's committed atlas take nothing
    /// ([`PagesBegun::Installed`]). Otherwise, in order: its retirement
    /// entry is reserved (a full list refuses it before anything is
    /// released), the result kind's committed installation is released
    /// into its standing entry, the pages are admitted and charged from
    /// now, and their array is created with nothing written. Allocates.
    ///
    /// # Errors
    ///
    /// [`RenderError::ResultDisabled`] while the result kind is disabled;
    /// a full retirement list, a refused admission or a refused creation,
    /// with nothing of the upload left in hand.
    pub fn begin_pages(
        &mut self,
        pages: &Arc<PackedPages>,
        ticket: u64,
    ) -> Result<PagesBegun, RenderError> {
        let kind = ScreenKind::Result;
        if self.result_disabled {
            return Err(RenderError::ResultDisabled);
        }
        if let Some(t) = self.withdraw_pages() {
            self.pages_report = Some(PagesStep::Withdrawn(t));
        }
        if self.committed_atlas(kind) == Some(pages.id()) {
            return Ok(PagesBegun::Installed);
        }
        #[cfg(any(test, feature = "dev-tools"))]
        let began = Instant::now();
        let entry = self.reserve_room(Hold::Staging(kind), kind)?;
        self.release(kind);
        self.staging_entry = Some(entry);
        let res = self.pages_array(pages);
        self.staging_entry = None;
        // Its timing is taken at its last step's completion.
        #[cfg(any(test, feature = "dev-tools"))]
        {
            self.upload_timing = None;
        }
        match res {
            Ok(res) => {
                self.pages_upload = Some(PagesUpload {
                    ticket,
                    res,
                    writes: skin_atlas::AtlasWrites::new(Arc::clone(pages)),
                    entry: Some(entry),
                    written: false,
                    #[cfg(any(test, feature = "dev-tools"))]
                    timing: (began, began),
                });
                Ok(PagesBegun::Begun)
            }
            Err(e) => {
                self.retire.release(entry);
                Err(e)
            }
        }
    }

    /// `pages` admitted as the result kind's and their array created,
    /// nothing written.
    fn pages_array(&mut self, pages: &Arc<PackedPages>) -> Result<PagesRes, RenderError> {
        self.admit_reclaiming(ScreenKind::Result, pages_texels(pages))?;
        let (atlas, atlas_bind, _, text_cache) = self.upload_array(pages, None, true)?;
        Ok(PagesRes {
            pages: Arc::clone(pages),
            atlas,
            atlas_bind,
            text_cache,
        })
    }

    /// The ticket of the result pages upload in hand, if any.
    #[must_use]
    pub fn pages_in_hand(&self) -> Option<u64> {
        self.pages_upload.as_ref().map(|u| u.ticket)
    }

    /// Makes the written result pages upload the result kind's pages-only
    /// installation, its entry the kind's standing one: a move, with no
    /// device call, since its begin released the earlier installation.
    /// Its ticket; `None` with no written upload in hand.
    pub fn publish_pages(&mut self) -> Option<u64> {
        if !self.pages_upload.as_ref().is_some_and(|u| u.written) {
            return None;
        }
        let up = self.pages_upload.take()?;
        self.staged_result_text = None;
        self.result_pages = Some(up.res);
        self.stand(up.entry, ScreenKind::Result);
        Some(up.ticket)
    }

    /// Withdraws the result pages upload in hand, if any: its array,
    /// written or partial, retires into the entry it reserved. Its ticket.
    pub fn withdraw_pages(&mut self) -> Option<u64> {
        let up = self.pages_upload.take()?;
        let ticket = up.ticket;
        self.retire_pages(up);
        Some(ticket)
    }

    /// The result kind's own operation withdraws the upload in hand first,
    /// reported at the next boundary.
    pub(super) fn end_pages_upload(&mut self) {
        if let Some(t) = self.withdraw_pages() {
            self.pages_report = Some(PagesStep::Withdrawn(t));
        }
    }

    /// `up`'s array retired into its entry, as a pages staging's is.
    fn retire_pages(&mut self, up: PagesUpload) {
        self.retire_candidate(Pending {
            token: PreparedToken {
                request: 0,
                device: self.id,
                kind: ScreenKind::Result,
            },
            skin: None,
            pages: None,
            res: PendingRes::Pages(Some(Box::new(up.res))),
            text: Holder::default(),
            loading: Holder::default(),
            chart: None,
            pool: None,
            entry: up.entry,
            writes: None,
            text_up: None,
        });
    }

    /// Whether the upload in hand has pages to write.
    pub(super) fn pages_steps(&self) -> bool {
        self.pages_upload.as_ref().is_some_and(|u| !u.written)
    }

    /// At a completion seen: the upload whose pages are all written is
    /// written from now on. Its ticket, once.
    pub(super) fn pages_completed(&mut self) -> Option<u64> {
        let up = self.pages_upload.as_mut()?;
        if up.written || !up.writes.written() {
            return None;
        }
        up.written = true;
        let ticket = up.ticket;
        // G1's timing for a stepped upload: from its begin to its last
        // step's submission, then to the completion seen.
        #[cfg(any(test, feature = "dev-tools"))]
        {
            let (began, last) = up.timing;
            self.upload_timing = Some(super::UploadTiming {
                written: last.duration_since(began),
                done: Some(last.elapsed()),
            });
        }
        Some(ticket)
    }

    /// One step of the upload's pages, as a staging's
    /// ([`write_staging_step`](Self::write_staging_step)): up to
    /// [`UPLOAD_STEP`](crate::UPLOAD_STEP) / [`UPLOAD_WRITE`](crate::UPLOAD_WRITE)
    /// writes under one submission inside one pair of error scopes, a write
    /// after the first only while the slice has time left and `past`
    /// answers that the hold's bound has not passed; then its completion
    /// registered. Whether a step was written. A refusal retires the
    /// upload and is reported at the boundary.
    pub(super) fn write_pages_step(
        &mut self,
        started: Instant,
        steps: u32,
        slice: Duration,
        past: &mut dyn crate::UploadGate,
    ) -> bool {
        const WRITES: u64 = crate::UPLOAD_STEP / crate::UPLOAD_WRITE;
        #[cfg(not(test))]
        let _ = steps;
        #[cfg(test)]
        let cost = self.step_cost.get();
        #[cfg(test)]
        let (write_fault, submit_fault, submits, counts) = (
            self.step_write_fault,
            self.step_submit_fault,
            &self.step_submits,
            &self.step_writes,
        );
        let gpu = &self.gpu;
        let Some(up) = self.pages_upload.as_mut() else {
            return false;
        };
        if up.writes.written() {
            return false;
        }
        let (atlas, writes) = (&up.res.atlas, &mut up.writes);
        let elapsed = || {
            #[cfg(test)]
            if let Some(each) = cost {
                return each.saturating_mul(steps);
            }
            started.elapsed()
        };
        let ((number, index), refused) = skin_atlas::scoped(gpu.device(), || {
            #[cfg(test)]
            if let Some(f) = write_fault {
                f(gpu);
            }
            let mut n = 0_u64;
            loop {
                writes.write_one(gpu, atlas, crate::UPLOAD_WRITE);
                n += 1;
                if n >= WRITES || writes.written() || elapsed() >= slice || past.past() {
                    break;
                }
            }
            #[cfg(test)]
            {
                counts.borrow_mut().push(n);
                if let Some(f) = submit_fault {
                    f(gpu);
                }
                submits.set(submits.get() + 1);
            }
            gpu.submit([])
        });
        atlas.mark(number);
        #[cfg(any(test, feature = "dev-tools"))]
        {
            up.timing.1 = Instant::now();
        }
        self.step = Some(crate::upload::InFlight::register(&self.gpu, index));
        if let Some(e) = refused
            && let Some(up) = self.pages_upload.take()
        {
            let ticket = up.ticket;
            self.retire_pages(up);
            self.pages_report = Some(PagesStep::Refused(
                ticket,
                RenderError::Atlas(UploadError::Device(e)),
            ));
        }
        true
    }
}
