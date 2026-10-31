//! Elements by object kind: images and image sets here, numbers, text
//! and the play objects in their modules.

pub(super) mod gauge;
pub(super) mod judge;
pub(super) mod lanes;
pub(super) mod number;
pub(super) mod text;

use skin_model::{ImageDef, MAX_SKIN_FRAMES, Source};

use super::asset::{self, Failed, Path};
use super::budget::ImportLimit;
use super::policy::Role;
use super::report::{NoteKind, Object, PropertyKind, Span, TrackWhy};
use super::track::{self, Stretch};
use super::{Common, Cx, Dst, Prop};
use crate::error::ImportError;

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

    /// The `String[]` items of a literal array field; empty when absent.
    pub(super) fn strings(
        &mut self,
        field: Option<u32>,
        name: &'static str,
    ) -> Result<Vec<String>, ImportError> {
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if self.def.arena.node(f).kind != crate::tree::Kind::Array {
            return Err(self.def.type_error(f, name, "an array"));
        }
        let items: Vec<u32> = self.def.arena.children(f).collect();
        items
            .into_iter()
            .map(|i| Ok(self.def.string(i, name)?.unwrap_or_default()))
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
        let key = (rec, linear, grid);
        if let Some(found) = self.assets.image(key) {
            return Ok(Some(found));
        }
        self.budget.image(frames).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        super::trace(super::Event::Charge);
        let Some(path) = self.source_path(s.src.as_deref())? else {
            self.drop_el(role, at, NoteKind::MissingAsset)?;
            return Ok(None);
        };
        if asset::is_movie(&path) {
            self.drop_el(
                role,
                at,
                NoteKind::Unsupported {
                    object: Object::Movie,
                },
            )?;
            return Ok(None);
        }
        let name = match self.choices.substitute(&path) {
            Path::File(p) => asset::join(&self.entry_dir, &p),
            Path::Missing => {
                self.drop_el(role, at, NoteKind::MissingAsset)?;
                return Ok(None);
            }
            Path::Glob => {
                self.drop_el(
                    role,
                    at,
                    NoteKind::UnsupportedField {
                        field: "source glob",
                    },
                )?;
                return Ok(None);
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
                self.drop_el(role, at, NoteKind::MissingAsset)?;
                return Ok(None);
            }
            Err(_) => {
                self.drop_el(role, at, NoteKind::MissingAsset)?;
                return Ok(None);
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
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "src rect" })?;
            return Ok(None);
        };
        let [dx, dy] = s.div;
        let cell = [w / dx, h / dy];
        let rect = [x, y, cell[0] * grid[0], cell[1] * grid[1]];
        let def = ImageDef {
            file,
            rect: Some(rect),
            grid: grid.map(|g| u16::try_from(g).unwrap_or(u16::MAX)),
            cycle,
            filter: c.filter,
        };
        match self.assets.add_image(key, def, cell) {
            Ok(i) => Ok(Some((i, cell))),
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
    pub(super) fn image(
        &mut self,
        dst: Dst,
        rec: u32,
        set: Option<u32>,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let s = self.src_image(rec)?;
        let [len, act] = self.def.fields(rec, ["len", "act"]);
        // A selector group (`len` above 1) is the image object's own; an
        // image set draws the image's frames whatever its `len`
        // (`JsonSkinObjectLoader.java:76-84`).
        let group = set.is_none() && self.int_or(len, "len", 0)? > 1;
        if dst.offsets.contains(&super::OFFSET_LANECOVER) {
            return self.lane_cover_image(dst, rec, &s, group);
        }
        if group {
            self.drop_el(
                Role::Decorative,
                at,
                NoteKind::UnsupportedField { field: "len" },
            )?;
            return Ok(());
        }
        // An image set's click action is its own, the image's unused.
        let act = match set {
            Some(set) => {
                let [act] = self.def.fields(set, ["act"]);
                act
            }
            None => act,
        };
        if act.is_some() {
            self.ignored(at, NoteKind::Ignored { field: "act" });
        }
        let stretch = Stretch::of(dst.stretch);
        if stretch == Stretch::Trimmed {
            self.drop_el(Role::Decorative, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(());
        }
        let Some(c) = self.common(Role::Decorative, &dst, false)? else {
            return Ok(());
        };
        let Some((image, cell)) = self.add_src_image(&c, rec, &s, s.div)? else {
            return Ok(());
        };
        let mut rects = Vec::with_capacity(c.track.keys.len());
        let mut branches = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            let (r, b) = track::stretch(c.stretch, r, cell).ok_or_else(|| self.range(at, "dst"))?;
            branches.push(b);
            rects.push(r.model().ok_or_else(|| self.range(at, "dst"))?);
        }
        if !track::one_branch(branches) {
            self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode))?;
            return Ok(());
        }
        let t = self.model_track(&c, &rects)?;
        self.push(c, Source::Image(image), t);
        Ok(())
    }

    /// An image set (`JsonSkinObjectLoader.java:74-98`): only one without
    /// a frame expression, drawing its first image.
    pub(super) fn image_set(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let [r, value, images] = self.def.fields(rec, ["ref", "value", "images"]);
        let selected = self.int_or(r, "ref", 0)? != 0 || self.prop(value, "value")? != Prop::Null;
        if selected {
            let why = NoteKind::UnsupportedProperty {
                kind: PropertyKind::Integer,
                id: None,
            };
            self.drop_el(Role::Decorative, at, why)?;
            return Ok(());
        }
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
        self.image(dst, image, Some(rec))
    }
}
