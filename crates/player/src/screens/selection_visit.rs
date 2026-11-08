//! One selection frame's runtime step (design.md "beatoraja Lua Skins",
//! the bounded runtime; docs/stage3.md S7-07).

use bms_model::TimeUs;
use skin_model::{BuildError, Selection, SelectionTimers, Skin};

use super::script::{ScriptSlot, ScriptStop};

/// Prepares `sel`'s frame at `now` for `skin`, its timers from `timers`
/// ([`Skin::prepare_selection_frame`]), then runs the runtime `slot`'s
/// screen has, if any, against that state, writing every callback's
/// output slot; without one every output is its constant. The build of
/// the same frame then draws this state without refilling it. Called
/// after the frame's view and settings are applied and before its build,
/// outside the build's no-allocation span.
///
/// Returns why the runtime stopped, once for `slot`'s screen: on the
/// frame it stops, or on the first frame of a screen resuming a stopped
/// one. The caller shows the notice on the strip.
///
/// # Errors
///
/// [`BuildError::Kind`] for a skin that is not a selection skin.
pub(crate) fn selection_visit(
    slot: &mut ScriptSlot,
    skin: &Skin,
    sel: &mut Selection,
    now: TimeUs,
    timers: &SelectionTimers,
) -> Result<Option<ScriptStop>, BuildError> {
    skin.prepare_selection_frame(sel, now, timers)?;
    match slot.running() {
        Some(rt) if rt.outputs() == skin.outputs() => {
            let (fields, timers) = sel.runtime_parts();
            rt.frame(fields, timers);
        }
        _ => sel.outputs_mut().fill(0),
    }
    Ok(slot.stop())
}
