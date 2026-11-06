//! The audio tail's feedback through the logic step (design.md "Play
//! Lifecycle", the audio tail): once evaluation is complete, the step keeps
//! converting the input that reaches its queues, from the staging the
//! completion left on, without judging or recording it
//! (`Logic::feedback_to`). Each fresh press sends its lane's unjudged
//! sound to the audio queue, each edge moves the view's lane input, and
//! the scored stream, the tally, the gauges, the note facts, the finalized
//! watermark and the record stay as the completion froze them.

mod common;

use bms_model::{ChartBuilder, Lane, SoundId, TimeUs};
use common::{MS, control, device, lane, press};
use eval::{
    AxisBinding, AxisMode, ControlId, InputKind, InputStart, LaneInput, PhysicalInputEvent,
    ScratchSettings, SourceId,
};
use game::{Logic, LogicConfig, audio_link_at};
use input::{
    DeviceId, DeviceKind, Packet, SourceConfig, SourceReader, SourceSet, SourceWriter, source_queue,
};

const CONFIG: LogicConfig = LogicConfig {
    inputs: 64,
    effects: 64,
};
/// Where the play's chart time 0 lies on the output's timeline.
const SHIFT: i64 = 5_000 * MS;
/// The scratch's axis on source 0.
const AXIS: ControlId = ControlId(0x1_0030);

fn release(received: i64, source: u16, l: Lane) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Control {
            control: control(l),
            value: 0,
        },
    }
}

fn input(held: bool, pressed: Option<i64>, released: Option<i64>) -> LaneInput {
    LaneInput {
        held,
        pressed: pressed.map(TimeUs),
        released: released.map(TimeUs),
        presses: u32::from(pressed.is_some()),
    }
}

/// Key 1's taps at 1.0 s (sound 1) and 1.1 s (sound 2), key 2's at 1.05 s
/// without a sound, key 3 without notes: the play completes at the last
/// tap's late BAD edge.
fn chart() -> bms_model::Chart {
    let mut chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("one.wav")
        .sound("two.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_100 * MS))
        .tap(lane(2), TimeUs(1_050 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(1));
    chart.lanes[1][1].sound = Some(SoundId(2));
    chart
}

/// Pushes `events` to their sources' writers, then has each publish its
/// time in `published` (µs).
fn feed(writers: &mut [SourceWriter], events: &[PhysicalInputEvent], published: &[i64]) {
    for ev in events {
        writers[usize::from(ev.source.0)].push(*ev).unwrap();
    }
    for (w, &t) in writers.iter_mut().zip(published) {
        w.publish(TimeUs(t));
    }
}

/// Fresh presses after completion, source 0 with a 3 ms input offset and
/// ahead of source 1: the press source 0 had queued past the completing
/// watermark stays staged and is converted by the tail's first feedback
/// step; it and the later presses sound by their lane's latest target,
/// key 2's soundless target and key 3's empty lane stay silent, every
/// edge reaches the view at its evaluation time, and the record, the live
/// stream, the tally, the gauges, the note facts and the finalized
/// watermark are those of the completion.
#[test]
fn tail_presses_sound_and_light_without_changing_the_play() {
    let chart = chart();
    let rule = rules::tapestry();
    let dev = device(&[3 * MS, 0]);
    let (mut writers, readers): (Vec<_>, Vec<_>) = (0..2).map(|_| source_queue(64)).unzip();
    let (mut logic, _side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        readers,
        CONFIG,
        Vec::new(),
    )
    .unwrap();
    let (sink, mut sounds) = audio_link_at(logic.max_events(), TimeUs(SHIFT));
    logic.add_sink(Box::new(sink));
    // Key 1 judged at 1.0 s; then source 0 runs ahead with a press at
    // 1.5 s, beyond the completing watermark source 1 holds at 1.4 s.
    feed(
        &mut writers,
        &[
            press(1_003 * MS, 0, lane(1)),
            release(1_040 * MS, 0, lane(1)),
        ],
        &[1_200 * MS, 1_200 * MS],
    );
    assert!(!logic.advance_to(TimeUs(1_200 * MS)).complete);
    feed(
        &mut writers,
        &[press(1_500 * MS, 0, lane(1))],
        &[2_000 * MS, 1_400 * MS],
    );
    let done = logic.advance_to(TimeUs(1_400 * MS));
    assert!(done.complete);
    assert_eq!(done.watermark, Some(TimeUs(1_400 * MS)));
    logic.stop_playing();
    let frozen = logic.view();
    let record = logic.record();
    // The staged press is in the record, as the completion popped it.
    assert_eq!(record.events.len(), 3);
    while let Ok(c) = sounds.commands().pop() {
        assert_eq!(c.sound, SoundId(1), "{c:?}");
    }
    let accepted = sounds.accepted();

    // The tail: source 1 catches up, and more presses come.
    feed(
        &mut writers,
        &[
            press(1_600 * MS, 1, lane(2)),
            press(1_650 * MS, 1, lane(3)),
            release(1_700 * MS, 0, lane(1)),
        ],
        &[2_000 * MS, 2_000 * MS],
    );
    assert_eq!(logic.feedback_to(), Some(TimeUs(1_997 * MS)));
    let first = sounds.commands().pop().unwrap();
    assert_eq!(
        (first.sound, first.origin),
        (SoundId(2), TimeUs(SHIFT + 1_500 * MS))
    );
    assert!(
        sounds.commands().pop().is_err(),
        "a soundless press sounded"
    );
    assert_eq!(sounds.accepted(), accepted + 1);
    let view = logic.view();
    assert_eq!(
        view.input.lane(lane(1)),
        input(false, Some(1_497 * MS), Some(1_697 * MS))
    );
    assert_eq!(
        view.input.lane(lane(2)),
        input(true, Some(1_600 * MS), None)
    );
    assert_eq!(
        view.input.lane(lane(3)),
        input(true, Some(1_650 * MS), None)
    );
    assert_eq!(view.input.converted(), Some(TimeUs(1_997 * MS)));
    // Everything scored as the completion froze it.
    assert_eq!(view.tally, frozen.tally);
    assert_eq!(view.gauges, frozen.gauges);
    assert_eq!(view.notes, frozen.notes);
    assert_eq!(view.finalized, frozen.finalized);
    assert_eq!(view.events, frozen.events);
    assert!(view.complete && !view.playing);
    assert!(logic.events().is_empty());
    assert_eq!(logic.feedback_dropped(), 0);
    assert_eq!(logic.into_record(), record);
}

/// A key held across the completion and the scratch's pending release
/// carry on through the source set's handoff, with no gap and no repeated
/// press: the key's release in the tail is its only new edge, the axis's
/// release deadline falls in the tail with no packet, at its exact time,
/// and the axis moves on from its position as of the completion.
#[test]
fn held_keys_and_the_scratch_carry_on_across_the_handoff() {
    let chart = chart();
    let rule = rules::tapestry();
    let mut dev = device(&[0]);
    dev.sources[0].axes.push(AxisBinding {
        control: AXIS,
        lane: Lane::SCRATCH,
        scratch: ScratchSettings {
            mode: AxisMode::Absolute { wrap: 256 },
            ..ScratchSettings::DEFAULT
        },
    });
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 1,
        max_held: 8,
        queue_capacity: 64,
        max_arrivals: 1,
    })
    .unwrap();
    let kb = DeviceId(1);
    set.begin_batch(TimeUs(0));
    set.connect(kb, DeviceKind::Keyboard);
    let mut recorded: Vec<ControlId> = Lane::all().map(control).collect();
    recorded.push(AXIS);
    set.set_recorded(kb, recorded);
    set.end_batch(TimeUs(1));
    let start = set.open_recording(TimeUs(10));
    let (mut logic, _side) = Logic::new(
        &chart,
        &rule,
        &dev,
        &start,
        set.take_readers(),
        CONFIG,
        Vec::new(),
    )
    .unwrap();
    let batch = |set: &mut SourceSet, at: i64, packets: &[(ControlId, i32)]| {
        set.begin_batch(TimeUs(at));
        for &(control, value) in packets {
            set.packet(&Packet {
                device: kb,
                control,
                value,
                background: false,
            });
        }
        set.end_batch(TimeUs(at + 1));
    };
    // Key 3 held from 0.9 s; the scratch pressed at 1.25 s by an axis at
    // 10 → 12 (a release deadline at 1.35 s).
    batch(&mut set, 900 * MS, &[(control(lane(3)), 1)]);
    batch(&mut set, 1_240 * MS, &[(AXIS, 10)]);
    batch(&mut set, 1_250 * MS, &[(AXIS, 12)]);
    batch(&mut set, 1_300 * MS, &[]);
    assert!(logic.advance_to(TimeUs(1_300 * MS)).complete);
    logic.stop_playing();
    let record = logic.record();
    set.hand_off();
    assert!(set.is_recording() && set.is_handed_off());
    let view = logic.view().input;
    assert_eq!(view.lane(lane(3)), input(true, Some(900 * MS), None));
    assert_eq!(
        view.lane(Lane::SCRATCH),
        input(true, Some(1_250 * MS), None)
    );
    // The tail: no packet until 1.4 s, then key 3 released and the axis
    // on to 13, one position from where it stood: a hold's movement that
    // presses nothing from a released lane.
    batch(&mut set, 1_400 * MS, &[]);
    logic.feedback_to().unwrap();
    assert_eq!(
        logic.view().input.lane(Lane::SCRATCH),
        input(false, Some(1_250 * MS), Some(1_350 * MS))
    );
    batch(&mut set, 1_500 * MS, &[(control(lane(3)), 0), (AXIS, 13)]);
    logic.feedback_to().unwrap();
    let view = logic.view().input;
    assert_eq!(
        view.lane(lane(3)),
        input(false, Some(900 * MS), Some(1_500 * MS))
    );
    assert_eq!(
        view.lane(Lane::SCRATCH),
        input(false, Some(1_250 * MS), Some(1_350 * MS))
    );
    // Two positions on in one direction press it again.
    batch(&mut set, 1_600 * MS, &[(AXIS, 15)]);
    logic.feedback_to().unwrap();
    assert_eq!(
        logic.view().input.lane(Lane::SCRATCH),
        input(true, Some(1_600 * MS), Some(1_350 * MS))
    );
    assert_eq!(logic.record(), record);
    // Once the sources close, nothing more reaches the queues.
    set.close_recording();
    batch(&mut set, 1_700 * MS, &[(control(lane(3)), 1)]);
    assert_eq!(logic.feedback_to(), Some(TimeUs(1_600 * MS)));
    assert!(!logic.view().input.lane(lane(3)).held);
}

/// Autoplay's generated release after the completion (its 80 ms tap hold
/// past a 45 ms late BAD edge) goes through the tail's feedback: the lane
/// is released at its generated time, and a sink that sends no key sounds
/// (autoplay's are on the engine's schedule) accepts nothing.
#[test]
fn autoplays_pending_release_goes_through_the_tail() {
    let chart = ChartBuilder::new(150_000)
        .sound("one.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .map(|mut c| {
            c.lanes[1][0].sound = Some(SoundId(0));
            c
        })
        .unwrap();
    let mut rule = rules::tapestry();
    let keys = &mut rule.tables.keys;
    keys.windows = keys.windows.scaled(25_000).unwrap();
    let dev = device(&[0]);
    let events = input::autoplay(&chart, &dev.sources[0]).unwrap();
    assert_eq!(events.len(), 2);
    let (mut writer, reader) = source_queue(8);
    let (mut logic, _side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        vec![reader],
        CONFIG,
        Vec::new(),
    )
    .unwrap();
    let (sink, sounds) = audio_link_at(logic.max_events(), TimeUs(0));
    logic.add_sink(Box::new(sink.without_key_sounds()));
    writer.push(events[0]).unwrap();
    writer.publish(TimeUs(1_046 * MS));
    assert!(logic.advance_to(TimeUs(1_046 * MS)).complete);
    logic.stop_playing();
    assert_eq!(
        logic.view().input.lane(lane(1)),
        input(true, Some(1_000 * MS), None)
    );
    writer.push(events[1]).unwrap();
    writer.publish(TimeUs(2_000 * MS));
    assert_eq!(logic.feedback_to(), Some(TimeUs(2_000 * MS)));
    assert_eq!(
        logic.view().input.lane(lane(1)),
        input(false, Some(1_000 * MS), Some(1_080 * MS))
    );
    assert_eq!(sounds.accepted(), 0);
}

/// A practice section's tail sounds only the section's targets: a press
/// after it on key 1, whose notes lie outside the section, is silent;
/// one on key 2, whose note in the section is given a sound here, sounds
/// that note's sound.
#[test]
fn a_sections_tail_sounds_only_its_targets() {
    let mut chart = chart();
    chart.lanes[2][0].sound = Some(SoundId(0));
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let section = eval::Section {
        start: TimeUs(1_050 * MS),
        end: TimeUs(1_060 * MS),
        lead_in: TimeUs(0),
        initial_gauges: rule.gauges.iter().map(|g| g.start).collect(),
    };
    let (mut writer, reader) = source_queue(8);
    let (mut logic, _side) = Logic::<SourceReader>::with_section(
        &chart,
        &rule,
        &dev,
        &InputStart::default(),
        Some(&section),
        vec![reader],
        CONFIG,
        Vec::new(),
    )
    .unwrap();
    let (sink, mut sounds) = audio_link_at(logic.max_events(), TimeUs(0));
    logic.add_sink(Box::new(sink));
    writer.publish(TimeUs(1_400 * MS));
    assert!(logic.advance_to(TimeUs(1_400 * MS)).complete);
    writer.push(press(1_500 * MS, 0, lane(1))).unwrap();
    writer.push(press(1_500 * MS, 0, lane(2))).unwrap();
    writer.publish(TimeUs(1_600 * MS));
    logic.feedback_to().unwrap();
    let heard = sounds.commands().pop().unwrap();
    assert_eq!(
        (heard.sound, heard.origin),
        (SoundId(0), TimeUs(1_500 * MS))
    );
    assert!(sounds.commands().pop().is_err(), "key 1 sounded");
    let input = logic.view().input;
    assert!(input.lane(lane(1)).held && input.lane(lane(2)).held);
}
