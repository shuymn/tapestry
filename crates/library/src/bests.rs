//! Personal bests (design.md "Personal Bests"): which plays count, what
//! they contribute, and the one function that writes `scores` and
//! `counted`, which live plays and the rebuild share.

use std::collections::HashMap;
use std::fmt;

use bms_model::Speed;
use eval::{Evaluation, JudgeEventKind};
use replay::{EndStatus, PlayRecord, PlayType};
use rules::RulePreset;
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

/// The version of the rules bests are derived by: eligibility, the values
/// and how a play is identified. A change bumps it, and opening the
/// database then clears every best and rebuilds them from the records.
pub const BESTS_VERSION: i64 = 2;

/// The LN mode bests are keyed by: the only one the evaluator judges, LN.
pub const LN_MODE: i64 = 1;

/// What verification established about a play, on either path: whether its
/// evaluation completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The as-played evaluation reached the final deadline.
    pub completed: bool,
}

/// Why a play sets no best, in the order the clauses are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// Verification refused the record.
    Unverified,
    /// The record was not saved.
    NotSaved,
    /// The recorded input is incomplete.
    Incomplete,
    /// The play ended midway.
    EndedMidway,
    /// The record says it was played to the end, yet its evaluation did
    /// not complete.
    EndMismatch,
    /// A practice round, not a full play.
    NotFullPlay,
    /// Played at another speed than 1×.
    NotNormalSpeed,
}

impl fmt::Display for Ineligible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unverified => "not verified",
            Self::NotSaved => "not saved",
            Self::Incomplete => "record incomplete",
            Self::EndedMidway => "ended midway",
            Self::EndMismatch => "the end does not match the evaluation",
            Self::NotFullPlay => "not a full play",
            Self::NotNormalSpeed => "not at 1x",
        })
    }
}

/// The first reason `record` sets no best, `None` when it may: verified,
/// saved, complete, played to the end (which its evaluation confirms), a
/// full play at 1×. A `#RANDOM` chart's draws share its file's best.
#[must_use]
pub fn ineligible(
    record: &PlayRecord,
    verified: Option<&Verified>,
    saved: bool,
) -> Option<Ineligible> {
    let Some(verified) = verified else {
        return Some(Ineligible::Unverified);
    };
    if !saved {
        return Some(Ineligible::NotSaved);
    }
    if record.status.incomplete.any() {
        return Some(Ineligible::Incomplete);
    }
    if record.status.end == EndStatus::EndedMidway {
        return Some(Ineligible::EndedMidway);
    }
    if !verified.completed {
        return Some(Ineligible::EndMismatch);
    }
    if record.play_type != PlayType::Full {
        return Some(Ineligible::NotFullPlay);
    }
    if record.speed != Speed::NORMAL {
        return Some(Ineligible::NotNormalSpeed);
    }
    None
}

/// Whether `record` counts for its best (design.md "Personal Bests": the
/// one eligibility predicate).
#[must_use]
pub fn counts_for_best(record: &PlayRecord, verified: Option<&Verified>, saved: bool) -> bool {
    ineligible(record, verified, saved).is_none()
}

/// A play's identity: its record file's path relative to `records\`, `/`
/// separated, and the SHA-256 of the record's canonical body; with the
/// file's size and modification time, by which an unchanged file is
/// skipped without reading it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayId {
    /// `<chart sha256 hex>/<file name>`.
    pub path: String,
    /// SHA-256 of [`replay::body_bytes`].
    pub body_sha256: [u8; 32],
    /// The file's size in bytes.
    pub size: i64,
    /// The file's modification time, ns since the Unix epoch.
    pub mtime_ns: i64,
}

impl PlayId {
    /// The identity of `record` saved at `path` (relative to `records\`),
    /// its body hashed in memory.
    #[must_use]
    pub fn of(path: String, record: &PlayRecord, size: i64, mtime_ns: i64) -> Self {
        Self {
            path,
            body_sha256: Sha256::digest(replay::body_bytes(record)).into(),
            size,
            mtime_ns,
        }
    }
}

/// What a play can set: its EX score and BP, the preset's gauges it
/// cleared (bit `i` of byte `i / 8` for gauge `i`, in the preset's order)
/// and whether it was a FULL COMBO. As a best, each value is the best of
/// any eligible play, independently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Best {
    /// EX score.
    pub ex: u32,
    /// BP.
    pub bp: u32,
    /// The gauges cleared, as a bit set.
    pub cleared: Vec<u8>,
    /// FULL COMBO.
    pub full_combo: bool,
}

impl Best {
    /// Whether gauge `i` of the preset is cleared.
    #[must_use]
    pub fn cleared(&self, i: usize) -> bool {
        self.cleared
            .get(i / 8)
            .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
    }

    /// The best of both, value by value: in any order, the same.
    fn merged(&self, other: &Self) -> Self {
        let len = self.cleared.len().max(other.cleared.len());
        let byte = |v: &[u8], i: usize| v.get(i).copied().unwrap_or(0);
        Self {
            ex: self.ex.max(other.ex),
            bp: self.bp.min(other.bp),
            cleared: (0..len)
                .map(|i| byte(&self.cleared, i) | byte(&other.cleared, i))
                .collect(),
            full_combo: self.full_combo || other.full_combo,
        }
    }
}

/// A key's stored score: the plays counted, and the best once a play was
/// eligible.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Score {
    /// Saved, verified plays counted under the key, eligible or not.
    pub plays: u64,
    /// The best values; `None` until an eligible play.
    pub best: Option<Best>,
}

/// The key bests are stored under (design.md "Personal Bests"): the chart
/// file's SHA-256, the preset's and the LN mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    /// SHA-256 of the preset's canonical encoding.
    pub preset_sha256: [u8; 32],
    /// [`LN_MODE`].
    pub ln_mode: i64,
    /// SHA-256 of the chart file.
    pub chart_sha256: [u8; 32],
}

/// One verified, saved play's contribution: one play to its key, and its
/// values when it is eligible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    /// Where it counts.
    pub key: Key,
    /// Which play it is.
    pub id: PlayId,
    /// Its values, when it counts for the best.
    pub best: Option<Best>,
    /// Why it sets no best, when it does not.
    pub ineligible: Option<Ineligible>,
}

impl Contribution {
    /// The contribution of `record`, saved as `id`, which verification
    /// accepted with `verified`, from its as-played `evaluation` under its
    /// `preset`. A FULL COMBO needs a completed evaluation, no judgment the
    /// preset counts as breaking the combo, and, when an empty POOR breaks
    /// it, no empty POOR.
    #[must_use]
    pub fn of(
        id: PlayId,
        record: &PlayRecord,
        verified: &Verified,
        evaluation: &Evaluation,
        preset: &RulePreset,
    ) -> Self {
        let ineligible = ineligible(record, Some(verified), true);
        let best = ineligible.is_none().then(|| {
            let mut cleared = vec![0_u8; evaluation.gauges.len().div_ceil(8)];
            for (i, g) in evaluation.gauges.iter().enumerate() {
                if g.cleared
                    && let Some(byte) = cleared.get_mut(i / 8)
                {
                    *byte |= 1 << (i % 8);
                }
            }
            let broken = evaluation.events.iter().any(|e| {
                matches!(e.kind, JudgeEventKind::Judged { judgment, .. }
                    if *preset.score.breaks_combo.get(judgment))
            });
            let empty_poor = preset.empty_poor.breaks_combo && evaluation.tally.empty_poor > 0;
            Best {
                ex: evaluation.tally.ex_score,
                bp: evaluation.tally.bp,
                cleared,
                full_combo: evaluation.completed && !broken && !empty_poor,
            }
        });
        Self {
            key: Key {
                preset_sha256: record.preset.sha256,
                ln_mode: LN_MODE,
                chart_sha256: record.chart.identity.sha256,
            },
            id,
            best,
            ineligible,
        }
    }
}

/// What [`ingest`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ingested {
    /// The play was counted before; nothing changed but its stored size
    /// and modification time.
    AlreadyCounted,
    /// The play was counted now: the key's score before and after.
    Counted {
        /// Before, `None` for the key's first play.
        previous: Option<Score>,
        /// After.
        current: Score,
    },
}

/// Counts `c` in the caller's transaction, the only writer of `scores` and
/// `counted`: once per play identity, merging its values into its key's
/// row. A play counted before only has its stamp refreshed.
pub(crate) fn ingest(conn: &Connection, c: &Contribution) -> rusqlite::Result<Ingested> {
    let id = &c.id;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO counted (path, body_sha256, size, mtime_ns)
         VALUES (?1, ?2, ?3, ?4)",
        params![id.path, id.body_sha256, id.size, id.mtime_ns],
    )?;
    if inserted == 0 {
        restamp(conn, id)?;
        return Ok(Ingested::AlreadyCounted);
    }
    let k = &c.key;
    let previous = score(conn, k)?;
    let base = previous.clone().unwrap_or_default();
    let best = match (&base.best, &c.best) {
        (Some(a), Some(b)) => Some(a.merged(b)),
        (a, b) => a.clone().or_else(|| b.clone()),
    };
    let current = Score {
        plays: base.plays.saturating_add(1),
        best,
    };
    let b = current.best.as_ref();
    conn.execute(
        "INSERT INTO scores (preset_sha256, ln_mode, chart_sha256, plays, best_ex, best_bp,
             cleared, full_combo)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (preset_sha256, ln_mode, chart_sha256) DO UPDATE SET
             plays = excluded.plays, best_ex = excluded.best_ex, best_bp = excluded.best_bp,
             cleared = excluded.cleared, full_combo = excluded.full_combo",
        params![
            k.preset_sha256,
            k.ln_mode,
            k.chart_sha256,
            i64::try_from(current.plays).unwrap_or(i64::MAX),
            b.map(|b| b.ex),
            b.map(|b| b.bp),
            b.map(|b| b.cleared.clone()),
            b.is_some_and(|b| b.full_combo),
        ],
    )?;
    conn.execute("DELETE FROM refused WHERE path = ?1", [&id.path])?;
    Ok(Ingested::Counted { previous, current })
}

/// Refreshes the stored size and modification time of a counted play.
pub(crate) fn restamp(conn: &Connection, id: &PlayId) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE counted SET size = ?3, mtime_ns = ?4 WHERE path = ?1 AND body_sha256 = ?2",
        params![id.path, id.body_sha256, id.size, id.mtime_ns],
    )
    .map(|_| ())
}

fn score_of(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Score> {
    let plays: i64 = r.get(at)?;
    let ex: Option<u32> = r.get(at + 1)?;
    let bp: Option<u32> = r.get(at + 2)?;
    let cleared: Option<Vec<u8>> = r.get(at + 3)?;
    let full_combo: bool = r.get(at + 4)?;
    Ok(Score {
        plays: u64::try_from(plays).unwrap_or(0),
        best: ex.zip(bp).map(|(ex, bp)| Best {
            ex,
            bp,
            cleared: cleared.unwrap_or_default(),
            full_combo,
        }),
    })
}

/// The stored score of `key`.
pub(crate) fn score(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Score>> {
    conn.query_row(
        "SELECT plays, best_ex, best_bp, cleared, full_combo FROM scores
         WHERE preset_sha256 = ?1 AND ln_mode = ?2 AND chart_sha256 = ?3",
        params![key.preset_sha256, key.ln_mode, key.chart_sha256],
        |r| score_of(r, 0),
    )
    .optional()
}

/// Every chart's score under a preset and LN mode, by chart SHA-256.
pub(crate) fn scores(
    conn: &Connection,
    preset_sha256: &[u8; 32],
    ln_mode: i64,
) -> rusqlite::Result<HashMap<[u8; 32], Score>> {
    let mut stmt = conn.prepare(
        "SELECT chart_sha256, plays, best_ex, best_bp, cleared, full_combo FROM scores
         WHERE preset_sha256 = ?1 AND ln_mode = ?2",
    )?;
    let rows = stmt.query_map(params![preset_sha256, ln_mode], |r| {
        Ok((r.get::<_, [u8; 32]>(0)?, score_of(r, 1)?))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::Best;

    #[test]
    fn bests_merge_value_by_value_in_any_order() {
        let a = Best {
            ex: 10,
            bp: 5,
            cleared: vec![0b01],
            full_combo: false,
        };
        let b = Best {
            ex: 8,
            bp: 2,
            cleared: vec![0b10, 0b1],
            full_combo: true,
        };
        let both = Best {
            ex: 10,
            bp: 2,
            cleared: vec![0b11, 0b1],
            full_combo: true,
        };
        assert_eq!(a.merged(&b), both);
        assert_eq!(b.merged(&a), both);
        assert!(both.cleared(0) && both.cleared(1) && both.cleared(8));
        assert!(!both.cleared(2) && !both.cleared(64));
    }
}
