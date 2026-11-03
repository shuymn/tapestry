//! The source's built-in system images: a destination whose id is a
//! negative number draws the image the player supplies for its negation
//! (`skin/json/JSONSkinLoader.java:316-339`,
//! `skin/SkinSourceReference.java:25-43` at `0.8.9`). 110 and 111 are an
//! opaque black and an opaque white 1×1 region, which become generated
//! images of that size and colour, drawn as any image is: stretched to
//! the destination, its colour, alpha and blend applied, following the
//! covers, and an offset-4 one painting SUDDEN+. The chart's stage file,
//! back image and banner (100–102) and every other id are dropped.

use skin_model::{CoverImage, Source};

use super::super::asset::SYSTEM_SIZE;
use super::super::policy::Role;
use super::super::report::{NoteKind, Object};
use super::super::track::Stretch;
use super::super::{Common, Cx, Dst, TimerWhy};
use super::image_choice::Lowered;
use crate::error::ImportError;

/// The straight RGBA of system image `id` (a destination's id negated),
/// if it is one the importer generates: `Color.toIntBits(255, 0, 0, 0)`
/// and `Color.toIntBits(255, 255, 255, 255)`, read back as RGBA8888 by
/// the pixmap.
fn color(id: i32) -> Option<[u8; 4]> {
    match id {
        110 => Some([0, 0, 0, 255]),
        111 => Some([255, 255, 255, 255]),
        _ => None,
    }
}

impl Cx<'_> {
    /// A destination of negative id `n`: a black or white system image,
    /// else dropped as unsupported.
    pub(in super::super) fn system(&mut self, dst: Dst, n: i32) -> Result<(), ImportError> {
        let at = dst.span;
        let Some(color) = n.checked_neg().and_then(color) else {
            let why = NoteKind::Unsupported {
                object: Object::System,
            };
            return self.drop_el(Role::Decorative, at, why);
        };
        let (charged, lowered) = self.lowered(&dst)?;
        let role = lowered.role();
        if lowered == Lowered::Paint {
            return self.system_paint(&dst, role, color);
        }
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        let Some(c) = self.image_common(&dst, charged, role)? else {
            return Ok(());
        };
        let Some(image) = self.add_system(&c, color)? else {
            return Ok(());
        };
        let Some((rects, native)) = self.baked_rects(&c, SYSTEM_SIZE)? else {
            return self.drop_el(c.role, c.span, NoteKind::Stretch(c.stretch_mode));
        };
        let t = self.model_track(&c, &rects)?;
        self.push_native(c, Source::Image(image), t, native)
    }

    /// SUDDEN+'s paint of system colour `color`, as an image record's
    /// paint is made ([`Self::cover_paint_image`]); stretch 5 keeps the
    /// destination or fits it to its width as it would a 1×1 frame, its
    /// cropped rows the same colour.
    fn system_paint(&mut self, dst: &Dst, role: Role, color: [u8; 4]) -> Result<(), ImportError> {
        let Some(c) = self.image_common(dst, true, role)? else {
            return Ok(());
        };
        let at = c.span;
        let (image, rects, native) = if dst.stretch == 5 {
            if !c.track.identical() {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            }
            let r = self.solid_cover_rect(&c)?;
            let Some(image) = self.add_system(&c, color)? else {
                return Ok(());
            };
            let rect = r.model().ok_or_else(|| self.range(at, "dst"))?;
            (image, vec![rect; c.track.keys.len()], None)
        } else {
            if c.stretch == Stretch::Trimmed {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            }
            let Some(image) = self.add_system(&c, color)? else {
                return Ok(());
            };
            let Some((rects, native)) = self.baked_rects(&c, SYSTEM_SIZE)? else {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            };
            (image, rects, native)
        };
        let t = self.model_track(&c, &rects)?;
        self.push_paint(c, CoverImage::Image(image), None, t, native)
    }

    /// The system image of `color` for `c`, sampled with `c`'s filter:
    /// charged and added once, a later request charging nothing. `None`
    /// once `c` is dropped, critical and untimed where it moves or
    /// changes a colour channel other than alpha (D3).
    fn add_system(&mut self, c: &Common, color: [u8; 4]) -> Result<Option<u32>, ImportError> {
        let at = c.span;
        if self.untimed_refused(c) {
            self.drop_el(c.role, at, NoteKind::Timer(TimerWhy::Untimed))?;
            return Ok(None);
        }
        if let Some(i) = self.assets.system(color, c.filter) {
            return Ok(Some(i));
        }
        self.budget.image(1).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        super::super::trace(super::super::Event::Charge);
        let i = self
            .assets
            .add_system(color, c.filter)
            .map_err(|l| self.limit(at, l))?;
        Ok(Some(i))
    }
}
