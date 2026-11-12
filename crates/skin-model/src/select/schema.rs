//! The selection screen's schema: the fields and built-in timers a
//! selection skin reads, each at a fixed slot, filled from the selection
//! view (docs/stage3.md §3, the Selection profile).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#![deny(clippy::float_arithmetic, clippy::float_cmp)]

use crate::JUDGE_CLASS_MAX;
use crate::play::schema::{FPS_MAX, YEAR_MAX};
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
/// from the selection view; the settings; the application's readouts,
/// play's fields of the same names, from each frame; timers from the
/// screen's lifetime and its UI events.
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
        value("selected.rate_percent"),
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
        int("selected.judge_class", 0, JUDGE_CLASS_MAX),
        flag("selected.judge_class_valid"),
        flag("selected.stagefile"),
        flag("selected.banner"),
        int("settings.display_offset", -VALUE_MAX, VALUE_MAX),
        int("settings.volume", 0, 100),
        flag("settings.floating"),
        value("settings.duration"),
        value("settings.green"),
        int("settings.fix_hispeed", 0, 4),
        int("settings.order", -1, 4),
        int("settings.gauge", -1, 5),
        flag("settings.sudden"),
        flag("settings.lift"),
        flag("settings.hidden"),
        int("settings.gauge_shift", 0, 1),
        int("settings.judge_algorithm", -1, 1),
        int("app.fps", 0, FPS_MAX),
        int("clock.year", 0, YEAR_MAX),
        int("clock.month", 0, 12),
        int("clock.day", 0, 31),
        int("clock.hour", 0, 23),
        int("clock.minute", 0, 59),
        int("clock.second", 0, 59),
        int("app.uptime_h", 0, i32::MAX),
        int("app.uptime_m", 0, 59),
        int("app.uptime_s", 0, 59),
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
    /// The chart's judgment class, 0 the hardest to 4 the easiest, from
    /// its library entry's rank with one validation, as the reference's
    /// song database computes it; 0 and not valid without a class or off a
    /// chart row.
    pub const SELECTED_JUDGE_CLASS: u16 = 25;
    pub const SELECTED_JUDGE_CLASS_VALID: u16 = 26;
    /// Whether the cursor's chart's stage file and banner are drawable:
    /// true only while the selection's chart table is current for the
    /// cursor generation and holds that image (options 190 to 193). The
    /// player writes them after the view's fields, which reset them.
    pub const SELECTED_STAGEFILE: u16 = 27;
    pub const SELECTED_BANNER: u16 = 28;
    /// The settings' fields, then the application's readouts, from here
    /// on, which a view leaves as they are
    /// ([`SelectionSettings`](crate::SelectionSettings),
    /// [`SelectionFrame::app`](crate::SelectionFrame::app)).
    pub const SETTINGS: u16 = 29;
    /// The display offset in whole ms.
    pub const SETTINGS_DISPLAY_OFFSET: u16 = 29;
    /// The master gain's number, `(int)(gain · 100f)`.
    pub const SETTINGS_VOLUME: u16 = 30;
    /// Whether the hi-speed is Floating.
    pub const SETTINGS_FLOATING: u16 = 31;
    /// In Floating, the travel duration of the target green number `G`,
    /// `⌊5 · G / 3⌋` ms, and `G`; 0 in Classic.
    pub const SETTINGS_DURATION: u16 = 32;
    pub const SETTINGS_GREEN: u16 = 33;
    /// The source's fixed hi-speed index: 0 in Classic; in Floating 1 to
    /// 4 by the reference tempo, start, maximum, main and minimum.
    pub const SETTINGS_FIX_HISPEED: u16 = 34;
    /// The source's sorter index of the order: title 0, artist 1, level 4;
    /// −1 for an order it has no sorter for.
    pub const SETTINGS_ORDER: u16 = 35;
    /// The displayed gauge's look, assist-easy 0 to hazard 5; −1 without
    /// one.
    pub const SETTINGS_GAUGE: u16 = 36;
    /// Whether SUDDEN+, LIFT and HIDDEN+ are on.
    pub const SETTINGS_SUDDEN: u16 = 37;
    pub const SETTINGS_LIFT: u16 = 38;
    pub const SETTINGS_HIDDEN: u16 = 39;
    /// The source's gauge auto-shift index for the failure handling
    /// `player.toml` sets: 0 (none) when a failed play closes, 1
    /// (continue) when it plays to the end.
    pub const SETTINGS_GAUGE_SHIFT: u16 = SETTINGS_HIDDEN + 1;
    /// The source's judgment-algorithm index of the chosen preset's press
    /// assignment: 0 (combo) for combo priority, 1 (duration) for the
    /// nearest note, earlier at a tie; −1 without a preset.
    pub const SETTINGS_JUDGE_ALGORITHM: u16 = SETTINGS_GAUGE_SHIFT + 1;
    /// The application's readouts of the frame
    /// ([`SelectionFrame::app`](crate::SelectionFrame::app)), as play's
    /// fields of the same names, after the settings: the frames presented
    /// in the last second, the local year, month (1 to 12), day, 24-hour
    /// hour, minute and second, then the uptime's whole hours (unwrapped),
    /// its minutes within the hour and its seconds within the minute.
    pub const APP_FPS: u16 = SETTINGS_JUDGE_ALGORITHM + 1;
    pub const CLOCK_YEAR: u16 = APP_FPS + 1;
    pub const CLOCK_SECOND: u16 = APP_FPS + 6;
    pub const APP_UPTIME_H: u16 = APP_FPS + 7;
    pub const APP_UPTIME_S: u16 = APP_FPS + 9;
    /// The number of fields.
    pub const COUNT: usize = APP_UPTIME_S as usize + 1;
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
            (
                field::SELECTED_JUDGE_CLASS_VALID,
                "selected.judge_class_valid",
            ),
            (field::SELECTED_STAGEFILE, "selected.stagefile"),
            (field::SELECTED_BANNER, "selected.banner"),
            (field::SETTINGS, "settings.display_offset"),
            (field::SETTINGS_DISPLAY_OFFSET, "settings.display_offset"),
            (field::SETTINGS_VOLUME, "settings.volume"),
            (field::SETTINGS_GREEN, "settings.green"),
            (field::SETTINGS_ORDER, "settings.order"),
            (field::SETTINGS_HIDDEN, "settings.hidden"),
            (field::SETTINGS_GAUGE_SHIFT, "settings.gauge_shift"),
            (field::SETTINGS_JUDGE_ALGORITHM, "settings.judge_algorithm"),
            (field::APP_FPS, "app.fps"),
            (field::CLOCK_YEAR, "clock.year"),
            (field::CLOCK_SECOND, "clock.second"),
            (field::APP_UPTIME_H, "app.uptime_h"),
            (field::APP_UPTIME_S, "app.uptime_s"),
        ] {
            assert_eq!(names[usize::from(slot)], name);
        }
        // The image fields are the selected chart's, before the settings,
        // which a view leaves as they are: the settings and the readouts
        // moved by two.
        assert_eq!(
            (field::SELECTED_STAGEFILE, field::SELECTED_BANNER),
            (27, 28)
        );
        assert_eq!((field::SETTINGS, field::APP_FPS), (29, 42));
        assert_eq!(
            SELECTION.timers[usize::from(timer::CHANGE)].name,
            "select.change"
        );
        assert_eq!(SELECTION.timers[usize::from(timer::PANEL3)].name, "panel3");
        // The application's readouts read as play's fields of the same
        // names, with the same ranges.
        for f in SELECTION
            .fields
            .get(usize::from(field::APP_FPS)..=usize::from(field::APP_UPTIME_S))
            .unwrap()
        {
            let (_, play) = crate::PLAY.field(f.name).unwrap();
            assert_eq!(f.ty, play.ty, "{}", f.name);
        }
    }
}
