//! Elements by object kind: images here, image selection (image groups
//! and image sets), numbers, text and the play objects in their modules.

pub(super) mod bar;
pub(super) mod cover_paint;
pub(super) mod gauge;
pub(super) mod image_choice;
pub(super) mod judge;
pub(super) mod lanes;
pub(super) mod number;
pub(super) mod text;

use skin_model::{CycleDef, ImageDef, MAX_SKIN_FRAMES, Native, Source, Track};

use super::asset::{self, Crop, Failed, ImageKey, Path};
use super::budget::ImportLimit;
use super::policy::Role;
use super::property::{self, IndexProp};
use super::report::{NoteKind, Object, Span};
use super::track::{self, Stretch};
use super::{Common, Cx, Dst, Opts, Prop};
use crate::error::ImportError;
use image_choice::{Lowered, SetSelector};

/// A track's keys baked against a cell, and the native size kept instead
/// under stretch 8 and 9.
pub(super) type Baked = (Vec<[i32; 4]>, Option<Native>);

/// An admitted image, its keys' model rects baked against it, and the
/// native size its quads keep.
pub(super) type Painted = (u32, Vec<[i32; 4]>, Option<Native>);

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

/// Whether `c`, critical and untimed, would draw what depends on when
/// the state started (owner decision 7, D3): its track static from 0 and
/// perpetual, and every cycle it shows `cycle_ok` (absent or on a timer).
fn untimed_refused(c: &Common, cycle_ok: bool) -> bool {
    c.untimed
        && matches!(c.role, Role::Critical(_))
        && !super::policy::untimed_ok(
            c.track.identical(),
            c.track.keys.first().is_some_and(|k| k.at == 0),
            c.track.end,
            cycle_ok,
        )
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

    /// The cycle of `s` over `frames` for `c`, resolved before its sheet
    /// is admitted, or why `c` is dropped: its timer or cycle refused, or
    /// `c` critical and untimed where it shows what the state's start
    /// decides (D3).
    pub(super) fn src_cycle(
        &mut self,
        c: &Common,
        frames: u32,
        s: &SrcImage,
    ) -> Result<Result<Option<CycleDef>, NoteKind>, ImportError> {
        let (cycle, on_timer) = match self.cycle(c.span, frames, &s.timer, s.cycle)? {
            Ok(v) => v,
            Err(why) => return Ok(Err(why)),
        };
        if untimed_refused(c, cycle.is_none() || on_timer) {
            return Ok(Err(NoteKind::Timer(super::TimerWhy::Untimed)));
        }
        Ok(Ok(cycle))
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
        let cycle = match self.src_cycle(c, frames, s)? {
            Ok(cycle) => cycle,
            Err(why) => {
                self.drop_el(role, at, why)?;
                return Ok(None);
            }
        };
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
        let (file, rect, cell) = match self.src_rect(role, at, s, grid, false)? {
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
    /// fails a check refuses a critical element with its error, except
    /// that with `holes` a file missing for the source too stays missing,
    /// a hole its table keeps.
    pub(super) fn src_rect(
        &mut self,
        role: Role,
        at: Span,
        s: &SrcImage,
        grid: [u32; 2],
        holes: bool,
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
            Err(Failed::File { refused })
                if matches!(role, Role::Critical(_)) && (refused || !holes) =>
            {
                if let Some(source) = self.assets.take_error(&name) {
                    return Err(ImportError::Asset {
                        at: self.loc(at),
                        source,
                    });
                }
                return missing(NoteKind::MissingAsset);
            }
            Err(Failed::File { refused: false }) => return missing(NoteKind::MissingAsset),
            // Only this importer refuses it: no hole stands for it.
            Err(Failed::File { refused: true } | Failed::Name) => {
                return refused(NoteKind::MissingAsset);
            }
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

    /// The common part of an image, image choice or paint image in
    /// `role`, following the covers; `charged` when its keys were charged
    /// as an offset-4 candidate.
    pub(super) fn image_common(
        &mut self,
        dst: &Dst,
        charged: bool,
        role: Role,
    ) -> Result<Option<Common>, ImportError> {
        let o = Opts {
            follow: true,
            charged,
            ..Opts::default()
        };
        self.common_with(role, dst, o)
    }

    /// Whether an image or image set's destination lists offset 4, its
    /// keys then charged once, and what it lowers to: SUDDEN+'s paint
    /// unless it cannot reach the lanes, where it follows SUDDEN+.
    fn lowered(&mut self, dst: &Dst) -> Result<(bool, Lowered), ImportError> {
        let charged = dst.offsets.contains(&super::OFFSET_LANECOVER);
        let lowered = if charged && !self.offset4_outside(dst)? {
            Lowered::Paint
        } else {
            Lowered::Image
        };
        Ok((charged, lowered))
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
        let (charged, lowered) = self.lowered(&dst)?;
        let role = lowered.role();
        if act.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "act" });
        }
        if lowered == Lowered::Paint && !group {
            return self.cover_paint_image(dst, rec, &s, role, None);
        }
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        if group {
            let Some(src) = index.selector() else {
                let id = match index {
                    IndexProp::Unsupported(id) => id,
                    _ => 0,
                };
                let why = image_choice::index_unsupported(id);
                return self.drop_el(role, at, why);
            };
            let selector = (src, index.deviates());
            return self.image_group(dst, rec, &s, len, selector, charged, lowered);
        }
        let Some(c) = self.image_common(&dst, charged, Role::Decorative)? else {
            return Ok(());
        };
        let Some((image, t, native)) = self.src_image_track(&c, rec, &s)? else {
            return Ok(());
        };
        self.push_native(c, Source::Image(image), t, native)?;
        Ok(())
    }

    /// `rec`'s whole-sheet image admitted for `c`, with `c`'s track baked
    /// against its cell and the native size stretch 8 and 9 keep; `None`
    /// once `c` is dropped, as a stretch changing branch between keys
    /// drops it.
    pub(super) fn src_image_track(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Option<(u32, Track, Option<Native>)>, ImportError> {
        let Some((image, rects, native)) = self.src_image_rects(c, rec, s)? else {
            return Ok(None);
        };
        let t = self.model_track(c, &rects)?;
        Ok(Some((image, t, native)))
    }

    /// [`Self::src_image_track`]'s image and its baked keys, before the
    /// track is built.
    pub(super) fn src_image_rects(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Option<Painted>, ImportError> {
        let Some((image, cell)) = self.add_src_image(c, rec, s, s.div)? else {
            return Ok(None);
        };
        let Some((rects, native)) = self.baked_rects(c, cell)? else {
            self.drop_el(c.role, c.span, NoteKind::Stretch(c.stretch_mode))?;
            return Ok(None);
        };
        Ok(Some((image, rects, native)))
    }

    /// `c`'s keys with its stretch baked against a `cell`, and the native
    /// size stretch 8 and 9 keep instead; `None` when the stretch changes
    /// branch between keys.
    pub(super) fn baked_rects(
        &self,
        c: &Common,
        cell: [u32; 2],
    ) -> Result<Option<Baked>, ImportError> {
        let at = c.span;
        let native = c.stretch.native(cell);
        let mut rects = Vec::with_capacity(c.track.keys.len());
        let mut shared = track::ANY_BRANCH;
        for k in &c.track.keys {
            let r = self.key_rect(c, k)?;
            let (r, b) = if native.is_some() {
                (r, 0b1)
            } else {
                track::stretch(c.stretch, r, cell).ok_or_else(|| self.range(at, "dst"))?
            };
            shared &= b;
            rects.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
        }
        Ok(track::one_branch(shared).then_some((rects, native)))
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
        let (charged, lowered) = self.lowered(&dst)?;
        let role = lowered.role();
        let (src, deviates) = match selector {
            SetSelector::Unsupported { why, .. } => {
                return self.drop_el(role, at, why);
            }
            SetSelector::Null => ("0".to_owned(), false),
            SetSelector::Expr { src, deviates, .. } => (src, deviates),
        };
        if act.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "act" });
        }
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        self.image_set_choices(dst, images, &src, deviates, charged, lowered)
    }
}
