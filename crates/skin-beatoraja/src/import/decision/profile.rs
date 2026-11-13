//! The decision profile (docs/stage3.md §3, Decision): one explicit
//! `match` per id space over every id a decision skin may read, each
//! classified. The decision schema ([`skin_model::DECISION`]) has no
//! field, so a decision skin shows the chosen chart's labels (**N**) and
//! truthful constants for the features tapestry lacks (**D**); everything
//! else is unsupported with a note: numbers, image indices, sliders,
//! graphs and events have no value on this screen.
//!
//! A JSON field's `ref` or `op` and a Lua callback's read go through these
//! functions. A Lua read while the skin loads ([`crate::lua::facts`])
//! answers from them too, except where `facts` answers every scene alike:
//! options 32 and 33 (false off play), 290 and 1008 (false everywhere)
//! and the judgment class (a fact of the snapshot), whose values agree
//! with these, and texts, which read the difficulty table's lines as empty
//! and anything else as live state.

use skin_model::decision_timer;

use super::super::result::profile::ResOption;
use super::super::selection::profile::Class;
use super::super::timer::Timer;

/// The decision text `id`: the chart's labels, as on play
/// ([`super::super::property::str_by_id`] reads them, 12 and 16 the full
/// strings); the difficulty table's line omitted, the source's empty
/// string without difficulty tables
/// (`skin/property/StringPropertyFactory.java:261`).
pub(crate) fn text(id: i32) -> Option<Class<()>> {
    Some(match id {
        10..=16 => Class::Bridge(()),
        1003 => Class::Omit,
        _ => return None,
    })
}

/// The decision option `id` (positive): a truthful constant, or the
/// judgment class (180 to 184), an import fact a Lua skin reads while it
/// loads; the decision schema has no field for it, so a frame condition
/// on it is unsupported. Any other built-in is unsupported, never a
/// constant: the stage file, banner and back image (190 to 195) among
/// them, until S10-12.
pub(crate) fn option(id: i32) -> Option<Class<ResOption>> {
    use ResOption::{Constant, JudgeClass};
    Some(Class::Bridge(match id {
        // Autoplay on and off: both false off the play screen, as the
        // source answers them (`BooleanPropertyFactory.java:413-416`).
        32 | 33 => Constant(false),
        // Offline: no IR.
        50 => Constant(true),
        51 => Constant(false),
        // Every chart is 7-key.
        160 => Constant(true),
        161..=164 => Constant(false),
        180..=184 => JudgeClass,
        // No courses, no difficulty tables.
        280..=283 | 289 | 290 | 1008 => Constant(false),
        _ => return None,
    }))
}

/// The decision timer `id` (built in): the input declaration's end (1)
/// and the fade-out (2); 0 never set.
pub(crate) fn timer(id: i32) -> Timer {
    match id {
        1 => Timer::Builtin(decision_timer::INPUT),
        2 => Timer::Builtin(decision_timer::FADEOUT),
        0 => Timer::Unset,
        _ => Timer::Unsupported,
    }
}

#[cfg(test)]
mod tests;
