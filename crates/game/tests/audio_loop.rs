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

/// A practice round's ring-out on the audio thread (design.md "Mid-Chart
/// Start and Section Repeat"): the BGM plays on, fades to silence at the
/// ring-out's end, where every sound stops and a later BGM event never
/// starts, and the play is done once the output has reached that end.
#[test]
fn a_rung_out_play_is_done_at_its_end() {
    use std::sync::Arc;

    // A 2 s sound at 0, and another at 1 s, past the ring-out's end.
    let bank = Arc::new(audio::SoundBank::from_sounds(
        RATE,
        vec![audio::Sound::mono(vec![4_000; 2 * RATE as usize])],
    ));
    let bgm = [0, 1_000 * MS].map(|t| bms_model::BgmEvent {
        time: TimeUs(t),
        sound: SoundId(0),
    });
    let engine = AudioEngine::new(bank, &bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        2 * RATE as usize,
    );
    // Chart time 0 at 50 ms: a ring-out fading from 300 ms to 500 ms.
    let (_sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control.play(TimeUs(50 * MS), queue).unwrap();
    control.ring_out(TimeUs(300 * MS), TimeUs(500 * MS));
    // Frame `n` plays at `(n + 480) / RATE`: the end is frame 25 920.
    let end = (550 - 10) * 48;
    let mut steps = 0;
    while !control.done() {
        assert!(worker.engine().written() < end as u64 + 3 * u64::from(PERIOD));
        worker.step(&mut out).unwrap();
        steps += 1;
        assert!(steps < 200, "never done");
    }
    assert!(worker.engine().written() >= end as u64);
    let left: Vec<f32> = out.samples().iter().step_by(2).copied().collect();
    let full = 4_000.0 / 65_536.0;
    let fade = (350 - 10) * 48;
    assert_eq!(left[fade - 1], full);
    assert!(left[fade + 1] < full && left[end - 1] > 0.0);
    assert!(left[end..].iter().all(|&x| x == 0.0));
    // Nothing sounds afterwards: the event at 1 s never starts.
    for _ in 0..80 {
        worker.step(&mut out).unwrap();
    }
    assert!(out.samples().iter().skip(2 * end).all(|&x| x == 0.0));
    assert_eq!(worker.engine().bgm_left(), 0);
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

/// A play's gain travels with its start and holds for the whole play: a
/// play at gain 1 mixes at twice the level of one at the 0.5 headroom,
/// while the volume still changes mid-play. A gain out of range cannot be
/// made, so no start carries one.
#[test]
fn a_plays_gain_applies_from_its_play() {
    for refused in [0.0, -0.5, f32::NAN, f32::INFINITY, audio::MAX_GAIN * 1.01] {
        assert!(audio::Gain::new(refused).is_err(), "{refused}");
    }
    assert!(audio::Gain::new(audio::MAX_GAIN).is_ok());
    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    let steps = |worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>| {
        for _ in 0..15 {
            worker.step(out).unwrap();
        }
    };
    let (mut sink, queue) = audio_link_at(8, TimeUs(50 * MS));
    let unity = audio::Gain::new(1.0).unwrap();
    control
        .play_at(TimeUs(50 * MS), queue, unity, None)
        .unwrap();
    steps(&mut worker, &mut out);
    assert_eq!(worker.engine().mixer().gain(), 1.0);
    // The volume halves mid-play; the gain stays.
    control.set_volume(0.5).unwrap();
    sink.receive(&key_sound(150 * MS));
    steps(&mut worker, &mut out);
    // The next play, at the headroom: its BGM at a quarter (volume 0.5).
    let (_sink, queue) = audio_link_at(8, TimeUs(600 * MS));
    control.play(TimeUs(600 * MS), queue).unwrap();
    for _ in 0..4 {
        steps(&mut worker, &mut out);
    }
    assert_eq!(worker.engine().mixer().gain(), audio::HEADROOM);
    let heard: Vec<f32> = out
        .samples()
        .iter()
        .step_by(2)
        .copied()
        .filter(|&s| s != 0.0)
        .collect();
    // Exact: every level, volume and gain is a power-of-two fraction.
    let bgm = 1_000.0 / 32_768.0;
    let key = 1_007.0 / 32_768.0;
    let mut expected = vec![bgm; 480];
    expected.extend([key * 0.5; 480]);
    expected.extend([bgm * 0.5 * 0.5; 480]);
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

/// Three missed buffer events leave the device without audio: the handle reads
/// the engine's count of wakes that found the buffer empty (design.md "Latency
/// Figures"), which sees the starved stretch at least once and at most as often
/// as the simulated device counted short passes.
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

/// The handle reads the engine's deferred wakes and stolen voices: 1,100
/// BGM sounds at one time overflow the mixer's 1,024 pending starts, and
/// two voices take them in turn.
#[test]
fn deferred_starts_and_stolen_voices_are_counted() {
    let mut builder = ChartBuilder::new(150_000).sound("key.wav");
    for i in 1..=1_100 {
        builder = builder
            .sound(&format!("bgm{i}.wav"))
            .bgm(TimeUs(0), SoundId(i));
    }
    let chart = builder.tap(lane(1), TimeUs(1_000 * MS)).build().unwrap();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, 2, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        RATE as usize,
    );
    let counters = control.counters();
    let (_sink, queue) = audio_link_at(4, TimeUs(30 * MS));
    control.play(TimeUs(30 * MS), queue).unwrap();
    for _ in 0..10 {
        worker.step(&mut out).unwrap();
    }
    assert!(counters.deferred_wakes() >= 1);
    assert_eq!(counters.deferred_wakes(), worker.engine().deferred_wakes());
    assert!(counters.steals() >= 1_000, "{}", counters.steals());
    assert_eq!(counters.steals(), worker.engine().mixer().steals());
}

/// Another chart's bank, taken between plays on the running audio thread:
/// the next play sounds the new bank under the same sound ids, never the
/// old one, and nothing sounds between the load and that play. A bank at
/// another rate is refused before it is sent, and a gone worker refuses
/// a load.
#[test]
fn a_loaded_bank_replaces_the_previous_charts_sounds() {
    use std::sync::Arc;

    let chart = chart();
    let engine = AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
    let (mut control, mut worker) = audio_control(engine);
    let mut device = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    // The old chart's BGM sound is the constant 1000; the new one's 5000.
    let new = Arc::new(audio::SoundBank::from_sounds(
        RATE,
        vec![audio::Sound::mono(vec![5_000; 480])],
    ));
    let schedule = vec![bms_model::BgmEvent {
        time: TimeUs(0),
        sound: SoundId(0),
    }];
    // The load waits for the audio thread, stepped here meanwhile. How many
    // steps that takes depends on when the loading thread runs, so they go
    // to the bare device: recorded, they could fill the recording.
    std::thread::scope(|s| {
        let load = s.spawn(|| control.load(Arc::clone(&new), audio::BgmSchedule::new(schedule)));
        while !load.is_finished() {
            worker.step(&mut device).unwrap();
        }
        load.join().unwrap().unwrap();
    });
    // Recorded from here on: a few periods between the load and the play.
    let mut out = RecordingOutput::new(device, RATE as usize);
    for _ in 0..3 {
        worker.step(&mut out).unwrap();
    }
    let silent = out.samples().len();
    assert_eq!(silent, 3 * PERIOD as usize * 2);
    let other = Arc::new(audio::SoundBank::from_sounds(44_100, Vec::new()));
    assert_eq!(
        control.load(other, audio::BgmSchedule::new(Vec::new())),
        Err(game::AudioLoadError::RateMismatch {
            bank: 44_100,
            output: RATE
        })
    );
    assert!(out.samples().iter().all(|&s| s == 0.0));
    // Chart time 0 50 ms after the next wake, wherever the load left the
    // output's clock.
    let origin = TimeUs(out.inner().next_wake().0 + 50 * MS);
    let (_sink, queue) = audio_link_at(4, origin);
    control.play(origin, queue).unwrap();
    control.seal();
    let mut steps = 0;
    while !control.done() {
        worker.step(&mut out).unwrap();
        steps += 1;
        assert!(steps < 100, "the play never ended");
    }
    assert_eq!(out.dropped(), 0);
    assert!(out.samples().len() > silent);
    drop(worker);
    let new_level = 5_000.0 / 32_768.0 / 2.0;
    let old_level = 1_000.0 / 32_768.0 / 2.0;
    let near = |s: f32, level: f32| (s - level).abs() < 1e-4;
    assert!(out.samples().iter().any(|&s| near(s, new_level)));
    assert!(!out.samples().iter().any(|&s| near(s, old_level)));
    assert_eq!(
        control.load(new, audio::BgmSchedule::new(Vec::new())),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Gone))
    );
}

/// A load that times out while the audio thread is stalled stays
/// outstanding: every later load is refused at once and sends nothing, so
/// the control queue never fills, until the stalled thread answers; that
/// answer is freed on the game thread with what it replaced, and the next
/// load goes through.
#[test]
fn a_timed_out_load_refuses_others_until_it_is_answered() {
    use std::sync::Arc;

    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    let first = Arc::new(audio::SoundBank::from_sounds(RATE, Vec::new()));
    // The worker is stalled: nothing steps it.
    assert_eq!(
        control.load(Arc::clone(&first), audio::BgmSchedule::new(Vec::new())),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    let started = std::time::Instant::now();
    for _ in 0..32 {
        let other = Arc::new(audio::SoundBank::from_sounds(RATE, Vec::new()));
        assert_eq!(
            control.load(other, audio::BgmSchedule::new(Vec::new())),
            Err(game::AudioLoadError::Busy)
        );
    }
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    // The thread recovers: it takes the timed-out load and answers it.
    worker.step(&mut out).unwrap();
    assert!(Arc::ptr_eq(worker.engine().bank(), &first));
    // The next load frees that answer here, then goes through.
    let second = Arc::new(audio::SoundBank::from_sounds(RATE, Vec::new()));
    std::thread::scope(|s| {
        let load =
            s.spawn(|| control.load(Arc::clone(&second), audio::BgmSchedule::new(Vec::new())));
        while !load.is_finished() {
            worker.step(&mut out).unwrap();
        }
        load.join().unwrap().unwrap();
    });
    assert!(Arc::ptr_eq(worker.engine().bank(), &second));
    // The first bank's answer and its replacement were freed on this
    // thread: only the engine holds the first's successor now.
    assert_eq!(Arc::strong_count(&first), 1);
}

/// A practice round's restore on the audio thread (design.md "Mid-Chart
/// Start and Section Repeat"): the round opens at chart time 7.8 s and its
/// audio is restored at its lead-in's start, 8 s. Sound 0 lasts 4 s at the
/// level `1 + its millisecond`, played as BGM at 5 s, so it is 3 s in at
/// 8 s; sound 1 (10 ms at 2,000) is BGM at 8.05 s; sound 2 (10 ms at
/// 3,000) is the key sound.
mod restore {
    use std::sync::Arc;

    use audio::{
        AudioEngine, Gain, MAX_VOICES, RecordingOutput, SeekPlan, SimConfig, SimDevice, Sound,
        SoundBank,
    };
    use bms_model::{BgmEvent, SoundId, TimeUs};
    use game::{
        AudioControl, AudioWorker, EventSink, RESTORE_RETRY, RestoreRefusal, Restored, Restorer,
        audio_control, audio_link_at,
    };

    use super::common::{MS, PERIOD, RATE, lane};

    const OPENS_AT: i64 = 7_800 * MS;
    const RESTORE_AT: TimeUs = TimeUs(8_000 * MS);
    /// Steps before the round opens.
    const WARM: usize = 3;

    fn bank() -> Arc<SoundBank> {
        let ms = RATE as usize / 1_000;
        Arc::new(SoundBank::from_sounds(
            RATE,
            vec![
                Sound::mono(
                    (0..4 * RATE as usize)
                        .map(|k| i16::try_from(k / ms + 1).unwrap())
                        .collect(),
                ),
                Sound::mono(vec![2_000; 480]),
                Sound::mono(vec![3_000; 480]),
            ],
        ))
    }

    fn schedule() -> Vec<BgmEvent> {
        [(5_000, 0), (8_050, 1)]
            .map(|(t, s)| BgmEvent {
                time: TimeUs(t * MS),
                sound: SoundId(s),
            })
            .to_vec()
    }

    fn engine(bank: Arc<SoundBank>) -> AudioEngine {
        AudioEngine::with_schedule(bank, schedule(), RATE, PERIOD, MAX_VOICES, 0).unwrap()
    }

    fn output() -> RecordingOutput<SimDevice> {
        RecordingOutput::new(
            SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
            2 * RATE as usize,
        )
    }

    fn rig() -> (AudioControl, AudioWorker, RecordingOutput<SimDevice>) {
        let (control, worker) = audio_control(engine(bank()));
        (control, worker, output())
    }

    /// The frame output time `us` falls in: frame `n` plays at
    /// `(n + 480) / RATE`.
    fn frame(us: i64) -> usize {
        usize::try_from((us - 10 * MS) * 48 / 1_000).unwrap()
    }

    /// The mono level of frame `f`, at `gain`.
    fn level(out: &RecordingOutput<SimDevice>, f: usize, gain: f32) -> f32 {
        out.samples()[2 * f] / gain * 32_768.0
    }

    /// The origin of a round opening when output time `opened` is due: its
    /// chart time 7.8 s there.
    fn origin(opened: i64) -> TimeUs {
        TimeUs(opened - OPENS_AT)
    }

    /// What a reference engine plays when, after `before` steps, it starts
    /// a play at `origin` and `gain` and seeks to `at` there, for `steps`
    /// steps more; and its frame line after the restart.
    fn reference(
        before: usize,
        origin: TimeUs,
        gain: Gain,
        at: Option<TimeUs>,
        steps: usize,
    ) -> (Vec<f32>, audio::FrameLine) {
        let mut e = engine(bank());
        e.stop();
        let mut out = output();
        let (_tx, mut rx) = rtrb::RingBuffer::new(1);
        for _ in 0..before {
            e.step(&mut out, &mut rx).unwrap();
        }
        e.set_gain(gain.get()).unwrap();
        e.restart(origin);
        let line = e.frame_line();
        if let Some(at) = at {
            let plan = SeekPlan::new(&e.seek_source(), &schedule(), line, at).unwrap();
            e.seek(&plan).unwrap();
        }
        for _ in 0..steps {
            e.step(&mut out, &mut rx).unwrap();
        }
        (out.samples().to_vec(), line)
    }

    fn steps(worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>, n: usize) {
        for _ in 0..n {
            worker.step(out).unwrap();
        }
    }

    #[test]
    fn play_restores_at_its_committed_origin() {
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        // The opening at 30 ms fixes the origin; the restore is planned on
        // its line for the lead-in's start, 200 ms later.
        let origin = origin(30 * MS);
        let line = control.counters().frame_line(origin);
        let restore = Restorer::new(control.seek_source(), schedule())
            .plan(line, RESTORE_AT)
            .unwrap();
        let (_sink, queue) = audio_link_at(4, origin);
        control
            .play_at(origin, queue, Gain::HEADROOM, Some(restore))
            .unwrap();
        steps(&mut worker, &mut out, 60);
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(Restored::First)
        );
        // The published line is the engine's after the restart, and the
        // output is a reference engine's that restarted and sought there.
        let (want, want_line) = reference(WARM, origin, Gain::HEADROOM, Some(RESTORE_AT), 60);
        assert_eq!(line, want_line);
        assert_eq!(out.samples(), &want[..]);
        // Silent until the lead-in's start at 230 ms, where the long voice
        // enters 3 s in; the BGM event 50 ms into the lead-in sounds over it.
        let at = frame(230 * MS);
        assert!(out.samples()[..2 * at].iter().all(|&x| x == 0.0));
        assert_eq!(level(&out, at, audio::HEADROOM), 3_001.0);
        let bgm = frame(280 * MS);
        assert_eq!(level(&out, bgm - 1, audio::HEADROOM), 3_050.0);
        assert_eq!(level(&out, bgm, audio::HEADROOM), 3_051.0 + 2_000.0);
    }

    /// Steps the worker until it has written past `us`'s frame: the
    /// steps taken.
    fn past(worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>, us: i64) -> usize {
        let mut n = 0;
        while worker.engine().written() <= frame(us) as u64 + u64::from(PERIOD) {
            worker.step(out).unwrap();
            n += 1;
            assert!(n < 200);
        }
        n
    }

    #[test]
    fn a_missed_origin_falls_back() {
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let origin = origin(30 * MS);
        let restore = Restorer::new(control.seek_source(), schedule())
            .plan(control.counters().frame_line(origin), RESTORE_AT)
            .unwrap();
        // The game thread was slow: the output passed the lead-in's start
        // before the start reached the audio thread.
        let late = WARM + past(&mut worker, &mut out, 230 * MS);
        let (_sink, queue) = audio_link_at(4, origin);
        control
            .play_at(origin, queue, Gain::HEADROOM, Some(restore))
            .unwrap();
        steps(&mut worker, &mut out, 60);
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(Restored::Fallback)
        );
        let fallback = TimeUs(RESTORE_AT.0 + RESTORE_RETRY.0);
        let (want, _) = reference(late, origin, Gain::HEADROOM, Some(fallback), 60);
        assert_eq!(out.samples(), &want[..]);
        // Silent until 500 ms later, where the long voice enters 3.5 s in.
        let at = frame(730 * MS);
        assert!(out.samples()[..2 * at].iter().all(|&x| x == 0.0));
        assert_eq!(level(&out, at, audio::HEADROOM), 3_501.0);
    }

    #[test]
    fn a_refused_restore_plays_silent() {
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let origin = origin(30 * MS);
        let restore = Restorer::new(control.seek_source(), schedule())
            .plan(control.counters().frame_line(origin), RESTORE_AT)
            .unwrap();
        // Both plans are late.
        past(&mut worker, &mut out, 730 * MS);
        let (mut sink, queue) = audio_link_at(4, origin);
        control
            .play_at(origin, queue, Gain::HEADROOM, Some(restore))
            .unwrap();
        // A key sound is still heard, and the ring-out still ends.
        let now = TimeUs(i64::try_from(worker.engine().written()).unwrap() * 1_000 / 48);
        sink.receive(&eval::JudgeEvent {
            seq: 0,
            time: TimeUs(0),
            origin: Some(now),
            kind: eval::JudgeEventKind::PlaySound {
                lane: lane(1),
                sound: SoundId(2),
            },
        });
        let end = TimeUs(now.0 - origin.0 + 200 * MS);
        control.ring_out(TimeUs(end.0 - 50 * MS), end);
        let mut n = 0;
        while !control.done() {
            worker.step(&mut out).unwrap();
            n += 1;
            assert!(n < 100, "never done");
        }
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(Restored::Refused(RestoreRefusal::Late))
        );
        // No BGM started, not from chart time 0 nor later: only the key
        // sound's 480 frames sounded.
        assert_eq!(worker.engine().bgm_left(), 0);
        let heard: Vec<f32> = out
            .samples()
            .iter()
            .step_by(2)
            .copied()
            .filter(|&x| x != 0.0)
            .collect();
        assert_eq!(heard, vec![3_000.0 / 65_536.0; 480]);
    }

    #[test]
    fn load_answer_carries_the_seek_source() {
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let stale = control.seek_source();
        // The same sounds and schedule loaded again: another chart for the
        // engine, so the source taken before is out of date.
        std::thread::scope(|s| {
            let load = s.spawn(|| control.load(bank(), audio::BgmSchedule::new(schedule())));
            while !load.is_finished() {
                worker.step(&mut out).unwrap();
            }
            load.join().unwrap().unwrap();
        });
        let fresh = control.seek_source();
        assert!(fresh.matches(&schedule()));
        for (source, want) in [
            (stale, Restored::Refused(RestoreRefusal::Source)),
            (fresh, Restored::First),
        ] {
            let opened = i64::try_from(worker.engine().written()).unwrap() * 1_000 / 48;
            let origin = origin(opened);
            let restore = Restorer::new(source, schedule())
                .plan(control.counters().frame_line(origin), RESTORE_AT)
                .unwrap();
            let (_sink, queue) = audio_link_at(4, origin);
            control
                .play_at(origin, queue, Gain::HEADROOM, Some(restore))
                .unwrap();
            steps(&mut worker, &mut out, 2);
            assert_eq!(
                control.counters().restored(control.generation()),
                Some(want)
            );
        }
    }

    #[test]
    fn restore_keeps_the_plays_gain_across_rounds() {
        // Two rounds at gain 1 (not the headroom), the first rung out to
        // its end: each restores its long voice at full level.
        let unity = Gain::new(1.0).unwrap();
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let mut opened = 30 * MS;
        for round in 0..2 {
            let origin = origin(opened);
            let restore = Restorer::new(control.seek_source(), schedule())
                .plan(control.counters().frame_line(origin), RESTORE_AT)
                .unwrap();
            let (_sink, queue) = audio_link_at(4, origin);
            control
                .play_at(origin, queue, unity, Some(restore))
                .unwrap();
            steps(&mut worker, &mut out, 30);
            assert_eq!(
                control.counters().restored(control.generation()),
                Some(Restored::First),
                "round {round}"
            );
            let at = frame(opened + 200 * MS);
            assert_eq!(out.samples()[2 * at - 2], 0.0, "round {round}");
            assert_eq!(level(&out, at, 1.0), 3_001.0, "round {round}");
            // Round 1 rings out 60 ms past its lead-in's start.
            control.ring_out(TimeUs(8_040 * MS), TimeUs(8_060 * MS));
            while !control.done() {
                worker.step(&mut out).unwrap();
            }
            opened = i64::try_from(worker.engine().written()).unwrap() * 1_000 / 48 + 10 * MS;
        }
    }
}
