//! Numbers: the effective property, the glyph set and its cut, and
//! the strip's layout from each key's exact geometry.

use skin_model::{
    Align, DigitsDef, GlyphSet, MAX_COORD, Native, NumberDef, Pad, Program, Source, Track, Ty,
};

use super::super::callback::NumberValue;
use super::super::geom::{Q, Rect};
use super::super::policy::Role;
use super::super::property::{self, IntProp};
use super::super::report::{NoteKind, PropertyKind};
use super::super::track::{self, Stretch};
use super::super::{Common, Cx, Dst, Opts, Prop};
use crate::error::ImportError;
use crate::lua::residual::Conv;

/// The most cells the model draws.
const CELLS: i32 = skin_model::MAX_CELLS as i32;

/// The grid of the first `keep` of a `div` grid's `frames` frames, when
/// they form whole rows (`JsonSkinObjectLoader.java:104-170`); `None`
/// otherwise.
pub(super) fn glyphs(keep: u32, frames: u32, [dx, dy]: [u32; 2]) -> Option<[u32; 2]> {
    if keep == frames {
        Some([dx, dy])
    } else if dy == 1 {
        Some([keep, 1])
    } else if keep.is_multiple_of(dx) {
        Some([dx, keep / dx])
    } else {
        None
    }
}

/// The cells of a `digit`-digit strip and the blank cells past the
/// model's most (`SkinNumber.java:189-191`); `None` for more than the
/// model's cells without blank padding.
pub(super) fn cells(digit: i32, pad: Pad) -> Option<(i32, i32)> {
    if digit <= CELLS {
        Some((digit, 0))
    } else if pad == Pad::Blank {
        Some((CELLS, digit - CELLS))
    } else {
        None
    }
}

/// The model's space between cells for keys of `(x, rect, pitch)`: the
/// rounded pitch less the rounded width while every key has the same
/// width and pitch, else the rounded scaled `space` `sr`; `None` when it
/// does not round or lies outside the model's coordinates.
pub(super) fn cell_space(keys: &[(Q, Rect, Q)], sr: Q) -> Option<i128> {
    let same = keys
        .windows(2)
        .all(|w| matches!(w, [(_, a, pa), (_, b, pb)] if a.w == b.w && pa == pb));
    let space = match (same, keys.first()) {
        (true, Some((_, r0, p0))) => p0
            .round()
            .zip(r0.w.round())
            .and_then(|(p, w)| p.checked_sub(w)),
        _ => sr.round(),
    }?;
    (-i128::from(MAX_COORD)..=i128::from(MAX_COORD))
        .contains(&space)
        .then_some(space)
}

impl Cx<'_> {
    /// The effective integer property (`JsonSkinObjectLoader.java:
    /// 104-160`): `value` when it resolves, else `ref`.
    fn int_prop(&mut self, value: &Prop, r: i32) -> IntProp {
        match property::value_prop(self.kind, value) {
            IntProp::Null => property::int_in(self.kind, r),
            p => p,
        }
    }

    /// A number object.
    pub(in super::super) fn number(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let [value, r] = self.def.fields(rec, ["value", "ref"]);
        let value = self.callback_prop(value, "value")?;
        let r = self.int_or(r, "ref", 0)?;
        // Its role when what it shows has none of its own.
        let base = Role::Decorative.or_failure(dst.failure);
        // A callable keeps its precedence over `ref`.
        let shown = if let Prop::Callable(id) = value {
            match self.value_callback(at, id, base, Conv::Int)? {
                Some(v) => v,
                None => return Ok(()),
            }
        } else {
            match self.int_prop(&value, r) {
                IntProp::Null => {
                    self.ignored(at, NoteKind::Ignored { field: "value" });
                    return Ok(());
                }
                IntProp::Unsupported(id) => {
                    let why = NoteKind::UnsupportedProperty {
                        kind: PropertyKind::Integer,
                        id,
                    };
                    self.drop_el(base, at, why)?;
                    return Ok(());
                }
                IntProp::Unavailable(id) => {
                    let why = NoteKind::Unavailable {
                        kind: PropertyKind::Integer,
                        id,
                    };
                    self.drop_el(base, at, why)?;
                    return Ok(());
                }
                IntProp::Shown(v) => NumberValue::of(v),
            }
        };
        let role = shown.role.or_failure(dst.failure);
        let Some(DigitsStyle {
            mut c,
            digits,
            track: t,
            native,
            program,
        }) = self.digits_style(role, &dst, rec, Some(&shown))?
        else {
            return Ok(());
        };
        // One D4 note for the value and the conditions.
        c.display_deviation |= shown.deviates;
        let index = u32::try_from(self.widgets.numbers.len()).unwrap_or(u32::MAX);
        self.widgets.numbers.push(NumberDef {
            digits,
            value: program.ok_or_else(|| self.range(at, "value"))?,
        });
        self.push_native(c, Source::Number(index), t, native)?;
        Ok(())
    }

    /// Number record `rec`'s glyph strip for destination `dst` of `role`:
    /// its glyph set, padding, cells, layout and track, its digit offsets
    /// charged and kept, and with `value` its value compiled and its cells
    /// charged as widget quads; `None` when the element was dropped or is
    /// never drawn. A song list's level style shows the row's level, so it
    /// compiles no value and its quads are charged with its song list.
    pub(in super::super) fn digits_style(
        &mut self,
        role: Role,
        dst: &Dst,
        rec: u32,
        value: Option<&NumberValue>,
    ) -> Result<Option<DigitsStyle>, ImportError> {
        let at = dst.span;
        let [digit, padding, zeropadding, space, align] = self
            .def
            .fields(rec, ["digit", "padding", "zeropadding", "space", "align"]);
        let digit = self.int_or(digit, "digit", 0)?;
        let padding = self.int_or(padding, "padding", 0)?;
        let zeropadding = self.int_or(zeropadding, "zeropadding", 0)?;
        let spaces = self.int_or(space, "space", 0)?;
        let align = match self.int_or(align, "align", 0)? {
            0 => Align::Right,
            1 => Align::Left,
            // Any other value shifts by half the blank cells
            // (`SkinNumber.java:189-191`).
            _ => Align::Center,
        };
        if digit < 0 {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(None);
        }
        if digit == 0 {
            self.ignored(at, NoteKind::Ignored { field: "digit" });
            return Ok(None);
        }
        let s = self.src_image(rec)?;
        let frames = self.grid_frames(at, s.div)?;
        // A song list's level (no value given) takes the selection loader's
        // strip: ten glyphs blank-padded when the frames are a multiple of
        // ten, else eleven padded with the eleventh, its `padding` and
        // `zeropadding` unread and no digit offsets
        // (`JsonSelectSkinObjectLoader.java:181-196`).
        let (set, pad, d) = if value.is_none() {
            if frames.is_multiple_of(10) {
                (GlyphSet::Ten, Pad::Blank, 10)
            } else {
                (GlyphSet::Eleven, Pad::Glyph, 11)
            }
        } else if frames.is_multiple_of(24) {
            let pad = match zeropadding {
                ..=0 => Pad::Blank,
                2 => Pad::Glyph,
                _ => Pad::Zero,
            };
            (GlyphSet::TwentyFour, pad, 24)
        } else if frames.is_multiple_of(10) {
            let pad = match padding {
                1 => Pad::Zero,
                // The padding glyph of a ten-glyph set is out of range.
                2 => {
                    self.drop_el(role, at, NoteKind::UnsupportedField { field: "padding" })?;
                    return Ok(None);
                }
                _ => Pad::Blank,
            };
            (GlyphSet::Ten, pad, 10)
        } else {
            (GlyphSet::Eleven, Pad::Glyph, 11)
        };
        let sets = frames / d;
        if sets == 0 {
            self.ignored(at, NoteKind::Ignored { field: "src" });
            return Ok(None);
        }
        let Some(grid) = glyphs(sets * d, frames, s.div) else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "divx" })?;
            return Ok(None);
        };
        let Some((cells, extra)) = cells(digit, pad) else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(None);
        };
        if set == GlyphSet::TwentyFour && pad != Pad::Blank && cells < 2 {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(None);
        }
        let stretch = Stretch::of(dst.stretch);
        if stretch == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        let o = Opts {
            follow: true,
            guard: value.and_then(|v| v.guard.as_deref()),
            ..Opts::default()
        };
        self.number_value = value.map(|v| v.expr.clone());
        let c = self.common_with(role, dst, o);
        self.number_value = None;
        let Some(c) = c? else {
            return Ok(None);
        };
        let program = match value {
            Some(v) => Some(self.compile(&v.expr, Ty::Int, at)?),
            None => None,
        };
        let Some((image, cell)) = self.add_src_image(&c, rec, &s, grid)? else {
            return Ok(None);
        };

        // Layout, from each key's offset geometry.
        let range = |cx: &Self| cx.range(at, "dst");
        let sr = Q::int(spaces).mul(self.r).ok_or_else(|| range(self))?;
        let n = Q::int(cells - 1);
        let e = Q::int(extra);
        // Stretch 8 and 9 apply per glyph at placement, after its digit
        // offset (`skin/SkinNumber.java:191-205`); any other mode must
        // leave each key's cell, moved by each row, as it is.
        let native = c.stretch.native(cell);
        // A level reads no digit offsets, so it has no rows to keep.
        let rows_kept = c.stretch == Stretch::Identity
            || value.is_none()
            || self.digit_rows_keep_aspect(rec, extra, cells, cell);
        let mut keys = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let rect = self.key_rect(&c, k)?;
            let (stretched, _) =
                track::stretch(c.stretch, rect, cell).ok_or_else(|| range(self))?;
            if native.is_none() && (stretched != rect || !rows_kept) {
                self.drop_el(role, at, NoteKind::Stretch(c.stretch_mode))?;
                return Ok(None);
            }
            let p = rect.w.add(sr).ok_or_else(|| range(self))?;
            let shift = match align {
                Align::Right => p.mul(e),
                Align::Left => Some(Q::ZERO),
                Align::Center => p.mul(e).and_then(Q::half),
            }
            .ok_or_else(|| range(self))?;
            let x = rect.x.add(shift).ok_or_else(|| range(self))?;
            keys.push((x, rect, p));
        }
        let space = cell_space(&keys, sr).ok_or_else(|| range(self))?;
        let mut rects = Vec::with_capacity(keys.len());
        for (x, rect, p) in &keys {
            let w = rect.w.round().ok_or_else(|| range(self))?;
            let h = self.flip;
            let top = h
                .sub(rect.y.add(rect.h).ok_or_else(|| range(self))?)
                .and_then(Q::round)
                .ok_or_else(|| range(self))?;
            let bottom = h
                .sub(rect.y)
                .and_then(Q::round)
                .ok_or_else(|| range(self))?;
            let left = if align == Align::Right {
                let last = x
                    .add(n.mul(*p).ok_or_else(|| range(self))?)
                    .and_then(Q::round)
                    .ok_or_else(|| range(self))?;
                let pitch = w.checked_add(space).ok_or_else(|| range(self))?;
                last.checked_sub(
                    i128::from(cells - 1)
                        .checked_mul(pitch)
                        .ok_or_else(|| range(self))?,
                )
            } else {
                x.round()
            }
            .ok_or_else(|| range(self))?;
            let v = super::super::geom::coords([left, top, w, bottom - top])
                .ok_or_else(|| range(self))?;
            rects.push(v);
        }
        let track = self.model_track(&c, &rects)?;
        if value.is_some() {
            let quads = u32::try_from(cells).unwrap_or(u32::MAX);
            self.budget
                .widget_quads(quads)
                .map_err(|l| self.limit(at, l))?;
        }
        let offsets = if value.is_some() {
            self.digit_offsets(at, rec, extra, cells)?
        } else {
            Default::default()
        };
        let digits = DigitsDef {
            image,
            set,
            cells: u8::try_from(cells).unwrap_or(u8::MAX),
            align,
            pad,
            space: i32::try_from(space).map_err(|_| range(self))?,
            offsets,
        };
        Ok(Some(DigitsStyle {
            c,
            digits,
            track,
            native,
            program,
        }))
    }
}

/// A number record's glyph strip ([`Cx::digits_style`]).
pub(in super::super) struct DigitsStyle {
    pub(in super::super) c: Common,
    pub(in super::super) digits: DigitsDef,
    pub(in super::super) track: Track,
    pub(in super::super) native: Option<Native>,
    /// The value, compiled when one was given.
    pub(in super::super) program: Option<Program>,
}
