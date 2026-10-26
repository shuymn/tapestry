//! Autoplay: the inputs that hit every note of a chart, generated once, and
//! the feeder that plays them into a [`SourceSet`] on the input thread
//! (design.md "Autoplay").

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use bms_model::{Chart, LANE_COUNT, Lane, Note, NoteKind, TimeUs};
use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId, SourceProfile};

use crate::InputAdapter;
use crate::gate::{DeviceId, Packet};
use crate::sources::{DeviceKind, SourceSet};

/// The generated keyboard's device id. Raw Input names devices by handle,
/// which is never this value.
pub const AUTOPLAY_DEVICE: DeviceId = DeviceId(u64::MAX);

/// How long autoplay holds a tap, unless the lane's next note comes first.
const TAP_HOLD: TimeUs = TimeUs(50_000);

/// A note time plus the hold or the input offset left the `i64` range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a note's time is too large for its key to be pressed and released")]
pub struct TimeOverflow;

/// The control `profile` binds to `lane`, if any.
fn bound(profile: &SourceProfile, lane: Lane) -> Option<ControlId> {
    profile
        .bindings
        .iter()
        .find(|b| b.lane == lane)
        .map(|b| b.control)
}

/// The notes of `lane` autoplay presses: its taps and long notes.
fn pressed(chart: &Chart, lane: Lane) -> impl Iterator<Item = &Note> {
    chart
        .lanes
        .get(lane.index())
        .into_iter()
        .flatten()
        .filter(|n| !matches!(n.kind, NoteKind::Mine { .. }))
}

/// How many events [`autoplay`] makes for `chart` with `profile`: a press
/// and a release for every tap and long note on a bound lane, soundless
/// ones included. Counting allocates nothing, so a play can be refused
/// before the events are made.
#[must_use]
pub fn autoplay_count(chart: &Chart, profile: &SourceProfile) -> usize {
    Lane::all()
        .filter(|&lane| bound(profile, lane).is_some())
        .map(|lane| pressed(chart, lane).count().saturating_mul(2))
        .fold(0, usize::saturating_add)
}

/// One lane's events in time order, made as they are taken.
struct LaneEvents<'c> {
    control: ControlId,
    notes: &'c [Note],
    /// The next note to press.
    next: usize,
    /// The release of the note last pressed, still to come.
    release: Option<TimeUs>,
}

impl LaneEvents<'_> {
    /// The lane's next event, as (time before the input offset, value).
    fn take(&mut self) -> Result<Option<(TimeUs, i32)>, TimeOverflow> {
        if let Some(at) = self.release.take() {
            return Ok(Some((at, 0)));
        }
        while let Some(note) = self.notes.get(self.next) {
            let i = self.next;
            self.next += 1;
            let release = match note.kind {
                NoteKind::Mine { .. } => continue,
                NoteKind::Long { end, .. } => end,
                NoteKind::Tap => {
                    // The minimum is taken in i128, so a next note before
                    // an unrepresentable hold end still gives a release.
                    let held = i128::from(note.time.0) + i128::from(TAP_HOLD.0);
                    let release = self
                        .notes
                        .get(i + 1)
                        .map_or(held, |next| held.min(i128::from(next.time.0)));
                    TimeUs(i64::try_from(release).map_err(|_| TimeOverflow)?)
                }
            };
            self.release = Some(release);
            return Ok(Some((note.time, 1)));
        }
        Ok(None)
    }
}

/// Inputs that hit every tap and long note of `chart` on time, as source 0
/// with `profile`'s bindings and input offset: each note's control is
/// pressed at the note and released after 50 ms or at the lane's next
/// note, whichever is first, or at a long note's end. Mines are left
/// alone, and lanes without a binding are skipped.
///
/// The events are in receive-time order, and at equal times in lane order
/// (the scratch first), each lane's in the order made: for a chart that
/// passes [`Chart::validate`], each lane's events are already in time
/// order, so the lanes are merged into a vector of exactly
/// [`autoplay_count`] events without sorting.
///
/// # Errors
///
/// [`TimeOverflow`] if a receive time (note time plus the hold and the
/// input offset) is outside the `i64` range.
pub fn autoplay(
    chart: &Chart,
    profile: &SourceProfile,
) -> Result<Vec<PhysicalInputEvent>, TimeOverflow> {
    let mut events = Vec::with_capacity(autoplay_count(chart, profile));
    let mut lanes: [Option<LaneEvents<'_>>; LANE_COUNT] = std::array::from_fn(|_| None);
    let mut heads: [Option<(TimeUs, i32)>; LANE_COUNT] = [None; LANE_COUNT];
    for ((lane, slot), head) in Lane::all().zip(&mut lanes).zip(&mut heads) {
        let Some(control) = bound(profile, lane) else {
            continue;
        };
        let mut l = LaneEvents {
            control,
            notes: chart.lanes.get(lane.index()).map_or(&[], Vec::as_slice),
            next: 0,
            release: None,
        };
        *head = l.take()?;
        *slot = Some(l);
    }
    loop {
        // The earliest head; the first lane wins a tie.
        let mut earliest: Option<(usize, TimeUs)> = None;
        for (i, head) in heads.iter().enumerate() {
            if let Some((at, _)) = *head
                && earliest.is_none_or(|(_, e)| at < e)
            {
                earliest = Some((i, at));
            }
        }
        let Some((i, _)) = earliest else {
            return Ok(events);
        };
        let (Some(head), Some(Some(lane))) = (heads.get_mut(i), lanes.get_mut(i)) else {
            return Ok(events);
        };
        let Some((at, value)) = head.take() else {
            return Ok(events);
        };
        events.push(PhysicalInputEvent {
            received: at.checked_add(profile.input_offset).ok_or(TimeOverflow)?,
            source: SourceId(0),
            kind: InputKind::Control {
                control: lane.control,
                value,
            },
        });
        *head = lane.take()?;
    }
}

/// Plays generated events into a [`SourceSet`] from the generated keyboard
/// [`AUTOPLAY_DEVICE`], on the play's timeline: every poll pushes each
/// event due by then, a batch per receive time, and then publishes, so an
/// event is always queued before its source publishes past it. Once the
/// focus gate has tripped, nothing more is fed. After [`new`](Self::new),
/// nothing allocates.
#[derive(Debug)]
pub struct AutoplayInput {
    events: Vec<PhysicalInputEvent>,
    next: usize,
}

impl AutoplayInput {
    /// A feeder of `events`, in receive-time order (as [`autoplay`] makes
    /// them), from its first event.
    #[must_use]
    pub const fn new(events: Vec<PhysicalInputEvent>) -> Self {
        Self { events, next: 0 }
    }

    /// Starts over for a new play, before recording opens: the next poll
    /// feeds the first event, and the generated keyboard is disconnected
    /// and connected again, which releases every control an earlier play
    /// left held, recording `recorded`. `recorded` is moved in; nothing is
    /// allocated.
    pub fn restart(&mut self, sources: &mut SourceSet, recorded: Vec<ControlId>) {
        self.next = 0;
        sources.disconnect(AUTOPLAY_DEVICE);
        sources.connect(AUTOPLAY_DEVICE, DeviceKind::Keyboard);
        sources.set_recorded(AUTOPLAY_DEVICE, recorded);
    }

    /// The events fed since the last start.
    #[must_use]
    pub const fn fed(&self) -> usize {
        self.next
    }

    /// Whether every event has been fed.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.next >= self.events.len()
    }
}

impl InputAdapter for AutoplayInput {
    fn poll(&mut self, now: TimeUs, sources: &mut SourceSet) {
        while let Some(&PhysicalInputEvent { received: at, .. }) = self.events.get(self.next)
            && at <= now
        {
            if !sources.begin_batch(at) {
                // The focus gate has tripped: the play is over, and nothing
                // after it is received.
                break;
            }
            while let Some(ev) = self.events.get(self.next)
                && ev.received == at
            {
                self.next += 1;
                if let InputKind::Control { control, value } = ev.kind {
                    sources.packet(&Packet {
                        device: AUTOPLAY_DEVICE,
                        control,
                        value,
                        background: false,
                    });
                }
            }
        }
        sources.end_batch(now);
    }
}

#[cfg(test)]
mod tests {
    use bms_model::{Chart, ChartBuilder, Lane, NoteKind, TimeUs};
    use eval::{Binding, ControlId, InputKind, PhysicalInputEvent, SourceId, SourceProfile};

    use super::{AUTOPLAY_DEVICE, AutoplayInput, TimeOverflow, autoplay, autoplay_count};
    use crate::{DeviceId, DeviceKind, InputAdapter, SourceConfig, SourceReader, SourceSet};

    const MS: i64 = 1_000;

    fn lane(n: u8) -> Lane {
        Lane::new(n).unwrap()
    }

    fn control(l: Lane) -> ControlId {
        ControlId(0x100 + u32::try_from(l.index()).unwrap())
    }

    fn profile(offset: i64) -> SourceProfile {
        SourceProfile {
            input_offset: TimeUs(offset),
            bindings: Lane::all()
                .map(|l| Binding {
                    control: control(l),
                    lane: l,
                })
                .collect(),
            axes: Vec::new(),
        }
    }

    /// Chords on three lanes at 1.0 s and 1.03 s (taps shortened to the
    /// next note), a long note on the scratch from 1.0 s to 1.2 s with a
    /// tap on the scratch at 1.3 s, a mine, and a lone tap at 2 s.
    fn chart() -> Chart {
        let mut b = ChartBuilder::new(150_000);
        for t in [1_000, 1_030] {
            for l in [1, 2, 5] {
                b = b.tap(lane(l), TimeUs(t * MS));
            }
        }
        b.long(lane(0), TimeUs(1_000 * MS), TimeUs(1_200 * MS))
            .tap(lane(0), TimeUs(1_300 * MS))
            .mine(lane(3), TimeUs(1_500 * MS), 5)
            .tap(lane(7), TimeUs(2_000 * MS))
            .build()
            .unwrap()
    }

    fn set_config() -> SourceConfig {
        SourceConfig {
            max_devices: 4,
            max_sources: 4,
            max_held: 8,
            queue_capacity: 64,
            max_arrivals: 2,
        }
    }

    fn set() -> SourceSet {
        SourceSet::new(set_config()).unwrap()
    }

    /// The generated keyboard connected, recording every lane's control,
    /// and recording open on the play's timeline at -1 s.
    fn opened(feeder: &mut AutoplayInput, set: &mut SourceSet) -> SourceReader {
        feeder.restart(set, Lane::all().map(control).collect());
        set.open_recording(TimeUs(-1_000 * MS));
        set.take_readers().pop().unwrap()
    }

    /// The stable sort of every lane's events, as autoplay was first
    /// written: the reference the merge must equal.
    fn sorted_reference(chart: &Chart, p: &SourceProfile) -> Vec<PhysicalInputEvent> {
        let mut events = Vec::new();
        for l in Lane::all() {
            let notes = &chart.lanes[l.index()];
            for (i, n) in notes.iter().enumerate() {
                let release = match n.kind {
                    NoteKind::Mine { .. } => continue,
                    NoteKind::Long { end, .. } => end,
                    NoteKind::Tap => notes.get(i + 1).map_or(TimeUs(n.time.0 + 50 * MS), |next| {
                        TimeUs((n.time.0 + 50 * MS).min(next.time.0))
                    }),
                };
                for (at, value) in [(n.time, 1), (release, 0)] {
                    events.push(PhysicalInputEvent {
                        received: TimeUs(at.0 + p.input_offset.0),
                        source: SourceId(0),
                        kind: InputKind::Control {
                            control: control(l),
                            value,
                        },
                    });
                }
            }
        }
        events.sort_by_key(|e| e.received);
        events
    }

    /// The merge equals the stable sort of each lane's events, fills
    /// exactly the counted capacity, and skips mines and unbound lanes.
    #[test]
    fn autoplay_merges_the_lanes_in_time_and_lane_order() {
        let chart = chart();
        let p = profile(2 * MS);
        let events = autoplay(&chart, &p).unwrap();
        assert_eq!(events, sorted_reference(&chart, &p));
        assert_eq!(autoplay_count(&chart, &p), 2 * 9);
        assert_eq!(events.capacity(), events.len());
        let mut unbound = p.clone();
        unbound.bindings.retain(|b| b.lane != lane(1));
        let events = autoplay(&chart, &unbound).unwrap();
        assert_eq!(autoplay_count(&chart, &unbound), 2 * 7);
        assert_eq!(events.len(), 2 * 7);
        assert_eq!(events.capacity(), events.len());
    }

    /// I4: a receive time outside the `i64` range is refused when the
    /// events are made, so feeding never meets one.
    #[test]
    fn autoplay_refuses_times_out_of_range() {
        let chart = ChartBuilder::new(150_000)
            .tap(lane(1), TimeUs(1_000 * MS))
            .build()
            .unwrap();
        assert_eq!(autoplay(&chart, &profile(i64::MAX)), Err(TimeOverflow));
        let mut late = chart.clone();
        late.lanes[1][0].time = TimeUs(i64::MAX - 10);
        assert_eq!(autoplay(&late, &profile(0)), Err(TimeOverflow));
        assert_eq!(autoplay(&chart, &profile(-5 * MS)).unwrap().len(), 2);
    }

    /// A small deterministic generator for poll schedules.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    /// I1: under poll schedules with 1 ms wakes, random delays and 50 ms
    /// stalls that leave several simultaneous events behind the poll, a
    /// consumer reading after each poll finds every event at or below the
    /// published watermark already poppable, and nothing below a watermark
    /// it saw arrives later. Every event arrives once, in order.
    #[test]
    fn autoplay_input_pushes_each_event_before_publishing_past_it() {
        let chart = chart();
        let events = autoplay(&chart, &profile(0)).unwrap();
        for seed in 0..64 {
            let mut rng = Lcg(seed);
            let mut feeder = AutoplayInput::new(events.clone());
            let mut set = set();
            let mut reader = opened(&mut feeder, &mut set);
            let mut seen = Vec::new();
            let mut watermark = TimeUs(i64::MIN);
            let mut now = TimeUs(-1_000 * MS);
            while now.0 < 2_200 * MS {
                now.0 += match rng.below(10) {
                    0 => 50 * MS,
                    1..=3 => 1 + i64::try_from(rng.below(3 * 1_000)).unwrap(),
                    _ => MS,
                };
                feeder.poll(now, &mut set);
                let published = reader.published().received_up_to;
                assert!(published >= watermark, "the watermark moved back");
                while let Some(ev) = reader.pop() {
                    assert!(ev.received > watermark, "{ev:?} below {watermark:?}");
                    seen.push(ev);
                }
                watermark = published;
                let due = events.iter().filter(|e| e.received <= watermark).count();
                assert!(
                    seen.len() >= due,
                    "seed {seed}: the prefix up to {watermark:?}"
                );
            }
            assert_eq!(seen, events, "seed {seed}");
        }
    }

    /// I1 on two threads: a consumer spinning beside the feeder, as the
    /// logic thread does, reads the watermark and then pops; whenever it
    /// looks, every event at or below what it read is already poppable,
    /// so a publication never runs ahead of a push within a poll.
    #[test]
    fn a_concurrent_consumer_never_sees_the_watermark_ahead_of_the_queue() {
        // Four-note chords every 3 ms: many batches, each of several
        // pushes, over 6 s.
        let mut b = ChartBuilder::new(150_000);
        for i in 0..2_000_i64 {
            for l in 0..4 {
                let l = u8::try_from(1 + (i + l) % 7).unwrap();
                b = b.tap(lane(l), TimeUs(i * 3 * MS));
            }
        }
        let events = autoplay(&b.build().unwrap(), &profile(0)).unwrap();
        let mut feeder = AutoplayInput::new(events.clone());
        let mut set = SourceSet::new(SourceConfig {
            queue_capacity: events.len(),
            ..set_config()
        })
        .unwrap();
        let mut reader = opened(&mut feeder, &mut set);
        let expected = events.clone();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        let go = std::sync::Arc::clone(&ready);
        let consumer = std::thread::spawn(move || {
            go.wait();
            let (mut seen, mut due) = (0, 0);
            while seen < expected.len() {
                let watermark = reader.published().received_up_to;
                while expected.get(due).is_some_and(|e| e.received <= watermark) {
                    due += 1;
                }
                while reader.pop().is_some() {
                    seen += 1;
                }
                assert!(
                    seen >= due,
                    "{seen} popped, {due} at or below {watermark:?}"
                );
            }
        });
        ready.wait();
        let mut rng = Lcg(7);
        let mut now = TimeUs(-1_000 * MS);
        while !feeder.is_done() {
            now.0 += 1 + i64::try_from(rng.below(2 * MS as u64)).unwrap();
            feeder.poll(now, &mut set);
            // Time for the consumer to look between polls, as the logic
            // thread runs beside the input thread.
            for _ in 0..rng.below(2_000) {
                std::hint::spin_loop();
            }
        }
        consumer.join().unwrap();
    }

    /// I2: with no event due, every poll still publishes `now − 1`: before
    /// the first event and after the last.
    #[test]
    fn autoplay_input_publishes_when_idle() {
        let mut feeder = AutoplayInput::new(autoplay(&chart(), &profile(0)).unwrap());
        let mut set = set();
        let reader = opened(&mut feeder, &mut set);
        for (now, published) in [(-999_000, -999_001), (-1, -2), (999_999, 999_998)] {
            feeder.poll(TimeUs(now), &mut set);
            assert_eq!(reader.published().received_up_to, TimeUs(published));
            assert_eq!(feeder.fed(), 0);
        }
        feeder.poll(TimeUs(3_000 * MS), &mut set);
        assert!(feeder.is_done());
        for now in [3_000_001, 3_000_002, 9_000_000] {
            feeder.poll(TimeUs(now), &mut set);
            assert_eq!(reader.published().received_up_to, TimeUs(now - 1));
        }
    }

    /// I3: a play closed while the scratch's long note is held: the
    /// restart releases it, so the next opening's snapshot holds nothing,
    /// and the next play replays every event from the first.
    #[test]
    fn autoplay_restart_replays_from_start_on_a_released_source() {
        let events = autoplay(&chart(), &profile(0)).unwrap();
        let mut feeder = AutoplayInput::new(events.clone());
        let mut set = set();
        let _first = opened(&mut feeder, &mut set);
        feeder.poll(TimeUs(1_100 * MS), &mut set);
        set.close_recording();
        assert!(!feeder.is_done());
        // Recording opened on the source as the first play left it would
        // hold the long note: the restart is what releases it.
        let held = set.open_recording(TimeUs(0)).held;
        assert_eq!(held, [(SourceId(0), control(lane(0)), 1)]);
        set.close_recording();
        feeder.restart(&mut set, Lane::all().map(control).collect());
        let start = set.open_recording(TimeUs(-1_000 * MS));
        assert!(start.held.is_empty(), "{:?}", start.held);
        let mut reader = set.take_readers().pop().unwrap();
        feeder.poll(TimeUs(3_000 * MS), &mut set);
        let mut replayed = Vec::new();
        while let Some(ev) = reader.pop() {
            replayed.push(ev);
        }
        assert_eq!(replayed, events);
    }

    /// I5: after the focus latch, with events due and room in the queue,
    /// a poll neither feeds an event nor moves the watermark.
    #[test]
    fn autoplay_input_stops_after_the_focus_latch() {
        let mut feeder = AutoplayInput::new(autoplay(&chart(), &profile(0)).unwrap());
        let mut set = set();
        let mut reader = opened(&mut feeder, &mut set);
        feeder.poll(TimeUs(1_010 * MS), &mut set);
        let fed = feeder.fed();
        assert!(fed > 0);
        let published = reader.published();
        while reader.pop().is_some() {}
        set.gate().latch();
        feeder.poll(TimeUs(2_100 * MS), &mut set);
        assert_eq!(feeder.fed(), fed);
        assert_eq!(reader.pop(), None);
        assert_eq!(reader.published(), published);
        assert!(set.gate().is_tripped());
    }

    /// Only the generated keyboard is fed: a device of another id that
    /// the set knows sees none of it.
    #[test]
    fn the_feeder_presses_only_the_generated_keyboard() {
        let mut feeder = AutoplayInput::new(autoplay(&chart(), &profile(0)).unwrap());
        let mut set = set();
        set.connect(DeviceId(1), DeviceKind::Keyboard);
        set.set_recorded(DeviceId(1), Lane::all().map(control).collect());
        feeder.restart(&mut set, Lane::all().map(control).collect());
        feeder.poll(TimeUs(1_010 * MS), &mut set);
        let start = set.open_recording(TimeUs(1_010 * MS));
        let devices: Vec<_> = set.participants().map(|(d, _)| d).collect();
        let held_by = |d: DeviceId| {
            let i = devices.iter().position(|&x| x == d).unwrap();
            start
                .held
                .iter()
                .filter(|h| h.0 == SourceId(u16::try_from(i).unwrap()))
                .count()
        };
        assert_eq!(held_by(AUTOPLAY_DEVICE), 4);
        assert_eq!(held_by(DeviceId(1)), 0);
    }
}
