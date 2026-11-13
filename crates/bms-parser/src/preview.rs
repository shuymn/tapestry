//! What a song list's preview reads of a chart beyond what is played
//! ([`PreviewInputs`], design.md "Loading and Song Selection", Summaries):
//! the 1× times of its hidden notes, BGA and layer events and scroll
//! changes, which the loader neither plays nor draws.
//!
//! They are timed by the loader's own timing over the chart's measures,
//! tempo changes and STOPs alone, apart from the play chart's timeline, so
//! the timeline, its rows, its length and its identity are what they would
//! be without them.

use bms_model::{PreviewInputs, Speed, TimeUs};

use crate::parse::{ObjectKind, RawObject, Source};
use crate::{Limits, timing};

/// The preview inputs of `source`; `None` when timing its preview objects
/// fails (past the duration cap, or a limit the play chart alone keeps
/// within), which leaves the preview as the play chart's.
pub fn inputs(source: &Source, limits: &Limits) -> Option<PreviewInputs> {
    if source.shown.is_empty() && source.scrolls.is_empty() {
        return Some(PreviewInputs::default());
    }
    let timing = |o: &&RawObject| {
        matches!(
            o.kind,
            ObjectKind::BpmHex(_) | ObjectKind::BpmRef(_) | ObjectKind::StopRef(_)
        )
    };
    let tempo: Vec<RawObject> = source.objects.iter().filter(timing).copied().collect();
    let first_shown = tempo.len();
    let first_scroll = first_shown + source.shown.len();
    let mut objects = tempo;
    objects.extend_from_slice(&source.shown);
    objects.extend_from_slice(&source.scrolls);
    let only = Source {
        bpm: source.bpm.clone(),
        bpm_defs: source.bpm_defs.clone(),
        stop_defs: source.stop_defs.clone(),
        measure_lengths: source.measure_lengths.clone(),
        objects,
        ..Source::default()
    };
    // Its warnings repeat the play chart's.
    let timed = timing::compute(&only, limits, Speed::NORMAL, &mut Vec::new()).ok()?;
    let mut inputs = PreviewInputs::default();
    for o in &timed.objects {
        if o.seq >= first_scroll {
            let ObjectKind::Bgm(id) = o.kind else {
                continue;
            };
            let Some(&factor) = source.scroll_defs.get(&id) else {
                continue;
            };
            // In position order, file order within one: a later change at
            // one time replaces an earlier one.
            match inputs.scroll.last_mut() {
                Some(last) if last.0 == o.time => last.1 = factor,
                _ => inputs.scroll.push((o.time, factor)),
            }
        } else if o.seq >= first_shown {
            inputs.last_shown = Some(inputs.last_shown.map_or(o.time, |t: TimeUs| t.max(o.time)));
        }
    }
    Some(inputs)
}
