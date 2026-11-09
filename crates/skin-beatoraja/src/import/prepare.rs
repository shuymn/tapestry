//! What the source checks before it calls an object's callbacks
//! (design.md "beatoraja Lua Skins", the program): installation, which
//! removes objects before the first frame (`Skin.java:199-233` at
//! `0.8.9`), and each kind's own `prepare`, which may return before its
//! conditions and timer or call a part's only under its own selection.
//!
//! The play program's gating starts here ([`Cx::prepare_gate`]): the
//! chart facts the source prunes by, the kind's gate, then the element's
//! conditions ([`Cx::gating`](super::Cx)). A gate the program cannot write
//! is an [`Unsupported`], and the callback follows its role.
//!
//! | Kind | Gate before its callbacks at `0.8.9` |
//! |---|---|
//! | system and plain image, text, sliders' conditions and timer, gauge, lanes, covers | none |
//! | grouped image, image set | selector not negative (`SkinImage.java:127-132`) |
//! | number | value given and no sentinel (`SkinNumber.java:138-150`) |
//! | a value slider's value | the element drew (`SkinSlider.java:104-113`) |
//! | judge, outer | a judgment exists (`SkinJudge.java:98-103`) |
//! | judge, grade part | its slot selected (`SkinJudge.java:105-116`) |
//! | judge, combo part | its grade drew (`SkinJudge.java:122-126`): not written |

use skin_model::{JudgeSlot, ScreenKind};

use super::ops::{self, Cond};
use super::property::IndexProp;
use super::report::Object;
use super::{Common, Cx, Dst, Op, Prop};
use crate::error::ImportError;
use crate::lua::Unsupported;
use crate::tree::NONE;

/// Why the source removes a destination's object at installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    /// A numeric option not selected, or undeclared (`Skin.java:216-229`).
    Option,
    /// A condition static on play that is false (`Skin.java:204-213`).
    Static,
    /// No authored key (`SkinObject.java:577-578`).
    NoKeys,
    /// An image whose source provably does not load
    /// (`JsonSkinObjectLoader.java:42-98`, `SkinImage.java:95-117`).
    NoSource,
}

/// A destination's installation, as far as the import can tell before it
/// reads the destination.
#[derive(Debug, Default)]
pub(super) struct Install {
    /// Why the source removes its object; `None` when the import cannot
    /// prove it does (installed, or uncertain).
    pub(super) removed: Option<Removal>,
    /// The chart facts the source prunes the object by, as an expression:
    /// constant for an attempt, so a callback gated by it runs on every
    /// play frame or none, as the source keeps or removes the object.
    pub(super) gate: Option<String>,
    /// The elements imported before it: an uncertain destination whose
    /// import adds none is one the source may have removed.
    pub(super) elements: usize,
    /// The slot roots' consumers it compiled, by root and position.
    pub(super) consumers: Vec<(u32, usize)>,
}

/// What a kind's own `prepare` checks before an element's conditions.
#[derive(Clone, Debug, Default)]
pub(super) enum Prepare {
    /// Nothing: a system or plain image, a text, a slider, a gauge, the
    /// lanes, a cover.
    #[default]
    Open,
    /// A grouped image's or image set's selector, `None` when it is proven
    /// never negative.
    Selector(Option<String>),
    /// The judge's outer object.
    JudgeOuter,
    /// A judge's grade part, and whether the source has a part at slot 6.
    JudgeGrade { slot: JudgeSlot, full_part: bool },
    /// A judge's combo part.
    JudgeCombo,
}

impl Prepare {
    /// The gate before an element's conditions, or why the program cannot
    /// write it.
    fn before(&self) -> Result<Option<String>, Unsupported> {
        Ok(match self {
            Self::Open | Self::Selector(None) => None,
            Self::Selector(Some(src)) => Some(format!("({src}) >= 0")),
            Self::JudgeOuter => Some("on(judge)".to_owned()),
            Self::JudgeGrade { slot, full_part } => Some(selected(*slot, *full_part).to_owned()),
            Self::JudgeCombo => {
                return Err(Unsupported::Construct(
                    "a judge combo's callback the program cannot gate",
                ));
            }
        })
    }

    /// Whether a judgment exists whenever its callbacks run, so the score
    /// data the source reads exists ([`super::property::Value::pre_judgment`]).
    fn after_a_judgment(&self) -> bool {
        matches!(
            self,
            Self::JudgeOuter | Self::JudgeGrade { .. } | Self::JudgeCombo
        )
    }
}

/// Whether grade slot `slot` is the one the judge shows, as the model's
/// judge selects it (`widget/judge.rs`, `SkinJudge.java:105-113`): the
/// latest judgment's grade, PERFECT at a full gauge the slot-6 part when
/// the source has one.
fn selected(slot: JudgeSlot, full_part: bool) -> &'static str {
    match slot {
        JudgeSlot::Perfect if full_part => "on(judge.perfect) && !on(gauge_max)",
        JudgeSlot::Perfect => "on(judge.perfect)",
        JudgeSlot::Great => "on(judge.great)",
        JudgeSlot::Good => "on(judge.good)",
        JudgeSlot::Bad => "on(judge.bad)",
        JudgeSlot::Poor => "on(judge.poor)",
        JudgeSlot::EmptyPoor => "on(judge.empty_poor)",
        JudgeSlot::PerfectAtMax => "on(judge.perfect) && on(gauge_max)",
    }
}

impl IndexProp {
    /// Whether its selector is never negative, so a grouped image or an
    /// image set choosing by it never skips its callbacks.
    pub(super) fn non_negative(self) -> bool {
        matches!(
            self,
            Self::Null
                | Self::CoverEnabled
                | Self::LiftEnabled
                | Self::HiddenEnabled
                | Self::RandomOption
                | Self::LaneJudge(_)
        )
    }
}

/// A boolean property's class on play at installation (survey of
/// `BooleanPropertyFactory.java` at `0.8.9`).
enum Static {
    /// Evaluated every frame.
    Dynamic,
    /// Static, and its value is known at import: tapestry has no
    /// difficulty table or course (`table_song`, `:351-352`; `mode_course`,
    /// `:526-527`), so both are false.
    Known(bool),
    /// Static, read from the chart (`SongDataBooleanProperty`, `:251-265`,
    /// `:435-452`), which the play maps.
    Chart,
    /// Static, unmapped by the play: production drops or refuses the
    /// element at it.
    Unmapped,
}

/// The class on play of boolean property `id` (its absolute id).
fn static_class(id: i32) -> Static {
    match id {
        ops::TABLE_SONG | ops::COURSE_MODE => Static::Known(false),
        150..=155 | 170 | 176 | 177 | 180..=184 => Static::Chart,
        171..=175
        | 178
        | 179
        | 1177
        | 160..=164
        | 1160
        | 1161
        | 40
        | 41
        | 190..=195
        | 280..=283
        | 289
        | 50
        | 51
        | 118
        | 119
        | 121
        | 125..=131
        | 1128..=1131 => Static::Unmapped,
        _ => Static::Dynamic,
    }
}

/// The literal skin timers a value slider's gate may name: set only at or
/// before the frame (`play/timers.rs`, `fill_builtin`), so an element on
/// one draws whenever it is on and its keys start at 0.
fn proven_timer(id: i32) -> Option<&'static str> {
    match id {
        40 => Some("ready"),
        41 => Some("play"),
        143 => Some("last_note"),
        _ => None,
    }
}

impl Cx<'_> {
    /// Destination `dst`'s installation on play, its object `object` with
    /// record `orec` (`None` for a system image): removed in the cases the
    /// source proves, else installed or uncertain, with the chart facts it
    /// is pruned by; its sources are checked only with `sources` (an id no
    /// other table names). A keyless judge or note keeps its object, its
    /// authored conditions and timer inert (cleared from `dst`).
    pub(super) fn install(
        &mut self,
        dst: &mut Dst,
        object: Option<(Object, u32)>,
        sources: bool,
    ) -> Result<(), ImportError> {
        self.install = Install {
            elements: self.elements.len(),
            ..Install::default()
        };
        self.prepare = Prepare::Open;
        self.sentinel_read = false;
        if self.kind != ScreenKind::Play {
            return Ok(());
        }
        let removal = self.removal(dst, object, sources)?;
        if removal.is_some() || self.inert(dst, object) {
            self.uncount_timer(&dst.timer);
            if removal.is_none() {
                #[cfg(feature = "dev-tools")]
                {
                    let callables = dst.ops.iter().filter_map(|op| match op {
                        Op::Callable(id) => Some(*id),
                        Op::Id(_) | Op::Name(_) => None,
                    });
                    let props = [&dst.draw, &dst.timer].into_iter().filter_map(|p| match p {
                        Prop::Callable(id) => Some(*id),
                        _ => None,
                    });
                    for id in callables.chain(props).collect::<Vec<_>>() {
                        self.unused(id, crate::lua::callback::Unused::Inert);
                    }
                }
                dst.ops.clear();
                dst.draw = Prop::Null;
                dst.timer = Prop::Null;
            }
        } else {
            self.count_timer_site(&dst.timer);
        }
        self.install.removed = removal;
        Ok(())
    }

    /// Whether `dst` is a judge's or the notes' with no authored key: its
    /// constructor's key keeps the object, and its `op`, `draw` and
    /// `timer`, installed only with an authored key
    /// (`JSONSkinLoader.java:427-463`), are inert
    /// (`play/SkinJudge.java:55`, `play/SkinNote.java:32`).
    fn inert(&self, dst: &Dst, object: Option<(Object, u32)>) -> bool {
        dst.keys.is_empty() && matches!(object, Some((Object::Judge | Object::Note, _)))
    }

    /// A timer callable that is no slot root's site: its reference does
    /// not count either ([`Self::close_slots`]).
    fn uncount_timer(&mut self, timer: &Prop) {
        if let Prop::Callable(id) = *timer
            && let Some((_, inst)) = self.callable_at(id).filter(|&(_, i)| i != NONE)
        {
            *self.removed_refs.entry(inst).or_default() += 1;
        }
    }

    /// Why the source removes `dst`'s object at installation, in the cases
    /// it proves; the chart facts it prunes by join the installation gate.
    fn removal(
        &mut self,
        dst: &Dst,
        object: Option<(Object, u32)>,
        sources: bool,
    ) -> Result<Option<Removal>, ImportError> {
        if dst.keys.is_empty() {
            return Ok((!self.inert(dst, object)).then_some(Removal::NoKeys));
        }
        let mut gate: Vec<String> = Vec::new();
        let stat = |id: i32, negative: bool, gate: &mut Vec<String>| {
            match static_class(id) {
                Static::Known(v) if v == negative => return true,
                Static::Chart => {
                    if let Cond::Expr { src, .. } =
                        ops::numeric(if negative { -id } else { id }, &self.items)
                    {
                        gate.push(src);
                    }
                }
                Static::Known(_) | Static::Dynamic | Static::Unmapped => {}
            }
            false
        };
        for op in &dst.ops {
            let (id, negative) = match op {
                Op::Id(0) | Op::Callable(_) => continue,
                Op::Id(n) => {
                    let id = i32::try_from(n.unsigned_abs()).unwrap_or(i32::MAX);
                    if !ops::is_builtin(id) {
                        // An option: selected for a positive id, not for a
                        // negative one; an undeclared one reads -1.
                        if self.items.get(&id) != Some(&(*n > 0)) {
                            return Ok(Some(Removal::Option));
                        }
                        continue;
                    }
                    (id, *n < 0)
                }
                Op::Name(s) => match ops::resolve_name(s) {
                    Some(p) => p,
                    None => continue,
                },
            };
            if stat(id, negative, &mut gate) {
                return Ok(Some(Removal::Static));
            }
        }
        let draw = match &dst.draw {
            Prop::Id(n) => i32::try_from(n.unsigned_abs())
                .ok()
                .filter(|&id| ops::is_builtin(id))
                .map(|id| (id, *n < 0)),
            Prop::Name(s) => ops::resolve_name(s),
            Prop::Null | Prop::Callable(_) => None,
        };
        if let Some((id, negative)) = draw
            && stat(id, negative, &mut gate)
        {
            return Ok(Some(Removal::Static));
        }
        if let Some((object, rec)) = object.filter(|_| sources)
            && self.no_source(object, rec)?
        {
            return Ok(Some(Removal::NoSource));
        }
        self.install.gate = (!gate.is_empty()).then(|| {
            gate.iter()
                .map(|g| format!("({g})"))
                .collect::<Vec<_>>()
                .join(" && ")
        });
        Ok(None)
    }

    /// Whether image object `object` (record `rec`) provably makes no
    /// image without a file being read: a plain or grouped image whose
    /// source does not resolve, a grouped image whose `len` exceeds its
    /// frames (every run empty), an image set every entry of which is a
    /// hole. Whether a resolved file loads, a movie and every other kind
    /// stay uncertain.
    fn no_source(&mut self, object: Object, rec: u32) -> Result<bool, ImportError> {
        match object {
            Object::Image => {
                let [len, divx, divy] = self.def.fields(rec, ["len", "divx", "divy"]);
                let len = self.int_or(len, "len", 0)?;
                if len > 1 {
                    let div = |v: i32| u64::try_from(v).ok().filter(|&d| d > 0).unwrap_or(1);
                    let frames =
                        div(self.int_or(divx, "divx", 1)?) * div(self.int_or(divy, "divy", 1)?);
                    if frames / u64::try_from(len).unwrap_or(1) == 0 {
                        return Ok(true);
                    }
                }
                self.unresolved(rec)
            }
            Object::ImageSet => {
                let [images] = self.def.fields(rec, ["images"]);
                let Some(list) = images.filter(|&l| self.def.arena.is_array(l)) else {
                    return Ok(false);
                };
                // Its entries, bounded as the import charges them: past the
                // cap the import refuses it, so nothing is decided here.
                let n = self.def.arena.children(list).count();
                let used = usize::try_from(self.budget.usage.choice_refs).unwrap_or(usize::MAX);
                if n > skin_model::MAX_IMAGE_CHOICE_REFS.saturating_sub(used) {
                    return Ok(false);
                }
                let items: Vec<u32> = self.def.arena.children(list).collect();
                for item in items {
                    let Some(name) = self.def.string(item, "images")? else {
                        continue;
                    };
                    let Some(img) = self.image_rec(&name)? else {
                        continue;
                    };
                    if !self.unresolved(img)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Whether image record `rec`'s source does not resolve without a file
    /// being read: no `source` record of its `src`, one without a `path`,
    /// or a file choice of none.
    pub(super) fn unresolved(&mut self, rec: u32) -> Result<bool, ImportError> {
        let [src] = self.def.fields(rec, ["src"]);
        let src = self.opt_string(src, "src")?;
        let Some(source) = self.source_rec(src.as_deref())? else {
            return Ok(true);
        };
        let Some(path) = self.source_path(source)? else {
            return Ok(true);
        };
        Ok(matches!(
            self.choices.substitute(&path),
            super::asset::Path::Missing
        ))
    }

    /// At a destination's end: a slot root's consumer on one whose import
    /// retained no element counts as a dropped destination's, since the
    /// source may have removed it ([`Self::close_slots`]).
    pub(super) fn installed(&mut self) {
        if self.elements.len() > self.install.elements {
            return;
        }
        for (inst, at) in std::mem::take(&mut self.install.consumers) {
            if let Some(c) = self
                .slot_roots
                .get_mut(&inst)
                .and_then(|r| r.consumers.get_mut(at))
            {
                c.kept = false;
            }
        }
    }

    /// The gate every program callback of the element being read starts
    /// with, after its value: the installation gate, then its kind's; or
    /// why the program cannot gate it. A callback after a read the source
    /// sees as a pre-judgment sentinel follows its role, but on a judge.
    pub(super) fn prepare_gate(&self) -> Result<Vec<String>, Unsupported> {
        if self.sentinel_read && !self.prepare.after_a_judgment() {
            return Err(Unsupported::Construct(
                "a program callback behind a pre-judgment sentinel",
            ));
        }
        Ok(self
            .install
            .gate
            .iter()
            .cloned()
            .chain(self.prepare.before()?)
            .collect())
    }

    /// A value slider's value gate, for its element `c` timed by its
    /// destination's `timer`: its conditions while it draws, which the
    /// program writes for a held end whose first key is at 0, untimed or
    /// on READY, PLAY or the last note; any other shape needs the source's
    /// prepare clock, and the value follows its role.
    pub(super) fn slider_gate(
        &self,
        c: &Common,
        timer: &Prop,
    ) -> Result<(Option<String>, Option<u16>), Unsupported> {
        let held =
            c.track.end == skin_model::End::Hold && c.track.keys.first().is_some_and(|k| k.at == 0);
        if !held {
            return Err(Unsupported::Construct(
                "a slider value whose keys need the source's prepare clock",
            ));
        }
        let mut conds = c.conds.clone();
        match *timer {
            Prop::Null => {}
            Prop::Id(n) if n < 0 => {}
            Prop::Id(n) => match proven_timer(n) {
                Some(t) => conds.push(format!("on({t})")),
                None => {
                    return Err(Unsupported::Construct(
                        "a slider value on a timer the program cannot gate",
                    ));
                }
            },
            Prop::Name(_) | Prop::Callable(_) => {
                return Err(Unsupported::Construct(
                    "a slider value on a timer the program cannot gate",
                ));
            }
        }
        self.gating(&conds, true)
    }

    /// Marks a read of a value the source sees as a pre-judgment sentinel:
    /// a program callback after it in the element follows its role.
    pub(super) fn read_sentinel(&mut self, read: bool) {
        self.sentinel_read |= read;
    }
}
