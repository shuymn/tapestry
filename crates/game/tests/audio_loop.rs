//! The audio thread's loop and its control, stepped on one thread over a
//! simulated device: silence until the first play, and the sealed play's
//! audio reported done only once its sounds have ended.

mod common;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice};
use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, bank, lane};
use eval::{JudgeEvent, JudgeEventKind};
use game::{AudioControl, AudioWorker, Channel, EventSink, audio_control, audio_link_at};

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

/// The key volume set from the game thread scales what the audio thread
/// mixes of its channel from its next step on, during a play: the BGM
/// sound at unity whatever the key volume, then key sounds at one half,
/// at zero and at one quarter. A volume outside 0 to the mixer's largest
/// is refused on every channel and changes nothing.
#[test]
fn volume_change_scales_mixed_samples() {
    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    let (mut sink, queue) = audio_link_at(8, TimeUs(50 * MS));
    control.play(TimeUs(50 * MS), queue).unwrap();
    // The key volume does not reach the BGM sound.
    control.set_volume(Channel::Key, 0.2).unwrap();
    let steps = |worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>| {
        for _ in 0..15 {
            worker.step(out).unwrap();
        }
    };
    steps(&mut worker, &mut out);
    for (volume, pressed) in [(0.5, 150), (0.0, 300), (0.25, 450)] {
        control.set_volume(Channel::Key, volume).unwrap();
        if volume == 0.25 {
            for refused in [-0.01, audio::MAX_VOLUME * 2.0, f32::NAN] {
                for channel in Channel::ALL {
                    assert!(
                        control.set_volume(channel, refused).is_err(),
                        "{channel:?} {refused}"
                    );
                }
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
    // The key and background volumes halve mid-play; the gain stays.
    control.set_volume(Channel::Key, 0.5).unwrap();
    control.set_volume(Channel::Background, 0.5).unwrap();
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
    let schedule = vec![audio::ScheduledSound::from(bms_model::BgmEvent {
        time: TimeUs(0),
        sound: SoundId(0),
    })];
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
        AudioEngine, Gain, MAX_VOICES, RecordingOutput, ScheduledSound, SeekPlan, SimConfig,
        SimDevice, Sound, SoundBank,
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

    fn schedule() -> Vec<ScheduledSound> {
        [(5_000, 0), (8_050, 1)]
            .map(|(t, s)| {
                ScheduledSound::from(BgmEvent {
                    time: TimeUs(t * MS),
                    sound: SoundId(s),
                })
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
        // A full play after it restores nothing, and the round's box has
        // come back to this thread with its queue.
        let (_sink, queue) = audio_link_at(4, TimeUs(0));
        control.play(TimeUs(0), queue).unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(control.counters().restored(control.generation()), None);
        control.ready().unwrap();
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
    fn a_late_load_answer_carries_the_seek_source() {
        // A load the audio thread takes only after it timed out: its answer
        // is drained by the next load, which times out too, and the source
        // is then the late load's, what the engine plays.
        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let late = vec![ScheduledSound::from(BgmEvent {
            time: TimeUs(6_000 * MS),
            sound: SoundId(0),
        })];
        let timeout = Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout));
        assert_eq!(
            control.load(bank(), audio::BgmSchedule::new(late.clone())),
            timeout
        );
        steps(&mut worker, &mut out, 1);
        assert_eq!(
            control.load(bank(), audio::BgmSchedule::new(Vec::new())),
            timeout
        );
        assert!(control.seek_source().matches(&late));
        assert!(!control.seek_source().matches(&schedule()));
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

    /// A gated round cancelled before its lead-in's start and rearmed
    /// (design.md "Play Lifecycle", the start gate): the first candidate's
    /// restore never runs, the rearm's restore, planned again in the box
    /// the game thread keeps, lands at the new lead-in's start, and the
    /// box it replaced comes back for the next rearm. Through
    /// `AudioWorker::step` and `AudioCtl::Rearm`.
    #[test]
    fn a_rearmed_round_restores_at_its_own_lead_in_start() {
        use game::TransportGate;
        use input::{GatePhase, StartGate};

        let (mut control, mut worker, mut out) = rig();
        steps(&mut worker, &mut out, WARM);
        let gate = StartGate::new();
        let restorer = Restorer::new(control.seek_source(), schedule());
        let transport = |generation| TransportGate {
            gate: gate.clone(),
            generation,
            commit_at: RESTORE_AT,
        };
        // Candidate 1 opens at 30 ms and is cancelled at once.
        let first = origin(30 * MS);
        let plan = restorer
            .plan(control.counters().frame_line(first), RESTORE_AT)
            .unwrap();
        let mut spare = plan.clone();
        gate.arm(1);
        let (_sink, queue) = audio_link_at(4, first);
        control
            .play_gated(first, queue, Gain::HEADROOM, Some(plan), transport(1))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(gate.cancel(), Some(1));
        control.cancel();
        steps(&mut worker, &mut out, 1);
        assert!(control.counters().dropped(1));
        // Candidate 2 opens at 150 ms, its restore planned in the spare box.
        let second = origin(150 * MS);
        restorer
            .replan(
                &mut spare,
                control.counters().frame_line(second),
                RESTORE_AT,
            )
            .unwrap();
        gate.arm(2);
        control.rearm(second, transport(2), Some(spare));
        steps(&mut worker, &mut out, 40);
        assert_eq!(gate.state().phase, GatePhase::Committed);
        assert_eq!(
            control.counters().restored(control.generation()),
            Some(Restored::First)
        );
        // Silent until the second lead-in's start at 350 ms, where the long
        // voice enters 3 s in.
        let at = frame(350 * MS);
        assert!(out.samples()[..2 * at].iter().all(|&x| x == 0.0));
        assert_eq!(level(&out, at, audio::HEADROOM), 3_001.0);
        assert!(control.take_restore().is_some(), "the replaced box is back");
    }
}

/// A READY candidate's transport (design.md "Play Lifecycle", the start
/// gate) through `AudioWorker::step`: committed by generation before the
/// pass reaching chart time 0, where the BGM starts; a candidate the
/// input side cancelled first, or an authorization of another generation,
/// starts nothing, its drop acknowledged; a rearmed candidate's own
/// generation commits on the same queue.
#[test]
fn a_gated_transport_commits_only_its_own_armed_candidate() {
    use game::TransportGate;
    use input::{GatePhase, StartGate};

    let chart = chart();
    let transport = |gate: &StartGate, generation| TransportGate {
        gate: gate.clone(),
        generation,
        commit_at: TimeUs(0),
    };
    let sounding = |out: &RecordingOutput<SimDevice>| out.samples().iter().any(|&s| s != 0.0);

    // Committed: the BGM at chart time 0 plays.
    let (mut control, mut worker, mut out) = rig(&chart);
    let gate = StartGate::new();
    gate.arm(1);
    let (_sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control
        .play_gated(
            TimeUs(50 * MS),
            queue,
            audio::Gain::HEADROOM,
            None,
            transport(&gate, 1),
        )
        .unwrap();
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    assert_eq!(gate.state().phase, GatePhase::Committed);
    assert!(sounding(&out));
    assert!(!control.counters().dropped(1));

    // Cancelled by a hold before the commit: nothing sounds, the drop is
    // acknowledged; the rearmed candidate 2 commits and plays.
    let (mut control, mut worker, mut out) = rig(&chart);
    let gate = StartGate::new();
    gate.arm(1);
    let (_sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control
        .play_gated(
            TimeUs(50 * MS),
            queue,
            audio::Gain::HEADROOM,
            None,
            transport(&gate, 1),
        )
        .unwrap();
    worker.step(&mut out).unwrap();
    assert_eq!(gate.cancel(), Some(1));
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    assert!(
        !sounding(&out),
        "the cancelled candidate's chart start never ran"
    );
    assert!(control.counters().dropped(1));
    gate.arm(2);
    control.rearm(TimeUs(400 * MS), transport(&gate, 2), None);
    for _ in 0..30 {
        worker.step(&mut out).unwrap();
    }
    assert_eq!(gate.state().phase, GatePhase::Committed);
    assert!(sounding(&out));

    // A stale authorization: the gate armed for 3, the play's for 2.
    let (mut control, mut worker, mut out) = rig(&chart);
    let gate = StartGate::new();
    gate.arm(3);
    let (_sink, queue) = audio_link_at(4, TimeUs(50 * MS));
    control
        .play_gated(
            TimeUs(50 * MS),
            queue,
            audio::Gain::HEADROOM,
            None,
            transport(&gate, 2),
        )
        .unwrap();
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    assert!(!sounding(&out), "a stale authorization starts nothing");
    assert_eq!(gate.state().phase, GatePhase::Armed);
    assert!(control.counters().dropped(2));
}

/// The transport is arbitrated against the wake that writes it: a cold
/// output's first pass writes two periods at once, and a pass after an
/// underrun refills the queue, re-anchoring the clock; chart time 0
/// inside either block is admitted only by that pass, so a candidate
/// cancelled before it never sounds, and one armed commits there. Drives
/// `AudioWorker::step`, whose engine holds the transport
/// (`AudioEngine::step_admitting`).
#[test]
fn the_transport_is_admitted_by_the_pass_that_writes_it_cold_or_after_an_underrun() {
    use game::TransportGate;
    use input::{GatePhase, StartGate};

    let chart = chart();
    let sounding = |out: &RecordingOutput<SimDevice>| out.samples().iter().any(|&s| s != 0.0);
    // (the device, chart time 0 on the output, steps before the play)
    let cold = SimConfig::steady(RATE, PERIOD, TimeUs(0));
    let mut underrun = SimConfig::steady(RATE, PERIOD, TimeUs(0));
    underrun.missed_wakes = vec![5];
    for (config, origin, before) in [(cold, 15 * MS, 0), (underrun, 75 * MS, 4)] {
        for cancelled in [true, false] {
            let engine =
                AudioEngine::new(bank(&chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, 0).unwrap();
            let (mut control, mut worker) = audio_control(engine);
            let mut out = RecordingOutput::new(SimDevice::new(config.clone()), RATE as usize);
            for _ in 0..before {
                worker.step(&mut out).unwrap();
            }
            let gate = StartGate::new();
            gate.arm(1);
            let (_sink, queue) = audio_link_at(4, TimeUs(origin));
            let transport = TransportGate {
                gate: gate.clone(),
                generation: 1,
                commit_at: TimeUs(0),
            };
            control
                .play_gated(
                    TimeUs(origin),
                    queue,
                    audio::Gain::HEADROOM,
                    None,
                    transport,
                )
                .unwrap();
            if cancelled {
                assert_eq!(gate.cancel(), Some(1));
            }
            for _ in 0..20 {
                worker.step(&mut out).unwrap();
            }
            assert_eq!(
                sounding(&out),
                !cancelled,
                "origin {origin}, cancelled {cancelled}"
            );
            assert_eq!(control.counters().dropped(1), cancelled);
            let phase = if cancelled {
                GatePhase::Cancelled
            } else {
                GatePhase::Committed
            };
            assert_eq!(gate.state().phase, phase);
        }
    }
}

/// Candidate numbers are unique over the gate's life, so a drop the worker
/// acknowledged for an earlier attempt never stands for this one: two
/// attempts on one worker, the first's candidate dropped.
#[test]
fn a_drop_acknowledgement_belongs_to_its_own_attempt() {
    use game::TransportGate;
    use input::StartGate;

    let chart = chart();
    let (mut control, mut worker, mut out) = rig(&chart);
    let gate = StartGate::new();
    let play = |control: &mut AudioControl, generation, origin| {
        gate.arm(generation);
        let (_sink, queue) = audio_link_at(4, TimeUs(origin));
        let transport = TransportGate {
            gate: gate.clone(),
            generation,
            commit_at: TimeUs(0),
        };
        control
            .play_gated(
                TimeUs(origin),
                queue,
                audio::Gain::HEADROOM,
                None,
                transport,
            )
            .unwrap();
    };
    // The first attempt's candidate is cancelled and dropped.
    play(&mut control, 1, 100 * MS);
    gate.cancel();
    for _ in 0..20 {
        worker.step(&mut out).unwrap();
    }
    assert!(control.counters().dropped(1));
    gate.disarm();
    // The next attempt numbers after the gate's last: its candidate is
    // not acknowledged as dropped by the first's drop.
    let next = gate.state().generation + 1;
    assert_eq!(next, 2);
    play(&mut control, next, 400 * MS);
    worker.step(&mut out).unwrap();
    assert!(!control.counters().dropped(next));
}

/// An engine on `chart`'s bank, the bank's `Arc` kept by the caller, and
/// its control and worker over a recorded simulated device.
fn rig_holding(
    chart: &Chart,
) -> (
    std::sync::Arc<audio::SoundBank>,
    AudioControl,
    AudioWorker,
    RecordingOutput<SimDevice>,
) {
    let held = bank(chart);
    let engine = AudioEngine::new(
        std::sync::Arc::clone(&held),
        &chart.bgm,
        RATE,
        PERIOD,
        MAX_VOICES,
        0,
    )
    .unwrap();
    let (control, worker) = audio_control(engine);
    let out = RecordingOutput::new(
        SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        RATE as usize * 4,
    );
    (held, control, worker, out)
}

/// Runs `call` on another thread while the worker is stepped here, until it
/// returns: a load or an unload the audio thread answers.
fn answered<T: Send>(
    worker: &mut AudioWorker,
    out: &mut RecordingOutput<SimDevice>,
    call: impl FnOnce() -> T + Send,
) -> T {
    std::thread::scope(|s| {
        let call = s.spawn(call);
        while !call.is_finished() {
            worker.step(out).unwrap();
        }
        call.join().unwrap()
    })
}

/// U1: an unload swaps an empty bank in, and the installed bank, with the
/// source the last load answered with, is freed by the unloading thread.
#[test]
fn an_unload_frees_the_installed_bank_on_the_game_thread() {
    let chart = chart();
    let (held, mut control, mut worker, mut out) = rig_holding(&chart);
    let weak = std::sync::Arc::downgrade(&held);
    drop(held);
    assert!(
        weak.upgrade().is_some(),
        "the engine and the source hold it"
    );
    answered(&mut worker, &mut out, || control.unload()).unwrap();
    assert_eq!(worker.engine().bank().stored_count(), 0);
    assert!(weak.upgrade().is_none(), "the installed bank was kept");
}

/// U2, U4: an unload the stalled audio thread does not answer times out
/// and stays outstanding: the next is refused (`Busy`) and sends nothing.
#[test]
fn an_unload_timeout_leaves_it_outstanding() {
    let chart = chart();
    let (_held, mut control, _worker, _out) = rig_holding(&chart);
    assert_eq!(
        control.unload(),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    assert_eq!(control.unload(), Err(game::AudioLoadError::Busy));
}

/// U3: the answer to an unload that timed out comes back late, holding the
/// bank it replaced; the next unload frees it first, then goes through.
#[test]
fn a_late_unload_answer_is_freed_by_the_next_unload() {
    let chart = chart();
    let (held, mut control, mut worker, mut out) = rig_holding(&chart);
    let weak = std::sync::Arc::downgrade(&held);
    drop(held);
    assert_eq!(
        control.unload(),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    // The stalled thread recovers and answers, late.
    worker.step(&mut out).unwrap();
    assert!(weak.upgrade().is_some(), "the late answer holds it");
    answered(&mut worker, &mut out, || control.unload()).unwrap();
    assert!(weak.upgrade().is_none(), "the late answer was kept");
}

/// B11: an install that timed out takes effect late; the next unload frees
/// that late answer, the bank it replaced and the source of the one it
/// installed with it, then takes the installed one away too.
#[test]
fn an_install_timeout_is_freed_by_the_next_unload() {
    let chart = chart();
    let (held, mut control, mut worker, mut out) = rig_holding(&chart);
    let old = std::sync::Arc::downgrade(&held);
    drop(held);
    let new = bank(&chart);
    let installed = std::sync::Arc::downgrade(&new);
    assert_eq!(
        control.load(
            new,
            audio::BgmSchedule::new(chart.bgm.iter().map(|&e| e.into()).collect())
        ),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    worker.step(&mut out).unwrap();
    assert!(installed.upgrade().is_some(), "taken late");
    answered(&mut worker, &mut out, || control.unload()).unwrap();
    assert!(old.upgrade().is_none(), "the bank the install replaced");
    assert!(installed.upgrade().is_none(), "the bank installed late");
}

/// U5: once the audio thread is gone, an unload is refused as gone.
#[test]
fn an_unload_after_the_worker_is_gone_is_gone() {
    let chart = chart();
    let (_held, mut control, worker, _out) = rig_holding(&chart);
    drop(worker);
    assert_eq!(
        control.unload(),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Gone))
    );
}

/// V1, through the control: the decision tone started before an unload
/// sounds on in the step that takes it. The unload is sent while the
/// worker is not stepped (it times out, still queued), so the tone has
/// sounded one step when the unload lands, whatever the scheduling.
#[test]
fn the_decision_tone_survives_an_unload() {
    let chart = chart();
    let (_held, mut control, mut worker, mut out) = rig_holding(&chart);
    control.play_tone();
    worker.step(&mut out).unwrap();
    assert!(worker.engine().tone_sounding());
    assert_eq!(
        control.unload(),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    let from = out.samples().len();
    worker.step(&mut out).unwrap();
    assert_eq!(worker.engine().bank().stored_count(), 0, "the unload taken");
    assert!(worker.engine().tone_sounding());
    assert!(out.samples()[from..].iter().any(|&x| x != 0.0));
}

/// V5: with a load pending (sent, timed out, still in the control queue),
/// the decision tone asked for starts in the very step that takes the load:
/// it does not wait behind the queue.
#[test]
fn the_decision_tone_starts_with_a_load_pending() {
    let chart = chart();
    let (_held, mut control, mut worker, mut out) = rig_holding(&chart);
    assert_eq!(
        control.unload(),
        Err(game::AudioLoadError::Wait(game::AudioWaitError::Timeout))
    );
    control.play_tone();
    let from = out.samples().len();
    worker.step(&mut out).unwrap();
    assert_eq!(worker.engine().bank().stored_count(), 0, "the load taken");
    assert!(worker.engine().tone_sounding());
    assert!(out.samples()[from..].iter().any(|&x| x != 0.0));
}

/// C1: a switch sent and left unanswered (the worker held) leaves its load
/// outstanding; once the worker answers it, late, the next switch takes
/// that answer first, so it is not `Busy`, then goes through: the engine
/// and the seek source hold the second bank, and the first switch's bank
/// is freed on this thread.
#[test]
fn a_left_switch_is_drained_and_replaced() {
    use std::sync::Arc;

    let chart = chart();
    let (_held, mut control, mut worker, mut out) = rig_holding(&chart);
    let first = bank(&chart);
    let left = Arc::downgrade(&first);
    let mut switch = control
        .switch(first, audio::BgmSchedule::new(Vec::new()))
        .unwrap();
    assert_eq!(control.poll_switch(&mut switch), game::SwitchPoll::Pending);
    assert!(switch.sent());
    // Left: its caller goes, the load still queued.
    drop(switch);
    worker.step(&mut out).unwrap();
    assert!(left.upgrade().is_some(), "taken late: the engine holds it");
    let second = bank(&chart);
    let mut next = control
        .switch(
            Arc::clone(&second),
            audio::BgmSchedule::new(chart.bgm.iter().map(|&e| e.into()).collect()),
        )
        .unwrap();
    let done = loop {
        if let game::SwitchPoll::Done(done) = control.poll_switch(&mut next) {
            break done;
        }
        assert!(
            control.expired(&next).is_none(),
            "the next switch waited out"
        );
        worker.step(&mut out).unwrap();
    };
    assert_eq!(done, Ok(()));
    assert!(Arc::ptr_eq(worker.engine().bank(), &second));
    assert!(
        control
            .seek_source()
            .matches(&chart.bgm.iter().map(|&e| e.into()).collect::<Vec<_>>())
    );
    assert!(left.upgrade().is_none(), "the late answer was kept");
}

/// C4: a switch whose push finds the control queue full keeps its bank and
/// schedule, unsent, with nothing outstanding; once there is room the
/// original bank and schedule are installed. Kept full to its bound, it
/// ends in `Timeout` with nothing outstanding, and the next load goes
/// through.
#[test]
fn a_full_control_queue_keeps_the_switch_unsent() {
    use std::sync::Arc;

    let chart = chart();
    let (_held, mut control, mut worker, mut out) = rig_holding(&chart);
    let fill = |control: &mut AudioControl| {
        // The queue's capacity, from the game crate's control queue.
        for _ in 0..16 {
            control.cancel();
        }
    };
    fill(&mut control);
    let original = bank(&chart);
    let mut switch = control
        .switch(
            Arc::clone(&original),
            audio::BgmSchedule::new(chart.bgm.iter().map(|&e| e.into()).collect()),
        )
        .unwrap();
    for _ in 0..3 {
        assert_eq!(control.poll_switch(&mut switch), game::SwitchPoll::Pending);
        assert!(!switch.sent(), "full: nothing sent");
    }
    // Room: the worker drains the queue.
    worker.step(&mut out).unwrap();
    let done = loop {
        if let game::SwitchPoll::Done(done) = control.poll_switch(&mut switch) {
            break done;
        }
        assert!(
            control.expired(&switch).is_none(),
            "the original never sent"
        );
        worker.step(&mut out).unwrap();
    };
    assert_eq!(done, Ok(()));
    assert!(Arc::ptr_eq(worker.engine().bank(), &original));
    assert!(
        control
            .seek_source()
            .matches(&chart.bgm.iter().map(|&e| e.into()).collect::<Vec<_>>())
    );
    // Kept full to the bound.
    fill(&mut control);
    let mut kept = control
        .switch(bank(&chart), audio::BgmSchedule::new(Vec::new()))
        .unwrap();
    let expired = loop {
        assert_eq!(control.poll_switch(&mut kept), game::SwitchPoll::Pending);
        if let Some(expired) = control.expired(&kept) {
            break expired;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert_eq!(
        expired,
        game::AudioLoadError::Wait(game::AudioWaitError::Timeout)
    );
    assert!(!kept.sent());
    drop(kept);
    // Nothing outstanding: the next load is not refused as busy.
    let next = bank(&chart);
    answered(&mut worker, &mut out, || {
        control.load(Arc::clone(&next), audio::BgmSchedule::new(Vec::new()))
    })
    .unwrap();
    assert!(Arc::ptr_eq(worker.engine().bank(), &next));
}

/// The system sounds' requests (design.md "Audio", system sounds): starts,
/// stops and installs from the game thread, numbered from one sequence,
/// taken by the audio thread's control step through the cutoff the game
/// thread publishes, and generations handed back through the return ring.
mod system_sounds {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use audio::{
        AudioEngine, AudioOutput, GenId, MAX_VOICES, OutputError, RecordingOutput, SimConfig,
        SimDevice, Sound, SoundBank, SystemGeneration, SystemKind, Wake,
    };
    use bms_model::TimeUs;
    use game::{AudioControl, AudioWorker, SystemStartError, audio_control};

    use super::common::{PERIOD, RATE};

    /// A silent chart's engine, its control and worker over a recorded
    /// simulated device.
    fn rig() -> (AudioControl, AudioWorker, RecordingOutput<SimDevice>) {
        let bank = Arc::new(SoundBank::from_sounds(RATE, Vec::new()));
        let engine = AudioEngine::new(bank, &[], RATE, PERIOD, MAX_VOICES, 0).unwrap();
        let (control, worker) = audio_control(engine);
        let out = RecordingOutput::new(
            SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0))),
            RATE as usize * 8,
        );
        (control, worker, out)
    }

    /// `frames` frames of the constant `level`.
    fn constant(level: i16, frames: usize) -> Arc<Sound> {
        Arc::new(Sound::mono(vec![level; frames]))
    }

    /// A generation under an id drawn now, holding `kinds`.
    fn pick(control: &AudioControl, kinds: &[(SystemKind, Arc<Sound>)]) -> SystemGeneration {
        let id = GenId(control.system_seq().unwrap());
        kinds
            .iter()
            .fold(SystemGeneration::new(id, RATE), |g, (k, s)| {
                g.with(*k, Arc::clone(s))
            })
    }

    /// Installs `generation` and returns its id.
    fn install(control: &mut AudioControl, generation: SystemGeneration) -> GenId {
        let id = generation.id();
        control.install_system(generation).unwrap();
        id
    }

    fn steps(worker: &mut AudioWorker, out: &mut RecordingOutput<SimDevice>, n: usize) {
        for _ in 0..n {
            worker.step(out).unwrap();
        }
    }

    /// The ids of the generations handed back so far, taken.
    fn returned(control: &mut AudioControl) -> Vec<GenId> {
        std::iter::from_fn(|| control.take_system_return().map(|g| g.id())).collect()
    }

    fn sounding(worker: &AudioWorker) -> usize {
        worker.engine().system().sounding()
    }

    /// S6: the empty install sends the current and the retired generation
    /// back in the step that takes it, each still holding a voice; and in
    /// an install held behind the taken retired slot the return ring takes
    /// all three it is sized for, which wait through 1 000 steps undrained
    /// without loss.
    #[test]
    fn the_return_ring_holds_the_latch() {
        let (mut control, mut worker, mut out) = rig();
        let long = constant(1_000, 10 * RATE as usize);
        let r = pick(&control, &[(SystemKind::Select, Arc::clone(&long))]);
        let r = install(&mut control, r);
        control
            .system_start(SystemKind::Select, true, Some(r))
            .unwrap();
        steps(&mut worker, &mut out, 2);
        let c = pick(&control, &[(SystemKind::Scratch, Arc::clone(&long))]);
        let c = install(&mut control, c);
        control
            .system_start(SystemKind::Scratch, false, None)
            .unwrap();
        steps(&mut worker, &mut out, 2);
        let system = worker.engine().system();
        assert_eq!((system.current(), system.retired()), (Some(c), Some(r)));
        assert_eq!(system.sounding(), 2);
        let empty = GenId(control.system_seq().unwrap());
        control
            .install_system(SystemGeneration::empty(empty))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(control.system_installed(), Some(empty));
        assert_eq!(sounding(&worker), 0);
        let mut back = returned(&mut control);
        back.sort();
        assert_eq!(back, [r, c]);

        // R retired with a voice, A current, B's install held behind R:
        // the stops raised to the empty install below it end R's voice, R
        // goes back at the sweep, and the next step sends A and B back.
        let (mut control, mut worker, mut out) = rig();
        let r = pick(&control, &[(SystemKind::Select, Arc::clone(&long))]);
        let r = install(&mut control, r);
        control
            .system_start(SystemKind::Select, true, Some(r))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        let a = pick(&control, &[(SystemKind::Scratch, Arc::clone(&long))]);
        let a = install(&mut control, a);
        steps(&mut worker, &mut out, 1);
        assert_eq!(worker.engine().system().retired(), Some(r));
        let b = pick(&control, &[(SystemKind::Scratch, Arc::clone(&long))]);
        let b = install(&mut control, b);
        let empty = GenId(control.system_seq().unwrap());
        for kind in SystemKind::ALL {
            control.system_stop(kind);
        }
        control
            .install_system(SystemGeneration::empty(empty))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(control.counters().system_install_waits(), 1);
        assert_eq!(control.system_installed(), Some(a));
        steps(&mut worker, &mut out, 1);
        assert_eq!(control.system_installed(), Some(empty));
        steps(&mut worker, &mut out, 1_000);
        assert_eq!(returned(&mut control), [r, a, b]);
    }

    /// S6: a voice of a generation replaced while it sounds plays on from
    /// the retired generation exactly as if nothing were installed, and
    /// the generation goes back in the step its last voice ends, not
    /// before.
    #[test]
    fn a_retired_generation_plays_out_and_returns() {
        let voice = Arc::new(Sound::mono(
            (0..RATE as usize / 2)
                .map(|i| i16::try_from(i % 4_000).unwrap() - 2_000)
                .collect(),
        ));
        let run = |replace: bool| {
            let (mut control, mut worker, mut out) = rig();
            let a = pick(&control, &[(SystemKind::Scratch, Arc::clone(&voice))]);
            let a = install(&mut control, a);
            control
                .system_start(SystemKind::Scratch, false, None)
                .unwrap();
            let from = out.samples().len();
            steps(&mut worker, &mut out, 3);
            let mut back_at = None;
            let mut ended_at = None;
            if replace {
                let b = pick(&control, &[(SystemKind::Select, constant(100, 480))]);
                install(&mut control, b);
            }
            for k in 0..80 {
                worker.step(&mut out).unwrap();
                let back = returned(&mut control);
                if replace && sounding(&worker) > 0 {
                    assert_eq!(worker.engine().system().retired(), Some(a));
                    assert!(back.is_empty(), "returned at step {k} while sounding");
                } else if !back.is_empty() {
                    assert_eq!(back, [a]);
                    back_at.get_or_insert(k);
                }
                if sounding(&worker) == 0 {
                    ended_at.get_or_insert(k);
                }
            }
            (out.samples()[from..].to_vec(), back_at, ended_at)
        };
        let (alone, never, _) = run(false);
        let (replaced, back_at, ended_at) = run(true);
        assert_eq!(never, None);
        // 0.5 s is 50 periods: it ends some 45 steps after the install,
        // and goes back in that step.
        assert!(ended_at.is_some_and(|k| k > 40), "{ended_at:?}");
        assert_eq!(back_at, ended_at);
        assert!(
            replaced
                .iter()
                .zip(&alone)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "the retired voice changed"
        );
    }

    /// S7: a stop is a word of its own, never queued behind starts: with
    /// the start ring full, a stop still ends its kind's loop in the
    /// retired generation at the next step, which then goes back; the
    /// start the full ring refused is counted, and a start after the stop
    /// plays.
    #[test]
    fn a_stop_behind_a_full_ring_wins() {
        let (mut control, mut worker, mut out) = rig();
        let long = constant(1_000, 10 * RATE as usize);
        let a = pick(&control, &[(SystemKind::Select, Arc::clone(&long))]);
        let a = install(&mut control, a);
        control
            .system_start(SystemKind::Select, true, Some(a))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        let b = pick(&control, &[(SystemKind::Select, Arc::clone(&long))]);
        let b = install(&mut control, b);
        steps(&mut worker, &mut out, 1);
        assert_eq!(worker.engine().system().retired(), Some(a));
        for _ in 0..audio::SYSTEM_STARTS {
            control
                .system_start(SystemKind::Scratch, false, None)
                .unwrap();
        }
        assert_eq!(
            control.system_start(SystemKind::Scratch, false, None),
            Err(SystemStartError::Full)
        );
        assert_eq!(control.counters().system_refused(), 1);
        control.system_stop(SystemKind::Select);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0, "the loop ended");
        assert_eq!(returned(&mut control), [a]);
        assert_eq!(control.system_installed(), Some(b));
        control
            .system_start(SystemKind::Select, false, None)
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 1, "a start after the stop plays");
        assert_eq!(control.counters().system_refused(), 1);
    }

    /// S5, S11, S13: the empty install alone, with no stop raised, is the
    /// latch: every set voice ends in both generations, a waiting loop and
    /// an older queued start are discarded, both generations go back, and
    /// the tone plays on; a decision sound after it is the tone.
    #[test]
    fn the_empty_install_is_the_latch() {
        let (mut control, mut worker, mut out) = rig();
        let long = constant(1_000, 10 * RATE as usize);
        let a = pick(&control, &[(SystemKind::Scratch, Arc::clone(&long))]);
        let a = install(&mut control, a);
        control
            .system_start(SystemKind::Scratch, false, None)
            .unwrap();
        steps(&mut worker, &mut out, 1);
        let kinds = [
            (SystemKind::Select, Arc::clone(&long)),
            (SystemKind::Fail, Arc::clone(&long)),
            (SystemKind::Decide, Arc::clone(&long)),
        ];
        let b = pick(&control, &kinds);
        let b = install(&mut control, b);
        control
            .system_start(SystemKind::Select, true, Some(b))
            .unwrap();
        let later = GenId(control.system_seq().unwrap());
        control
            .system_start(SystemKind::Fail, true, Some(later))
            .unwrap();
        control.play_tone();
        steps(&mut worker, &mut out, 1);
        let system = worker.engine().system();
        assert_eq!((system.current(), system.retired()), (Some(b), Some(a)));
        assert_eq!(system.sounding(), 2);
        assert!(system.pending().is_some());
        assert!(system.tone_sounding());
        control.system_start(SystemKind::Fail, false, None).unwrap();
        let empty = GenId(control.system_seq().unwrap());
        control
            .install_system(SystemGeneration::empty(empty))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        let system = worker.engine().system();
        assert_eq!(system.sounding(), 0, "every set voice ended");
        assert_eq!(system.pending(), None, "the waiting loop discarded");
        assert_eq!(system.retired(), None);
        assert!(system.tone_sounding(), "the tone plays on");
        let mut back = returned(&mut control);
        back.sort();
        assert_eq!(back, [a, b]);
        steps(&mut worker, &mut out, 70);
        assert!(!worker.engine().tone_sounding());
        control
            .system_start(SystemKind::Decide, false, None)
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert!(
            worker.engine().tone_sounding(),
            "the decision sound is the tone"
        );
        assert_eq!(sounding(&worker), 0);
    }

    /// S7, S9: a loop bound to a generation plays only from it: it waits
    /// for a later one and starts at its install, is discarded by a stop
    /// above it while it waits, by an install later than its own, and when
    /// its generation is already earlier than the current.
    #[test]
    fn a_loop_binds_to_its_generation() {
        let long = constant(1_000, 10 * RATE as usize);
        let kinds = [(SystemKind::Select, Arc::clone(&long))];
        let (mut control, mut worker, mut out) = rig();
        let a = pick(&control, &kinds);
        let a = install(&mut control, a);
        steps(&mut worker, &mut out, 1);
        // Waits for B, then starts at B's install.
        let b = pick(&control, &kinds);
        control
            .system_start(SystemKind::Select, true, Some(b.id()))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0);
        assert!(worker.engine().system().pending().is_some());
        let b = install(&mut control, b);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 1);
        assert_eq!(worker.engine().system().pending(), None);
        // A loop for an earlier generation: discarded.
        control
            .system_start(SystemKind::Select, true, Some(a))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 1);
        assert_eq!(worker.engine().system().pending(), None);
        control.system_stop(SystemKind::Select);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0);
        // A loop for C with a stop above it: nothing starts at C's install.
        let c = pick(&control, &kinds);
        control
            .system_start(SystemKind::Select, true, Some(c.id()))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        control.system_stop(SystemKind::Select);
        install(&mut control, c);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0);
        assert_eq!(worker.engine().system().pending(), None);
        // A loop for D when E, later than D, installs: discarded there.
        let d = GenId(control.system_seq().unwrap());
        control
            .system_start(SystemKind::Select, true, Some(d))
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(
            worker
                .engine()
                .system()
                .pending()
                .and_then(|p| p.generation),
            Some(d)
        );
        // A and B are back; taken, they make room for E (the fourth).
        assert_eq!(returned(&mut control), [a, b]);
        let e = pick(&control, &kinds);
        install(&mut control, e);
        steps(&mut worker, &mut out, 10);
        assert_eq!(sounding(&worker), 0);
        assert_eq!(worker.engine().system().pending(), None);
    }

    /// An output that fails every write once told to.
    struct Failing {
        inner: RecordingOutput<SimDevice>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl AudioOutput for Failing {
        fn rate(&self) -> u32 {
            self.inner.rate()
        }

        fn period(&self) -> u32 {
            self.inner.period()
        }

        fn wait(&mut self) -> Wake {
            self.inner.wait()
        }

        fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                return Err(OutputError::DeviceLost);
            }
            self.inner.write(f)
        }
    }

    /// Waits up to 5 s for `done`.
    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// S6, S14: at an output failure every system voice and the tone end;
    /// the control-only steps after it discard a waiting loop and every
    /// start, counted, but still apply installs and send a silent retired
    /// generation back; at the exit, the current generation and one still
    /// in the install ring drop on the audio thread after its loop has
    /// returned, outside any step.
    #[test]
    fn output_failure_and_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (mut control, mut worker, out) = rig();
        let fail = Arc::new(AtomicBool::new(false));
        let mut out = Failing {
            inner: out,
            fail: Arc::clone(&fail),
        };
        let long = constant(1_000, 10 * RATE as usize);
        let kinds = [
            (SystemKind::Select, Arc::clone(&long)),
            (SystemKind::Scratch, Arc::clone(&long)),
        ];
        let a = pick(&control, &kinds);
        let a = install(&mut control, a);
        control
            .system_start(SystemKind::Scratch, false, None)
            .unwrap();
        worker.step(&mut out).unwrap();
        let b = pick(&control, &kinds);
        let b = install(&mut control, b);
        control
            .system_start(SystemKind::Select, true, Some(b))
            .unwrap();
        control.play_tone();
        worker.step(&mut out).unwrap();
        let c_sound = constant(2_000, 480);
        let c = pick(&control, &[(SystemKind::Select, Arc::clone(&c_sound))]);
        control
            .system_start(SystemKind::Select, true, Some(c.id()))
            .unwrap();
        worker.step(&mut out).unwrap();
        let system = worker.engine().system();
        assert_eq!((system.current(), system.retired()), (Some(b), Some(a)));
        assert_eq!(system.sounding(), 2);
        assert!(system.tone_sounding());
        assert!(system.pending().is_some());
        let c_probe = Arc::downgrade(&c_sound);
        drop(c_sound);
        let d_sound = constant(3_000, 480);
        let d_probe = Arc::downgrade(&d_sound);
        let d = pick(&control, &[(SystemKind::Select, d_sound)]);

        fail.store(true, Ordering::Release);
        let (ran_tx, ran_rx) = std::sync::mpsc::channel();
        let (drop_tx, drop_rx) = std::sync::mpsc::channel::<()>();
        let audio = std::thread::spawn(move || {
            worker.run(&mut out, Duration::from_millis(1));
            let system = worker.engine().system();
            ran_tx
                .send((system.sounding(), system.tone_sounding()))
                .unwrap();
            drop_rx.recv().unwrap();
            let held = (c_probe.upgrade().is_some(), d_probe.upgrade().is_some());
            drop(worker);
            let dropped = (c_probe.upgrade().is_none(), d_probe.upgrade().is_none());
            (held, dropped, std::thread::current().id())
        });
        let counters = control.counters();
        until("the failure", || control.failure().is_some());
        // The waiting loop was discarded at the failure, counted.
        until("A's return", || {
            control.take_system_return().map(|g| g.id()) == Some(a)
        });
        for _ in 0..5 {
            control
                .system_start(SystemKind::Scratch, false, None)
                .unwrap();
        }
        until("the starts' discard", || counters.system_refused() == 6);
        let c = install(&mut control, c);
        control
            .system_start(SystemKind::Select, false, None)
            .unwrap();
        until("C's install", || control.system_installed() == Some(c));
        until("B's return", || {
            control.take_system_return().map(|g| g.id()) == Some(b)
        });
        until("the start's discard", || counters.system_refused() == 7);
        control.exit();
        let (sounding, tone) = ran_rx.recv().unwrap();
        assert_eq!((sounding, tone), (0, false));
        // The exit was taken: D stays in the install ring, and the control
        // goes, so the ring's last owner is the worker.
        install(&mut control, d);
        drop(control);
        drop_tx.send(()).unwrap();
        let (held, dropped, thread) = audio.join().unwrap();
        assert_eq!(held, (true, true), "C and D held until the worker drops");
        assert_eq!(dropped, (true, true), "C and D dropped with the worker");
        assert_ne!(thread, std::thread::current().id());
    }

    /// A decision sound of `level`.
    #[cfg(feature = "dev-tools")]
    fn decide(level: i16) -> Vec<(SystemKind, Arc<Sound>)> {
        vec![(SystemKind::Decide, constant(level, RATE as usize))]
    }

    /// The decision sound of `level` when `has`, else none.
    #[cfg(feature = "dev-tools")]
    fn decide_if(has: bool, level: i16) -> Vec<(SystemKind, Arc<Sound>)> {
        if has { decide(level) } else { Vec::new() }
    }

    /// The control shared with a snapshot hook.
    #[cfg(feature = "dev-tools")]
    type Shared = Arc<std::sync::Mutex<AudioControl>>;

    /// A hook that runs `at` once, at the first `point` reached, the game
    /// side's pushes made from a thread of their own.
    #[cfg(feature = "dev-tools")]
    fn once_at(
        worker: &mut AudioWorker,
        control: &Shared,
        point: game::SnapshotPoint,
        at: impl FnOnce(&mut AudioControl) + Send + 'static,
    ) {
        let control = Arc::clone(control);
        let mut at = Some(at);
        worker.set_snapshot_hook(move |reached| {
            if reached == point
                && let Some(at) = at.take()
            {
                let control = Arc::clone(&control);
                std::thread::spawn(move || at(&mut control.lock().unwrap()))
                    .join()
                    .unwrap();
            }
        });
    }

    /// S7: each step takes what its snapshot saw, and no more: starts and
    /// a stop published after it wait for the next step, which discards
    /// the starts older than the stop; a start taken a step after a stop
    /// numbered above it is discarded, a newer one plays. The sequence is
    /// checked and sticky: it issues numbers up to 2^63 - 1, refuses every
    /// draw from there without moving, and never wraps; a stop drawn then
    /// raises its kind's word to the terminal number, which ends its voices
    /// in every generation, and a start is refused and counted.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_snapshot_orders_and_bounds_each_step() {
        use game::{SEQ_LIMIT, SnapshotPoint, TERMINAL};

        let (control, mut worker, mut out) = rig();
        let control: Shared = Arc::new(std::sync::Mutex::new(control));
        let long = constant(1_000, 10 * RATE as usize);
        let kinds = [
            (SystemKind::Scratch, Arc::clone(&long)),
            (SystemKind::Fail, Arc::clone(&long)),
            (SystemKind::Select, Arc::clone(&long)),
        ];
        {
            let mut c = control.lock().unwrap();
            let a = pick(&c, &kinds);
            install(&mut c, a);
        }
        steps(&mut worker, &mut out, 1);
        for _ in 0..10 {
            control
                .lock()
                .unwrap()
                .system_start(SystemKind::Scratch, false, None)
                .unwrap();
        }
        once_at(&mut worker, &control, SnapshotPoint::AfterSnapshot, |c| {
            for _ in 0..20 {
                c.system_start(SystemKind::Scratch, false, None).unwrap();
            }
            c.system_stop(SystemKind::Scratch);
        });
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 10, "this step's ten, nothing after");
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0, "the stop, and the twenty below it");

        // A fail stop with nothing sounding, then an older fail start taken
        // at the next step: discarded; a newer one plays.
        once_at(&mut worker, &control, SnapshotPoint::AfterCutoff, |c| {
            c.system_start(SystemKind::Fail, false, None).unwrap();
            c.system_stop(SystemKind::Fail);
        });
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0, "the older start discarded");
        control
            .lock()
            .unwrap()
            .system_start(SystemKind::Fail, false, None)
            .unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 1, "a newer one plays");

        // Exhaustion.
        let c = control.lock().unwrap();
        c.seed_system_seq(SEQ_LIMIT - 1);
        assert_eq!(c.system_seq(), Some(SEQ_LIMIT - 1));
        assert_eq!(c.system_seq(), None);
        for _ in 0..1_000 {
            assert_eq!(c.system_seq(), None);
        }
        assert_eq!(c.system_seq_word(), SEQ_LIMIT, "no advance, no wrap");
        c.seed_system_seq(u64::MAX - 1);
        assert_eq!(c.system_seq(), None);
        assert_eq!(c.system_seq_word(), u64::MAX - 1);
        drop(c);

        // A scratch loop in the retired and in the current generation (one
        // kind sounding twice takes the one-shot voices), then a scratch
        // stop with the sequence exhausted.
        let (mut c, mut worker, mut out) = rig();
        let r = pick(&c, &kinds);
        let r = install(&mut c, r);
        c.system_start(SystemKind::Scratch, true, Some(r)).unwrap();
        steps(&mut worker, &mut out, 1);
        let a = pick(&c, &kinds);
        let a = install(&mut c, a);
        c.system_start(SystemKind::Scratch, true, Some(a)).unwrap();
        steps(&mut worker, &mut out, 1);
        assert_eq!(worker.engine().system().retired(), Some(r));
        assert_eq!(sounding(&worker), 2);
        c.seed_system_seq(SEQ_LIMIT);
        assert_eq!(c.system_stop(SystemKind::Scratch), None);
        assert_eq!(c.system_stop_word(SystemKind::Scratch), TERMINAL);
        steps(&mut worker, &mut out, 1);
        assert_eq!(sounding(&worker), 0);
        assert_eq!(returned(&mut c), [r]);
        assert_eq!(
            c.system_start(SystemKind::Scratch, false, None),
            Err(SystemStartError::Exhausted)
        );
        assert_eq!(c.counters().system_refused(), 1);
        // A decision sound's refusal is its caller's to turn into the tone.
        assert_eq!(
            c.system_start(SystemKind::Decide, false, None),
            Err(SystemStartError::Exhausted)
        );
        assert_eq!(c.counters().system_refused(), 1);
    }

    /// S7, S13: the cutoff orders the game thread's starts and installs
    /// across threads, whichever ring a push lands in between the
    /// snapshot's reads: nothing published after the cutoff is taken in
    /// that step, and the next applies it in number order, so a decision
    /// sound plays from the generation current at its number; the latch's
    /// stops and empty install discard a decision sound below them, and
    /// the tone asked for with them starts in the step that applies the
    /// empty install and not in the step before it. The order inside that
    /// step, the install and then the tone, is `SystemVoices::apply`'s,
    /// which no snapshot hook reaches; the audio crate's apply test checks
    /// that a tone behind a blocked install waits for it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_cutoff_orders_starts_and_installs_across_threads() {
        use game::SnapshotPoint;

        // A current, holding a decision sound when `a_has`.
        let setup = |a_has: bool| {
            let (control, mut worker, mut out) = rig();
            let control: Shared = Arc::new(std::sync::Mutex::new(control));
            let a = {
                let mut c = control.lock().unwrap();
                let a = pick(&c, &decide_if(a_has, 1_000));
                install(&mut c, a)
            };
            steps(&mut worker, &mut out, 1);
            (control, worker, out, a)
        };

        for a_has in [true, false] {
            let b_has = !a_has;
            // (a) install B then a decision sound, at the hook between the
            // ring reads, and (c) both at the hook after the cutoff: neither
            // taken; the next step installs B, then plays B's or the tone.
            for point in [SnapshotPoint::BetweenRings, SnapshotPoint::AfterCutoff] {
                let (control, mut worker, mut out, a) = setup(a_has);
                once_at(&mut worker, &control, point, move |c| {
                    let b = pick(c, &decide_if(b_has, 2_000));
                    install(c, b);
                    c.system_start(SystemKind::Decide, false, None).unwrap();
                });
                steps(&mut worker, &mut out, 1);
                let c = control.lock().unwrap();
                assert_eq!(c.system_installed(), Some(a), "{point:?}");
                drop(c);
                assert_eq!(sounding(&worker), 0);
                assert!(!worker.engine().tone_sounding());
                steps(&mut worker, &mut out, 1);
                assert_ne!(control.lock().unwrap().system_installed(), Some(a));
                assert_eq!(sounding(&worker), usize::from(b_has), "{point:?}");
                assert_eq!(worker.engine().tone_sounding(), !b_has, "{point:?}");
            }

            // (b) a decision sound then install B, between the ring reads:
            // at the next step, A's or the tone.
            let (control, mut worker, mut out, a) = setup(a_has);
            once_at(
                &mut worker,
                &control,
                SnapshotPoint::BetweenRings,
                move |c| {
                    c.system_start(SystemKind::Decide, false, None).unwrap();
                    let b = pick(c, &decide_if(b_has, 2_000));
                    install(c, b);
                },
            );
            steps(&mut worker, &mut out, 1);
            assert_eq!(sounding(&worker), 0);
            assert!(!worker.engine().tone_sounding());
            steps(&mut worker, &mut out, 1);
            assert_eq!(sounding(&worker), usize::from(a_has));
            assert_eq!(worker.engine().tone_sounding(), !a_has);
            if a_has {
                assert_eq!(worker.engine().system().retired(), Some(a));
            }
        }

        // (d) the latch: B and a decision sound after the cutoff, then the
        // stops, the empty install and the tone between the ring reads.
        let (control, mut worker, mut out, a) = setup(true);
        let shared = Arc::clone(&control);
        let ids = Arc::new(std::sync::Mutex::new(None));
        let ids_in = Arc::clone(&ids);
        let mut first = Some(move |c: &mut AudioControl| {
            let b = pick(c, &decide(2_000));
            let b = install(c, b);
            c.system_start(SystemKind::Decide, false, None).unwrap();
            *ids_in.lock().unwrap() = Some(b);
        });
        let mut second = Some(|c: &mut AudioControl| {
            let empty = GenId(c.system_seq().unwrap());
            for kind in SystemKind::ALL {
                c.system_stop(kind);
            }
            c.install_system(SystemGeneration::empty(empty)).unwrap();
            c.play_tone();
        });
        worker.set_snapshot_hook(move |point| {
            let run = |f: &mut (dyn FnMut(&mut AudioControl) + Send)| {
                let shared = Arc::clone(&shared);
                std::thread::scope(|s| {
                    s.spawn(|| f(&mut shared.lock().unwrap())).join().unwrap();
                });
            };
            match point {
                SnapshotPoint::AfterCutoff => {
                    if let Some(mut f) = first.take() {
                        run(&mut f);
                    }
                }
                SnapshotPoint::BetweenRings => {
                    if let Some(mut f) = second.take() {
                        run(&mut f);
                    }
                }
                SnapshotPoint::AfterSnapshot => {}
            }
        });
        steps(&mut worker, &mut out, 1);
        assert_eq!(control.lock().unwrap().system_installed(), Some(a));
        assert!(
            !worker.engine().tone_sounding(),
            "no tone before the empty install"
        );
        assert_eq!(sounding(&worker), 0);
        steps(&mut worker, &mut out, 1);
        let b = ids.lock().unwrap().unwrap();
        let mut c = control.lock().unwrap();
        let installed = c.system_installed().unwrap();
        assert!(installed > b, "the empty install is current");
        assert_eq!(sounding(&worker), 0, "the decision sound below the stops");
        assert!(
            worker.engine().tone_sounding(),
            "the tone, after the empty install"
        );
        assert_eq!(returned(&mut c), [a, b]);
    }
}
