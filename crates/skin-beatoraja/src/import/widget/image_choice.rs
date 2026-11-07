//! Image selection: an image group (`image` with `len` above 1) and an
//! image set, each lowered to a model image choice table chosen by an
//! integer selector (`JsonSkinObjectLoader.java:49-64, 74-91` at
//! `0.8.9`).
//!
//! A group cuts its source's row-major frames into `len` runs of
//! `⌊N / len⌋` frames, the remainder never shown; an image set's entries
//! are image records, each its own frames, cycle and timer, a missing one
//! a hole. Selection follows the source (`SkinImage.java:123-139`): a
//! negative value hides, a value past the table selects entry 0, and a
//! hole hides.
//!
//! A selecting image that SUDDEN+ may paint lowers to a play-critical
//! cover paint showing its table ([`Lowered::Paint`]).

use std::collections::HashMap;

use skin_model::{
    CoverImage, CycleDef, ImageChoiceDef, ImageChoicePart, ImageDef, ImageSpan, PastTable, Program,
    ScreenKind, Source, Track, Ty,
};

use super::super::asset::{ImageKey, NotAdded};
use super::super::policy::{CriticalPart, Role};
use super::super::property::{self, IndexProp, IntProp, Value};
use super::super::report::{NoteKind, PropertyKind, Span, TrackWhy};
use super::super::{Common, Cx, Dst, Prop};
#[cfg(test)]
use super::super::{Event, trace};
use super::{SrcImage, Unplaced};
use crate::error::ImportError;

/// An image set's selector as the source resolves it: its `value` through
/// the value-property factory, else its `ref` as an image-index property.
pub(in super::super) enum SetSelector {
    /// A selector the model runs.
    Expr { src: String, deviates: bool },
    /// Neither resolves: entry 0.
    Null,
    /// A property the model has no value for.
    Unsupported { why: NoteKind },
}

/// The selector source of value `v`: hidden (−1) while it is unknown.
fn value_selector(v: Value) -> String {
    guarded(v.expr(), v.availability())
}

/// Selector `src`, choosing no entry (-1) while `guard` is false.
pub(in super::super) fn guarded(src: &str, guard: Option<&str>) -> String {
    match guard {
        Some(valid) => format!("if {valid} then {src} else -1"),
        None => src.to_owned(),
    }
}

/// Whether selecting by `v` differs from the source in an owner-decided
/// way (D4): a deviating value, or EX score, which the source reads as
/// `Integer.MIN_VALUE` before its score data exists
/// (`IntegerPropertyFactory.java:606-615`).
fn value_deviates(v: Value) -> bool {
    v.deviates() || v == Value::ExScore
}

/// Image set `value` and `ref`'s selector: an explicit value first, its
/// null falling back to `ref` in the image-index namespace; a value the
/// model cannot show does not fall back.
pub(in super::super) fn set_selector(kind: ScreenKind, value: &Prop, r: i32) -> SetSelector {
    match property::value_prop(kind, value) {
        IntProp::Shown(v) => SetSelector::Expr {
            src: value_selector(v),
            deviates: value_deviates(v),
        },
        IntProp::Unsupported(id) => SetSelector::Unsupported {
            why: NoteKind::UnsupportedProperty {
                kind: PropertyKind::Integer,
                id,
            },
        },
        IntProp::Unavailable(id) => SetSelector::Unsupported {
            why: NoteKind::Unavailable {
                kind: PropertyKind::Integer,
                id,
            },
        },
        IntProp::Null => match property::index_in(kind, r) {
            IndexProp::Null => SetSelector::Null,
            IndexProp::Unsupported(id) => SetSelector::Unsupported {
                why: index_unsupported(id),
            },
            IndexProp::Unavailable(id) => SetSelector::Unsupported {
                why: index_unavailable(id),
            },
            p => SetSelector::Expr {
                src: p.selector().unwrap_or("0").to_owned(),
                deviates: p.deviates(),
            },
        },
    }
}

/// The note of an image-index property bridged only with the native
/// settings (class E of the selection profile).
pub(in super::super) fn index_unavailable(id: i32) -> NoteKind {
    NoteKind::Unavailable {
        kind: PropertyKind::ImageIndex,
        id,
    }
}

/// The note of an unsupported image-index property.
pub(in super::super) fn index_unsupported(id: i32) -> NoteKind {
    NoteKind::UnsupportedProperty {
        kind: PropertyKind::ImageIndex,
        id: Some(id),
    }
}

/// What a selecting element lowers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) enum Lowered {
    /// A decorative image choice element.
    Image,
    /// SUDDEN+'s paint showing the choice: play-critical.
    Paint,
}

impl Lowered {
    /// The role its element takes, failure presentation when `failure`
    /// (owner decision 16): SUDDEN+'s paint keeps its own.
    pub(in super::super) fn role(self, failure: bool) -> Role {
        match self {
            Self::Image => Role::Decorative.or_failure(failure),
            Self::Paint => Role::Critical(CriticalPart::Cover),
        }
    }
}

/// One present entry before its track: its image and cell, its frames
/// and its cycle.
struct Entry {
    image: u32,
    cell: [u32; 2],
    first: u16,
    frames: u16,
    cycle: Option<CycleDef>,
}

/// The cache key of record `rec`'s image for an image choice of `c`.
fn choice_key(c: &Common, rec: u32, s: &SrcImage) -> ImageKey {
    (
        rec,
        c.filter == skin_model::Filter::Linear,
        s.div,
        true,
        s.timing,
    )
}

/// A choice's image the sheet would not take, as a refusal no hole stands
/// for.
fn not_added(e: NotAdded) -> Unplaced {
    let why = match e {
        NotAdded::CellTooLarge { w, h } => NoteKind::CellTooLarge { w, h },
        NotAdded::Image(_) => NoteKind::MissingAsset,
    };
    Unplaced {
        why,
        missing: false,
    }
}

impl Cx<'_> {
    /// Selector `src` compiled once per import and charged for each
    /// destination that keeps a copy.
    fn selector(&mut self, src: &str, at: Span) -> Result<Program, ImportError> {
        if let Some(p) = self.selectors.get(src) {
            // Charged before the copy is made.
            if let Err(l) = self.budget.ops(p) {
                return Err(self.limit(at, l));
            }
            #[cfg(test)]
            trace(Event::Charge);
            return Ok(p.clone());
        }
        let p = self.compile(src, Ty::Int, at)?;
        self.selectors.insert(src.to_owned(), p.clone());
        Ok(p)
    }

    /// Charges and places source `s` for an image choice,
    /// without the sheet-level cycle its entries replace, and checks the
    /// sheet would take it, adding nothing: its image and cell.
    fn place_choice(
        &mut self,
        c: &Common,
        s: &SrcImage,
    ) -> Result<Result<(ImageDef, [u32; 2]), Unplaced>, ImportError> {
        let at = c.span;
        let frames = self.grid_frames(at, s.div)?;
        self.budget.image(frames).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Charge);
        let (file, rect, cell) = match self.src_rect(c.role, at, s, s.div, true)? {
            Ok(found) => found,
            Err(u) => return Ok(Err(u)),
        };
        let def = ImageDef {
            file,
            rect: Some(rect),
            grid: s.div.map(|g| u16::try_from(g).unwrap_or(u16::MAX)),
            cycle: None,
            filter: c.filter,
        };
        Ok(match self.assets.check_image(&def) {
            Ok(()) => Ok((def, cell)),
            Err(e) => Err(not_added(e)),
        })
    }

    /// Admits record `rec`'s source `s` for an image choice ([`Self::place_choice`]):
    /// its image and cell.
    fn admit_choice(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Result<(u32, [u32; 2]), Unplaced>, ImportError> {
        let key = choice_key(c, rec, s);
        if let Some(found) = self.assets.image(key, None) {
            // A cached cut past its file's edge is noted for this
            // destination too, with no reload or charge.
            if self.overruns.contains(&key) {
                self.overrun_note(c.span);
            }
            return Ok(Ok(found));
        }
        let (def, cell) = match self.place_choice(c, s)? {
            Ok(placed) => placed,
            Err(u) => return Ok(Err(u)),
        };
        if self.cut_overran {
            self.overruns.insert(key);
        }
        Ok(self
            .assets
            .add_image(key, None, def, cell)
            .map_err(not_added))
    }

    /// An image group (`len` above 1) selecting by `src`: `len` runs of
    /// `⌊N / len⌋` of its `N` frames, the remainder never shown; never
    /// drawn when a run would be empty. The cycle is resolved for a run's
    /// frames, so a run of one frame ignores its timer. It is lowered as
    /// `lowered` says, with `role`: its failure role, or the essential part
    /// its index shows.
    #[expect(clippy::too_many_arguments, reason = "the group's parts as read")]
    pub(in super::super) fn image_group(
        &mut self,
        dst: Dst,
        rec: u32,
        s: &SrcImage,
        len: i32,
        selector: (&'static str, bool),
        charged: bool,
        (lowered, role): (Lowered, Role),
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let n = self.grid_frames(at, s.div)?;
        let len = u32::try_from(len).unwrap_or(0);
        let q = n.checked_div(len).unwrap_or(0);
        if q == 0 {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        }
        let refs = usize::try_from(len).unwrap_or(usize::MAX);
        self.budget
            .choice_refs(refs)
            .map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Refs);
        let Some(c) = self.image_common(&dst, charged, role)? else {
            return Ok(());
        };
        let (src, deviates) = selector;
        let value = self.selector(src, at)?;
        let cycle = match self.src_cycle(&c, q, s)? {
            Ok(cycle) => cycle,
            Err(why) => return self.drop_el(c.role, at, why),
        };
        let (image, cell) = match self.admit_choice(&c, rec, s)? {
            Ok(found) => found,
            Err(u) => return self.drop_el(c.role, at, u.why),
        };
        // Runs of at most 1 024 frames, as the sheet admitted them.
        let frames = u16::try_from(q).map_err(|_| self.range(at, "len"))?;
        #[cfg(test)]
        trace(Event::Table);
        let entries = (0..len)
            .map(|i| {
                let first = u16::try_from(i * q).ok()?;
                Some(Entry {
                    image,
                    cell,
                    first,
                    frames,
                    cycle,
                })
            })
            .collect();
        self.push_choices(c, value, entries, deviates, lowered)
    }

    /// An image set selecting by `src`: one entry per item of its
    /// `images` array, in order, repeats kept; a null item, an id no
    /// `image` record defines and a source proven missing are holes. It
    /// is lowered as `lowered` says, with `role`, its value's reads joined. Its images reach the sheet
    /// only once every entry and its stretch are accepted, so a dropped set
    /// adds none: images others use stay, each at its index.
    pub(in super::super) fn image_set_choices(
        &mut self,
        dst: Dst,
        images: Option<u32>,
        src: &str,
        deviates: bool,
        charged: bool,
        (lowered, role): (Lowered, Role),
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let Some(list) = images else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        if !self.def.arena.is_array(list) {
            return Err(self.def.type_error(list, "images", "an array"));
        }
        let n = self.def.arena.children(list).count();
        if n == 0 {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        }
        self.budget.choice_refs(n).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Refs);
        let Some(c) = self.image_common(&dst, charged, role)? else {
            return Ok(());
        };
        let value = self.selector(src, at)?;
        #[cfg(test)]
        trace(Event::Table);
        let items: Vec<u32> = self.def.arena.children(list).collect();
        let mut pending = Vec::with_capacity(items.len());
        let mut placed: HashMap<ImageKey, [u32; 2]> = HashMap::new();
        let mut to_add = Vec::new();
        for item in items {
            let Some(name) = self.def.string(item, "images")? else {
                pending.push(None);
                continue;
            };
            let Some(rec) = self.image_rec(&name)? else {
                self.ignored(self.arena_span(item), NoteKind::UndefinedObject);
                pending.push(None);
                continue;
            };
            let s = self.src_image(rec)?;
            let n = self.grid_frames(at, s.div)?;
            let cycle = match self.src_cycle(&c, n, &s)? {
                Ok(cycle) => cycle,
                Err(why) => return self.drop_el(c.role, at, why),
            };
            let key = choice_key(&c, rec, &s);
            let cell = match (placed.get(&key), self.assets.image(key, None)) {
                (Some(&cell), _) | (None, Some((_, cell))) => {
                    // A cached cut past its file's edge is noted for this
                    // destination too, with no reload or charge.
                    if self.overruns.contains(&key) {
                        self.overrun_note(c.span);
                    }
                    cell
                }
                (None, None) => match self.place_choice(&c, &s)? {
                    Ok((def, cell)) => {
                        if self.cut_overran {
                            self.overruns.insert(key);
                        }
                        placed.insert(key, cell);
                        to_add.push((key, def, cell));
                        cell
                    }
                    Err(Unplaced { why, missing: true }) => {
                        self.ignored(self.arena_span(item), why);
                        pending.push(None);
                        continue;
                    }
                    Err(Unplaced { why, .. }) => return self.drop_el(c.role, at, why),
                },
            };
            let frames = u16::try_from(n).map_err(|_| self.range(at, "images"))?;
            pending.push(Some((key, cell, frames, cycle)));
        }
        if pending.iter().all(Option::is_none) {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        }
        for (_, cell, ..) in pending.iter().flatten() {
            if self.baked_rects(&c, *cell)?.is_none() {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            }
        }
        for (key, def, cell) in to_add {
            if let Err(e) = self.assets.add_image(key, None, def, cell) {
                return self.drop_el(c.role, at, not_added(e).why);
            }
        }
        let entries = pending
            .into_iter()
            .map(|entry| {
                let (key, cell, frames, cycle) = entry?;
                let (image, _) = self.assets.image(key, None)?;
                Some(Entry {
                    image,
                    cell,
                    first: 0,
                    frames,
                    cycle,
                })
            })
            .collect();
        self.push_choices(c, value, entries, deviates, lowered)
    }

    /// Bakes each present entry's stretch against its own cell: the first
    /// entry's track becomes the element's, and an entry whose keys differ
    /// keeps its own, its keys charged before it is built. Stretch 8 and 9
    /// keep each entry's native size instead. A stretch whose branch
    /// changes between keys drops the element. The element is an image
    /// choice, or as `lowered` says SUDDEN+'s paint showing the table.
    fn push_choices(
        &mut self,
        mut c: Common,
        value: Program,
        entries: Vec<Option<Entry>>,
        deviates: bool,
        lowered: Lowered,
    ) -> Result<(), ImportError> {
        let at = c.span;
        let mut element: Option<Vec<[i32; 4]>> = None;
        let mut choices = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(e) = entry else {
                choices.push(None);
                continue;
            };
            let Some((rects, native)) = self.baked_rects(&c, e.cell)? else {
                return self.drop_el(c.role, at, NoteKind::Stretch(c.stretch_mode));
            };
            let own: Option<Track> = match &element {
                None => {
                    element = Some(rects);
                    None
                }
                Some(first) if *first == rects => None,
                Some(_) => {
                    self.budget
                        .keys(rects.len())
                        .map_err(|l| self.limit(at, l))?;
                    Some(self.model_track(&c, &rects)?)
                }
            };
            self.cycle_kept(&c, e.cycle.as_ref());
            choices.push(Some(ImageChoicePart {
                span: ImageSpan {
                    image: e.image,
                    first: e.first,
                    frames: e.frames,
                },
                cycle: e.cycle,
                track: own,
                native,
            }));
        }
        let Some(rects) = element else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        let t = self.model_track(&c, &rects)?;
        c.display_deviation |= deviates;
        let index = u32::try_from(self.widgets.image_choices.len()).unwrap_or(u32::MAX);
        self.widgets.image_choices.push(ImageChoiceDef {
            value,
            choices,
            past_table: PastTable::First,
        });
        match lowered {
            Lowered::Image => self.push(c, Source::ImageChoice(index), t),
            Lowered::Paint => self.push_paint(c, CoverImage::Choice(index), None, t, None),
        }
    }
}
