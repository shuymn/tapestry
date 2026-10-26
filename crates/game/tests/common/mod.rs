//! Helpers shared by the game's integration tests.

#![allow(dead_code)]

use std::sync::Arc;

use audio::{AudioEngine, MAX_VOICES, RecordingOutput, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{Chart, ChartBuilder, Lane, TimeUs};
use eval::{
    Binding, ControlId, DeviceProfile, InputKind, JudgeEvent, JudgeEventKind, PhysicalInputEvent,
    SourceId, SourceProfile,
};
use game::{Runner, RunnerAudio, Wakes};
use input::{DeviceKind, ScriptedInput, SourceConfig, autoplay};
use rules::Judgment;

pub const MS: i64 = 1_000;

/// The control a source binds to `lane`: `0x100 + lane index`, so control
/// ids never equal lane numbers.
pub fn control(lane: Lane) -> ControlId {
    ControlId(0x100 + u32::try_from(lane.index()).unwrap())
}

/// One source per offset (µs), each binding every lane.
pub fn device(offsets: &[i64]) -> DeviceProfile {
    DeviceProfile {
        sources: offsets
            .iter()
            .map(|&o| SourceProfile {
                input_offset: TimeUs(o),
                bindings: Lane::all()
                    .map(|l| Binding {
                        control: control(l),
                        lane: l,
                    })
                    .collect(),
                axes: Vec::new(),
            })
            .collect(),
    }
}

pub fn lane(n: u8) -> Lane {
    Lane::new(n).unwrap()
}

/// `count` notes from 1 s on, `gap` apart, cycling over the 8 lanes from
/// the scratch; every 5th is a long note `long` long.
pub fn chart(count: i64, gap: i64, long: i64) -> Chart {
    let mut b = ChartBuilder::new(150_000);
    for i in 0..count {
        let l = lane(u8::try_from(i % 8).unwrap());
        let t = 1_000 * MS + i * gap;
        b = if i % 5 == 4 {
            b.long(l, TimeUs(t), TimeUs(t + long))
        } else {
            b.tap(l, TimeUs(t))
        };
    }
    b.build().unwrap()
}

/// The recording window's input capacity (D8): every bound button pressed
/// and released 30 times a second from `start` to `end`, whole seconds up.
pub fn window_capacity(start: TimeUs, end: TimeUs, buttons: usize) -> usize {
    let secs = u64::try_from(end.0 - start.0).unwrap().div_ceil(1_000_000);
    let secs = usize::try_from(secs).unwrap();
    buttons * 60 * secs
}

/// The simulated device's rate and period (10 ms).
pub const RATE: u32 = 48_000;
pub const PERIOD: u32 = 480;

/// Sound `i` of `chart` as 480 frames of the constant `1000 + 7 i`.
pub fn bank(chart: &Chart) -> Arc<SoundBank> {
    let sounds = (0..chart.sounds.len())
        .map(|i| Sound::mono(vec![1_000 + 7 * i16::try_from(i).unwrap(); 480]))
        .collect();
    Arc::new(SoundBank::from_sounds(RATE, sounds))
}

/// The runner's audio: an engine with `chart`'s BGM, playing into a steady
/// simulated device that wakes first at `start`, recording `record_frames`
/// frames and logging `log` scheduled starts.
pub fn audio_with(
    chart: &Chart,
    start: TimeUs,
    record_frames: usize,
    log: usize,
) -> RunnerAudio<RecordingOutput<SimDevice>> {
    let engine = AudioEngine::new(bank(chart), &chart.bgm, RATE, PERIOD, MAX_VOICES, log).unwrap();
    let device = SimDevice::new(SimConfig::steady(RATE, PERIOD, start));
    RunnerAudio::new(engine, RecordingOutput::new(device, record_frames))
}

/// [`audio_with`] recording and logging nothing.
pub fn audio(chart: &Chart, start: TimeUs) -> RunnerAudio<RecordingOutput<SimDevice>> {
    audio_with(chart, start, 0, 0)
}

pub fn sources() -> SourceConfig {
    SourceConfig {
        max_devices: 4,
        max_sources: 4,
        max_held: 16,
        queue_capacity: 1024,
        max_arrivals: 2,
    }
}

/// Autoplay of `chart` for source 0 of `device`, and a script feeding it
/// from one keyboard.
pub fn autoplay_script(
    chart: &Chart,
    device: &DeviceProfile,
) -> (Vec<PhysicalInputEvent>, ScriptedInput) {
    let events = autoplay(chart, &device.sources[0]).unwrap();
    let script = ScriptedInput::from_log(&Default::default(), &events, &[DeviceKind::Keyboard]);
    (events, script)
}

pub fn press(received: i64, source: u16, lane: Lane) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(received),
        source: SourceId(source),
        kind: InputKind::Control {
            control: control(lane),
            value: 1,
        },
    }
}

/// The judgments in `events`: (lane index, note index, judgment, deviation
/// in µs).
pub fn judgments(events: &[JudgeEvent]) -> Vec<(usize, usize, Judgment, Option<i64>)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            JudgeEventKind::Judged {
                lane,
                note,
                judgment,
                deviation,
                ..
            } => Some((lane.index(), note, judgment, deviation.map(|d| d.0))),
            _ => None,
        })
        .collect()
}

pub fn count_judged(events: &[JudgeEvent], j: Judgment) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, JudgeEventKind::Judged { judgment, .. } if judgment == j))
        .count()
}

/// The left channel `runner`'s output recorded.
pub fn left<O: audio::AudioOutput + Wakes>(runner: &Runner<'_, RecordingOutput<O>>) -> Vec<f32> {
    runner
        .audio()
        .output()
        .samples()
        .iter()
        .step_by(2)
        .copied()
        .collect()
}

/// Writes a small self-made BMS chart into `dir` and returns its path: at
/// 120 BPM (2 s a measure), four lane 1 notes and two lane 2 notes in
/// measure 1, and a scratch note and a lane 3 long note (`#LNOBJ`) in
/// measure 2: 4 + 2 + 1 + 1 = 8 judgeable notes.
pub fn write_bms(dir: &std::path::Path) -> std::path::PathBuf {
    let text = "#PLAYER 1\n#TITLE generated\n#BPM 120\n#LNOBJ ZZ\n#WAV01 kick.wav\n\
                #00111:01010101\n#00112:00010001\n#00216:01000000\n#00213:0100ZZ00\n";
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("generated.bms");
    std::fs::write(&path, text).unwrap();
    path
}

/// The judgeable notes of `chart`: taps and long-note heads.
pub fn judgeable(chart: &Chart) -> usize {
    chart
        .lanes
        .iter()
        .flatten()
        .filter(|n| !matches!(n.kind, bms_model::NoteKind::Mine { .. }))
        .count()
}

/// The frames where sound starts after silence.
pub fn onsets(left: &[f32]) -> Vec<usize> {
    (1..left.len())
        .filter(|&f| left[f - 1] == 0.0 && left[f] != 0.0)
        .collect()
}
