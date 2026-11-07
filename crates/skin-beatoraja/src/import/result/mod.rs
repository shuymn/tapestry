//! The result importer (docs/stage3.md §3, Result; S10-09g): a result
//! skin's destinations through the result profile ([`profile`]), on the
//! common objects alone, as the source's result loader reads them
//! (`skin/json/JsonResultSkinObjectLoader.java`), and its essential parts.
//!
//! Its essential parts are judged after the configured options are
//! folded, over the retained elements (owner decision O4): the EX score,
//! a number showing 71 or 171, and the clear status, an element whose
//! visibility reads option 90 or 91 or that shows image index or number
//! 370. A skin without either is refused ([`ImportError::Essential`]); an
//! element showing the EX score or the clear type that cannot be
//! reproduced refuses it too, and any other is dropped with a note: one
//! that only reads option 90 or 91 in its visibility is the status when
//! retained, and dropped with a note otherwise.
//!
//! The graphs the result draws become the model's: a `judgegraph` its
//! judge graph ([`super::widget::judge_graph`]), a `gaugegraph` and a
//! `timingdistributiongraph` its gauge graph and timing distribution
//! ([`super::widget::result_graphs`]).

pub(crate) mod profile;

use super::policy::{EssentialPart, Role};
use super::report::{NoteKind, Object};
use super::{Cx, Dst};
use crate::error::ImportError;

/// What the result import holds besides the elements: whether each
/// essential part was retained.
#[derive(Debug, Default)]
pub(crate) struct State {
    score: bool,
    status: bool,
}

/// The field a visibility reads to test the clear status (options 90 and
/// 91 of [`profile::option`]), and nothing else of the profile does.
pub(super) const STATUS_FIELD: &str = "result.cleared";

impl Cx<'_> {
    /// One destination of a result skin, naming `object` of record `rec`.
    pub(super) fn result_destination(
        &mut self,
        dst: Dst,
        object: Object,
        rec: u32,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        // The source makes no image when `src` names no source
        // (`JsonSkinObjectLoader.java:42-72`).
        if object == Object::Image {
            let [src] = self.def.fields(rec, ["src"]);
            let src = self.opt_string(src, "src")?;
            if self.source_rec(src.as_deref())?.is_none() {
                self.ignored(at, NoteKind::UndefinedSource);
                return Ok(());
            }
        }
        match object {
            Object::Image => self.image(dst, rec),
            Object::ImageSet => self.image_set(dst, rec),
            Object::Value => self.number(dst, rec),
            Object::Text => self.text(dst, rec),
            Object::JudgeGraph => self.judge_graph(dst, rec),
            Object::GaugeGraph => self.gauge_graph(dst, rec),
            Object::TimingDistribution => self.timing_graph(dst, rec),
            _ => self.drop_el(Role::Decorative, at, NoteKind::Unsupported { object }),
        }
    }

    /// Records what a retained element of `role` shows of the essential
    /// parts; `tests_status` when its visibility tests the clear status.
    pub(super) fn result_retained(&mut self, role: Role, tests_status: bool) {
        match role {
            Role::Essential(EssentialPart::ResultScore) => self.res.score = true,
            Role::Essential(EssentialPart::ResultStatus) => self.res.status = true,
            _ => {}
        }
        self.res.status |= tests_status;
    }

    /// The essential parts once every destination is read: the EX score
    /// and the clear status were retained.
    pub(super) fn result_essentials(&self) -> Result<(), ImportError> {
        for (kept, part) in [
            (self.res.score, EssentialPart::ResultScore),
            (self.res.status, EssentialPart::ResultStatus),
        ] {
            if !kept {
                return Err(ImportError::Essential {
                    at: None,
                    part,
                    why: None,
                });
            }
        }
        Ok(())
    }
}

/// The role of an image set whose value gives it `role`, else `base`:
/// only a number shows the EX score (docs/stage3.md §3, Result), so a set
/// choosing by it keeps `base` and is not the score.
pub(crate) fn set_role(role: Role, base: Role) -> Role {
    if role == Role::Essential(EssentialPart::ResultScore) {
        base
    } else {
        role
    }
}

/// The role of an element on a result screen showing image index `id`.
pub(crate) fn index_role(base: Role, id: i32) -> Role {
    if profile::status_index(id) {
        Role::Essential(EssentialPart::ResultStatus)
    } else {
        base
    }
}
