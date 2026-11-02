//! The note record's part arrays and note heights, checked before any
//! part is resolved, admitted or generated.
//!
//! The lanes draw six parts per lane from the arrays the source selects
//! (`JsonPlaySkinObjectLoader.java:46-77` at `0.8.9`): `note`,
//! `lnstart`, `lnend`, the two selected bodies and `mine`. Each must be
//! an array with an id for each of the eight lanes; a missing, `null`,
//! short or malformed one, or a lane's `null` or non-id entry, refuses
//! with its field and index before anything is loaded. Entries past the
//! lanes and the arrays the lanes never draw are not checked, where the
//! source throws on some of them. An entry that names no image is not
//! refused: the source draws generated paint for it ([`super::lanes`]).
//! Only the eight lanes' entries are kept, as node ids.

use super::super::Cx;
use super::super::geom::Q;
use super::super::policy::CriticalPart;
use super::super::report::{NoteKind, PartWhy, Span};
use crate::error::ImportError;
use crate::tree::{Kind, Scalar};

/// The lanes the source draws.
pub(super) const LANES: usize = 8;

/// The part arrays, in the record's field order.
const FIELDS: [&str; 18] = [
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
];

const NOTE: usize = 0;
const LNSTART: usize = 1;
const LNEND: usize = 2;
const LNBODY: usize = 3;
const LNBODY_ACTIVE: usize = 4;
const LNACTIVE: usize = 5;
const MINE: usize = 15;
const HIDDEN: usize = 16;
const PROCESSED: usize = 17;

/// One array as scanned: its node, its length and its first eight
/// entries' nodes.
#[derive(Clone, Copy, Default)]
struct Scan {
    node: Option<u32>,
    len: u32,
    first: [u32; LANES],
}

/// The eight lanes' part entries, in source lane order, each the node of
/// the id the lane draws: note, long-note start, end, active body,
/// inactive body, mine.
pub(super) struct NotePartsPlan {
    pub(super) nodes: [[u32; 6]; LANES],
    /// The field each part reads: the body pair follows `lnbodyActive`.
    pub(super) fields: [&'static str; 6],
    /// `hidden` and `processed` are non-empty: the model does not draw
    /// them.
    pub(super) hidden: bool,
    pub(super) processed: bool,
    /// An HCN array is non-empty: its paint is drawn as a CN's.
    pub(super) hcn: bool,
}

impl Cx<'_> {
    /// A refusal of the lanes for note part `field`, at `node` or, when
    /// the field is missing, at the record `at`.
    pub(super) fn part_error(
        &self,
        at: Span,
        node: Option<u32>,
        field: &'static str,
        why: PartWhy,
    ) -> ImportError {
        let at = node.map_or(at, |n| self.arena_span(n));
        ImportError::Critical {
            at: self.loc(at),
            part: CriticalPart::Lanes,
            why: NoteKind::NotePart { field, why },
        }
    }

    /// Part array `i` of record `rec` scanned in place. One the lanes
    /// draw (`drawn`) must be an array whose lanes' entries are ids; any
    /// other is only counted, an array's entries or else none. `hidden`
    /// and `processed` keep their own reading: a type error for anything
    /// but an array of strings or `null`s.
    fn scan_part(
        &self,
        at: Span,
        i: usize,
        node: Option<u32>,
        drawn: bool,
    ) -> Result<Scan, ImportError> {
        let field = FIELDS.get(i).copied().unwrap_or("note");
        let lenient = i == HIDDEN || i == PROCESSED;
        let Some(f) = node else {
            return Ok(Scan::default());
        };
        let arena = &self.def.arena;
        if arena.node(f).kind != Kind::Array {
            if lenient {
                return Err(self.def.type_error(f, field, "an array"));
            }
            if !drawn {
                return Ok(Scan::default());
            }
            let why = if matches!(arena.scalar(f), Scalar::Null) {
                PartWhy::Null
            } else {
                PartWhy::NotArray
            };
            return Err(self.part_error(at, Some(f), field, why));
        }
        let mut scan = Scan {
            node: Some(f),
            ..Scan::default()
        };
        let lanes = u32::try_from(LANES).unwrap_or(u32::MAX);
        for (index, c) in (0_u32..).zip(arena.children(f)) {
            let why = match arena.scalar(c) {
                Scalar::Object | Scalar::Array if lenient => {
                    return Err(self.def.type_error(c, field, "a string"));
                }
                _ if !drawn || index >= lanes => None,
                Scalar::Object | Scalar::Array => Some(PartWhy::BadEntry { index }),
                Scalar::Null => Some(PartWhy::NullEntry { index }),
                _ => None,
            };
            if let Some(why) = why {
                return Err(self.part_error(at, Some(c), field, why));
            }
            if let Some(slot) = usize::try_from(index)
                .ok()
                .and_then(|i| scan.first.get_mut(i))
            {
                *slot = c;
            }
            scan.len = index.saturating_add(1);
        }
        Ok(scan)
    }

    /// Record `rec`'s part arrays checked, in field order: each array the
    /// lanes draw well formed through the lanes, `hidden` and `processed`
    /// as their reading says; then each drawn array present with eight
    /// entries, exactly eight for `note`. The bodies are `lnbodyActive`
    /// and `lnbody` when `lnbodyActive` is a non-empty array, else
    /// `lnbody` and `lnactive`. A missing array is located at the record.
    pub(super) fn note_parts(&self, rec: u32) -> Result<NotePartsPlan, ImportError> {
        let at = self.arena_span(rec);
        let nodes = self.def.fields(rec, FIELDS);
        let node = |i: usize| nodes.get(i).copied().flatten();
        let field = |i: usize| FIELDS.get(i).copied().unwrap_or("note");

        // The selected bodies (`JsonPlaySkinObjectLoader.java:46-77`).
        let ln_active = self
            .scan_part(at, LNBODY_ACTIVE, node(LNBODY_ACTIVE), false)?
            .len
            > 0;
        let (on, off) = if ln_active {
            (LNBODY_ACTIVE, LNBODY)
        } else {
            (LNBODY, LNACTIVE)
        };
        let parts = [NOTE, LNSTART, LNEND, on, off, MINE];
        let mut scans = [Scan::default(); FIELDS.len()];
        for (i, scan) in scans.iter_mut().enumerate() {
            *scan = self.scan_part(at, i, node(i), parts.contains(&i))?;
        }
        let scan = |i: usize| scans.get(i).copied().unwrap_or_default();
        let need = |cx: &Self, i: usize| {
            let s = scan(i);
            let Some(node) = s.node else {
                return Err(cx.part_error(at, None, field(i), PartWhy::Missing));
            };
            let need = u32::try_from(LANES).unwrap_or(u32::MAX);
            if s.len < need || (i == NOTE && s.len != need) {
                let why = PartWhy::Length { len: s.len, need };
                return Err(cx.part_error(at, Some(node), field(i), why));
            }
            Ok(())
        };
        for i in parts {
            need(self, i)?;
        }

        let mut plan = NotePartsPlan {
            nodes: [[0; 6]; LANES],
            fields: parts.map(field),
            hidden: scan(HIDDEN).len > 0,
            processed: scan(PROCESSED).len > 0,
            hcn: (6..15).any(|i| scan(i).len > 0),
        };
        for (u, lane) in plan.nodes.iter_mut().enumerate() {
            for (slot, &p) in lane.iter_mut().zip(&parts) {
                *slot = scan(p).first.get(u).copied().unwrap_or_default();
            }
        }
        Ok(plan)
    }

    /// The note heights `size` gives, per source lane, exactly
    /// ([`Q::decimal`]): the array streamed once, every entry a number
    /// and only the first eight kept, so no more than eight are held
    /// however long it is. An unused entry is checked as a number only;
    /// a used one the rationals cannot hold is a range error.
    pub(super) fn note_sizes(&self, field: Option<u32>) -> Result<[Option<Q>; LANES], ImportError> {
        let mut out = [None; LANES];
        let Some(f) = field else {
            return Ok(out);
        };
        let arena = &self.def.arena;
        if arena.node(f).kind != Kind::Array {
            return Err(self.def.type_error(f, "size", "an array"));
        }
        for (i, c) in arena.children(f).enumerate() {
            let (Scalar::Long(raw) | Scalar::Double(raw)) = arena.scalar(c) else {
                return Err(self.def.type_error(c, "size", "a number"));
            };
            if let Some(slot) = out.get_mut(i) {
                let q = Q::decimal(raw).ok_or_else(|| self.range(self.arena_span(c), "size"))?;
                *slot = Some(q);
            }
        }
        Ok(out)
    }
}
