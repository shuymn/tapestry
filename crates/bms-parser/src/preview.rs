//! What a song list's preview reads of a chart beyond what is played
//! ([`PreviewInputs`], design.md "Loading and Song Selection", Summaries):
//! the 1× times of its hidden notes, BGA and layer events and scroll
//! changes, which the loader neither plays nor draws, and a timeline that
//! keeps the tempo changes and STOPs before them.
//!
//! They are timed by the loader's own timing over the chart's objects and
//! the preview's together, a timeline of their own apart from the play
//! chart's, so the play chart's timeline, rows, length and identity are
//! what they would be without them. That timeline is held to the same
//! limits, the 60-minute duration among them: one past a limit leaves the
//! preview as the play chart's.

use bms_model::{PreviewInputs, PreviewTimeline, Speed, TimeUs};

use crate::parse::{ObjectKind, Source};
use crate::{Limits, timing};

/// The preview inputs of `source`; `None` when timing its preview objects
/// fails, which leaves the preview as the play chart's.
pub fn inputs(source: &Source, limits: &Limits) -> Option<PreviewInputs> {
    if source.shown.is_empty() && source.scrolls.is_empty() {
        return Some(PreviewInputs::default());
    }
    let first_shown = source.objects.len();
    let first_scroll = first_shown + source.shown.len();
    let mut objects = Vec::with_capacity(first_scroll + source.scrolls.len());
    objects.extend_from_slice(&source.objects);
    objects.extend_from_slice(&source.shown);
    objects.extend_from_slice(&source.scrolls);
    let both = Source {
        bpm: source.bpm.clone(),
        bpm_defs: source.bpm_defs.clone(),
        stop_defs: source.stop_defs.clone(),
        measure_lengths: source.measure_lengths.clone(),
        objects,
        ..Source::default()
    };
    // Its warnings repeat the play chart's.
    let timed = timing::compute(&both, limits, Speed::NORMAL, &mut Vec::new()).ok()?;
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
    inputs.timeline = Some(PreviewTimeline {
        points: timed.rows,
        long_stops: timed.long_stops,
        initial: timed.initial_tempo,
    });
    Some(inputs)
}
