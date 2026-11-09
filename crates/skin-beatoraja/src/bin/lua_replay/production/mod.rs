//! The production run: the production import's skin built frame by frame
//! with `Skin::build`, the program's trace recorded (design.md "beatoraja
//! Lua Skins", the reference executor; s7-06e-design.md §3.1, §3.5).
//!
//! [`ProductionRun`] owns the skin, the play and the trace; nothing
//! outside this module and its child [`compare`] can read them. The
//! reference gets only the frames' inputs ([`FrameInputs`]).

pub mod compare;

use std::collections::BTreeMap;

use bms_model::TimeUs;
use skin_beatoraja::lua::replay::Recorded;
use skin_model::present::trace::{self, Event};
use skin_model::{
    AtlasLimits, Attempt, ChartMetadata, CoverSettings, FrameReadouts, Gain, GaugeDesc, LaneView,
    Layout, PlayContext, QuadBatch, Side, Skin, SkinFrame, Variant, field,
};

use crate::inputs::FrameInputs;
use crate::script::{STATE_AT_ZERO, Script};

/// One frame's trace. Its fields, and [`ProductionRun`]'s, are
/// `pub(in crate::production)`: readable here and in [`compare`] only, as
/// `cargo xtask check-arch`'s privacy harness checks.
pub(in crate::production) struct FrameTrace {
    pub(in crate::production) events: Vec<Event>,
    pub(in crate::production) now: TimeUs,
    pub(in crate::production) state_us: i64,
}

/// What production did, owned and unreadable outside this module.
pub struct ProductionRun {
    pub(in crate::production) recorded: Recorded,
    pub(in crate::production) frames: Vec<FrameTrace>,
    /// Each timer callback's timer slot.
    pub(in crate::production) timer_slots: BTreeMap<u16, usize>,
    /// The program's callbacks.
    pub(in crate::production) callbacks: usize,
}

const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 8_192,
    max_layers: 16,
    reserved_layers: 1,
};

/// The readouts of a frame: every cover on at 0, nothing else known.
fn readouts() -> FrameReadouts {
    FrameReadouts {
        covers: CoverSettings {
            cover: 0,
            lift: 0,
            hidden: 0,
            cover_enabled: true,
            lift_enabled: true,
            hidden_enabled: true,
            adjusting: false,
        },
        ..FrameReadouts::UNAVAILABLE
    }
}

/// Plays `script` over `skin`, as production imported it, recording
/// each frame: the run and the frames' inputs.
///
/// # Errors
///
/// The atlas, the preparation, a frame's build or the evaluator failing.
pub fn record(
    skin: &Skin,
    recorded: Recorded,
    script: &Script,
    play_level: Option<i32>,
) -> Result<(ProductionRun, Vec<FrameInputs>), String> {
    let chart = script.chart();
    let preset = rules::tapestry();
    let gauges: Vec<GaugeDesc> = preset.gauges.iter().map(GaugeDesc::from_rule).collect();
    let ctx = PlayContext {
        meta: ChartMetadata::UNKNOWN,
        autoplay: false,
        gauges: &gauges,
        attempt: Attempt::Full { lead_in: TimeUs(0) },
        empty_poor_breaks_combo: false,
        empty_poor_counted: false,
        scratch: Side::Left,
        ln_default: rules::LnMode::Ln,
        last_note_end: script.last_note(),
        bpm_range: None,
        bpm_whole_change: None,
        play_level,
    };
    let uvs = skin
        .atlas(Variant::Lo, LIMITS, &|| false)
        .map_err(|e| format!("atlas: {e:?}"))?
        .into_parts()
        .1;
    let mut play = skin
        .prepare(&chart, &ctx)
        .map_err(|e| format!("prepare: {e:?}"))?;
    let mut batch = QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
    let view = LaneView {
        gain: Gain::new(8, 3),
        ..LaneView::NONE
    };
    let end = script.last_note();
    let mut frames = Vec::new();
    let mut inputs = Vec::new();
    trace::arm();
    let played = script.play(&chart, &preset, |now, effects, ev| {
        let state_us = now.0 + STATE_AT_ZERO;
        let frame = SkinFrame {
            state_us,
            now,
            display_offset: TimeUs(0),
            tally: ev.tally(),
            gauges: ev.gauges(),
            gauge: 0,
            effects,
            notes: ev.notes(),
            input: ev.input(),
            complete: ev.is_complete(),
            target: [1_920, 1_080],
            layout: &Layout::AUTHOR,
            turntable: 0,
            lanes: view,
            playing: now <= end,
            closing: None,
            readouts: readouts(),
        };
        batch.clear();
        skin.build(&mut play, &frame, &uvs, &mut batch)
            .map_err(|e| format!("build at {}: {e:?}", now.0))?;
        let st = play.state();
        let fields = st.fields.get(..field::COUNT).unwrap_or(st.fields);
        inputs.push(FrameInputs::new(fields, st.timers, state_us));
        frames.push(FrameTrace {
            events: trace::take(),
            now,
            state_us,
        });
        Ok(())
    });
    let slots = play.state().timers.len();
    trace::disarm();
    played?;
    let mut timers: Vec<u16> = last_records(&recorded)
        .values()
        .filter(|o| o.field == "timer")
        .filter_map(|o| o.program)
        .collect();
    timers.sort_unstable();
    timers.dedup();
    let base = slots.saturating_sub(timers.len());
    let timer_slots = timers
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, base + i))
        .collect();
    Ok((
        ProductionRun {
            recorded,
            frames,
            timer_slots,
            callbacks: skin.program_callbacks(),
        },
        inputs,
    ))
}

/// Each node's last record: a later record of a node supersedes an
/// earlier one.
fn last_records(r: &Recorded) -> BTreeMap<u32, &skin_beatoraja::lua::replay::Occurrence> {
    let mut out = BTreeMap::new();
    for o in &r.occurrences {
        out.insert(o.node, o);
    }
    out
}

impl ProductionRun {
    /// The occurrence records production made: what [`crate`] checks the
    /// reference import against, and only that.
    pub fn same_import(&self, other: &Recorded) -> bool {
        self.recorded.occurrences == other.occurrences
    }
}
