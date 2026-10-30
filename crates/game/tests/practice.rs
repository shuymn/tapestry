//! Practice rounds (design.md "Mid-Chart Start and Section Repeat"): the
//! round's opening, its section and recording window, its ring-out and the
//! next round, its record, and the excerpt property end to end.
//!
//! Most tests run D15's example: 100 % speed, a 2 s lead-in, input offset
//! 0, aligned 1 ms steps and 48 kHz output; section `[10 s, 20 s)` of a
//! 180 s chart whose last target is a tap at 19.5 s, with the tapestry
//! preset's 180 ms late BAD edge. The rig below drives the shared play
//! lifecycle on a virtual clock with a real audio engine on a simulated
//! device, and logs when each thing happens on round 1's timeline.

mod common;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use audio::{
    AudioEngine, HEADROOM, RecordingOutput, ScheduledKind, SeekPlan, SimConfig, SimDevice, Sound,
    SoundBank,
};
use bms_model::{Chart, ChartBuilder, SoundId, Speed, TimeUs};
use common::{MS, custom_settings, lane};
use eval::{ControlId, JudgeEventKind};
use game::{
    AudioQueue, ChartIngest, Closing, Command, DataRoot, IngestError, Ingested, InputWaitError,
    Loaded, Opening, Parts, PlayEnd, PlayReport, Practice, RecordRef, RecordSink, RenderSide, Rig,
    SaveError, SessionConfig,
};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput, SourceReader,
    SourceSet,
};
use replay::{BuildProvenance, PlayRecord, PlayType, ReceiveMethod};
use rules::GaugeValue;

const RATE: u32 = 48_000;
const PERIOD: u32 = 480;
/// Where the rig's clock starts.
const CLOCK_START: i64 = 1_000 * MS;
/// Lane 1's key in the default key config (Z).
const Z: ControlId = ControlId(0x2c);

/// The section and lead-in of D15's example.
const SECTION: (i64, i64) = (10_000 * MS, 20_000 * MS);
const LEAD_IN: TimeUs = TimeUs(2_000 * MS);

/// Reads the chart it holds, whatever the file.
struct Built(Chart);

impl ChartIngest for Built {
    fn max_bytes(&self) -> usize {
        1 << 20
    }

    fn accepts(&self, path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "chart")
    }

    fn ingest(
        &self,
        _bytes: &[u8],
        _dir: &Path,
        _seed: u64,
        _speed: Speed,
    ) -> Result<Ingested, IngestError> {
        Ok(Ingested {
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

/// D15's chart: a 60 s BGM sound at 5 s (sound 0), a short one at 20.5 s
/// and 30 s (sound 1); taps on lane 1 at 9 s, then the targets at 10, 12,
/// 14, 16 and 19.5 s, and at 179 s; a tap on lane 2 at 20.4 s with key
/// sound 2, after the section.
fn d15_chart() -> Chart {
    let mut b = ChartBuilder::new(120_000)
        .sound("long.wav")
        .sound("short.wav")
        .sound("key.wav")
        .bgm(TimeUs(5_000 * MS), SoundId(0))
        .bgm(TimeUs(20_500 * MS), SoundId(1))
        .bgm(TimeUs(30_000 * MS), SoundId(1))
        .tap(lane(2), TimeUs(20_400 * MS));
    for t in [9_000, 10_000, 12_000, 14_000, 16_000, 19_500, 179_000] {
        b = b.tap(lane(1), TimeUs(t * MS));
    }
    let mut chart = b.build().unwrap();
    chart.lanes[2][0].sound = Some(SoundId(2));
    chart
}

/// The level of frame `k` of the long sound: its 10 ms block number plus 1,
/// so the output tells how far into the sound a frame is.
fn long_level(k: usize) -> i16 {
    i16::try_from(k / 480 % 1_000 + 1).unwrap()
}

/// Sound 0 lasts 60 s ([`long_level`]); sounds 1 and 2 are 0.1 s at 3000
/// and 5000.
fn bank() -> Arc<SoundBank> {
    Arc::new(SoundBank::from_sounds(
        RATE,
        vec![
            Sound::mono((0..60 * RATE as usize).map(long_level).collect()),
            Sound::mono(vec![3_000; RATE as usize / 10]),
            Sound::mono(vec![5_000; RATE as usize / 10]),
        ],
    ))
}

/// When something happened, on the rig's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Open { at: i64, opened_at: TimeUs },
    RingOut { at: i64, fade: TimeUs, end: TimeUs },
    Done { at: i64 },
    Cancel { at: i64 },
    Close { at: i64 },
    Save { at: i64 },
}

type Log = Rc<RefCell<Vec<Ev>>>;

/// Saves in memory, logging when, and taking `delay` of the clock: a slow
/// write.
struct Saves {
    clock: Rc<Cell<i64>>,
    log: Log,
    delay: i64,
    records: Rc<RefCell<Vec<PlayRecord>>>,
}

impl RecordSink for Saves {
    fn save(&mut self, rec: &PlayRecord) -> Result<RecordRef, SaveError> {
        self.log.borrow_mut().push(Ev::Save {
            at: self.clock.get(),
        });
        self.clock.set(self.clock.get() + self.delay);
        self.records.borrow_mut().push(rec.clone());
        Ok(RecordRef {
            path: PathBuf::from("memory"),
        })
    }
}

/// A rig on a virtual clock, 1 ms a step, with an audio engine playing
/// into a simulated device stepped through the clock, and a keyboard fed by
/// a script on the clock. Commands are given by round and play time.
struct Rounds {
    clock: Rc<Cell<i64>>,
    sources: SourceSet,
    script: ScriptedInput,
    engine: AudioEngine,
    out: RecordingOutput<SimDevice>,
    queue: AudioQueue,
    /// (round from 1, play time, command).
    commands: Vec<(usize, i64, Command)>,
    round: usize,
    origin: i64,
    sealed: bool,
    done: Cell<bool>,
    log: Log,
    reports: Vec<PlayReport>,
}

impl Rounds {
    fn new(
        schedule: Vec<bms_model::BgmEvent>,
        queued: u32,
        log: &Log,
        clock: &Rc<Cell<i64>>,
    ) -> Self {
        let mut sources = SourceSet::new(common::sources()).unwrap();
        let mut script = ScriptedInput::new(vec![ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(4), DeviceKind::Keyboard),
        }]);
        script.poll(TimeUs(CLOCK_START), &mut sources);
        let engine = AudioEngine::with_schedule(bank(), schedule, RATE, PERIOD, 64, 4_096).unwrap();
        let dev = SimDevice::new(SimConfig {
            queue_periods: queued,
            ..SimConfig::steady(RATE, PERIOD, TimeUs(CLOCK_START))
        });
        Self {
            clock: Rc::clone(clock),
            sources,
            script,
            engine,
            out: RecordingOutput::new(dev, 40 * RATE as usize),
            queue: game::audio_link(1).1,
            commands: Vec::new(),
            round: 0,
            origin: 0,
            sealed: false,
            done: Cell::new(false),
            log: Rc::clone(log),
            reports: Vec::new(),
        }
    }

    fn at(&self) -> i64 {
        self.clock.get()
    }

    /// Every buffer event due before now, or at now with `through`.
    fn step_audio(&mut self, through: bool) {
        loop {
            let at = self.out.inner().next_wake().0;
            if at > self.at() || (at == self.at() && !through) {
                return;
            }
            self.engine
                .step(&mut self.out, self.queue.commands())
                .unwrap();
        }
    }

    fn push(&self, ev: Ev) {
        self.log.borrow_mut().push(ev);
    }
}

impl Rig for Rounds {
    type Reader = SourceReader;
    type ScreenError = std::convert::Infallible;

    fn now(&self) -> TimeUs {
        TimeUs(self.at())
    }

    fn wait(&mut self) {
        self.clock.set(self.at() + MS);
        assert!(
            self.at() < CLOCK_START + 120_000 * MS,
            "the run did not end within two minutes of its clock"
        );
        self.script.poll(TimeUs(self.at()), &mut self.sources);
        self.step_audio(false);
    }

    fn stepped(&mut self, aborted: bool) {
        if !aborted {
            self.step_audio(true);
        }
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.at() - self.origin;
        let i = self
            .commands
            .iter()
            .position(|&(round, at, _)| round == self.round && at <= now)?;
        Some(self.commands.remove(i).2)
    }

    fn quitting(&self) -> bool {
        false
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        let now = TimeUs(self.at());
        let origin = TimeUs(now.0 + lead_in.0);
        let recorded = game::DeviceSettings::default().keyboard.recorded();
        let devices: Vec<_> = self.sources.devices().map(|(d, _)| d).collect();
        for d in devices {
            self.sources.set_recorded(d, recorded.clone());
        }
        self.sources.gate_mut().reset();
        let start = self.sources.open_play(now, origin);
        self.round += 1;
        self.origin = origin.0;
        self.push(Ev::Open {
            at: now.0,
            opened_at: start.opened_at,
        });
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
            axes: Vec::new(),
        })
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, _screen: RenderSide) {
        self.engine.restart(origin);
        self.queue = sounds;
        self.sealed = false;
        self.done.set(false);
    }

    fn restore_audio(&mut self, at: TimeUs) {
        // As the headless session restores: at the first frame not yet
        // written if `at`'s already is.
        let line = self.engine.frame_line();
        let written = i128::from(self.engine.written());
        let first = (written * 1_000_000 + i128::from(RATE) - 1) / i128::from(RATE)
            - i128::from(line.base_us);
        let at = TimeUs(at.0.max(i64::try_from(first).unwrap()));
        let plan =
            SeekPlan::new(&self.engine.seek_source(), self.engine.schedule(), line, at).unwrap();
        self.engine.seek(&plan).unwrap();
    }

    fn focus_lost(&self) -> bool {
        false
    }

    fn connected(&self) -> bool {
        true
    }

    fn audio_failure(&self) -> Option<audio::OutputError> {
        None
    }

    fn screen_failed(&self) -> bool {
        false
    }

    fn cancel_audio(&mut self) {
        self.push(Ev::Cancel { at: self.at() });
        self.engine.stop();
        while self.queue.commands().pop().is_ok() {}
    }

    fn seal_audio(&mut self) {
        self.sealed = true;
    }

    fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        self.push(Ev::RingOut {
            at: self.at(),
            fade: fade_at,
            end: end_at,
        });
        self.engine.ring_out(fade_at, end_at);
        self.sealed = true;
    }

    fn audio_done(&self) -> bool {
        let m = self.engine.mixer();
        let done = self.sealed
            && self.queue.is_empty()
            && m.pending() == 0
            && m.sounding() == 0
            && self.engine.bgm_left() == 0;
        if done && !self.done.replace(true) {
            self.log.borrow_mut().push(Ev::Done { at: self.at() });
        }
        done
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        self.push(Ev::Close { at: self.at() });
        let closing = Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: self.sources.axis_out_of_range().collect(),
        };
        self.sources.close_recording();
        Ok(closing)
    }

    fn ended(&mut self, report: &PlayReport) {
        self.reports.push(report.clone());
    }

    fn show_end(&mut self, _report: &PlayReport) {}
}

fn config() -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(CLOCK_START),
        effects: 4_096,
        seed: 0,
        speed: Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

/// D15's practice: the section repeated, every gauge at its start.
fn d15_practice(loaded: &Loaded) -> Practice {
    Practice {
        start: TimeUs(SECTION.0),
        end: TimeUs(SECTION.1),
        lead_in: LEAD_IN,
        initial_gauges: loaded.preset().gauges.iter().map(|g| g.start).collect(),
        repeat: true,
    }
}

/// The chart loaded under the tapestry preset, saving to `saves`.
fn load(dir: &Path, chart: Chart, saves: Saves, config: SessionConfig) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let settings = custom_settings(&root);
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        records: Some(Box::new(saves)),
        ..Parts::standard(&root, &settings.player)
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// What a run of D15's rounds left: the log, the reports, the saved
/// records, the rig, and round 1's origin on the clock.
struct Run {
    log: Vec<Ev>,
    reports: Vec<PlayReport>,
    records: Vec<PlayRecord>,
    rig: Rounds,
}

impl Run {
    /// Round 1's play time of a clock time.
    fn t(&self, at: i64) -> i64 {
        let Some(&Ev::Open { at: opened, .. }) = self.log.first() else {
            panic!("no round opened");
        };
        at - opened + SECTION.0 - LEAD_IN.0
    }
}

/// Runs D15's rounds with `queued` periods on the output, a save taking
/// `save_delay`, and `commands`, until a quit in round 2 at 9 s.
fn d15(queued: u32, save_delay: i64, commands: &[(usize, i64, Command)]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let clock = Rc::new(Cell::new(CLOCK_START));
    let log = Log::default();
    let records = Rc::new(RefCell::new(Vec::new()));
    let saves = Saves {
        clock: Rc::clone(&clock),
        log: Rc::clone(&log),
        delay: save_delay,
        records: Rc::clone(&records),
    };
    let mut loaded = load(dir.path(), d15_chart(), saves, config());
    loaded.set_practice(Some(d15_practice(&loaded))).unwrap();
    let mut rig = Rounds::new(loaded.audio_schedule(), queued, &log, &clock);
    rig.commands = commands.to_vec();
    rig.commands.push((2, 9_000 * MS, Command::Quit));
    let left = game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(left, game::Leave::Back);
    let log = log.borrow().clone();
    let records = records.borrow().clone();
    Run {
        log,
        reports: std::mem::take(&mut rig.reports),
        records,
        rig,
    }
}

/// The first logged event `pick` takes, as round 1's play time.
fn first(run: &Run, pick: impl Fn(&Ev) -> Option<i64>) -> i64 {
    run.t(run.log.iter().find_map(pick).expect("logged"))
}

#[test]
fn section_round_rings_out_and_repeats() {
    let mut run = d15(2, 0, &[]);
    assert_eq!(run.reports.len(), 2);
    let round1 = &run.reports[0];
    assert_eq!(round1.end, PlayEnd::Completed);
    assert_eq!(round1.tail_cut, None);
    // Round 1 opened at its lead-in's start, chart time 8 s.
    assert_eq!(
        run.log[0],
        Ev::Open {
            at: CLOCK_START,
            opened_at: TimeUs(8_000 * MS)
        }
    );
    // SongEnd at the section's final deadline, 19.5 s + 180 ms.
    let song_end: Vec<TimeUs> = round1
        .live
        .iter()
        .filter(|e| e.kind == JudgeEventKind::SongEnd)
        .map(|e| e.time)
        .collect();
    assert_eq!(song_end, [TimeUs(19_680_000)]);
    // Completion and the close in the step at 19.681 s, where the ring-out
    // is sent.
    assert_eq!(
        first(&run, |e| match *e {
            Ev::Close { at } => Some(at),
            _ => None,
        }),
        19_681 * MS
    );
    let Some(&Ev::RingOut { at, fade, end }) =
        run.log.iter().find(|e| matches!(e, Ev::RingOut { .. }))
    else {
        panic!("no ring-out");
    };
    assert_eq!(run.t(at), 19_681 * MS);
    assert_eq!((fade, end), (TimeUs(20_681 * MS), TimeUs(21_181 * MS)));
    // The record is saved only after the engine reported the ring-out
    // done, in the step at 21.181 s.
    let done = first(&run, |e| match *e {
        Ev::Done { at } => Some(at),
        _ => None,
    });
    let saved = first(&run, |e| match *e {
        Ev::Save { at } => Some(at),
        _ => None,
    });
    assert!(done <= saved, "{done} {saved}");
    assert_eq!(saved, 21_181 * MS);
    let save_index = run.log.iter().position(|e| matches!(e, Ev::Save { .. }));
    let done_index = run.log.iter().position(|e| matches!(e, Ev::Done { .. }));
    assert!(done_index < save_index);
    assert_eq!(run.records.len(), 2);
    assert_eq!(run.records[0].status.end, replay::EndStatus::PlayedToEnd);
    // During the ring-out the BGM event at 20.5 s and the autoplay key
    // sound of the note at 20.4 s started; the one at 30 s never did.
    let mut starts = Vec::new();
    run.rig.engine.drain_scheduled(|s| {
        if let ScheduledKind::Bgm { time } = s.kind {
            starts.push(time.0);
        }
    });
    for t in [20_400 * MS, 20_500 * MS] {
        assert!(starts.contains(&t), "{t} {starts:?}");
    }
    assert!(!starts.contains(&(30_000 * MS)), "{starts:?}");
    // Round 2's countdown began at 21.182 s, at its own chart time 8 s.
    let opens: Vec<(i64, TimeUs)> = run
        .log
        .iter()
        .filter_map(|e| match *e {
            Ev::Open { at, opened_at } => Some((run.t(at), opened_at)),
            _ => None,
        })
        .collect();
    assert_eq!(opens[1], (21_182 * MS, TimeUs(8_000 * MS)));
    // The 60 s BGM voice that started at 5 s is playing 3 s in at round 2's
    // countdown, restored, at full gain: the first frame it sounds is
    // its 3.00-3.05 s block.
    let left: Vec<f32> = run.rig.out.samples().iter().step_by(2).copied().collect();
    // Frame `n` plays at `CLOCK_START + (n + 480) / RATE`, and round 1's
    // chart time 8 s at `CLOCK_START`.
    let countdown =
        usize::try_from((21_182 - 8_000 - 10) * MS * i64::from(RATE) / 1_000_000).unwrap();
    let entry = (countdown..).find(|&f| left[f] != 0.0).unwrap();
    assert!(
        entry < countdown + 4 * PERIOD as usize,
        "{entry} {countdown}"
    );
    let level = left[entry] / HEADROOM * 32_768.0;
    assert_eq!(level.fract(), 0.0, "{level}");
    assert!((301.0..=306.0).contains(&level), "{level}");
    // Silent before, from round 1's ring-out's end at 21.181 s.
    let rung = usize::try_from((21_181 - 8_000 - 10) * MS * i64::from(RATE) / 1_000_000).unwrap();
    assert!(left[rung - 1] > 0.0);
    assert!(left[rung..entry].iter().all(|&x| x == 0.0));
}

#[test]
fn early_done_report_waits_for_end_at() {
    for queued in [2, 3] {
        let run = d15(queued, 0, &[]);
        let done = first(&run, |e| match *e {
            Ev::Done { at } => Some(at),
            _ => None,
        });
        // The engine mixes ahead: it reports done before 21.181 s.
        assert!(done < 21_181 * MS, "{queued} queued: done at {done}");
        let saved = first(&run, |e| match *e {
            Ev::Save { at } => Some(at),
            _ => None,
        });
        assert_eq!(saved, 21_181 * MS, "{queued} queued");
        let Some(Ev::Open { at, .. }) = run
            .log
            .iter()
            .filter(|e| matches!(e, Ev::Open { .. }))
            .nth(1)
        else {
            panic!("no round 2");
        };
        assert_eq!(run.t(*at), 21_182 * MS, "{queued} queued");
    }
}

#[test]
fn retry_cuts_the_ring_out_and_saves() {
    let run = d15(2, 0, &[(1, 20_900 * MS, Command::Retry)]);
    let round1 = &run.reports[0];
    assert_eq!(round1.end, PlayEnd::Completed);
    assert_eq!(round1.tail_cut, Some(game::AbortCause::Retry));
    // The ring-out was under way, and was cut in that step: the audio
    // stopped at 20.9 s, before its end, and the frozen record was saved
    // then.
    let rung = first(&run, |e| match *e {
        Ev::RingOut { at, .. } => Some(at),
        _ => None,
    });
    assert_eq!(rung, 19_681 * MS);
    let cancel = first(&run, |e| match *e {
        Ev::Cancel { at } => Some(at),
        _ => None,
    });
    assert_eq!(cancel, 20_900 * MS);
    let saved = first(&run, |e| match *e {
        Ev::Save { at } => Some(at),
        _ => None,
    });
    assert_eq!(saved, 20_900 * MS);
    assert!(round1.saved.as_ref().is_some_and(Result::is_ok));
    assert_eq!(run.records[0].status.end, replay::EndStatus::PlayedToEnd);
    assert_eq!(run.records[0].status.finalized, TimeUs(19_680_999));
    // The retry starts the round over.
    assert_eq!(run.reports.len(), 2);
}

#[test]
fn saving_never_delays_the_ring_out() {
    // A ring-out run to its end, and one quit at 20.9 s, each with an
    // instant save and one taking 400 ms.
    for commands in [vec![], vec![(1, 20_900 * MS, Command::Quit)]] {
        let instant = d15(2, 0, &commands);
        let slow = d15(2, 400 * MS, &commands);
        // Everything of round 1 up to its save, at the same times.
        let round1 = |run: &Run| -> Vec<Ev> {
            let to = run
                .log
                .iter()
                .position(|e| matches!(e, Ev::Save { .. }))
                .unwrap();
            run.log[..=to].to_vec()
        };
        assert_eq!(round1(&instant), round1(&slow));
        assert!(
            round1(&instant)
                .iter()
                .any(|e| matches!(e, Ev::RingOut { .. }))
        );
        // The quit cuts the ring-out in its step, before the save.
        if !commands.is_empty() {
            assert!(round1(&slow).contains(&Ev::Cancel {
                at: CLOCK_START + 20_900 * MS - 8_000 * MS
            }));
        }
        // The fade and the stop on the output, sample for sample, through
        // the ring-out's end.
        // Frame `n` plays at `CLOCK_START + (n + 480) / RATE`, and round
        // 1's chart time 8 s at `CLOCK_START`.
        // A quit ends the run, and the output, at 20.9 s.
        let end = usize::try_from((21_181 - 8_000) * MS * i64::from(RATE) / 1_000_000).unwrap();
        let (a, b) = (instant.rig.out.samples(), slow.rig.out.samples());
        let n = (2 * end).min(a.len());
        assert!(n > 2 * 600_000, "{n}");
        assert_eq!(a[..n], b[..n]);
        assert_eq!(instant.records[0], slow.records[0]);
    }
}

#[test]
fn section_round_records_its_conditions() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Rc::new(Cell::new(CLOCK_START));
    let log = Log::default();
    let records = Rc::new(RefCell::new(Vec::new()));
    let saves = Saves {
        clock: Rc::clone(&clock),
        log: Rc::clone(&log),
        delay: 0,
        records: Rc::clone(&records),
    };
    let mut loaded = load(dir.path(), d15_chart(), saves, config());
    let ids: Vec<String> = loaded
        .preset()
        .gauges
        .iter()
        .map(|g| g.id.clone())
        .collect();
    assert_eq!(ids, ["normal", "survival"]);
    let pct = |n: i64| GaugeValue(n * GaugeValue::PERCENT);
    for repeat in [true, false] {
        let practice = Practice {
            initial_gauges: vec![pct(50), pct(70)],
            repeat,
            ..d15_practice(&loaded)
        };
        loaded.set_practice(Some(practice)).unwrap();
        let mut rig = Rounds::new(loaded.audio_schedule(), 2, &log, &clock);
        // Round 1 is left by a quit at 18 s.
        rig.commands.push((1, 18_000 * MS, Command::Quit));
        game::live::run_chart(&mut loaded, &mut rig).unwrap();
    }
    let records = records.borrow();
    assert_eq!(records.len(), 2);
    let round = |attempt| replay::Practice {
        start: TimeUs(SECTION.0),
        end: TimeUs(SECTION.1),
        lead_in: LEAD_IN,
        attempt,
        initial_gauges: vec![
            replay::InitialGauge {
                id: "normal".to_owned(),
                value: pct(50),
            },
            replay::InitialGauge {
                id: "survival".to_owned(),
                value: pct(70),
            },
        ],
    };
    assert_eq!(records[0].play_type, PlayType::SectionRepeat(round(1)));
    assert_eq!(records[1].play_type, PlayType::MidChartStart(round(2)));
    assert_eq!(records[0].start.opened_at, TimeUs(8_000 * MS));
    for r in records.iter() {
        assert_eq!(r.speed, Speed::NORMAL);
        let back = replay::decode(&replay::encode(r), &replay::DecodeLimits::STANDARD).unwrap();
        assert_eq!(&back, r);
        // The record re-evaluates as played, in its section, from its
        // recorded gauges.
        let chart = d15_chart();
        let re = replay::verify::evaluate_under(r, &chart, loaded.preset()).unwrap();
        assert!(re.as_played);
        assert_eq!(re.started_at_default, [false, false]);
    }
}

/// `text` (a BMS chart) loaded at `speed` under the tapestry preset.
fn load_bms(dir: &Path, text: &str, speed: Speed) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let settings = custom_settings(&root);
    let chart_path = dir.join("chart.bms");
    std::fs::write(&chart_path, text).unwrap();
    let parts = Parts::standard(&root, &settings.player);
    Loaded::load(
        root,
        settings,
        &chart_path,
        SessionConfig { speed, ..config() },
        parts,
    )
    .unwrap()
}

#[test]
fn practice_window_is_sized_by_the_section() {
    // 60 BPM, so a measure is 4 s: a tap at 69.5 s is 1.5 s into measure
    // 17; taps at 2 s and 74 s lie before and after the section.
    let text = "#BPM 60\n#WAV01 a.wav\n#00011:0001\n#01711:0000000100000000\n#01811:0001\n";
    let dir = tempfile::tempdir().unwrap();
    let mut seen = 0;
    for (k, window) in [
        (10, 21_180_000),
        (13, 16_795_384),
        (20, 11_680_000),
        (30, 8_513_333),
    ] {
        let speed = Speed::from_twentieths(k).unwrap();
        let mut loaded = load_bms(dir.path(), text, speed);
        let practice = Practice {
            start: TimeUs(60_000 * MS),
            end: TimeUs(70_000 * MS),
            lead_in: LEAD_IN,
            initial_gauges: loaded.preset().gauges.iter().map(|g| g.start).collect(),
            repeat: true,
        };
        loaded.set_practice(Some(practice)).unwrap();
        let plan = loaded.plan(&[DeviceKind::Keyboard], &[]).unwrap();
        assert_eq!(plan.window, TimeUs(window), "{k}/20");
        // Capacity counts whole seconds of the window, rounded up: the
        // keyboard's 240 edges a second, and 2 inputs for each of its
        // retry and quit keys.
        let secs = usize::try_from((window + 999_999) / 1_000_000).unwrap();
        assert_eq!(plan.logic.inputs, secs * 240 + 4, "{k}/20");
        seen += 1;
    }
    assert_eq!(seen, 4);
}

#[test]
fn practice_window_includes_ln_end_deadlines() {
    // One long note from 18.000 s to 19.900 s, and a tap at 30 s.
    let chart = ChartBuilder::new(120_000)
        .long(lane(1), TimeUs(18_000 * MS), TimeUs(19_900 * MS))
        .tap(lane(2), TimeUs(30_000 * MS))
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let saves = Saves {
        clock: Rc::new(Cell::new(0)),
        log: Log::default(),
        delay: 0,
        records: Rc::default(),
    };
    let mut loaded = load(dir.path(), chart, saves, config());
    let mut practice = d15_practice(&loaded);
    practice.end = TimeUs(20_000 * MS);
    loaded.set_practice(Some(practice)).unwrap();
    let plan = loaded.plan(&[DeviceKind::Keyboard], &[]).unwrap();
    // The deadline is the LN end, 19.900 s, not the head's miss (18.180 s):
    // a window of 11.900 s from 8 s, and a capacity of 12 seconds.
    assert_eq!(plan.window, TimeUs(11_900_000));
    assert_eq!(plan.logic.inputs, 12 * 240 + 4);
}

#[test]
fn slow_full_play_over_budget_is_refused() {
    // 30 minutes at 1× (one note in measure 450 at 60 BPM): 60 at 50 %,
    // more than one keyboard's 54.8 minutes.
    let text = "#BPM 60\n#WAV01 a.wav\n#45011:01\n";
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = load_bms(dir.path(), text, Speed::SLOWEST);
    let err = loaded.plan(&[DeviceKind::Keyboard], &[]).unwrap_err();
    let game::LogicError::RecordingTooLarge {
        needed,
        allowed,
        section,
    } = err
    else {
        panic!("{err:?}");
    };
    assert!(!section);
    assert!(needed > allowed, "{needed} {allowed}");
    let message = err.to_string();
    for part in [
        format!("{needed} inputs"),
        format!("{allowed} fit"),
        "practice a section".to_owned(),
    ] {
        assert!(message.contains(&part), "{message}");
    }
    // Played, it is refused at the opening: the countdown is never handed
    // to audio and the screen.
    let log = Log::default();
    let clock = Rc::new(Cell::new(CLOCK_START));
    let mut rig = Rounds::new(loaded.audio_schedule(), 2, &log, &clock);
    let refused = game::live::run_chart(&mut loaded, &mut rig).unwrap_err();
    assert!(
        matches!(
            refused,
            game::LiveError::Logic(game::LogicError::RecordingTooLarge { section: false, .. })
        ),
        "{refused:?}"
    );
    assert!(rig.reports.is_empty());
    assert!(rig.out.samples().iter().all(|&x| x == 0.0));
    // A section of it fits, with no hint.
    let practice = Practice {
        start: TimeUs(1_700_000 * MS),
        end: TimeUs(1_801_000 * MS),
        lead_in: LEAD_IN,
        initial_gauges: loaded.preset().gauges.iter().map(|g| g.start).collect(),
        repeat: true,
    };
    loaded.set_practice(Some(practice)).unwrap();
    assert!(loaded.plan(&[DeviceKind::Keyboard], &[]).is_ok());
}

#[test]
fn excerpt_matches_mid_chart_start_end_to_end() {
    // Lane 1 taps every 500 ms from 1 s to 20 s.
    let mut b = ChartBuilder::new(120_000);
    for i in 0..39 {
        b = b.tap(lane(1), TimeUs(1_000 * MS + i * 500 * MS));
    }
    let chart = b.build().unwrap();
    // The player's presses on the chart's time: lane 1 hit 20 ms late from
    // 3 s on, and lane 3's key held from 5.5 s to 7 s, across the
    // lead-in's start at 6 s.
    let x = ControlId(0x2d);
    let mut presses: Vec<(i64, ControlId, i32)> = vec![(5_500 * MS, x, 1), (7_000 * MS, x, 0)];
    for i in 4..39 {
        let t = 1_000 * MS + i * 500 * MS + 20 * MS;
        presses.push((t, Z, 1));
        presses.push((t + 50 * MS, Z, 0));
    }
    let section = (8_000 * MS, 14_000 * MS);
    let play = |practice: bool| -> PlayRecord {
        let dir = tempfile::tempdir().unwrap();
        let clock = Rc::new(Cell::new(CLOCK_START));
        let log = Log::default();
        let records = Rc::new(RefCell::new(Vec::new()));
        let saves = Saves {
            clock: Rc::clone(&clock),
            log: Rc::clone(&log),
            delay: 0,
            records: Rc::clone(&records),
        };
        let mut loaded = load(dir.path(), chart.clone(), saves, config());
        if practice {
            let p = Practice {
                start: TimeUs(section.0),
                end: TimeUs(section.1),
                lead_in: LEAD_IN,
                initial_gauges: loaded.preset().gauges.iter().map(|g| g.start).collect(),
                repeat: false,
            };
            loaded.set_practice(Some(p)).unwrap();
        }
        let mut rig = Rounds::new(loaded.audio_schedule(), 2, &log, &clock);
        // The play opens now, so chart time `t` is on the clock at
        // `CLOCK_START + t - opens_at`.
        let shift = CLOCK_START - loaded.opens_at().0;
        let mut items: Vec<ScriptItem> = presses
            .iter()
            .map(|&(t, control, value)| ScriptItem {
                at: TimeUs(shift + t),
                action: Action::Packet(Packet {
                    device: DeviceId(4),
                    control,
                    value,
                    background: false,
                }),
            })
            .collect();
        items.push(ScriptItem {
            at: TimeUs(i64::MIN),
            action: Action::Connect(DeviceId(4), DeviceKind::Keyboard),
        });
        rig.sources = SourceSet::new(common::sources()).unwrap();
        rig.script = ScriptedInput::new(items);
        rig.script.poll(TimeUs(CLOCK_START - 1), &mut rig.sources);
        // The full play is left on its result; the round shows its result
        // too, as a mid-chart start does.
        rig.commands.push((1, 25_000 * MS, Command::Quit));
        let _ = game::live::run_chart(&mut loaded, &mut rig);
        // Live, the play judged what its record re-evaluates to.
        let summary = rig.reports[0].result.as_ref().unwrap();
        assert!(summary.matches_live, "practice: {practice}");
        let records = records.borrow();
        assert_eq!(records.len(), 1);
        records[0].clone()
    };
    let full = play(false);
    let round = play(true);
    assert_eq!(round.start.opened_at, TimeUs(section.0 - LEAD_IN.0));
    assert!(
        round.start.held.iter().any(|&(_, c, v)| c == x && v != 0),
        "lane 3's key is held at the opening"
    );
    let preset = rules::custom();
    let sec = eval::Section {
        start: TimeUs(section.0),
        end: TimeUs(section.1),
        lead_in: LEAD_IN,
        initial_gauges: preset.gauges.iter().map(|g| g.start).collect(),
    };
    // The excerpt of the full play's record, evaluated in the section.
    let excerpt = eval::evaluate(
        &chart,
        &eval::EvalInput {
            events: full.events.clone(),
            device: full.devices.profile(),
            start: full.start.clone(),
            finalized: Some(full.status.finalized),
            section: Some(sec),
        },
        &preset,
    )
    .unwrap();
    // The mid-chart start's record, as played.
    let started = replay::verify::evaluate_under(&round, &chart, &preset)
        .unwrap()
        .evaluation;
    assert!(excerpt.completed && started.completed);
    // 12 targets, each GREAT or better 20 ms late.
    assert_eq!(
        excerpt.tally.counts.perfect + excerpt.tally.counts.great,
        12
    );
    assert_eq!(excerpt, started);
}
