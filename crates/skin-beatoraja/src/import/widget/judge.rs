//! The judge (`JsonPlaySkinObjectLoader.java:226-291`, `play/SkinJudge.
//! java` at `0.8.9`): a grade image per slot and the combo beside it.
//!
//! The source keeps the grades of slots 0–6 and the combos of slots 0–2,
//! with PERFECT's standing in for an empty full-gauge slot 6, each on its
//! own (`SkinJudge.java:98-113`); the model's judge widget resolves its
//! slots the same way. The combo is a relative number: its keys are moved
//! left by half its width times its digits before inheritance, its user
//! offsets change only its size, and it sits on the grade's bottom edge.

use bms_model::TimeUs;
use skin_model::{
    Align, Attach, ComboPart, DigitsDef, Ease, GlyphSet, GradePart, JudgeDef, JudgeSlot, KeyDef,
    MAX_COORD, Pad, Source, Track,
};

use super::super::geom::{Q, Rect};
use super::super::policy::{self, CriticalPart, Role};
use super::super::report::{NoteKind, Span, TimerWhy};
use super::super::track::{self, Stretch};
use super::super::{Common, Cx, Dst, Opts};
use super::number;
use crate::error::ImportError;

#[cfg(test)]
thread_local! {
    /// Each judge's retained keys this thread imported: the outer track's,
    /// slot 0's combo's and slot 0's grade's.
    pub(crate) static RETAINED: std::cell::RefCell<Vec<[usize; 3]>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn retained(def: &JudgeDef, outer: &Track) -> [usize; 3] {
    let combo = def.combos[0].as_ref().map_or(0, |c| c.track.key_count());
    let grade = def.grades[0].as_ref().map_or(0, |g| g.track.key_count());
    [outer.key_count(), combo, grade]
}

/// The slots whose combo the source draws (`SkinJudge.java:104-111`).
const COMBO_SLOTS: [usize; 4] = [0, 1, 2, 6];

impl Cx<'_> {
    /// A model track of neutral keys at 0 and at the last of `c`'s keys.
    pub(super) fn neutral_track(&self, c: &Common) -> Result<Track, ImportError> {
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
        Track::new(&keys, Ease::Linear, c.track.end).map_err(|_| self.range(c.span, "dst"))
    }

    /// A judge destination.
    pub(in super::super) fn judge(&mut self, dst: Dst, rec: u32) -> Result<(), ImportError> {
        let at = dst.span;
        let part = CriticalPart::Judge;
        let role = Role::Critical(part);
        let refuse = |cx: &Self, at: Span, why: NoteKind| {
            Err(ImportError::Critical {
                at: cx.loc(at),
                part,
                why,
            })
        };
        let field = |field| NoteKind::UnsupportedField { field };
        let [index, images, numbers, shift] = self
            .def
            .fields(rec, ["index", "images", "numbers", "shift"]);
        // A second judge splits the lanes among the judges
        // (`JudgeManager.java:676-689`).
        if self.int_or(index, "index", 0)? != 0 {
            return refuse(self, at, field("index"));
        }
        if self.judges.contains(&rec) {
            return refuse(self, at, field("judge"));
        }
        self.judges.push(rec);
        let nested = |cx: &Self, f: Option<u32>| {
            f.and_then(|f| cx.tables.nested.get(&f))
                .cloned()
                .unwrap_or_default()
        };
        let images = nested(self, images);
        let numbers = nested(self, numbers);
        let shift = match shift {
            Some(f) => self.def.bool(f, "shift")?,
            None => false,
        };
        // The source reads `numbers[i]` for every image while the skin
        // has numbers (`JsonPlaySkinObjectLoader.java:244-245`).
        let has_values = !super::super::object::records(&self.tables, "value").is_empty();
        if numbers.len() < images.len() && has_values {
            return refuse(self, at, field("numbers"));
        }
        // The source compares every image's id, and every number's for
        // each image, with the skin's records, and fails on a missing one
        // (`JsonPlaySkinObjectLoader.java:230-232, 243-244`).
        let has_images = !super::super::object::records(&self.tables, "image").is_empty();
        let lists = [
            (&images[..], has_images, "images"),
            (
                numbers.get(..images.len()).unwrap_or(&numbers),
                has_values,
                "numbers",
            ),
        ];
        for (list, compared, name) in lists {
            for &r in list {
                if compared && super::super::object::id_of(self.def, r)?.is_none() {
                    return refuse(self, self.arena_span(r), field(name));
                }
            }
        }

        // The outer destination: lifetime, timer and conditions.
        let Some(c) = self.common_with(
            role,
            &dst,
            Opts {
                neutral: true,
                no_offsets: true,
                ..Opts::default()
            },
        )?
        else {
            return Ok(());
        };
        if c.untimed && !policy::untimed_ok(true, true, c.track.end, true) {
            return refuse(self, at, NoteKind::Timer(TimerWhy::Untimed));
        }

        let mut grades: [Option<GradePart>; JudgeSlot::COUNT] = Default::default();
        let mut moved = [false; JudgeSlot::COUNT];
        let mut stretch_mode = [0; JudgeSlot::COUNT];
        if images.len() > JudgeSlot::COUNT {
            self.ignored(at, NoteKind::Ignored { field: "images" });
        }
        for (k, &g) in images.iter().enumerate().take(JudgeSlot::COUNT) {
            let (id, gdst) = self.read_nested(g)?;
            let Some(img) = id.map(|id| self.image_rec(&id)).transpose()?.flatten() else {
                continue;
            };
            stretch_mode[k] = gdst.stretch;
            if let Some((grade, anchor_moved)) = self.grade(&gdst, img)? {
                grades[k] = Some(grade);
                moved[k] = anchor_moved;
            }
        }
        if grades.iter().all(Option::is_none) {
            self.ignored(at, NoteKind::Ignored { field: "judge" });
            return Ok(());
        }

        let mut combos: [Option<ComboPart>; JudgeSlot::COUNT] = Default::default();
        for (k, &n) in numbers.iter().enumerate().take(images.len()) {
            let (id, ndst) = self.read_nested(n)?;
            let Some(value) = id.map(|id| self.value_rec(&id)).transpose()?.flatten() else {
                continue;
            };
            if !COMBO_SLOTS.contains(&k) {
                self.ignored(ndst.span, NoteKind::Ignored { field: "numbers" });
                continue;
            }
            if let Some(combo) = self.combo(&ndst, value)? {
                combos[k] = Some(combo);
            }
        }

        // The source places the combo at the grade's region, the model at
        // the grade's sampled rect: they agree only while the grade's
        // stretch keeps its left and bottom edges.
        let full = if grades[6].is_some() { 6 } else { 0 };
        let combo_at_full = combos[6].is_some() || combos[0].is_some();
        let pairs = [
            (0, combos[0].is_some()),
            (1, combos[1].is_some()),
            (2, combos[2].is_some()),
            (full, combo_at_full),
        ];
        for (g, combo) in pairs {
            if combo && grades[g].is_some() && moved[g] {
                return refuse(self, at, NoteKind::Stretch(stretch_mode[g]));
            }
        }

        let def = JudgeDef {
            grades,
            combos,
            shift,
        };
        let quads = quads(&def);
        self.budget
            .widget_quads(quads)
            .map_err(|l| self.limit(at, l))?;
        let t = self.neutral_track(&c)?;
        let index = u32::try_from(self.widgets.judges.len()).unwrap_or(u32::MAX);
        #[cfg(test)]
        RETAINED.with(|r| r.borrow_mut().push(retained(&def, &t)));
        self.widgets.judges.push(def);
        self.push(c, Source::Judge(index), t);
        Ok(())
    }

    /// The `value` record of id `id`, the first of it
    /// (`JsonPlaySkinObjectLoader.java:247`).
    fn value_rec(&mut self, id: &str) -> Result<Option<u32>, ImportError> {
        let recs = super::super::object::records(&self.tables, "value").to_vec();
        for rec in recs {
            if super::super::object::id_of(self.def, rec)?.as_deref() == Some(id) {
                return Ok(Some(rec));
            }
        }
        Ok(None)
    }

    /// A grade image and whether its stretch moves its left or bottom
    /// edge; `None` when it is never drawn.
    fn grade(&mut self, gdst: &Dst, img: u32) -> Result<Option<(GradePart, bool)>, ImportError> {
        let at = gdst.span;
        let role = Role::Critical(CriticalPart::Judge);
        if Stretch::of(gdst.stretch) == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(gdst.stretch))?;
        }
        let s = self.src_image(img)?;
        let Some(c) = self.common(role, gdst, false)? else {
            return Ok(None);
        };
        let Some((image, cell)) = self.add_src_image(&c, img, &s, s.div)? else {
            return Ok(None);
        };
        let mut rects = Vec::with_capacity(c.track.keys.len());
        let mut branches = Vec::with_capacity(c.track.keys.len());
        let mut moved = false;
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            let (st, b) =
                track::stretch(c.stretch, r, cell).ok_or_else(|| self.range(at, "dst"))?;
            branches.push(b);
            let plain = r.model().ok_or_else(|| self.range(at, "dst"))?;
            let m = st.model().ok_or_else(|| self.range(at, "dst"))?;
            moved |= plain[0] != m[0] || plain[1] + plain[3] != m[1] + m[3];
            rects.push(m);
        }
        if !track::one_branch(branches) {
            self.drop_el(role, at, NoteKind::Stretch(c.stretch_mode))?;
        }
        let track = self.model_track(&c, &rects)?;
        Ok(Some((
            GradePart {
                image,
                track,
                timer: c.timer,
                pivot: c.pivot,
                blend: c.blend,
                visible: c.visible,
            },
            moved,
        )))
    }

    /// A combo strip; `None` when it shows nothing.
    fn combo(&mut self, ndst: &Dst, value: u32) -> Result<Option<ComboPart>, ImportError> {
        let at = ndst.span;
        let role = Role::Critical(CriticalPart::Judge);
        let [digit, space, offset] = self.def.fields(value, ["digit", "space", "offset"]);
        if self.digit_offsets(offset)? {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "offset" })?;
        }
        let digit = self.int_or(digit, "digit", 0)?;
        let spaces = self.int_or(space, "space", 0)?;
        let s = self.src_image(value)?;
        let frames = self.grid_frames(at, s.div)?;
        // `JsonPlaySkinObjectLoader.java:250, 258-259`.
        let (set, pad, d) = if frames.is_multiple_of(10) {
            (GlyphSet::Ten, Pad::Blank, 10)
        } else {
            (GlyphSet::Eleven, Pad::Glyph, 11)
        };
        let sets = frames / d;
        if sets == 0 || digit <= 0 {
            self.ignored(at, NoteKind::Ignored { field: "numbers" });
            return Ok(None);
        }
        let Some(grid) = number::glyphs(sets * d, frames, s.div) else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "divx" })?;
            return Ok(None);
        };
        let Some((cells, extra)) = number::cells(digit, pad) else {
            self.drop_el(role, at, NoteKind::UnsupportedField { field: "digit" })?;
            return Ok(None);
        };
        if Stretch::of(ndst.stretch) == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(ndst.stretch))?;
        }
        let Some(c) = self.common_with(
            role,
            ndst,
            Opts {
                digits: Some(digit),
                ..Opts::default()
            },
        )?
        else {
            return Ok(None);
        };
        let Some((image, cell)) = self.add_src_image(&c, value, &s, grid)? else {
            return Ok(None);
        };

        // Layout, relative: from the grade's left and bottom edges, the
        // extra cells' shift halved for the centre alignment.
        let range = |cx: &Self| cx.range(at, "dst");
        let sr = Q::int(spaces).mul(self.r).ok_or_else(|| range(self))?;
        let e = Q::int(extra);
        let mut keys = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            let rect = Rect::scaled(k.rect, self.r)
                .and_then(|r| r.offset_relative(c.offset))
                .ok_or_else(|| range(self))?;
            let (stretched, _) =
                track::stretch(c.stretch, rect, cell).ok_or_else(|| range(self))?;
            if stretched != rect {
                self.drop_el(role, at, NoteKind::Stretch(c.stretch_mode))?;
            }
            let p = rect.w.add(sr).ok_or_else(|| range(self))?;
            let x = p
                .mul(e)
                .and_then(Q::half)
                .and_then(|shift| rect.x.add(shift))
                .ok_or_else(|| range(self))?;
            keys.push((x, rect, p));
        }
        let same = keys
            .windows(2)
            .all(|w| matches!(w, [(_, a, pa), (_, b, pb)] if a.w == b.w && pa == pb));
        let space = match (same, keys.first()) {
            (true, Some((_, r0, p0))) => p0
                .round()
                .zip(r0.w.round())
                .and_then(|(p, w)| p.checked_sub(w)),
            _ => sr.round(),
        }
        .ok_or_else(|| range(self))?;
        if !(-i128::from(MAX_COORD)..=i128::from(MAX_COORD)).contains(&space) {
            return Err(range(self));
        }
        let mut rects = Vec::with_capacity(keys.len());
        for (x, rect, _) in &keys {
            let w = rect.w.round().ok_or_else(|| range(self))?;
            let top = Q::ZERO
                .sub(rect.y.add(rect.h).ok_or_else(|| range(self))?)
                .and_then(Q::round)
                .ok_or_else(|| range(self))?;
            let bottom = Q::ZERO
                .sub(rect.y)
                .and_then(Q::round)
                .ok_or_else(|| range(self))?;
            let left = x.round().ok_or_else(|| range(self))?;
            let v = super::super::geom::coords([left, top, w, bottom - top])
                .ok_or_else(|| range(self))?;
            rects.push(v);
        }
        let track = self.model_track(&c, &rects)?;
        Ok(Some(ComboPart {
            digits: DigitsDef {
                image,
                set,
                cells: u8::try_from(cells).unwrap_or(u8::MAX),
                align: Align::Center,
                pad,
                space: i32::try_from(space).map_err(|_| range(self))?,
            },
            track,
            timer: c.timer,
            attach: Attach::Bottom,
            pivot: c.pivot,
            blend: c.blend,
            visible: c.visible,
        }))
    }
}

/// The quads one frame of `def` can push: over its effective states, the
/// grade and its combo's cells (the model's own count).
fn quads(def: &JudgeDef) -> u32 {
    let grade = |s: usize| {
        def.grades[s]
            .as_ref()
            .or(if s == 6 { def.grades[0].as_ref() } else { None })
    };
    let combo = |s: usize| {
        def.combos[s]
            .as_ref()
            .or(if s == 6 { def.combos[0].as_ref() } else { None })
    };
    (0..JudgeSlot::COUNT)
        .filter(|&s| grade(s).is_some())
        .map(|s| 1 + combo(s).map_or(0, |c| u32::from(c.digits.cells)))
        .max()
        .unwrap_or(0)
}
