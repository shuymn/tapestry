//! Personal bests (design.md "Personal Bests"): which plays count, what
//! they contribute, and the one function that writes `scores` and
//! `counted`, which live plays and the rebuild share.

use std::collections::HashMap;
use std::fmt;

use bms_model::{Chart, ChartSummary, Speed};
use eval::{Evaluation, JudgeEventKind};
use replay::{EndStatus, PlayRecord, PlayType};
use rules::{PerJudgment, RulePreset};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

/// The version of the rules bests are derived by: eligibility, the values
/// and how a play is identified. A change bumps it, and opening the
/// database then clears every best and rebuilds them from the records.
pub const BESTS_VERSION: i64 = 3;

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
    /// A capture cancelled before playback: no played attempt, so neither
    /// a best nor a play count.
    NoPlayedAttempt,
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
            Self::NoPlayedAttempt => "no played attempt",
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

/// The first reason `record` sets no best, `None` when it may: a played
/// attempt, verified, saved, complete, played to the end (which its
/// evaluation confirms), a full play at 1×. A `#RANDOM` chart's draws
/// share its file's best.
#[must_use]
pub fn ineligible(
    record: &PlayRecord,
    verified: Option<&Verified>,
    saved: bool,
) -> Option<Ineligible> {
    if !record.played() {
        return Some(Ineligible::NoPlayedAttempt);
    }
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

/// The values a [`Breakdown`] stores, each a little-endian `u32`.
pub const BREAKDOWN_FIELDS: usize = 22;

/// The stored length of a [`Breakdown`]'s values.
pub const BREAKDOWN_BYTES: usize = 4 * BREAKDOWN_FIELDS;

/// One play's breakdown, as its as-played evaluation counted it, beside the
/// play's identity: what the bests keep of the play that set the best EX
/// score, so every figure of it comes from that one play (design.md
/// "Personal Bests").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breakdown {
    /// SHA-256 of the record's canonical body: the second part of the
    /// play's identity, by which equal EX scores are ordered.
    pub body_sha256: [u8; 32],
    /// The judged notes of the chart as drawn: its taps and long notes.
    pub notes: u32,
    /// EX score.
    pub ex: u32,
    /// BP.
    pub bp: u32,
    /// The highest combo.
    pub max_combo: u32,
    /// Judgments PERFECT, GREAT, GOOD, BAD and POOR.
    pub counts: [u32; 5],
    /// The judgments that came early, in the same order.
    pub early: [u32; 5],
    /// The judgments that came late, in the same order.
    pub late: [u32; 5],
    /// Empty POORs.
    pub empty_poor: u32,
    /// Empty POORs pressed at or before their note.
    pub empty_poor_early: u32,
    /// Empty POORs pressed after their note.
    pub empty_poor_late: u32,
}

impl Breakdown {
    /// Its stored values in their stored order, the one definition of the
    /// layout the encoder, the decoder and the table's length check share:
    /// notes, EX, BP, max combo, the five counts, the five early, the five
    /// late, then the empty POORs, early and late.
    fn fields_mut(&mut self) -> [&mut u32; BREAKDOWN_FIELDS] {
        let [c0, c1, c2, c3, c4] = &mut self.counts;
        let [e0, e1, e2, e3, e4] = &mut self.early;
        let [l0, l1, l2, l3, l4] = &mut self.late;
        [
            &mut self.notes,
            &mut self.ex,
            &mut self.bp,
            &mut self.max_combo,
            c0,
            c1,
            c2,
            c3,
            c4,
            e0,
            e1,
            e2,
            e3,
            e4,
            l0,
            l1,
            l2,
            l3,
            l4,
            &mut self.empty_poor,
            &mut self.empty_poor_early,
            &mut self.empty_poor_late,
        ]
    }

    /// The stored bytes: the values as little-endian `u32`s in the
    /// layout's order.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; BREAKDOWN_BYTES] {
        let mut values = *self;
        let mut out = [0; BREAKDOWN_BYTES];
        for (to, v) in out
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(values.fields_mut())
        {
            *to = v.to_le_bytes();
        }
        out
    }

    /// The breakdown of the play `body_sha256` from its stored bytes
    /// ([`to_bytes`](Self::to_bytes)).
    #[must_use]
    pub fn from_bytes(body_sha256: [u8; 32], bytes: &[u8; BREAKDOWN_BYTES]) -> Self {
        let mut b = Self {
            body_sha256,
            ..Self::default()
        };
        for (to, from) in b.fields_mut().into_iter().zip(bytes.as_chunks::<4>().0) {
            *to = u32::from_le_bytes(*from);
        }
        b
    }

    /// Whether this play is kept over `other`: the higher EX score, and of
    /// equal ones the lower body hash, so the order plays arrive in does
    /// not matter.
    fn beats(&self, other: &Self) -> bool {
        (self.ex, std::cmp::Reverse(self.body_sha256))
            > (other.ex, std::cmp::Reverse(other.body_sha256))
    }
}

/// What a play can set: its EX score, BP and max combo, the preset's
/// gauges it cleared (bit `i` of byte `i / 8` for gauge `i`, in the
/// preset's order), whether it was a FULL COMBO, and its breakdown. As a
/// best, each of the first five is the best of any eligible play,
/// independently, and the breakdown is the one play that set the best EX
/// score, the lowest body hash among equals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Best {
    /// EX score.
    pub ex: u32,
    /// BP.
    pub bp: u32,
    /// The highest combo, which may come from another play than `play`.
    pub max_combo: u32,
    /// The gauges cleared, as a bit set.
    pub cleared: Vec<u8>,
    /// FULL COMBO.
    pub full_combo: bool,
    /// The play that set `ex`.
    pub play: Breakdown,
}

impl Best {
    /// Whether gauge `i` of the preset is cleared.
    #[must_use]
    pub fn cleared(&self, i: usize) -> bool {
        self.cleared
            .get(i / 8)
            .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
    }

    /// Whether the lamp is not FAILED: a FULL COMBO or a cleared gauge
    /// (design.md "Personal Bests").
    #[must_use]
    pub fn lamp_cleared(&self) -> bool {
        self.full_combo || self.cleared.iter().any(|b| *b != 0)
    }

    /// The best of both, value by value: in any order, the same.
    fn merged(&self, other: &Self) -> Self {
        let len = self.cleared.len().max(other.cleared.len());
        let byte = |v: &[u8], i: usize| v.get(i).copied().unwrap_or(0);
        Self {
            ex: self.ex.max(other.ex),
            bp: self.bp.min(other.bp),
            max_combo: self.max_combo.max(other.max_combo),
            cleared: (0..len)
                .map(|i| byte(&self.cleared, i) | byte(&other.cleared, i))
                .collect(),
            full_combo: self.full_combo || other.full_combo,
            play: if other.play.beats(&self.play) {
                other.play
            } else {
                self.play
            },
        }
    }
}

/// A key's stored score: the plays counted, the cleared ones among them,
/// and the best once a play was eligible.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Score {
    /// Saved, verified plays counted under the key, eligible or not.
    pub plays: u64,
    /// The eligible plays among them whose own lamp is not FAILED.
    pub clears: u64,
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
    /// Whether it counts as a clear: eligible, and its own lamp not FAILED.
    pub clear: bool,
    /// Why it sets no best, when it does not.
    pub ineligible: Option<Ineligible>,
}

impl Contribution {
    /// The contribution of `record`, saved as `id`, which verification
    /// accepted with `verified`, from its as-played `evaluation` of `chart`
    /// (as drawn) under its `preset`. A FULL COMBO needs a completed
    /// evaluation, no judgment the preset counts as breaking the combo,
    /// and, when an empty POOR breaks it, no empty POOR.
    #[must_use]
    pub fn of(
        id: PlayId,
        record: &PlayRecord,
        verified: &Verified,
        evaluation: &Evaluation,
        chart: &Chart,
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
            let t = &evaluation.tally;
            let per = |p: &PerJudgment<u32>| [p.perfect, p.great, p.good, p.bad, p.poor];
            let summary = ChartSummary::of(chart);
            Best {
                ex: t.ex_score,
                bp: t.bp,
                max_combo: t.max_combo,
                cleared,
                full_combo: evaluation.completed && !broken && !empty_poor,
                play: Breakdown {
                    body_sha256: id.body_sha256,
                    notes: summary.taps.saturating_add(summary.longs),
                    ex: t.ex_score,
                    bp: t.bp,
                    max_combo: t.max_combo,
                    counts: per(&t.counts),
                    early: per(&t.early),
                    late: per(&t.late),
                    empty_poor: t.empty_poor,
                    empty_poor_early: t.empty_poor_early,
                    empty_poor_late: t.empty_poor_late,
                },
            }
        });
        Self {
            key: Key {
                preset_sha256: record.preset.sha256,
                ln_mode: LN_MODE,
                chart_sha256: record.chart.identity.sha256,
            },
            id,
            clear: best.as_ref().is_some_and(Best::lamp_cleared),
            best,
            ineligible,
        }
    }
}

/// What [`ingest`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "made once per play counted and read at once; boxing buys nothing"
)]
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
        clears: base.clears.saturating_add(u64::from(c.clear)),
        best,
    };
    let b = current.best.as_ref();
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO scores (preset_sha256, ln_mode, chart_sha256, plays, clears, best_ex,
             best_bp, best_combo, cleared, full_combo, play_sha256, play)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (preset_sha256, ln_mode, chart_sha256) DO UPDATE SET
             plays = excluded.plays, clears = excluded.clears, best_ex = excluded.best_ex,
             best_bp = excluded.best_bp, best_combo = excluded.best_combo,
             cleared = excluded.cleared, full_combo = excluded.full_combo,
             play_sha256 = excluded.play_sha256, play = excluded.play",
        params![
            k.preset_sha256,
            k.ln_mode,
            k.chart_sha256,
            count(current.plays),
            count(current.clears),
            b.map(|b| b.ex),
            b.map(|b| b.bp),
            b.map(|b| b.max_combo),
            b.map(|b| b.cleared.clone()),
            b.is_some_and(|b| b.full_combo),
            b.map(|b| b.play.body_sha256),
            b.map(|b| b.play.to_bytes()),
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
    let inconsistent = |i: usize, what: &'static str| {
        rusqlite::Error::FromSqlConversionFailure(
            at + i,
            Type::Integer,
            Box::new(Inconsistent(what)),
        )
    };
    let count = |i: usize, what| {
        let n: i64 = r.get(at + i)?;
        u64::try_from(n).map_err(|_| inconsistent(i, what))
    };
    let plays = count(0, "a negative play count")?;
    let clears = count(1, "a negative clear count")?;
    if clears > plays {
        return Err(inconsistent(1, "more clears than plays"));
    }
    let ex: Option<u32> = r.get(at + 2)?;
    let best = match ex {
        None if clears > 0 => return Err(inconsistent(1, "clears without a best")),
        None => None,
        Some(ex) => {
            // Fixed arrays: a blob of another length is refused from its
            // borrowed bytes, before anything is copied.
            let sha: [u8; 32] = r.get(at + 7)?;
            let bytes: [u8; BREAKDOWN_BYTES] = r.get(at + 8)?;
            let best = Best {
                ex,
                bp: r.get(at + 3)?,
                max_combo: r.get(at + 4)?,
                cleared: r.get::<_, Option<Vec<u8>>>(at + 5)?.unwrap_or_default(),
                full_combo: r.get(at + 6)?,
                play: Breakdown::from_bytes(sha, &bytes),
            };
            // The play is the best EX's, and the best BP and max combo are
            // at least as good as any one play's.
            if best.play.ex != best.ex {
                return Err(inconsistent(2, "a best EX that is not its play's"));
            }
            if best.bp > best.play.bp {
                return Err(inconsistent(3, "a best BP worse than its play's"));
            }
            if best.max_combo < best.play.max_combo {
                return Err(inconsistent(4, "a best max combo below its play's"));
            }
            // A best comes from a counted play.
            if plays == 0 {
                return Err(inconsistent(0, "a best without a play"));
            }
            // A clear is an eligible play whose own lamp is not FAILED, and
            // the merged lamp is cleared exactly when one of them was.
            match (clears > 0, best.lamp_cleared()) {
                (true, false) => return Err(inconsistent(1, "clears with a FAILED lamp")),
                (false, true) => return Err(inconsistent(1, "a cleared lamp without clears")),
                _ => {}
            }
            // One play is the whole best: its BP and max combo are the
            // best's.
            if plays == 1 && best.bp != best.play.bp {
                return Err(inconsistent(3, "one play whose best BP is not its own"));
            }
            if plays == 1 && best.max_combo != best.play.max_combo {
                return Err(inconsistent(
                    4,
                    "one play whose best max combo is not its own",
                ));
            }
            Some(best)
        }
    };
    Ok(Score {
        plays,
        clears,
        best,
    })
}

/// A stored score whose values contradict one another (design.md
/// "Personal Bests"): a failed read, never a corrected value.
#[derive(Debug)]
struct Inconsistent(&'static str);

impl fmt::Display for Inconsistent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a stored score is inconsistent: {}", self.0)
    }
}

impl std::error::Error for Inconsistent {}

/// The columns [`score_of`] reads, in its order.
const SCORE_COLUMNS: &str =
    "plays, clears, best_ex, best_bp, best_combo, cleared, full_combo, play_sha256, play";

/// The stored score of `key`.
pub(crate) fn score(conn: &Connection, key: &Key) -> rusqlite::Result<Option<Score>> {
    conn.query_row(
        &format!(
            "SELECT {SCORE_COLUMNS} FROM scores
             WHERE preset_sha256 = ?1 AND ln_mode = ?2 AND chart_sha256 = ?3"
        ),
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
    let mut stmt = conn.prepare(&format!(
        "SELECT chart_sha256, {SCORE_COLUMNS} FROM scores
         WHERE preset_sha256 = ?1 AND ln_mode = ?2"
    ))?;
    let rows = stmt.query_map(params![preset_sha256, ln_mode], |r| {
        Ok((r.get::<_, [u8; 32]>(0)?, score_of(r, 1)?))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{BREAKDOWN_BYTES, Best, Breakdown, Contribution, Key, LN_MODE, PlayId, Score};

    /// A breakdown of EX `ex` and max combo `combo` for the play hashed
    /// `body`, its other values derived from all three so plays differ.
    fn play(body: u8, ex: u32, combo: u32) -> Breakdown {
        let v = u32::from(body);
        Breakdown {
            body_sha256: [body; 32],
            notes: 10,
            ex,
            // Above every best BP the tests give, as a best's BP never
            // exceeds its play's.
            bp: v + 100,
            max_combo: combo,
            counts: [ex / 2, ex % 2, v, 0, 1],
            early: [ex / 4, 0, v, 0, 0],
            late: [ex / 2 - ex / 4, ex % 2, 0, 0, 1],
            empty_poor: v,
            empty_poor_early: v / 2,
            empty_poor_late: v - v / 2,
        }
    }

    /// The best one play of `p` sets, its BP, clears and FULL COMBO given:
    /// its values are its play's own, as a counted play's are.
    fn best(p: Breakdown, bp: u32, cleared: u8, full_combo: bool) -> Best {
        Best {
            ex: p.ex,
            bp,
            max_combo: p.max_combo,
            cleared: vec![cleared],
            full_combo,
            play: Breakdown { bp, ..p },
        }
    }

    /// The play `c` contributes.
    fn play_of(c: &Contribution) -> Breakdown {
        c.best.as_ref().unwrap().play
    }

    /// A contribution of the play hashed `body`, saved as `c/<body>.tprl`;
    /// ineligible without a best.
    fn contribution(body: u8, best: Option<Best>) -> Contribution {
        Contribution {
            key: Key {
                preset_sha256: [1; 32],
                ln_mode: LN_MODE,
                chart_sha256: [2; 32],
            },
            id: PlayId {
                path: format!("c/{body}.tprl"),
                body_sha256: [body; 32],
                size: 1,
                mtime_ns: 1,
            },
            clear: best.as_ref().is_some_and(Best::lamp_cleared),
            ineligible: best.is_none().then_some(super::Ineligible::NotFullPlay),
            best,
        }
    }

    /// A database with the current schema, in memory.
    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::create(&conn).unwrap();
        conn
    }

    /// The score that counting `cs` in order through [`super::ingest`]
    /// stores, in a database of its own.
    fn fold(cs: &[&Contribution]) -> Score {
        let conn = database();
        for c in cs {
            super::ingest(&conn, c).unwrap();
        }
        super::score(&conn, &cs[0].key).unwrap().unwrap()
    }

    fn permutations<'a>(items: &[&'a Contribution]) -> Vec<Vec<&'a Contribution>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let first = rest.remove(i);
            for mut p in permutations(&rest) {
                p.insert(0, first);
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn bests_merge_value_by_value_in_any_order() {
        let a = Best {
            ex: 10,
            bp: 5,
            max_combo: 3,
            cleared: vec![0b01],
            full_combo: false,
            play: play(9, 10, 3),
        };
        let b = Best {
            ex: 8,
            bp: 2,
            max_combo: 7,
            cleared: vec![0b10, 0b1],
            full_combo: true,
            play: play(1, 8, 7),
        };
        let both = Best {
            ex: 10,
            bp: 2,
            max_combo: 7,
            cleared: vec![0b11, 0b1],
            full_combo: true,
            play: play(9, 10, 3),
        };
        assert_eq!(a.merged(&b), both);
        assert_eq!(b.merged(&a), both);
        assert!(both.cleared(0) && both.cleared(1) && both.cleared(8));
        assert!(!both.cleared(2) && !both.cleared(64));
    }

    /// Every value has its own slot, in the stored order, and only the
    /// exact length decodes.
    #[test]
    fn a_breakdown_round_trips_in_its_layout() {
        let p = Breakdown {
            body_sha256: [7; 32],
            notes: 1,
            ex: 2,
            bp: 3,
            max_combo: 4,
            counts: [5, 6, 7, 8, 9],
            early: [10, 11, 12, 13, 14],
            late: [15, 16, 17, 18, 19],
            empty_poor: 20,
            empty_poor_early: 21,
            empty_poor_late: 22,
        };
        let bytes = p.to_bytes();
        let words: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        assert_eq!(words, (1..=22).collect::<Vec<u32>>());
        assert_eq!(Breakdown::from_bytes([7; 32], &bytes), p);
        assert_eq!(super::BREAKDOWN_FIELDS * 4, BREAKDOWN_BYTES);
    }

    /// The DDL checks the length the layout has.
    #[test]
    fn the_scores_ddl_checks_the_breakdown_length() {
        assert!(crate::db::scores_ddl().contains(&format!("length(play) = {BREAKDOWN_BYTES})")));
    }

    /// A stored breakdown or play hash of another length is a read error,
    /// never a default.
    #[test]
    fn a_malformed_breakdown_is_a_read_error() {
        let conn = database();
        let c = contribution(3, Some(best(play(3, 6, 3), 0, 1, true)));
        super::ingest(&conn, &c).unwrap();
        // Past the length checks, as a file written by other means could be.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        let good_play = c.best.as_ref().unwrap().play.to_bytes().to_vec();
        // The stored values cut short or run on: read as a layout, each
        // would still give a consistent best, so only the length refuses.
        let mut longer = good_play.clone();
        longer.push(0);
        let mut huge = good_play.clone();
        huge.resize(64 << 20, 0);
        for (column, bad, good) in [
            (
                "play",
                good_play[..BREAKDOWN_BYTES - 1].to_vec(),
                good_play.clone(),
            ),
            ("play", longer, good_play.clone()),
            // Refused from its borrowed bytes, never copied whole.
            ("play", huge, good_play),
            ("play_sha256", vec![3; 31], vec![3; 32]),
            ("play_sha256", vec![3; 33], vec![3; 32]),
        ] {
            let len = bad.len();
            conn.execute(&format!("UPDATE scores SET {column} = ?1"), [bad])
                .unwrap();
            let e = super::score(&conn, &c.key).unwrap_err();
            assert!(
                e.to_string().contains(&format!("out of {len} byte blob")),
                "{column} {len}: {e}"
            );
            conn.execute(&format!("UPDATE scores SET {column} = ?1"), [good])
                .unwrap();
            assert!(super::score(&conn, &c.key).is_ok(), "{column} restored");
        }
    }

    /// A stored score whose values contradict one another is a read
    /// error, never a corrected value: a negative play or clear count,
    /// more clears than plays, clears without a best, and a best whose EX
    /// is not its play's or whose BP or max combo is worse than its
    /// play's.
    #[test]
    fn an_inconsistent_score_is_a_read_error() {
        let conn = database();
        // One play, cleared, its own BP and max combo the best's.
        let c = contribution(3, Some(best(play(3, 6, 3), 103, 1, false)));
        super::ingest(&conn, &c).unwrap();
        let score = || super::score(&conn, &c.key);
        assert_eq!(score().unwrap().map(|s| (s.plays, s.clears)), Some((1, 1)));
        // Each change breaks one relation only, and is read back as that
        // one.
        for (set, undo, why) in [
            (
                "plays = -1, clears = 0",
                "plays = 1, clears = 1",
                "a negative play count",
            ),
            ("clears = -1", "clears = 1", "a negative clear count"),
            (
                "plays = 2, clears = 3",
                "plays = 1, clears = 1",
                "more clears than plays",
            ),
            (
                "best_ex = 7",
                "best_ex = 6",
                "a best EX that is not its play's",
            ),
            (
                "plays = 2, best_bp = 104",
                "plays = 1, best_bp = 103",
                "a best BP worse than its play's",
            ),
            (
                "plays = 2, best_combo = 2",
                "plays = 1, best_combo = 3",
                "a best max combo below its play's",
            ),
            (
                "plays = 0, clears = 0",
                "plays = 1, clears = 1",
                "a best without a play",
            ),
            (
                "plays = 2, cleared = x'00'",
                "plays = 1, cleared = x'01'",
                "clears with a FAILED lamp",
            ),
            ("clears = 0", "clears = 1", "a cleared lamp without clears"),
            (
                "best_bp = 102",
                "best_bp = 103",
                "one play whose best BP is not its own",
            ),
            (
                "best_combo = 4",
                "best_combo = 3",
                "one play whose best max combo is not its own",
            ),
        ] {
            conn.execute(&format!("UPDATE scores SET {set}"), [])
                .unwrap();
            let e = score().unwrap_err();
            assert!(
                matches!(e, rusqlite::Error::FromSqlConversionFailure(..))
                    && e.to_string().contains(why),
                "{set}: {e}"
            );
            conn.execute(&format!("UPDATE scores SET {undo}"), [])
                .unwrap();
            assert!(score().is_ok(), "{undo}");
        }
        // Clears with no best: the best's columns null together.
        conn.execute(
            "UPDATE scores SET best_ex = NULL, best_bp = NULL, best_combo = NULL,
                 play_sha256 = NULL, play = NULL",
            [],
        )
        .unwrap();
        assert!(
            score()
                .unwrap_err()
                .to_string()
                .contains("clears without a best")
        );
        conn.execute("UPDATE scores SET clears = 0", []).unwrap();
        assert!(score().unwrap().unwrap().best.is_none());
    }

    #[test]
    fn the_higher_ex_play_keeps_its_breakdown() {
        let low = contribution(1, Some(best(play(1, 4, 2), 3, 1, false)));
        let high = contribution(9, Some(best(play(9, 6, 2), 3, 1, false)));
        for order in [[&low, &high], [&high, &low]] {
            assert_eq!(fold(&order).best.unwrap().play, play_of(&high));
        }
    }

    #[test]
    fn equal_ex_keeps_the_lowest_body_hash() {
        let a = contribution(4, Some(best(play(4, 6, 2), 3, 1, false)));
        let b = contribution(8, Some(best(play(8, 6, 3), 3, 1, false)));
        for order in [[&a, &b], [&b, &a]] {
            assert_eq!(fold(&order).best.unwrap().play, play_of(&a));
        }
    }

    /// A lower-EX play with a better max combo raises the best max combo
    /// and leaves the kept breakdown alone: the best-EX play's combo 50,
    /// another play's 100.
    #[test]
    fn a_better_combo_leaves_the_breakdown() {
        let ex = contribution(2, Some(best(play(2, 90, 50), 30, 1, false)));
        let combo = contribution(3, Some(best(play(3, 80, 100), 30, 1, false)));
        for order in [[&ex, &combo], [&combo, &ex]] {
            let b = fold(&order).best.unwrap();
            assert_eq!((b.max_combo, b.play), (100, play_of(&ex)));
        }
    }

    /// Any order of the same plays stores the same score, every value
    /// included; the best's EX is always its play's, and the merge agrees
    /// however it is grouped.
    #[test]
    fn bests_fold_alike_in_every_order() {
        let cs = [
            contribution(5, Some(best(play(5, 6, 2), 4, 0b01, false))),
            contribution(4, Some(best(play(4, 6, 3), 3, 0, false))),
            contribution(6, Some(best(play(6, 4, 9), 5, 0b10, false))),
            contribution(7, Some(best(play(7, 2, 1), 1, 0, true))),
            contribution(8, None),
        ];
        let refs: Vec<&Contribution> = cs.iter().collect();
        let all = permutations(&refs);
        assert_eq!(all.len(), 120);
        let first = fold(&all[0]);
        let b = first.best.as_ref().unwrap();
        assert_eq!(
            (first.plays, first.clears, b.ex, b.bp, b.max_combo),
            (5, 3, 6, 1, 9)
        );
        assert_eq!((b.cleared.clone(), b.full_combo), (vec![0b11], true));
        assert_eq!(b.play, play_of(&cs[1]));
        assert_eq!(b.ex, b.play.ex);
        for order in &all[1..] {
            assert_eq!(fold(order), first);
        }
        let [x, y, z] = [&cs[0], &cs[1], &cs[2]].map(|c| c.best.clone().unwrap());
        assert_eq!(x.merged(&y).merged(&z), x.merged(&y.merged(&z)));
    }

    /// Counting a play identity again changes nothing.
    #[test]
    fn a_play_counted_again_changes_nothing() {
        let c = contribution(5, Some(best(play(5, 6, 2), 4, 1, false)));
        assert_eq!(fold(&[&c, &c]), fold(&[&c]));
    }

    /// Only an eligible play whose own lamp is not FAILED is a clear.
    #[test]
    fn only_eligible_clears_count() {
        let cleared = contribution(1, Some(best(play(1, 6, 2), 0, 0b1, false)));
        let fc = contribution(2, Some(best(play(2, 6, 2), 0, 0, true)));
        let failed = contribution(3, Some(best(play(3, 6, 2), 0, 0, false)));
        let ineligible = contribution(4, None);
        assert!(cleared.clear && fc.clear && !failed.clear && !ineligible.clear);
        let s = fold(&[&cleared, &failed, &fc, &ineligible]);
        assert_eq!((s.plays, s.clears), (4, 2));
    }

    /// The inline sizes design.md "Personal Bests" bounds a batch and the
    /// selection's map by, on the 64-bit target.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_bounded_sizes_are_measured() {
        use std::mem::size_of;
        assert_eq!(size_of::<Breakdown>(), 120);
        assert_eq!(size_of::<Best>(), 160);
        assert_eq!(size_of::<Score>(), 176);
        assert_eq!(size_of::<Contribution>(), 312);
    }
}
