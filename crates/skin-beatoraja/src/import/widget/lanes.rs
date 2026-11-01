//! The lanes (`note`), and the offset-4 rule that tells SUDDEN+'s paint
//! (`cover_paint`) from an image following it.
//!
//! The source draws the lanes from eight lane rects
//! (`JsonPlaySkinObjectLoader.java:79-93`, `play/LaneRenderer.java:
//! 288-727` at `0.8.9`); the model draws them as one widget whose parts
//! are the notes, the judgment line and the bar line groups
//! ([`super::bar`]). The covers' paint is apart ([`super::cover_paint`]).

use skin_model::{Filter, LaneParts, LaneStyle, LanesDef, Side, Source, SourceLanes};

use super::super::asset::{Crop, FALLBACK_SIZE};
use super::super::geom::{HEIGHT, Offset, Q, Rect};
use super::super::policy::{self, CriticalPart, Role};
use super::super::report::{
    DeviationId, LaneApprox, NoteFallback, NoteKind, PartWhy, Severity, Span,
};
use super::super::track::{self, Stretch};
use super::super::{Common, Cx, Dst, Opts, RUNTIME_OFFSETS};
use super::SrcImage;
use super::note_parts::LANES;
use crate::error::ImportError;

/// The lanes once their destination is mapped.
pub(crate) struct LaneState {
    pub(crate) def: LanesDef,
    /// The note destination.
    pub(crate) span: Span,
    /// Each kept bar line group's record.
    pub(crate) bar_spans: Vec<Span>,
    /// The lanes' rounded left and right edges.
    lane_span: [i128; 2],
}

/// The texels of a frame of `[iw, ih]` that stretch 5 keeps for
/// destination `r`, exactly: `r.h × iw / r.w`
/// (`skin/StretchType.java:65-68, 143-148`).
/// `Some(None)` when the fitted frame is no taller than the destination,
/// so nothing is cropped; `None` on overflow.
fn trimmed(r: Rect, [iw, ih]: [u32; 2]) -> Option<Option<Q>> {
    let kept = r.h.mul(Q::int(iw))?.div(r.w)?;
    Some((kept < Q::int(ih)).then_some(kept))
}

/// The rows a frame at row `y`, `h` high, keeps when it keeps `kept`
/// texels about its middle, its edges rounded half up as D1 rounds;
/// `None` when no row is left or on overflow.
fn crop_rows(y: u32, h: u32, kept: Q) -> Option<Crop> {
    let top = Q::int(y).add(Q::int(h).sub(kept)?.half()?)?;
    let (first, end) = (top.round()?, top.add(kept)?.round()?);
    let rows = u32::try_from(end.checked_sub(first)?)
        .ok()
        .filter(|&n| n > 0)?;
    Some([u32::try_from(first).ok()?, rows])
}

/// The lanes' span as read ahead of the walk, for the offset-4 rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SpanHint {
    /// No destination names the notes: nothing can reach the lanes.
    #[default]
    NoLanes,
    /// The lanes' rounded left and right edges.
    Span([i128; 2]),
    /// The notes' destination cannot be read ahead; their mapping then
    /// refuses or fails, and no offset-4 image is decorative before it.
    Undetermined,
}

impl Cx<'_> {
    /// The lanes' rounded span, read ahead of the walk from the first
    /// destination naming the notes; destinations without an id, or
    /// naming no object, are skipped as the walk skips them.
    pub(in super::super) fn lane_span_hint(&mut self, destinations: &[u32]) -> SpanHint {
        for &rec in destinations {
            let Ok(id) = super::super::object::id_of(self.def, rec) else {
                return SpanHint::Undetermined;
            };
            let Some(id) = id.filter(|id| !ops_negative(id)) else {
                continue;
            };
            let Ok(found) = self.objects.lookup(self.def, &self.tables, &id) else {
                return SpanHint::Undetermined;
            };
            let Some((object, note)) = found else {
                continue;
            };
            if object != super::super::report::Object::Note {
                continue;
            }
            let Ok((_, dst)) = self.read_dst_keys(rec, Vec::new()) else {
                return SpanHint::Undetermined;
            };
            let off = self.configured(&dst.offsets).0;
            return match self.lane_xs(note, off) {
                Ok(Some(xs)) => SpanHint::Span(span_of(&xs)),
                Ok(None) | Err(_) => SpanHint::Undetermined,
            };
        }
        SpanHint::NoLanes
    }

    /// The sum of the configured offsets among `ids` (run-time and
    /// all-elements ids skipped), and whether any rotates.
    pub(in super::super) fn configured(&self, ids: &[i32]) -> (Offset, bool) {
        let mut o = Offset::default();
        let mut rotates = false;
        let mut seen: Vec<i32> = Vec::new();
        for &id in ids {
            if !(1..=super::super::OFFSET_MAX).contains(&id)
                || seen.contains(&id)
                || RUNTIME_OFFSETS.contains(&id)
                || id == super::super::OFFSET_ALL
            {
                continue;
            }
            seen.push(id);
            if !self.header.offsets.iter().any(|d| d.id == id) {
                continue;
            }
            if let Some(v) = self.config.offsets.get(&id) {
                o.x += i64::from(v.x);
                o.y += i64::from(v.y);
                o.w += i64::from(v.w);
                o.h += i64::from(v.h);
                o.r += i64::from(v.r);
                rotates |= v.r != 0;
            }
        }
        (o, rotates)
    }

    /// The note record's raw lane rects (`dst`, no inheritance:
    /// `JsonPlaySkinObjectLoader.java:86-87`), in source lane order.
    fn lane_rects(&mut self, note: u32) -> Result<Vec<(Span, [i32; 4])>, ImportError> {
        let keys = match self.def.fields(note, ["dst"]) {
            [Some(f)] => self.tables.nested.get(&f).cloned().unwrap_or_default(),
            [None] => Vec::new(),
        };
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let mut v = [i32::MIN; 4];
            for (slot, f) in v.iter_mut().zip(self.def.fields(k, ["x", "y", "w", "h"])) {
                if let Some(f) = f {
                    *slot = self.def.int(f, "dst")?;
                }
            }
            out.push((self.arena_span(k), v));
        }
        Ok(out)
    }

    /// Each lane's rounded left and right edges with the lane offsets
    /// `o` added directly (`play/LaneRenderer.java:289-298, 521-524`):
    /// `rnd(x·r + Σx)` and `rnd((x + w)·r + Σx + Σw)`. `None` without
    /// eight lanes.
    fn lane_xs(&mut self, note: u32, o: Offset) -> Result<Option<Vec<[i128; 2]>>, ImportError> {
        let rects = self.lane_rects(note)?;
        if rects.len() != LANES {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(LANES);
        for (at, [x, _, w, _]) in rects {
            let range = || self.range(at, "dst");
            let left = Q::int(x)
                .mul(self.r)
                .and_then(|v| v.add(Q::int(o.x)))
                .ok_or_else(range)?;
            let right = Q::int(i64::from(x) + i64::from(w))
                .mul(self.r)
                .and_then(|v| v.add(Q::int(o.x + o.w)))
                .ok_or_else(range)?;
            out.push([
                left.round().ok_or_else(range)?,
                right.round().ok_or_else(range)?,
            ]);
        }
        Ok(Some(out))
    }

    /// Notes lane approximation `a` at `at`, which the owner accepts
    /// (decisions 12 and 15); `part` is what a rejected one would refuse.
    #[cfg_attr(
        not(test),
        expect(unused_variables, reason = "only a test rejects an approximation")
    )]
    pub(in super::super) fn lane_approx(
        &mut self,
        at: Span,
        part: CriticalPart,
        a: LaneApprox,
    ) -> Result<(), ImportError> {
        let kind = NoteKind::Deviation(DeviationId::D9(a));
        #[cfg(test)]
        if REJECTED.with(std::cell::Cell::get) == Some(a) {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part,
                why: kind,
            });
        }
        self.note(Some(at), Severity::Deviation, kind);
        Ok(())
    }

    /// The note destination: the lanes widget.
    pub(in super::super) fn lanes_element(
        &mut self,
        dst: Dst,
        rec: u32,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        let part = CriticalPart::Lanes;
        let role = Role::Critical(part);
        let refuse = |cx: &Self, field: &'static str| {
            Err(ImportError::Critical {
                at: cx.loc(at),
                part,
                why: NoteKind::UnsupportedField { field },
            })
        };
        if self.lanes.is_some() {
            return refuse(self, "note");
        }
        let [dst2, expansion, size, group] = self
            .def
            .fields(rec, ["dst2", "expansionrate", "size", "group"]);

        // Preconditions: the part arrays whole (`note_parts`), eight
        // lanes on one row.
        let plan = self.note_parts(rec)?;
        let lane_rects = self.lane_rects(rec)?;
        if lane_rects.len() != LANES {
            return refuse(self, "dst");
        }
        let ys: Vec<i32> = lane_rects.iter().map(|(_, r)| r[1]).collect();
        if ys.windows(2).any(|w| matches!(w, [a, b] if a != b)) {
            return refuse(self, "dst");
        }

        // The element: the destination's lifetime, timer and conditions.
        let Some(mut c) = self.common_with(
            role,
            &dst,
            Opts {
                neutral: true,
                ..Opts::default()
            },
        )?
        else {
            return Ok(());
        };
        if c.untimed && !policy::untimed_ok(true, true, c.track.end, true) {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part,
                why: NoteKind::Timer(super::super::TimerWhy::Untimed),
            });
        }
        if c.offset.r != 0 {
            self.ignored(at, NoteKind::Ignored { field: "angle" });
        }
        if dst.filter != 0 {
            self.ignored(at, NoteKind::Ignored { field: "filter" });
        }
        if dst.stretch != -1 {
            self.ignored(at, NoteKind::Ignored { field: "stretch" });
        }
        let o = c.offset;
        let Some(xs) = self.lane_xs(rec, o)? else {
            return refuse(self, "dst");
        };
        // The source paints by timeline then by lane, the model lane by
        // lane: the same only while no two lanes overlap.
        let mut sorted = xs.clone();
        sorted.sort_unstable();
        if sorted
            .windows(2)
            .any(|w| matches!(w, [a, b] if a[1] > b[0]))
        {
            return refuse(self, "dst");
        }
        let span = span_of(&xs);

        // Note heights: `size[i]·r` exactly, else the note's cell height
        // (`JsonPlaySkinObjectLoader.java:85-92`), plus the h offset.
        let sizes = self.note_sizes(size)?;

        // The parts: each the first image of its id in the image table
        // (`JsonSkinObjectLoader.java:584-598`). An id naming none draws
        // the source's generated paint (`play/SkinNote.java:112-140`),
        // except an unsized tap: the source reads its height from the
        // missing image (`JsonPlaySkinObjectLoader.java:91`).
        let mut resolved = [[None::<u32>; 6]; LANES];
        for (u, (lane, nodes)) in resolved.iter_mut().zip(&plan.nodes).enumerate() {
            for ((slot, &node), &field) in lane.iter_mut().zip(nodes).zip(&plan.fields) {
                let name = self.def.string(node, field)?.unwrap_or_default();
                *slot = self.image_rec(&name)?;
            }
            let sized = sizes.get(u).is_some_and(Option::is_some);
            if let ([None, ..], [tap, ..], false) = (*lane, *nodes, sized) {
                return Err(ImportError::Critical {
                    at: self.loc(self.arena_span(tap)),
                    part,
                    why: NoteKind::NotePart {
                        field: "note",
                        why: PartWhy::MissingSize {
                            lane: u8::try_from(u).unwrap_or(u8::MAX),
                        },
                    },
                });
            }
        }

        // Each part added, drawn nearest; the generated paint shared by
        // every part that draws it, each reported where it is named.
        // The lane map (`play/LaneProperty.java:46-50`): source lane 7 is
        // the scratch, model lane 0.
        let model = |u: usize| if u == LANES - 1 { 0 } else { u + 1 };
        c.filter = Filter::Nearest;
        let paints = [
            NoteFallback::Tap,
            NoteFallback::Long,
            NoteFallback::Long,
            NoteFallback::Long,
            NoteFallback::Long,
            NoteFallback::Mine,
        ];
        let mut cells = [[0_u32; 2]; LANES];
        let mut images = [[0_u32; 6]; LANES];
        for (u, ((recs, nodes), (lane_images, lane_cell))) in resolved
            .iter()
            .zip(&plan.nodes)
            .zip(images.iter_mut().zip(&mut cells))
            .enumerate()
        {
            let parts = recs.iter().zip(nodes).zip(plan.fields.iter().zip(paints));
            for (p, ((img, &node), (&field, paint))) in parts.enumerate() {
                let (image, cell) = match *img {
                    Some(img) => {
                        let s = self.src_image(img)?;
                        let Some(found) = self.add_src_image(&c, img, &s, s.div)? else {
                            return refuse(self, field);
                        };
                        found
                    }
                    None => {
                        let span = self.arena_span(node);
                        let image = self
                            .assets
                            .note_fallback(&mut self.budget, paint)
                            .map_err(|l| self.limit(span, l))?;
                        let usage = &mut self.budget.usage;
                        usage.note_fallbacks = usage.note_fallbacks.saturating_add(1);
                        let lane = u8::try_from(model(u)).unwrap_or(u8::MAX);
                        self.note(
                            Some(span),
                            Severity::Fallback,
                            NoteKind::NotePartFallback { field, lane, paint },
                        );
                        (image, FALLBACK_SIZE)
                    }
                };
                if let Some(slot) = lane_images.get_mut(p) {
                    *slot = image;
                }
                if p == 0 {
                    *lane_cell = cell;
                }
            }
        }

        let mut heights = [Q::ZERO; LANES];
        for ((size, cell), h) in sizes.iter().zip(&cells).zip(&mut heights) {
            *h = match size {
                Some(v) => v.mul(self.r),
                None => Q::int(cell[1]).mul(self.r),
            }
            .and_then(|q| q.add(Q::int(o.h)))
            .ok_or_else(|| self.range(at, "size"))?;
        }
        // One height for every lane, kept unrounded; its rounding is the
        // model's nominal note height.
        let [nh, ..] = heights;
        let note_h = nh.round().ok_or_else(|| self.range(at, "size"))?;
        if heights.iter().any(|&h| h != nh) || note_h < 1 {
            return refuse(self, "size");
        }
        let note_h = u16::try_from(note_h).map_err(|_| self.range(at, "size"))?;

        // Vertical (y down): the line and the top half a note above the
        // lanes' bottom and top (`play/LaneRenderer.java:324-326`).
        let [_, y0, _, h0] = lane_rects.first().map_or([0; 4], |(_, r)| *r);
        let range = |cx: &Self| cx.range(at, "dst");
        let y_up = Q::int(y0).mul(self.r).ok_or_else(|| range(self))?;
        let h_up = Q::int(h0).mul(self.r).ok_or_else(|| range(self))?;
        let base = Q::int(HEIGHT).sub(y_up).ok_or_else(|| range(self))?;
        let shifted = base
            .sub(Q::int(o.y))
            .and_then(|v| v.sub(nh.half()?))
            .ok_or_else(|| range(self))?;
        let line = shifted.round().ok_or_else(|| range(self))?;
        let top = shifted
            .sub(h_up)
            .and_then(Q::round)
            .ok_or_else(|| range(self))?;
        let bottom = base.round().ok_or_else(|| range(self))?;
        let i32_of = |cx: &Self, v: i128| i32::try_from(v).map_err(|_| range(cx));
        let [top, line, bottom] = [
            i32_of(self, top)?,
            i32_of(self, line)?,
            i32_of(self, bottom)?,
        ];
        // The source draws a note's lower edge `y` offset above the lifted
        // region's lower edge at its judgment time
        // (`play/LaneRenderer.java:521-524`). An offset that puts that note
        // below the lanes' bottom, which also takes the source's clipped
        // long-note branch (`:577-581`), or above their top, refuses.
        let oy = Q::int(o.y);
        let fits = base
            .sub(oy)
            .and_then(|v| v.sub(nh))
            .ok_or_else(|| range(self))?;
        if o.y < 0 || fits < Q::int(top) {
            return refuse(self, "offset");
        }
        let source = SourceLanes {
            bottom: base.to_f32(),
            height: h_up.to_f32(),
            offset_y: oy.to_f32(),
            note_h: nh.to_f32(),
        };

        let mut lanes = [[0_i32; 2]; LANES];
        let mut parts = LaneParts {
            note: [0; LANES],
            head: [0; LANES],
            tail: [0; LANES],
            body_on: [0; LANES],
            body_off: [0; LANES],
            mine: [0; LANES],
            line: None,
        };
        for (u, ([x0, x1], imgs)) in xs.iter().zip(images).enumerate() {
            let m = model(u);
            let x = i32::try_from(*x0).map_err(|_| range(self))?;
            let w = i32::try_from(x1 - x0).map_err(|_| range(self))?;
            if let Some(l) = lanes.get_mut(m) {
                *l = [x, w];
            }
            let [n, head, tail, body_on, body_off, mine] = imgs;
            for (list, v) in [
                (&mut parts.note, n),
                (&mut parts.head, head),
                (&mut parts.tail, tail),
                (&mut parts.body_on, body_on),
                (&mut parts.body_off, body_off),
                (&mut parts.mine, mine),
            ] {
                if let Some(slot) = list.get_mut(m) {
                    *slot = v;
                }
            }
        }
        let scratch_x = lanes.first().map_or(0, |l| l[0]);
        let scratch = if lanes.iter().skip(1).all(|l| scratch_x < l[0]) {
            Side::Left
        } else {
            Side::Right
        };

        let (bar_groups, bar_spans) = self.bar_groups(group, at)?;

        // The element.
        let t = self.neutral_track(&c)?;
        let state = LaneState {
            def: LanesDef {
                style: LaneStyle::Source(source),
                lanes,
                scratch,
                top,
                line,
                bottom,
                note_h,
                mine_h: note_h,
                line_h: 1,
                parts,
                bar_groups,
            },
            span: at,
            bar_spans,
            lane_span: span,
        };
        self.push(c, Source::Lanes, t)?;
        self.lanes = Some(state);

        // What the source draws that the model does not.
        for (field, set) in [("hidden", plan.hidden), ("processed", plan.processed)] {
            if set {
                self.ignored(at, NoteKind::Ignored { field });
            }
        }
        if plan.hcn {
            self.ignored(at, NoteKind::Ignored { field: "hcn" });
        }
        // The BPM, STOP and time guides, which the source draws only under
        // player settings (`play/LaneRenderer.java:401-438`), are not drawn.
        let [bpm, stop, time] = self.def.fields(rec, ["bpm", "stop", "time"]);
        for (field, f) in [("bpm", bpm), ("stop", stop), ("time", time)] {
            if f.and_then(|f| self.tables.nested.get(&f))
                .is_some_and(|l| !l.is_empty())
            {
                self.note(
                    Some(at),
                    Severity::Dropped,
                    NoteKind::UnsupportedField { field },
                );
            }
        }
        for a in [LaneApprox::TopCrop, LaneApprox::LnBody] {
            self.lane_approx(at, part, a)?;
        }
        if dst2.is_some_and(|f| self.def.int(f, "dst2").is_ok_and(|v| v != i32::MIN)) {
            self.lane_approx(at, part, LaneApprox::Dst2)?;
        }
        if expansion.is_some() && self.ints(expansion, "expansionrate")? != [100, 100] {
            self.lane_approx(at, part, LaneApprox::Expansion)?;
        }
        Ok(())
    }

    /// Whether an image listing offset 4 cannot move over the lanes
    /// (stage3.md, the SUDDEN+ mapping): no rotation of any kind, the
    /// identity stretch, and every key's drawn x edges, with the
    /// horizontal user offsets, outside the lanes' rounded span. Offsets
    /// 3, 4 and 5 move only y and alpha (`play/LaneRenderer.java:332-345`)
    /// and an identity-stretch frame interpolates its neighbouring keys'
    /// edges, so the hull bounds every frame.
    fn decorative_offset4(&mut self, dst: &Dst) -> Result<bool, ImportError> {
        if dst.offsets.iter().any(|&id| id == 1 || id == 2)
            || Stretch::of(dst.stretch) != Stretch::Identity
        {
            return Ok(false);
        }
        let (o, rotates) = self.configured(&dst.offsets);
        let keys = track::read(self.def, &dst.keys, None)?;
        if rotates || keys.keys.iter().any(|k| k.angle != 0) {
            return Ok(false);
        }
        let hint = match self.lanes.as_ref() {
            Some(l) => SpanHint::Span(l.lane_span),
            None => self.span_hint,
        };
        let [s0, s1] = match hint {
            SpanHint::Span(s) => s,
            SpanHint::NoLanes => return Ok(true),
            SpanHint::Undetermined => return Ok(false),
        };
        let range = |cx: &Self| cx.range(dst.span, "dst");
        let mut hull: Option<(Q, Q)> = None;
        for k in &keys.keys {
            let Rect { x, w, .. } = Rect::scaled(k.rect, self.r)
                .and_then(|r| r.offset(o))
                .ok_or_else(|| range(self))?;
            let x1 = x.add(w).ok_or_else(|| range(self))?;
            let (lo, hi) = (x.min(x1), x.max(x1));
            hull = Some(match hull {
                Some((a, b)) => (a.min(lo), b.max(hi)),
                None => (lo, hi),
            });
        }
        Ok(hull.is_none_or(|(lo, hi)| hi <= Q::int(s0) || lo >= Q::int(s1)))
    }

    /// Charges an image listing offset 4 its keys as written, once before
    /// either reading, and tells whether it cannot move over the lanes:
    /// `true` for an ordinary image following SUDDEN+, `false` for
    /// SUDDEN+'s paint, which is play-critical.
    pub(super) fn offset4_outside(&mut self, dst: &Dst) -> Result<bool, ImportError> {
        let at = dst.span;
        self.budget
            .keys(dst.keys.len())
            .map_err(|l| self.limit(at, l))?;
        self.decorative_offset4(dst)
    }

    /// A cover's destination in reference pixels, y up.
    fn cover_rect(&self, c: &Common) -> Result<Rect, ImportError> {
        match c.track.keys.first() {
            Some(k) => self.key_rect(c, k),
            None => Err(self.range(c.span, "dst")),
        }
    }

    /// A cover's image and its destination stretched against the image's
    /// frame, y up: unchanged for identity; fit to its width for stretch
    /// 4 (`skin/StretchType.java:61-64, 125-129`); for stretch 5 the same
    /// unless the fitted frame is taller than the destination, when the
    /// destination stays and the frame keeps its middle rows, which only
    /// a single frame can (`:65-68, 143-153`). `None` when the image was
    /// dropped, also as its role says, before anything is added, when the
    /// crop cannot be reproduced.
    pub(super) fn add_cover_image(
        &mut self,
        c: &Common,
        dst: &Dst,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Option<(u32, Rect)>, ImportError> {
        let at = c.span;
        let r = self.cover_rect(c)?;
        let (mode, grid) = (dst.stretch, s.div);
        let mut cropped = false;
        let found = self.add_src_image_cropped(c, rec, s, grid, |cx, [_, y, w, h]| {
            if mode != 5 {
                return Ok(Ok(None));
            }
            let cell = [w / grid[0].max(1), h / grid[1].max(1)];
            let Some(kept) = trimmed(r, cell).ok_or_else(|| cx.range(at, "dst"))? else {
                return Ok(Ok(None));
            };
            if grid != [1, 1] {
                return Ok(Err(NoteKind::Stretch(mode)));
            }
            let Some(crop) = crop_rows(y, h, kept) else {
                return Ok(Err(NoteKind::UnsupportedField { field: "dst" }));
            };
            cropped = true;
            Ok(Ok(Some(crop)))
        })?;
        let Some((image, cell)) = found else {
            return Ok(None);
        };
        if matches!(mode, 4 | 5) && !cropped {
            let fitted = track::stretch(Stretch::FitWidth, r, cell);
            return match fitted {
                Some((r, _)) => Ok(Some((image, r))),
                None => Err(self.range(at, "dst")),
            };
        }
        Ok(Some((image, r)))
    }
}

#[cfg(test)]
thread_local! {
    /// A lane approximation this thread's imports treat as rejected by
    /// the owner.
    pub(crate) static REJECTED: std::cell::Cell<Option<LaneApprox>> =
        const { std::cell::Cell::new(None) };
}

/// The span of lane edges `xs`.
fn span_of(xs: &[[i128; 2]]) -> [i128; 2] {
    xs.iter().fold([i128::MAX, i128::MIN], |[a, b], &[x0, x1]| {
        [a.min(x0), b.max(x1)]
    })
}

/// Whether a destination id is a negative number: a player-drawn image.
fn ops_negative(id: &str) -> bool {
    id.parse::<i32>().is_ok_and(|n| n < 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: Q, h: Q) -> Rect {
        Rect {
            x: Q::int(20),
            y: Q::int(800),
            w,
            h,
        }
    }

    /// The crop of a `[iw, ih]` frame at row `y` under `w`×`h`.
    fn crop(w: Q, h: Q, y: u32, [iw, ih]: [u32; 2]) -> Option<Crop> {
        crop_rows(y, ih, trimmed(rect(w, h), [iw, ih]).unwrap()?)
    }

    #[test]
    fn stretch_five_crops_exactly() {
        let int = |v: i32| Q::int(v);
        // A 25×128 frame over 430×258 keeps 258·25/430 = 15 rows about
        // its middle: its edges 56.5 and 71.5 round to rows 57 to 71.
        assert_eq!(crop(int(430), int(258), 0, [25, 128]), Some([57, 15]));
        // A 1600×900 skin scales 430×86 to 516×103.2: a 15×4 frame keeps
        // 3 rows from 0.5, rows 1 to 3.
        let (w, h) = (int(516), Q::new(516, 5).unwrap());
        assert_eq!(crop(w, h, 0, [15, 4]), Some([1, 3]));
        // A 75×17 frame at row 24 under 430×86 keeps 15 rows from 25.
        assert_eq!(crop(int(430), int(86), 24, [75, 17]), Some([25, 15]));
        // A frame no taller than the destination once fit is not cropped.
        assert_eq!(trimmed(rect(int(430), int(300)), [43, 30]), Some(None));
        assert_eq!(
            trimmed(rect(int(430), int(299)), [43, 30]),
            Some(Q::new(299, 10))
        );
        // No row left: 0.4 texels about row 64.
        assert_eq!(crop_rows(0, 128, Q::new(2, 5).unwrap()), None);
    }
}
