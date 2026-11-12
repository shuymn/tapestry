//! The decision screen's schema: no field, since every value a decision
//! skin reads folds to a constant when it is imported, and the built-in
//! timers of the screen's lifetime (docs/stage3.md §3, Decision).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use crate::schema::{Export, Schema, TimerDef};

const fn timer(name: &'static str) -> TimerDef {
    TimerDef {
        name,
        export: Export::TapestryOnly,
    }
}

/// The decision screen's state: its timers alone.
pub static DECISION: Schema = Schema {
    fields: &[],
    timers: &[timer("screen"), timer("fadeout"), timer("input")],
    enums: &[],
};

/// The built-in timer slots of [`DECISION`].
pub mod timer {
    /// The choice: the screen's start.
    pub const SCREEN: u16 = 0;
    /// The fade-out's start (the source's timer 2); off until it starts.
    pub const FADEOUT: u16 = 1;
    /// The end of the skin's input declaration (the source's timer 1):
    /// on only after the choice plus `input`. It drives the skin's drawing
    /// and changes nothing else.
    pub const INPUT: u16 = 2;
    /// The number of built-in timers.
    pub const BUILTIN: u16 = 3;
}

#[cfg(test)]
mod tests {
    use super::{DECISION, timer};

    #[test]
    fn the_timers_are_at_their_slots() {
        DECISION.check().unwrap();
        let names: Vec<&str> = DECISION.timers.iter().map(|t| t.name).collect();
        assert_eq!(names, ["screen", "fadeout", "input"]);
        assert_eq!(usize::from(timer::BUILTIN), names.len());
        assert!(DECISION.fields.is_empty());
    }
}
