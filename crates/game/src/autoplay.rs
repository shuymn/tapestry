//! What an autoplay session plays, made once before its first play
//! (design.md "Autoplay"): the generated input and the audio schedule with
//! the key sounds merged into the BGM.

use bms_model::{BgmEvent, Chart, LANE_COUNT, Note, NoteKind, SoundId, TimeUs};
use eval::{ControlId, PhysicalInputEvent};
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
    /// The chart's BGM, merged with the key sounds autoplay presses when
    /// they are scheduled ([`crate::AutoKeySounds`]), for the audio engine
    /// to take ([`::audio::AudioEngine::with_schedule`]).
    pub schedule: Vec<BgmEvent>,
    /// The controls the generated keyboard records.
    pub recorded: Vec<ControlId>,
}

impl Autoplay {
    /// The source queue an autoplay play's input side needs: room for the
    /// whole generated stream, so the feeder never waits for the logic
    /// step however dense a moment of the chart is.
    #[must_use]
    pub const fn queue_capacity(&self) -> usize {
        self.events.len()
    }
}

/// Why autoplay cannot play a chart.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AutoplayError {
    /// The plan is refused: the evaluator refuses the chart, or the play
    /// does not fit the recording reservation, autoplay's own buffers
    /// included.
    #[error("autoplay cannot play this chart")]
    Plan(#[from] LogicError),
    /// A note's time is out of the range autoplay can press it in.
    #[error("autoplay cannot play this chart")]
    Time(#[from] TimeOverflow),
}

/// The key sounds of a lane autoplay plays, of the notes `keep` keeps: one
/// per pressed note with a sound, at the note's time.
fn lane_sounds<'c>(
    notes: &'c [Note],
    keep: &'c impl Fn(&Note) -> bool,
) -> impl Iterator<Item = (TimeUs, SoundId)> + 'c {
    notes
        .iter()
        .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }) && keep(n))
        .filter_map(|n| n.sound.map(|s| (n.time, s)))
}

/// How many entries [`schedule`] makes: the chart's BGM events and one key
/// sound per pressed note with a sound. The generated keyboard binds every
/// lane.
#[must_use]
pub fn schedule_len(chart: &Chart) -> usize {
    schedule_len_where(chart, |_| true)
}

/// How many entries [`schedule_where`] makes.
pub(crate) fn schedule_len_where(chart: &Chart, keep: impl Fn(&Note) -> bool) -> usize {
    chart
        .lanes
        .iter()
        .map(|notes| lane_sounds(notes, &keep).count())
        .fold(chart.bgm.len(), usize::saturating_add)
}

/// The audio schedule of an autoplay session: `chart`'s BGM merged in time
/// order with the key sound of every note autoplay presses (taps and long
/// note heads, at the note's time, with the sound the evaluator plays for
/// an on-time press). At equal times BGM comes first, then the key sounds
/// in lane order. It is built in a vector of exactly [`schedule_len`]
/// entries, without sorting.
#[must_use]
pub fn schedule(chart: &Chart) -> Vec<BgmEvent> {
    schedule_where(chart, |_| true)
}

/// [`schedule`] with the key sounds of the notes `keep` keeps only, in a
/// vector of exactly [`schedule_len_where`] entries.
pub(crate) fn schedule_where(chart: &Chart, keep: impl Fn(&Note) -> bool) -> Vec<BgmEvent> {
    let mut out = Vec::with_capacity(schedule_len_where(chart, &keep));
    out.extend(merged(chart, &keep));
    out
}

/// Keeps every note.
static EVERY_NOTE: fn(&Note) -> bool = |_| true;

/// [`schedule`] as an iterator, in the same order, allocating nothing: the
/// 1× autoplay schedule the loudness analysis mixes (design.md
/// "Algorithms"), in memory independent of the chart's length. A change in
/// what it yields changes what the analysis measures, so it bumps
/// `audio::ANALYSIS_VERSION`.
pub fn schedule_iter(chart: &Chart) -> impl Iterator<Item = BgmEvent> + '_ {
    merged(chart, &EVERY_NOTE)
}

/// The merge of [`schedule_where`]: fixed state, one head per lane.
fn merged<'c>(
    chart: &'c Chart,
    keep: &'c impl Fn(&Note) -> bool,
) -> impl Iterator<Item = BgmEvent> + 'c {
    let mut lanes = chart.lanes.each_ref().map(|notes| lane_sounds(notes, keep));
    let mut heads: [Option<(TimeUs, SoundId)>; LANE_COUNT] = [None; LANE_COUNT];
    for (head, lane) in heads.iter_mut().zip(&mut lanes) {
        *head = lane.next();
    }
    let mut bgm = chart.bgm.iter().peekable();
    std::iter::from_fn(move || {
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
            (None, None) => None,
            (Some((_, key)), Some(b)) if b <= key => bgm.next().copied(),
            (None, Some(_)) => bgm.next().copied(),
            (Some((i, _)), _) => {
                let (head, lane) = (heads.get_mut(i)?, lanes.get_mut(i)?);
                let (time, sound) = head.take()?;
                *head = lane.next();
                Some(BgmEvent { time, sound })
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use bms_model::{
        BgmEvent, Chart, ChartBuilder, LANE_COUNT, Lane, Note, NoteKind, SoundId, TimeUs,
    };

    use super::{schedule, schedule_iter, schedule_where};

    /// A chart of pseudo-random taps, long notes and mines on every lane
    /// and BGM, on a coarse time grid so equal times are frequent, every
    /// note with a sound but every fifth.
    fn chart(seed: u64) -> Chart {
        let mut x = seed;
        let mut next = |n: u64| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) % n
        };
        let mut b = ChartBuilder::new(150_000);
        for k in 0..40 {
            b = b.sound(&format!("{k}.wav"));
        }
        for _ in 0..30 {
            b = b.bgm(TimeUs(next(20) as i64 * 50_000), SoundId(next(40) as u16));
        }
        for lane in 0..LANE_COUNT as u8 {
            let lane = Lane::new(lane).unwrap();
            let mut t = 0;
            for _ in 0..12 {
                t += next(3) as i64 * 50_000 + 50_000;
                b = match next(5) {
                    0 => b.mine(lane, TimeUs(t), 10),
                    1 => {
                        let end = t + 25_000;
                        let b = b.long(lane, TimeUs(t), TimeUs(end));
                        t = end;
                        b
                    }
                    _ => b.tap(lane, TimeUs(t)),
                };
            }
        }
        let mut chart = b.build().unwrap();
        let mut n = 0_u16;
        for note in chart.lanes.iter_mut().flatten() {
            n += 1;
            note.sound = (!n.is_multiple_of(5)).then_some(SoundId(n % 40));
        }
        chart
    }

    /// The order the schedule is specified in, made independently: every
    /// BGM event, and the key sound of every kept note but a mine, sorted
    /// by time, BGM before key sounds, key sounds by lane, each source in
    /// its own order (a stable sort).
    fn expected(chart: &Chart, keep: impl Fn(&Note) -> bool) -> Vec<BgmEvent> {
        let mut all: Vec<(TimeUs, usize, BgmEvent)> =
            chart.bgm.iter().map(|&e| (e.time, 0, e)).collect();
        for (lane, notes) in chart.lanes.iter().enumerate() {
            for n in notes {
                if matches!(n.kind, NoteKind::Mine { .. }) || !keep(n) {
                    continue;
                }
                if let Some(sound) = n.sound {
                    all.push((
                        n.time,
                        1 + lane,
                        BgmEvent {
                            time: n.time,
                            sound,
                        },
                    ));
                }
            }
        }
        all.sort_by_key(|&(t, source, _)| (t, source));
        all.into_iter().map(|(_, _, e)| e).collect()
    }

    /// The iterator, the whole schedule and a filtered (practice) schedule
    /// each yield the specified order.
    #[test]
    fn schedule_iter_matches_schedule() {
        for seed in 0..32 {
            let chart = chart(seed);
            let all = expected(&chart, |_| true);
            assert!(all.len() > 60, "{seed}: {}", all.len());
            assert_eq!(schedule_iter(&chart).collect::<Vec<_>>(), all, "{seed}");
            assert_eq!(schedule(&chart), all, "{seed}");
            // A practice round's schedule keeps the notes outside its
            // section.
            let outside = |n: &Note| n.time < TimeUs(300_000) || n.time >= TimeUs(700_000);
            assert_eq!(
                schedule_where(&chart, outside),
                expected(&chart, outside),
                "{seed}"
            );
        }
    }
}
