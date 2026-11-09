//! The judge (`JsonPlaySkinObjectLoader.java:226-291`, `play/SkinJudge.
//! java` at `0.8.9`): a grade image per slot and the combo beside it.
//!
//! The source keeps the grades of slots 0–6 and the combos of slots 0–2,
//! with PERFECT's standing in for an empty full-gauge slot 6, each on its
//! own (`SkinJudge.java:98-113`); the model's judge widget resolves its
//! slots the same way. The combo is a relative number: its keys are moved
//! left by half their raw width times its digits, exactly, its user
//! offsets change only its size, and it sits on the bottom edge of the
//! grade's destination before its stretch, as the source attaches it to
//! the grade's region and stretches only a copy at drawing
//! (`SkinJudge.java:115-125`, `skin/SkinObject.java:649-667`): a grade
//! whose stretch moves that edge or its left one keeps the unstretched
//! keys as its attachment track, when a combo pairs with it.

use bms_model::TimeUs;
use skin_model::{
    Align, Attach, ComboPart, DigitsDef, Ease, GlyphSet, GradePart, JudgeDef, JudgeSlot, KeyDef,
    Pad, Source, Track,
};

use super::super::geom::{Q, Rect};
use super::super::policy::{CriticalPart, Role};
use super::super::report::{NoteKind, Span};
use super::super::track::{self, Stretch};
use super::super::{Common, Cx, Dst, Opts};
use super::number;
use crate::error::ImportError;
use crate::import::prepare::Prepare;

#[cfg(test)]
thread_local! {
    /// Each judge's retained keys this thread imported: the outer track's,
    /// slot 0's combo's, slot 0's grade's and slot 0's grade attachment's.
    pub(crate) static RETAINED: std::cell::RefCell<Vec<[usize; 4]>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn retained(def: &JudgeDef, outer: &Track) -> [usize; 4] {
    let combo = def.combos[0].as_ref().map_or(0, |c| c.track.key_count());
    let grade = def.grades[0].as_ref().map_or(0, |g| g.track.key_count());
    let attachment = def.grades[0]
        .as_ref()
        .and_then(|g| g.attachment.as_ref())
        .map_or(0, Track::key_count);
    [outer.key_count(), combo, grade, attachment]
}

/// The slots whose combo the source draws (`SkinJudge.java:104-111`).
const COMBO_SLOTS: [usize; 4] = [0, 1, 2, 6];

/// A grade admitted before the combos.
struct PendingGrade {
    part: GradePart,
    /// Its accepted destination, kept while its stretch moves the left or
    /// bottom edge, to build its attachment track should a combo pair
    /// with it; its visibility already moved into `part`.
    attachment_source: Option<Common>,
}

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
        // The outer destination: lifetime, timer and conditions, its
        // callbacks run only once a judgment exists
        // (`SkinJudge.java:98-103`).
        self.prepare = Prepare::JudgeOuter;
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

        // Whether the source has a slot-6 part, which takes PERFECT at a
        // full gauge from slot 0 (`SkinJudge.java:107-109`,
        // `JsonPlaySkinObjectLoader.java:230-239`): as far as it is known
        // without reading the file.
        let full_part = match images.get(JudgeSlot::PerfectAtMax.index()) {
            Some(&g) => match self.read_nested(g)?.0 {
                Some(id) => match self.image_rec(&id)? {
                    Some(img) => !self.unresolved(img)?,
                    None => false,
                },
                None => false,
            },
            None => false,
        };
        let mut grades: [Option<PendingGrade>; JudgeSlot::COUNT] = Default::default();
        if images.len() > JudgeSlot::COUNT {
            self.ignored(at, NoteKind::Ignored { field: "images" });
        }
        // A drawn slot's grade, and the combo of a slot that draws one
        // beside it, must name its record; the other entries are not read.
        for (k, &g) in images.iter().enumerate().take(JudgeSlot::COUNT) {
            let (id, gdst) = self.read_nested(g)?;
            let Some(id) = id else {
                return refuse(self, self.arena_span(g), field("images"));
            };
            let Some(img) = self.image_rec(&id)? else {
                continue;
            };
            if let Some(slot) = grades.get_mut(k) {
                // Only the selected grade prepares (`SkinJudge.java:105-116`).
                if let Some(&s) = JudgeSlot::ALL.get(k) {
                    self.prepare = Prepare::JudgeGrade { slot: s, full_part };
                }
                *slot = self.grade(&gdst, img)?;
            }
        }
        if grades.iter().all(Option::is_none) {
            self.ignored(at, NoteKind::Ignored { field: "judge" });
            return Ok(());
        }

        let mut combos: [Option<ComboPart>; JudgeSlot::COUNT] = Default::default();
        for (k, &n) in numbers.iter().enumerate().take(images.len()) {
            if !COMBO_SLOTS.contains(&k) {
                // Never drawn, so not read: only a value it names is noted.
                if let Ok(Some(id)) = super::super::object::id_of(self.def, n)
                    && self.value_rec(&id)?.is_some()
                {
                    self.ignored(self.arena_span(n), NoteKind::Ignored { field: "numbers" });
                }
                continue;
            }
            let (id, ndst) = self.read_nested(n)?;
            let Some(id) = id else {
                return refuse(self, self.arena_span(n), field("numbers"));
            };
            let Some(value) = self.value_rec(&id)? else {
                continue;
            };
            // Prepared only when its grade drew (`SkinJudge.java:122-126`).
            self.prepare = Prepare::JudgeCombo;
            if let Some(combo) = self.combo(&ndst, value)? {
                combos[k] = Some(combo);
            }
        }

        // The source places the combo at the grade's region before its
        // stretch: each grade an effective state pairs with a combo, the
        // full-gauge slot's each falling back to PERFECT's, gets its
        // unstretched keys as its attachment when its stretch moved them.
        let full = if grades[6].is_some() { 6 } else { 0 };
        let combo_at_full = combos[6].is_some() || combos[0].is_some();
        let pairs = [
            (0, combos[0].is_some()),
            (1, combos[1].is_some()),
            (2, combos[2].is_some()),
            (full, combo_at_full),
        ];
        for (g, combo) in pairs {
            if let Some(p) = grades.get_mut(g).and_then(Option::as_mut)
                && combo
                && let Some(c) = p.attachment_source.take()
            {
                p.part.attachment = Some(self.grade_attachment(&c)?);
            }
        }
        let grades = grades.map(|g| g.map(|g| g.part));

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
        self.push(c, Source::Judge(index), t)?;
        Ok(())
    }

    /// The `value` record of id `id`, the first of it
    /// (`JsonPlaySkinObjectLoader.java:247`).
    fn value_rec(&mut self, id: &str) -> Result<Option<u32>, ImportError> {
        for &rec in super::super::object::records(&self.tables, "value") {
            if super::super::object::id_of(self.def, rec)?.as_deref() == Some(id) {
                return Ok(Some(rec));
            }
        }
        Ok(None)
    }

    /// A grade image, with its destination kept when its stretch moves its
    /// left or bottom edge; `None` when it is never drawn.
    fn grade(&mut self, gdst: &Dst, img: u32) -> Result<Option<PendingGrade>, ImportError> {
        let at = gdst.span;
        let role = Role::Critical(CriticalPart::Judge);
        if Stretch::of(gdst.stretch) == Stretch::Trimmed {
            self.drop_el(role, at, NoteKind::Stretch(gdst.stretch))?;
        }
        let s = self.src_image(img)?;
        let o = Opts {
            follow: true,
            ..Opts::default()
        };
        let Some(mut c) = self.common_with(role, gdst, o)? else {
            return Ok(None);
        };
        let Some((image, cell)) = self.add_src_image(&c, img, &s, s.div)? else {
            return Ok(None);
        };
        // Stretch 8 and 9 apply at placement, inside the destination the
        // combo attaches to (`play/SkinJudge.java:124`), so the model's
        // runtime fit keeps the attachment.
        let native = c.stretch.native(cell);
        let mut rects = Vec::with_capacity(c.track.keys.len());
        let mut shared = track::ANY_BRANCH;
        let mut moved = false;
        for k in &c.track.keys {
            let r = self.key_rect(&c, k)?;
            let (st, b) = if native.is_some() {
                (r, 0b1)
            } else {
                track::stretch(c.stretch, r, cell).ok_or_else(|| self.range(at, "dst"))?
            };
            shared &= b;
            let plain = r.model().ok_or_else(|| self.range(at, "dst"))?;
            let m = st.model().ok_or_else(|| self.range(at, "dst"))?;
            moved |= plain[0] != m[0] || plain[1] + plain[3] != m[1] + m[3];
            rects.push(m);
        }
        if !track::one_branch(shared) {
            self.drop_el(role, at, NoteKind::Stretch(c.stretch_mode))?;
        }
        self.retained(&c)?;
        let track = self.model_track(&c, &rects)?;
        let part = GradePart {
            image,
            track,
            timer: c.timer,
            pivot: c.pivot,
            blend: c.blend,
            visible: c.visible.take(),
            native,
            follow: c.follow,
            attachment: None,
        };
        Ok(Some(PendingGrade {
            part,
            attachment_source: moved.then_some(c),
        }))
    }

    /// A grade's attachment track: its destination's keys before the
    /// stretch, each edge rounded on its own, timed as its paint. Its keys
    /// are charged before anything is built.
    fn grade_attachment(&mut self, c: &Common) -> Result<Track, ImportError> {
        let n = c.track.keys.len();
        self.budget
            .grade_anchor_keys(n)
            .map_err(|l| self.limit(c.span, l))?;
        let mut rects = Vec::with_capacity(n);
        for k in &c.track.keys {
            let r = self.key_rect(c, k)?;
            rects.push(r.model().ok_or_else(|| self.range(c.span, "dst"))?);
        }
        self.model_track(c, &rects)
    }

    /// A combo strip; `None` when it shows nothing.
    fn combo(&mut self, ndst: &Dst, value: u32) -> Result<Option<ComboPart>, ImportError> {
        let at = ndst.span;
        let role = Role::Critical(CriticalPart::Judge);
        let [digit, space] = self.def.fields(value, ["digit", "space"]);
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
                relative: true,
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
        // Stretch 8 and 9 apply per glyph at placement, after its digit
        // offset (`skin/SkinNumber.java:191-205`).
        let native = c.stretch.native(cell);
        let rows_kept = c.stretch == Stretch::Identity
            || self.digit_rows_keep_aspect(value, extra, cells, cell);
        let mut keys = Vec::with_capacity(c.track.keys.len());
        for k in &c.track.keys {
            // The raw `x` moved left by half the raw `w` times the digits
            // (`JsonPlaySkinObjectLoader.java:268-270`), exactly: only the
            // final edges round.
            let [_, _, raw_w, _] = k.rect;
            let rect = Rect::scaled(k.rect, self.r)
                .and_then(|r| {
                    let shift = Q::int(raw_w).mul(Q::int(digit))?.half()?.mul(self.r)?;
                    Some(Rect {
                        x: r.x.sub(shift)?,
                        ..r
                    })
                })
                .and_then(|r| r.offset_relative(c.offset))
                .ok_or_else(|| range(self))?;
            let (stretched, _) =
                track::stretch(c.stretch, rect, cell).ok_or_else(|| range(self))?;
            if native.is_none() && (stretched != rect || !rows_kept) {
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
        let space = number::cell_space(&keys, sr).ok_or_else(|| range(self))?;
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
        self.retained(&c)?;
        let offsets = self.digit_offsets(at, value, extra, cells)?;
        Ok(Some(ComboPart {
            digits: DigitsDef {
                image,
                set,
                cells: u8::try_from(cells).unwrap_or(u8::MAX),
                align: Align::Center,
                pad,
                space: i32::try_from(space).map_err(|_| range(self))?,
                offsets,
            },
            track,
            timer: c.timer,
            attach: Attach::Bottom,
            pivot: c.pivot,
            blend: c.blend,
            visible: c.visible,
            native,
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
