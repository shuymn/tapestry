//! What an autoplay session plays, made once before its first play
//! (design.md "Autoplay"): the generated input and the audio schedule with
//! the key sounds merged into the BGM.

use bms_model::{BgmEvent, Chart, LANE_COUNT, Lane, Note, NoteKind, SoundId, TimeUs};
use eval::{ControlId, PhysicalInputEvent, SourceProfile};
use input::TimeOverflow;

use crate::logic::LogicError;

/// An autoplay session's input and sounds, from [`Loaded::autoplay`].
///
/// [`Loaded::autoplay`]: crate::Loaded::autoplay
#[derive(Debug)]
pub struct Autoplay {
    /// The generated input, in receive-time order, for the input side's
    /// feeder ([`input::AutoplayInput`]).
    pub events: Vec<PhysicalInputEvent>,
    /// The chart's BGM merged with the key sounds autoplay presses, for
    /// the audio engine to take ([`::audio::AudioEngine::with_schedule`]).
    pub schedule: Vec<BgmEvent>,
    /// The controls the generated keyboard records.
    pub recorded: Vec<ControlId>,
}

/// Why autoplay cannot play a chart.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AutoplayError {
    /// The play does not fit the recording reservation, autoplay's own
    /// buffers included.
    #[error("autoplay cannot play this chart")]
    Plan(#[source] LogicError),
    /// A note's time is out of the range autoplay can press it in.
    #[error("autoplay cannot play this chart")]
    Time(#[source] TimeOverflow),
}

/// The key sounds of `lane` autoplay plays: one per pressed note with a
/// sound, at the note's time.
fn lane_sounds(notes: &[Note]) -> impl Iterator<Item = (TimeUs, SoundId)> + '_ {
    notes
        .iter()
        .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }))
        .filter_map(|n| n.sound.map(|s| (n.time, s)))
}

/// The lanes `profile` binds, as slices of `chart`'s notes; empty for an
/// unbound lane.
fn bound_lanes<'c>(chart: &'c Chart, profile: &SourceProfile) -> [&'c [Note]; LANE_COUNT] {
    let mut lanes: [&[Note]; LANE_COUNT] = [&[]; LANE_COUNT];
    for (lane, slot) in Lane::all().zip(&mut lanes) {
        if profile.bindings.iter().any(|b| b.lane == lane) {
            *slot = chart.lanes.get(lane.index()).map_or(&[], Vec::as_slice);
        }
    }
    lanes
}

/// How many entries [`schedule`] makes: the chart's BGM events and one key
/// sound per pressed note with a sound on a lane `profile` binds.
#[must_use]
pub fn schedule_len(chart: &Chart, profile: &SourceProfile) -> usize {
    bound_lanes(chart, profile)
        .iter()
        .map(|notes| lane_sounds(notes).count())
        .fold(chart.bgm.len(), usize::saturating_add)
}

/// The audio schedule of an autoplay session: `chart`'s BGM merged in time
/// order with the key sound of every note autoplay presses with
/// `profile`'s bindings (taps and long note heads, at the note's time, with
/// the sound the evaluator plays for an on-time press). At equal times BGM
/// comes first, then the key sounds in lane order. It is built in a vector
/// of exactly [`schedule_len`] entries, without sorting.
#[must_use]
pub fn schedule(chart: &Chart, profile: &SourceProfile) -> Vec<BgmEvent> {
    let mut out = Vec::with_capacity(schedule_len(chart, profile));
    let mut lanes = bound_lanes(chart, profile).map(lane_sounds);
    let mut heads: [Option<(TimeUs, SoundId)>; LANE_COUNT] = [None; LANE_COUNT];
    for (head, lane) in heads.iter_mut().zip(&mut lanes) {
        *head = lane.next();
    }
    let mut bgm = chart.bgm.iter().peekable();
    loop {
        // The earliest key sound; the first lane wins a tie.
        let mut earliest: Option<(usize, TimeUs)> = None;
        for (i, head) in heads.iter().enumerate() {
            if let Some((at, _)) = *head
                && earliest.is_none_or(|(_, e)| at < e)
            {
                earliest = Some((i, at));
            }
        }
        // BGM first at equal times.
        let next_bgm = bgm.peek().map(|b| b.time);
        match (earliest, next_bgm) {
            (None, None) => return out,
            (Some((_, key)), Some(b)) if b <= key => out.extend(bgm.next().copied()),
            (None, Some(_)) => out.extend(bgm.next().copied()),
            (Some((i, _)), _) => {
                let (Some(head), Some(lane)) = (heads.get_mut(i), lanes.get_mut(i)) else {
                    return out;
                };
                if let Some((time, sound)) = head.take() {
                    out.push(BgmEvent { time, sound });
                }
                *head = lane.next();
            }
        }
    }
}

impl From<LogicError> for AutoplayError {
    fn from(e: LogicError) -> Self {
        Self::Plan(e)
    }
}

impl From<TimeOverflow> for AutoplayError {
    fn from(e: TimeOverflow) -> Self {
        Self::Time(e)
    }
}
