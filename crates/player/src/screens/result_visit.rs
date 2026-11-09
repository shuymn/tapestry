//! One result frame's runtime step (design.md "beatoraja Lua Skins", the
//! bounded runtime; docs/stage3.md S7-07; s7-07-design §3.7.1).

use bms_model::TimeUs;
use skin_model::{BuildError, ResultScreen, ResultTimers, Skin};

use super::script::{ScriptNotes, ScriptSlot, ScriptStop};

/// Prepares `res`'s frame at `now` for `skin`, its timers from `timers`
/// ([`Skin::prepare_result_frame`]), then runs the runtime of the
/// presented view's `slot`, if any, against that state, writing every
/// callback's output slot, its notes going to the lifetime's `notes`;
/// without one every output is its constant. The build of the same frame
/// then draws this state without refilling it. Called after the frame's
/// commit point and before its build, outside both counted spans.
///
/// Returns why the runtime stopped, once for `slot`: on the frame it
/// stops, or on the first frame of a slot taking a stopped one. The
/// caller shows the notice on the strip.
///
/// # Errors
///
/// [`BuildError::Kind`] for a skin that is not a result skin.
pub(crate) fn result_visit(
    slot: &mut ScriptSlot,
    notes: &ScriptNotes,
    skin: &Skin,
    res: &mut ResultScreen,
    now: TimeUs,
    timers: &ResultTimers,
) -> Result<Option<ScriptStop>, BuildError> {
    skin.prepare_result_frame(res, now, timers)?;
    // The switches' `off` (dev-tools): no runtime taken.
    #[cfg(feature = "dev-tools")]
    if super::probe::off() {
        res.outputs_mut().fill(0);
        return Ok(None);
    }
    // The frame probe's generation: the slot's (dev-tools).
    #[cfg(feature = "dev-tools")]
    let generation = slot.generation().unwrap_or(0);
    match slot.result_running(notes) {
        Some(rt) if rt.outputs() == skin.outputs() => {
            let (fields, timers) = res.runtime_parts();
            #[cfg(feature = "dev-tools")]
            super::probe::run(rt, skin_model::ScreenKind::Result, generation, |rt| {
                rt.frame(fields, timers);
            });
            #[cfg(not(feature = "dev-tools"))]
            rt.frame(fields, timers);
        }
        _ => res.outputs_mut().fill(0),
    }
    Ok(slot.stop())
}
