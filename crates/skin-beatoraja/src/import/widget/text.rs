//! Text: the effective string, the font, the box and the
//! deviations D5, D6, D12 and D13. Text is decorative unless it is
//! failure presentation (owner decision 16).

use skin_model::{
    Align, FontRef, MAX_FALLBACKS, MAX_TEXT_BYTES, Overflow, Source, TextDef, TextField, TextValue,
    Track,
};

use super::super::asset::{self, FntFailure, Font};
use super::super::budget::ImportLimit;
use super::super::callback::TextFold;
use super::super::geom::{Q, coords};
use super::super::policy::Role;
use super::super::property::{self, StrProp};
use super::super::report::{DeviationId, NoteKind, PropertyKind, Severity, Span};
use super::super::{Common, Cx, Dst, Opts, Prop};
use crate::error::ImportError;
use crate::tree::Scalar;

/// A text record's style ([`Cx::text_style`]): its element's common
/// part, its box track, font, alignment and overflow.
pub(in super::super) struct TextStyle {
    pub(in super::super) c: Common,
    pub(in super::super) track: Track,
    pub(in super::super) font: FontRef,
    pub(in super::super) align: Align,
    pub(in super::super) overflow: Overflow,
}

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
    /// `literal` cleaned for a text at `at`, a D6 note when that changed it;
    /// `None` when nothing is left.
    fn cleaned(&mut self, at: Span, literal: &str) -> Option<String> {
        let cleaned = clean(literal);
        if cleaned.is_empty() {
            return None;
        }
        if cleaned != literal {
            self.note(
                Some(at),
                Severity::Deviation,
                NoteKind::Deviation(DeviationId::D6),
            );
        }
        Some(cleaned)
    }

    /// What `fold` shows, for a text at `at`; `None` for nothing.
    fn folded_text(&mut self, at: Span, fold: TextFold) -> Option<TextValue> {
        match fold {
            TextFold::Literal(s) => self.cleaned(at, &s).map(TextValue::Literal),
            TextFold::Field(id) => field(id).map(TextValue::Field),
            TextFold::FieldOr(id, s) => {
                let field = field(id)?;
                Some(match self.cleaned(at, &s) {
                    Some(empty) => TextValue::FieldOr { field, empty },
                    None => TextValue::Field(field),
                })
            }
        }
    }

    /// A text object.
    pub(in super::super) fn text(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative.or_failure(dst.failure);
        let [r, value, constant] = self.def.fields(rec, ["ref", "value", "constantText"]);
        // The effective string (`JsonSkinObjectLoader.java:633-636`,
        // `SkinText.java:102`). A function folds or follows the role, and
        // it never falls back to `ref`.
        let text = 'text: {
            let resolved = match self.callback_prop(value, "value")? {
                Prop::Id(n) => property::str_in(self.kind, n),
                Prop::Name(s) => {
                    property::str_by_name_in(self.kind, &s).unwrap_or(StrProp::Unsupported(None))
                }
                Prop::Callable(id) => {
                    let fold = match self.text_callback(role, id)? {
                        Ok(fold) => fold,
                        Err(why) => {
                            self.drop_el(role, at, why)?;
                            return Ok(());
                        }
                    };
                    // An empty result draws nothing (`SkinText.java:106-107`).
                    let Some(value) = self.folded_text(at, fold) else {
                        self.ignored(at, NoteKind::Ignored { field: "value" });
                        return Ok(());
                    };
                    break 'text value;
                }
                Prop::Null => StrProp::Null,
            };
            let resolved = match resolved {
                StrProp::Null => property::str_in(self.kind, self.int_or(r, "ref", 0)?),
                p => p,
            };
            match resolved {
                StrProp::Screen(s) => TextValue::Selection(s),
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
                    let Some(cleaned) = self.cleaned(at, &literal) else {
                        self.ignored(
                            at,
                            NoteKind::Ignored {
                                field: "constantText",
                            },
                        );
                        return Ok(());
                    };
                    TextValue::Literal(cleaned)
                }
            }
        };
        let Some(TextStyle {
            mut c,
            track: t,
            font,
            align,
            overflow,
        }) = self.text_style(role, &dst, rec)?
        else {
            return Ok(());
        };
        // A bitmap line pushes a quad per character at most.
        let quads = match (font, &text) {
            (FontRef::Bitmap(_), TextValue::Literal(s)) => s.chars().count(),
            (
                FontRef::Bitmap(_),
                TextValue::Field(_) | TextValue::FieldOr { .. } | TextValue::Selection(_),
            ) => MAX_TEXT_BYTES,
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
        self.push(c, Source::Text(index), t)?;
        Ok(())
    }

    /// Text record `rec`'s style for destination `dst` of `role`: its
    /// alignment, overflow, font and box track, with the D6, D12 and D13
    /// notes; `None` when the element was dropped or is never drawn. A
    /// text's string is its caller's; a song list's row text style draws
    /// the row's title whatever it names.
    pub(in super::super) fn text_style(
        &mut self,
        role: Role,
        dst: &Dst,
        rec: u32,
    ) -> Result<Option<TextStyle>, ImportError> {
        let at = dst.span;
        let [
            font,
            size,
            align,
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
                "editable",
                "wrapping",
                "overflow",
                "outlineWidth",
                "shadowOffsetX",
                "shadowOffsetY",
            ],
        );
        let (shift, align) = match self.int_or(align, "align", 0)? {
            0 => (Q::ZERO, Align::Left),
            1 => (Q::new(1, 2).unwrap_or(Q::ZERO), Align::Center),
            2 => (Q::int(1), Align::Right),
            _ => {
                self.drop_el(role, at, NoteKind::UnsupportedField { field: "align" })?;
                return Ok(None);
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
                return Ok(None);
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
            return Ok(None);
        };
        let [path] = self.def.fields(font_rec, ["path"]);
        let path = self.opt_string(path, "path")?.unwrap_or_default();

        let o = Opts {
            text: true,
            follow: true,
            ..Opts::default()
        };
        let Some(c) = self.common_with(role, dst, o)? else {
            return Ok(None);
        };
        if dst.stretch > 0 {
            self.ignored(at, NoteKind::Ignored { field: "stretch" });
        }
        let tf = if path.to_ascii_lowercase().ends_with(".fnt") {
            // The source scales the font by the signed size
            // (`JsonSkinObjectLoader.java:652`, `SkinTextBitmap.java:
            // 118-123`): `setScale` throws at 0 (gdx 1.9.9 `setScale`, jar
            // lines 868-869), so nothing is drawn there either, and a
            // negative size mirrors the text, which is not reproduced.
            if size == 0 {
                self.ignored(at, NoteKind::Ignored { field: "size" });
                return Ok(None);
            }
            if size < 0 {
                self.drop_el(role, at, NoteKind::UnsupportedField { field: "size" })?;
                return Ok(None);
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
                    return Ok(None);
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
            let name = asset::resolve(&self.entry_dir, &path);
            match self
                .assets
                .font(&mut self.budget, &name)
                .map_err(|l| self.limit(at, l))?
            {
                Font::File(i) => TextFont::Model(FontRef::File(u32::from(i))),
                Font::Failed => {
                    self.drop_el(role, at, NoteKind::MissingAsset)?;
                    return Ok(None);
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
                TextFont::Model(_) => moved.model_from(self.flip),
                TextFont::Fixed(_, em) => moved
                    .edges_from(self.flip)
                    .and_then(|[l, t, r, _]| coords([l, t, r.checked_sub(l)?, em.round()?])),
            }
            .ok_or_else(|| self.range(at, "dst"))?;
            // A key the model cannot draw (`TextError::Size`) is its
            // refusal, located at this element.
            rects.push(v);
        }
        let track = self.model_track(&c, &rects)?;
        let font = match tf {
            TextFont::Model(f) | TextFont::Fixed(f, _) => f,
        };
        Ok(Some(TextStyle {
            c,
            track,
            font,
            align,
            overflow,
        }))
    }

    /// The bitmap font of `.fnt` font record `rec` with primary `path`:
    /// made once per record, its files admitted once per name. A fallback
    /// that fails is skipped with a note, ignored when the source also
    /// fails to load it and dropped otherwise; such a loss, kept with the
    /// font, refuses a critical text whether it made the font or uses it
    /// later. A primary that fails ignores the element when the source
    /// also draws nothing, and drops it otherwise. `None` when the element
    /// went.
    fn bitmap_font(
        &mut self,
        role: Role,
        at: Span,
        rec: u32,
        path: &str,
    ) -> Result<Option<u32>, ImportError> {
        let outcome = match self.assets.bitmap_font(rec) {
            Some((o, lost)) => {
                // A fallback the source draws that this importer lost when
                // the font was made refuses a critical text.
                if let Some(failure) = lost.filter(|_| role.refuses()) {
                    self.drop_el(role, at, failure_note(failure))?;
                }
                o
            }
            None => {
                let names = self.fallbacks(rec, at)?;
                let primary = asset::resolve(&self.entry_dir, path);
                let def = &self.def;
                let primary = self
                    .assets
                    .fnt_file(&mut self.budget, &primary, &|| def.is_cancelled())
                    .map_err(|s| self.stopped(at, s))?;
                let mut fallbacks = Vec::with_capacity(names.len());
                let mut lost = None;
                if primary.is_ok() {
                    for name in &names {
                        let def = &self.def;
                        match self
                            .assets
                            .fnt_file(&mut self.budget, name, &|| def.is_cancelled())
                            .map_err(|s| self.stopped(at, s))?
                        {
                            Ok(f) => fallbacks.push(f),
                            Err(failure) => {
                                // Where the source draws the fallback, its
                                // omission is ours: a critical text is
                                // refused and any other dropped, and the
                                // loss stays with the font for its later
                                // users; otherwise it is ignored.
                                let ours = match failure {
                                    FntFailure::Fnt(e) => !e.kind.upstream_fails(),
                                    FntFailure::Refused => true,
                                    FntFailure::Missing => false,
                                };
                                let why = failure_note(failure);
                                if ours {
                                    lost = lost.or(Some(failure));
                                    self.drop_el(role, at, why)?;
                                } else {
                                    self.ignored(at, why);
                                }
                            }
                        }
                    }
                }
                let files = primary.map(|p| (p, fallbacks.as_slice()));
                self.assets
                    .add_bitmap_font(rec, files, lost)
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

    /// The fallback files of font record `rec`, resolved from the
    /// definition's folder ([`asset::resolve`]): the `path` of each
    /// `fallback[]` entry that has one (`JsonSkinObjectLoader.java:
    /// 637-649`). The count is checked before anything is kept.
    fn fallbacks(&mut self, rec: u32, at: Span) -> Result<Vec<String>, ImportError> {
        let [field] = self.def.fields(rec, ["fallback"]);
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if !self.def.arena.is_array(f) {
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
                names.push(asset::resolve(&self.entry_dir, &p));
            }
        }
        Ok(names)
    }
}

/// The note for a `.fnt` file that failed.
fn failure_note(failure: FntFailure) -> NoteKind {
    match failure {
        FntFailure::Missing | FntFailure::Refused => NoteKind::MissingAsset,
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
