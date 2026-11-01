//! The covers' paint, each lowered to a model cover paint drawn where its
//! destination stands with its own track, timer, conditions, blend and
//! placement: SUDDEN+'s, an image or an image selection listing offset 4
//! that may reach the lanes and a `slider` of type 4; LIFT's
//! (`liftCover`) and HIDDEN+'s (`hiddenCover`), one for every
//! destination.
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
//!
//! LIFT's and HIDDEN+'s paint rise by the lift, HIDDEN+'s by its own share
//! too: the source lists offset 3 on LIFT and offsets 3 and 5 on HIDDEN+
//! (`skin/json/JsonPlaySkinObjectLoader.java:182-210`), consumed however
//! they are listed; offset 4 besides makes either follow SUDDEN+, and
//! offset 5 anywhere else is unsupported. A `disapearLine` at or above 0
//! trims the paint (`play/SkinHidden.java:60-90`): nothing while its
//! unstretched destination's upper edge is at or below the line, that
//! destination's part above the line while the line crosses it, the
//! whole paint otherwise; the line is fixed, or rises with the lift when
//! `isDisapearLineLinkLift` (default false for LIFT, true for HIDDEN+,
//! `skin/json/JsonSkin.java:380-407`). HIDDEN+'s paint shows only while
//! HIDDEN+ is on. LIFT is play-critical; HIDDEN+, which the player may
//! leave off, is dropped with a note.

use skin_model::{
    CoverImage, CoverMotion, CoverPaintDef, CoverTrim, Follow, MAX_COORD, Native, Source, Track,
};

use super::super::geom::{HEIGHT, Q};
use super::super::policy::{CriticalPart, Role};
use super::super::report::{LaneApprox, NoteKind, Object};
use super::super::track::Stretch;
use super::super::{Common, Cx, Dst, OFFSET_HIDDEN, OFFSET_LIFT, Opts, Prop};
use super::{Painted, SrcImage};
use crate::error::ImportError;

/// The rate a `slider` of this `type` shows: SUDDEN+'s share of the whole
/// lane (`skin/property/FloatPropertyFactory.java:139-151`).
const RATE_LANECOVER: i32 = 4;
/// The slider direction that moves down (`skin/SkinSlider.java:116-120`).
const DOWN: i32 = 2;

impl Cx<'_> {
    /// Adds the cover paint `def` and the element `c` drawing it on
    /// `track`.
    fn push_paint_def(
        &mut self,
        c: Common,
        def: CoverPaintDef,
        track: Track,
        native: Option<Native>,
    ) -> Result<(), ImportError> {
        let index = u32::try_from(self.widgets.cover_paints.len()).unwrap_or(u32::MAX);
        self.widgets.cover_paints.push(def);
        self.push_native(c, Source::CoverPaint(index), track, native)
    }

    /// Adds SUDDEN+'s paint `image`, moved by a slider of `range`
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
        let def = CoverPaintDef {
            image,
            slider_range: range,
            motion: CoverMotion::None,
            trim: CoverTrim::None,
            gate_track: None,
        };
        self.push_paint_def(c, def, track, native)
    }

    /// The paint of image record `rec` (`s`) for `c`: its stretch is baked
    /// into its keys as an ordinary image's; stretch 8 and 9 keep its
    /// native size. Stretch 5 maps when its track is static
    /// ([`Self::add_cover_image`]); the other trimming stretches are
    /// refused as `c`'s role says. `None` once `c` is dropped.
    fn paint_image(
        &mut self,
        c: &Common,
        dst: &Dst,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Option<Painted>, ImportError> {
        let at = c.span;
        if dst.stretch == 5 {
            if !c.track.identical() {
                self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode))?;
                return Ok(None);
            }
            let Some((image, r)) = self.add_cover_image(c, dst, rec, s)? else {
                return Ok(None);
            };
            let rect = r.model().ok_or_else(|| self.range(at, "dst"))?;
            return Ok(Some((image, vec![rect; c.track.keys.len()], None)));
        }
        if c.stretch == Stretch::Trimmed {
            self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode))?;
            return Ok(None);
        }
        self.src_image_rects(c, rec, s)
    }

    /// SUDDEN+'s paint from image record `rec` (`s`) in `role`, its keys
    /// already charged as an offset-4 candidate, moved by a slider of
    /// `range` reference pixels when one ([`Self::paint_image`]).
    pub(super) fn cover_paint_image(
        &mut self,
        dst: Dst,
        rec: u32,
        s: &SrcImage,
        role: Role,
        range: Option<u32>,
    ) -> Result<(), ImportError> {
        let Some(c) = self.image_common(&dst, true, role)? else {
            return Ok(());
        };
        let Some((image, rects, native)) = self.paint_image(&c, &dst, rec, s)? else {
            return Ok(());
        };
        let t = self.model_track(&c, &rects)?;
        self.push_paint(c, CoverImage::Image(image), range, t, native)
    }

    /// A `liftCover` destination: LIFT's paint, play-critical.
    pub(in super::super) fn lift_cover(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        self.moving_cover(dst, rec, CoverMotion::Lift)
    }

    /// A `hiddenCover` destination: HIDDEN+'s paint, dropped with a note
    /// where unsupported.
    pub(in super::super) fn hidden_cover(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        self.moving_cover(dst, rec, CoverMotion::Hidden)
    }

    /// LIFT's or HIDDEN+'s paint, as `motion` says, from the destination
    /// `dst` of record `rec`, with its own image, keys, timer, conditions,
    /// blend and placement, trimmed by the record's line.
    ///
    /// The line is reflected and scaled exactly and rounded half up (D1).
    /// When a stretch moves the paint off its destination, the unstretched
    /// destination gates it as its own track, its keys charged before it
    /// is built. A trimmed destination of negative width or height is
    /// refused as the role says. HIDDEN+'s paint with animated colour is
    /// noted: the source can skip its disabled alpha while it interpolates
    /// (`play/LaneRenderer.java:335-345`, `skin/SkinObject.java:480-519`),
    /// where the model hides it whenever HIDDEN+ is off.
    fn moving_cover(&mut self, dst: Dst, rec: u32, motion: CoverMotion) -> Result<(), ImportError> {
        let at = dst.span;
        let (role, allow, linked): (Role, &[i32], bool) = match motion {
            CoverMotion::Hidden => (Role::Decorative, &[OFFSET_LIFT, OFFSET_HIDDEN], true),
            CoverMotion::Lift | CoverMotion::None => {
                (Role::Critical(CriticalPart::Lift), &[OFFSET_LIFT], false)
            }
        };
        let [line, link] = self
            .def
            .fields(rec, ["disapearLine", "isDisapearLineLinkLift"]);
        let line = self.int_or(line, "disapearLine", -1)?;
        let link = match link {
            Some(f) => self.def.bool(f, "isDisapearLineLinkLift")?,
            None => linked,
        };
        let s = self.src_image(rec)?;
        let trim = if line < 0 {
            CoverTrim::None
        } else {
            let y = self.trim_base(at, line)?;
            if link {
                CoverTrim::Lift(y)
            } else {
                CoverTrim::Fixed(y)
            }
        };
        let o = Opts {
            follow: true,
            allow,
            ..Opts::default()
        };
        let Some(c) = self.common_with(role, &dst, o)? else {
            return Ok(());
        };
        let raw = if trim == CoverTrim::None {
            Vec::new()
        } else {
            // The signs are the exact sizes', after the offsets: rounding
            // the edges can turn a negative size into 0.
            let mut raw = Vec::with_capacity(c.track.keys.len());
            let mut negative = false;
            for k in &c.track.keys {
                let r = self.key_rect(&c, k)?;
                raw.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
                negative |= r.w < Q::int(0) || r.h < Q::int(0);
            }
            if negative {
                return self.drop_el(role, at, NoteKind::UnsupportedField { field: "dst" });
            }
            raw
        };
        let Some((image, rects, native)) = self.paint_image(&c, &dst, rec, &s)? else {
            return Ok(());
        };
        let gate_track = if trim == CoverTrim::None || raw == rects {
            None
        } else {
            self.budget.keys(raw.len()).map_err(|l| self.limit(at, l))?;
            Some(self.model_track(&c, &raw)?)
        };
        let t = self.model_track(&c, &rects)?;
        let part = match role {
            Role::Critical(part) => part,
            Role::Decorative => CriticalPart::Cover,
        };
        if motion == CoverMotion::Hidden && !c.track.fixed_color() {
            self.lane_approx(at, part, LaneApprox::HiddenDisable)?;
        }
        // The paint's own motion is the covers', noted once with any
        // following.
        if c.follow == Follow::None {
            self.lane_approx(at, part, LaneApprox::CoverMotion)?;
        }
        let def = CoverPaintDef {
            image: CoverImage::Image(image),
            slider_range: None,
            motion,
            trim,
            gate_track,
        };
        self.push_paint_def(c, def, t, native)
    }

    /// The trim line `line` (skin pixels, y up, at least 0) in reference
    /// pixels, y down, rounded half up, within ±[`MAX_COORD`].
    fn trim_base(&self, at: super::super::report::Span, line: i32) -> Result<i32, ImportError> {
        let max = i128::from(MAX_COORD);
        Q::int(line)
            .mul(self.r)
            .and_then(|v| Q::int(HEIGHT).sub(v))
            .and_then(Q::round)
            .filter(|y| (-max..=max).contains(y))
            .and_then(|y| i32::try_from(y).ok())
            .ok_or_else(|| self.range(at, "disapearLine"))
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
