//! The decision importer (docs/stage3.md §3, Decision; S10-09f): a
//! decision skin's destinations through the decision profile
//! ([`profile`]), on the common objects alone, as the source's decision
//! loader adds none of its own
//! (`skin/json/JsonDecideSkinObjectLoader.java`). It draws images, image
//! sets and text; any other object is dropped with a note. It has no
//! essential parts: only limits and the model's checks refuse it.

pub(crate) mod profile;

use super::policy::Role;
use super::report::{NoteKind, Object};
use super::{Cx, Dst};
use crate::error::ImportError;

impl Cx<'_> {
    /// One destination of a decision skin, naming `object` of record
    /// `rec`.
    pub(super) fn decision_destination(
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
        let r = match object {
            Object::Image => self.image(dst, rec),
            Object::ImageSet => self.image_set(dst, rec),
            Object::Text => self.text(dst, rec),
            _ => self.drop_el(Role::Decorative, at, NoteKind::Unsupported { object }),
        };
        self.end_value_gate();
        r
    }
}
