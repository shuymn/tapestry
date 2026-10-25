//! The runner's audio step: when key sounds and BGM reach the output, and
//! what an abort or the end of evaluation does to them. Expected frames are
//! worked out from the device's timing: it wakes every 10 ms (480 frames at
//! 48 kHz) from START, wake j writes from frame (j + 1) × 480, and frame f
//! plays at START + 10 ms + f / 48 kHz.

mod common;

use std::sync::Arc;

use audio::{
    AudioEngine, AudioOutput, OutputError, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank,
    Wake,
};
use bms_model::{BgmEvent, Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, autoplay_script, control, device, lane, left, onsets};
use game::{AbortCause, LogicConfig, RunOutcome, Runner, RunnerAudio, Wakes};
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

/// An engine with `chart`'s BGM playing into `out`, recording 6 s.
fn audio_on<O: AudioOutput + Wakes>(
    chart: &Chart,
    bank: Arc<SoundBank>,
    out: O,
) -> RunnerAudio<RecordingOutput<O>> {
    let engine = AudioEngine::new(bank, &chart.bgm, RATE, PERIOD, 64, 0).unwrap();
    RunnerAudio::new(engine, RecordingOutput::new(out, 6 * 48_000))
}

fn run_spaced(sim: SimConfig, step: i64) -> (Vec<f32>, SimDevice) {
    let chart = spaced_chart();
    let rule = rules::custom();
    let dev = device(&[3 * MS]);
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, common::bank(&chart), SimDevice::new(sim));
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

/// A simulated device that, for `empty_wakes` buffer events from
/// `empty_from` on, hands the engine an empty buffer (a legal write that
/// takes nothing), so nothing it renders reaches the device.
struct Flaky {
    inner: SimDevice,
    empty_from: TimeUs,
    empty_wakes: u32,
    last: TimeUs,
    /// Writes from wakes at or after this time fail.
    fail_from: TimeUs,
}

impl Flaky {
    fn new(empty_from: TimeUs, empty_wakes: u32) -> Self {
        Self {
            inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, START)),
            empty_from,
            empty_wakes,
            last: TimeUs(i64::MIN),
            fail_from: TimeUs(i64::MAX),
        }
    }
}

impl AudioOutput for Flaky {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }
    fn period(&self) -> u32 {
        self.inner.period()
    }
    fn wait(&mut self) -> Wake {
        let w = self.inner.wait();
        self.last = w.at;
        w
    }
    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), OutputError> {
        if self.last >= self.fail_from {
            return Err(OutputError::DeviceLost);
        }
        if self.last >= self.empty_from && self.empty_wakes > 0 {
            self.empty_wakes -= 1;
            f(&mut []);
            return Ok(());
        }
        self.inner.write(f)
    }
}

impl Wakes for Flaky {
    fn next_wake(&self) -> TimeUs {
        self.inner.next_wake()
    }
}

/// One keyboard pressing lane 1 at each of `presses` (released 40 µs
/// later), and losing focus at `focus`.
fn key_script(presses: &[i64], focus: Option<i64>) -> ScriptedInput {
    key_script_with(presses, 40, focus)
}

/// As [`key_script`], each press held `hold` µs.
fn key_script_with(presses: &[i64], hold: i64, focus: Option<i64>) -> ScriptedInput {
    let key = |value| {
        Action::Packet(Packet {
            device: DeviceId(0),
            control: control(lane(1)),
            value,
            background: false,
        })
    };
    let at = |t: i64, action| ScriptItem {
        at: TimeUs(t),
        action,
    };
    let mut items = vec![at(
        i64::MIN,
        Action::Connect(DeviceId(0), DeviceKind::Keyboard),
    )];
    for &t in presses {
        items.push(at(t, key(1)));
        items.push(at(t + hold, key(0)));
    }
    items.extend(focus.map(|t| at(t, Action::FocusLoss)));
    ScriptedInput::new(items).with_recorded(DeviceId(0), vec![control(lane(1))])
}

/// One sounded tap at 1 s, pressed exactly on its late edge (1.180 s) with
/// no offset: the press and the song end are emitted together at 1.181 s.
fn late_edge(out: Flaky, step: i64) -> (RunOutcome, Vec<f32>) {
    let mut chart = ChartBuilder::new(150_000)
        .sound("a")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let rule = rules::custom();
    let dev = device(&[0]);
    let script = key_script(&[1_180 * MS], None);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, common::bank(&chart), out);
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    let outcome = runner.run_to_end(TimeUs(step));
    (outcome, left(&runner))
}

#[test]
fn a_key_sound_judged_with_the_song_end_is_still_mixed() {
    // The next wake, 1.190 s, comes after evaluation completed and must
    // still take the sound: wake j = 219 writes from frame 220 × 480.
    let (outcome, samples) = late_edge(Flaky::new(START, 0), MS);
    assert_eq!(outcome, RunOutcome::Complete);
    assert_eq!(onsets(&samples), [220 * 480]);
}

#[test]
fn a_key_sound_taken_by_an_empty_write_is_rendered_by_the_next() {
    // The 1.190 s wake takes the sound but writes nothing, so it waits as a
    // pending start; the 1.200 s wake renders it, still from frame
    // 220 × 480 (the first one not written).
    let (outcome, samples) = late_edge(Flaky::new(TimeUs(1_190 * MS), 1), MS);
    assert_eq!(outcome, RunOutcome::Complete);
    assert_eq!(onsets(&samples), [220 * 480]);
}

#[test]
fn the_handoff_ends_at_the_wake_that_renders_the_last_sound() {
    // Steps of 25 ms: the press at 1.180 s and the song end are emitted at
    // the 1.200 s step, whose wake takes the sound but writes nothing. The
    // 1.210 s wake renders it, from frame 221 × 480; the device fails from
    // 1.220 s on, which the finished handoff must not reach.
    let out = Flaky {
        fail_from: TimeUs(1_220 * MS),
        ..Flaky::new(TimeUs(1_200 * MS), 1)
    };
    let (outcome, samples) = late_edge(out, 25 * MS);
    assert_eq!(outcome, RunOutcome::Complete);
    assert_eq!(onsets(&samples), [221 * 480]);
}

#[test]
fn a_healthy_backlog_of_sounds_completes() {
    // 5000 key sounds pressed from 0.71 s, while the device misses every
    // wake from 0.70 s to 1.20 s (passes 169 to 219), so all of them are
    // still queued when evaluation completes at 1.181 s. Each wake after
    // takes up to 1024, more than four wakes in all, and every one makes
    // progress: the handoff completes.
    let mut chart = ChartBuilder::new(150_000)
        .sound("a")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let rule = rules::custom();
    let dev = device(&[0]);
    let presses: Vec<i64> = (0..5_000).map(|k| 710 * MS + k * 20).collect();
    let script = key_script_with(&presses, 10, None);
    let sources = SourceSet::new(common::sources()).unwrap();
    let out = Flaky {
        inner: SimDevice::new(SimConfig {
            missed_wakes: (169..=219).collect(),
            ..SimConfig::steady(RATE, PERIOD, START)
        }),
        ..Flaky::new(START, 0)
    };
    let audio = audio_on(&chart, common::bank(&chart), out);
    let config = LogicConfig {
        inputs: 16_384,
        effects: 16_384,
    };
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config, START, audio).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::Complete);
    let sounds = runner
        .stream()
        .iter()
        .filter(|e| matches!(e.kind, eval::JudgeEventKind::PlaySound { .. }))
        .count();
    assert_eq!(sounds, 5_000);
    assert_eq!(runner.queued_sounds(), 0);
    assert_eq!(runner.audio().engine().mixer().pending(), 0);
}

#[test]
fn a_stalled_output_ends_the_handoff_instead_of_waiting_forever() {
    // An output that never takes a frame, and 1100 key sounds, more than the
    // 1024 starts the mixer holds: they can never all be rendered, so the
    // runner gives up after HANDOFF_WAKES wakes and says so.
    let mut chart = ChartBuilder::new(150_000)
        .sound("a")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let rule = rules::custom();
    let dev = device(&[0]);
    let presses: Vec<i64> = (0..1_100).map(|k| 200 * MS + k * 100).collect();
    let script = key_script(&presses, None);
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, common::bank(&chart), Flaky::new(START, u32::MAX));
    let config = LogicConfig {
        inputs: 4_096,
        effects: 4_096,
    };
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config, START, audio).unwrap();
    assert_eq!(runner.run_to_end(TimeUs(MS)), RunOutcome::HandoffStalled);
    assert!(onsets(&left(&runner)).is_empty());
    assert_eq!(runner.queued_sounds(), 0);
    assert_eq!(runner.audio().engine().mixer().pending(), 0);
}

/// A 2 s BGM sound playing from 0.5 s, a BGM click due at `click`, a
/// sounded tap at 2 s whose key sound is pressed at each of `presses`, and
/// focus lost at `focus`; returns the run's outcome and samples.
fn abort_run(
    step: i64,
    focus: i64,
    click: i64,
    presses: &[i64],
    out: Flaky,
) -> (RunOutcome, Vec<f32>, usize, usize, usize) {
    let mut chart = ChartBuilder::new(150_000)
        .sound("long")
        .sound("click")
        .sound("key")
        .tap(lane(1), TimeUs(2_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(2));
    chart.bgm = vec![
        BgmEvent {
            time: TimeUs(500 * MS),
            sound: SoundId(0),
        },
        BgmEvent {
            time: TimeUs(click),
            sound: SoundId(1),
        },
    ];
    let bank = Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono(vec![8_192; 96_000]),
            Sound::mono(vec![4_096; 480]),
            Sound::mono(vec![2_048; 480]),
        ],
    ));
    let rule = rules::custom();
    let dev = device(&[0]);
    let script = key_script(presses, Some(focus));
    let sources = SourceSet::new(common::sources()).unwrap();
    let audio = audio_on(&chart, bank, out);
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    let outcome = runner.run_to_end(TimeUs(step));
    let engine = runner.audio().engine();
    (
        outcome,
        left(&runner),
        engine.mixer().sounding(),
        engine.bgm_left(),
        runner.queued_sounds(),
    )
}

#[test]
fn an_abort_stops_the_sounds_and_the_bgm_to_come() {
    // Focus is lost at 1 s, while the long sound plays. A click is due at
    // 1.025 s, inside the block the 1 s wake would write. A key sound
    // pressed at 0.985 s was taken by the 0.99 s wake, which wrote nothing,
    // so it waits as a pending start; one pressed at 0.998 s is still
    // queued. The abort comes before the 1 s wake: the click never sounds,
    // and the long sound, the pending start (counted as sounding), the
    // queued sound and the BGM
    // to come all stop.
    let (outcome, samples, sounding, bgm_left, queued) = abort_run(
        MS,
        1_000 * MS,
        1_025 * MS,
        &[985 * MS, 998 * MS],
        Flaky::new(TimeUs(990 * MS), 1),
    );
    assert_eq!(outcome, RunOutcome::Aborted(AbortCause::FocusLoss));
    // The long sound was playing (0.25 of full scale), nothing else.
    assert!(samples.contains(&0.25));
    assert!(samples.iter().all(|&x| x <= 0.25), "the click sounded");
    assert_eq!((sounding, bgm_left, queued), (0, 0, 0));
}

#[test]
fn a_failed_write_stops_the_sounds_and_the_bgm_to_come() {
    // The device fails from its 1 s wake on, while the long sound plays and
    // a key sound pressed at 0.998 s is queued: the run stops with the
    // failure, and everything still to sound is cancelled.
    let out = Flaky {
        fail_from: TimeUs(1_000 * MS),
        ..Flaky::new(START, 0)
    };
    let (outcome, _, sounding, bgm_left, queued) =
        abort_run(MS, 3_000 * MS, 1_025 * MS, &[998 * MS], out);
    assert_eq!(outcome, RunOutcome::OutputFailed(OutputError::DeviceLost));
    assert_eq!((sounding, bgm_left, queued), (0, 0, 0));
}

#[test]
fn a_focus_loss_between_coarse_steps_stops_audio_before_its_overdue_wakes() {
    // Steps of 100 ms: focus lost at 1.050 s is seen at the 1.100 s step.
    // The wakes from 1.060 s on are overdue then, but come after the loss;
    // the 1.080 s one would write the block holding a click due at
    // 1.105 s. The abort stops audio before any of them runs.
    let (outcome, samples, sounding, bgm_left, _) =
        abort_run(100 * MS, 1_050 * MS, 1_105 * MS, &[], Flaky::new(START, 0));
    assert_eq!(outcome, RunOutcome::Aborted(AbortCause::FocusLoss));
    assert!(samples.contains(&0.25));
    assert!(samples.iter().all(|&x| x <= 0.25), "the click sounded");
    assert_eq!((sounding, bgm_left), (0, 0));
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
        SimDevice::new(SimConfig::steady(RATE, PERIOD, START)),
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

#[test]
fn a_failed_output_stops_the_run() {
    // The device's first wake is 20 ms into the run, and every write fails.
    let first = TimeUs(START.0 + 20 * MS);
    let chart = spaced_chart();
    let rule = rules::custom();
    let dev = device(&[3 * MS]);
    let (_, script) = autoplay_script(&chart, &dev);
    let sources = SourceSet::new(common::sources()).unwrap();
    let out = Flaky {
        inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, first)),
        fail_from: TimeUs(i64::MIN),
        ..Flaky::new(START, 0)
    };
    let audio = audio_on(&chart, common::bank(&chart), out);
    let mut runner =
        Runner::new(&chart, &rule, &dev, sources, script, config(), START, audio).unwrap();
    assert_eq!(
        runner.run_to_end(TimeUs(MS)),
        RunOutcome::OutputFailed(OutputError::DeviceLost)
    );
    // It stopped at the step that woke the device, not before or after:
    // that step published `first` - 1 us, less the 3 ms input offset.
    assert_eq!(runner.view().finalized, Some(TimeUs(first.0 - 1 - 3 * MS)));
}
