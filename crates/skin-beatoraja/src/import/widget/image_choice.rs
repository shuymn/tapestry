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

use skin_model::{
    CoverImage, CycleDef, ImageChoiceDef, ImageChoicePart, ImageSpan, Program, Source, Track, Ty,
};

use super::super::policy::{CriticalPart, Role};
use super::super::property::{self, IndexProp, IntProp, Value};
use super::super::report::{NoteKind, PropertyKind, Span, TrackWhy};
use super::super::{Common, Cx, Dst, Opts, Prop};
#[cfg(test)]
use super::super::{Event, trace};
use super::{SrcImage, Unplaced};
use crate::error::ImportError;
use crate::tree::Kind;

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
    match v.availability() {
        Some(valid) => format!("if {valid} then {} else -1", v.expr()),
        None => v.expr().to_owned(),
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
pub(in super::super) fn set_selector(value: &Prop, r: i32) -> SetSelector {
    match property::value_prop(value) {
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
        IntProp::Null => match property::index_by_id(r) {
            IndexProp::Null => SetSelector::Null,
            IndexProp::Unsupported(id) => SetSelector::Unsupported {
                why: index_unsupported(id),
            },
            p => SetSelector::Expr {
                src: p.selector().unwrap_or("0").to_owned(),
                deviates: p.deviates(),
            },
        },
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
    /// The role its element takes.
    pub(in super::super) fn role(self) -> Role {
        match self {
            Self::Image => Role::Decorative,
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

    /// Admits record `rec`'s source `s` for an image choice, without the
    /// sheet-level cycle its entries replace: its image and cell.
    fn admit_choice(
        &mut self,
        c: &Common,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Result<(u32, [u32; 2]), Unplaced>, ImportError> {
        let at = c.span;
        let frames = self.grid_frames(at, s.div)?;
        let key = (rec, c.filter == skin_model::Filter::Linear, s.div, true);
        if let Some(found) = self.assets.image(key, None) {
            return Ok(Ok(found));
        }
        self.budget.image(frames).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        trace(Event::Charge);
        let (file, rect, cell) = match self.src_rect(c.role, at, s, s.div)? {
            Ok(found) => found,
            Err(u) => return Ok(Err(u)),
        };
        let def = skin_model::ImageDef {
            file,
            rect: Some(rect),
            grid: s.div.map(|g| u16::try_from(g).unwrap_or(u16::MAX)),
            cycle: None,
            filter: c.filter,
        };
        Ok(match self.assets.add_image(key, None, def, cell) {
            Ok(found) => Ok(found),
            Err(_) => Err(Unplaced {
                why: NoteKind::MissingAsset,
                missing: false,
            }),
        })
    }

    /// The common part of a selecting element, in the role `lowered`
    /// gives it and following the covers; `charged` when its keys were
    /// charged as an offset-4 candidate.
    fn choice_common(
        &mut self,
        dst: &Dst,
        charged: bool,
        lowered: Lowered,
    ) -> Result<Option<Common>, ImportError> {
        let o = Opts {
            follow: true,
            charged,
            ..Opts::default()
        };
        self.common_with(lowered.role(), dst, o)
    }

    /// An image group (`len` above 1) selecting by `src`: `len` runs of
    /// `⌊N / len⌋` of its `N` frames, the remainder never shown; never
    /// drawn when a run would be empty. The cycle is resolved for a run's
    /// frames, so a run of one frame ignores its timer.
    #[expect(clippy::too_many_arguments, reason = "the group's parts as read")]
    pub(in super::super) fn image_group(
        &mut self,
        dst: Dst,
        rec: u32,
        s: &SrcImage,
        len: i32,
        selector: (&'static str, bool),
        charged: bool,
        lowered: Lowered,
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
        let Some(c) = self.choice_common(&dst, charged, lowered)? else {
            return Ok(());
        };
        let (src, deviates) = selector;
        let value = self.selector(src, at)?;
        let cycle = match self.cycle(at, q, &s.timer, s.cycle)? {
            Ok((cycle, _)) => cycle,
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
    /// `image` record defines and a source proven missing are holes.
    pub(in super::super) fn image_set_choices(
        &mut self,
        dst: Dst,
        images: Option<u32>,
        src: &str,
        deviates: bool,
        charged: bool,
        lowered: Lowered,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let Some(list) = images else {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        };
        if self.def.arena.node(list).kind != Kind::Array {
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
        let Some(c) = self.choice_common(&dst, charged, lowered)? else {
            return Ok(());
        };
        let value = self.selector(src, at)?;
        #[cfg(test)]
        trace(Event::Table);
        let items: Vec<u32> = self.def.arena.children(list).collect();
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            let Some(name) = self.def.string(item, "images")? else {
                entries.push(None);
                continue;
            };
            let Some(rec) = self.image_rec(&name)? else {
                self.ignored(self.arena_span(item), NoteKind::UndefinedObject);
                entries.push(None);
                continue;
            };
            let s = self.src_image(rec)?;
            let n = self.grid_frames(at, s.div)?;
            let cycle = match self.cycle(at, n, &s.timer, s.cycle)? {
                Ok((cycle, _)) => cycle,
                Err(why) => return self.drop_el(c.role, at, why),
            };
            match self.admit_choice(&c, rec, &s)? {
                Ok((image, cell)) => entries.push(Some(Entry {
                    image,
                    cell,
                    first: 0,
                    frames: u16::try_from(n).map_err(|_| self.range(at, "images"))?,
                    cycle,
                })),
                Err(Unplaced { why, missing: true }) => {
                    self.ignored(self.arena_span(item), why);
                    entries.push(None);
                }
                Err(Unplaced { why, .. }) => return self.drop_el(c.role, at, why),
            }
        }
        if entries.iter().all(Option::is_none) {
            self.ignored(at, NoteKind::Track(TrackWhy::NeverDrawn));
            return Ok(());
        }
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
        self.widgets
            .image_choices
            .push(ImageChoiceDef { value, choices });
        match lowered {
            Lowered::Image => self.push(c, Source::ImageChoice(index), t),
            Lowered::Paint => self.push_paint(c, CoverImage::Choice(index), None, t, None),
        }
    }
}
