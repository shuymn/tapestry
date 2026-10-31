//! The lanes (`note`), SUDDEN+ (an image listing offset 4), LIFT
//! (`liftCover`) and HIDDEN+ (`hiddenCover`).
//!
//! The source draws the lanes from eight lane rects
//! (`JsonPlaySkinObjectLoader.java:79-93`, `play/LaneRenderer.java:
//! 288-727` at `0.8.9`); the model draws them as one widget whose parts
//! are the notes, the bar line, SUDDEN+ and LIFT. SUDDEN+ and LIFT map
//! only in a narrow subset: static, white, exactly over the lanes, on the
//! lanes' own timer, conditions and window. HIDDEN+ is never drawn
//! (owner decision 15, D11).

use bms_model::TimeUs;
use skin_model::{
    Blend, Ease, End, Filter, KeyDef, LaneParts, LanesDef, Side, Source, TimerRef, Track,
};

use super::super::geom::{HEIGHT, Offset, Q};
use super::super::policy::{self, CriticalPart, Role};
use super::super::report::{DeviationId, LaneApprox, NoteKind, Severity, Span};
use super::super::track::{self, Accepted, Stretch};
use super::super::{Common, Cx, Dst, OFFSET_LANECOVER, OFFSET_LIFT, Opts, RUNTIME_OFFSETS};
use super::SrcImage;
use crate::error::ImportError;
use crate::tree::Scalar;

/// The lanes the source draws.
const LANES: usize = 8;

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
    /// Where SUDDEN+'s lower edge sits at 0 %: the source's lane top.
    cover_at: i32,
    timer: TimerRef,
    cond_src: String,
    window: Window,
    /// Some user offset moves the lanes.
    moved: bool,
    /// An element was kept after the notes.
    pub(crate) kept_after: bool,
}

impl LaneState {
    /// The model's lanes: SUDDEN+ anchored at the source's lane top when
    /// it maps, LIFT at the lanes' bottom when it maps.
    pub(crate) fn def(&self) -> LanesDef {
        let mut def = self.def;
        if def.parts.cover.is_some() {
            def.cover_at = self.cover_at;
        }
        if def.parts.lift.is_some() {
            def.lift_at = def.bottom;
        }
        def
    }
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

    /// Notes lane approximation `a` at `at` for `part`, or refuses it
    /// when the owner does not accept it.
    pub(in super::super) fn lane_approx(
        &mut self,
        at: Span,
        part: CriticalPart,
        a: LaneApprox,
    ) -> Result<(), ImportError> {
        #[cfg(test)]
        if REJECTED.with(std::cell::Cell::get) == Some(a) {
            return self.lane_approx_with(at, part, a, |_| false);
        }
        self.lane_approx_with(at, part, a, policy::lane_approx_accepted)
    }

    fn lane_approx_with(
        &mut self,
        at: Span,
        part: CriticalPart,
        a: LaneApprox,
        accepted: fn(LaneApprox) -> bool,
    ) -> Result<(), ImportError> {
        let kind = NoteKind::Deviation(DeviationId::D9(a));
        if !accepted(a) {
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
        let list = |i: usize| lists.get(i).cloned().unwrap_or_default();
        let [note, lnstart, lnend, lnbody, lnbody_active, lnactive] = [0, 1, 2, 3, 4, 5].map(list);
        let hcn: Vec<Vec<Option<String>>> = (6..15).map(list).collect();
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
        let hcn_len = |i: usize| hcn.get(i).map_or(0, Vec::len);
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
            || [&lnstart, &on, &off].iter().any(|l| l.len() < tail)
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
        let Some(c) = self.common_with(
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

        // The parts, each its lane's image in the image table.
        let parts_c = Common {
            role,
            span: at,
            track: c.track.clone(),
            ease: Ease::Linear,
            timer: c.timer,
            visible: None,
            blend: Blend::Alpha,
            filter: Filter::Nearest,
            pivot: c.pivot,
            offset: Offset::default(),
            stretch: Stretch::Identity,
            stretch_mode: -1,
            untimed: c.untimed,
            cond_src: String::new(),
        };
        let mut cells = [[0_u32; 2]; LANES];
        let mut images = [[0_u32; 6]; LANES];
        let names: [(&Vec<Option<String>>, &'static str); 6] = [
            (&note, "note"),
            (&lnstart, "lnstart"),
            (&lnend, "lnend"),
            (&on, "lnbody"),
            (&off, "lnbody"),
            (&mine, "mine"),
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
                let Some((image, cell)) = self.add_src_image(&parts_c, img, &s, s.div)? else {
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
        let mut heights = Vec::with_capacity(LANES);
        for (u, cell) in cells.iter().enumerate() {
            let nh = match sizes.get(u) {
                Some(&v) => f32_q(v).and_then(|q| q.mul(self.r)),
                None => Q::int(cell[1]).mul(self.r),
            }
            .and_then(|q| q.add(Q::int(o.h)))
            .ok_or_else(|| self.range(at, "size"))?;
            heights.push(nh);
        }
        let rounded: Vec<i128> = heights
            .iter()
            .map(|h| h.round())
            .collect::<Option<_>>()
            .ok_or_else(|| self.range(at, "size"))?;
        let Some(&note_h) = rounded.first() else {
            return refuse(self, "size");
        };
        if rounded.len() != LANES || rounded.iter().any(|&h| h != note_h) || note_h < 1 {
            return refuse(self, "size");
        }
        let note_h = u16::try_from(note_h).map_err(|_| self.range(at, "size"))?;

        // Vertical (y down): the line and the top half a note above the
        // lanes' bottom and top (`play/LaneRenderer.java:324-326`).
        let [_, y0, _, h0] = lane_rects.first().map_or([0; 4], |(_, r)| *r);
        let range = |cx: &Self| cx.range(at, "dst");
        let nh = heights.first().copied().unwrap_or(Q::ZERO);
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
        let cover_at = base
            .sub(h_up)
            .and_then(Q::round)
            .ok_or_else(|| range(self))?;
        let i32_of = |cx: &Self, v: i128| i32::try_from(v).map_err(|_| range(cx));
        let [top, line, bottom, cover_at] = [
            i32_of(self, top)?,
            i32_of(self, line)?,
            i32_of(self, bottom)?,
            i32_of(self, cover_at)?,
        ];

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
        let kn = c.track.keys.last().map_or(0, |k| k.at);
        let mut times = vec![0];
        if kn > 0 {
            times.push(kn);
        }
        let keys: Vec<KeyDef> = times
            .iter()
            .map(|&t| KeyDef {
                at: TimeUs(t * 1000),
                props: [0, 0, 0, 0, 255, 255, 255, 255, 0].map(Some),
            })
            .collect();
        let t = Track::new(&keys, Ease::Linear, c.track.end).map_err(|_| range(self))?;
        let state = LaneState {
            def: LanesDef {
                lanes,
                scratch,
                top,
                line,
                bottom,
                cover_at: top,
                lift_at: line,
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
            cover_at,
            timer: c.timer,
            cond_src: c.cond_src.clone(),
            window: window(&c.track),
            moved,
            kept_after: false,
        };
        self.push(c, Source::Lanes, t);
        self.lanes = Some(state);

        // What the source draws that the model does not.
        for (field, list) in [("hidden", &hidden), ("processed", &processed)] {
            if !list.is_empty() {
                self.ignored(at, NoteKind::Ignored { field });
            }
        }
        if hcn.iter().any(|l| !l.is_empty()) {
            self.ignored(at, NoteKind::Ignored { field: "hcn" });
        }
        let [bpm, stop, time] = self.def.fields(rec, ["bpm", "stop", "time"]);
        for (field, f) in [("bpm", bpm), ("stop", stop), ("time", time)] {
            if f.and_then(|f| self.tables.nested.get(&f))
                .is_some_and(|l| !l.is_empty())
            {
                self.ignored(at, NoteKind::Ignored { field });
            }
        }
        for a in [
            LaneApprox::TopCrop,
            LaneApprox::LateTaps,
            LaneApprox::EarlyTaps,
            LaneApprox::Mines,
            LaneApprox::LnTail,
            LaneApprox::LnBody,
        ] {
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
    fn floats(&mut self, field: Option<u32>, name: &'static str) -> Result<Vec<f32>, ImportError> {
        let Some(f) = field else {
            return Ok(Vec::new());
        };
        if self.def.arena.node(f).kind != crate::tree::Kind::Array {
            return Err(self.def.type_error(f, name, "an array"));
        }
        let items: Vec<u32> = self.def.arena.children(f).collect();
        let mut out = Vec::with_capacity(items.len());
        for i in items {
            #[expect(clippy::cast_possible_truncation, reason = "the source's float field")]
            let v = match self.def.arena.scalar(i) {
                Scalar::Long(raw) => raw.parse::<i64>().ok().map(|v| v as f32),
                Scalar::Double(raw) => raw.parse::<f64>().ok().map(|v| v as f32),
                _ => None,
            };
            out.push(v.ok_or_else(|| self.def.type_error(i, name, "a number"))?);
        }
        Ok(out)
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
        let (ow, oxw) = (Q::int(o.w), Q::int(o.x));
        let mut hull: Option<(Q, Q)> = None;
        for k in &keys.keys {
            let [x, _, w, _] = k.rect;
            let x = Q::int(x)
                .mul(self.r)
                .and_then(|v| v.add(oxw)?.sub(ow.half()?))
                .ok_or_else(|| range(self))?;
            let w = Q::int(w)
                .mul(self.r)
                .and_then(|v| v.add(ow))
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
        let Some(c) = self.cover_common(&dst, part, &[OFFSET_LANECOVER])? else {
            return Ok(());
        };
        let Some(l) = self.lanes.as_ref() else {
            return refuse(self, "offset");
        };
        // Exactly over the lanes, its lower edge on their top.
        let [xc, yc, wc, hc] = c.track.keys.first().map_or([0; 4], |k| k.rect);
        if !self.over_lanes(at, xc, wc)?
            || i64::from(yc) != i64::from(l.y0) + i64::from(l.h0)
            || hc != l.h0
        {
            return refuse(self, "dst");
        }
        let Some((image, _)) = self.add_src_image(&c, rec, s, s.div)? else {
            return refuse(self, "src");
        };
        let kept_after = self.lanes.as_ref().is_some_and(|l| l.kept_after);
        if let Some(l) = self.lanes.as_mut() {
            l.def.parts.cover = Some(image);
        }
        self.lane_approx(at, part, LaneApprox::CoverAbove)?;
        if kept_after {
            self.lane_approx(at, part, LaneApprox::CoverOrder)?;
        }
        Ok(())
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

    /// SUDDEN+'s or LIFT's common part, refused unless it is static,
    /// white, opaque, alpha-blended, unrotated, unstretched, not moved by
    /// any user offset, and shown exactly when the lanes are.
    fn cover_common(
        &mut self,
        dst: &Dst,
        part: CriticalPart,
        allow: &[i32],
    ) -> Result<Option<Common>, ImportError> {
        let at = dst.span;
        let Some(c) = self.common_with(
            Role::Critical(part),
            dst,
            Opts {
                allow,
                charged: part == CriticalPart::Cover,
                ..Opts::default()
            },
        )?
        else {
            return Ok(None);
        };
        let Some(l) = self.lanes.as_ref() else {
            return Ok(None);
        };
        let still = c.track.identical()
            && c.track
                .keys
                .first()
                .is_some_and(|k| k.color == [255; 4] && k.angle == 0)
            && c.blend == Blend::Alpha
            && dst.blend != 3
            && Stretch::of(dst.stretch) == Stretch::Identity
            && c.offset == Offset::default()
            && !l.moved;
        let same = c.timer == l.timer && c.cond_src == l.cond_src && window(&c.track) == l.window;
        if !(still && same) {
            return Err(ImportError::Critical {
                at: self.loc(at),
                part,
                why: NoteKind::UnsupportedField { field: "dst" },
            });
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
        let Some(c) = self.cover_common(&dst, part, &[OFFSET_LIFT])? else {
            return Ok(());
        };
        let [xl, yl, wl, hl] = c.track.keys.first().map_or([0; 4], |k| k.rect);
        if !self.over_lanes(at, xl, wl)?
            || hl <= 0
            || i64::from(yl) + i64::from(hl) != i64::from(y0)
        {
            return refuse(self, "dst");
        }
        let Some((image, _)) = self.add_src_image(&c, rec, &s, s.div)? else {
            return refuse(self, "src");
        };
        let kept_after = self.lanes.as_ref().is_some_and(|l| l.kept_after);
        if let Some(l) = self.lanes.as_mut() {
            l.def.parts.lift = Some(image);
        }
        self.lane_approx(at, part, LaneApprox::LiftStretch)?;
        if kept_after {
            self.lane_approx(at, part, LaneApprox::CoverOrder)?;
        }
        Ok(())
    }

    /// A `hiddenCover` destination: never drawn (owner decision 15, D11).
    /// Nothing of it is read or admitted.
    pub(in super::super) fn hidden_cover(&mut self, at: Span) {
        self.note(
            Some(at),
            Severity::Ignored,
            NoteKind::Deviation(DeviationId::D11),
        );
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
    super::super::ops::java_int(id).is_some_and(|n| n < 0)
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
}
