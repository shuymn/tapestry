//! Numbers: the effective property, the glyph set and its cut, and
//! the strip's layout from each key's exact geometry.

use skin_model::{Align, DigitsDef, GlyphSet, MAX_COORD, NumberDef, Pad, Source, Ty};

use super::super::geom::{HEIGHT, Q};
use super::super::policy::{CriticalPart, Role};
use super::super::property::{self, IntProp, Value};
use super::super::report::{DeviationId, NoteKind, PropertyKind, Severity};
use super::super::track::{self, Stretch};
use super::super::{Cx, Dst, Prop};
use crate::error::ImportError;

/// The most cells the model draws.
const CELLS: i32 = skin_model::MAX_CELLS as i32;

impl Cx<'_> {
    /// The effective integer property (`JsonSkinObjectLoader.java:
    /// 104-160`): `value` when it resolves, else `ref`.
    fn int_prop(&mut self, value: Prop, r: i32) -> IntProp {
        let resolved = match value {
            Prop::Id(n) => property::int_by_id(n),
            Prop::Name(s) => property::int_by_name(&s).unwrap_or(IntProp::Unsupported(None)),
            Prop::Null => IntProp::Null,
        };
        match resolved {
            IntProp::Null => property::int_by_id(r),
            p => p,
        }
    }

    /// Whether a value's `offset` array moves any digit.
    fn digit_offsets(&mut self, field: Option<u32>) -> Result<bool, ImportError> {
        let Some(list) = field else {
            return Ok(false);
        };
        if self.def.arena.node(list).kind != crate::tree::Kind::Array {
            return Ok(false);
        }
        let items: Vec<u32> = self.def.arena.children(list).collect();
        for item in items {
            for f in self.def.fields(item, ["x", "y", "w", "h"]) {
                if self.int_or(f, "offset", 0)? != 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// A number object.
    pub(in super::super) fn number(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let [value, r, digit, padding, zeropadding, space, align, offset] = self.def.fields(
            rec,
            [
                "value",
                "ref",
                "digit",
                "padding",
                "zeropadding",
                "space",
                "align",
                "offset",
            ],
        );
        let value = self.prop(value, "value")?;
        let r = self.int_or(r, "ref", 0)?;
        let shown = match self.int_prop(value, r) {
            IntProp::Null => {
                self.ignored(at, NoteKind::Ignored { field: "value" });
                return Ok(());
            }
            IntProp::Unsupported(id) => {
                let why = NoteKind::UnsupportedProperty {
                    kind: PropertyKind::Integer,
                    id,
                };
                self.drop_el(Role::Decorative, at, why)?;
                return Ok(());
            }
            IntProp::Shown(v) => v,
        };
        let role = match shown {
            Value::ExScore => Role::Critical(CriticalPart::ExScore),
            Value::MaxCombo => Role::Critical(CriticalPart::MaxCombo),
            _ => Role::Decorative,
        };
        debug_assert_eq!(shown.critical(), role != Role::Decorative);
        if self.digit_offsets(offset)? {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "offset" })?;
            return Ok(());
        }
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
            return Ok(());
        }
        if digit == 0 {
            self.ignored(at, NoteKind::Ignored { field: "digit" });
            return Ok(());
        }
        let s = self.src_image(rec)?;
        let frames = self.grid_frames(at, s.div)?;
        let (set, pad, d) = if frames.is_multiple_of(24) {
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
                    return Ok(());
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
            return Ok(());
        }
        let keep = sets * d;
        let [dx, dy] = s.div;
        let grid = if keep == frames {
            s.div
        } else if dy == 1 {
            [keep, 1]
        } else if keep.is_multiple_of(dx) {
            [dx, keep / dx]
        } else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "divx" })?;
            return Ok(());
        };
        let (cells, extra) = if digit <= CELLS {
            (digit, 0)
        } else if pad == Pad::Blank {
            (CELLS, digit - CELLS)
        } else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(());
        };
        if set == GlyphSet::TwentyFour && pad != Pad::Blank && cells < 2 {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(());
        }
        let stretch = Stretch::of(dst.stretch);
        if stretch == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(());
        }
        let Some(c) = self.common(role, &dst, false)? else {
            return Ok(());
        };
        let program = self.compile(shown.expr(), Ty::Int, at)?;
        let Some((image, cell)) = self.add_src_image(&c, rec, &s, grid)? else {
            return Ok(());
        };

        // Layout, from each key's offset geometry.
        let range = |cx: &Self| cx.range(at, "dst");
        let sr = Q::int(spaces).mul(self.r).ok_or_else(|| range(self))?;
        let n = Q::int(cells - 1);
        let e = Q::int(extra);
        let mut keys = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let rect = self.key_rect(&c, k)?;
            let (stretched, _) =
                track::stretch(c.stretch, rect, cell).ok_or_else(|| range(self))?;
            if stretched != rect {
                self.drop_el(role, at, NoteKind::Stretch(c.stretch_mode))?;
                return Ok(());
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
        let same = keys
            .windows(2)
            .all(|w| matches!(w, [(_, a, pa), (_, b, pb)] if a.w == b.w && pa == pb));
        let space = match (same, keys.first()) {
            (true, Some((_, r0, p0))) => p0
                .round()
                .zip(r0.w.round())
                .and_then(|(p, w)| p.checked_sub(w)),
            _ => sr.round(),
        }
        .ok_or_else(|| range(self))?;
        if !(-i128::from(MAX_COORD)..=i128::from(MAX_COORD)).contains(&space) {
            return Err(range(self));
        }
        let mut rects = Vec::with_capacity(keys.len());
        for (x, rect, p) in &keys {
            let w = rect.w.round().ok_or_else(|| range(self))?;
            let h = Q::int(HEIGHT);
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
        let t = self.model_track(&c, &rects)?;
        let quads = u32::try_from(cells).unwrap_or(u32::MAX);
        self.budget
            .widget_quads(quads)
            .map_err(|l| self.limit(at, l))?;
        if shown.deviates() {
            self.note(
                Some(at),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D4),
            );
        }
        let index = u32::try_from(self.widgets.numbers.len()).unwrap_or(u32::MAX);
        self.widgets.numbers.push(NumberDef {
            digits: DigitsDef {
                image,
                set,
                cells: u8::try_from(cells).unwrap_or(u8::MAX),
                align,
                pad,
                space: i32::try_from(space).map_err(|_| range(self))?,
            },
            value: program,
        });
        self.push(c, Source::Number(index), t);
        Ok(())
    }
}
