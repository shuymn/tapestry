//! The digit offset tables of the `value` records (`Value.offset`, a
//! `Value[]`, `JsonSkin.java:138-156`), resolved once per record with the
//! wrappers of an array field of records (`JsonSkinSerializer.java:43,
//! 79, 220-268, 278-323`).
//!
//! Each row is a position: a wrapper inserts or removes rows, and a row
//! the resolution selects but cannot read refuses the import rather
//! than vanishing and moving the rows after it. So, beyond the generic
//! array: a record choice that selects nothing where a row is required,
//! a row that is no object, and a coordinate that is not an exact `i32`
//! refuse (an intentional policy: the source keeps partial or default
//! rows, or fails as it reads them). An absent or `null` table is empty,
//! a false array-level condition adds no row, and an empty object `{}`
//! is a zero row. Only `x`, `y`, `w` and `h` are read, as the source
//! copies only them (`JsonSkinObjectLoader.java:127-136, 157-166`).
//!
//! Every row is charged against the expansion count and
//! [`MAX_RESOLVED_DIGIT_OFFSETS`] before it is decoded or stored. The rows
//! of all tables share one buffer, and a record whose table holds rows
//! keeps their range in an index; an empty table keeps nothing.

use std::collections::HashMap;
use std::ops::Range;

use super::Resolver;
use crate::error::{ImportError, InputLimit};
use crate::limits::MAX_RESOLVED_DIGIT_OFFSETS;
use crate::tree::{Kind, NONE, Scalar};

/// One resolved row as written: its node, for locations, and its `x`,
/// `y`, `w` and `h`, each 0 when absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawDigitOffset {
    pub(crate) node: u32,
    pub(crate) xywh: [i32; 4],
}

/// The resolved rows of every `value` record's table.
#[derive(Debug, Default)]
pub(crate) struct DigitOffsetTables {
    rows: Vec<RawDigitOffset>,
    /// By value record: its rows' range in `rows`, for a nonempty table.
    index: HashMap<u32, Range<u32>>,
}

impl DigitOffsetTables {
    /// The rows of value record `rec`, in order; none for an empty table.
    pub(crate) fn of(&self, rec: u32) -> &[RawDigitOffset] {
        self.index
            .get(&rec)
            .and_then(|r| self.rows.get(r.start as usize..r.end as usize))
            .unwrap_or_default()
    }

    /// The rows resolved across every table.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

/// The field each refusal names.
const FIELD: &str = "offset";

impl Resolver<'_, '_> {
    /// The `offset` table of each value record in `values` at `depth`,
    /// into `out`; a record resolved before is not resolved again.
    pub(super) fn digit_offsets(
        &mut self,
        values: &[u32],
        depth: u32,
        out: &mut DigitOffsetTables,
    ) -> Result<(), ImportError> {
        for &rec in values {
            if out.index.contains_key(&rec) {
                continue;
            }
            let Some(table) = self.def.fields(rec, [FIELD])[0] else {
                continue;
            };
            let start = out.rows.len();
            self.offset_table(table, depth + 1, out)?;
            let end = out.rows.len();
            if end > start {
                // Each bound is at most the row cap, far inside `u32`.
                let r = u32::try_from(start).unwrap_or(u32::MAX)
                    ..u32::try_from(end).unwrap_or(u32::MAX);
                out.index.insert(rec, r);
            }
        }
        Ok(())
    }

    /// A table at `depth`: an included array, an array's items, an object
    /// as one row, `null` as none.
    fn offset_table(
        &mut self,
        id: u32,
        depth: u32,
        out: &mut DigitOffsetTables,
    ) -> Result<(), ImportError> {
        if let Some(member) = self.arena().first_member_ci(id, "include") {
            return self.include(member, depth, |r, root, d| r.offset_table(root, d, out));
        }
        match self.arena().node(id).kind {
            Kind::Array => {
                let mut next = self.arena().node(id).first_child;
                while next != NONE {
                    let item = next;
                    next = self.arena().node(item).next_sibling;
                    self.offset_item(item, depth + 1, out)?;
                }
                Ok(())
            }
            Kind::Object => self.offset_row(id, out),
            Kind::Quoted | Kind::Bare => match self.arena().scalar(id) {
                Scalar::Null => Ok(()),
                _ => Err(self.def.type_error(id, FIELD, "an array")),
            },
        }
    }

    /// One item of a table, as [`Resolver::item`] reads it, except that a
    /// row it selects must be read.
    fn offset_item(
        &mut self,
        item: u32,
        depth: u32,
        out: &mut DigitOffsetTables,
    ) -> Result<(), ImportError> {
        let a = self.arena();
        let cond = a.first_member_ci(item, "if");
        let value = a.first_member_ci(item, "value");
        let values = a.first_member_ci(item, "values");
        if let Some(cond) = cond.filter(|_| value.is_some() || values.is_some()) {
            if self.test(Some(cond))? {
                if let Some(v) = value {
                    let rec = self.offset_record(v, depth + 1)?;
                    self.offset_row(rec, out)?;
                }
                if let Some(vs) = values {
                    self.offset_table(vs, depth + 1, out)?;
                }
            }
            return Ok(());
        }
        if let Some(member) = self.arena().first_member_ci(item, "include") {
            return self.include(member, depth, |r, root, d| r.offset_table(root, d, out));
        }
        let rec = self.offset_record(item, depth)?;
        self.offset_row(rec, out)
    }

    /// The record `id` gives where a row is required: a choice that
    /// selects nothing refuses.
    fn offset_record(&mut self, id: u32, depth: u32) -> Result<u32, ImportError> {
        self.record(id, depth)?
            .ok_or_else(|| self.def.type_error(id, FIELD, "a row"))
    }

    /// Row `rec`: counted and charged, then read and stored.
    fn offset_row(&mut self, rec: u32, out: &mut DigitOffsetTables) -> Result<(), ImportError> {
        self.count(rec)?;
        if out.rows.len() >= MAX_RESOLVED_DIGIT_OFFSETS {
            return Err(ImportError::Input {
                at: Some(self.def.loc(rec)),
                limit: InputLimit::DigitOffsets,
            });
        }
        if self.arena().node(rec).kind != Kind::Object {
            return Err(self.def.type_error(rec, FIELD, "an object"));
        }
        let fields = self.def.fields(rec, ["x", "y", "w", "h"]);
        let mut xywh = [0; 4];
        for ((v, f), name) in xywh.iter_mut().zip(fields).zip(["x", "y", "w", "h"]) {
            if let Some(f) = f {
                *v = self.def.exact_int(f, name)?;
            }
        }
        out.rows.push(RawDigitOffset { node: rec, xywh });
        Ok(())
    }
}
