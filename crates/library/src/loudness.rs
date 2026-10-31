//! The loudness cache (design.md "Algorithms", per-chart volume
//! normalization): what an analysis measured, by everything that decides
//! it. The key is content, not a path, so moved or copied charts share a
//! row and a scan never touches it; the gain is not stored, so the formula
//! can change without analyzing again.

use rusqlite::{Connection, OptionalExtension as _, params};

/// Everything a loudness measurement depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LoudnessKey {
    /// The chart's resolved hash (its notes, BGM and sound names).
    pub resolved_hash: [u8; 32],
    /// The sound-content digest of its bank (what its sound files hold).
    pub sounds: [u8; 32],
    /// The mixer's version.
    pub mixer_version: u32,
    /// The analysis's version.
    pub analysis_version: u32,
    /// The device rate it was mixed at.
    pub rate: u32,
}

/// What an analysis measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessRow {
    /// The integrated loudness, LUFS; `None` when every block was gated.
    pub integrated: Option<f64>,
    /// The true peak, linear.
    pub true_peak: f64,
    /// Whether a gating block reached the meter's ceiling.
    pub over_range: bool,
}

/// The row of `key`, if one is stored.
pub fn get(conn: &Connection, key: &LoudnessKey) -> rusqlite::Result<Option<LoudnessRow>> {
    conn.query_row(
        "SELECT integrated, true_peak, over_range FROM loudness
         WHERE resolved_hash = ?1 AND sounds = ?2 AND mixer_version = ?3
           AND analysis_version = ?4 AND rate = ?5",
        params![
            key.resolved_hash,
            key.sounds,
            i64::from(key.mixer_version),
            i64::from(key.analysis_version),
            i64::from(key.rate),
        ],
        |r| {
            Ok(LoudnessRow {
                integrated: r.get(0)?,
                true_peak: r.get(1)?,
                over_range: r.get(2)?,
            })
        },
    )
    .optional()
}

/// Stores `row` under `key` in one transaction, replacing a row of the same
/// key, and removes every row of another mixer or analysis version, which
/// no build reading these versions looks up again.
pub fn put(conn: &mut Connection, key: &LoudnessKey, row: &LoudnessRow) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO loudness
             (resolved_hash, sounds, mixer_version, analysis_version, rate,
              integrated, true_peak, over_range)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            key.resolved_hash,
            key.sounds,
            i64::from(key.mixer_version),
            i64::from(key.analysis_version),
            i64::from(key.rate),
            row.integrated,
            row.true_peak,
            row.over_range,
        ],
    )?;
    tx.execute(
        "DELETE FROM loudness WHERE mixer_version <> ?1 OR analysis_version <> ?2",
        params![
            i64::from(key.mixer_version),
            i64::from(key.analysis_version)
        ],
    )?;
    tx.commit()
}
