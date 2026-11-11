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

use super::report::{NoteKind, Object, ScheduleWhy, Severity};
use super::{Common, Cx, Dst, Op, Prop};
use crate::error::ImportError;
use crate::lua::{Disposition, Unsupported};
use crate::tree::NONE;

/// The reason an inert authored callback is not compiled.
const INERT: &str = "an authored `op`, `draw` or `timer` inert without an authored key";

/// Where an image record's source leads, as the source resolves its path
/// (`JSONSkinLoader.java:497-501`, `SkinLoader.java:95-130`): the file
/// choice substituted first, then the resulting name classified.
pub(super) enum Resolved {
    /// No `source` record of its `src`, one without a `path`, a file
    /// choice of none, or a pattern no file matches (the source keeps the
    /// raw `*` path, which does not exist).
    Absent,
    /// A movie by its resolved name, or a pattern whose matches all are
    /// movies: the source makes a movie object, before it reads `len`
    /// (`JsonSkinObjectLoader.java:44-49`). Its texture is unknown unless
    /// the file is missing: the source's texture getter decodes the file
    /// whatever its suffix, unless a movie object of it was made first
    /// (`JsonSkinObjectLoader.java:561-582`).
    Movie(Option<String>),
    /// A pattern some file matches, or one the import cannot prove
    /// unmatched (its listing skipped an entry the source would see): the
    /// source picks a match at random.
    Matched,
    /// A file, its name resolved.
    File(String),
}

/// Whether an image record's texture loads, as the source needs one
/// (`JsonSkinObjectLoader.java:561-582`, `SkinLoader.java:136-178`: a
/// whole decode, `null` on any failure, and none for a movie).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Load {
    /// It decodes.
    Loads,
    /// It does not, by evidence the import fully sees: no source, a file
    /// choice of none, a pattern whose whole folder matches nothing, a
    /// missing file, or a file that does not decode.
    Absent,
    /// The import cannot tell: a random pick among matches, a movie by its
    /// name that is there, a file only the importer refuses, or a file not
    /// decoded here.
    Unknown,
}

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
    /// Its program callbacks: node, field and callback.
    pub(super) compiled: Vec<(u32, &'static str, u16)>,
    /// Whether the source installs its object whatever its assets: a
    /// system image, a judge or the notes, which validate by their keys
    /// alone, with no condition the import cannot decide.
    pub(super) certain: bool,
    /// Whether its selector is gated by an availability the source does
    /// not have ([`ScheduleWhy::Availability`]).
    pub(super) availability: bool,
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
    /// A judge's grade part, and whether the source has a part at slot 6
    /// (`None`: the import cannot tell).
    JudgeGrade {
        slot: JudgeSlot,
        full_part: Option<bool>,
    },
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
            Self::JudgeGrade { slot, full_part } => match (slot, full_part) {
                (_, Some(full)) => Some(selected(*slot, *full).to_owned()),
                // Which part a full gauge's PERFECT selects is unknown.
                (JudgeSlot::Perfect | JudgeSlot::PerfectAtMax, None) => {
                    return Err(Unsupported::Construct(
                        "a judge whose slot-6 texture the import cannot prove",
                    ));
                }
                (_, None) => Some(selected(*slot, false).to_owned()),
            },
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
                | Self::FixHispeed
                | Self::AutoAdjust
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
    /// `:526-527`; the course stages, `:521-525`), so all are false.
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
        ops::TABLE_SONG | ops::COURSE_MODE | 280..=283 | 289 => Static::Known(false),
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
        self.install.certain = removal.is_none()
            && matches!(object, None | Some((Object::Judge | Object::Note, _)))
            && !self.undecided(dst);
        if removal.is_some() || self.inert(dst, object) {
            self.uncount_timer(&dst.timer);
            if removal.is_none() {
                // Each inert use recorded on its own, its callable's other
                // uses left alone.
                let callables = dst.ops.iter().filter_map(|op| match op {
                    Op::Callable(id) => Some((*id, "op")),
                    Op::Id(_) | Op::Name(_) => None,
                });
                let props = [(&dst.draw, "draw"), (&dst.timer, "timer")]
                    .into_iter()
                    .filter_map(|(p, f)| match p {
                        Prop::Callable(id) => Some((*id, f)),
                        _ => None,
                    });
                for (id, field) in callables.chain(props).collect::<Vec<_>>() {
                    let why = Unsupported::Construct(INERT);
                    let role = super::policy::Role::Decorative;
                    self.consumed(
                        id,
                        field,
                        role,
                        (Disposition::Replaced(why), Some(why)),
                        (None, None),
                    );
                    #[cfg(feature = "dev-tools")]
                    self.unused(id, crate::lua::callback::Unused::Inert);
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

    /// Whether one of `dst`'s conditions is static on play but unmapped:
    /// the source may remove the object by it.
    fn undecided(&self, dst: &Dst) -> bool {
        let numeric = dst.ops.iter().filter_map(|op| match op {
            Op::Id(n) => Some(*n),
            Op::Name(_) | Op::Callable(_) => None,
        });
        let named = dst
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Name(s) => ops::resolve_name(s).map(|(id, _)| id),
                Op::Id(_) | Op::Callable(_) => None,
            })
            .chain(match &dst.draw {
                Prop::Name(s) => ops::resolve_name(s).map(|(id, _)| id),
                Prop::Id(n) => Some(*n),
                Prop::Null | Prop::Callable(_) => None,
            });
        numeric
            .chain(named)
            .filter_map(|n| i32::try_from(n.unsigned_abs()).ok())
            .any(|id| ops::is_builtin(id) && matches!(static_class(id), Static::Unmapped))
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
            && self.no_source(object, rec, dst.span)?
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
    fn no_source(
        &mut self,
        object: Object,
        rec: u32,
        at: super::Span,
    ) -> Result<bool, ImportError> {
        match object {
            Object::Image => {
                match self.resolve_source(rec, at)? {
                    Resolved::Absent => return Ok(true),
                    // A movie is an object whatever its `len`; a random pick
                    // may be one.
                    Resolved::Movie(_) | Resolved::Matched => return Ok(false),
                    Resolved::File(_) => {}
                }
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
                Ok(self.texture(rec, false, at)? == Load::Absent)
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
                    // An entry is a hole when its texture is absent, a movie
                    // included (`JsonSkinObjectLoader.java:80-85`).
                    if self.texture(img, false, at)? != Load::Absent {
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
    /// Where image record `rec`'s source leads, for the element at `at`.
    fn resolve_source(&mut self, rec: u32, at: super::Span) -> Result<Resolved, ImportError> {
        let [src] = self.def.fields(rec, ["src"]);
        let src = self.opt_string(src, "src")?;
        let Some(source) = self.source_rec(src.as_deref())? else {
            return Ok(Resolved::Absent);
        };
        let Some(path) = self.source_path(source)? else {
            return Ok(Resolved::Absent);
        };
        self.resolve_path(&path, at)
    }

    /// Where source path `path` leads, the file choice substituted before
    /// the name is classified; the one resolution the installation pass
    /// and the placement read ([`Self::src_rect`]).
    pub(super) fn resolve_path(
        &mut self,
        path: &str,
        at: super::Span,
    ) -> Result<Resolved, ImportError> {
        use super::asset::{self, Path};
        Ok(match self.choices.substitute(path) {
            Path::Missing => Resolved::Absent,
            Path::File(p) if asset::is_movie(&p) => {
                Resolved::Movie(Some(asset::resolve(&self.entry_dir, &p)))
            }
            Path::File(p) => Resolved::File(asset::resolve(&self.entry_dir, &p)),
            // Every match ends with the pattern's tail.
            Path::Glob if asset::is_movie(path) => Resolved::Movie(None),
            Path::Glob => match self.glob_matches(path, at)? {
                Some(false) => Resolved::Absent,
                Some(true) | None => Resolved::Matched,
            },
        })
    }

    /// Whether some file matches pattern `path` as the source picks one
    /// (`SkinLoader.java:107-127`): a name in its folder ending, lowercased,
    /// with the tail after its last `*` (its `|` part dropped). `None` when
    /// no match is found but the import cannot prove there is none: the
    /// listing skipped an entry the source would see (a link out of the
    /// folder, a name that is not UTF-8), or the folder could not be read.
    /// Each folder is listed once, charged to the definition's folder
    /// entries ([`crate::limits::MAX_LIST_ENTRIES`], T08); past them the
    /// import stops.
    fn glob_matches(&mut self, path: &str, at: super::Span) -> Result<Option<bool>, ImportError> {
        let Some(star) = path.rfind('*') else {
            return Ok(None);
        };
        let mut tail = path.get(star + 1..).unwrap_or("").to_owned();
        if let Some(bar) = path.find('|') {
            let head = path.get(star + 1..bar).unwrap_or("");
            let rest = path
                .rfind('|')
                .and_then(|b| path.get(b + 1..))
                .unwrap_or("");
            tail = format!("{head}{rest}");
        }
        let folder = path
            .get(..path.get(..star).and_then(|h| h.rfind('/')).unwrap_or(0))
            .unwrap_or("");
        let dir = super::asset::resolve(&self.entry_dir, folder);
        if !self.listings.contains_key(&dir) {
            let remaining = crate::limits::MAX_LIST_ENTRIES.saturating_sub(self.def.run.entries);
            let listed = match self.def.root().list(&dir, remaining) {
                Ok(listing) => {
                    self.def.run.entries += listing.enumerated;
                    let skipped = listing.enumerated > listing.entries.len();
                    Some((
                        listing.entries.into_iter().map(|e| e.name).collect(),
                        skipped,
                    ))
                }
                Err(skin_model::RootError::Missing | skin_model::RootError::NotAFolder) => {
                    Some((Vec::new(), false))
                }
                Err(skin_model::RootError::TooManyEntries { .. }) => {
                    return Err(ImportError::Input {
                        at: Some(self.loc(at)),
                        limit: crate::error::InputLimit::ListEntries,
                    });
                }
                Err(_) => None,
            };
            self.listings.insert(dir.clone(), listed);
        }
        Ok(match self.listings.get(&dir) {
            Some(Some((names, skipped))) => {
                let found = names.iter().any(|n| n.to_lowercase().ends_with(&tail));
                if found || !*skipped {
                    Some(found)
                } else {
                    None
                }
            }
            Some(None) | None => None,
        })
    }

    /// Whether image record `rec`'s texture loads: the one decision the
    /// installation pass and a judge's slot 6 read, from the source's
    /// resolution ([`Self::resolve_path`]). A file is unknown unless
    /// `decode`: the installation pass reads no file, since the import
    /// charges an element before it admits a file and admits none for an
    /// element it drops. With `decode` a file is admitted, as its own
    /// import would (cached for it): missing, it is absent; refused only by
    /// this importer, unknown; admitted, it loads once a whole decode
    /// proves it (the verdict cached), else it is absent.
    pub(super) fn texture(
        &mut self,
        rec: u32,
        decode: bool,
        at: super::Span,
    ) -> Result<Load, ImportError> {
        let (name, movie) = match self.resolve_source(rec, at)? {
            Resolved::Absent => return Ok(Load::Absent),
            Resolved::Matched | Resolved::Movie(None) => return Ok(Load::Unknown),
            Resolved::File(_) | Resolved::Movie(_) if !decode => return Ok(Load::Unknown),
            Resolved::File(name) => (name, false),
            Resolved::Movie(Some(name)) => (name, true),
        };
        let def = skin_model::FileDef {
            path: name,
            hi: None,
        };
        let admitted = self
            .assets
            .admit(&mut self.budget, def)
            .map_err(|l| self.limit(at, l))?;
        Ok(match admitted {
            Err(super::asset::Failed::File { refused: false }) => Load::Absent,
            // A movie that is there: the texture getter may decode it, or
            // find a movie object of it made first.
            _ if movie => Load::Unknown,
            Err(_) => Load::Unknown,
            Ok(file) => {
                let def = &*self.def;
                match self.assets.validate(file, &|| def.is_cancelled()) {
                    Ok(()) => Load::Loads,
                    Err(super::asset::Invalid::Fails) => Load::Absent,
                    Err(super::asset::Invalid::Refused) => Load::Unknown,
                    Err(super::asset::Invalid::Cancelled) => {
                        return Err(crate::resolve::Definition::cancellation(Some(self.loc(at))));
                    }
                }
            }
        })
    }

    /// At a destination's end: on one whose import retained no element,
    /// which the source may have removed, a slot root's consumer counts as
    /// a dropped destination's ([`Self::close_slots`]) and every program
    /// callback compiled is noted as run where the source may not; on an
    /// element whose selector waits for an availability the source does
    /// not have, each is noted as skipped where the source runs it.
    pub(super) fn installed(&mut self) {
        let retained = self.elements.len() > self.install.elements || self.install.certain;
        let why = if !retained && self.install.removed.is_none() {
            Some(ScheduleWhy::MayBeRemoved)
        } else if self.install.availability {
            Some(ScheduleWhy::Availability)
        } else {
            None
        };
        for (id, field, k) in std::mem::take(&mut self.install.compiled) {
            if let Some(why) = why {
                let at = self.arena_span(id);
                self.schedule
                    .push((k, at, NoteKind::Schedule { field, why }));
            }
        }
        if retained {
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
        // The source's first key, before D20 settles a critical track.
        let held = c.track.end == skin_model::End::Hold && c.first_at == 0;
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

    /// The schedule notes, once the slot roots are closed: a callback
    /// whose gate [`Self::close_slots`] made false never runs, so it has
    /// none.
    pub(super) fn schedule_notes(&mut self, never: &std::collections::HashSet<u16>) {
        for (k, at, note) in std::mem::take(&mut self.schedule) {
            if !never.contains(&k) {
                self.note(Some(at), Severity::Deviation, note);
            }
        }
    }

    /// Marks a read of a value the source sees as a pre-judgment sentinel:
    /// a program callback after it in the element follows its role.
    pub(super) fn read_sentinel(&mut self, read: bool) {
        self.sentinel_read |= read;
    }
}
