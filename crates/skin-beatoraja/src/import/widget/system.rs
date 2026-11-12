//! The source's built-in system images: a destination whose id is a
//! negative number draws the image the player supplies for its negation
//! (`skin/json/JSONSkinLoader.java:316-339`,
//! `skin/SkinSourceReference.java:25-43` at `0.8.9`). 110 and 111 are an
//! opaque black and an opaque white 1×1 region, which become generated
//! images of that size and colour, drawn as any image is: stretched to
//! the destination, its colour, alpha and blend applied, following the
//! covers, and an offset-4 one painting SUDDEN+. On a play skin, the
//! chart's stage file and back image (100 and 101) become chart images
//! (S10-12 design §6): drawn from the chart layer the player writes, their
//! stretch applied at placement against the image's own size. The banner
//! (102), 100 and 101 on any other screen, and every other id are dropped,
//! or refuse the skin when they are failure presentation.

use skin_model::Source;
use skin_model::chart_image::ChartImage;

use super::super::asset::SYSTEM_SIZE;
use super::super::policy::Role;
use super::super::report::{NoteKind, Object};
use super::super::track::Stretch;
use super::super::{Common, Cx, Dst, TimerWhy};
use super::Painted;
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
        let chart = match n {
            -100 => Some(ChartImage::Stage),
            -101 => Some(ChartImage::Back),
            _ => None,
        };
        if let Some(which) = chart
            && self.kind == skin_model::ScreenKind::Play
            && !dst.failure
        {
            return self.chart_image(dst, which);
        }
        let Some(color) = n.checked_neg().and_then(color) else {
            let why = NoteKind::Unsupported {
                object: Object::System,
            };
            return self.drop_el(Role::Decorative.or_failure(dst.failure), at, why);
        };
        let (charged, lowered) = self.lowered(&dst)?;
        let role = lowered.role(dst.failure);
        if lowered == Lowered::Paint {
            // SUDDEN+'s paint, made as an image record's is; under stretch
            // 5 a 1x1 frame's rect.
            return self.cover_paint_with(dst, role, None, |cx, c| {
                cx.paint_rects(
                    c,
                    |cx| {
                        let r = cx.solid_cover_rect(c)?;
                        Ok(cx.add_system(c, color)?.map(|i| (i, r)))
                    },
                    |cx| cx.system_rects(c, color),
                )
            });
        }
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        let Some(c) =
            self.turning_image_common(&dst, charged, (super::image_choice::Lowered::Image, role))?
        else {
            return Ok(());
        };
        let Some((image, rects, native)) = self.system_rects(&c, color)? else {
            return Ok(());
        };
        let t = self.model_track(&c, &rects)?;
        self.push_native(c, Source::Image(image), t, native)
    }

    /// A play destination of the chart's stage file or back image: an
    /// element of [`Source::ChartImage`], its keys' rects unbaked, as the
    /// image's size is known only when the player publishes it, and its
    /// stretch kept for placement. A trimmed stretch is dropped, as a
    /// system image's is, and an offset-4 one that may reach the lanes too:
    /// the chart's image is never SUDDEN+'s paint. Charges no image.
    fn chart_image(&mut self, dst: Dst, which: ChartImage) -> Result<(), ImportError> {
        let at = dst.span;
        let (charged, lowered) = self.lowered(&dst)?;
        let role = super::super::policy::Role::Decorative;
        if lowered == Lowered::Paint {
            return self.drop_el(
                role,
                at,
                NoteKind::Unsupported {
                    object: Object::System,
                },
            );
        }
        let Some(stretch) = Stretch::of(dst.stretch).chart() else {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        };
        let Some(c) = self.turning_image_common(&dst, charged, (Lowered::Image, role))? else {
            return Ok(());
        };
        if self.untimed_refused(&c) {
            return self.drop_el(c.role, at, NoteKind::Timer(TimerWhy::Untimed));
        }
        let mut rects = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            rects.push(
                r.model_from(self.flip)
                    .ok_or_else(|| self.range(at, "dst"))?,
            );
        }
        let t = self.model_track(&c, &rects)?;
        let filter = c.filter;
        self.push_native(
            c,
            Source::ChartImage {
                which,
                filter,
                stretch,
            },
            t,
            None,
        )
    }

    /// The system image of `color` for `c` and `c`'s keys baked against
    /// its 1x1 cell, as [`Self::src_image_rects`] gives an image record's.
    /// `None` once `c` is dropped.
    fn system_rects(&mut self, c: &Common, color: [u8; 4]) -> Result<Option<Painted>, ImportError> {
        let Some(image) = self.add_system(c, color)? else {
            return Ok(None);
        };
        self.baked_image(c, image, SYSTEM_SIZE)
    }

    /// The system image of `color` for `c`, sampled with `c`'s filter:
    /// charged and added once, a later request charging nothing. `None`
    /// once `c` is dropped, critical and untimed where it moves or
    /// changes a colour channel other than alpha, unless it settles (D3).
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
