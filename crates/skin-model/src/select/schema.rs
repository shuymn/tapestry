//! The selection screen's schema: the fields and built-in timers a
//! selection skin reads, each at a fixed slot, filled from the selection
//! view (docs/stage3.md §3, the Selection profile).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use crate::schema::{Export, Field, FieldTy, Schema, TimerDef};

/// The largest value an integer field reads: one below `i32::MAX`, which a
/// source number widget reserves, as play's readouts.
pub const VALUE_MAX: i32 = i32::MAX - 1;

const T: Export = Export::TapestryOnly;

const fn flag(name: &'static str) -> Field {
    Field {
        name,
        ty: FieldTy::Bool,
        export: T,
    }
}

const fn int(name: &'static str, lo: i32, hi: i32) -> Field {
    Field {
        name,
        ty: FieldTy::Int { lo, hi },
        export: T,
    }
}

const fn value(name: &'static str) -> Field {
    int(name, 0, VALUE_MAX)
}

const fn timer(name: &'static str) -> TimerDef {
    TimerDef { name, export: T }
}

/// The selection screen's state: the cursor's row and the chart under it,
/// from the selection view; timers from the screen's lifetime and its UI
/// events.
pub static SELECTION: Schema = Schema {
    fields: &[
        flag("cursor.chart"),
        flag("cursor.folder"),
        flag("cursor.all"),
        value("cursor.charts"),
        flag("selected.played"),
        value("selected.plays"),
        flag("selected.best"),
        value("selected.ex"),
        value("selected.bp"),
        int(
            "selected.lamp",
            0,
            // `LampKey::COUNT - 1`.
            7,
        ),
        flag("selected.max_ex_valid"),
        value("selected.max_ex"),
        flag("selected.rate_valid"),
        int("selected.rate_percent", 0, 100),
        int("selected.rate_hundredths", 0, 99),
        int("selected.rank", 0, 7),
        value("selected.next_rank"),
        flag("selected.summary"),
        value("selected.notes"),
        value("selected.bpm_max"),
        value("selected.bpm_min"),
        flag("selected.bpm_change"),
        int("selected.level", -VALUE_MAX, VALUE_MAX),
        value("selected.length_min"),
        int("selected.length_sec", 0, 59),
    ],
    timers: &[
        timer("screen"),
        timer("select.change"),
        timer("panel1"),
        timer("panel2"),
        timer("panel3"),
    ],
    enums: &[],
};

/// The field slots of [`SELECTION`].
pub mod field {
    /// Whether the cursor is on a chart row.
    pub const CURSOR_CHART: u16 = 0;
    /// Whether the cursor is on a folder row.
    pub const CURSOR_FOLDER: u16 = 1;
    /// Whether the cursor is on the "all charts" row.
    pub const CURSOR_ALL: u16 = 2;
    /// The listed charts under a folder row or the "all charts" row under
    /// the cursor; 0 on another row.
    pub const CURSOR_CHARTS: u16 = 3;
    /// Whether the chart under the cursor was played under the chosen
    /// preset.
    pub const SELECTED_PLAYED: u16 = 4;
    /// Its plays; 0 while `selected.played` is false.
    pub const SELECTED_PLAYS: u16 = 5;
    /// Whether it has a best under the chosen preset.
    pub const SELECTED_BEST: u16 = 6;
    /// The best's EX score; 0 while `selected.best` is false.
    pub const SELECTED_EX: u16 = 7;
    /// The best's BP.
    pub const SELECTED_BP: u16 = 8;
    /// Its lamp, a [`LampKey`](crate::LampKey) index: `NoPlay` without a
    /// best, and on a row that is not a chart.
    pub const SELECTED_LAMP: u16 = 9;
    /// Whether the largest EX score its summary allows is known.
    pub const SELECTED_MAX_EX_VALID: u16 = 10;
    /// That largest EX score; 0 while unknown.
    pub const SELECTED_MAX_EX: u16 = 11;
    /// Whether the rate and rank are known: a best and a largest EX score
    /// above 0.
    pub const SELECTED_RATE_VALID: u16 = 12;
    /// `⌊100 · ex / max_ex⌋`.
    pub const SELECTED_RATE_PERCENT: u16 = 13;
    /// `⌊10 000 · ex / max_ex⌋ mod 100`.
    pub const SELECTED_RATE_HUNDREDTHS: u16 = 14;
    /// The rank, 0 (lowest) to 7 (highest): the greatest `k` with
    /// `27 · ex ≥ low_k · max_ex`, `low` being 0, 6, 9, 12, 15, 18, 21, 24.
    pub const SELECTED_RANK: u16 = 15;
    /// The EX score to the next rank of 3/27 steps, or to the largest.
    pub const SELECTED_NEXT_RANK: u16 = 16;
    /// Whether the chart has a summary.
    pub const SELECTED_SUMMARY: u16 = 17;
    /// Its judged notes: taps and long notes.
    pub const SELECTED_NOTES: u16 = 18;
    /// Its fastest and slowest tempos, whole BPM rounded down.
    pub const SELECTED_BPM_MAX: u16 = 19;
    pub const SELECTED_BPM_MIN: u16 = 20;
    /// Whether its tempos differ.
    pub const SELECTED_BPM_CHANGE: u16 = 21;
    /// Its `#PLAYLEVEL` as a number.
    pub const SELECTED_LEVEL: u16 = 22;
    /// Its length's whole minutes, and the seconds past them.
    pub const SELECTED_LENGTH_MIN: u16 = 23;
    pub const SELECTED_LENGTH_SEC: u16 = 24;
    /// The number of fields.
    pub const COUNT: usize = 25;
}

/// The built-in timer slots of [`SELECTION`]; `when` timers follow from
/// [`BUILTIN`](timer::BUILTIN).
pub mod timer {
    /// The screen's entry.
    pub const SCREEN: u16 = 0;
    /// The cursor's latest move.
    pub const CHANGE: u16 = 1;
    /// Panel 1 to 3's opening; off while it is closed.
    pub const PANEL1: u16 = 2;
    pub const PANEL2: u16 = 3;
    pub const PANEL3: u16 = 4;
    /// The number of built-in timers.
    pub const BUILTIN: u16 = 5;
}

#[cfg(test)]
mod tests {
    use super::{SELECTION, field, timer};

    #[test]
    fn the_selection_schema_is_sound_and_its_slots_match() {
        SELECTION.check().unwrap();
        assert_eq!(SELECTION.fields.len(), field::COUNT);
        assert_eq!(SELECTION.timers.len(), usize::from(timer::BUILTIN));
        let names: Vec<&str> = SELECTION.fields.iter().map(|f| f.name).collect();
        for (slot, name) in [
            (field::CURSOR_CHARTS, "cursor.charts"),
            (field::SELECTED_LAMP, "selected.lamp"),
            (field::SELECTED_RANK, "selected.rank"),
            (field::SELECTED_LEVEL, "selected.level"),
            (field::SELECTED_LENGTH_SEC, "selected.length_sec"),
        ] {
            assert_eq!(names[usize::from(slot)], name);
        }
        assert_eq!(
            SELECTION.timers[usize::from(timer::CHANGE)].name,
            "select.change"
        );
        assert_eq!(SELECTION.timers[usize::from(timer::PANEL3)].name, "panel3");
    }
}
