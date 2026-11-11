//! The selection importer (docs/stage3.md §3, Selection; S10-09d): a
//! selection skin's destinations through the selection profile
//! ([`profile`]), its `songlist` as the model's `song list`, its sliders
//! and graphs as `ratio bar`s, and its essential parts.
//!
//! The song list is read once, at the first retained destination naming
//! it; each such destination is its own song list element, charged its
//! own widget quads and its parts' visibilities once per slot, all
//! sharing one definition and so one row text table
//! (`select/SkinBar.java:31`, `JsonSelectSkinObjectLoader.java:30-235`).
//!
//! Its essential parts are judged after the configured options are
//! folded: the song list, the center slot's selected placement and a title
//! style for every listed row kind (chart, folder, "all charts"). A part
//! of them that is missing or cannot be reproduced refuses the skin
//! ([`ImportError::Essential`]); any other part that cannot be reproduced
//! is dropped with a note.

pub(crate) mod profile;

use std::collections::HashMap;

use skin_model::{
    End, ImagePartDef, KeyDef, LEVEL_STYLES, LampKey, LevelDef, MAX_PLACEMENTS, PartDef,
    PlacementDef, Program, RatioBarDef, RatioForm, RatioSource, RowLabel, RowStyle, RowTextDef,
    ScoreRate, ScreenKind, SliderDirection, SongListDef, Source, Track, Ty,
};

use self::profile::Class;
use super::budget::{Budget, ImportLimit};
use super::geom::{HEIGHT, Q};
use super::policy::{EssentialPart, Role};
use super::report::{NoteKind, Object, PropertyKind, Span};
use super::track::Stretch;
use super::widget::number::DigitsStyle;
use super::widget::text::TextStyle;
use super::{Cx, Dst, Opts, Prop};
use crate::error::ImportError;

/// The text styles a row kind takes, first that is given first
/// (`select/BarRenderer.java:180-193`): a chart 2 then 0, a folder 4 then
/// 0, the "all charts" row 9, then 4, then 0 (owner decision O3).
const TITLE_CHAINS: [(RowStyle, &[usize]); RowStyle::COUNT] = [
    (RowStyle::Chart, &[2, 0]),
    (RowStyle::Folder, &[4, 0]),
    (RowStyle::All, &[9, 4, 0]),
];

/// The source's text styles a song list reads (`select/SkinBar.java`).
const TEXT_STYLES: usize = 11;
/// The source's level styles: by difficulty 0 to 6
/// (`select/SkinBar.java:67`); the model keeps the first six.
const SOURCE_LEVELS: usize = 7;
/// The source's lamp styles, by clear type 0 to 10 (`ClearType.java`).
const SOURCE_LAMPS: usize = 11;
/// The source's label styles: long notes, random, mines, and the two long
/// note modes tapestry never judges (`select/BarRenderer.java:374-397`).
const SOURCE_LABELS: usize = 5;

/// The model's lamp of the source's clear type index (docs/stage3.md
/// S10-09d, D7): the assist-option clear (2), PERFECT (9) and MAX (10)
/// are never met.
fn lamp_key(index: usize) -> Option<LampKey> {
    Some(match index {
        0 => LampKey::NoPlay,
        1 => LampKey::Failed,
        3 => LampKey::AssistEasy,
        4 => LampKey::Easy,
        5 => LampKey::Normal,
        6 => LampKey::Hard,
        7 => LampKey::ExHard,
        8 => LampKey::FullCombo,
        _ => return None,
    })
}

/// The model's label of the source's label index.
fn label(index: usize) -> Option<RowLabel> {
    Some(match index {
        0 => RowLabel::LongNotes,
        1 => RowLabel::Random,
        2 => RowLabel::Mines,
        _ => return None,
    })
}

/// What the selection import holds besides the elements.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// The song list, once read.
    pub(crate) list: Option<SongListDef>,
    /// The `songlist` record's place, once read.
    pub(crate) span: Option<Span>,
    /// The song list elements kept.
    pub(crate) lists: u32,
    /// The song list's visibilities each of its elements evaluates: its
    /// arms' once a frame, its styles' once a slot. Each part's own, so
    /// at most two a slot and 20 styles.
    arms: Vec<Program>,
    styles: Vec<Program>,
    pub(crate) ratio_bars: Vec<RatioBarDef>,
    pub(crate) bar_spans: Vec<Span>,
    /// The `imageset`, `text` and `value` tables by id, the first record
    /// of an id winning, each built once when the song list first needs
    /// it.
    tables: [Option<HashMap<String, u32>>; 3],
}

/// The tables of [`State::tables`].
const TABLES: [&str; 3] = ["imageset", "text", "value"];

impl Cx<'_> {
    /// Charges `charge` against the budget at `at`, then does `work`:
    /// when the charge fails, nothing of the work starts.
    pub(super) fn charged<T>(
        &mut self,
        at: Span,
        charge: impl FnOnce(&mut Budget) -> Result<(), ImportLimit>,
        work: impl FnOnce(&mut Self) -> Result<T, ImportError>,
    ) -> Result<T, ImportError> {
        charge(&mut self.budget).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        super::trace(super::Event::Charge);
        work(self)
    }

    /// The selection skin's own tables before its destinations: each
    /// `customEvents` entry, an action with no native contract, is noted.
    pub(super) fn selection_tables(&mut self) -> Result<(), ImportError> {
        let n = super::object::records(&self.tables, "customEvents").len();
        for i in 0..n {
            let Some(&rec) = super::object::records(&self.tables, "customEvents").get(i) else {
                break;
            };
            let at = self.arena_span(rec);
            self.ignored(
                at,
                NoteKind::Ignored {
                    field: "customEvents",
                },
            );
        }
        Ok(())
    }

    /// One destination of a selection skin, naming `object` of record
    /// `rec`.
    pub(super) fn selection_destination(
        &mut self,
        dst: Dst,
        object: Object,
        rec: u32,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        // The source makes no object of these when `src` names no source
        // (`JsonSkinObjectLoader.java:42-72, 394-455`).
        if matches!(object, Object::Image | Object::Slider | Object::Graph) {
            let [src] = self.def.fields(rec, ["src"]);
            let src = self.opt_string(src, "src")?;
            if self.source_rec(src.as_deref())?.is_none() {
                self.ignored(at, NoteKind::UndefinedSource);
                return Ok(());
            }
        }
        let r = match object {
            Object::Image => self.image(dst, rec),
            Object::ImageSet => self.image_set(dst, rec),
            Object::Value => self.number(dst, rec),
            Object::Text => self.text(dst, rec),
            Object::Slider => self.ratio_bar(dst, rec, false),
            Object::Graph => self.ratio_bar(dst, rec, true),
            Object::SongList => self.song_list_element(dst, rec),
            _ => self.drop_el(Role::Decorative, at, NoteKind::Unsupported { object }),
        };
        self.end_value_gate();
        r
    }

    /// A slider (`graph` false) or a graph of a selection skin as a ratio
    /// bar ([`profile::slider`], [`profile::graph`]): a slider moves its
    /// image `range` along its `angle` (`skin/SkinSlider.java:117-119`), a
    /// graph cuts it to its ratio of its width, or with `angle` 1 of its
    /// height from the bottom (`skin/SkinGraph.java:98-106`).
    pub(in crate::import) fn ratio_bar(
        &mut self,
        dst: Dst,
        rec: u32,
        graph: bool,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let [ty, angle, range, value, ref_num, changeable, event] = self.def.fields(
            rec,
            [
                "type",
                "angle",
                "range",
                "value",
                "isRefNum",
                "changeable",
                "event",
            ],
        );
        let ty = self.int_or(ty, "type", 0)?;
        // A graph's direction defaults to vertical, a slider's to up
        // (`JsonSkin.java:214, 236`).
        let angle = self.int_or(angle, "angle", i32::from(graph))?;
        let range = self.int_or(range, "range", 0)?;
        let value = self.callback_prop(value, "value")?;
        let ref_num = match ref_num {
            Some(f) => self.def.bool(f, "isRefNum")?,
            None => false,
        };
        let changeable = match changeable {
            Some(f) => self.def.bool(f, "changeable")?,
            None => true,
        };
        let (kind, class) = if graph && self.kind == ScreenKind::Play {
            (PropertyKind::Graph, play_graph(ty).map(Class::Bridge))
        } else if graph {
            (PropertyKind::Graph, profile::graph(ty))
        } else {
            (PropertyKind::Slider, profile::slider(ty))
        };
        let field = |field| NoteKind::UnsupportedField { field };
        let source = match class {
            _ if value != Prop::Null => return self.drop_el(role, at, field("value")),
            _ if ref_num => return self.drop_el(role, at, field("isRefNum")),
            Some(Class::Bridge(s)) => s,
            Some(Class::Unavailable) => {
                return self.drop_el(role, at, NoteKind::Unavailable { kind, id: ty });
            }
            Some(Class::Omit) | None => {
                let why = NoteKind::UnsupportedProperty { kind, id: Some(ty) };
                return self.drop_el(role, at, why);
            }
        };
        let form = if graph {
            RatioForm::Bar {
                vertical: angle == 1,
            }
        } else {
            let direction = match angle {
                0 => SliderDirection::Up,
                1 => SliderDirection::Right,
                2 => SliderDirection::Down,
                3 => SliderDirection::Left,
                _ => return self.drop_el(role, at, field("angle")),
            };
            if range < 0 {
                return self.drop_el(role, at, field("range"));
            }
            // The range scaled to the reference window along its axis and
            // truncated once, exactly
            // (`JsonSkinObjectLoader.java:398-413`).
            let scaled = if matches!(direction, SliderDirection::Right | SliderDirection::Left) {
                Q::int(range).mul(self.r).and_then(Q::integer)
            } else {
                (HEIGHT * i128::from(range)).checked_div(i128::from(self.height))
            };
            let range = scaled
                .and_then(|r| u32::try_from(r).ok())
                .filter(|&r| r <= skin_model::MAX_COORD.unsigned_abs())
                .ok_or_else(|| self.range(at, "range"))?;
            if event.is_some() {
                self.ignored(at, NoteKind::Ignored { field: "event" });
            }
            RatioForm::Slider {
                direction,
                range,
                changeable,
            }
        };
        if Stretch::of(dst.stretch) != Stretch::Identity {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        }
        let s = self.src_image(rec)?;
        let Some(mut c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(());
        };
        // The progresses follow tapestry's load and end at the last
        // resolved object (D4).
        c.display_deviation |= matches!(
            source,
            RatioSource::LoadProgress | RatioSource::SongProgress
        );
        let Some((image, cell)) = self.add_src_image(&c, rec, &s, s.div)? else {
            return Ok(());
        };
        let Some((rects, _)) = self.baked_rects(&c, cell)? else {
            return self.drop_el(role, at, NoteKind::Stretch(dst.stretch));
        };
        let t = self.model_track(&c, &rects)?;
        let index = u32::try_from(self.sel.ratio_bars.len()).unwrap_or(u32::MAX);
        self.sel.ratio_bars.push(RatioBarDef {
            image,
            form,
            source,
        });
        self.sel.bar_spans.push(at);
        self.push(c, Source::RatioBar(index), t)
    }

    /// Whether graph `rec` of a play skin is a play graph: its `type`
    /// read as [`Self::ratio_bar`] reads it is 101, 102 or one of 110 to
    /// 115.
    pub(in crate::import) fn play_graph(&mut self, rec: u32) -> Result<bool, ImportError> {
        let [ty] = self.def.fields(rec, ["type"]);
        Ok(play_graph(self.int_or(ty, "type", 0)?).is_some())
    }

    /// A destination naming the `songlist`: one song list element, the
    /// song list read at the first ([`Self::song_list`]). Its own track
    /// gives only colour and alpha, so it is read as a play widget's outer
    /// destination is; its visibility and timer apply.
    fn song_list_element(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Essential(EssentialPart::SongList);
        let o = Opts {
            neutral: true,
            ..Opts::default()
        };
        let Some(c) = self.common_with(role, &dst, o)? else {
            return Ok(());
        };
        if self.sel.list.is_none() {
            self.song_list(rec)?;
        }
        let (quads, slots) = match &self.sel.list {
            Some(l) => (worst_quads(l), l.placements.len()),
            None => return Err(self.essential(Some(at), EssentialPart::SongList, None)),
        };
        // Each element draws every slot: its quads, its arms' visibilities
        // and each style's once per slot, as the model charges them.
        let (arms, styles) = (&self.sel.arms, &self.sel.styles);
        let charge = |b: &mut Budget| {
            b.widget_quads(quads)?;
            for p in arms {
                b.ops(p)?;
            }
            for p in styles {
                b.ops_times(p, slots)?;
            }
            Ok(())
        };
        charge(&mut self.budget).map_err(|l| self.limit(at, l))?;
        #[cfg(test)]
        super::trace(super::Event::Charge);
        let rects = vec![[0, 0, 0, 0]; c.track.keys.len()];
        let t = self.model_track(&c, &rects)?;
        self.sel.lists += 1;
        self.push(c, Source::SongList, t)
    }

    /// The essential refusal of `part` at `at`.
    fn essential(
        &self,
        at: Option<Span>,
        part: EssentialPart,
        why: Option<NoteKind>,
    ) -> ImportError {
        ImportError::Essential {
            at: at.map(|s| self.loc(s)),
            part,
            why,
        }
    }

    /// The `songlist` record `rec` as the model's song list: its
    /// placements, center and styles.
    fn song_list(&mut self, rec: u32) -> Result<(), ImportError> {
        let at = self.arena_span(rec);
        self.sel.span = Some(at);
        let [
            center,
            clickable,
            listoff,
            liston,
            text,
            level,
            lamp,
            playerlamp,
            rivallamp,
            trophy,
            labels,
            graph,
        ] = self.def.fields(
            rec,
            [
                "center",
                "clickable",
                "listoff",
                "liston",
                "text",
                "level",
                "lamp",
                "playerlamp",
                "rivallamp",
                "trophy",
                "label",
                "graph",
            ],
        );
        let center = self.int_or(center, "center", 0)?;
        let list = |cx: &Self, f: Option<u32>| {
            f.and_then(|f| cx.tables.nested.get(&f).cloned())
                .unwrap_or_default()
        };
        let on = list(self, liston);
        let off = list(self, listoff);
        // The slots the source draws (`select/SkinBar.java:31`): past them,
        // ignored.
        if on.len() > MAX_PLACEMENTS {
            self.ignored(at, NoteKind::Ignored { field: "liston" });
        }
        let slots = on.len().min(MAX_PLACEMENTS);
        if slots == 0 {
            return Err(self.essential(Some(at), EssentialPart::SongList, None));
        }
        let center = usize::try_from(center)
            .ok()
            .filter(|&c| c < slots)
            .ok_or_else(|| self.essential(Some(at), EssentialPart::SelectedRow, None))?;
        let placements = self.charged(
            at,
            |b| b.placements(slots),
            |cx| {
                let mut placements = Vec::with_capacity(slots);
                for (i, &on_rec) in on.iter().take(slots).enumerate() {
                    placements.push(cx.slot(i == center, on_rec, off.get(i).copied())?);
                }
                Ok(placements)
            },
        )?;
        for (f, field) in [
            (clickable, "clickable"),
            (playerlamp, "playerlamp"),
            (rivallamp, "rivallamp"),
            (trophy, "trophy"),
            (graph, "graph"),
        ] {
            if f.is_some_and(|f| {
                !self.def.arena.is_array(f) || self.def.arena.children(f).next().is_some()
            }) {
                self.ignored(at, NoteKind::Ignored { field });
            }
        }
        // The parts' keys are offsets from their placement's bottom-left
        // corner: reflected about 0.
        self.flip = Q::ZERO;
        let styles = self.styles(
            at,
            &list(self, text),
            &list(self, level),
            &list(self, lamp),
            &list(self, labels),
        );
        self.flip = Q::int(HEIGHT);
        let (text, level, lamp, label) = styles?;
        let list = SongListDef {
            placements,
            center: u8::try_from(center).unwrap_or(u8::MAX),
            text,
            level,
            lamp,
            label,
        };
        // The visibilities each element charges, as the model evaluates
        // them: every arm's, and every style's, a style two row kinds
        // share counted for each.
        self.sel.arms = list
            .placements
            .iter()
            .flat_map(|p| p.iter().filter_map(|p| p.part.visible.clone()))
            .collect();
        self.sel.styles = list
            .text
            .iter()
            .flatten()
            .map(|t| &t.part)
            .chain(list.level.iter().flatten().map(|v| &v.part))
            .chain(list.lamp.iter().flatten().map(|v| &v.part))
            .chain(list.label.iter().flatten().map(|v| &v.part))
            .filter_map(|p| p.visible.clone())
            .collect();
        self.sel.list = Some(list);
        Ok(())
    }

    /// Slot `[unselected, selected]` from `liston` record `on` and
    /// `listoff` record `off`, both drawing the image set `on` names
    /// (`JsonSelectSkinObjectLoader.java:40-72`). The center slot's
    /// `liston` and every other slot's `listoff` are its active arms
    /// (`select/BarRenderer.java:124`): one that cannot be reproduced
    /// refuses the skin, the center's as the selected row; an inactive
    /// arm that cannot is noted and never draws.
    fn slot(
        &mut self,
        center: bool,
        on: u32,
        off: Option<u32>,
    ) -> Result<[PlacementDef; 2], ImportError> {
        let essential = if center {
            EssentialPart::SelectedRow
        } else {
            EssentialPart::SongList
        };
        let (id, _) = self.read_nested(on)?;
        let set = match id {
            Some(id) => self.image_set_rec(&id)?,
            None => None,
        };
        let arms = [(off, !center), (Some(on), center)];
        let mut out: [Option<PlacementDef>; 2] = [None, None];
        for (slot, &(rec, active)) in out.iter_mut().zip(&arms) {
            let role = if active {
                Role::Essential(essential)
            } else {
                Role::Decorative
            };
            let at = rec.map_or_else(|| self.arena_span(on), |r| self.arena_span(r));
            let Some(set) = set else {
                if active {
                    return Err(self.essential(
                        Some(at),
                        essential,
                        Some(NoteKind::UndefinedObject),
                    ));
                }
                self.ignored(at, NoteKind::UndefinedObject);
                continue;
            };
            let Some(rec) = rec else {
                // An active `listoff` the source would read past its list
                // (`JsonSelectSkinObjectLoader.java:68-69`): the slot's
                // geometry is missing.
                if active {
                    return Err(self.essential(Some(at), essential, None));
                }
                self.ignored(at, NoteKind::Ignored { field: "listoff" });
                continue;
            };
            *slot = self.placement(role, rec, set)?;
            if slot.is_none() && active && center {
                // The cursor's row must draw: an op folding false refuses.
                return Err(self.essential(
                    Some(at),
                    essential,
                    Some(NoteKind::Ignored { field: "op" }),
                ));
            }
        }
        let [unselected, selected] = out;
        let hidden = |cx: &mut Self, other: Option<&PlacementDef>| cx.hidden_placement(other);
        let unselected = match unselected {
            Some(p) => p,
            None => hidden(self, selected.as_ref())?,
        };
        let selected = match selected {
            Some(p) => p,
            None => hidden(self, Some(&unselected))?,
        };
        Ok([unselected, selected])
    }

    /// A placement that never draws: `like`'s track with no image and a
    /// visibility of `false`, or a still one.
    fn hidden_placement(
        &mut self,
        like: Option<&PlacementDef>,
    ) -> Result<PlacementDef, ImportError> {
        let at = self
            .sel
            .span
            .unwrap_or_else(|| self.arena_span(self.def.top()));
        // Its keys, a copy of `like`'s or one, charged before it is made.
        let n = like.map_or(1, |p| p.part.track.key_count());
        self.charged(at, |b| b.keys(n).map(|_| ()), |_| Ok(()))?;
        let visible = Some(self.compile("false", Ty::Bool, at)?);
        let part = match like {
            Some(p) => PartDef {
                visible,
                ..p.part.clone()
            },
            None => {
                let key = KeyDef {
                    at: bms_model::TimeUs(0),
                    props: [Some(0); 9],
                };
                let track = Track::new(&[key], skin_model::Ease::Linear, End::Hold)
                    .map_err(|_| self.range(at, "dst"))?;
                PartDef {
                    track,
                    timer: skin_model::TimerRef::Builtin(skin_model::selection_timer::SCREEN),
                    visible,
                    blend: skin_model::Blend::Alpha,
                    pivot: skin_model::Anchor::BottomLeft,
                }
            }
        };
        Ok(PlacementDef {
            part,
            images: [None; RowStyle::COUNT],
        })
    }

    /// The `imageset` record of id `id`, the first of it, as the source
    /// looks a placement's set up.
    fn image_set_rec(&mut self, id: &str) -> Result<Option<u32>, ImportError> {
        self.named_rec("imageset", Some(id))
    }

    /// One arm: destination record `rec` drawing image set `set`, each row
    /// kind's image chosen by the decided chain (chart 0; folder 1, else 0;
    /// "all charts" 5, else 1, else 0: owner decision O3), every image the
    /// cycle and timer of the set's first that loads
    /// (`JsonSelectSkinObjectLoader.java:47-66`). `None` when it is
    /// dropped as `role` says or never drawn.
    fn placement(
        &mut self,
        role: Role,
        rec: u32,
        set: u32,
    ) -> Result<Option<PlacementDef>, ImportError> {
        let (_, dst) = self.read_nested(rec)?;
        let at = dst.span;
        if Stretch::of(dst.stretch) != Stretch::Identity {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        let Some(mut c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(None);
        };
        let [images] = self.def.fields(set, ["images"]);
        // The set's entries charged, holes and repeats included, before
        // any is read or looked up.
        let n = images.map_or(0, |f| self.def.arena.children(f).count());
        let names = self.charged(
            at,
            |b| b.choice_refs(n),
            |cx| cx.opt_strings(images, "images"),
        )?;
        let mut recs = Vec::with_capacity(names.len());
        for name in &names {
            recs.push(match name {
                Some(n) => self.image_rec(n)?,
                None => None,
            });
        }
        // The set's timing, as the source takes it over the images that
        // load, in order (`JsonSelectSkinObjectLoader.java:47-66`): the
        // first timer that is given, and the first cycle other than -1,
        // each with the record it comes from. An image loads here when its
        // source resolves; a file missing for the source too is not told
        // apart.
        let mut timer: Option<(Prop, u32)> = None;
        let mut cycle: Option<(i32, u32)> = None;
        for &r in recs.iter().flatten() {
            if timer.is_some() && cycle.is_some() {
                break;
            }
            let s = self.src_image(r)?;
            if self.source_rec(s.src_name())?.is_none() {
                continue;
            }
            // A negative id is no timer: the source's factory makes none
            // of it (`TimerPropertyFactory.java:7-8`), so the search goes
            // on.
            let given = !matches!(s.timer, Prop::Null | Prop::Id(i32::MIN..=-1));
            if timer.is_none() && given {
                timer = Some((s.timer.clone(), r));
            }
            if cycle.is_none() && s.cycle != -1 {
                cycle = Some((s.cycle, r));
            }
        }
        let mut image = [None; 6];
        for (j, slot) in image.iter_mut().enumerate() {
            if ![0, 1, 5].contains(&j) {
                continue;
            }
            let Some(&Some(r)) = recs.get(j) else {
                continue;
            };
            let mut s = self.src_image(r)?;
            s.timer = timer.as_ref().map_or(Prop::Null, |(t, _)| t.clone());
            s.cycle = cycle.map_or(0, |(c, _)| c);
            // The image runs on that timing: cached apart from the record
            // drawn on its own.
            s.timing = [
                timer.as_ref().map_or(u32::MAX, |&(_, r)| r),
                cycle.map_or(u32::MAX, |(_, r)| r),
            ];
            // Image 0 is the arm's own; a later one falls back.
            let own = c.role;
            if j != 0 {
                c.role = Role::Decorative;
            }
            let added = self.add_src_image(&c, r, &s, s.div)?;
            c.role = own;
            *slot = added.map(|(i, _)| i);
        }
        let [Some(i0), i1, _, _, _, i5] = image else {
            self.drop_el(role, at, NoteKind::MissingAsset)?;
            return Ok(None);
        };
        let Some((rects, _)) = self.baked_rects(&c, [1, 1])? else {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        };
        let track = self.model_track(&c, &rects)?;
        self.retained(&c)?;
        Ok(Some(PlacementDef {
            part: PartDef {
                track,
                timer: c.timer,
                visible: c.visible,
                blend: c.blend,
                pivot: c.pivot,
            },
            images: [Some(i0), i1.or(Some(i0)), i5.or(i1).or(Some(i0))],
        }))
    }

    /// The song list's styles: a title for each row kind, essential, and
    /// the decorative level, lamp and label styles.
    #[expect(clippy::type_complexity, reason = "the four style tables at once")]
    fn styles(
        &mut self,
        at: Span,
        text: &[u32],
        level: &[u32],
        lamp: &[u32],
        labels: &[u32],
    ) -> Result<
        (
            [Option<RowTextDef>; RowStyle::COUNT],
            [Option<LevelDef>; LEVEL_STYLES],
            [Option<skin_model::ImagePartDef>; LampKey::COUNT],
            [Option<ImagePartDef>; RowLabel::COUNT],
        ),
        ImportError,
    > {
        // Titles: each index a chain reaches, read once.
        let mut read: [Option<Option<RowTextDef>>; TEXT_STYLES] = Default::default();
        let mut titles: [Option<RowTextDef>; RowStyle::COUNT] = [None, None, None];
        let mut used = [false; TEXT_STYLES];
        for (style, (_, chain)) in TITLE_CHAINS.into_iter().enumerate() {
            let mut found = None;
            for &k in chain {
                if read.get(k).is_some_and(Option::is_none) {
                    let def = self.row_text(text.get(k).copied())?;
                    if let Some(r) = read.get_mut(k) {
                        *r = Some(def);
                    }
                }
                if let Some(Some(Some(def))) = read.get(k) {
                    found = Some((k, def.part.track.key_count()));
                    break;
                }
            }
            let Some((k, keys)) = found else {
                return Err(self.essential(Some(at), EssentialPart::RowTitle, None));
            };
            // A style another row kind takes too is the model's copy: its
            // keys are charged again before it is made.
            if let Some(u) = used.get_mut(k) {
                if *u {
                    self.charged(at, |b| b.keys(keys).map(|_| ()), |_| Ok(()))?;
                }
                *u = true;
            }
            let def = read.get(k).cloned().flatten().flatten();
            if let Some(t) = titles.get_mut(style) {
                *t = def;
            }
        }
        for (k, &rec) in text.iter().enumerate() {
            if !matches!(k, 0 | 2 | 4 | 9) {
                let at = self.arena_span(rec);
                self.ignored(at, NoteKind::Ignored { field: "text" });
            }
        }
        let mut levels: [Option<LevelDef>; LEVEL_STYLES] = Default::default();
        for (k, &rec) in level.iter().enumerate() {
            match levels.get_mut(k) {
                Some(slot) if k < SOURCE_LEVELS => *slot = self.level_part(rec)?,
                _ => {
                    let at = self.arena_span(rec);
                    self.ignored(at, NoteKind::Ignored { field: "level" });
                }
            }
        }
        let mut lamps: [Option<ImagePartDef>; LampKey::COUNT] = Default::default();
        for (k, &rec) in lamp.iter().enumerate() {
            match lamp_key(k).filter(|_| k < SOURCE_LAMPS) {
                Some(key) => {
                    let part = self.image_part(rec)?;
                    if let Some(slot) = lamps.get_mut(key.index()) {
                        *slot = part;
                    }
                }
                None => {
                    let at = self.arena_span(rec);
                    self.ignored(at, NoteKind::Ignored { field: "lamp" });
                }
            }
        }
        let mut labelled: [Option<ImagePartDef>; RowLabel::COUNT] = Default::default();
        for (k, &rec) in labels.iter().enumerate() {
            match label(k).filter(|_| k < SOURCE_LABELS) {
                Some(l) => {
                    let part = self.image_part(rec)?;
                    if let Some(i) = RowLabel::ALL.iter().position(|&x| x == l)
                        && let Some(slot) = labelled.get_mut(i)
                    {
                        *slot = part;
                    }
                }
                None => {
                    let at = self.arena_span(rec);
                    self.ignored(at, NoteKind::Ignored { field: "label" });
                }
            }
        }
        Ok((titles, levels, lamps, labelled))
    }

    /// Title style entry `rec`: `None` when it is absent (no entry, or its
    /// id names no text), else the style, essential: one that cannot be
    /// reproduced refuses the skin (docs/stage3.md S10-09d, D10), never
    /// replaced by the next in its chain.
    fn row_text(&mut self, rec: Option<u32>) -> Result<Option<RowTextDef>, ImportError> {
        let Some(rec) = rec else {
            return Ok(None);
        };
        let (id, dst) = self.read_nested(rec)?;
        let Some(text) = self.named_rec("text", id.as_deref())? else {
            return Ok(None);
        };
        let at = dst.span;
        let role = Role::Essential(EssentialPart::RowTitle);
        let notes = self.notes.list.len();
        let Some(TextStyle {
            c,
            track,
            font,
            align,
            overflow,
        }) = self.text_style(role, &dst, text)?
        else {
            let why = self.notes.list.get(notes).map(|n| n.2);
            return Err(self.essential(Some(at), EssentialPart::RowTitle, why));
        };
        self.retained(&c)?;
        Ok(Some(RowTextDef {
            part: PartDef {
                track,
                timer: c.timer,
                visible: c.visible,
                // D5: text always blends normally.
                blend: skin_model::Blend::Alpha,
                pivot: c.pivot,
            },
            font,
            align,
            overflow,
        }))
    }

    /// The first record of table `table` with id `id`.
    fn named_rec(&mut self, table: &str, id: Option<&str>) -> Result<Option<u32>, ImportError> {
        let Some(id) = id else {
            return Ok(None);
        };
        let Some(t) = TABLES.iter().position(|n| *n == table) else {
            return Ok(None);
        };
        // Built once: each record's id decoded once, the map at most the
        // table's records.
        if self.sel.tables.get(t).is_some_and(Option::is_none) {
            let recs = super::object::records(&self.tables, table).to_vec();
            let map = super::object::index(self.def, &recs, false)?;
            if let Some(slot) = self.sel.tables.get_mut(t) {
                *slot = Some(map);
            }
        }
        Ok(self
            .sel
            .tables
            .get(t)
            .and_then(Option::as_ref)
            .and_then(|m| m.get(id))
            .copied())
    }

    /// Level style entry `rec`, decorative: its `value` record's glyph
    /// strip, showing the row's level whatever its value names.
    fn level_part(&mut self, rec: u32) -> Result<Option<LevelDef>, ImportError> {
        let (id, dst) = self.read_nested(rec)?;
        let at = dst.span;
        let Some(value) = self.named_rec("value", id.as_deref())? else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(None);
        };
        let Some(DigitsStyle {
            c,
            digits,
            track,
            native,
            ..
        }) = self.digits_style(Role::Decorative, &dst, value, None)?
        else {
            return Ok(None);
        };
        if native.is_some() {
            self.drop_el(Role::Decorative, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        self.retained(&c)?;
        Ok(Some(LevelDef {
            part: PartDef {
                track,
                timer: c.timer,
                visible: c.visible,
                blend: c.blend,
                pivot: c.pivot,
            },
            digits,
        }))
    }

    /// Lamp or label style entry `rec`, decorative: its `image` record.
    fn image_part(&mut self, rec: u32) -> Result<Option<ImagePartDef>, ImportError> {
        let (id, dst) = self.read_nested(rec)?;
        let at = dst.span;
        let Some(image) = (match id {
            Some(id) => self.image_rec(&id)?,
            None => None,
        }) else {
            self.ignored(at, NoteKind::UndefinedObject);
            return Ok(None);
        };
        let role = Role::Decorative;
        if Stretch::of(dst.stretch) == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        let s = self.src_image(image)?;
        let Some(c) = self.common_with(role, &dst, Opts::default())? else {
            return Ok(None);
        };
        let Some((i, cell)) = self.add_src_image(&c, image, &s, s.div)? else {
            return Ok(None);
        };
        let Some((rects, None)) = self.baked_rects(&c, cell)? else {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        };
        let track = self.model_track(&c, &rects)?;
        self.retained(&c)?;
        Ok(Some(ImagePartDef {
            part: PartDef {
                track,
                timer: c.timer,
                visible: c.visible,
                blend: c.blend,
                pivot: c.pivot,
            },
            image: i,
        }))
    }

    /// The essential parts once every destination is read: a song list
    /// element was kept.
    pub(super) fn selection_essentials(&mut self) -> Result<(), ImportError> {
        if self.sel.lists == 0 {
            return Err(self.essential(self.sel.span, EssentialPart::SongList, None));
        }
        Ok(())
    }
}

/// The widget quads one song list element pushes at worst, as the model
/// counts them: per slot its bar, its text (one, or a field's glyphs in a
/// bitmap font), its lamp, its level's cells and its labels.
fn worst_quads(l: &SongListDef) -> u32 {
    let text = l
        .text
        .iter()
        .flatten()
        .map(|t| match t.font {
            skin_model::FontRef::Bitmap(_) => {
                u32::try_from(skin_model::MAX_TEXT_BYTES).unwrap_or(u32::MAX)
            }
            skin_model::FontRef::Default | skin_model::FontRef::File(_) => 1,
        })
        .max()
        .unwrap_or(0);
    let cells = l
        .level
        .iter()
        .flatten()
        .map(|v| u32::from(v.digits.cells))
        .max()
        .unwrap_or(0);
    let per_slot = 2_u32
        .saturating_add(text)
        .saturating_add(cells)
        .saturating_add(u32::try_from(RowLabel::COUNT).unwrap_or(u32::MAX));
    per_slot.saturating_mul(u32::try_from(l.placements.len()).unwrap_or(u32::MAX))
}

/// A play skin's graph `type` as its source (`SkinGraph.java:95-106` at
/// `0.8.9`): 101 the play's progress (`BARGRAPH_MUSIC_PROGRESS`,
/// `skin/SkinProperty.java:190`; `FloatPropertyFactory.java:128-137,
/// 284`), 102 the load's (`BARGRAPH_LOAD_PROGRESS`, `:191`;
/// `FloatPropertyFactory.java:285-290`), and the play's score rates
/// (`FloatPropertyFactory.java:297-302`): 110 the current EX over the
/// whole chart, 111 over the judged notes, 112 and 113 the best paced and
/// whole, 114 and 115 the target's; any other type is none.
fn play_graph(ty: i32) -> Option<RatioSource> {
    match ty {
        101 => return Some(RatioSource::SongProgress),
        102 => return Some(RatioSource::LoadProgress),
        _ => {}
    }
    let k = usize::try_from(ty.checked_sub(110)?).ok()?;
    ScoreRate::ALL.get(k).copied().map(RatioSource::Score)
}
