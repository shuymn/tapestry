//! A completed play's handoff and its audio tail's feedback allocate
//! nothing and hold memory flat (design.md "Scope of No Allocation",
//! "Play Lifecycle", the audio tail): the completing step, the source
//! set's handoff, every tail batch of presses, releases and the scratch,
//! each feedback step's pops, staging, merge, conversion and view, the
//! key sounds' publication to the audio queue, the audio thread's steps
//! that take them and report the audio done, the done check, and the
//! screen's acquisition of each view. This file holds a single test, so
//! the harness's other threads are not tracked.

mod common;
mod counting;

use std::sync::Arc;

use audio::{AudioEngine, Gain, MAX_VOICES, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{ChartBuilder, Lane, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, control, device, lane};
use eval::ControlId;
use game::{Logic, LogicConfig, audio_control, audio_link_at};
use input::{DeviceId, DeviceKind, Packet, SourceConfig, SourceSet};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn the_handoff_and_the_tails_feedback_allocate_nothing() {
    let mut chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("key.wav")
        .bgm(TimeUs(0), SoundId(0))
        .tap(lane(1), TimeUs(500 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(1));
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![900; 20 * RATE as usize]),
            Sound::mono(vec![500; 480]),
        ],
    ));
    let rule = rules::tapestry();
    let dev = device(&[0]);
    let mut set = SourceSet::new(SourceConfig {
        max_devices: 2,
        max_sources: 1,
        max_held: 8,
        queue_capacity: 256,
        max_arrivals: 1,
    })
    .unwrap();
    let kb = DeviceId(1);
    set.begin_batch(TimeUs(0));
    set.connect(kb, DeviceKind::Keyboard);
    set.set_recorded(kb, Lane::all().map(control).collect::<Vec<ControlId>>());
    set.end_batch(TimeUs(1));
    let start = set.open_recording(TimeUs(10));
    let (mut logic, mut side) = Logic::new(
        &chart,
        &rule,
        &dev,
        &start,
        set.take_readers(),
        LogicConfig {
            inputs: 256,
            effects: 64,
        },
        Vec::new(),
    )
    .unwrap();
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut audio, mut worker) = audio_control(engine);
    let mut out = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let (sink, queue) = audio_link_at(logic.max_events(), TimeUs(0));
    logic.add_sink(Box::new(sink));
    let accepted = queue.accepted_count();
    audio
        .play_at(TimeUs(0), queue, Gain::HEADROOM, None)
        .unwrap();
    let audio_to = |worker: &mut game::AudioWorker, out: &mut SimDevice, at: i64| {
        while out.next_wake().0 <= at {
            worker.step(out).unwrap();
        }
    };
    audio_to(&mut worker, &mut out, 400 * MS);
    let batch = |set: &mut SourceSet, at: i64, packets: &[(Lane, i32)]| {
        set.begin_batch(TimeUs(at));
        for &(l, value) in packets {
            set.packet(&Packet {
                device: kb,
                control: control(l),
                value,
                background: false,
            });
        }
        set.end_batch(TimeUs(at + 1));
    };
    // Key 3 held into the tail.
    batch(&mut set, 450 * MS, &[(lane(3), 1)]);
    logic.advance_to(TimeUs(450 * MS));

    counting::track(true);
    counting::self_check();
    let before = counting::calls();
    batch(&mut set, 700 * MS, &[(lane(1), 1)]);
    let done = logic.advance_to(TimeUs(700 * MS));
    assert!(done.complete);
    logic.stop_playing();
    set.hand_off();
    audio.seal();
    // The view as the completion froze it, read off the tracked steps.
    let mut calls = counting::calls() - before;
    counting::track(false);
    let frozen = logic.view();
    counting::track(true);
    let before = counting::calls();
    let mut at = 700 * MS;
    let mut lit = 0;
    for k in 0..400 {
        at += 2 * MS;
        let press = k % 2 == 0;
        batch(
            &mut set,
            at,
            &[(lane(1), i32::from(press)), (lane(2), i32::from(press))],
        );
        logic.feedback_to().unwrap();
        audio_to(&mut worker, &mut out, at);
        assert!(!audio.done());
        let v = side.acquire(64, |_| {}).unwrap();
        lit += usize::from(v.input.lane(lane(1)).held);
        assert!(v.input.lane(lane(3)).held);
    }
    // Key 3 released at last.
    at += 2 * MS;
    batch(&mut set, at, &[(lane(3), 0)]);
    logic.feedback_to().unwrap();
    calls += counting::calls() - before;
    counting::track(false);
    assert_eq!(calls, 0);
    assert_eq!(lit, 200);
    // Every press edge of key 1 published its sound: the one at the
    // completion, held through the first tail batch, and 199 more.
    assert_eq!(accepted.load(std::sync::atomic::Ordering::Acquire), 200);
    let view = logic.view();
    assert_eq!(
        (
            view.tally,
            view.gauges,
            view.notes,
            view.finalized,
            view.events
        ),
        (
            frozen.tally,
            frozen.gauges,
            frozen.notes,
            frozen.finalized,
            frozen.events
        )
    );
    assert_eq!(logic.feedback_dropped(), 0);
    assert_eq!(set.feedback_dropped(), 0);
    assert!(!logic.view().input.lane(lane(3)).held);
}
