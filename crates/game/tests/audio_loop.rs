//! The audio thread's loop and its control, stepped on one thread over a
//! simulated device: silence until the first play, and the sealed play's
//! audio reported done only once its sounds have ended.

mod common;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use eval::{JudgeEvent, JudgeEventKind};
use game::{AudioControl, AudioWorker, EventSink, audio_control, audio_link_at};

/// Two sounds of 480 frames at 48 kHz (10 ms), sound 0 played as BGM at
/// chart 0 and sound 1 as the key sound. The bank fills sound `i` with the
/// constant `1000 + 7 i`.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("key.wav")
        .bgm(TimeUs(0), SoundId(0))
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap()
}

fn rig(chart: &Chart) -> (AudioControl, AudioWorker, RecordingOutput<SimDevice>) {
    let engine = AudioEngine::new(bank(chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (control, worker) = audio_control(engine);
    let out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        RATE as usize,
    );
    (control, worker, out)
}

fn key_sound(origin: i64) -> JudgeEvent {
    JudgeEvent {
        seq: 0,
        time: TimeUs(origin),
        origin: Some(TimeUs(origin)),
        kind: JudgeEventKind::PlaySound {
            lane: lane(1),
            sound: SoundId(1),
        },
    }
}

#[test]
fn nothing_sounds_before_the_first_play() {
    let chart = chart();
    let (_control, mut worker, mut out) = rig(&chart);
    // 20 periods (200 ms) without a play: chart time 0 would be at the
    // output's time 0, where the BGM sound is.
    for _ in 0..20 {
        assert_eq!(worker.step(&mut out), Ok(true));
    }
    assert!(out.samples().len() >= 20 * PERIOD as usize * 2);
    assert!(out.samples().iter().all(|&s| s == 0.0));
}

#[test]
fn a_sealed_play_is_done_once_its_sounds_have_ended() {
    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    for _ in 0..3 {
        worker.step(&mut out).unwrap();
    }
    // Chart time 0 at 50 ms: the BGM sound starts there, and a key sound
    // pressed at 60 ms is queued before the seal.
    let (mut sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control.play(TimeUs(50 * MS), queue).unwrap();
    sink.receive(&key_sound(10 * MS));
    control.seal();
    assert!(
        !control.done(),
        "done before the audio thread took the seal"
    );
    worker.step(&mut out).unwrap();
    assert!(!control.done(), "done while the sounds are still to play");
    let mut steps = 1;
    while !control.done() {
        worker.step(&mut out).unwrap();
        steps += 1;
        assert!(steps < 100, "never done");
    }
    // Both 10 ms sounds (480 frames each) were heard before it was done,
    // each at its own level (1000 and 1007 of i16::MAX + 1, halved by the
    // mixer's headroom), the BGM sound first and the key sound right after
    // it, nothing else.
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let bgm = 1_000.0 / 65_536.0;
    let key = 1_007.0 / 65_536.0;
    let heard: Vec<usize> = (0..left.len()).filter(|&f| left[f] != 0.0).collect();
    assert_eq!(heard.len(), 960);
    let (first, second) = heard.split_at(480);
    assert!(first.iter().all(|&f| left[f] == bgm), "BGM at {first:?}");
    assert!(second.iter().all(|&f| left[f] == key), "key at {second:?}");
    // Each a contiguous 10 ms run.
    assert_eq!(first[479] - first[0], 479);
    assert_eq!(second[479] - second[0], 479);
    // A play that is not sealed is never done, even when silent: this one
    // is cancelled, which leaves no BGM to come.
    let (_sink, queue) = audio_link_at(4, TimeUs(1_000 * MS));
    control.play(TimeUs(1_000 * MS), queue).unwrap();
    control.cancel();
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    assert!(!control.done());
}

/// The audio thread is gone before it took a play's switch: waiting for
/// it reports that at once, so the next play gets an error, not a hang.
#[test]
fn a_gone_worker_fails_the_next_play() {
    let chart = chart();
    let (mut control, worker, _out) = rig(&chart);
    let (_sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control.play(TimeUs(50 * MS), queue).unwrap();
    drop(worker);
    let started = std::time::Instant::now();
    let (_sink, queue) = audio_link_at(4, TimeUs(500 * MS));
    assert_eq!(
        control.play(TimeUs(500 * MS), queue),
        Err(game::AudioWaitError::Gone)
    );
    assert_eq!(control.ready(), Err(game::AudioWaitError::Gone));
    assert!(started.elapsed() < game::SWITCH_TIMEOUT / 2);
    assert_eq!(control.failure(), Some(audio::OutputError::DeviceLost));
}

/// The master volume set from the game thread scales what the audio
/// thread mixes from its next step on, during a play: the BGM sound at
/// unity, then key sounds at one half, at zero and at one quarter. A
/// volume outside 0 to the mixer's largest is refused and changes nothing.
#[test]
fn volume_change_scales_mixed_samples() {
    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    let (mut sink, queue) = audio_link_at(8, TimeUs(50 * MS));
    control.play(TimeUs(50 * MS), queue).unwrap();
    let steps = |worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>| {
        for _ in 0..15 {
            worker.step(out).unwrap();
        }
    };
    steps(&mut worker, &mut out);
    for (volume, pressed) in [(0.5, 150), (0.0, 300), (0.25, 450)] {
        control.set_volume(volume).unwrap();
        if volume == 0.25 {
            for refused in [-0.01, audio::MAX_VOLUME * 2.0, f32::NAN] {
                assert!(control.set_volume(refused).is_err(), "{refused}");
            }
        }
        sink.receive(&key_sound(pressed * MS));
        // The sound is playing, even at volume 0.
        worker.step(&mut out).unwrap();
        let mixer = worker.engine().mixer();
        assert!(mixer.pending() + mixer.sounding() > 0, "at volume {volume}");
        steps(&mut worker, &mut out);
    }
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let heard: Vec<f32> = left.iter().copied().filter(|&s| s != 0.0).collect();
    // Exact: each level, each volume and the mixer's headroom (0.5) is a
    // power-of-two fraction.
    let bgm = 1_000.0 / 65_536.0;
    let key = 1_007.0 / 65_536.0;
    let mut expected = vec![bgm; 480];
    expected.extend([key * 0.5; 480]);
    expected.extend([key * 0.25; 480]);
    assert_eq!(heard, expected);
}

/// Each key sound's press → sound scheduled latency reaches the probe,
/// tagged with its play: the time the device plays the sound's first
/// frame (found in the recording and timed by the simulated device, not
/// by the engine's mix clock) minus the press's receive time. The
/// underrun count is readable from another thread's handle.
#[test]
fn the_probe_reports_each_key_sound_with_its_play() {
    let chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("key.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 4).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        RATE as usize,
    );
    let (tx, mut rx) = rtrb::RingBuffer::new(8);
    worker.set_probe(tx);
    let counters = control.counters();
    for _ in 0..3 {
        worker.step(&mut out).unwrap();
    }
    // Chart time 0 at 30 ms; the press at chart time 3 ms, 33 ms on the
    // output's timeline.
    let (mut sink, queue) = audio_link_at(4, TimeUs(30 * MS));
    control.play(TimeUs(30 * MS), queue).unwrap();
    assert_eq!(control.generation(), 1);
    worker.step(&mut out).unwrap();
    sink.receive(&key_sound(3 * MS));
    for _ in 0..5 {
        worker.step(&mut out).unwrap();
    }
    let probe = rx.pop().expect("a key sound probe");
    assert!(rx.pop().is_err(), "one key sound, one probe");
    assert_eq!(probe.generation, 1);
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let first = left.iter().position(|&s| s != 0.0).expect("the key sound") as u64;
    let played = out.inner().true_time(first);
    let game::Probed::KeyScheduled { latency } = probe.kind;
    assert!(latency.0 > 0);
    assert_eq!(latency, TimeUs(played.0 - 33 * MS));
    assert_eq!(counters.underruns(), 0);
}

/// Three missed buffer events leave the device without audio: the
/// handle reads the engine's count of wakes that found the buffer empty
/// (D11), which sees the starved stretch at least once and at most as
/// often as the simulated device counted short passes.
#[test]
fn underruns_are_counted_as_the_device_saw_them() {
    let chart = chart();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (control, mut worker) = audio_control(engine);
    let config = SimConfig {
        missed_wakes: vec![5, 6, 7],
        ..SimConfig::steady(RATE, PERIOD, TimeUs(0))
    };
    let mut out = RecordingOutput::new(SimDevice::new(config), RATE as usize);
    let counters = control.counters();
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    let seen = out.inner().underruns();
    assert!((1..=seen).contains(&counters.underruns()), "{seen}");
    assert_eq!(counters.underruns(), worker.engine().underruns());
}

/// Probe samples the full probe queue refuses are counted, not lost
/// silently: three key sounds scheduled in one step into a queue of one.
#[test]
fn dropped_probe_samples_are_counted() {
    let chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("key.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 8).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        RATE as usize,
    );
    let (tx, mut rx) = rtrb::RingBuffer::new(1);
    worker.set_probe(tx);
    let counters = control.counters();
    for _ in 0..3 {
        worker.step(&mut out).unwrap();
    }
    let (mut sink, queue) = audio_link_at(8, TimeUs(30 * MS));
    control.play(TimeUs(30 * MS), queue).unwrap();
    worker.step(&mut out).unwrap();
    for press in [1, 2, 3] {
        sink.receive(&key_sound(press * MS));
    }
    worker.step(&mut out).unwrap();
    assert!(rx.pop().is_ok());
    assert!(rx.pop().is_err());
    assert_eq!(counters.probes_dropped(), 2);
}
