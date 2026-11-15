//! Personal bests (design.md "Personal Bests"): which plays count, what
//! they contribute, and the one function that writes `scores` and
//! `counted`, which live plays and the rebuild share.

use std::collections::HashMap;
use std::fmt;

use bms_model::{Chart, ChartSummary, Note, Speed, TimeUs};
use eval::{Evaluation, JudgeEventKind};
use replay::{EndStatus, PlayRecord, PlayType};
use rules::{Judgment, PerJudgment, RulePreset};
use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

/// The version of the rules bests are derived by: eligibility, the values,
/// how a play is identified, and the layout of the bests tables. A change
/// bumps it, and opening the database then clears every best and rebuilds
/// them from the records.
pub const BESTS_VERSION: i64 = 6;

/// The most judgments a stored progress holds: the loader's object cap
/// (design.md "Loading Limits"), which bounds a chart's judged targets.
pub const MAX_PROGRESS: usize = 500_000;

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

/// The kind of play a key's play count and sums are kept by, each in its
/// own columns (design.md "Personal Bests"): a practice round, from a
/// mid-chart start or a section repeat, or any other play. The totals a
/// skin reads add both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayKind {
    /// A play of the whole chart, at any speed, ended midway or not.
    #[default]
    Normal,
    /// A practice round.
    Practice,
}

impl PlayKind {
    /// The kind of `record`'s play.
    #[must_use]
    pub const fn of(record: &PlayRecord) -> Self {
        if record.play_type.practice().is_some() {
            Self::Practice
        } else {
            Self::Normal
        }
    }

    /// The columns of its play count and sums: the count, then [`Sums`]'
    /// order.
    const fn columns(self) -> [&'static str; 7] {
        match self {
            Self::Normal => [
                "plays",
                "sum_perfect",
                "sum_great",
                "sum_good",
                "sum_bad",
                "sum_poor",
                "sum_play_us",
            ],
            Self::Practice => [
                "practice_plays",
                "practice_sum_perfect",
                "practice_sum_great",
                "practice_sum_good",
                "practice_sum_bad",
                "practice_sum_poor",
                "practice_sum_play_us",
            ],
        }
    }
}

/// What one counted play adds to its key's sums, eligible or not, and so
/// to the player's totals (design.md "Personal Bests").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sums {
    /// Its PERFECT, GREAT, GOOD, BAD and POOR counts, POOR as the preset's
    /// tally counts it.
    pub counts: [u32; 5],
    /// Its play time in µs ([`play_us`]).
    pub play_us: u64,
}

/// The player's totals over every key (design.md "Personal Bests"): the
/// plays and clears counted, and the [`Sums`] of those plays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Plays counted, every chart, preset and LN mode included.
    pub plays: u64,
    /// The clears among them.
    pub clears: u64,
    /// Their PERFECT, GREAT, GOOD, BAD and POOR totals.
    pub counts: [u64; 5],
    /// Their play time in µs.
    pub play_us: u64,
}

/// The player's totals kept per [`PlayKind`]: practice rounds' apart from
/// the other plays', so either can be shown alone. A practice round is
/// never a clear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TotalsByKind {
    /// Every play but practice rounds.
    pub normal: Totals,
    /// Practice rounds.
    pub practice: Totals,
}

impl TotalsByKind {
    /// Both kinds added: the totals a skin reads. Each part is at most
    /// SQLite's largest integer, so the sum never saturates.
    #[must_use]
    pub fn sum(&self) -> Totals {
        let (a, b) = (&self.normal, &self.practice);
        let mut counts = a.counts;
        for (c, add) in counts.iter_mut().zip(b.counts) {
            *c = c.saturating_add(add);
        }
        Totals {
            plays: a.plays.saturating_add(b.plays),
            clears: a.clears.saturating_add(b.clears),
            counts,
            play_us: a.play_us.saturating_add(b.play_us),
        }
    }
}

/// How far into the chart `record`'s play ran, in µs: the latest end (a
/// long note's end) among the notes `evaluation` judged, misses included,
/// never past an aborted play's finalized watermark, less its evaluated
/// range's start (0 for a full play, the section's start for a practice
/// round); 0 when it judged none. `chart` is the one evaluated, at the
/// record's speed, so the time is the time played.
// Source: SF-014
#[must_use]
pub fn play_us(record: &PlayRecord, evaluation: &Evaluation, chart: &Chart) -> u64 {
    let last = evaluation
        .events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged { lane, note, .. } => {
                chart.lanes.get(lane.index())?.get(note).map(Note::end)
            }
            _ => None,
        })
        .max();
    let Some(mut end) = last else {
        return 0;
    };
    if record.status.end == EndStatus::EndedMidway {
        end = end.min(record.status.finalized);
    }
    let start = record.play_type.practice().map_or(TimeUs(0), |p| p.start);
    u64::try_from(end.0.saturating_sub(start.0)).unwrap_or(0)
}

/// One verified, saved play's contribution: one play to its key, its sums,
/// and its values when it is eligible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contribution {
    /// Where it counts.
    pub key: Key,
    /// Which play it is.
    pub id: PlayId,
    /// Its values, when it counts for the best.
    pub best: Option<Best>,
    /// Its progress, when it counts for the best: the judgment of each
    /// judged target in the order its evaluation emitted them, kept when
    /// it sets the best's breakdown (design.md "Personal Bests").
    pub progress: Option<Box<[Judgment]>>,
    /// Whether it counts as a clear: eligible, and its own lamp not FAILED.
    pub clear: bool,
    /// Why it sets no best, when it does not.
    pub ineligible: Option<Ineligible>,
    /// What it adds to its key's sums, eligible or not.
    pub sums: Sums,
    /// The kind whose play count and sums it adds to.
    pub kind: PlayKind,
}

impl Contribution {
    /// The contribution of `record`, saved as `id`, which verification
    /// accepted with `verified`, from its as-played `evaluation` of `chart`
    /// (as drawn) under its `preset`. A FULL COMBO needs a completed
    /// evaluation, no judgment the preset counts as breaking the combo,
    /// and, when an empty POOR breaks it, no empty POOR. The progress is
    /// the judgment of each `Judged` event in emitted order: empty POORs
    /// and mines judge no target and have none.
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
        let per = |p: &PerJudgment<u32>| [p.perfect, p.great, p.good, p.bad, p.poor];
        let sums = Sums {
            counts: per(&evaluation.tally.counts),
            play_us: play_us(record, evaluation, chart),
        };
        let eligible = ineligible.is_none().then(|| {
            let mut cleared = vec![0_u8; evaluation.gauges.len().div_ceil(8)];
            for (i, g) in evaluation.gauges.iter().enumerate() {
                if g.cleared
                    && let Some(byte) = cleared.get_mut(i / 8)
                {
                    *byte |= 1 << (i % 8);
                }
            }
            // The progress, each `Judged` event's judgment in emitted
            // order, and whether any of them breaks the combo, in one pass.
            let mut broken = false;
            let progress: Box<[Judgment]> = evaluation
                .events
                .iter()
                .filter_map(|e| match e.kind {
                    JudgeEventKind::Judged { judgment, .. } => {
                        broken |= *preset.score.breaks_combo.get(judgment);
                        Some(judgment)
                    }
                    _ => None,
                })
                .collect();
            let empty_poor = preset.empty_poor.breaks_combo && evaluation.tally.empty_poor > 0;
            let t = &evaluation.tally;
            let summary = ChartSummary::of(chart);
            let best = Best {
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
            };
            (best, progress)
        });
        let (best, progress) = eligible.unzip();
        Self {
            key: Key {
                preset_sha256: record.preset.sha256,
                ln_mode: LN_MODE,
                chart_sha256: record.chart.identity.sha256,
            },
            id,
            clear: best.as_ref().is_some_and(Best::lamp_cleared),
            progress,
            best,
            ineligible,
            sums,
            kind: PlayKind::of(record),
        }
    }
}

/// A judgment's stored byte: its index in [`Judgment::ALL`], PERFECT 0 to
/// POOR 4.
const fn judgment_byte(j: Judgment) -> u8 {
    match j {
        Judgment::Perfect => 0,
        Judgment::Great => 1,
        Judgment::Good => 2,
        Judgment::Bad => 3,
        Judgment::Poor => 4,
    }
}

/// The columns of a key's play counts and sums: [`PlayKind::Normal`]'s,
/// then [`PlayKind::Practice`]'s.
const PART_COLUMNS: [[&str; 7]; 2] = [PlayKind::Normal.columns(), PlayKind::Practice.columns()];

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

/// Counts `c` in the caller's transaction, the only writer of `scores`,
/// `progress` and `counted`: once per play identity, merging its values
/// into its key's row, and storing its progress when its breakdown is the
/// one kept. A play counted before only has its stamp refreshed.
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
    // The play adds one play and its sums to its kind's columns and 0 to
    // the other kind's, each saturating, in the same upsert: addition
    // commutes, so live counting and a rebuild agree in any order.
    let parts = PART_COLUMNS.as_flattened();
    let added = parts.iter().map(|c| {
        format!(
            "{c} = CASE WHEN {c} > {max} - excluded.{c} THEN {max} ELSE {c} + excluded.{c} END",
            max = i64::MAX
        )
    });
    let [s0, s1, s2, s3, s4] = c.sums.counts.map(i64::from);
    let own = [1, s0, s1, s2, s3, s4, count(c.sums.play_us)];
    let values =
        [PlayKind::Normal, PlayKind::Practice].map(|k| if k == c.kind { own } else { [0; 7] });
    let (cleared, play) = (b.map(|b| b.cleared.clone()), b.map(|b| b.play.to_bytes()));
    let best: [&dyn rusqlite::ToSql; 11] = [
        &k.preset_sha256,
        &k.ln_mode,
        &k.chart_sha256,
        &count(current.clears),
        &b.map(|b| b.ex),
        &b.map(|b| b.bp),
        &b.map(|b| b.max_combo),
        &cleared,
        &b.is_some_and(|b| b.full_combo),
        &b.map(|b| b.play.body_sha256),
        &play,
    ];
    let params: Vec<&dyn rusqlite::ToSql> = best
        .into_iter()
        .chain(
            values
                .as_flattened()
                .iter()
                .map(|v| v as &dyn rusqlite::ToSql),
        )
        .collect();
    let placeholders = (1..=params.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!(
            "INSERT INTO scores (preset_sha256, ln_mode, chart_sha256, clears, best_ex,
                 best_bp, best_combo, cleared, full_combo, play_sha256, play, {parts})
             VALUES ({placeholders})
             ON CONFLICT (preset_sha256, ln_mode, chart_sha256) DO UPDATE SET
                 clears = excluded.clears, best_ex = excluded.best_ex,
                 best_bp = excluded.best_bp, best_combo = excluded.best_combo,
                 cleared = excluded.cleared, full_combo = excluded.full_combo,
                 play_sha256 = excluded.play_sha256, play = excluded.play, {added}",
            parts = parts.join(", "),
            added = added.collect::<Vec<_>>().join(", "),
        ),
        params.as_slice(),
    )?;
    // The progress follows the breakdown: replaced whole when this play's
    // is the one kept, left alone otherwise. A kept play without a
    // progress to store leaves none, rather than another play's.
    if c.best.is_some() && b.is_some_and(|b| b.play.body_sha256 == id.body_sha256) {
        match c.progress.as_deref().filter(|p| p.len() <= MAX_PROGRESS) {
            Some(p) => {
                let bytes: Vec<u8> = p.iter().copied().map(judgment_byte).collect();
                conn.execute(
                    "INSERT OR REPLACE INTO progress
                         (preset_sha256, ln_mode, chart_sha256, play_sha256, judgments)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        k.preset_sha256,
                        k.ln_mode,
                        k.chart_sha256,
                        id.body_sha256,
                        bytes
                    ],
                )?;
            }
            None => {
                conn.execute(
                    "DELETE FROM progress
                     WHERE preset_sha256 = ?1 AND ln_mode = ?2 AND chart_sha256 = ?3",
                    params![k.preset_sha256, k.ln_mode, k.chart_sha256],
                )?;
            }
        }
    }
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
    // A best and its clears come from normal plays alone, so they are
    // checked against those; the score's play count adds both kinds.
    let plays = count(0, "a negative play count")?;
    let practice_plays = count(9, "a negative play count")?;
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
        plays: plays.saturating_add(practice_plays),
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
const SCORE_COLUMNS: &str = "plays, clears, best_ex, best_bp, best_combo, cleared, full_combo, \
     play_sha256, play, practice_plays";

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

/// What a play asks of its best's progress: the judged targets of its
/// chart, and whether its preset counts an empty POOR as a POOR, by which
/// the breakdown's POOR count includes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressQuery {
    /// The chart's judged targets, its taps and long notes.
    pub targets: u32,
    /// The play preset's `empty_poor.counted`.
    pub empty_poor_counted: bool,
}

/// The best's progress as [`play_best`] read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The judgments of the best's play, consistent with its breakdown.
    Known(Box<[Judgment]>),
    /// No best, or none stored with it.
    Missing,
    /// A stored progress refused, for the reason given; the best stays.
    Refused(&'static str),
}

/// A play's best and its progress, from one statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayBest {
    /// The stored score, if a row exists.
    pub score: Option<Score>,
    /// The progress, when asked for.
    pub progress: Option<Progress>,
}

/// The stored score of `key` and, when `query` asks, its best's progress,
/// read in one statement so both come from one snapshot (design.md
/// "Personal Bests"). The progress is fetched only when its length is the
/// query's targets, checked from its borrowed bytes against the breakdown
/// it is read with, and copied only when accepted. A refused progress
/// leaves the score as read.
pub(crate) fn play_best(
    conn: &Connection,
    key: &Key,
    query: Option<ProgressQuery>,
) -> rusqlite::Result<PlayBest> {
    let Some(q) = query else {
        return Ok(PlayBest {
            score: score(conn, key)?,
            progress: None,
        });
    };
    conn.query_row(
        "SELECT s.plays, s.clears, s.best_ex, s.best_bp, s.best_combo, s.cleared,
             s.full_combo, s.play_sha256, s.play, s.practice_plays, p.play_sha256,
             length(p.judgments),
             CASE WHEN length(p.judgments) = ?4 THEN p.judgments END
         FROM scores s LEFT JOIN progress p USING (preset_sha256, ln_mode, chart_sha256)
         WHERE s.preset_sha256 = ?1 AND s.ln_mode = ?2 AND s.chart_sha256 = ?3",
        params![key.preset_sha256, key.ln_mode, key.chart_sha256, q.targets],
        |r| {
            let score = score_of(r, 0)?;
            let progress = match &score.best {
                None => Progress::Missing,
                Some(best) => progress_of(r, 10, &best.play, q)?,
            };
            Ok(PlayBest {
                score: Some(score),
                progress: Some(progress),
            })
        },
    )
    .optional()
    .map(|read| {
        read.unwrap_or(PlayBest {
            score: None,
            progress: Some(Progress::Missing),
        })
    })
}

/// The progress in columns `at` (its play's hash), `at + 1` (its length)
/// and `at + 2` (its bytes, when of the query's length) of `r`, checked
/// against `play`, the breakdown it is read with.
fn progress_of(
    r: &rusqlite::Row<'_>,
    at: usize,
    play: &Breakdown,
    q: ProgressQuery,
) -> rusqlite::Result<Progress> {
    let refused = |why| Ok(Progress::Refused(why));
    let sha = match r.get_ref(at)? {
        ValueRef::Null => return Ok(Progress::Missing),
        ValueRef::Blob(sha) => sha,
        _ => return refused("its play hash is not a hash"),
    };
    if sha != play.body_sha256 {
        return refused("it is another play's than the breakdown's");
    }
    let len: i64 = r.get(at + 1)?;
    if u32::try_from(len).ok() != Some(play.notes) {
        return refused("its length is not the breakdown's notes");
    }
    if len != i64::from(q.targets) {
        return refused("its length is not the chart's judged targets");
    }
    let ValueRef::Blob(bytes) = r.get_ref(at + 2)? else {
        return refused("its judgments are not bytes");
    };
    let mut counts = [0_u32; 5];
    for &b in bytes {
        let Some(c) = counts.get_mut(usize::from(b)) else {
            return refused("a judgment is out of range");
        };
        *c = c.saturating_add(1);
    }
    // An empty POOR the preset counted is in the breakdown's POOR count
    // and judged no target.
    let [p0, p1, p2, p3, poor] = play.counts;
    let targets_poor = if q.empty_poor_counted {
        poor.checked_sub(play.empty_poor)
    } else {
        Some(poor)
    };
    if Some(counts) != targets_poor.map(|poor| [p0, p1, p2, p3, poor]) {
        return refused("its counts are not the breakdown's");
    }
    let judgments = bytes
        .iter()
        .filter_map(|&b| Judgment::ALL.get(usize::from(b)).copied())
        .collect();
    Ok(Progress::Known(judgments))
}

/// The player's totals per kind: one sum of each column over every row of
/// `scores`, every preset and LN mode included, 0 with no row. A negative
/// stored count or sum, or a part's total past SQLite's integer range, is
/// a failed read, never a default.
pub(crate) fn totals(conn: &Connection) -> rusqlite::Result<TotalsByKind> {
    let parts = PART_COLUMNS.as_flattened();
    let sums = parts
        .iter()
        .map(|c| format!("SUM({c})"))
        .collect::<Vec<_>>()
        .join(", ");
    let least = parts.join(", ");
    conn.query_row(
        &format!("SELECT MIN(MIN(clears, {least})), SUM(clears), {sums} FROM scores"),
        [],
        |r| {
            let negative = |i: usize| {
                rusqlite::Error::FromSqlConversionFailure(
                    i,
                    Type::Integer,
                    Box::new(Inconsistent("a negative count or sum")),
                )
            };
            if r.get::<_, Option<i64>>(0)?.is_some_and(|m| m < 0) {
                return Err(negative(0));
            }
            let total = |i: usize| -> rusqlite::Result<u64> {
                let n: Option<i64> = r.get(i)?;
                u64::try_from(n.unwrap_or(0)).map_err(|_| negative(i))
            };
            // Each kind's play count, five counts and play time, from
            // column `at` on.
            let part = |at: usize, clears: u64| -> rusqlite::Result<Totals> {
                Ok(Totals {
                    plays: total(at)?,
                    clears,
                    counts: [
                        total(at + 1)?,
                        total(at + 2)?,
                        total(at + 3)?,
                        total(at + 4)?,
                        total(at + 5)?,
                    ],
                    play_us: total(at + 6)?,
                })
            };
            Ok(TotalsByKind {
                normal: part(2, total(1)?)?,
                practice: part(9, 0)?,
            })
        },
    )
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

    use super::{
        BREAKDOWN_BYTES, Best, Breakdown, Contribution, Key, LN_MODE, PlayId, PlayKind, Score,
        Sums, Totals, TotalsByKind, Verified,
    };
    use crate::test_common as common;

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

    /// The sums the play hashed `body` adds, each derived from `body` so
    /// plays differ.
    fn sums(body: u8) -> Sums {
        let v = u32::from(body);
        Sums {
            counts: [v, v + 1, 2 * v, v * v, 3],
            play_us: 1_000_003 * u64::from(body),
        }
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
            progress: None,
            ineligible: best.is_none().then_some(super::Ineligible::NotFullPlay),
            best,
            sums: sums(body),
            kind: PlayKind::Normal,
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
        fold_both(cs).0
    }

    /// The score and the totals counting `cs` in order stores.
    fn fold_both(cs: &[&Contribution]) -> (Score, Totals) {
        let conn = database();
        for c in cs {
            super::ingest(&conn, c).unwrap();
        }
        (
            super::score(&conn, &cs[0].key).unwrap().unwrap(),
            super::totals(&conn).unwrap().sum(),
        )
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
    fn the_bests_ddl_checks_the_breakdown_length() {
        assert!(crate::db::bests_ddl().contains(&format!("length(play) = {BREAKDOWN_BYTES})")));
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
        // Past the CHECKs, as a file written by other means could be.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        // Each change breaks one relation only, and is read back as that
        // one.
        for (set, undo, why) in [
            (
                "plays = -1, clears = 0",
                "plays = 1, clears = 1",
                "a negative play count",
            ),
            (
                "practice_plays = -1",
                "practice_plays = 0",
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
        let (first, totals) = fold_both(&all[0]);
        // Every play's sums, added: bodies 4 to 8.
        assert_eq!(
            totals,
            Totals {
                plays: 5,
                clears: 3,
                counts: [30, 35, 60, 190, 15],
                play_us: 30_000_090,
            }
        );
        let b = first.best.as_ref().unwrap();
        assert_eq!(
            (first.plays, first.clears, b.ex, b.bp, b.max_combo),
            (5, 3, 6, 1, 9)
        );
        assert_eq!((b.cleared.clone(), b.full_combo), (vec![0b11], true));
        assert_eq!(b.play, play_of(&cs[1]));
        assert_eq!(b.ex, b.play.ex);
        for order in &all[1..] {
            assert_eq!(fold_both(order), (first.clone(), totals));
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

    /// A play counted again adds nothing to the totals.
    #[test]
    fn a_play_counted_again_adds_no_totals() {
        let c = contribution(5, None);
        let once = database();
        super::ingest(&once, &c).unwrap();
        let twice = database();
        super::ingest(&twice, &c).unwrap();
        assert_eq!(
            super::ingest(&twice, &c).unwrap(),
            super::Ingested::AlreadyCounted
        );
        let expected = Totals {
            plays: 1,
            clears: 0,
            counts: [5, 6, 10, 25, 3],
            play_us: 5_000_015,
        };
        assert_eq!(super::totals(&once).unwrap().sum(), expected);
        assert_eq!(super::totals(&twice).unwrap().sum(), expected);
    }

    /// A sum saturates at the largest stored integer, and the totals of an
    /// empty table are 0.
    #[test]
    fn a_sum_saturates() {
        let conn = database();
        assert_eq!(super::totals(&conn).unwrap(), TotalsByKind::default());
        let first = contribution(5, None);
        super::ingest(&conn, &first).unwrap();
        conn.execute(
            "UPDATE scores SET sum_play_us = ?1, sum_perfect = ?1",
            [i64::MAX - 1],
        )
        .unwrap();
        super::ingest(&conn, &contribution(6, None)).unwrap();
        let t = super::totals(&conn).unwrap().sum();
        assert_eq!((t.play_us, t.counts[0]), (i64::MAX as u64, i64::MAX as u64));
        assert_eq!((t.plays, t.counts[1]), (2, 13));
    }

    /// A negative stored count or sum of either kind, even beside a larger
    /// positive one, or a part's total past SQLite's integer range is a
    /// failed read of the totals, never a default.
    #[test]
    fn a_negative_sum_is_a_read_error() {
        let conn = database();
        super::ingest(&conn, &contribution(5, None)).unwrap();
        let mut other = contribution(6, None);
        other.key.chart_sha256 = [3; 32];
        super::ingest(&conn, &other).unwrap();
        // Past the CHECKs, as a file written by other means could be.
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        for column in super::PART_COLUMNS
            .as_flattened()
            .iter()
            .copied()
            .chain(["clears"])
        {
            conn.execute(
                &format!("UPDATE scores SET {column} = -1 WHERE chart_sha256 = ?1"),
                [[2_u8; 32]],
            )
            .unwrap();
            let e = super::totals(&conn).unwrap_err();
            assert!(
                matches!(e, rusqlite::Error::FromSqlConversionFailure(..))
                    && e.to_string().contains("a negative count or sum"),
                "{column}: {e}"
            );
            conn.execute(
                &format!("UPDATE scores SET {column} = 1 WHERE chart_sha256 = ?1"),
                [[2_u8; 32]],
            )
            .unwrap();
            assert!(super::totals(&conn).is_ok(), "{column} restored");
        }
        conn.execute("UPDATE scores SET sum_play_us = ?1", [i64::MAX])
            .unwrap();
        let e = super::totals(&conn).unwrap_err();
        assert!(e.to_string().contains("integer overflow"), "{e}");
    }

    /// The play time `Contribution::of` gives `record` of the chart in
    /// `text`, as the live path makes it.
    fn time_of(text: &str, record: &replay::PlayRecord) -> u64 {
        let dir = tempfile::tempdir().unwrap();
        let chart =
            replay::verify::load_recorded_chart(record, text.as_bytes(), dir.path()).unwrap();
        let re = replay::verify::evaluate_record(record, &chart, None).unwrap();
        let preset = replay::verify::check_record(record).unwrap();
        let c = Contribution::of(
            PlayId::of(String::new(), record, 0, 0),
            record,
            &Verified {
                completed: re.evaluation.completed,
            },
            &re.evaluation,
            &chart,
            &preset,
        );
        c.sums.play_us
    }

    /// A play's time reaches its last judged note's end (a long note's
    /// end, where the source counts its head: 2 s in the first three
    /// cases), never past an aborted play's finalized watermark, less a
    /// practice round's section start, and is 0 with no judged note.
    // Source: SF-014
    #[test]
    fn a_plays_time_reaches_its_last_judged_note() {
        use common::{MS, key, record_of};
        // One long note on key 1 from 2 s to 10 s: the chart's last note.
        let long = "#BPM 120\n#LNTYPE 1\n#00151:01\n#00551:01\n";
        let at = |ms: i64| bms_model::TimeUs(ms * MS);
        let held = record_of(long, 0, vec![key(1, 2_000 * MS, 1), key(1, 10_010 * MS, 0)]);
        assert_eq!(time_of(long, &held), 10_000_000, "held to its end");
        let missed = record_of(long, 0, Vec::new());
        assert_eq!(time_of(long, &missed), 10_000_000, "its head missed");
        let early = record_of(long, 0, vec![key(1, 2_000 * MS, 1), key(1, 6_000 * MS, 0)]);
        assert_eq!(time_of(long, &early), 10_000_000, "released at 6 s");
        let mut aborted = record_of(long, 0, Vec::new());
        aborted.status.end = replay::EndStatus::EndedMidway;
        aborted.status.finalized = at(3_000);
        assert_eq!(time_of(long, &aborted), 3_000_000, "aborted at 3 s");
        // Taps on key 1 at 2 s, 2.5 s and 3 s; ended at 2.9 s, the 2.5 s
        // tap missed (judged at 2.68 s), the 3 s one not reached.
        let taps = "#BPM 120\n#00111:01010100\n";
        let mut midway = record_of(taps, 0, vec![key(1, 2_000 * MS, 1), key(1, 2_050 * MS, 0)]);
        midway.status.end = replay::EndStatus::EndedMidway;
        midway.status.finalized = at(2_900);
        assert_eq!(time_of(taps, &midway), 2_500_000, "the missed tap counts");
        // A round of the long note from 1.5 s: 10 s less 1.5 s.
        let mut round = record_of(long, 0, vec![key(1, 2_000 * MS, 1), key(1, 10_010 * MS, 0)]);
        let preset = rules::tapestry();
        round.play_type = replay::PlayType::SectionRepeat(replay::Practice {
            start: at(1_500),
            end: at(10_500),
            lead_in: at(1_000),
            attempt: 1,
            initial_gauges: preset
                .gauges
                .iter()
                .map(|g| replay::InitialGauge {
                    id: g.id.clone(),
                    value: g.start,
                })
                .collect(),
        });
        assert_eq!(time_of(long, &round), 8_500_000, "less the section's start");
        // Ended before any note was judged.
        let mut none = record_of(taps, 0, Vec::new());
        none.status.end = replay::EndStatus::EndedMidway;
        none.status.finalized = at(1_000);
        assert_eq!(time_of(taps, &none), 0, "nothing judged");
    }

    /// A practice round adds its play, counts and time to the practice part
    /// only, and a normal play to the normal part only; the totals a skin
    /// reads add both, and the key's play count (number 77) counts both.
    #[test]
    fn a_practice_round_counts_in_its_own_part() {
        let conn = database();
        let normal = contribution(5, Some(best(play(5, 6, 2), 4, 1, false)));
        let mut practice = contribution(7, None);
        practice.kind = PlayKind::Practice;
        super::ingest(&conn, &normal).unwrap();
        super::ingest(&conn, &practice).unwrap();
        let parts = super::totals(&conn).unwrap();
        assert_eq!(
            parts.practice,
            Totals {
                plays: 1,
                clears: 0,
                counts: [7, 8, 14, 49, 3],
                play_us: 7_000_021,
            },
            "practice"
        );
        assert_eq!(
            parts.normal,
            Totals {
                plays: 1,
                clears: 1,
                counts: [5, 6, 10, 25, 3],
                play_us: 5_000_015,
            },
            "normal"
        );
        assert_eq!(
            parts.sum(),
            Totals {
                plays: 2,
                clears: 1,
                counts: [12, 14, 24, 74, 6],
                play_us: 12_000_036,
            }
        );
        let score = super::score(&conn, &normal.key).unwrap().unwrap();
        assert_eq!((score.plays, score.clears), (2, 1));
    }

    /// A mid-chart start and a section repeat are practice rounds at 1×,
    /// at another speed and when quit midway; a play of the whole chart at
    /// another speed or quit midway is a normal play.
    #[test]
    fn a_plays_kind_is_practice_only_for_a_practice_round() {
        let full = common::record_of("#BPM 120\n#00111:01\n", 0, Vec::new());
        let preset = rules::tapestry();
        let practice = replay::Practice {
            start: bms_model::TimeUs(1_500 * common::MS),
            end: bms_model::TimeUs(4_500 * common::MS),
            lead_in: bms_model::TimeUs(1_000 * common::MS),
            attempt: 1,
            initial_gauges: preset
                .gauges
                .iter()
                .map(|g| replay::InitialGauge {
                    id: g.id.clone(),
                    value: g.start,
                })
                .collect(),
        };
        let variant = |play_type: replay::PlayType, speed: bms_model::Speed, midway: bool| {
            let mut r = full.clone();
            r.play_type = play_type;
            r.speed = speed;
            if midway {
                r.status.end = replay::EndStatus::EndedMidway;
            }
            r
        };
        let (normal, fastest) = (bms_model::Speed::NORMAL, bms_model::Speed::FASTEST);
        for (round, name) in [
            (
                replay::PlayType::MidChartStart(practice.clone()),
                "mid-chart start",
            ),
            (replay::PlayType::SectionRepeat(practice), "section repeat"),
        ] {
            for (speed, midway, how) in [
                (normal, false, "at 1x"),
                (fastest, false, "at another speed"),
                (normal, true, "quit midway"),
            ] {
                assert_eq!(
                    PlayKind::of(&variant(round.clone(), speed, midway)),
                    PlayKind::Practice,
                    "{name} {how}"
                );
            }
        }
        for (speed, midway, how) in [
            (normal, false, "at 1x"),
            (fastest, false, "at another speed"),
            (normal, true, "quit midway"),
        ] {
            assert_eq!(
                PlayKind::of(&variant(replay::PlayType::Full, speed, midway)),
                PlayKind::Normal,
                "full play {how}"
            );
        }
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
        assert_eq!(size_of::<Contribution>(), 360);
    }
}
