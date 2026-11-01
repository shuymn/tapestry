//! The lanes (`note`), SUDDEN+ (an image listing offset 4), LIFT
//! (`liftCover`) and HIDDEN+ (`hiddenCover`).
//!
//! The source draws the lanes from eight lane rects
//! (`JsonPlaySkinObjectLoader.java:79-93`, `play/LaneRenderer.java:
//! 288-727` at `0.8.9`); the model draws them as one widget whose parts
//! are the notes, the bar line, SUDDEN+, LIFT and HIDDEN+. The covers map
//! only in a narrow subset: static, white, exactly over the lanes, on the
//! lanes' own timer, conditions and window. SUDDEN+ and LIFT are
//! play-critical and refuse outside it; HIDDEN+, which the player may
//! leave off, is dropped with a note (owner decision 15).

use skin_model::{
    Blend, End, Filter, HiddenTrim, LaneParts, LaneStyle, LanesDef, MAX_COORD, Side, Source,
    SourceLanes, TimerRef,
};

use super::super::asset::Crop;
use super::super::geom::{HEIGHT, Offset, Q, Rect};
use super::super::policy::{self, CriticalPart, Role};
use super::super::report::{DeviationId, LaneApprox, NoteKind, Severity, Span};
use super::super::track::{self, Accepted, Stretch};
use super::super::{
    Common, Cx, Dst, OFFSET_HIDDEN, OFFSET_LANECOVER, OFFSET_LIFT, Op, Opts, Prop, RUNTIME_OFFSETS,
};
use super::SrcImage;
use crate::error::ImportError;
use crate::tree::Scalar;

/// A cover's image and its stretched destination, or why its stretch
/// cannot be reproduced.
type Fitted = Result<(u32, Rect), NoteKind>;

/// The lanes the source draws.
const LANES: usize = 8;

/// HIDDEN+'s own switch, `hidden1_on` (`SkinProperty.java:860`).
const OP_HIDDEN: i32 = 273;

/// Whether `name`, a boolean property name, is `hidden1_on` with an even
/// number of `!` before it.
fn hidden_on_name(name: &str) -> bool {
    let rest = name.trim_start_matches('!');
    rest == "hidden1_on" && (name.len() - rest.len()).is_multiple_of(2)
}

/// When an element shows: from its first key to its last under `Hide`,
/// from its first key on otherwise (`None`).
type Window = (i64, Option<i64>);

/// The window of an accepted track.
fn window(t: &Accepted) -> Window {
    let k0 = t.keys.first().map_or(0, |k| k.at);
    let kn = t.keys.last().map_or(0, |k| k.at);
    match t.end {
        End::Hide => (k0, Some(kn)),
        End::Hold | End::Loop { .. } => (k0, None),
    }
}

/// The lanes once their destination is mapped.
pub(crate) struct LaneState {
    pub(crate) def: LanesDef,
    /// The note destination.
    pub(crate) span: Span,
    /// The lanes' rounded left and right edges.
    lane_span: [i128; 2],
    /// Lane 0's source `y` and `h` (`play/LaneRenderer.java:324-326`).
    y0: i32,
    h0: i32,
    timer: TimerRef,
    cond_src: String,
    window: Window,
    /// Some user offset moves the lanes.
    moved: bool,
    /// An element was kept after the notes.
    pub(crate) kept_after: bool,
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

/// `v` exactly; `None` for one beyond the rationals' range.
fn f32_q(v: f32) -> Option<Q> {
    if !v.is_finite() {
        return None;
    }
    let bits = v.to_bits();
    let negative = bits >> 31 == 1;
    let exp = i32::try_from((bits >> 23) & 0xFF).ok()?;
    let frac = i128::from(bits & 0x7F_FFFF);
    let (m, e) = if exp == 0 {
        (frac, -149)
    } else {
        (frac | 0x80_0000, exp - 150)
    };
    let m = if negative { -m } else { m };
    if e >= 0 {
        Q::new(m.checked_mul(1_i128.checked_shl(e.unsigned_abs())?)?, 1)
    } else {
        let d = 1_i128.checked_shl(e.unsigned_abs())?;
        if d <= 0 {
            return None;
        }
        Q::new(m, d)
    }
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
    /// (decision 12); `part` is what a rejected one would refuse.
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
        const FIELDS: [&str; 22] = [
            "note",
            "lnstart",
            "lnend",
            "lnbody",
            "lnbodyActive",
            "lnactive",
            "hcnstart",
            "hcnend",
            "hcnbody",
            "hcnactive",
            "hcnbodyActive",
            "hcndamage",
            "hcnbodyMiss",
            "hcnreactive",
            "hcnbodyReactive",
            "mine",
            "hidden",
            "processed",
            "dst2",
            "expansionrate",
            "size",
            "group",
        ];
        let fields = self.def.fields(rec, FIELDS);
        let mut lists: Vec<Vec<Option<String>>> = Vec::with_capacity(18);
        for (f, name) in fields.iter().zip(FIELDS).take(18) {
            lists.push(self.opt_strings(*f, name)?);
        }
        let none = Vec::new();
        let list = |i: usize| lists.get(i).unwrap_or(&none);
        let [note, lnstart, lnend, lnbody, lnbody_active, lnactive] = [0, 1, 2, 3, 4, 5].map(list);
        let hcn: Vec<&Vec<Option<String>>> = (6..15).map(list).collect();
        let [mine, hidden, processed] = [15, 16, 17].map(list);
        let [.., dst2, expansion, size, group] = fields;

        // Preconditions (`SkinNote.java:27-40`, `JsonPlaySkinObjectLoader.
        // java:50-93`): eight notes and lanes, long-note arrays at least
        // `lnend`'s length, eight mines.
        let lane_rects = self.lane_rects(rec)?;
        if note.len() != LANES || lane_rects.len() != LANES {
            return refuse(self, "dst");
        }
        let (on, off) = if lnbody_active.is_empty() {
            (lnbody, lnactive)
        } else {
            (lnbody_active, lnbody)
        };
        let hcn_len = |i: usize| hcn.get(i).map_or(0, |l| l.len());
        let hcn_lists: [usize; 6] = if hcn_len(4) == 0 {
            // hcnstart, hcnend, hcnbody, hcnactive, hcndamage, hcnreactive
            [0, 1, 2, 3, 5, 7]
        } else {
            // hcnstart, hcnend, hcnbodyActive, hcnbody, hcnbodyReactive,
            // hcnbodyMiss
            [0, 1, 4, 2, 8, 6]
        };
        let tail = lnend.len();
        if tail < LANES
            || [lnstart, on, off].iter().any(|l| l.len() < tail)
            || hcn_lists.iter().any(|&i| hcn_len(i) < tail)
        {
            return refuse(self, "lnend");
        }
        if mine.len() < LANES {
            return refuse(self, "mine");
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
        let moved = [o.x, o.y, o.w, o.h].iter().any(|&v| v != 0);
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

        // The parts, each its lane's image in the image table, drawn nearest.
        c.filter = Filter::Nearest;
        let mut cells = [[0_u32; 2]; LANES];
        let mut images = [[0_u32; 6]; LANES];
        let names: [(&Vec<Option<String>>, &'static str); 6] = [
            (note, "note"),
            (lnstart, "lnstart"),
            (lnend, "lnend"),
            (on, "lnbody"),
            (off, "lnbody"),
            (mine, "mine"),
        ];
        for u in 0..LANES {
            for (p, (names, field)) in names.iter().enumerate() {
                // A `null` entry names no image, unlike an empty id
                // (`JsonSkinObjectLoader.java:584-599` fails on it).
                let Some(Some(name)) = names.get(u) else {
                    return refuse(self, field);
                };
                let Some(img) = self.image_rec(name)? else {
                    return refuse(self, field);
                };
                let s = self.src_image(img)?;
                let Some((image, cell)) = self.add_src_image(&c, img, &s, s.div)? else {
                    return refuse(self, field);
                };
                if let Some(slot) = images.get_mut(u).and_then(|i| i.get_mut(p)) {
                    *slot = image;
                }
                if p == 0
                    && let Some(c) = cells.get_mut(u)
                {
                    *c = cell;
                }
            }
        }

        // Note heights: `size[i]·r`, else the note's cell height
        // (`JsonPlaySkinObjectLoader.java:88-92`), plus the h offset.
        let sizes = self.floats(size, "size")?;
        let mut heights = [Q::ZERO; LANES];
        for ((u, cell), h) in cells.iter().enumerate().zip(&mut heights) {
            *h = match sizes.get(u) {
                Some(&v) => f32_q(v).and_then(|q| q.mul(self.r)),
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
        let region_top = base.sub(h_up).ok_or_else(|| range(self))?;
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

        // The lane map (`play/LaneProperty.java:46-50`): source lane 7 is
        // the scratch, model lane 0.
        let model = |u: usize| if u == LANES - 1 { 0 } else { u + 1 };
        let mut lanes = [[0_i32; 2]; LANES];
        let mut parts = LaneParts {
            note: [0; LANES],
            head: [0; LANES],
            tail: [0; LANES],
            body_on: [0; LANES],
            body_off: [0; LANES],
            mine: [0; LANES],
            bar: None,
            line: None,
            cover: None,
            lift: None,
            hidden: None,
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

        // The bar line, from the first group (decision 13).
        let (bar, bar_h) = self.bar_line(group, at)?.unzip();
        parts.bar = bar;

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
                // SUDDEN+, LIFT and HIDDEN+ set their own extents when they map.
                cover_at: region_top.to_f32(),
                cover_h: 0.0,
                lift_at: source.bottom,
                lift_h: 0.0,
                hidden_at: source.bottom,
                hidden_h: 0.0,
                hidden_trim: HiddenTrim::None,
                note_h,
                mine_h: note_h,
                bar_h: bar_h.unwrap_or(1),
                line_h: 1,
                parts,
            },
            span: at,
            lane_span: span,
            y0,
            h0,
            timer: c.timer,
            cond_src: c.cond_src.clone(),
            window: window(&c.track),
            moved,
            kept_after: false,
        };
        self.push(c, Source::Lanes, t);
        self.lanes = Some(state);

        // What the source draws that the model does not.
        for (field, list) in [("hidden", hidden), ("processed", processed)] {
            if !list.is_empty() {
                self.ignored(at, NoteKind::Ignored { field });
            }
        }
        if hcn.iter().any(|l| !l.is_empty()) {
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
        if bar.is_some() {
            self.lane_approx(at, part, LaneApprox::BarLine)?;
        }
        Ok(())
    }

    /// A `float[]` field's items as the source reads them, as `f32`.
    fn floats(&self, field: Option<u32>, name: &'static str) -> Result<Vec<f32>, ImportError> {
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        let arena = &self.def.arena;
        if arena.node(f).kind != crate::tree::Kind::Array {
            return Err(self.def.type_error(f, name, "an array"));
        }
        arena
            .children(f)
            .map(|i| {
                #[expect(clippy::cast_possible_truncation, reason = "the source's float field")]
                let v = match arena.scalar(i) {
                    Scalar::Long(raw) => raw.parse::<i64>().ok().map(|v| v as f32),
                    Scalar::Double(raw) => raw.parse::<f64>().ok().map(|v| v as f32),
                    _ => None,
                };
                v.ok_or_else(|| self.def.type_error(i, name, "a number"))
            })
            .collect()
    }

    /// The bar line of the note's `group` (`JsonPlaySkinObjectLoader.
    /// java:94-113`): its image and height when the first and only group
    /// is a plain, perpetual, white image; dropped with a note otherwise
    /// (decision 13). The source draws it on the chart clock
    /// (`play/LaneRenderer.java:300-301, 442-446`), so it maps only when
    /// that clock cannot show.
    fn bar_line(
        &mut self,
        group: Option<u32>,
        at: Span,
    ) -> Result<Option<(u32, u16)>, ImportError> {
        let groups = group
            .and_then(|f| self.tables.nested.get(&f))
            .cloned()
            .unwrap_or_default();
        let drop = |cx: &mut Self, field: &'static str| {
            cx.drop_el(Role::Decorative, at, NoteKind::UnsupportedField { field })
                .map(|()| None)
        };
        let [g] = groups[..] else {
            return if groups.is_empty() {
                Ok(None)
            } else {
                drop(self, "group")
            };
        };
        let (id, gdst) = self.read_nested(g)?;
        let Some(img) = id.map(|id| self.image_rec(&id)).transpose()?.flatten() else {
            return drop(self, "group");
        };
        let s = self.src_image(img)?;
        let frames = self.grid_frames(at, s.div)?;
        let plain = gdst.timer == super::super::Prop::Null
            && gdst.ops.is_empty()
            && gdst.draw == super::super::Prop::Null
            && (frames == 1 || s.cycle == 0)
            && Stretch::of(gdst.stretch) == Stretch::Identity
            && !matches!(gdst.blend, 2..=4 | 9)
            && !gdst.offsets.iter().any(|id| RUNTIME_OFFSETS.contains(id));
        if !plain {
            return drop(self, "group");
        }
        let Some(c) = self.common(Role::Decorative, &gdst, false)? else {
            return Ok(None);
        };
        let still = c.track.identical()
            && c.track.keys.first().is_some_and(|k| {
                k.at == 0 && k.color == [255; 4] && k.angle == 0 && c.offset.r == 0
            })
            && matches!(c.track.end, End::Hold | End::Loop { .. })
            && c.offset == Offset::default();
        if !still {
            return drop(self, "group");
        }
        let h = c.track.keys.first().map_or(0, |k| k.rect[3]);
        let bar_h = Q::int(h)
            .mul(self.r)
            .and_then(Q::round)
            .and_then(|h| u16::try_from(h).ok())
            .filter(|&h| h >= 1);
        let Some(bar_h) = bar_h else {
            return drop(self, "group");
        };
        let Some((image, _)) = self.add_src_image(&c, img, &s, s.div)? else {
            return Ok(None);
        };
        Ok(Some((image, bar_h)))
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
    /// either reading, and drops it as decorative when it cannot move
    /// over the lanes: `true` when dropped.
    fn offset4_dropped(&mut self, dst: &Dst) -> Result<bool, ImportError> {
        let at = dst.span;
        self.budget
            .keys(dst.keys.len())
            .map_err(|l| self.limit(at, l))?;
        if !self.decorative_offset4(dst)? {
            return Ok(false);
        }
        self.drop_el(
            Role::Decorative,
            at,
            NoteKind::RuntimeOffset(OFFSET_LANECOVER),
        )?;
        Ok(true)
    }

    /// An image listing offset 4 that selects its frames by `field`, an
    /// expression the mapping does not run: decorative and dropped when
    /// it cannot move over the lanes, refused otherwise.
    pub(super) fn lane_cover_selector(
        &mut self,
        dst: Dst,
        field: &'static str,
    ) -> Result<(), ImportError> {
        if self.offset4_dropped(&dst)? {
            return Ok(());
        }
        Err(ImportError::Critical {
            at: self.loc(dst.span),
            part: CriticalPart::Cover,
            why: NoteKind::UnsupportedField { field },
        })
    }

    /// An image listing offset 4: decorative and dropped when it cannot
    /// move over the lanes, else SUDDEN+ when it is the first exact match,
    /// else refused.
    pub(super) fn lane_cover_image(
        &mut self,
        dst: Dst,
        rec: u32,
        s: &SrcImage,
    ) -> Result<(), ImportError> {
        let at = dst.span;
        if self.offset4_dropped(&dst)? {
            return Ok(());
        }
        let part = CriticalPart::Cover;
        let refuse = |cx: &Self, field: &'static str| {
            Err(ImportError::Critical {
                at: cx.loc(at),
                part,
                why: NoteKind::UnsupportedField { field },
            })
        };
        let Some(l) = self.lanes.as_ref() else {
            return refuse(self, "offset");
        };
        if l.def.parts.cover.is_some() {
            return refuse(self, "offset");
        }
        let Some(c) = self.cover_common(&dst, Role::Critical(part), &[OFFSET_LANECOVER])? else {
            return Ok(());
        };
        let Some(l) = self.lanes.as_ref() else {
            return refuse(self, "offset");
        };
        // Exactly over the lanes, its lower edge on their top; the source
        // moves it by an offset and draws it at its own height
        // (`play/LaneRenderer.java:332-333`).
        let [xc, yc, wc, hc] = c.track.keys.first().map_or([0; 4], |k| k.rect);
        if !self.over_lanes(at, xc, wc)?
            || i64::from(yc) != i64::from(l.y0) + i64::from(l.h0)
            || hc < 0
        {
            return refuse(self, "dst");
        }
        let (image, r) = match self.add_cover_image(&c, &dst, rec, s)? {
            Some(Ok(found)) => found,
            Some(Err(why)) => {
                return Err(ImportError::Critical {
                    at: self.loc(at),
                    part,
                    why,
                });
            }
            None => return refuse(self, "src"),
        };
        if r.h <= Q::int(0) {
            return refuse(self, "dst");
        }
        let [_, lower] = self.cover_edges(at, r)?;
        let kept_after = self.lanes.as_ref().is_some_and(|l| l.kept_after);
        if let Some(l) = self.lanes.as_mut() {
            l.def.parts.cover = Some(image);
            l.def.cover_at = lower.to_f32();
            l.def.cover_h = r.h.to_f32();
        }
        self.lane_approx(at, part, LaneApprox::CoverAbove)?;
        if kept_after {
            self.lane_approx(at, part, LaneApprox::CoverOrder)?;
        }
        Ok(())
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
    /// dropped; `Some(Err(why))`, before anything is added, when the crop
    /// cannot be reproduced.
    fn add_cover_image(
        &mut self,
        c: &Common,
        dst: &Dst,
        rec: u32,
        s: &SrcImage,
    ) -> Result<Option<Fitted>, ImportError> {
        let at = c.span;
        let r = self.cover_rect(c)?;
        let (mode, grid) = (dst.stretch, s.div);
        let mut cropped = false;
        // Why the crop cannot be reproduced; the error returned with it
        // only stops the admission.
        let mut refused = None;
        let found = self.add_src_image_cropped(c, rec, s, grid, |cx, [_, y, w, h]| {
            if mode != 5 {
                return Ok(None);
            }
            let cell = [w / grid[0].max(1), h / grid[1].max(1)];
            let Some(kept) = trimmed(r, cell).ok_or_else(|| cx.range(at, "dst"))? else {
                return Ok(None);
            };
            if grid != [1, 1] {
                refused = Some(NoteKind::Stretch(mode));
                return Err(cx.range(at, "dst"));
            }
            let Some(crop) = crop_rows(y, h, kept) else {
                refused = Some(NoteKind::UnsupportedField { field: "dst" });
                return Err(cx.range(at, "dst"));
            };
            cropped = true;
            Ok(Some(crop))
        });
        if let Some(why) = refused {
            return Ok(Some(Err(why)));
        }
        let Some((image, cell)) = found? else {
            return Ok(None);
        };
        if matches!(mode, 4 | 5) && !cropped {
            let fitted = track::stretch(Stretch::FitWidth, r, cell);
            return match fitted {
                Some((r, _)) => Ok(Some(Ok((image, r)))),
                None => Err(self.range(at, "dst")),
            };
        }
        Ok(Some(Ok((image, r))))
    }

    /// A cover's upper and lower edges, y down, each within
    /// ±[`MAX_COORD`].
    fn cover_edges(&self, at: Span, r: Rect) -> Result<[Q; 2], ImportError> {
        let max = i128::from(MAX_COORD);
        let near = |v: Q| Q::int(-max) <= v && v <= Q::int(max);
        let lower = Q::int(HEIGHT).sub(r.y);
        match (lower.and_then(|v| v.sub(r.h)), lower) {
            (Some(upper), Some(lower)) if near(upper) && near(lower) => Ok([upper, lower]),
            _ => Err(self.range(at, "dst")),
        }
    }

    /// Whether `x` and `w` (source pixels) round to the lanes' span.
    fn over_lanes(&self, at: Span, x: i32, w: i32) -> Result<bool, ImportError> {
        let Some([s0, s1]) = self.lanes.as_ref().map(|l| l.lane_span) else {
            return Ok(false);
        };
        let left = Q::int(x).mul(self.r).and_then(Q::round);
        let right = Q::int(i64::from(x) + i64::from(w))
            .mul(self.r)
            .and_then(Q::round);
        let (Some(left), Some(right)) = (left, right) else {
            return Err(self.range(at, "dst"));
        };
        Ok(left == s0 && right == s1)
    }

    /// A cover's common part, refused as `role` says unless it is static,
    /// white, opaque, alpha-blended, unrotated, unstretched or fit to its
    /// width (stretch 4 or 5), not moved by any user offset, and shown
    /// exactly when the lanes are.
    fn cover_common(
        &mut self,
        dst: &Dst,
        role: Role,
        allow: &[i32],
    ) -> Result<Option<Common>, ImportError> {
        let at = dst.span;
        let Some(c) = self.common_with(
            role,
            dst,
            Opts {
                allow,
                charged: role == Role::Critical(CriticalPart::Cover),
                ..Opts::default()
            },
        )?
        else {
            return Ok(None);
        };
        let Some(l) = self.lanes.as_ref() else {
            return Ok(None);
        };
        if !(Stretch::of(dst.stretch) == Stretch::Identity || matches!(dst.stretch, 4 | 5)) {
            self.drop_el(role, at, NoteKind::Stretch(dst.stretch))?;
            return Ok(None);
        }
        let still = c.track.identical()
            && c.track
                .keys
                .first()
                .is_some_and(|k| k.color == [255; 4] && k.angle == 0)
            && c.blend == Blend::Alpha
            && dst.blend != 3
            && c.offset == Offset::default()
            && !l.moved;
        let same = c.timer == l.timer && c.cond_src == l.cond_src && window(&c.track) == l.window;
        if !(still && same) {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "dst" })?;
            return Ok(None);
        }
        Ok(Some(c))
    }

    /// A `liftCover` destination: LIFT when it is the first, static, exact
    /// cover under the lanes trimmed at their bottom; refused otherwise.
    pub(in super::super) fn lift_cover(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let part = CriticalPart::Lift;
        let refuse = |cx: &Self, field: &'static str| {
            Err(ImportError::Critical {
                at: cx.loc(at),
                part,
                why: NoteKind::UnsupportedField { field },
            })
        };
        let Some(l) = self.lanes.as_ref() else {
            return refuse(self, "liftCover");
        };
        if l.def.parts.lift.is_some() {
            return refuse(self, "liftCover");
        }
        let y0 = l.y0;
        let [line, link] = self
            .def
            .fields(rec, ["disapearLine", "isDisapearLineLinkLift"]);
        let line = self.int_or(line, "disapearLine", -1)?;
        let link = match link {
            Some(f) => self.def.bool(f, "isDisapearLineLinkLift")?,
            None => false,
        };
        // Trimmed at the lanes' bottom and not following LIFT
        // (`play/SkinHidden.java:63-66, 75-90`).
        if line != y0 || link {
            return refuse(self, "disapearLine");
        }
        // The source adds the LIFT offset (`JsonPlaySkinObjectLoader.java:
        // 207-210`).
        let s = self.src_image(rec)?;
        let Some(c) = self.cover_common(&dst, Role::Critical(part), &[OFFSET_LIFT])? else {
            return Ok(());
        };
        let [xl, yl, wl, hl] = c.track.keys.first().map_or([0; 4], |k| k.rect);
        // A destination without height refuses, even with stretch 4: at
        // zero lift the source hides it, but a lift can pass its trim gate
        // and stretch 4 would then grow it, changing its vertical extent
        // (below).
        if !self.over_lanes(at, xl, wl)?
            || hl <= 0
            || i64::from(yl) + i64::from(hl) != i64::from(y0)
        {
            return refuse(self, "dst");
        }
        let unstretched = self.cover_rect(&c)?;
        let (image, r) = match self.add_cover_image(&c, &dst, rec, &s)? {
            Some(Ok(found)) => found,
            Some(Err(why)) => {
                return Err(ImportError::Critical {
                    at: self.loc(at),
                    part,
                    why,
                });
            }
            None => return refuse(self, "src"),
        };
        // The source clips LIFT to its unstretched destination before
        // stretching it (`play/SkinHidden.java:75-88`,
        // `skin/SkinObject.java:655-656`): a stretch maps only when it
        // keeps the destination's vertical extent, so the cover's upper
        // edge stays on the raised line.
        if (r.y, r.h) != (unstretched.y, unstretched.h) {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part,
                why: NoteKind::Stretch(dst.stretch),
            });
        }
        let [upper, _] = self.cover_edges(at, r)?;
        let kept_after = self.lanes.as_ref().is_some_and(|l| l.kept_after);
        if let Some(l) = self.lanes.as_mut() {
            l.def.parts.lift = Some(image);
            l.def.lift_at = upper.to_f32();
            l.def.lift_h = r.h.to_f32();
        }
        if kept_after {
            self.lane_approx(at, part, LaneApprox::CoverOrder)?;
        }
        Ok(())
    }

    /// A `hiddenCover` destination: HIDDEN+ when it is the first, static,
    /// exact cover across the lanes; dropped with a note otherwise (owner
    /// decision 15).
    ///
    /// The source raises it by the LIFT and HIDDEN+ offsets (3 and 5,
    /// `JsonPlaySkinObjectLoader.java:182-194`, `play/LaneRenderer.java:
    /// 324-345`), which the model's geometry is, so they are consumed
    /// however they are listed. Its enable switch, op 273, is the player's
    /// setting: the model draws HIDDEN+ exactly while its amount is above
    /// 0, so a positive op 273 is taken from its conditions. Its
    /// `disapearLine` trims it below (`play/SkinHidden.java:61-90`):
    /// fixed, or rising with the lift when `isDisapearLineLinkLift`
    /// (default true, `skin/json/JsonSkin.java:391-392`); a negative line
    /// is no trim.
    pub(in super::super) fn hidden_cover(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let role = Role::Decorative;
        let drop = |cx: &mut Self, field: &'static str| {
            cx.drop_el(role, at, NoteKind::UnsupportedField { field })
        };
        let Some(l) = self.lanes.as_ref() else {
            return drop(self, "hiddenCover");
        };
        if l.def.parts.hidden.is_some() {
            return drop(self, "hiddenCover");
        }
        let (y0, h0) = (l.y0, l.h0);
        let [line, link] = self
            .def
            .fields(rec, ["disapearLine", "isDisapearLineLinkLift"]);
        let line = self.int_or(line, "disapearLine", -1)?;
        let link = match link {
            Some(f) => self.def.bool(f, "isDisapearLineLinkLift")?,
            None => true,
        };
        let s = self.src_image(rec)?;
        let mut dst = dst;
        dst.ops.retain(|op| match op {
            Op::Id(n) => *n != OP_HIDDEN,
            Op::Name(name) => !hidden_on_name(name),
        });
        if match &dst.draw {
            Prop::Id(n) => *n == OP_HIDDEN,
            Prop::Name(name) => hidden_on_name(name),
            Prop::Null => false,
        } {
            dst.draw = Prop::Null;
        }
        let Some(c) = self.cover_common(&dst, role, &[OFFSET_LIFT, OFFSET_HIDDEN])? else {
            return Ok(());
        };
        // Across the lanes, at any height; a destination without height
        // takes one only from a stretch.
        let [xh, _, wh, hh] = c.track.keys.first().map_or([0; 4], |k| k.rect);
        let identity = Stretch::of(dst.stretch) == Stretch::Identity;
        if !self.over_lanes(at, xh, wh)? || hh < 0 || (hh == 0 && identity) {
            return drop(self, "dst");
        }
        let unstretched = self.cover_rect(&c)?;
        let (image, r) = match self.add_cover_image(&c, &dst, rec, &s)? {
            Some(Ok(found)) => found,
            Some(Err(why)) => return self.drop_el(role, at, why),
            None => return Ok(()),
        };
        if r.h <= Q::int(0) {
            return drop(self, "dst");
        }
        let [upper, lower] = self.cover_edges(at, r)?;
        // The trim line, y down; checked within ±MAX_COORD as the edges.
        let max = i128::from(MAX_COORD);
        let trim_y = if line < 0 {
            None
        } else {
            let y = Q::int(line)
                .mul(self.r)
                .and_then(|v| Q::int(HEIGHT).sub(v))
                .filter(|&y| Q::int(-max) <= y && y <= Q::int(max))
                .ok_or_else(|| self.range(at, "disapearLine"))?;
            Some(y)
        };
        let trim = match trim_y {
            None => HiddenTrim::None,
            Some(y) if link => HiddenTrim::Lift(y.to_f32()),
            Some(y) => HiddenTrim::Fixed(y.to_f32()),
        };
        let (kept_after, before) = self.lanes.as_ref().map_or((false, false), |l| {
            let p = &l.def.parts;
            (l.kept_after, p.cover.is_some() || p.lift.is_some())
        });
        if let Some(l) = self.lanes.as_mut() {
            l.def.parts.hidden = Some(image);
            l.def.hidden_at = upper.to_f32();
            l.def.hidden_h = r.h.to_f32();
            l.def.hidden_trim = trim;
        }
        let part = CriticalPart::Lanes;
        if kept_after {
            self.lane_approx(at, part, LaneApprox::CoverOrder)?;
        }
        if before {
            self.lane_approx(at, part, LaneApprox::HiddenOrder)?;
        }
        if trim_y.is_some() && (r.y, r.h) != (unstretched.y, unstretched.h) {
            self.lane_approx(at, part, LaneApprox::HiddenStretch)?;
        }
        // Where the source can draw it over its whole travel: from its
        // upper edge raised by the lane's height to its lower edge at rest
        // or the trim line above it. A trim line, fixed or raised with the
        // lift, at or above its highest upper edge leaves nothing drawn.
        // The model cuts it to the lanes.
        let range = |cx: &Self| cx.range(at, "dst");
        let h_up = Q::int(h0).mul(self.r).ok_or_else(|| range(self))?;
        let bottom = Q::int(y0)
            .mul(self.r)
            .and_then(|v| Q::int(HEIGHT).sub(v))
            .ok_or_else(|| range(self))?;
        let highest = upper.sub(h_up).ok_or_else(|| range(self))?;
        let lowest = trim_y.map_or(lower, |y| y.min(lower));
        let top = self.lanes.as_ref().map_or(0, |l| l.def.top);
        let drawn = trim_y.is_none_or(|y| highest < y);
        if drawn && (highest < Q::int(top) || lowest > bottom) {
            self.lane_approx(at, part, LaneApprox::HiddenClip)?;
        }
        Ok(())
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

    #[test]
    fn floats_are_exact() {
        assert_eq!(f32_q(13.0), Q::new(13, 1));
        assert_eq!(f32_q(12.5), Q::new(25, 2));
        assert_eq!(f32_q(0.1), Q::new(13_421_773, 134_217_728));
        assert_eq!(f32_q(f32::INFINITY), None);
        assert_eq!(f32_q(f32::MAX), None);
    }

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
