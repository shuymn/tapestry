//! The bar line groups (`note.group`): each record an image drawn at every
//! chart bar, lowered to a model bar group in order, repeats kept.
//!
//! The source makes one image per record from the first image of its id,
//! with that image's whole grid, timer and cycle, and the record's
//! destination (`JsonPlaySkinObjectLoader.java:94-113` at `0.8.9`), then
//! draws every record at each bar line shifted by the line's distance
//! from the lanes' bottom (`play/LaneRenderer.java:441-445`). The image's
//! `len`, `ref` and actions are unused there and here. A record whose
//! image is missing or fails is dropped with a note where the source
//! would fail on it later (`LaneRenderer.java:443-444`); a record without
//! keys, which the source fails on at once (`:97-98`), refuses.

use skin_model::{BarGroupDef, Place};

use super::super::policy::{CriticalPart, Role};
use super::super::report::{LaneApprox, NoteKind, Span, TrackWhy};
use super::super::track::Stretch;
use super::super::{Cx, Opts};
use crate::error::ImportError;

/// The groups the lanes keep, and each one's record.
pub(crate) type Groups = (Vec<BarGroupDef>, Vec<Span>);

impl Cx<'_> {
    /// The groups of the lanes element at `at` from its `group` field, as
    /// resolved: the count charged before anything is read, each record's
    /// keys charged before they are read, each group moving with LIFT and
    /// SUDDEN+ by offsets 3 and 4 and noted as D9
    /// [`LaneApprox::BarLine`]. A group's own clip is ignored: the source
    /// draws the image itself, outside any clip (`skin/SkinImage.java:
    /// 169-174`).
    pub(super) fn bar_groups(
        &mut self,
        group: Option<u32>,
        at: Span,
    ) -> Result<Groups, ImportError> {
        let n = group
            .and_then(|f| self.tables.nested.get(&f))
            .map_or(0, Vec::len);
        self.budget.bar_groups(n).map_err(|l| self.limit(at, l))?;
        let records = group
            .and_then(|f| self.tables.nested.get(&f))
            .cloned()
            .unwrap_or_default();
        let mut groups = Vec::with_capacity(records.len());
        let mut spans = Vec::with_capacity(records.len());
        for rec in records {
            let Some(g) = self.bar_group(rec)? else {
                continue;
            };
            groups.push(g.0);
            spans.push(g.1);
        }
        Ok((groups, spans))
    }

    /// Group record `rec`: `None` when it is dropped.
    fn bar_group(&mut self, rec: u32) -> Result<Option<(BarGroupDef, Span)>, ImportError> {
        let role = Role::Decorative;
        let at = self.arena_span(rec);
        let [dst] = self.def.fields(rec, ["dst"]);
        let keys = dst
            .and_then(|f| self.tables.nested.get(&f))
            .map_or(0, Vec::len);
        if keys == 0 {
            return Err(ImportError::Type {
                at: self.def.loc(dst.unwrap_or(rec)),
                field: "dst",
                expected: "a non-empty array",
            });
        }
        let fits = self.budget.keys(keys).map_err(|l| self.limit(at, l))?;
        if !fits {
            self.drop_el(role, at, NoteKind::Track(TrackWhy::Limits))?;
            return Ok(None);
        }
        let (id, dst) = self.read_nested(rec)?;
        let Some(img) = id.map(|id| self.image_rec(&id)).transpose()?.flatten() else {
            self.drop_el(role, at, NoteKind::UndefinedObject)?;
            return Ok(None);
        };
        let s = self.src_image(img)?;
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        let o = Opts {
            follow: true,
            charged: true,
            unclipped: true,
            ..Opts::default()
        };
        let Some(c) = self.common_with(role, &dst, o)? else {
            return Ok(None);
        };
        let Some((image, track, native)) = self.src_image_track(&c, img, &s)? else {
            return Ok(None);
        };
        self.retained(&c)?;
        self.lane_approx(at, CriticalPart::Lanes, LaneApprox::BarLine)?;
        let def = BarGroupDef {
            image,
            track,
            timer: c.timer,
            visible: c.visible,
            blend: c.blend,
            place: Place {
                pivot: c.pivot,
                native,
                follow: c.follow,
                ..Place::SCREEN
            },
        };
        Ok(Some((def, at)))
    }
}
