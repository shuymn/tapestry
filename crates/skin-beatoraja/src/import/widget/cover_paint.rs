//! SUDDEN+'s paint: an image or an image selection listing offset 4 that
//! may reach the lanes, and a `slider` of type 4, each lowered to a model
//! cover paint drawn where its destination stands with its own track,
//! timer, conditions, blend and placement.
//!
//! Offset 4 moves the paint down by SUDDEN+'s share of the lane, offset 3
//! up by the lift's (`play/LaneRenderer.java:324-333` at `0.8.9`), as for
//! any follower. A type-4 slider moves it down by its range times
//! SUDDEN+'s share of the whole lane, `cover · (1 − lift)` while SUDDEN+
//! is on and 0 while it is off (`skin/property/FloatPropertyFactory.java:
//! 139-151`, `skin/SkinSlider.java:116-120`); both add. Nothing cuts the
//! paint to the lanes or replaces it (`skin/SkinImage.java:136-149`,
//! `skin/SkinSlider.java:109-120`), and the logical coverage never
//! depends on it.

use skin_model::{CoverImage, CoverPaintDef, MAX_COORD, Native, Source, Track};

use super::super::geom::HEIGHT;
use super::super::policy::{CriticalPart, Role};
use super::super::report::{NoteKind, Object};
use super::super::track::Stretch;
use super::super::{Common, Cx, Dst, Prop};
use super::SrcImage;
use crate::error::ImportError;

/// The rate a `slider` of this `type` shows: SUDDEN+'s share of the whole
/// lane (`skin/property/FloatPropertyFactory.java:139-151`).
const RATE_LANECOVER: i32 = 4;
/// The slider direction that moves down (`skin/SkinSlider.java:116-120`).
const DOWN: i32 = 2;

impl Cx<'_> {
    /// Adds the cover paint `image`, moved by a slider of `range`
    /// reference pixels when one, and the element `c` drawing it on
    /// `track`.
    pub(super) fn push_paint(
        &mut self,
        c: Common,
        image: CoverImage,
        range: Option<u32>,
        track: Track,
        native: Option<Native>,
    ) -> Result<(), ImportError> {
        let index = u32::try_from(self.widgets.cover_paints.len()).unwrap_or(u32::MAX);
        self.widgets.cover_paints.push(CoverPaintDef {
            image,
            slider_range: range,
        });
        self.push_native(c, Source::CoverPaint(index), track, native)
    }

    /// SUDDEN+'s paint from image record `rec` (`s`) in `role`, its keys
    /// already charged as an offset-4 candidate, moved by a slider of
    /// `range` reference pixels when one. Its stretch is baked into its
    /// keys as an ordinary image's; stretch 8 and 9 keep its native size.
    /// Stretch 5 maps when its track is static, as a lane cover's does
    /// ([`Self::add_cover_image`]); the other trimming stretches are
    /// refused as the role says.
    pub(super) fn cover_paint_image(
        &mut self,
        dst: Dst,
        rec: u32,
        s: &SrcImage,
        role: Role,
        range: Option<u32>,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let Some(c) = self.image_common(&dst, true, role)? else {
            return Ok(());
        };
        if dst.stretch == 5 {
            if !c.track.identical() {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            }
            let Some((image, r)) = self.add_cover_image(&c, &dst, rec, s)? else {
                return Ok(());
            };
            let rect = r.model().ok_or_else(|| self.range(at, "dst"))?;
            let rects = vec![rect; c.track.keys.len()];
            let t = self.model_track(&c, &rects)?;
            return self.push_paint(c, CoverImage::Image(image), range, t, None);
        }
        if c.stretch == Stretch::Trimmed {
            return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
        }
        let Some((image, t, native)) = self.src_image_track(&c, rec, s)? else {
            return Ok(());
        };
        self.push_paint(c, CoverImage::Image(image), range, t, native)
    }

    /// A `slider` (`skin/json/JsonSkinObjectLoader.java:393-417`): one of
    /// type 4 moving down, without a value property or a numeric
    /// reference, is SUDDEN+'s paint, play-critical unless it is proven
    /// outside the lanes as an offset-4 image is; `changeable` needs
    /// nothing, since the play has no pointer, and `event` is unused on
    /// that path. Its range is scaled from the skin's height to 1 080
    /// reference pixels and truncated once, exactly. Any other slider is
    /// dropped with a note.
    pub(in super::super) fn slider(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let [ty, angle, range, value, ref_num, changeable, event] = self.def.fields(
            rec,
            [
                "type",
                "angle",
                "range",
                "value",
                "isRefNum",
                "changeable",
                "event",
            ],
        );
        let ty = self.int_or(ty, "type", 0)?;
        let angle = self.int_or(angle, "angle", 0)?;
        let range = self.int_or(range, "range", 0)?;
        let value = self.prop(value, "value")?;
        let ref_num = match ref_num {
            Some(f) => self.def.bool(f, "isRefNum")?,
            None => false,
        };
        if let Some(f) = changeable {
            self.def.bool(f, "changeable")?;
        }
        let field = |field| NoteKind::UnsupportedField { field };
        let why = if value != Prop::Null {
            Some(field("value"))
        } else if ref_num {
            Some(field("isRefNum"))
        } else if ty != RATE_LANECOVER {
            Some(NoteKind::Unsupported {
                object: Object::Slider,
            })
        } else if angle != DOWN {
            Some(field("angle"))
        } else if range < 0 {
            Some(field("range"))
        } else {
            None
        };
        if let Some(why) = why {
            return self.drop_el(Role::Decorative, at, why);
        }
        // `⌊1080 · range / height⌋`, at most `MAX_COORD`.
        let r = (HEIGHT * i128::from(range))
            .checked_div(i128::from(self.height))
            .and_then(|r| u32::try_from(r).ok())
            .filter(|&r| i128::from(r) <= i128::from(MAX_COORD))
            .ok_or_else(|| self.range(at, "range"))?;
        if event.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "event" });
        }
        let s = self.src_image(rec)?;
        let role = if self.offset4_outside(&dst)? {
            Role::Decorative
        } else {
            Role::Critical(CriticalPart::Cover)
        };
        self.cover_paint_image(dst, rec, &s, role, Some(r))
    }
}
