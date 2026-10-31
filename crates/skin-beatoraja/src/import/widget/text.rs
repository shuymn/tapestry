//! Text: the effective string, the font, the box and the
//! deviations D5, D6, D12 and D13. All text is decorative.

use skin_model::{
    Align, FontRef, MAX_FALLBACKS, MAX_TEXT_BYTES, Overflow, Source, TextDef, TextField, TextValue,
};

use super::super::asset::{self, FntFailure, Font};
use super::super::budget::ImportLimit;
use super::super::geom::{Q, coords};
use super::super::policy::Role;
use super::super::property::{self, StrProp};
use super::super::report::{DeviationId, NoteKind, PropertyKind, Severity, Span};
use super::super::{Cx, Dst, Prop};
use crate::error::ImportError;
use crate::tree::{Kind, Scalar};

/// A text's font as the model draws it.
enum TextFont {
    /// A model font, the em the destination's height.
    Model(FontRef),
    /// A font at the source's size, `size · r` reference pixels, with the
    /// destination's top kept (`JsonSkinObjectLoader.java:652` at
    /// `0.8.9`).
    Fixed(FontRef, Q),
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
            // A non-positive size cannot be drawn: the source's
            // `setScale` throws at 0 (gdx 1.9.9 `setScale`, jar lines
            // 868-869) and mirrors the text below it.
            if size <= 0 {
                self.ignored(at, NoteKind::Ignored { field: "size" });
                return Ok(());
            }
            let em = Q::int(size)
                .mul(self.r)
                .ok_or_else(|| self.range(at, "size"))?;
            let [kind] = self.def.fields(font_rec, ["type"]);
            if matches!(self.int_or(kind, "type", 0)?, 1 | 2) {
                // D13: a distance-field font is drawn with the default font.
                self.note(
                    Some(at),
                    Severity::Deviation,
                    NoteKind::Deviation(DeviationId::D13),
                );
                TextFont::Fixed(FontRef::Default, em)
            } else {
                let Some(font) = self.bitmap_font(role, at, font_rec, &path)? else {
                    return Ok(());
                };
                // D12: the size is rounded to a whole reference pixel.
                if em.round().map(Q::int) != Some(em) {
                    self.note(
                        Some(at),
                        Severity::Deviation,
                        NoteKind::Deviation(DeviationId::D12),
                    );
                }
                TextFont::Fixed(FontRef::Bitmap(font), em)
            }
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
        let font = match tf {
            TextFont::Model(f) | TextFont::Fixed(f, _) => f,
        };
        // A bitmap line pushes a quad per character at most.
        let quads = match (font, &text) {
            (FontRef::Bitmap(_), TextValue::Literal(s)) => s.chars().count(),
            (FontRef::Bitmap(_), TextValue::Field(_)) => MAX_TEXT_BYTES,
            _ => 1,
        };
        self.budget
            .text(u32::try_from(quads).unwrap_or(u32::MAX))
            .map_err(|l| self.limit(at, l))?;
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

    /// The bitmap font of `.fnt` font record `rec` with primary `path`:
    /// made once per record, its files admitted once per name. A fallback
    /// that fails is skipped with a note; a primary that fails ignores
    /// the element when the source also draws nothing, and drops it
    /// otherwise. `None` when the element went.
    fn bitmap_font(
        &mut self,
        role: Role,
        at: Span,
        rec: u32,
        path: &str,
    ) -> Result<Option<u32>, ImportError> {
        let outcome = match self.assets.bitmap_font(rec) {
            Some(o) => o,
            None => {
                let names = self.fallbacks(rec, at)?;
                let primary = asset::join(&self.entry_dir, path);
                let primary = self
                    .assets
                    .fnt_file(&mut self.budget, &primary)
                    .map_err(|l| self.limit(at, l))?;
                let mut fallbacks = Vec::with_capacity(names.len());
                if primary.is_ok() {
                    for name in &names {
                        match self
                            .assets
                            .fnt_file(&mut self.budget, name)
                            .map_err(|l| self.limit(at, l))?
                        {
                            Ok(f) => fallbacks.push(f),
                            Err(failure) => {
                                let why = failure_note(failure);
                                self.note(Some(at), Severity::Ignored, why);
                            }
                        }
                    }
                }
                let files = primary.map(|p| (p, fallbacks.as_slice()));
                self.assets
                    .add_bitmap_font(rec, files)
                    .map_err(|l| self.limit(at, l))?
            }
        };
        match outcome {
            Ok(font) => Ok(Some(font)),
            Err(FntFailure::Fnt(e)) if e.kind.upstream_fails() => {
                self.ignored(at, NoteKind::BitmapFont(e));
                Ok(None)
            }
            Err(failure) => {
                self.drop_el(role, at, failure_note(failure))?;
                Ok(None)
            }
        }
    }

    /// The fallback files of font record `rec`, joined to the
    /// definition's folder: the `path` of each `fallback[]` entry that has
    /// one (`JsonSkinObjectLoader.java:637-649`). The count is checked
    /// before anything is kept.
    fn fallbacks(&mut self, rec: u32, at: Span) -> Result<Vec<String>, ImportError> {
        let [field] = self.def.fields(rec, ["fallback"]);
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if self.def.arena.node(f).kind != Kind::Array {
            return Err(self.def.type_error(f, "fallback", "an array"));
        }
        let n = self.def.arena.children(f).count();
        if n > MAX_FALLBACKS {
            return Err(self.limit(at, ImportLimit::Fonts));
        }
        let items: Vec<u32> = self.def.arena.children(f).collect();
        let mut names = Vec::with_capacity(n);
        for item in items {
            if !matches!(self.def.arena.scalar(item), Scalar::Object) {
                continue;
            }
            let [path] = self.def.fields(item, ["path"]);
            if let Some(p) = self.opt_string(path, "path")? {
                names.push(asset::join(&self.entry_dir, &p));
            }
        }
        Ok(names)
    }
}

/// The note for a `.fnt` file that failed.
fn failure_note(failure: FntFailure) -> NoteKind {
    match failure {
        FntFailure::Missing => NoteKind::MissingAsset,
        FntFailure::Fnt(e) => NoteKind::BitmapFont(e),
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
