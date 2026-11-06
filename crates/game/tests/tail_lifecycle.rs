//! The audio tail's feedback through the play lifecycle (design.md "Play
//! Lifecycle", the audio tail), on a rig with the real audio thread's
//! loop (`AudioControl` and `AudioWorker` on a simulated output, stepped
//! as its buffer events fall due) and the real source set, on a virtual
//! clock: while the BGM keeps the tail going, fresh presses sound and light
//! their lanes without changing the record, the live stream or the totals;
//! every way out of the tail ends the feedback first, so a press after it
//! neither sounds nor lights; the audio thread's report that the audio
//! ended counts only when it names every key sound the queue accepted; and
//! a practice round's ring-out keeps its limits.
//!
//! State × event table (each cell pinned by the test named):
//!
//! | State \ event | fresh press | release / scratch | staged, input offset | audio done | stale done | rearm |
//! |---|---|---|---|---|---|---|
//! | Playing | judged (existing) | judged (existing) | merged by the watermark (existing) | — | — | fresh opening (`rearm_alloc`) |
//! | Completion step | ≤ watermark judged; beyond it staged | as press | staged kept, record holds it (`tail_feedback`) | — | — | — |
//! | Audio tail | sounds, lights, unjudged (`tail_presses_sound_and_light`) | lane off, deadline at its time (`tail_feedback`) | converted first, evaluation time (`tail_feedback`) | ends the tail (`tail_presses_sound_and_light`) | ignored until consumed (`a_stale_audio_report_does_not_end_the_tail`) | none: committed |
//! | Exit: ran out, retry, skip, close, suspend, focus loss, device loss, input, audio or screen failure | not converted, silent (`every_tail_exit_ends_the_feedback_first`) | not converted | dropped with the sources | — | — | retry: next attempt's fresh opening |
//! | Exit: practice ring-out | before its end: faded; after: silent (`a_rounds_ring_out_keeps_its_limits`) | — | — | with the play time at its end | as above | — |

mod common;

use std::path::Path;

use audio::{AudioEngine, Gain, MAX_VOICES, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{Chart, ChartBuilder, SoundId, TimeSource, TimeUs, VirtualClock};
use common::{MS, PERIOD, RATE, lane, tapestry_settings};
use eval::{ControlId, InputFacts, LaneInput, Tally};
use game::{
    AbortCause, AudioControl, AudioProbe, AudioQueue, AudioWorker, ChartIngest, Closing, Command,
    DataRoot, DeviceSettings, IngestError, Ingested, InputWaitError, Loaded, Opening, Parts,
    PlayEnd, PlayReport, Practice, RenderSide, ResultScreen, Rig, SessionConfig, audio_control,
};
use input::{
    Action, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput, SourceReader,
    SourceSet,
};
use replay::{BuildProvenance, ReceiveMethod};

/// Where the rig's clock starts.
const CLOCK_START: i64 = 7_300_000;
/// The input offset every play is evaluated with.
const OFFSET: i64 = 3_000;
const KEYBOARD: DeviceId = DeviceId(4);

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
        _speed: bms_model::Speed,
    ) -> Result<Ingested, IngestError> {
        Ok(Ingested {
            labels: game::ChartLabels::default(),
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

/// Key 1's taps at 1.0 s (sound 1) and 1.1 s (sound 2), key 2's tap at
/// 1.05 s without a sound, and a BGM sound at 1.0 s (sound 0) lasting
/// 3 s: the play completes at 1.28 s and its tail lasts to about 4 s.
fn chart() -> Chart {
    let mut chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .sound("one.wav")
        .sound("two.wav")
        .bgm(TimeUs(1_000 * MS), SoundId(0))
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(1_100 * MS))
        .tap(lane(2), TimeUs(1_050 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(1));
    chart.lanes[1][1].sound = Some(SoundId(2));
    chart
}

/// The BGM 3 s long, each key sound 10 ms.
fn bank() -> std::sync::Arc<SoundBank> {
    let key = || Sound::mono(vec![500; 480]);
    std::sync::Arc::new(SoundBank::from_sounds(
        RATE,
        vec![Sound::mono(vec![900; 3 * RATE as usize]), key(), key()],
    ))
}

fn loaded(dir: &Path) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart())),
        ..Parts::standard(&root, &settings.player)
    };
    let config = SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 11,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// The control the default key config binds to lane `n`.
fn code(n: u8) -> ControlId {
    let keys = DeviceSettings::default().keyboard;
    ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control)
}

/// Lane `n`'s key set to `value` at play time `ms`.
fn key(ms: i64, n: u8, value: i32) -> ScriptItem {
    item(
        ms * MS,
        Action::Packet(Packet {
            device: KEYBOARD,
            control: code(n),
            value,
            background: false,
        }),
    )
}

fn item(at: i64, action: Action) -> ScriptItem {
    ScriptItem {
        at: TimeUs(at),
        action,
    }
}

/// Lane `n` tapped at play time `ms`, released 20 ms later.
fn tap(ms: i64, n: u8) -> [ScriptItem; 2] {
    [key(ms, n, 1), key(ms + 20, n, 0)]
}

/// Both of key 1's notes hit.
fn hits() -> Vec<ScriptItem> {
    [tap(1_000, 1), tap(1_100, 1)].concat()
}

/// What one tail step's view showed.
#[derive(Clone, Debug)]
struct Seen {
    /// The play time of the step.
    at: i64,
    complete: bool,
    input: InputFacts,
    tally: Tally,
    finalized: Option<TimeUs>,
    events: u64,
}

/// The game's threads on a virtual clock: each wait moves it 1 ms, feeds
/// the script's batches up to the play's time, and runs the audio
/// thread's steps that fall due.
struct Tail {
    clock: VirtualClock,
    sources: SourceSet,
    script: ScriptedInput,
    /// Commands at play times.
    commands: Vec<(i64, Command)>,
    /// The window closes at this play time.
    quit_at: i64,
    origin: Option<TimeUs>,
    participants: Vec<(DeviceId, Option<u64>)>,
    control: AudioControl,
    worker: AudioWorker,
    out: Failing,
    /// The input thread is gone from this play time on.
    input_fails_at: i64,
    /// The screen fails from this play time on.
    screen_fails_at: i64,
    probes: rtrb::Consumer<AudioProbe>,
    /// Key sounds the engine scheduled.
    key_starts: usize,
    side: Option<RenderSide>,
    seen: Vec<Seen>,
    /// The play time of the step that sealed or rang out the audio.
    completed_at: Option<i64>,
    /// Each play's report.
    reports: Vec<PlayReport>,
    handoffs: usize,
    feedback_ends: usize,
    cancels: usize,
}

impl Tail {
    fn new(items: Vec<ScriptItem>, commands: &[(i64, Command)], quit_at: i64) -> Self {
        let mut items = items;
        items.push(item(
            i64::MIN,
            Action::Connect(KEYBOARD, DeviceKind::Keyboard),
        ));
        let recorded = DeviceSettings::default().keyboard.recorded();
        let script = ScriptedInput::new(items).with_recorded(KEYBOARD, recorded);
        let chart = chart();
        let engine = AudioEngine::new(bank(), &chart.bgm, RATE, PERIOD, MAX_VOICES, 64).unwrap();
        let (control, mut worker) = audio_control(engine);
        let (tx, probes) = rtrb::RingBuffer::new(1_024);
        worker.set_probe(tx);
        Self {
            clock: VirtualClock::new(TimeUs(CLOCK_START)),
            sources: SourceSet::new(common::sources()).unwrap(),
            script,
            commands: commands.to_vec(),
            quit_at,
            origin: None,
            participants: Vec::new(),
            control,
            worker,
            out: Failing {
                inner: SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(CLOCK_START))),
                last: TimeUs(i64::MIN),
                fail_from: TimeUs(i64::MAX),
                failed: false,
            },
            input_fails_at: i64::MAX,
            screen_fails_at: i64::MAX,
            probes,
            key_starts: 0,
            side: None,
            seen: Vec::new(),
            completed_at: None,
            reports: Vec::new(),
            handoffs: 0,
            feedback_ends: 0,
            cancels: 0,
        }
    }

    fn play_time(&self) -> Option<i64> {
        self.origin.map(|o| self.clock.now().0 - o.0)
    }

    fn closing(&self) -> Closing {
        Closing {
            arrivals: self.sources.arrivals().to_vec(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: self.sources.axis_out_of_range().collect(),
        }
    }

    /// The tail's views from the completion on.
    fn tail(&self) -> impl Iterator<Item = &Seen> {
        self.seen.iter().filter(|s| s.complete)
    }
}

impl Rig for Tail {
    type Reader = SourceReader;
    type ScreenError = std::convert::Infallible;

    fn now(&self) -> TimeUs {
        self.clock.now()
    }

    fn wait(&mut self) {
        self.clock.advance(TimeUs(MS));
        assert!(
            self.clock.now().0 < CLOCK_START + 60_000_000,
            "the play did not end within a minute"
        );
        if let Some(now) = self.play_time() {
            self.script.poll(TimeUs(now), &mut self.sources);
        }
        // A failed output takes no more steps here; the failure is
        // published to the control side.
        while !self.out.failed && self.out.inner.next_wake() <= self.clock.now() {
            self.out.failed = self.worker.step(&mut self.out).is_err();
        }
        while let Ok(p) = self.probes.pop() {
            assert!(matches!(p.kind, game::Probed::KeyScheduled { .. }));
            self.key_starts += 1;
        }
    }

    fn take(&mut self) -> Option<Command> {
        let now = self.play_time()?;
        let i = self.commands.iter().position(|&(at, _)| at <= now)?;
        Some(self.commands.remove(i).1)
    }

    fn quitting(&self) -> bool {
        self.play_time().is_some_and(|t| t >= self.quit_at)
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        let origin = TimeUs(self.clock.now().0 + lead_in.0);
        let open_at = TimeUs(-lead_in.0);
        self.script.poll(TimeUs(open_at.0 - 1), &mut self.sources);
        self.sources.gate_mut().reset();
        let start = self.sources.open_recording(open_at);
        self.origin = Some(origin);
        let sources = &self.sources;
        self.participants = sources
            .participants()
            .map(|(d, _)| (d, sources.connection(d)))
            .collect();
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: self.sources.participants().map(|(_, k)| k).collect(),
            axes: Vec::new(),
        })
    }

    fn start(&mut self, origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        self.control
            .play_at(origin, sounds, Gain::HEADROOM, None)
            .unwrap();
        self.side = Some(screen);
    }

    fn stepped(&mut self, _aborted: bool) {
        let at = self.play_time().unwrap();
        if let Some(side) = &mut self.side {
            let v = side.view();
            self.seen.push(Seen {
                at,
                complete: v.complete,
                input: v.input,
                tally: v.tally,
                finalized: v.finalized,
                events: v.events,
            });
        }
    }

    fn focus_lost(&self) -> bool {
        self.sources.gate().is_tripped()
    }

    fn connected(&self) -> bool {
        self.participants
            .iter()
            .any(|&(d, c)| c.is_some() && self.sources.connection(d) == c)
    }

    fn audio_failure(&self) -> Option<audio::OutputError> {
        self.control.failure()
    }

    fn screen_failed(&self) -> bool {
        self.play_time().is_some_and(|t| t >= self.screen_fails_at)
    }

    fn input_failure(&self) -> Option<InputWaitError> {
        self.play_time()
            .is_some_and(|t| t >= self.input_fails_at)
            .then_some(InputWaitError::Gone)
    }

    fn cancel_audio(&mut self) {
        self.cancels += 1;
        self.control.cancel();
    }

    fn seal_audio(&mut self) {
        self.completed_at = self.play_time();
        self.control.seal();
    }

    fn ring_out(&mut self, fade_at: TimeUs, end_at: TimeUs) {
        self.completed_at = self.play_time();
        self.control.ring_out(fade_at, end_at);
    }

    fn audio_done(&self) -> bool {
        self.control.done()
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        let closing = self.closing();
        self.sources.close_recording();
        Ok(closing)
    }

    fn hand_off(&mut self) -> Result<Closing, InputWaitError> {
        self.handoffs += 1;
        let closing = self.closing();
        self.sources.hand_off();
        Ok(closing)
    }

    fn end_feedback(&mut self) -> Result<(), InputWaitError> {
        self.feedback_ends += 1;
        self.sources.close_recording();
        Ok(())
    }

    fn ended(&mut self, report: &PlayReport) {
        self.reports.push(report.clone());
    }

    fn show_end(&mut self, _report: &PlayReport, _screen: &ResultScreen) {}
}

/// A steady simulated device whose writes fail from the wake at
/// `fail_from` on (the rig's clock).
struct Failing {
    inner: SimDevice,
    last: TimeUs,
    fail_from: TimeUs,
    /// A write failed: the audio thread takes no more steps.
    failed: bool,
}

impl audio::AudioOutput for Failing {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }
    fn period(&self) -> u32 {
        self.inner.period()
    }
    fn wait(&mut self) -> audio::Wake {
        let w = self.inner.wait();
        self.last = w.at;
        w
    }
    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), audio::OutputError> {
        if self.last >= self.fail_from {
            return Err(audio::OutputError::DeviceLost);
        }
        self.inner.write(f)
    }
}

/// One play of `items` with `commands`, the window closing at `quit_at`.
fn played(items: Vec<ScriptItem>, commands: &[(i64, Command)], quit_at: i64) -> (Tail, PlayReport) {
    played_with(items, commands, quit_at, |_| {})
}

/// [`played`] on a rig `setup` changes first.
fn played_with(
    items: Vec<ScriptItem>,
    commands: &[(i64, Command)],
    quit_at: i64,
    setup: impl FnOnce(&mut Tail),
) -> (Tail, PlayReport) {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let mut rig = Tail::new(items, commands, quit_at);
    setup(&mut rig);
    let report = game::live::play(&mut loaded, &mut rig).unwrap().unwrap();
    (rig, report)
}

fn input(held: bool, pressed: Option<i64>, released: Option<i64>) -> LaneInput {
    LaneInput {
        held,
        pressed: pressed.map(|ms| TimeUs(ms * MS - OFFSET)),
        released: released.map(|ms| TimeUs(ms * MS - OFFSET)),
        presses: u32::from(pressed.is_some()),
    }
}

/// While the BGM keeps the tail going, key 1 tapped twice sounds twice
/// (its latest note's sound), key 2's soundless note and key 3's empty
/// lane stay silent, and each lights its lane at its evaluation time;
/// the record, the live stream, the totals and the finalized watermark
/// equal those of the same play with no tail presses, and the tail ends
/// once the audio has ended, the presses' sounds included.
#[test]
fn tail_presses_sound_and_light_without_changing_the_play() {
    let (quiet, quiet_report) = played(hits(), &[], i64::MAX);
    let mut items = hits();
    items.extend(tap(2_000, 1));
    items.extend(tap(2_500, 1));
    items.extend(tap(2_600, 2));
    items.push(key(2_700, 3, 1));
    let (rig, report) = played(items, &[], i64::MAX);

    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!((report.tail_cut, report.tail_skipped), (None, false));
    assert_eq!(report.record, quiet_report.record);
    assert_eq!(report.live, quiet_report.live);
    assert_eq!(report.tally, quiet_report.tally);
    assert_eq!((rig.handoffs, rig.feedback_ends), (1, 1));
    // The two hits' sounds, then the two tail taps'.
    assert_eq!(quiet.key_starts, 2);
    assert_eq!(rig.key_starts, 4);
    let frozen = rig.tail().next().unwrap().clone();
    assert!(frozen.at < 2_000 * MS, "{frozen:?}");
    for s in rig.tail() {
        assert_eq!(
            (s.tally, s.finalized, s.events),
            (frozen.tally, frozen.finalized, frozen.events)
        );
    }
    let last = &rig.tail().last().unwrap().input;
    assert_eq!(last.lane(lane(1)), input(false, Some(2_500), Some(2_520)));
    assert_eq!(last.lane(lane(2)), input(false, Some(2_600), Some(2_620)));
    assert_eq!(last.lane(lane(3)), input(true, Some(2_700), None));
    // Lit while held: the step after the press shows it.
    let at = |ms: i64| rig.tail().find(|s| s.at >= ms * MS).unwrap().input;
    assert_eq!(
        at(2_001).lane(lane(1)),
        input(true, Some(2_000), Some(1_120))
    );
    // The tail outlasted the BGM (3 s from 1.0 s) by no more than a few
    // periods.
    let end = rig.tail().last().unwrap().at;
    assert!((3_900 * MS..4_100 * MS).contains(&end), "{end}");
    assert_eq!(rig.cancels, 0);
}

/// Every way out of the tail ends the feedback before anything else: a
/// press received with the exit, in the step that takes it, is never
/// converted and never sounds, the sources close once, and a cut stops
/// the audio.
#[test]
fn every_tail_exit_ends_the_feedback_first() {
    let gone = Action::Disconnect(KEYBOARD);
    type Exit = (Vec<ScriptItem>, Vec<(i64, Command)>, i64);
    let at = 2_200 * MS;
    let exits: [(&str, Exit); 9] = [
        ("retry", (Vec::new(), vec![(at, Command::Retry)], i64::MAX)),
        ("skip", (Vec::new(), vec![(at, Command::Quit)], i64::MAX)),
        ("close", (Vec::new(), Vec::new(), at)),
        (
            "focus",
            (
                vec![item(at - 500, Action::FocusLoss)],
                Vec::new(),
                i64::MAX,
            ),
        ),
        ("device", (vec![item(at - 500, gone)], Vec::new(), i64::MAX)),
        (
            "suspend",
            (Vec::new(), vec![(at, Command::Suspend)], i64::MAX),
        ),
        ("input", (Vec::new(), Vec::new(), i64::MAX)),
        ("audio", (Vec::new(), Vec::new(), i64::MAX)),
        ("screen", (Vec::new(), Vec::new(), i64::MAX)),
    ];
    // The play's chart time 0 on the rig's clock: its 1 s READY from the
    // clock's start.
    let origin = CLOCK_START + 1_000 * MS;
    for (name, (extra, commands, quit_at)) in exits {
        let mut items = hits();
        items.extend(tap(2_000, 1));
        items.extend(extra);
        // Received half a millisecond before the step that takes the
        // exit, which its wait feeds with it.
        items.push(item(
            at - 500,
            Action::Packet(Packet {
                device: KEYBOARD,
                control: code(1),
                value: 1,
                background: false,
            }),
        ));
        let (mut rig, report) = played_with(items, &commands, quit_at, |rig| match name {
            "input" => rig.input_fails_at = at,
            // A wake falls there, every 10 ms from the clock's start.
            "audio" => rig.out.fail_from = TimeUs(origin + at),
            "screen" => rig.screen_fails_at = at,
            _ => {}
        });
        assert_eq!(report.end, PlayEnd::Completed, "{name}");
        // The view the screen keeps once the tail is over.
        let kept = rig.side.as_mut().unwrap().view().input;
        assert_eq!(
            kept.lane(lane(1)),
            input(false, Some(2_000), Some(2_020)),
            "{name}"
        );
        let cut = match name {
            "retry" => Some(AbortCause::Retry),
            "close" => Some(AbortCause::Quit),
            "focus" => Some(AbortCause::FocusLoss),
            "device" => Some(AbortCause::LastSourceRemoved),
            "suspend" => Some(AbortCause::Suspend),
            "input" => Some(AbortCause::InputFailed),
            "audio" => Some(AbortCause::OutputFailed),
            "screen" => Some(AbortCause::ScreenFailed),
            _ => None,
        };
        assert_eq!(report.tail_cut, cut, "{name}");
        assert_eq!(report.tail_skipped, name == "skip", "{name}");
        // The hits and the tap at 2.0 s; never the one at 2.2 s.
        assert_eq!(rig.key_starts, 3, "{name}");
        let last = rig.tail().last().unwrap();
        assert!(last.at < 2_200 * MS, "{name}: {last:?}");
        assert_eq!(
            last.input.lane(lane(1)),
            input(false, Some(2_000), Some(2_020)),
            "{name}"
        );
        assert_eq!((rig.handoffs, rig.feedback_ends), (1, 1), "{name}");
        assert_eq!(rig.cancels, 1, "{name}");
        assert!(!rig.sources.is_recording(), "{name}");
    }
}

/// The audio thread reports the sealed audio ended at the step where the
/// BGM's last block is mixed; a press converted right after it, in the
/// same tail step, makes that report stale: the tail goes on until the
/// press's sound was taken and has played, instead of ending with it
/// accepted and never heard.
#[test]
fn a_stale_audio_report_does_not_end_the_tail() {
    let (quiet, _) = played(hits(), &[], i64::MAX);
    let ended = quiet.tail().last().unwrap().at;
    // Key 1 pressed half a millisecond before that step: its wait feeds
    // the press after the report, and the step converts it.
    let mut items = hits();
    items.push(item(
        ended - 500,
        Action::Packet(Packet {
            device: KEYBOARD,
            control: code(1),
            value: 1,
            background: false,
        }),
    ));
    let (rig, report) = played(items, &[], i64::MAX);
    assert_eq!((report.tail_cut, report.tail_skipped), (None, false));
    assert_eq!(
        rig.tail()
            .find(|s| s.at == ended)
            .unwrap()
            .input
            .lane(lane(1)),
        LaneInput {
            held: true,
            pressed: Some(TimeUs(ended - 500 - OFFSET)),
            released: Some(TimeUs(1_120 * MS - OFFSET)),
            presses: 1,
        }
    );
    assert_eq!(rig.key_starts, quiet.key_starts + 1);
    let end = rig.tail().last().unwrap().at;
    assert!(end > ended, "{end} {ended}");
}

/// One practice round of key 1's section with `items`.
fn round(items: Vec<ScriptItem>) -> (Tail, PlayReport) {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    let gauges = loaded.preset().gauges.iter().map(|g| g.start).collect();
    loaded
        .set_practice(Some(Practice {
            start: TimeUs(900 * MS),
            end: TimeUs(1_200 * MS),
            lead_in: TimeUs(500 * MS),
            initial_gauges: gauges,
            repeat: false,
        }))
        .unwrap();
    let mut rig = Tail::new(items, &[], i64::MAX);
    let report = game::live::play(&mut loaded, &mut rig).unwrap().unwrap();
    (rig, report)
}

/// A practice round's tail rings out: a press during the ring-out sounds
/// under its fade, one just before its end, whose sound would start past
/// the end on the output, is converted but silent, and the round ends
/// once the play's time has reached the end and the audio is done.
#[test]
fn a_rounds_ring_out_keeps_its_limits() {
    let (quiet, _) = round(hits());
    let end_at = quiet.completed_at.unwrap() + game::RING_OUT.0;
    let late = (end_at - 5 * MS) / MS;
    let mut items = hits();
    items.extend(tap(1_500, 1));
    items.push(key(late, 1, 1));
    let (rig, report) = round(items);
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(report.tail_cut, None);
    assert_eq!(rig.completed_at, quiet.completed_at);
    // The hits and the press at 1.5 s; the late one is past the end.
    assert_eq!(rig.key_starts, quiet.key_starts + 1);
    let last = rig.tail().last().unwrap();
    assert_eq!(
        last.input.lane(lane(1)),
        input(true, Some(late), Some(1_520))
    );
    assert!(
        last.at >= end_at && last.at < end_at + 50 * MS,
        "{last:?} {end_at}"
    );
}
