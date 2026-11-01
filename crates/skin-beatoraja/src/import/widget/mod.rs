//! Elements by object kind: images here, image selection (image groups
//! and image sets), numbers, text and the play objects in their modules.

pub(super) mod gauge;
pub(super) mod image_choice;
pub(super) mod judge;
pub(super) mod lanes;
pub(super) mod number;
pub(super) mod text;

use skin_model::{ImageDef, MAX_SKIN_FRAMES, Source};

use super::asset::{self, Crop, Failed, ImageKey, Path};
use super::budget::ImportLimit;
use super::policy::Role;
use super::property::{self, IndexProp};
use super::report::{NoteKind, Object, Span, TrackWhy};
use super::track::{self, Stretch};
use super::{Common, Cx, Dst, Opts, Prop};
use crate::error::ImportError;
use image_choice::SetSelector;

/// A placed source image: its admitted file, its rect in texels and its
/// cell.
pub(super) type Placed = (u32, [u32; 4], [u32; 2]);

/// Why a source image was not placed: `missing` when its source is proven
/// absent (no source, a missing or unreadable file, a movie), which an
/// image set keeps as a hole, else a stricter refusal.
pub(super) struct Unplaced {
    pub(super) why: NoteKind,
    pub(super) missing: bool,
}

/// The source-image fields an image or a number reads
/// (`JsonSkinObjectLoader.java:601-621` at `0.8.9`).
pub(super) struct SrcImage {
    src: Option<String>,
    rect: [i32; 4],
    /// Columns and rows, a division of 0 or less read as 1.
    pub(super) div: [u32; 2],
    timer: Prop,
    cycle: i32,
}

impl Cx<'_> {
    /// The frames of a `div` grid. A grid of more than the skin's frames
    /// is the aggregate frame limit, refused before anything reads it.
    pub(super) fn grid_frames(&self, at: Span, div: [u32; 2]) -> Result<u32, ImportError> {
        let n = u64::from(div[0]) * u64::from(div[1]);
        u32::try_from(n)
            .ok()
            .filter(|&n| usize::try_from(n).is_ok_and(|n| n <= MAX_SKIN_FRAMES))
            .ok_or_else(|| self.limit(at, ImportLimit::Frames))
    }

    /// The `image` record of id `id`, the first of it: the play objects
    /// look their parts up in the image table alone
    /// (`JsonPlaySkinObjectLoader.java:96-103, 230-232`).
    pub(super) fn image_rec(&mut self, id: &str) -> Result<Option<u32>, ImportError> {
        if self.images.is_none() {
            let recs = super::object::records(&self.tables, "image").to_vec();
            self.images = Some(super::object::index(self.def, &recs, false)?);
        }
        Ok(self.images.as_ref().and_then(|m| m.get(id)).copied())
    }

    /// The `String[]` items of a literal array field, each `null` item
    /// `None`; empty when absent.
    pub(super) fn opt_strings(
        &mut self,
        field: Option<u32>,
        name: &'static str,
    ) -> Result<Vec<Option<String>>, ImportError> {
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if self.def.arena.node(f).kind != crate::tree::Kind::Array {
            return Err(self.def.type_error(f, name, "an array"));
        }
        let items: Vec<u32> = self.def.arena.children(f).collect();
        items
            .into_iter()
            .map(|i| self.def.string(i, name))
            .collect()
    }

    /// Reads record `rec`'s source-image fields.
    pub(super) fn src_image(&mut self, rec: u32) -> Result<SrcImage, ImportError> {
        let [src, x, y, w, h, divx, divy, timer, cycle] = self.def.fields(
            rec,
            ["src", "x", "y", "w", "h", "divx", "divy", "timer", "cycle"],
        );
        let div = |v: i32| u32::try_from(v).ok().filter(|&d| d > 0).unwrap_or(1);
        Ok(SrcImage {
            src: self.opt_string(src, "src")?,
            rect: [
                self.int_or(x, "x", 0)?,
                self.int_or(y, "y", 0)?,
                self.int_or(w, "w", 0)?,
                self.int_or(h, "h", 0)?,
            ],
            div: [
                div(self.int_or(divx, "divx", 1)?),
                div(self.int_or(divy, "divy", 1)?),
            ],
            timer: self.prop(timer, "timer")?,
            cycle: self.int_or(cycle, "cycle", 0)?,
        })
    }

    /// The model image of `s` (record `rec`) cut to its first `grid`
    /// columns and rows, and its cell size: charged, its file admitted,
    /// added. `None` when the element was dropped.
    pub(super) fn add_src_image(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
        grid: [u32; 2],
    ) -> Result<Option<(u32, [u32; 2])>, ImportError> {
        self.add_src_image_cropped(c, rec, s, grid, |_, _| Ok(Ok(None)))
    }

    /// [`Self::add_src_image`] with its frame's rows cut to what `crop`
    /// returns for the uncropped rect `[x, y, w, h]` (texels, y down),
    /// asked once that rect is known; a cropped image is cached apart
    /// from the uncropped one, sharing its file. A crop `crop` refuses
    /// with a reason drops the element as its role says.
    pub(super) fn add_src_image_cropped(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
        grid: [u32; 2],
        crop: impl FnOnce(&Self, [u32; 4]) -> Result<Result<Option<Crop>, NoteKind>, ImportError>,
    ) -> Result<Option<(u32, [u32; 2])>, ImportError> {
        let at = c.span;
        let role = c.role;
        let frames = self.grid_frames(at, grid)?;
        let (cycle, cycle_on_timer) = match self.cycle(at, frames, &s.timer, s.cycle)? {
            Ok(v) => v,
            Err(why) => {
                self.drop_el(role, at, why)?;
                return Ok(None);
            }
        };
        if c.untimed
            && let Role::Critical(_) = role
            && !super::policy::untimed_ok(
                c.track.identical(),
                c.track.keys.first().is_some_and(|k| k.at == 0),
                c.track.end,
                cycle.is_none() || cycle_on_timer,
            )
        {
            self.drop_el(role, at, NoteKind::Timer(super::TimerWhy::Untimed))?;
            return Ok(None);
        }
        let linear = c.filter == skin_model::Filter::Linear;
        let key = (rec, linear, grid, false);
        if let Some((def, cell)) = self.assets.base(key) {
            let crop = match crop(self, def.rect.unwrap_or_default())? {
                Ok(crop) => crop,
                Err(why) => {
                    self.drop_el(role, at, why)?;
                    return Ok(None);
                }
            };
            if let Some(found) = self.assets.image(key, crop) {
                return Ok(Some(found));
            }
            self.budget.image(frames).map_err(|l| self.limit(at, l))?;
            #[cfg(test)]
            super::trace(super::Event::Charge);
            return self.add_image(c, key, crop, def, cell);
        }
        self.budget.image(frames).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        super::trace(super::Event::Charge);
        let (file, rect, cell) = match self.src_rect(role, at, s, grid)? {
            Ok(found) => found,
            Err(Unplaced { why, .. }) => {
                self.drop_el(role, at, why)?;
                return Ok(None);
            }
        };
        let def = ImageDef {
            file,
            rect: Some(rect),
            grid: grid.map(|g| u16::try_from(g).unwrap_or(u16::MAX)),
            cycle,
            filter: c.filter,
        };
        let crop = match crop(self, rect)? {
            Ok(crop) => crop,
            Err(why) => {
                self.drop_el(role, at, why)?;
                return Ok(None);
            }
        };
        self.add_image(c, key, crop, def, cell)
    }

    /// Where `s`'s first `grid` columns and rows lie, its image charged:
    /// its admitted file, its rect in texels and its cell. A file that
    /// fails a check refuses a critical element with its error.
    pub(super) fn src_rect(
        &mut self,
        role: Role,
        at: Span,
        s: &SrcImage,
        grid: [u32; 2],
    ) -> Result<Result<Placed, Unplaced>, ImportError> {
        let missing = |why| Ok(Err(Unplaced { why, missing: true }));
        let refused = |why| {
            Ok(Err(Unplaced {
                why,
                missing: false,
            }))
        };
        let Some(path) = self.source_path(s.src.as_deref())? else {
            return missing(NoteKind::MissingAsset);
        };
        if asset::is_movie(&path) {
            return missing(NoteKind::Unsupported {
                object: Object::Movie,
            });
        }
        let name = match self.choices.substitute(&path) {
            Path::File(p) => asset::join(&self.entry_dir, &p),
            Path::Missing => return missing(NoteKind::MissingAsset),
            Path::Glob => {
                return refused(NoteKind::UnsupportedField {
                    field: "source glob",
                });
            }
        };
        let def = skin_model::FileDef {
            path: name.clone(),
            hi: None,
        };
        let file = match self
            .assets
            .admit(&mut self.budget, def)
            .map_err(|l| self.limit(at, l))?
        {
            Ok(f) => f,
            Err(Failed::File) if matches!(role, Role::Critical(_)) => {
                if let Some(source) = self.assets.take_error(&name) {
                    return Err(ImportError::Asset {
                        at: self.loc(at),
                        source,
                    });
                }
                return missing(NoteKind::MissingAsset);
            }
            Err(_) => return missing(NoteKind::MissingAsset),
        };
        let [fw, fh] = self.assets.file_dims(file).unwrap_or([0, 0]);
        let [x, y, w, h] = s.rect;
        let w = if w == -1 { i64::from(fw) } else { i64::from(w) };
        let h = if h == -1 { i64::from(fh) } else { i64::from(h) };
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
            u32::try_from(x),
            u32::try_from(y),
            u32::try_from(w),
            u32::try_from(h),
        ) else {
            return refused(NoteKind::SourceRect);
        };
        let [dx, dy] = s.div;
        let cell = [w / dx, h / dy];
        let rect = [x, y, cell[0] * grid[0], cell[1] * grid[1]];
        let inside = |at: u32, len: u32, max: u32| at.checked_add(len).is_some_and(|e| e <= max);
        if !inside(x, rect[2], fw) || !inside(y, rect[3], fh) {
            return refused(NoteKind::SourceRect);
        }
        Ok(Ok((file, rect, cell)))
    }

    /// Adds `def` cut to `crop` under `key`: a failure refuses a critical
    /// element and drops any other.
    fn add_image(
        &mut self,
        c: &Common,
        key: ImageKey,
        crop: Option<Crop>,
        def: ImageDef,
        cell: [u32; 2],
    ) -> Result<Option<(u32, [u32; 2])>, ImportError> {
        let (at, role) = (c.span, c.role);
        match self.assets.add_image(key, crop, def, cell) {
            Ok(found) => Ok(Some(found)),
            Err(e) if matches!(role, Role::Critical(_)) => Err(ImportError::Asset {
                at: self.loc(at),
                source: e,
            }),
            Err(_) => {
                self.drop_el(role, at, NoteKind::MissingAsset)?;
                Ok(None)
            }
        }
    }

    /// An image object.
    pub(super) fn image(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let s = self.src_image(rec)?;
        let [len, act, r] = self.def.fields(rec, ["len", "act", "ref"]);
        // A group (`len` above 1) selects its run of frames by its `ref`
        // in the image-index namespace (`JsonSkinObjectLoader.java:49-64`);
        // a `len` of 1 or less is an ordinary image whatever its `ref`.
        let len = self.int_or(len, "len", 0)?;
        let group = len > 1;
        let index = property::index_by_id(self.int_or(r, "ref", 0)?);
        // Listing offset 4, it follows SUDDEN+ when it cannot reach the
        // lanes, and is SUDDEN+ otherwise; its keys are charged once.
        let charged = dst.offsets.contains(&super::OFFSET_LANECOVER);
        if charged && !self.offset4_outside(&dst)? {
            if group {
                let field = match index {
                    IndexProp::Null | IndexProp::Unsupported(_) => "len",
                    _ => "image selection",
                };
                return self.lane_cover_selector(dst, field);
            }
            return self.lane_cover_image(dst, rec, &s);
        }
        if act.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "act" });
        }
        let stretch = Stretch::of(dst.stretch);
        if stretch == Stretch::Trimmed {
            self.drop_el(Role::Decorative, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(());
        }
        if group {
            let Some(src) = index.selector() else {
                let id = match index {
                    IndexProp::Unsupported(id) => id,
                    _ => 0,
                };
                let why = image_choice::index_unsupported(id);
                return self.drop_el(Role::Decorative, at, why);
            };
            let selector = (src, index.deviates());
            return self.image_group(dst, rec, &s, len, selector, charged);
        }
        let o = Opts {
            follow: true,
            charged,
            ..Opts::default()
        };
        let Some(c) = self.common_with(Role::Decorative, &dst, o)? else {
            return Ok(());
        };
        let Some((image, cell)) = self.add_src_image(&c, rec, &s, s.div)? else {
            return Ok(());
        };
        let native = c.stretch.native(cell);
        let mut rects = Vec::with_capacity(c.track.keys.len());
        let mut shared = track::ANY_BRANCH;
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            let (r, b) = if native.is_some() {
                (r, 0b1)
            } else {
                track::stretch(c.stretch, r, cell).ok_or_else(|| self.range(at, "dst"))?
            };
            shared &= b;
            rects.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
        }
        if !track::one_branch(shared) {
            self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode))?;
            return Ok(());
        }
        let t = self.model_track(&c, &rects)?;
        self.push_native(c, Source::Image(image), t, native)?;
        Ok(())
    }

    /// An image set (`JsonSkinObjectLoader.java:74-98`): a table of its
    /// `images`, chosen by its `value` when that resolves as a value
    /// property, else by its `ref` as an image-index property
    /// (`JsonSkinObjectLoader.java:91`), entry 0 when neither does. Its
    /// images' `len`, `ref` and actions are unused; its own action is
    /// ignored.
    pub(super) fn image_set(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let [r, value, images, act] = self.def.fields(rec, ["ref", "value", "images", "act"]);
        let r = self.int_or(r, "ref", 0)?;
        let value = self.prop(value, "value")?;
        let selector = image_choice::set_selector(&value, r);
        // Listing offset 4, it is classified before its selector: one
        // without a selector is SUDDEN+ by its first image, as an image
        // is; a selecting one is refused.
        let charged = dst.offsets.contains(&super::OFFSET_LANECOVER);
        if charged && !self.offset4_outside(&dst)? {
            let field = match &selector {
                SetSelector::Null => return self.lane_cover_set(dst, images),
                SetSelector::Expr { field, .. } | SetSelector::Unsupported { field, .. } => field,
            };
            return self.lane_cover_selector(dst, field);
        }
        let (src, deviates) = match selector {
            SetSelector::Unsupported { why, .. } => {
                return self.drop_el(Role::Decorative, at, why);
            }
            SetSelector::Null => ("0".to_owned(), false),
            SetSelector::Expr { src, deviates, .. } => (src, deviates),
        };
        if act.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "act" });
        }
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(Role::Decorative, at, NoteKind::Stretch(dst.stretch));
        }
        self.image_set_choices(dst, images, &src, deviates, charged)
    }

    /// An image set without a selector that may reach the lanes: SUDDEN+
    /// by its first image, as an image listing offset 4 is.
    fn lane_cover_set(&mut self, dst: Dst, images: Option<u32>) -> Result<(), ImportError> {
        let at = dst.span;
        let first = match images {
            Some(list) if self.def.arena.node(list).kind == crate::tree::Kind::Array => {
                self.def.arena.children(list).next()
            }
            _ => None,
        };
        let Some(first) = first else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        let Some(name) = self.def.string(first, "images")? else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        let Some(image) = self.image_rec(&name)? else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        let s = self.src_image(image)?;
        self.lane_cover_image(dst, image, &s)
    }
}
