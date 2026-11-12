//! Elements by object kind: images here, image selection (image groups
//! and image sets), numbers, text and the play objects in their modules.

pub(super) mod bar;
pub(super) mod bpm_graph;
pub(super) mod cover_paint;
pub(super) mod digit_offsets;
pub(super) mod gauge;
pub(super) mod image_choice;
pub(super) mod judge;
pub(super) mod judge_graph;
pub(super) mod lanes;
pub(super) mod note_parts;
pub(super) mod number;
pub(super) mod result_graphs;
pub(super) mod system;
pub(super) mod text;
pub(super) mod visualizer;

use skin_model::{CycleDef, ImageDef, MAX_SKIN_FRAMES, Native, Source, Track};

use super::asset::{Axis, Crop, Failed, ImageKey, NotAdded, Path};
use super::budget::ImportLimit;
use super::geom::Q;
use super::policy::{self, Role};
use super::prepare::Resolved;
use super::property::{self, IndexProp};
use super::report::{NoteKind, Object, Span};
use super::track::{self, Stretch, Trim, TrimMode};
use super::{Common, Cx, Dst, Opts, Prop};
use crate::error::ImportError;
use crate::lua::residual::Conv;
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
    pub(super) timer: Prop,
    pub(super) cycle: i32,
    /// The records its timer and its cycle come from: its own, or a song
    /// list placement's set's ([`ImageKey`]).
    pub(super) timing: [u32; 2],
}

impl SrcImage {
    /// The `source` id its `src` names.
    pub(super) fn src_name(&self) -> Option<&str> {
        self.src.as_deref()
    }
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

    /// Whether `c`, critical and without a timer, is refused (owner
    /// decision 7, D3): unless only its presentation phase varies
    /// ([`policy::untimed_ok`]) or it settles ([`policy::settles`]). An
    /// admitted one whose presentation phase depends on the start is noted
    /// once it is retained (`Cx::untimed_phase`).
    fn untimed_refused(&self, c: &Common) -> bool {
        c.untimed
            && matches!(c.role, Role::Critical(_))
            && !(policy::untimed_ok(&c.track) || policy::settles(&c.track))
    }

    /// Records that `c` keeps an image cycling on `cycle`, for its D3
    /// note once `c` is retained, when the cycle has no timer: only an
    /// untimed cycle runs on `screen`.
    pub(super) fn cycle_kept(&mut self, c: &Common, cycle: Option<&CycleDef>) {
        if cycle.is_some_and(|y| y.timer == self.screen()) {
            self.cycled.insert(c.span);
        }
    }

    /// The cycle of `s` over `frames` for `c`, resolved before its sheet
    /// is admitted, or why `c` is dropped: its timer or cycle refused, or
    /// `c` critical and untimed where it moves or changes a colour
    /// channel other than alpha, unless it settles (D3).
    pub(super) fn src_cycle(
        &mut self,
        c: &Common,
        frames: u32,
        s: &SrcImage,
    ) -> Result<Result<Option<CycleDef>, NoteKind>, ImportError> {
        let cycle = match self.cycle((c.span, c.role), frames, &s.timer, s.cycle)? {
            Ok(v) => v,
            Err(why) => return Ok(Err(why)),
        };
        if self.untimed_refused(c) {
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
        if !self.def.arena.is_array(f) {
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
            timer: self.callback_prop(timer, "timer")?,
            cycle: self.int_or(cycle, "cycle", 0)?,
            timing: [rec, rec],
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

    /// [`Self::add_src_image`] with its sheet's columns or rows cut to
    /// what `crop` returns for the uncropped rect `[x, y, w, h]` (texels,
    /// y down, the source's size), asked once that rect is known; a
    /// cropped image is cached apart from the uncropped one, sharing its
    /// file. A crop `crop` refuses with a reason, or any crop along an
    /// axis of a frame longer than one texel cut to the file's last
    /// column or row, drops the element as its role says.
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
        let key = (rec, linear, grid, false, s.timing);
        if let Some((def, cell)) = self.assets.base(key) {
            let rect = def.rect.unwrap_or_default();
            let Some(crop) = self.frame_crop(c, rect, cell, grid, crop)? else {
                return Ok(None);
            };
            if let Some(found) = self.assets.image(key, crop) {
                self.cycle_kept(c, cycle.as_ref());
                return Ok(Some(found));
            }
            self.budget.image(frames).map_err(|l| self.limit(at, l))?;
            #[cfg(test)]
            super::trace(super::Event::Charge);
            let found = self.add_image(c, key, crop, def, cell)?;
            if found.is_some() {
                self.cycle_kept(c, cycle.as_ref());
            }
            return Ok(found);
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
        let Some(crop) = self.frame_crop(c, rect, cell, grid, crop)? else {
            return Ok(None);
        };
        let found = self.add_image(c, key, crop, def, cell)?;
        if found.is_some() {
            self.cycle_kept(c, cycle.as_ref());
        }
        Ok(found)
    }

    /// What `crop` returns for model rect `rect` of `grid` frames of
    /// `cell`, asked with the frame the source cuts ([`nominal`]); `None`
    /// once `c` is dropped as its role says, for a crop `crop` refuses or
    /// any crop along an axis of a frame longer than one texel cut to the
    /// file's last column or row.
    fn frame_crop(
        &mut self,
        c: &Common,
        rect: [u32; 4],
        cell: [u32; 2],
        grid: [u32; 2],
        crop: impl FnOnce(&Self, [u32; 4]) -> Result<Result<Option<Crop>, NoteKind>, ImportError>,
    ) -> Result<Option<Option<Crop>>, ImportError> {
        let (frame, cut) = nominal(rect, cell, grid);
        let why = match crop(self, frame)? {
            Ok(Some(c)) if cut[c.axis.index()] => NoteKind::SourceRect,
            Ok(crop) => return Ok(Some(crop)),
            Err(why) => why,
        };
        self.drop_el(c.role, c.span, why)?;
        Ok(None)
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
        let r = self.src_rect_admitted(role, at, s, grid, holes)?;
        #[cfg(feature = "dev-tools")]
        if let Some(src) = s.src.as_deref() {
            use crate::lua::replay::SourceFact;
            let fact = match &r {
                Ok(_) => Some(SourceFact::Loaded),
                Err(u) => match u.why {
                    NoteKind::UndefinedSource => None,
                    NoteKind::MissingAsset if u.missing => Some(SourceFact::Missing),
                    NoteKind::Unsupported {
                        object: Object::Movie,
                    } => Some(SourceFact::Movie),
                    NoteKind::SourceRect | NoteKind::OverCap { .. } => Some(SourceFact::Loaded),
                    _ => Some(SourceFact::Unknown),
                },
            };
            if let Some(f) = fact {
                crate::lua::replay::record_source(src, f);
            }
        }
        Ok(r)
    }

    /// [`Self::src_rect`]'s work.
    fn src_rect_admitted(
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
        let Some(source) = self.source_rec(s.src.as_deref())? else {
            return missing(NoteKind::UndefinedSource);
        };
        let Some(path) = self.source_path(source)? else {
            return missing(NoteKind::MissingAsset);
        };
        // The file choice substituted, then the name classified, as the
        // installation pass reads it ([`Self::resolve_path`]).
        let name = match self.resolve_path(&path, at)? {
            Resolved::File(name) => name,
            Resolved::Movie(_) => {
                return missing(NoteKind::Unsupported {
                    object: Object::Movie,
                });
            }
            Resolved::Absent if !matches!(self.choices.substitute(&path), Path::Glob) => {
                return missing(NoteKind::MissingAsset);
            }
            Resolved::Absent | Resolved::Matched => {
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
            // A play-critical image's failure is its own error; an
            // essential part's is refused as that part, with its note.
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
            // Only this importer refuses it: no hole stands for it. A file
            // past a per-file cap is noted with that cap.
            Err(Failed::File { refused: true } | Failed::Name) => {
                let why = self
                    .assets
                    .failed_cap(&name)
                    .map_or(NoteKind::MissingAsset, |cap| NoteKind::OverCap { cap });
                return refused(why);
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
        let (Some([x, w]), Some([y, h])) = (
            cut_axis(x, cell[0], grid[0], fw),
            cut_axis(y, cell[1], grid[1], fh),
        ) else {
            return refused(NoteKind::SourceRect);
        };
        Ok(Ok((file, [x, y, w, h], cell)))
    }

    /// Adds `def` cut to `crop` under `key`: a failure refuses a critical
    /// element and drops any other, a cell no atlas page holds with its own
    /// note.
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
            Err(NotAdded::CellTooLarge { w, h }) => {
                self.drop_el(role, at, NoteKind::CellTooLarge { w, h })?;
                Ok(None)
            }
            Err(NotAdded::Image(e)) if matches!(role, Role::Critical(_)) => {
                Err(ImportError::Asset {
                    at: self.loc(at),
                    source: e,
                })
            }
            Err(NotAdded::Image(_)) => {
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

    /// [`image_common`](Self::image_common) of an image or image choice,
    /// which turns with the turntable (offset 1); lowered as an element of
    /// its own, it may multiply (blend 4), and as SUDDEN+'s paint showing
    /// a table it may not, as no cover paint does.
    pub(super) fn turning_image_common(
        &mut self,
        dst: &Dst,
        charged: bool,
        (lowered, role): (Lowered, Role),
    ) -> Result<Option<Common>, ImportError> {
        let o = Opts {
            follow: true,
            turntable: true,
            multiply: lowered == Lowered::Image,
            charged,
            ..Opts::default()
        };
        self.common_with(role, dst, o)
    }

    /// Whether an image or image set's destination lists offset 4, its
    /// keys then charged once, and what it lowers to: SUDDEN+'s paint
    /// unless it cannot reach the lanes, where it follows SUDDEN+.
    fn lowered(&mut self, dst: &Dst) -> Result<(bool, Lowered), ImportError> {
        // Only a play skin has SUDDEN+.
        if self.kind != skin_model::ScreenKind::Play {
            return Ok((false, Lowered::Image));
        }
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
        let r = self.int_or(r, "ref", 0)?;
        let index = property::index_in(self.kind, r);
        let (charged, lowered) = self.lowered(&dst)?;
        let mut role = lowered.role(dst.failure);
        if group && self.kind == skin_model::ScreenKind::Result {
            role = super::result::index_role(role, r);
        }
        self.actions(at, act)?;
        if lowered == Lowered::Paint && !group {
            return self.cover_paint_image(dst, rec, &s, role, None);
        }
        // Trimmed stretches 3, 5 and 7 map on a plain image
        // ([`Self::trimmed_image`]); 10 and a group's stay unsupported.
        if Stretch::of(dst.stretch) == Stretch::Trimmed
            && (group || TrimMode::of(dst.stretch).is_none())
        {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        if group {
            let Some(src) = index.selector() else {
                let why = match index {
                    IndexProp::Unavailable(id) => image_choice::index_unavailable(id),
                    IndexProp::Unsupported(id) => image_choice::index_unsupported(id),
                    _ => image_choice::index_unsupported(0),
                };
                return self.drop_el(role, at, why);
            };
            let selector = (src, index.deviates());
            // Its callbacks run only while it selects a run
            // (`SkinImage.java:127-132`).
            self.gate_by_value(src, None, Conv::Index);
            self.prepare =
                super::prepare::Prepare::Selector((!index.non_negative()).then(|| src.to_owned()));
            return self.image_group(dst, rec, &s, len, selector, charged, (lowered, role));
        }
        // A plain image lowered as a paint left above.
        let Some(c) = self.turning_image_common(&dst, charged, (Lowered::Image, role))? else {
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
        if let Some(mode) = TrimMode::of(c.stretch_mode) {
            return self.trimmed_image(c, rec, s, mode);
        }
        let Some((image, cell)) = self.add_src_image(c, rec, s, s.div)? else {
            return Ok(None);
        };
        self.baked_image(c, image, cell)
    }

    /// [`Self::src_image_rects`] under trimmed stretch `mode`
    /// (`skin/StretchType.java:47-80, 131-153`), decided per key on exact
    /// rationals ([`track::trim`]): when every key fits, each key's rect
    /// shrinks and recentres to its scaled frame; when every key crops
    /// one axis keeping the same texels, the sheet is cut about its middle
    /// on that axis once ([`track::crop_middle`]) and the keys stay. The
    /// importer crops only a sheet of one cell along the cropped axis,
    /// whose frames the model's grid then splits each cropped alike; keys
    /// mixing the branches, cropping different amounts (cropping keys
    /// changing aspect) or cropping across several cells drop `c` with its
    /// stretch's note, as a crop leaving nothing drops it with `dst`'s.
    fn trimmed_image(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
        mode: TrimMode,
    ) -> Result<Option<Painted>, ImportError> {
        let (at, grid) = (c.span, s.div);
        let stretch = NoteKind::Stretch(c.stretch_mode);
        let mut walked = Vec::new();
        let found = self.add_src_image_cropped(c, rec, s, grid, |cx, [x, y, w, h]| {
            let cell = [w / grid[0].max(1), h / grid[1].max(1)];
            // Each key's rect as it draws, and the branch every key took so
            // far: `None` before the first key, then the crop or `None`
            // for fitting.
            let mut rects = Vec::with_capacity(c.track.keys.len());
            let mut branch: Option<Option<(Axis, Q)>> = None;
            for k in &c.track.keys {
                let r = cx.key_rect(c, k)?;
                let (taken, r) =
                    match track::trim(mode, r, cell).ok_or_else(|| cx.range(at, "dst"))? {
                        Trim::Fit(fitted) => (None, fitted),
                        Trim::Crop { axis, kept } => (Some((axis, kept)), r),
                    };
                if branch.is_some_and(|b| b != taken) {
                    return Ok(Err(stretch));
                }
                branch = Some(taken);
                rects.push(r);
            }
            walked = rects;
            let Some((axis, kept)) = branch.flatten() else {
                return Ok(Ok(None));
            };
            if grid[axis.index()] != 1 {
                return Ok(Err(stretch));
            }
            let (from, len) = match axis {
                Axis::Cols => (x, w),
                Axis::Rows => (y, h),
            };
            let Some([at, len]) = track::crop_middle(from, len, kept) else {
                return Ok(Err(NoteKind::UnsupportedField { field: "dst" }));
            };
            Ok(Ok(Some(Crop { axis, at, len })))
        })?;
        let Some((image, _)) = found else {
            return Ok(None);
        };
        let rects = walked
            .into_iter()
            .map(|r| r.model_from(self.flip).ok_or_else(|| self.range(at, "dst")))
            .collect::<Result<_, _>>()?;
        Ok(Some((image, rects, None)))
    }

    /// Admitted `image` of `cell` with `c`'s keys baked against it
    /// ([`Self::baked_rects`]); `None` once `c` is dropped, as a stretch
    /// changing branch between keys drops it.
    pub(super) fn baked_image(
        &mut self,
        c: &Common,
        image: u32,
        cell: [u32; 2],
    ) -> Result<Option<Painted>, ImportError> {
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
            rects.push(
                r.model_from(self.flip)
                    .ok_or_else(|| self.range(at, "dst"))?,
            );
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
        let value = self.callback_prop(value, "value")?;
        let selector = image_choice::set_selector(self.kind, &value, r);
        let (charged, lowered) = self.lowered(&dst)?;
        let mut role = lowered.role(dst.failure);
        // On a result skin a set choosing by the clear type is the clear
        // status, an essential part.
        if self.kind == skin_model::ScreenKind::Result {
            role = match property::value_prop(self.kind, &value) {
                property::IntProp::Shown(v) => {
                    super::result::set_role(v.essential().map_or(role, Role::Essential), role)
                }
                // A function's value overrides `ref`, so `ref` says nothing.
                property::IntProp::Null if !matches!(value, Prop::Callable(_)) => {
                    super::result::index_role(role, r)
                }
                _ => role,
            };
        }
        // A value property the source reads as a pre-judgment sentinel; a
        // selector by `ref` alone that is never negative needs no gate.
        let non_negative = match property::value_prop(self.kind, &value) {
            property::IntProp::Shown(v) => {
                self.read_sentinel(v.pre_judgment());
                false
            }
            property::IntProp::Null => {
                !matches!(value, Prop::Callable(_))
                    && property::index_in(self.kind, r).non_negative()
            }
            _ => false,
        };
        let (src, deviates, guard) = if let Prop::Callable(id) = value {
            // A function's value selects, its element's role joined by the
            // part its reads show; it never falls back to `ref`.
            role = self.callback_role(id, "value", role)?;
            let Some(v) = self.value_callback(at, id, role, Conv::Index)? else {
                return Ok(());
            };
            role = super::result::set_role(v.role, role);
            let guarded = v.guard.is_some();
            (
                image_choice::guarded(&v.expr, v.guard.as_deref()),
                v.deviates,
                guarded,
            )
        } else {
            match selector {
                SetSelector::Unsupported { why, .. } => {
                    return self.drop_el(role, at, why);
                }
                SetSelector::Null => ("0".to_owned(), false, false),
                SetSelector::Expr {
                    src,
                    deviates,
                    guard,
                } => (src, deviates, guard.is_some()),
            }
        };
        // A selector gated by an availability the source does not have
        // skips the set's callbacks where the source runs them, whatever
        // backs it.
        self.install.availability |= guard;
        self.gate_by_value(&src, None, Conv::Index);
        self.prepare = super::prepare::Prepare::Selector((!non_negative).then(|| src.clone()));
        self.actions(at, act)?;
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        self.image_set_choices(dst, images, &src, deviates, charged, (lowered, role))
    }

    /// An image's or an image set's `act`: no action has a hit target. On
    /// play it is ignored; on selection its id is noted by its class
    /// ([`super::selection::profile::event`]), an E action as unavailable
    /// until the native actions exist and any other as ignored.
    fn actions(&mut self, at: Span, act: Option<u32>) -> Result<(), ImportError> {
        let Some(f) = act else {
            return Ok(());
        };
        let event = if self.kind == skin_model::ScreenKind::Selection {
            match self.callback_prop(Some(f), "act")? {
                Prop::Id(id) => super::selection::profile::event(id)
                    .filter(|c| *c == super::selection::profile::Class::Unavailable)
                    .map(|_| id),
                _ => None,
            }
        } else {
            None
        };
        let why = match event {
            Some(id) => NoteKind::Unavailable {
                kind: super::PropertyKind::Event,
                id,
            },
            None => NoteKind::Ignored { field: "act" },
        };
        self.ignored(at, why);
        Ok(())
    }
}

/// One axis of a source cut: `grid` frames of `cell` texels from `at` in
/// a file `extent` texels long, as its model rect's offset and length. The
/// source cuts it unchecked from a texture clamped to its edge
/// (`JsonSkinObjectLoader.java:601-621`, libGDX 1.9.9 `GLTexture`
/// defaults), so its texels past the far edge repeat that edge's last
/// texel line:
/// - a cut starting inside the file and running past it keeps its whole
///   length, and the sheet pads each cell from the edge
///   ([`skin_model::ImageDef::rect`]), whatever its frames;
/// - a single frame lying wholly at or past the edge samples only that
///   line, which is cut in its place, one texel long, rather than padded
///   to its length.
///
/// `None` for several frames starting at or past the edge, and for a cut
/// whose frame coordinates leave the source's `int` sums, where its
/// coordinates wrap and it draws no edge line.
fn cut_axis(at: u32, cell: u32, grid: u32, extent: u32) -> Option<[u32; 2]> {
    let len = cell.checked_mul(grid)?;
    let end = at.checked_add(len)?;
    i32::try_from(end).ok()?;
    if end <= extent || at < extent {
        return Some([at, len]);
    }
    if cell == 0 || extent == 0 || grid != 1 {
        return None;
    }
    Some([extent - 1, 1])
}

/// The rect of `grid` frames of `cell` that model rect `rect` stands
/// for (`rect`'s corner, the source's size), and whether its columns and
/// its rows were cut to the file's last line ([`cut_axis`]), seen for a
/// frame longer than one texel: a one-texel frame cut there is that line
/// either way.
fn nominal(rect: [u32; 4], cell: [u32; 2], grid: [u32; 2]) -> ([u32; 4], [bool; 2]) {
    let w = cell[0].saturating_mul(grid[0]);
    let h = cell[1].saturating_mul(grid[1]);
    ([rect[0], rect[1], w, h], [rect[2] != w, rect[3] != h])
}
