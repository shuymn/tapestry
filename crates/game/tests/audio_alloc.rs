//! The audio thread neither allocates nor frees across retries, loads,
//! practice rounds' restores and the decision tone: each play's command
//! queue and restore, and each bank a load replaces, go back to the game
//! thread to be dropped
//! there, and the control messages travel through a preallocated queue. Only the audio loop's steps are counted
//! (see `counting`); this file holds a single test. Profiles and the
//! positive control: design.md "Scope of No Allocation".

mod common;
mod counting;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use eval::{JudgeEvent, JudgeEventKind};
use game::{EventSink, audio_control, audio_link_at};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

#[test]
fn audio_loop_neither_allocates_nor_frees_across_retries() {
    let chart = ChartBuilder::new(150_000)
        .sound("s.wav")
        .bgm(TimeUs(0), SoundId(0))
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        0,
    );
    let mut steps = |worker: &mut game::AudioWorker, n: usize| {
        counting::track(true);
        counting::self_check();
        for _ in 0..n {
            worker.step(&mut out).unwrap();
        }
        counting::track(false);
    };
    steps(&mut worker, 5);
    // The first play and 3 retries, each with key sounds; the second is
    // cancelled and the last sealed.
    for play in 0..4_i64 {
        let origin = TimeUs((100 + play * 400) * MS);
        // Between plays the queue deepens to three periods and back.
        control.request_queued_periods(if play % 2 == 0 { 3 } else { 2 });
        // Each play a volume change on every channel, which the counted
        // steps take.
        for (channel, volume) in [
            (game::Channel::System, 0.45),
            (game::Channel::Key, 0.2),
            (game::Channel::Background, 0.999),
        ] {
            control.set_volume(channel, volume).unwrap();
        }
        // Before the third play another chart.s sounds are loaded: the
        // audio thread swaps them in and hands the old ones back.
        if play == 2 {
            // First one that times out while the audio thread stalls: the
            // thread takes it late, in a counted step, and its answer is
            // freed by the next load, on the game thread.
            assert_eq!(
                control.load(
                    bank(&chart),
                    audio::BgmSchedule::new(chart.bgm.iter().map(|&e| e.into()).collect())
                ),
                Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
            );
            steps(&mut worker, 1);
            let next = bank(&chart);
            let schedule = chart.bgm.iter().map(|&e| e.into()).collect();
            std::thread::scope(|s| {
                let load = s.spawn(|| control.load(next, audio::BgmSchedule::new(schedule)));
                while !load.is_finished() {
                    steps(&mut worker, 1);
                }
                load.join().unwrap().unwrap();
            });
        }
        let (mut sink, queue) = audio_link_at(64, origin);
        // Each play carries its own gain, applied on the audio thread.
        let gain = audio::Gain::new(if play % 2 == 0 { 1.0 } else { 0.5 }).unwrap();
        control.play_at(origin, queue, gain, None).unwrap();
        for k in 0..3 {
            sink.receive(&JudgeEvent {
                seq: k,
                time: TimeUs(0),
                origin: Some(TimeUs(i64::try_from(k).unwrap() * 20 * MS)),
                kind: JudgeEventKind::PlaySound {
                    lane: lane(1),
                    sound: SoundId(0),
                },
            });
        }
        match play {
            1 => control.cancel(),
            3 => control.seal(),
            _ => {}
        }
        steps(&mut worker, 40);
    }
    assert!(control.done());
    assert_eq!(control.queued_periods(), Some(Some(2)));
    assert_eq!(counting::calls(), 0);
    // Three practice rounds whose restores are placed, fall back and are
    // refused, the last rung out to its end: the plans are made and freed
    // here, and travel with the starts.
    let restorer = game::Restorer::new(
        control.seek_source(),
        chart.bgm.iter().map(|&e| e.into()).collect(),
    );
    for (round, late) in [(0, 0), (1, 25), (2, 75)] {
        // Chart time 1 s is 200 ms after the opening.
        let written = i64::try_from(worker.engine().written()).unwrap();
        let origin = TimeUs(written * 1_000 / 48 + 10 * MS - 800 * MS);
        let restore = restorer
            .plan(control.counters().frame_line(origin), TimeUs(1_000 * MS))
            .unwrap();
        // A slow game thread: the output passes the first plan's frame, or
        // both plans' frames.
        steps(&mut worker, late);
        let (mut sink, queue) = audio_link_at(64, origin);
        control
            .play_at(origin, queue, audio::Gain::HEADROOM, Some(restore))
            .unwrap();
        sink.receive(&JudgeEvent {
            seq: 0,
            time: TimeUs(0),
            origin: Some(origin),
            kind: JudgeEventKind::PlaySound {
                lane: lane(1),
                sound: SoundId(0),
            },
        });
        steps(&mut worker, 2);
        let want = match round {
            0 => game::Restored::First,
            1 => game::Restored::Fallback,
            _ => game::Restored::Refused(game::RestoreRefusal::Late),
        };
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(want)
        );
        if round == 2 {
            control.ring_out(TimeUs(1_700 * MS), TimeUs(1_800 * MS));
        }
        steps(&mut worker, 10);
    }
    let mut n = 0;
    while !control.done() {
        steps(&mut worker, 1);
        n += 1;
        assert!(n < 200, "never done");
    }
    assert_eq!(counting::calls(), 0);
    // The decision tone: its first start, a start again while it sounds,
    // and the steps it sounds through, each start taken in the worker's
    // control step: nothing allocated or freed.
    control.play_tone();
    steps(&mut worker, 5);
    assert!(worker.engine().tone_sounding());
    control.play_tone();
    steps(&mut worker, 80);
    assert!(!worker.engine().tone_sounding());
    assert_eq!(counting::calls(), 0);
    // The system sounds (design.md "Audio", system sounds): the game
    // thread's draws, starts, stops and tone requests in play, and audio
    // steps that install a generation over one still sounding, send the
    // silent one back and leave it in the return ring, and take the empty
    // install, which sends the other back too. The generations are built
    // before, and the returns dropped after, what is counted.
    {
        use audio::{GenId, SystemGeneration, SystemKind};

        let sound = std::sync::Arc::new(audio::Sound::mono(vec![500; 4_800]));
        let pick = |id| {
            [SystemKind::Select, SystemKind::Scratch, SystemKind::Decide]
                .into_iter()
                .fold(SystemGeneration::new(GenId(id), RATE), |g, k| {
                    g.with(k, std::sync::Arc::clone(&sound))
                })
        };
        let ids: Vec<u64> = (0..3).map(|_| control.system_seq().unwrap()).collect();
        let (a, b) = (pick(ids[0]), pick(ids[1]));
        let empty = SystemGeneration::empty(GenId(ids[2]));
        let mut game = |f: &mut dyn FnMut(&mut game::AudioControl)| {
            counting::track(true);
            counting::self_check();
            f(&mut control);
            counting::track(false);
        };
        let mut a = Some(a);
        game(&mut |c| {
            c.install_system(a.take().unwrap()).unwrap();
            c.system_start(SystemKind::Select, true, Some(GenId(ids[0])))
                .unwrap();
            c.system_start(SystemKind::Scratch, false, None).unwrap();
            let _ = c.system_seq();
        });
        steps(&mut worker, 5);
        let mut b = Some(b);
        game(&mut |c| {
            c.install_system(b.take().unwrap()).unwrap();
            c.system_start(SystemKind::Decide, false, None).unwrap();
            c.system_stop(SystemKind::Scratch);
            c.play_tone();
        });
        steps(&mut worker, 5);
        assert_eq!(worker.engine().system().retired(), Some(GenId(ids[0])));
        game(&mut |c| {
            c.system_stop(SystemKind::Select);
        });
        steps(&mut worker, 3);
        assert_eq!(worker.engine().system().retired(), None);
        let mut empty = Some(empty);
        game(&mut |c| {
            c.install_system(empty.take().unwrap()).unwrap();
            c.system_start(SystemKind::Decide, false, None).unwrap();
        });
        steps(&mut worker, 60);
        assert_eq!(control.system_installed(), Some(GenId(ids[2])));
        assert_eq!(counting::calls(), 0, "the system sounds allocated or freed");
        let back: Vec<_> = std::iter::from_fn(|| control.take_system_return()).collect();
        assert_eq!(back.len(), 2, "both left in the return ring");
    }
    // Three gated practice attempts with one rearm each, the game thread
    // taking back none of the restores the rearms replace: the first fills
    // the return queue, the second's waits on the audio thread, and the
    // third rearm is set aside rather than free either there. Its
    // cancellation is still taken and acknowledged, and so is the exit,
    // with every restore still owned.
    let gate = input::StartGate::new();
    let transport = |generation| game::TransportGate {
        gate: gate.clone(),
        generation,
        commit_at: TimeUs(1_000 * MS),
    };
    let mut generation = 0;
    for attempt in 0..3 {
        let written = i64::try_from(worker.engine().written()).unwrap();
        let origin = TimeUs(written * 1_000 / 48 + 10 * MS - 800 * MS);
        let line = control.counters().frame_line(origin);
        let plan = restorer.plan(line, TimeUs(1_000 * MS)).unwrap();
        let spare = plan.clone();
        generation += 1;
        gate.arm(generation);
        let (_sink, queue) = audio_link_at(64, origin);
        control
            .play_gated(
                origin,
                queue,
                audio::Gain::HEADROOM,
                Some(plan),
                transport(generation),
            )
            .unwrap();
        steps(&mut worker, 2);
        assert_eq!(gate.cancel(), Some(generation));
        control.cancel();
        steps(&mut worker, 2);
        assert!(control.counters().dropped(generation));
        generation += 1;
        gate.arm(generation);
        control.rearm(origin, transport(generation), Some(spare));
        steps(&mut worker, 2);
        assert_eq!(gate.cancel(), Some(generation));
        control.cancel();
        steps(&mut worker, 2);
        assert!(control.counters().dropped(generation), "attempt {attempt}");
    }
    // The set-aside restore now waits to go back: a second cancel, then
    // another play and its cancel, still free nothing.
    control.cancel();
    steps(&mut worker, 2);
    let written = i64::try_from(worker.engine().written()).unwrap();
    let origin = TimeUs(written * 1_000 / 48 + 10 * MS - 800 * MS);
    let plan = restorer
        .plan(control.counters().frame_line(origin), TimeUs(1_000 * MS))
        .unwrap();
    generation += 1;
    gate.arm(generation);
    let (_sink, queue) = audio_link_at(64, origin);
    control
        .play_gated(
            origin,
            queue,
            audio::Gain::HEADROOM,
            Some(plan),
            transport(generation),
        )
        .unwrap();
    steps(&mut worker, 2);
    assert_eq!(gate.cancel(), Some(generation));
    control.cancel();
    control.cancel();
    steps(&mut worker, 2);
    assert!(control.counters().dropped(generation), "the last play");
    control.exit();
    counting::track(true);
    let running = worker.step(&mut out).unwrap();
    counting::track(false);
    assert!(!running, "the exit was not taken");
    assert_eq!(counting::calls(), 0, "the audio thread allocated or freed");
    // The restores the game thread never took are still owned, and go
    // back once it takes them.
    assert!(control.take_restore().is_some());
}
