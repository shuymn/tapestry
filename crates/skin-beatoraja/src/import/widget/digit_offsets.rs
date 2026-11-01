//! Digit offsets: a `value`'s `offset` rows lowered to the model's
//! per-cell paint deltas, for numbers and judge combos alike.
//!
//! The source draws cell `j` of a `D`-cell strip with row `j` added to
//! its rect after its layout, in window pixels and y up, unscaled by the
//! destination (`skin/SkinNumber.java:196-205`, the rows copied by
//! `JsonSkinObjectLoader.java:127-136, 157-166`); a combo gets the same
//! rows (`JsonPlaySkinObjectLoader.java:258-276`). The model's deltas
//! are target pixels too, y down, so a row `(x, y, w, h)` is
//! `(x, −y − h, w, h)`: growing the height moves the top edge. Of a
//! strip whose leading `E` blank cells the model drops, retained cell `j`
//! takes row `E + j`; trailing zero rows are dropped.

use skin_model::{DigitOffset, MAX_COORD};

use super::super::Cx;
use super::super::report::Span;
use crate::error::ImportError;
use crate::resolve::digit_offsets::RawDigitOffset;

/// Row `r` lowered; `None` past the model's coordinates.
fn lower(r: &RawDigitOffset) -> Option<DigitOffset> {
    let [x, y, w, h] = r.xywh.map(i64::from);
    let max = i64::from(MAX_COORD);
    let coord = |v: i64| {
        (-max..=max)
            .contains(&v)
            .then(|| i32::try_from(v).ok())
            .flatten()
    };
    Some(DigitOffset {
        dx: coord(x)?,
        dy: coord(y.checked_neg()?.checked_sub(h)?)?,
        dw: coord(w)?,
        dh: coord(h)?,
    })
}

/// Whether `r` moves nothing.
fn zero(r: &RawDigitOffset) -> bool {
    r.xywh == [0; 4]
}

impl Cx<'_> {
    /// The rows of value `rec` that `cells` retained cells after `extra`
    /// dropped ones read, in cell order; fewer when the table is short.
    fn retained_rows(&self, rec: u32, extra: i32, cells: i32) -> &[RawDigitOffset] {
        let rows = self.tables.digit_offsets.of(rec);
        let from = usize::try_from(extra).unwrap_or(usize::MAX).min(rows.len());
        let to = usize::try_from(extra.saturating_add(cells))
            .unwrap_or(usize::MAX)
            .min(rows.len());
        rows.get(from..to).unwrap_or_default()
    }

    /// Whether every retained row keeps an aspect-preserving stretch an
    /// identity over a `[iw, ih]`-texel frame: its size change has the
    /// frame's aspect (`w · ih = h · iw`), so a cell whose own rect has
    /// it keeps it once moved.
    pub(super) fn digit_rows_keep_aspect(
        &self,
        rec: u32,
        extra: i32,
        cells: i32,
        [iw, ih]: [u32; 2],
    ) -> bool {
        self.retained_rows(rec, extra, cells).iter().all(|r| {
            let [_, _, w, h] = r.xywh.map(i64::from);
            w * i64::from(ih) == h * i64::from(iw)
        })
    }

    /// The model's digit offsets of a strip over value `rec` with
    /// `cells` retained cells after `extra` dropped ones, charged before
    /// they are allocated. A retained row past the model's coordinates is
    /// a range error at the row.
    pub(super) fn digit_offsets(
        &mut self,
        at: Span,
        rec: u32,
        extra: i32,
        cells: i32,
    ) -> Result<Vec<DigitOffset>, ImportError> {
        let rows = self.retained_rows(rec, extra, cells);
        let n = rows.iter().rposition(|r| !zero(r)).map_or(0, |i| i + 1);
        if let Some(bad) = rows.iter().take(n).find(|r| lower(r).is_none()) {
            return Err(ImportError::Range {
                at: self.def.loc(bad.node),
                field: "offset",
            });
        }
        self.budget
            .digit_offsets(n)
            .map_err(|l| self.limit(at, l))?;
        let rows = self.retained_rows(rec, extra, cells);
        let mut out = Vec::with_capacity(n);
        out.extend(rows.iter().take(n).filter_map(lower));
        Ok(out)
    }
}
