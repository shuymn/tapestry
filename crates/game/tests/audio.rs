//! The runner's audio step: when key sounds and BGM reach the output, and
//! what an abort or the end of evaluation does to them. Expected frames are
//! worked out from the device's timing: it wakes every 10 ms (480 frames at
//! 48 kHz) from START, wake j writes from frame (j + 1) × 480, and frame f
//! plays at START + 10 ms + f / 48 kHz.

mod common;

use std::sync::Arc;

use audio::{AudioEngine, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{BgmEvent, Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, autoplay_script, control, device, lane};
use game::{AbortCause, LogicConfig, RunOutcome, Runner, RunnerAudio};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};

const START: TimeUs = TimeUs(-1_000_000);
const PERIOD_US: i64 = 10 * MS;

fn config() -> LogicConfig {
    LogicConfig {
        inputs: 512,
        effects: 512,
    }
}

/// Five sounded taps 300 ms apart from 1.047 s, sounds 0, 1, 2 in turn.
fn spaced_chart() -> Chart {
    let mut b = ChartBuilder::new(150_000).sound("a").sound("b").sound("c");
    for i in 0..5 {
        b = b.tap(lane(i + 1), TimeUs(1_047 * MS + i64::from(i) * 300 * MS));
    }
    let mut chart = b.build().unwrap();
    for (i, l) in chart.lanes[1..=5].iter_mut().enumerate() {
        l[0].sound = Some(SoundId(u16::try_from(i % 3).unwrap()));
    }
    chart
}

fn audio_on(
    chart: &Chart,
    bank: Arc<SoundBank>,
    sim: SimConfig,
) -> RunnerAudio<RecordingOutput<SimDevice>> {
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, PERIOD, 64, 0).unwrap();
    RunnerAudio::new(
        engine,
        RecordingOutput::new(SimDevice::new(sim), 6 * 48_000),
    )
}

fn left<O>(runner: &Runner<'_, RecordingOutput<O>>) -> Vec<f32>
where
    O: audio::AudioOutput + game::Wakes,
{
    runner
        .audio()
        .output()
        .samples()
        .iter()
        .step_by(2)
        .copied()
        .collect()
}

fn onsets(left: &[f32]) -> Vec<usize> {
    (1..left.len())
        .filter(|&f| left[f - 1] == 0.0 && left[f] != 0.0)
        .collect()
}

fn run_spaced(sim: SimConfig, step: i64) -> (Vec<f32>, SimDevice) {
    let chart = spaced_chart();
    let rule = rules::custom();
    let dev = device(&[3 * MS]);
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, common::bank(&chart), sim);
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(step)), RunOutcome::Complete);
    let samples = left(&runner);
    let device = runner.audio().output().inner().clone();
    (samples, device)
}

/// When the press of tap i is emitted with 100 ms steps: it arrives at
/// 1.050 s + 300 ms × i, between steps, and the first step after it judges
/// it, 50 ms later.
fn emitted(i: i64) -> i64 {
    1_100 * MS + i * 300 * MS
}

#[test]
fn coarse_steps_hand_each_key_sound_to_the_wake_after_its_judgment() {
    // Wakes fall on the 100 ms steps too; the one at the emitting step runs
    // after the logic, so it takes the sound. The nine wakes overdue at that
    // step came before the judgment and must not.
    let (samples, _) = run_spaced(SimConfig::steady(RATE, PERIOD, START), 100 * MS);
    let expected: Vec<usize> = (0..5)
        .map(|i| {
            let j = (emitted(i) - START.0) / PERIOD_US;
            usize::try_from((j + 1) * 480).unwrap()
        })
        .collect();
    assert_eq!(onsets(&samples), expected);
}

#[test]
fn jittered_wakes_never_take_a_key_sound_before_its_judgment() {
    // Wakes up to 3 ms late: whichever wake takes a sound comes at or after
    // its judgment, so the sound plays 2 to 3 periods after it (less the
    // jitter of the wake that took it).
    let jitter = 3 * MS;
    let sim = SimConfig {
        jitter_us: jitter,
        seed: 21,
        ..SimConfig::steady(RATE, PERIOD, START)
    };
    let (samples, device) = run_spaced(sim, 100 * MS);
    let found = onsets(&samples);
    assert_eq!(found.len(), 5);
    for (i, &f) in found.iter().enumerate() {
        let delay = device.true_time(f as u64).0 - emitted(i as i64);
        assert!(
            (2 * PERIOD_US - jitter..=3 * PERIOD_US).contains(&delay),
            "sound {i}: {delay} us after its judgment"
        );
    }
}

#[test]
fn a_key_sound_judged_with_the_song_end_is_still_mixed() {
    // One sounded tap at 1 s, pressed exactly on its late edge (1.180 s)
    // with no offset. The press and the song end are emitted together at
    // 1.181 s; the next wake, 1.190 s, comes after evaluation completed and
    // must still take the sound: wake j = 219 writes from frame 220 × 480.
    let mut chart = ChartBuilder::new(150_000)
        .sound("a")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let rule = rules::custom();
    let dev = device(&[0]);
    let key = |value| {
        Action::Packet(Packet {
            device: DeviceId(0),
            control: control(lane(1)),
            value,
            background: false,
        })
    };
    let script = ScriptedInput::new(vec![
        ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(0), DeviceKind::Keyboard),
        },
        ScriptItem {
            at: TimeUs(1_180 * MS),
            action: key(1),
        },
        ScriptItem {
            at: TimeUs(1_200 * MS),
            action: key(0),
        },
    ])
    .with_recorded(DeviceId(0), vec![control(lane(1))]);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(
        &chart,
        common::bank(&chart),
        SimConfig::steady(RATE, PERIOD, START),
    );
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    assert_eq!(onsets(&left(&runner)), [220 * 480]);
}

#[test]
fn an_abort_stops_the_sounds_and_the_bgm_to_come() {
    // A 2 s BGM sound from 0.5 s is playing when focus is lost at 1 s; a
    // BGM click is due at 1.025 s, inside the block the 1 s wake would
    // write. The abort comes before that wake's mixing: the click never
    // sounds, and the long sound, the BGM still to come and the queue all
    // stop.
    let mut chart = ChartBuilder::new(150_000)
        .sound("long")
        .sound("click")
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    chart.bgm = vec![
        BgmEvent {
            time: TimeUs(500 * MS),
            sound: SoundId(0),
        },
        BgmEvent {
            time: TimeUs(1_025 * MS),
            sound: SoundId(1),
        },
    ];
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![8_192; 96_000]),
            Sound::mono(vec![4_096; 480]),
        ],
    ));
    let rule = rules::custom();
    let dev = device(&[0]);
    let script = ScriptedInput::new(vec![
        ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(0), DeviceKind::Keyboard),
        },
        ScriptItem {
            at: TimeUs(1_000 * MS),
            action: Action::FocusLoss,
        },
    ])
    .with_recorded(DeviceId(0), vec![control(lane(1))]);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, bank, SimConfig::steady(RATE, PERIOD, START));
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    assert_eq!(
        runner.run_to_end(TimeUs(MS)),
        RunOutcome::Aborted(AbortCause::FocusLoss)
    );
    let samples = left(&runner);
    // The long sound was playing (0.25 of full scale), the click never.
    assert!(samples.contains(&0.25));
    assert!(samples.iter().all(|&x| x <= 0.25), "the click sounded");
    let engine = runner.audio().engine();
    assert_eq!(engine.mixer().sounding(), 0);
    assert_eq!(engine.bgm_left(), 0);
}

#[test]
fn bgm_starts_at_its_chart_time_frames_across_the_lead_in() {
    // BGM at chart times -0.5 s (in the lead-in), 0 and 0.25 s. Frame f
    // plays at START + 10 ms + f / 48 kHz, so time t is frame
    // (t + 990 ms) × 48: 23 520, 47 520 and 59 520.
    let mut chart = ChartBuilder::new(150_000)
        .sound("a")
        .sound("b")
        .sound("c")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.bgm = [-500, 0, 250]
        .iter()
        .enumerate()
        .map(|(i, &ms)| BgmEvent {
            time: TimeUs(ms * MS),
            sound: SoundId(u16::try_from(i).unwrap()),
        })
        .collect();
    let rule = rules::custom();
    let dev = device(&[0]);
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(
        &chart,
        common::bank(&chart),
        SimConfig::steady(RATE, PERIOD, START),
    );
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    let samples = left(&runner);
    assert_eq!(onsets(&samples), [23_520, 47_520, 59_520]);
    for (k, f) in [23_520, 47_520, 59_520].into_iter().enumerate() {
        // Sound k is the constant 1000 + 7 k (common::bank).
        let want = f32::from(1_000 + 7 * i16::try_from(k).unwrap()) / 32_768.0;
        assert_eq!(samples[f], want);
    }
}
