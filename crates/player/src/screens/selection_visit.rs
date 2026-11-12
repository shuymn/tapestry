//! One selection frame's runtime step (design.md "beatoraja Lua Skins",
//! the bounded runtime; docs/stage3.md S7-07).

use bms_model::TimeUs;
use skin_model::{AppReadouts, BuildError, Selection, SelectionTimers, Skin};

use super::script::{ScriptSlot, ScriptStop};

/// Prepares `sel`'s frame at `now` for `skin`, its application readouts
/// from `app` and its timers from `timers`
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
/// [`BuildError::Kind`] for a skin that is not a selection skin;
/// [`BuildError::InvalidReadouts`] for a readout of `app` outside its
/// field's range, before the runtime runs.
pub(crate) fn selection_visit(
    slot: &mut ScriptSlot,
    skin: &Skin,
    sel: &mut Selection,
    now: TimeUs,
    timers: &SelectionTimers,
    app: &AppReadouts,
) -> Result<Option<ScriptStop>, BuildError> {
    skin.prepare_selection_frame(sel, now, timers, app)?;
    // The switches' `off` (dev-tools): no runtime taken.
    #[cfg(feature = "dev-tools")]
    if super::probe::off() {
        sel.outputs_mut().fill(0);
        return Ok(None);
    }
    // The frame probe's generation, read once the runtime is taken
    // (dev-tools).
    #[cfg(feature = "dev-tools")]
    let generation = {
        let _ = slot.running();
        slot.generation().unwrap_or(0)
    };
    match slot.running() {
        Some(rt) if rt.outputs() == skin.outputs() => {
            let (fields, timers) = sel.runtime_parts();
            #[cfg(feature = "dev-tools")]
            super::probe::run(rt, skin_model::ScreenKind::Selection, generation, |rt| {
                rt.frame(fields, timers);
            });
            #[cfg(not(feature = "dev-tools"))]
            rt.frame(fields, timers);
        }
        _ => sel.outputs_mut().fill(0),
    }
    Ok(slot.stop())
}
