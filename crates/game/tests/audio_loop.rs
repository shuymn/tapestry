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
    // each at its own level (1000 and 1007 of i16::MAX + 1), the BGM
    // sound first and the key sound right after it, nothing else.
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let bgm = 1_000.0 / 32_768.0;
    let key = 1_007.0 / 32_768.0;
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
