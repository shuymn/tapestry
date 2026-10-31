//! Text: the effective string, the font, the box and the
//! fallbacks of D5–D7. All text is decorative.

use skin_model::{Align, FontRef, MAX_TEXT_BYTES, Overflow, Source, TextDef, TextField, TextValue};

use super::super::asset::{self, Font};
use super::super::geom::{Q, coords};
use super::super::policy::Role;
use super::super::property::{self, StrProp};
use super::super::report::{DeviationId, NoteKind, PropertyKind, Severity};
use super::super::{Cx, Dst, Prop};
use crate::error::ImportError;

/// A text's font as the model draws it.
enum TextFont {
    /// A model font, the em the destination's height.
    Model(FontRef),
    /// A bitmap font drawn with another font at a fixed em, its top kept.
    Fixed(FontRef, Q),
}

/// D7, owner decision 3: a bitmap `.fnt` font is drawn with the default
/// font at the source's size, `size · r` reference pixels, with the
/// destination's top kept (`JsonSkinObjectLoader.java:652`,
/// `SkinTextBitmap.java:86-110` at `0.8.9`). Bitmap-font support replaces
/// this function.
fn bitmap_font(size: i32, r: Q) -> Option<TextFont> {
    Some(TextFont::Fixed(FontRef::Default, Q::int(size).mul(r)?))
}

/// The chart field of string property `id` 10–16.
fn field(id: i32) -> Option<TextField> {
    Some(match id {
        10 => TextField::Title,
        11 => TextField::Subtitle,
        12 => TextField::FullTitle,
        13 => TextField::Genre,
        14 => TextField::Artist,
        15 => TextField::Subartist,
        16 => TextField::FullArtist,
        _ => return None,
    })
}

/// `s` without control characters (C0, DEL, C1), cut at a character
/// boundary to [`MAX_TEXT_BYTES`] (D6).
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT_BYTES));
    for c in s.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

impl Cx<'_> {
    /// A text object.
    pub(in super::super) fn text(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let [
            font,
            size,
            align,
            r,
            value,
            constant,
            editable,
            wrapping,
            overflow,
            outline,
            sx,
            sy,
        ] = self.def.fields(
            rec,
            [
                "font",
                "size",
                "align",
                "ref",
                "value",
                "constantText",
                "editable",
                "wrapping",
                "overflow",
                "outlineWidth",
                "shadowOffsetX",
                "shadowOffsetY",
            ],
        );
        // The effective string (`JsonSkinObjectLoader.java:633-636`,
        // `SkinText.java:102`).
        let resolved = match self.prop(value, "value")? {
            Prop::Id(n) => property::str_by_id(n),
            Prop::Name(s) => property::str_by_name(&s).unwrap_or(StrProp::Unsupported(None)),
            Prop::Null => StrProp::Null,
        };
        let resolved = match resolved {
            StrProp::Null => property::str_by_id(self.int_or(r, "ref", 0)?),
            p => p,
        };
        let text = match resolved {
            StrProp::Field(id) => match field(id) {
                Some(f) => TextValue::Field(f),
                None => {
                    self.ignored(at, NoteKind::Ignored { field: "value" });
                    return Ok(());
                }
            },
            StrProp::Unsupported(id) => {
                let why = NoteKind::UnsupportedProperty {
                    kind: PropertyKind::String,
                    id,
                };
                self.drop_el(role, at, why)?;
                return Ok(());
            }
            StrProp::Null => {
                let literal = self
                    .opt_string(constant, "constantText")?
                    .unwrap_or_default();
                let cleaned = clean(&literal);
                if cleaned.is_empty() {
                    self.ignored(
                        at,
                        NoteKind::Ignored {
                            field: "constantText",
                        },
                    );
                    return Ok(());
                }
                if cleaned != literal {
                    self.note(
                        Some(at),
                        Severity::Deviation,
                        NoteKind::Deviation(DeviationId::D6),
                    );
                }
                TextValue::Literal(cleaned)
            }
        };
        let (shift, align) = match self.int_or(align, "align", 0)? {
            0 => (Q::ZERO, Align::Left),
            1 => (Q::new(1, 2).unwrap_or(Q::ZERO), Align::Center),
            2 => (Q::int(1), Align::Right),
            _ => {
                self.drop_el(role, at, NoteKind::UnsupportedField { field: "align" })?;
                return Ok(());
            }
        };
        let overflow = match self.int_or(overflow, "overflow", 0)? {
            0 => Overflow::Spill,
            1 => Overflow::Shrink,
            2 => {
                self.note(
                    Some(at),
                    Severity::Deviation,
                    NoteKind::Deviation(DeviationId::D6),
                );
                Overflow::Shrink
            }
            _ => {
                self.drop_el(role, at, NoteKind::UnsupportedField { field: "overflow" })?;
                return Ok(());
            }
        };
        let wraps = wrapping.map_or(Ok(false), |f| self.def.bool(f, "wrapping"))?;
        let mut styled = wraps;
        for (f, name) in [
            (outline, "outlineWidth"),
            (sx, "shadowOffsetX"),
            (sy, "shadowOffsetY"),
        ] {
            if let Some(f) = f {
                let v = self.def.string(f, name)?.unwrap_or_default();
                styled |= v.parse::<f64>().is_ok_and(|v| v != 0.0);
            }
        }
        if styled {
            self.note(
                Some(at),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D6),
            );
        }
        if editable.map_or(Ok(false), |f| self.def.bool(f, "editable"))? {
            self.ignored(at, NoteKind::Ignored { field: "editable" });
        }
        let size = self.int_or(size, "size", 0)?;
        let font_name = self.opt_string(font, "font")?;
        if self.fonts.is_none() {
            let recs = super::super::object::records(&self.tables, "font").to_vec();
            self.fonts = Some(super::super::object::index(self.def, &recs, false)?);
        }
        let font_rec = font_name.and_then(|n| self.fonts.as_ref().and_then(|m| m.get(&n)).copied());
        let Some(font_rec) = font_rec else {
            self.ignored(at, NoteKind::Ignored { field: "font" });
            return Ok(());
        };
        let [path] = self.def.fields(font_rec, ["path"]);
        let path = self.opt_string(path, "path")?.unwrap_or_default();

        let Some(mut c) = self.common(role, &dst, true)? else {
            return Ok(());
        };
        if dst.stretch > 0 {
            self.ignored(at, NoteKind::Ignored { field: "stretch" });
        }
        let tf = if path.to_ascii_lowercase().ends_with(".fnt") {
            self.note(
                Some(at),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D7),
            );
            bitmap_font(size, self.r).ok_or_else(|| self.range(at, "size"))?
        } else {
            let name = asset::join(&self.entry_dir, &path);
            match self
                .assets
                .font(&mut self.budget, &name)
                .map_err(|l| self.limit(at, l))?
            {
                Font::File(i) => TextFont::Model(FontRef::File(u32::from(i))),
                Font::Failed => {
                    self.drop_el(role, at, NoteKind::MissingAsset)?;
                    return Ok(());
                }
            }
        };
        let mut rects = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let rect = self.key_rect(&c, k)?;
            // The box's left edge moves by its width times 0, ½ or 1
            // (`SkinTextFont.java:158-176`).
            let x = rect
                .w
                .mul(shift)
                .and_then(|s| rect.x.sub(s))
                .ok_or_else(|| self.range(at, "dst"))?;
            let moved = super::super::geom::Rect { x, ..rect };
            let v = match &tf {
                TextFont::Model(_) => moved.model(),
                TextFont::Fixed(_, em) => moved
                    .edges()
                    .and_then(|[l, t, r, _]| coords([l, t, r.checked_sub(l)?, em.round()?])),
            }
            .ok_or_else(|| self.range(at, "dst"))?;
            // A key the model cannot draw (`TextError::Size`) is its
            // refusal, located at this element.
            rects.push(v);
        }
        let t = self.model_track(&c, &rects)?;
        self.budget.text().map_err(|l| self.limit(at, l))?;
        let font = match tf {
            TextFont::Model(f) | TextFont::Fixed(f, _) => f,
        };
        let index = u32::try_from(self.widgets.texts.len()).unwrap_or(u32::MAX);
        self.widgets.texts.push(TextDef {
            value: text,
            font,
            align,
            overflow,
        });
        // D5: text always blends normally.
        c.blend = skin_model::Blend::Alpha;
        self.push(c, Source::Text(index), t);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_are_cleaned_and_cut_at_a_boundary() {
        assert_eq!(clean("a\u{7}b\u{85}c"), "abc");
        let long = "é".repeat(200);
        let cut = clean(&long);
        assert_eq!(cut.len(), 256);
        assert!(clean(&format!("{}x", "a".repeat(256))).len() == 256);
    }
}
