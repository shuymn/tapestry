//! The audio tail's feedback and a full play's ending through the play
//! lifecycle up to its result (design.md "Play Lifecycle", the audio
//! tail), on a rig with the real audio thread's loop (`AudioControl` and
//! `AudioWorker` on a simulated output, stepped as its buffer events fall
//! due) and the real source set, on a virtual clock, run by
//! [`game::live::run_chart`] so each play's result is shown: while the BGM
//! keeps the tail going, fresh presses sound and light their lanes without
//! changing the record, the live stream or the totals; a full play holds
//! until its audio is done and its play time has reached its finish
//! anchor plus 5 s and the skin's finish margin, then fades out for the
//! skin's fadeout, converting nothing, before its result; every way out of
//! the hold or the fade-out ends the feedback first, so a press after it
//! neither sounds nor lights; the audio thread's report that the audio
//! ended counts only when it names every key sound the queue accepted; and
//! a practice round's ring-out keeps its limits, with no hold and no
//! fade-out.
//!
//! State × event table (each cell pinned by the test named):
//!
//! | State \ event | fresh press | release / scratch | staged, input offset | audio done | stale done | rearm |
//! |---|---|---|---|---|---|---|
//! | Playing | judged (existing) | judged (existing) | merged by the watermark (existing) | — | — | fresh opening (`rearm_alloc`) |
//! | Completion step | ≤ watermark judged; beyond it staged | as press | staged kept, record holds it (`tail_feedback`) | — | — | — |
//! | Audio tail | sounds, lights, unjudged (`tail_presses_sound_and_light`) | lane off, deadline at its time (`tail_feedback`) | converted first, evaluation time (`tail_feedback`) | ends the tail (`tail_presses_sound_and_light`) | ignored until consumed (`a_stale_audio_report_does_not_end_the_tail`) | none: committed |
//! | Fade-out | not converted, silent (`the_fade_out_converts_no_press`) | not converted | not converted | — | — | — |
//! | Exit: ran out, retry, skip, close, suspend, focus loss, device loss, input, audio or screen failure, in the hold or the fade-out | not converted, silent (`every_ending_exit_ends_the_feedback_first`) | not converted | dropped with the sources | — | — | retry: next attempt's fresh opening |
//! | Exit: practice ring-out | before its end: faded; after: silent (`a_rounds_ring_out_keeps_its_limits`) | — | — | with the play time at its end | as above | — |

mod common;

use std::path::Path;

use audio::{AudioEngine, Gain, MAX_VOICES, SimConfig, SimDevice, Sound, SoundBank};
use bms_model::{Chart, ChartBuilder, SoundId, TimeSource, TimeUs, VirtualClock};
use common::{MS, PERIOD, RATE, lane, tapestry_settings};
use eval::{ControlId, InputFacts, LaneInput, Tally};
use game::{
    AbortCause, AudioControl, AudioProbe, AudioQueue, AudioWorker, ChartIngest, Closing, Command,
    DataRoot, DeviceSettings, FadeEnd, IngestError, Ingested, InputWaitError, Loaded, Opening,
    Parts, PlayEnd, PlayEnding, PlayReport, Practice, RenderSide, ResultScreen, Rig, SessionConfig,
    ViewStatus, ViewSummary, audio_control,
};
use input::{
    Action, AutoplayInput, DeviceId, DeviceKind, InputAdapter, Packet, ScriptItem, ScriptedInput,
    SourceReader, SourceSet,
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
            profile: bms_model::ChartProfile::of(&self.0, &[]),
            chart: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

/// Key 1's taps at 1.0 s (sound 1) and 1.1 s (sound 2), key 2's tap at
/// 1.05 s without a sound, and a BGM sound at 1.0 s (sound 0), [`BGM`]
/// long by default: the play completes at 1.28 s, its last note ends at
/// [`T`], and its hold lasts to its audio's end at about 7 s, past the
/// 6.1 s its finish anchor gives.
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

/// The chart's last note's end, its finish anchor: chart time 1.1 s.
const T: i64 = 1_100 * MS;
/// The BGM's default length (ms).
const BGM: i64 = 6_000;

/// The BGM `bgm_ms` long, each key sound 10 ms.
fn bank(bgm_ms: i64) -> std::sync::Arc<SoundBank> {
    let key = || Sound::mono(vec![500; 480]);
    let bgm = usize::try_from(bgm_ms).unwrap() * RATE as usize / 1_000;
    std::sync::Arc::new(SoundBank::from_sounds(
        RATE,
        vec![Sound::mono(vec![900; bgm]), key(), key()],
    ))
}

fn loaded(dir: &Path) -> Loaded {
    loaded_with(dir, chart(), None)
}

/// A session of `chart`, autoplayed with `autoplay`.
fn loaded_with(dir: &Path, chart: Chart, autoplay: Option<game::AutoKeySounds>) -> Loaded {
    loaded_on(dir, chart, autoplay, None)
}

/// A session of [`chart`] whose records go to [`NotingSink`], noting in
/// `order`.
fn loaded_noting(dir: &Path, order: &Order) -> Loaded {
    loaded_sinking(dir, NotingSink::new(dir, order))
}

/// A session of [`chart`] whose records go to `sink`.
fn loaded_sinking(dir: &Path, sink: NotingSink) -> Loaded {
    loaded_on(dir, chart(), None, Some(Box::new(sink)))
}

/// [`loaded_with`], its records going to `records` when given.
fn loaded_on(
    dir: &Path,
    chart: Chart,
    autoplay: Option<game::AutoKeySounds>,
    records: Option<Box<dyn game::RecordSink>>,
) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.device.input_offset_us = OFFSET;
    let chart_path = dir.join("play.chart");
    std::fs::write(&chart_path, b"built in code").unwrap();
    // An autoplay session saves nothing, as the player makes it.
    let base = if autoplay.is_some() {
        Parts::unsaved(&settings.player)
    } else {
        Parts::standard(&root, &settings.player)
    };
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        records: records.or(base.records),
        ..base
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
        autoplay,
    };
    Loaded::load(root, settings, &chart_path, config, parts).unwrap()
}

/// Where a rig and its record sink note the lifecycle's steps, in order.
type Order = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;

/// A record sink that saves locally, noting in `order` when it saves, when
/// it counts a play and when it is asked for the totals. Its save takes
/// `save_for` on `clock`, if given, and fails when `save_fails`; it counts
/// each play into its totals' plays and answers `count` for it, and the
/// totals unless `no_totals`.
struct NotingSink {
    local: game::LocalRecords,
    order: Order,
    clock: Option<std::sync::Arc<VirtualClock>>,
    save_for: TimeUs,
    save_fails: bool,
    count: Option<game::BestView>,
    no_totals: bool,
    plays: u64,
}

impl NotingSink {
    /// A sink saving under `dir`, noting in `order`, that saves at once,
    /// counts each play as its first and answers the totals.
    fn new(dir: &Path, order: &Order) -> Self {
        Self {
            local: game::LocalRecords::new(DataRoot::new(dir.to_owned()).records()),
            order: std::sync::Arc::clone(order),
            clock: None,
            save_for: TimeUs(0),
            save_fails: false,
            count: Some(game::BestView::Counted {
                play: (1, 0),
                previous: None,
                current: game::BestScore::default(),
                ineligible: None,
            }),
            no_totals: false,
            plays: 0,
        }
    }
}

impl game::RecordSink for NotingSink {
    fn save(&mut self, rec: &replay::PlayRecord) -> Result<game::RecordRef, game::SaveError> {
        self.order.lock().unwrap().push("saved");
        if let Some(clock) = &self.clock {
            clock.advance(self.save_for);
        }
        if self.save_fails {
            return Err(game::SaveError::from(std::io::Error::other("held")));
        }
        self.local.save(rec)
    }

    fn after_result(&mut self, _: &game::SavedPlay<'_>) -> Option<game::BestView> {
        self.order.lock().unwrap().push("counted");
        self.plays += 1;
        self.count.clone()
    }

    fn totals(&mut self) -> Option<game::PlayerTotals> {
        self.order.lock().unwrap().push("totals");
        (!self.no_totals).then_some(game::PlayerTotals {
            plays: self.plays,
            ..game::PlayerTotals::default()
        })
    }
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

/// What one step's view showed.
#[derive(Clone, Debug)]
struct Seen {
    /// The play it belongs to, from 1.
    play: usize,
    /// The play time of the step.
    at: i64,
    complete: bool,
    fading: Option<TimeUs>,
    input: InputFacts,
    tally: Tally,
    finalized: Option<TimeUs>,
    events: u64,
    /// The judgment bins' bin 1 (the chart's second 1, all its notes)
    /// and whether they are available.
    bins: ([u32; game::JUDGE_CHANNELS], bool),
}

/// The game's threads on a virtual clock: each wait moves it 1 ms, feeds
/// the script's batches up to the play's time, and runs the audio
/// thread's steps that fall due.
struct Tail {
    clock: std::sync::Arc<VirtualClock>,
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
    /// The play skin's declared ending and Closing.
    ending: PlayEnding,
    close: Option<TimeUs>,
    /// Plays opened.
    plays: usize,
    /// Each end screen shown: its play time, the report and the summary.
    /// One made ready in a fade phase shows at the phase's end.
    shown: Vec<(i64, PlayReport, Option<ViewSummary>)>,
    /// A fade phase is on ([`Rig::fade_phase`] to [`Rig::fade_ended`]).
    in_fade: bool,
    /// The end screen made ready in the fade phase, shown at its end.
    held_show: Option<(PlayReport, Option<ViewSummary>)>,
    /// How each play's fade phase ended ([`Rig::fade_ended`]), by play.
    fade_ends: Vec<(usize, FadeEnd)>,
    /// The fade phase's, the play's and the result's steps, in order.
    order: Order,
    /// The play times each fade phase's deadline moved to.
    skipped: Vec<i64>,
    /// The play time of each fade phase's end ([`Rig::fade_ended`]).
    fade_ended_at: Vec<i64>,
    /// The play time each fade phase's result took the input over from.
    takeovers: Vec<i64>,
    /// How far past its deadline a fade phase's wait wakes, in µs (a
    /// descheduled game thread); zero: the next millisecond.
    wake_late: i64,
    /// A fade phase's waits hold the game thread to this play time (a
    /// game thread descheduled while commands queue); zero: no hold.
    look_after: i64,
    /// How long the turntable's pause after a save holds the game thread,
    /// in µs (a descheduled game thread before the flow goes on).
    pause_after_save: i64,
    /// The totals each result's own view read, as its layout is asked for.
    totals: Vec<Option<Option<game::PlayerTotals>>>,
    /// An autoplay session's input side, polled on the play's timeline in
    /// place of the script, with the controls it records.
    feeder: Option<(AutoplayInput, Vec<ControlId>)>,
    /// The play time of the first wait after the seal at which the audio
    /// thread reported the audio done.
    audio_done_at: Option<i64>,
    /// READY states opened ([`Rig::ready_began`]) and plays admitted to
    /// Closing ([`Rig::closing_began`]).
    readies: u32,
    closings: u32,
    /// The full play's fade-out as [`Rig::fade_began`] gave it: its start
    /// and its end on the clock (development builds).
    #[cfg(feature = "dev-tools")]
    fade_began: Option<(TimeUs, TimeUs)>,
}

impl Tail {
    fn new(items: Vec<ScriptItem>, commands: &[(i64, Command)], quit_at: i64) -> Self {
        Self::with_bgm(items, commands, quit_at, BGM)
    }

    /// [`Tail::new`] with the BGM `bgm_ms` long.
    fn with_bgm(
        items: Vec<ScriptItem>,
        commands: &[(i64, Command)],
        quit_at: i64,
        bgm_ms: i64,
    ) -> Self {
        let mut items = items;
        items.push(item(
            i64::MIN,
            Action::Connect(KEYBOARD, DeviceKind::Keyboard),
        ));
        let recorded = DeviceSettings::default().keyboard.recorded();
        let script = ScriptedInput::new(items).with_recorded(KEYBOARD, recorded);
        let chart = chart();
        let engine =
            AudioEngine::new(bank(bgm_ms), &chart.bgm, RATE, PERIOD, MAX_VOICES, 64).unwrap();
        Self::on(script, commands, quit_at, engine, common::sources())
    }

    /// An autoplay session of `loaded` with the BGM `bgm_ms` long: its
    /// generated input fed on the play's timeline, its merged schedule (the
    /// BGM and autoplay's key sounds) in the audio engine.
    fn autoplay(loaded: &Loaded, bgm_ms: i64) -> Self {
        let made = loaded.autoplay().unwrap();
        let queue_capacity = made.queue_capacity();
        let engine =
            AudioEngine::with_schedule(bank(bgm_ms), made.schedule, RATE, PERIOD, MAX_VOICES, 64)
                .unwrap();
        let sources = input::SourceConfig {
            queue_capacity,
            ..common::sources()
        };
        let mut rig = Self::on(
            ScriptedInput::new(Vec::new()),
            &[],
            i64::MAX,
            engine,
            sources,
        );
        rig.feeder = Some((AutoplayInput::new(made.events), made.recorded));
        rig
    }

    /// A rig whose input comes from `script`, on `engine` and a source set
    /// of `sources`.
    fn on(
        script: ScriptedInput,
        commands: &[(i64, Command)],
        quit_at: i64,
        engine: AudioEngine,
        sources: input::SourceConfig,
    ) -> Self {
        let (control, mut worker) = audio_control(engine);
        let (tx, probes) = rtrb::RingBuffer::new(1_024);
        worker.set_probe(tx);
        Self {
            clock: std::sync::Arc::new(VirtualClock::new(TimeUs(CLOCK_START))),
            sources: SourceSet::new(sources).unwrap(),
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
            ending: PlayEnding::NONE,
            close: None,
            plays: 0,
            shown: Vec::new(),
            in_fade: false,
            held_show: None,
            fade_ends: Vec::new(),
            order: Order::default(),
            skipped: Vec::new(),
            fade_ended_at: Vec::new(),
            takeovers: Vec::new(),
            wake_late: 0,
            look_after: 0,
            pause_after_save: 0,
            totals: Vec::new(),
            feeder: None,
            audio_done_at: None,
            readies: 0,
            closings: 0,
            #[cfg(feature = "dev-tools")]
            fade_began: None,
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

    /// The play's view as the screen reads it now, kept.
    fn sample(&mut self) {
        let at = self.play_time().unwrap();
        if let Some(side) = &mut self.side {
            let v = side.view();
            self.seen.push(Seen {
                play: self.plays,
                at,
                complete: v.complete,
                fading: v.fading,
                input: v.input,
                tally: v.tally,
                finalized: v.finalized,
                events: v.events,
                bins: (v.judge_bins.bins[1], v.judge_bins.available),
            });
        }
    }

    /// The first play's tail views, from the completion on.
    fn tail(&self) -> impl Iterator<Item = &Seen> {
        self.seen.iter().filter(|s| s.play == 1 && s.complete)
    }

    /// Where the first play's views first published a fade-out, if any.
    fn fading(&self) -> Option<TimeUs> {
        self.seen
            .iter()
            .filter(|s| s.play == 1)
            .find_map(|s| s.fading)
    }

    /// How the first play's fade phase ended, if it had one.
    fn fade_end(&self) -> Option<FadeEnd> {
        self.fade_ends
            .iter()
            .find(|&&(play, _)| play == 1)
            .map(|&(_, how)| how)
    }

    /// The play time the first play's result showed at, if it did.
    fn shown_at(&self) -> Option<i64> {
        self.shown
            .iter()
            .find(|(_, r, _)| r.attempt == 1)
            .map(|&(at, _, _)| at)
    }
}

impl Rig for Tail {
    type Reader = SourceReader;
    type ScreenError = std::convert::Infallible;
    type ResultPrep = ();

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
            match &mut self.feeder {
                Some((feeder, _)) => feeder.poll(TimeUs(now), &mut self.sources),
                None => self.script.poll(TimeUs(now), &mut self.sources),
            }
        }
        // A failed output takes no more steps here; the failure is
        // published to the control side.
        while !self.out.failed && self.out.inner.next_wake() <= self.clock.now() {
            self.out.failed = self.worker.step(&mut self.out).is_err();
        }
        if self.completed_at.is_some() && self.audio_done_at.is_none() && self.control.done() {
            self.audio_done_at = self.play_time();
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

    /// A command keeps its receipt, its scripted time, however late it is
    /// taken; one received after `until` stays.
    fn take_received(&mut self, until: TimeUs) -> Option<(Command, TimeUs)> {
        let (origin, now) = (self.origin?, self.play_time()?);
        let i = self
            .commands
            .iter()
            .position(|&(at, _)| at <= now && origin.0 + at <= until.0)?;
        let (at, c) = self.commands.remove(i);
        Some((c, TimeUs(origin.0 + at)))
    }

    /// The next millisecond, or, in a fade phase that wakes late, at once
    /// `wake_late` past `deadline`; in a fade phase, never before
    /// `look_after`.
    fn idle_wait(&mut self, deadline: TimeUs) {
        self.wait();
        while self.in_fade && self.wake_late > 0 && self.clock.now().0 < deadline.0 + self.wake_late
        {
            self.wait();
        }
        while self.in_fade && self.play_time().is_some_and(|t| t < self.look_after) {
            self.wait();
        }
    }

    fn turntable_paused(&mut self) {
        let until = self.clock.now().0 + self.pause_after_save;
        while self.clock.now().0 < until {
            self.wait();
        }
    }

    fn fade_skipped(&mut self, at: TimeUs) {
        let origin = self.origin.unwrap();
        self.skipped.push(at.0 - origin.0);
    }

    fn result_compared(&mut self, _report: &PlayReport, screen: &ResultScreen) {
        self.order.lock().unwrap().push("compared");
        self.totals.push(screen.totals());
    }

    /// The window closes at `quit_at`, or once a result has shown: the
    /// run ends there.
    fn quitting(&self) -> bool {
        !self.shown.is_empty() || self.play_time().is_some_and(|t| t >= self.quit_at)
    }

    fn declared_ending(&self) -> PlayEnding {
        self.ending
    }

    fn declared_close(&self) -> Option<TimeUs> {
        self.close
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        let origin = TimeUs(self.clock.now().0 + lead_in.0);
        let open_at = TimeUs(-lead_in.0);
        match &mut self.feeder {
            Some((feeder, recorded)) => feeder.restart(&mut self.sources, recorded.clone()),
            None => self.script.poll(TimeUs(open_at.0 - 1), &mut self.sources),
        }
        self.sources.gate_mut().reset();
        let start = self.sources.open_recording(open_at);
        self.origin = Some(origin);
        self.plays += 1;
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

    fn ready_began(&mut self) {
        self.readies += 1;
    }

    fn closing_began(&mut self) {
        self.closings += 1;
    }

    #[cfg(feature = "dev-tools")]
    fn fade_began(&mut self, from: TimeUs, to: TimeUs) {
        self.fade_began = Some((from, to));
    }

    fn stepped(&mut self, _aborted: bool) {
        self.sample();
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
        self.order.lock().unwrap().push("ended");
        self.reports.push(report.clone());
    }

    fn play_over(&mut self) {
        self.order.lock().unwrap().push("play over");
    }

    fn show_end(&mut self, report: &PlayReport, screen: &ResultScreen) {
        let summary = match screen.status() {
            ViewStatus::Ready(s) => Some(s.clone()),
            _ => None,
        };
        if self.in_fade {
            self.held_show = Some((report.clone(), summary));
            return;
        }
        self.shown
            .push((self.play_time().unwrap(), report.clone(), summary));
    }

    /// The fade-out's view, published as the phase begins, is what the
    /// screen draws from here: no step follows it.
    fn fade_phase(&mut self, _start: TimeUs, _deadline: TimeUs) {
        self.order.lock().unwrap().push("fade phase");
        self.in_fade = true;
        self.sample();
    }

    /// The result made ready in the phase shows at its end, unless a
    /// retry or a close withdrew it.
    fn fade_ended(&mut self, how: FadeEnd, takeover: TimeUs) {
        self.order.lock().unwrap().push("fade ended");
        self.fade_ended_at.push(self.play_time().unwrap());
        self.takeovers.push(takeover.0 - self.origin.unwrap().0);
        self.in_fade = false;
        self.fade_ends.push((self.plays, how));
        let held = self.held_show.take();
        if matches!(how, FadeEnd::Cut(AbortCause::Retry | AbortCause::Quit)) {
            return;
        }
        if let Some((report, summary)) = held {
            self.shown
                .push((self.play_time().unwrap(), report, summary));
        }
    }
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

/// One play of `items` with `commands`, the window closing at `quit_at`
/// or once its result has shown.
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
    played_on(Tail::new(items, commands, quit_at), setup)
}

/// The chart played on `rig`, which `setup` changes first, until the window
/// closes: the rig and the first play's report.
fn played_on(mut rig: Tail, setup: impl FnOnce(&mut Tail)) -> (Tail, PlayReport) {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded(dir.path());
    setup(&mut rig);
    let run = game::live::run_chart(&mut loaded, &mut rig);
    // An input thread gone ends the run with its error, once the play is
    // over.
    if rig.input_fails_at == i64::MAX {
        run.unwrap();
    } else {
        assert!(matches!(run, Err(game::LiveError::Input(_))), "{run:?}");
    }
    let report = rig.reports[0].clone();
    (rig, report)
}

/// One play of `items` with the BGM `bgm_ms` long and the skin declaring
/// `ending`, until its result shows.
fn ending(items: Vec<ScriptItem>, bgm_ms: i64, ending: PlayEnding) -> (Tail, PlayReport) {
    played_on(Tail::with_bgm(items, &[], i64::MAX, bgm_ms), |rig| {
        rig.ending = ending;
    })
}

/// A declared ending of `finish_margin_ms` and `fadeout_ms`.
const fn declared(finish_margin_ms: i64, fadeout_ms: i64) -> PlayEnding {
    PlayEnding {
        finish_margin: TimeUs(finish_margin_ms * MS),
        fadeout: TimeUs(fadeout_ms * MS),
    }
}

/// Every note hit, key 2's included: a full combo.
fn all_hits() -> Vec<ScriptItem> {
    let mut items = hits();
    items.extend(tap(1_050, 2));
    items
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
        // The judgment bins likewise (design.md "Rendering and Skins"):
        // the three hits counted on the production route, the tail's
        // presses and its idle steps counting nothing.
        assert_eq!(s.bins, frozen.bins);
    }
    let counted: u32 = frozen.bins.0.iter().sum();
    assert!(frozen.bins.1, "the play mapped its notes");
    assert_eq!(counted, 3);
    assert_eq!(
        counted,
        frozen.tally.counts.perfect
            + frozen.tally.counts.great
            + frozen.tally.counts.good
            + frozen.tally.counts.bad
            + frozen.tally.counts.poor
    );
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
    // The tail outlasted the BGM (6 s from 1.0 s), which outlasts the
    // hold's deadline (6.1 s), by no more than a few periods.
    let end = rig.tail().last().unwrap().at;
    assert!((6_900 * MS..7_100 * MS).contains(&end), "{end}");
    assert_eq!(rig.cancels, 0);
}

/// Every way out of a full play's hold or fade-out ends the feedback
/// before anything else: a press received with the exit, in the step that
/// takes it, is never converted and never sounds, the sources close once,
/// and a cut stops the audio. The skin declares a 3 s finish margin and a
/// 1 s fadeout, so the hold lasts to `T + 8 s` and the fade-out to
/// `T + 9 s`; each exit is taken once in the hold (`T + 2.2 s`) and once in
/// the fade-out (`T + 8.5 s`). Each changes nothing the play scored: its
/// record, finalized watermark, live stream, totals, displayed gauge,
/// result and shown summary equal those of the same input run out
/// naturally; only its own diagnostics differ.
#[test]
fn every_ending_exit_ends_the_feedback_first() {
    let gone = Action::Disconnect(KEYBOARD);
    type Exit = (Vec<ScriptItem>, Vec<(i64, Command)>, i64);
    let ending_of = declared(3_000, 1_000);
    for at in [T + 2_200 * MS, T + 8_500 * MS] {
        let phase = if at < T + 8_000 * MS {
            "hold"
        } else {
            "fade-out"
        };
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
        // The play's chart time 0 on the rig's clock: its 1 s READY from
        // the clock's start.
        let origin = CLOCK_START + 1_000 * MS;
        let base = || {
            let mut items = hits();
            items.extend(tap(2_000, 1));
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
            items
        };
        // The same input, run out naturally.
        let (reference, natural) = played_with(base(), &[], i64::MAX, |rig| {
            rig.ending = ending_of;
        });
        assert_eq!(
            (natural.tail_skipped, natural.tail_cut),
            (false, None),
            "{phase}"
        );
        assert_eq!(
            (
                natural.input_error.is_none(),
                natural.output_error.is_none()
            ),
            (true, true),
            "{phase}"
        );
        assert_eq!(reference.shown_at(), Some(T + 9_000 * MS), "{phase}");
        let natural_shown = reference.shown[0].2.clone();
        assert!(natural_shown.is_some(), "{phase}");
        for (name, (extra, commands, quit_at)) in exits {
            let mut items = base();
            items.extend(extra);
            let (mut rig, report) = played_with(items, &commands, quit_at, |rig| {
                rig.ending = ending_of;
                match name {
                    "input" => rig.input_fails_at = at,
                    // A wake falls there, every 10 ms from the clock's start.
                    "audio" => rig.out.fail_from = TimeUs(origin + at),
                    "screen" => rig.screen_fails_at = at,
                    _ => {}
                }
            });
            let what = format!("{phase}, {name}");
            assert_eq!(report.end, PlayEnd::Completed, "{what}");
            // In the fade-out, it was published before the exit.
            let fading = (phase == "fade-out").then_some(TimeUs(T + 8_000 * MS));
            assert_eq!(rig.fading(), fading, "{what}");
            // Nothing the play scored changed.
            // The record whole: its packets, closing and flags.
            assert_eq!(report.record, natural.record, "{what}");
            assert_eq!(report.live, natural.live, "{what}");
            assert_eq!(report.tally, natural.tally, "{what}");
            assert_eq!(report.displayed_gauge, natural.displayed_gauge, "{what}");
            assert_eq!(report.result, natural.result, "{what}");
            let finalized = |r: &Tail| r.tail().last().map(|s| s.finalized);
            assert_eq!(finalized(&rig), finalized(&reference), "{what}");
            // Its own diagnostics: the hold's in the report; the fade
            // phase's at its end, the play saved before it.
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
            let in_hold = phase == "hold";
            let skipped = name == "skip";
            assert_eq!(report.tail_cut, cut.filter(|_| in_hold), "{what}");
            assert_eq!(report.tail_skipped, skipped && in_hold, "{what}");
            let fade_end = if skipped {
                Some(FadeEnd::Skipped)
            } else {
                cut.map(FadeEnd::Cut)
            };
            assert_eq!(rig.fade_end(), fade_end.filter(|_| !in_hold), "{what}");
            assert_eq!(
                report.input_error.is_some(),
                name == "input" && in_hold,
                "{what}"
            );
            assert_eq!(
                report.output_error.is_some(),
                name == "audio" && in_hold,
                "{what}"
            );
            assert_eq!(report.closing, None, "{what}");
            // Its result shows at the exit, the same as the natural one's,
            // but for a retry, which plays again, and a close.
            let shown: Vec<_> = rig
                .shown
                .iter()
                .filter(|(_, r, _)| r.attempt == 1)
                .collect();
            if matches!(name, "retry" | "close") {
                assert!(shown.is_empty(), "{what}");
            } else {
                assert_eq!(shown.len(), 1, "{what}");
                assert_eq!(shown[0].0, at, "{what}");
                assert_eq!(shown[0].2, natural_shown, "{what}");
            }
            // A retry's next play starts with no fade-out.
            if name == "retry" {
                let next = rig.seen.iter().find(|s| s.play == 2).unwrap();
                assert_eq!(next.fading, None, "{what}");
                assert!(!next.complete, "{what}");
            }
            // The view the screen keeps once the tail is over.
            if name != "retry" {
                let kept = rig.side.as_mut().unwrap().view().input;
                assert_eq!(
                    kept.lane(lane(1)),
                    input(false, Some(2_000), Some(2_020)),
                    "{what}"
                );
            }
            // The hits and the tap at 2.0 s; never the press at the exit.
            assert_eq!(rig.key_starts, 3, "{what}");
            let last = rig.tail().last().unwrap();
            assert!(last.at < at, "{what}: {last:?}");
            assert_eq!(
                last.input.lane(lane(1)),
                input(false, Some(2_000), Some(2_020)),
                "{what}"
            );
            let plays = if name == "retry" { 2 } else { 1 };
            assert_eq!((rig.handoffs, rig.feedback_ends), (plays, plays), "{what}");
            assert_eq!(rig.cancels, 1, "{what}");
            assert!(!rig.sources.is_recording(), "{what}");
        }
    }
}

/// A full play with a full combo and its audio done at `T + 0.4 s` holds
/// to its finish anchor plus 5 s and the skin's 3 s finish margin, then
/// publishes its fade-out there and shows its result after the skin's 1 s
/// fadeout.
#[test]
fn the_result_waits_for_the_finish_margin_and_the_fade_out() {
    let (rig, report) = ending(all_hits(), 500, declared(3_000, 1_000));
    assert_eq!(rig.fading(), Some(TimeUs(T + 8_000 * MS)));
    assert_eq!(rig.shown_at(), Some(T + 9_000 * MS));
    assert!(
        rig.seen
            .iter()
            .filter(|s| s.at < T + 8_000 * MS)
            .all(|s| s.fading.is_none())
    );
    // From its start, every view publishes it.
    assert!(
        rig.tail()
            .filter(|s| s.at > T + 8_000 * MS)
            .all(|s| s.fading == Some(TimeUs(T + 8_000 * MS)))
    );
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(report.tally.combo, report.tally.max_combo);
    assert!(!report.tail_skipped);
    assert_eq!(rig.cancels, 0);
}

/// A full play's fade phase saves and counts the play and reads its
/// result's totals at the phase's start, before its own view's layout is
/// asked for; the play is over and reported at the phase's end, after the
/// save (design.md "State Model", ResultPreparing).
#[test]
fn the_fade_phase_saves_at_its_start_and_ends_the_play_at_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let order = Order::default();
    let mut loaded = loaded_noting(dir.path(), &order);
    let mut rig = Tail::with_bgm(all_hits(), &[], i64::MAX, 500);
    rig.ending = declared(3_000, 1_000);
    rig.order = std::sync::Arc::clone(&order);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    assert_eq!(
        *order.lock().unwrap(),
        [
            "fade phase",
            "saved",
            "counted",
            "totals",
            "compared",
            "play over",
            "ended",
            "fade ended"
        ]
    );
    assert_eq!(rig.fade_end(), Some(FadeEnd::RanOut));
    assert_eq!(rig.shown_at(), Some(T + 9_000 * MS));
}

/// The fade-out's start and its end, the start plus the skin's fadeout,
/// reach the rig on its clock when the fade-out begins (development
/// builds: a run record's mark).
#[cfg(feature = "dev-tools")]
#[test]
fn the_fade_out_s_span_reaches_the_rig() {
    let (rig, _) = ending(all_hits(), 500, declared(3_000, 1_000));
    let origin = rig.origin.unwrap().0;
    assert_eq!(
        rig.fade_began
            .map(|(from, to)| (from.0 - origin, to.0 - origin)),
        Some((T + 8_000 * MS, T + 9_000 * MS))
    );
}

/// Audio still sounding at the hold's deadline delays the fade-out until
/// it is done; the result follows the fadeout after that.
#[test]
fn a_long_audio_delays_the_fade_out() {
    // The BGM from 1.0 s to `T + 9.5 s`: the audio side reports it done
    // within a few periods of that, as it mixes ahead.
    let (rig, report) = ending(all_hits(), 9_600, declared(3_000, 1_000));
    let held = rig.fading().unwrap().0;
    assert!((T + 9_450 * MS..T + 9_550 * MS).contains(&held), "{held}");
    assert_eq!(rig.shown_at(), Some(held + 1_000 * MS));
    assert_eq!(report.tail_cut, None);
}

/// The fade-out converts no input: a press in the hold sounds and lights
/// its lane, one in the fade-out does neither.
#[test]
fn the_fade_out_converts_no_press() {
    let mut items = all_hits();
    items.extend(tap((T + 7_000 * MS) / MS, 1));
    items.extend(tap((T + 8_500 * MS) / MS, 1));
    let (quiet, _) = ending(all_hits(), 500, declared(3_000, 1_000));
    let (rig, report) = ending(items, 500, declared(3_000, 1_000));
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(rig.key_starts, quiet.key_starts + 1);
    let last = rig.tail().last().unwrap();
    assert!(last.fading.is_some(), "{last:?}");
    let hold = (T + 7_000 * MS) / MS;
    assert_eq!(
        last.input.lane(lane(1)),
        input(false, Some(hold), Some(hold + 20))
    );
    assert_eq!(rig.shown_at(), Some(T + 9_000 * MS));
}

/// With no declaration a full play holds 5 s past its last note, not a
/// round's 1.5 s nor until its audio's end, and shows no fade-out.
#[test]
fn a_full_play_without_a_declaration_holds_five_seconds() {
    let (mut rig, _) = ending(all_hits(), 500, PlayEnding::NONE);
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
    // Nor the view kept after the tail: no fade-out was ever published.
    assert_eq!(rig.side.as_mut().unwrap().view().fading, None);
    assert_eq!(rig.shown_at(), Some(T + 5_000 * MS));
}

/// A negative fadeout is read as 0, and a fadeout of 0 has no fade-out:
/// the play holds its finish margin and shows the result at the hold's
/// end, with no fade phase.
#[test]
fn a_negative_fadeout_keeps_the_finish_margin() {
    for fadeout in [-1, 0] {
        let (mut rig, _) = ending(all_hits(), 500, declared(3_000, fadeout));
        assert!(rig.seen.iter().all(|s| s.fading.is_none()), "{fadeout}");
        // Nor the view kept after the tail: no fade-out was ever published.
        assert_eq!(rig.side.as_mut().unwrap().view().fading, None, "{fadeout}");
        assert_eq!(rig.shown_at(), Some(T + 8_000 * MS), "{fadeout}");
        assert_eq!(rig.fade_end(), None, "{fadeout}");
    }
}

/// A negative finish margin is read as 0: the fade-out starts 5 s past
/// the anchor and the result follows the fadeout after it.
#[test]
fn a_negative_finish_margin_holds_as_none() {
    let (rig, _) = ending(all_hits(), 500, declared(-3_000, 1_000));
    assert_eq!(rig.fading(), Some(TimeUs(T + 5_000 * MS)));
    assert_eq!(rig.shown_at(), Some(T + 6_000 * MS));
}

/// A declaration past 10 s is honoured whole, and the quit key still shows
/// the result at once within it.
#[test]
fn a_long_declaration_is_honoured() {
    let long = declared(12_000, 11_000);
    let (rig, _) = ending(all_hits(), 500, long);
    assert_eq!(rig.fading(), Some(TimeUs(T + 17_000 * MS)));
    assert_eq!(rig.shown_at(), Some(T + 28_000 * MS));
    let quit = T + 20_000 * MS;
    let (rig, report) = played_on(
        Tail::with_bgm(all_hits(), &[(quit, Command::Quit)], i64::MAX, 500),
        |rig| rig.ending = long,
    );
    assert_eq!(rig.fade_end(), Some(FadeEnd::Skipped));
    assert!(!report.tail_skipped);
    assert_eq!(rig.shown_at(), Some(quit));
}

/// The quit key in the hold shows the result at once, stopping the audio,
/// with no fade-out.
#[test]
fn a_quit_in_the_hold_shows_the_result_at_once() {
    let quit = T + 2_000 * MS;
    let (rig, report) = played_on(
        Tail::with_bgm(all_hits(), &[(quit, Command::Quit)], i64::MAX, 500),
        |rig| rig.ending = declared(3_000, 1_000),
    );
    assert_eq!(rig.shown_at(), Some(quit));
    assert!(report.tail_skipped);
    assert_eq!(rig.cancels, 1);
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
    // C26-2: the hold is no Closing.
    assert_eq!((rig.readies, rig.closings), (1, 0));
}

/// The quit key during the fade-out shows the result at once, without
/// playing the rest of it.
#[test]
fn a_quit_during_the_fade_out_shows_the_result_at_once() {
    let quit = T + 8_500 * MS;
    let (rig, report) = played_on(
        Tail::with_bgm(all_hits(), &[(quit, Command::Quit)], i64::MAX, 500),
        |rig| rig.ending = declared(3_000, 1_000),
    );
    assert_eq!(rig.fading(), Some(TimeUs(T + 8_000 * MS)));
    assert_eq!(rig.shown_at(), Some(quit));
    // Skipped in the fade phase, the play saved before it.
    assert_eq!(rig.fade_end(), Some(FadeEnd::Skipped));
    assert!(!report.tail_skipped);
    assert_eq!(rig.cancels, 1);
    // C26-2: nor is the fade-out.
    assert_eq!((rig.readies, rig.closings), (1, 0));
}

/// A quit and a retry received in the fade phase after the save, both
/// pending when the game looks: the quit skips to the result, its receipt
/// the takeover, and the phase takes no retry (design.md "State Model",
/// ResultPreparing). The result shows, and the play is saved and counted
/// once.
#[test]
fn a_retry_pending_with_a_skip_retries_nothing() {
    let quit = T + 8_200 * MS;
    let retry = quit + 10 * MS;
    let commands = [(quit, Command::Quit), (retry, Command::Retry)];
    let mut rig = Tail::with_bgm(all_hits(), &commands, i64::MAX, 500);
    rig.ending = declared(3_000, 500);
    rig.look_after = retry + 10 * MS;
    let (rig, report) = played_on(rig, |_| {});
    assert_eq!(rig.fading(), Some(TimeUs(T + 8_000 * MS)));
    assert_eq!(rig.fade_end(), Some(FadeEnd::Skipped));
    assert_eq!(rig.skipped, vec![quit]);
    assert_eq!(rig.takeovers, vec![quit]);
    assert!(rig.shown_at().is_some_and(|at| at >= retry));
    assert_eq!((rig.plays, rig.reports.len()), (1, 1));
    assert!(!report.tail_skipped);
}

/// The chart played with every note hit, the skin declaring `ending`, with
/// `commands`, its records going to a [`NotingSink`] on the rig's clock
/// that `setup` changes first, until the window closes: the rig and the
/// steps noted.
fn sunk(
    ending: PlayEnding,
    commands: &[(i64, Command)],
    setup: impl FnOnce(&mut NotingSink),
) -> (Tail, Order) {
    let dir = tempfile::tempdir().unwrap();
    let order = Order::default();
    let mut rig = Tail::with_bgm(all_hits(), commands, i64::MAX, 500);
    rig.ending = ending;
    rig.order = std::sync::Arc::clone(&order);
    let mut sink = NotingSink::new(dir.path(), &order);
    sink.clock = Some(std::sync::Arc::clone(&rig.clock));
    setup(&mut sink);
    let mut loaded = loaded_sinking(dir.path(), sink);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    (rig, order)
}

/// A save that outlasts its fade phase (design.md "State Model",
/// ResultPreparing): a quit received before the fade's deadline and one
/// received after it, both while the play is saved, belong to the ending
/// play. Each skips to the result, shown once the save has ended, never
/// leaving it; the first moves the deadline to its receipt. The result
/// takes the input over at the save's end, the later of the two ends.
#[test]
fn a_save_past_the_fade_takes_its_commands_as_the_plays() {
    let save_end = T + 8_600 * MS;
    for (quit, moved) in [
        (T + 8_490 * MS, vec![T + 8_490 * MS]),
        (T + 8_510 * MS, vec![]),
    ] {
        let (rig, _) = sunk(declared(3_000, 500), &[(quit, Command::Quit)], |s| {
            s.save_for = TimeUs(600 * MS);
        });
        assert_eq!(rig.fading(), Some(TimeUs(T + 8_000 * MS)), "{quit}");
        assert_eq!(rig.fade_end(), Some(FadeEnd::Skipped), "{quit}");
        assert_eq!(rig.skipped, moved, "{quit}");
        assert_eq!(rig.plays, 1, "{quit}");
        let shown = rig.shown_at().expect("the result shown");
        assert!(shown >= save_end, "{quit}: shown at {shown}");
        assert!(
            rig.fade_ended_at.first().is_some_and(|&at| at >= save_end),
            "{quit}: the takeover at {:?}",
            rig.fade_ended_at
        );
    }
}

/// A quit received after the fade phase's deadline, the save ended before
/// it, is the result's however late the game looks (design.md "State
/// Model", ResultPreparing): the phase runs out at its deadline, where the
/// result takes the input over, and the quit stays pending for the
/// result. One received before the deadline is the phase's skip.
#[test]
fn a_quit_past_the_fade_deadline_is_the_results_however_late_the_look() {
    let deadline = T + 8_500 * MS;
    for (quit, end, moved) in [
        (T + 8_505 * MS, FadeEnd::RanOut, vec![]),
        (T + 8_495 * MS, FadeEnd::Skipped, vec![T + 8_495 * MS]),
    ] {
        let commands = [(quit, Command::Quit)];
        let mut rig = Tail::with_bgm(all_hits(), &commands, i64::MAX, 500);
        rig.ending = declared(3_000, 500);
        rig.wake_late = 10 * MS;
        let (rig, _) = played_on(rig, |_| {});
        assert_eq!(rig.fading(), Some(TimeUs(T + 8_000 * MS)), "{quit}");
        assert_eq!(rig.fade_end(), Some(end), "{quit}");
        assert_eq!(rig.skipped, moved, "{quit}");
        assert!(
            rig.fade_ended_at
                .first()
                .is_some_and(|&at| at >= deadline + 10 * MS),
            "{quit}: the look at {:?}",
            rig.fade_ended_at
        );
        let takeover = moved.first().copied().unwrap_or(deadline);
        assert_eq!(rig.takeovers.first(), Some(&takeover), "{quit}");
        let pending: Vec<_> = rig.commands.iter().map(|&(at, _)| at).collect();
        match end {
            FadeEnd::RanOut => assert_eq!(pending, [quit], "the result's quit"),
            _ => assert!(pending.is_empty(), "{quit}: {pending:?}"),
        }
    }
}

/// The result takes the input over from the save's end, read as the
/// save returned: a game thread held after it, before the flow goes on,
/// moves the takeover no later, so a quit received in that delay is the
/// result's back, which withdraws its preparation, not the phase's skip
/// (design.md "State Model", ResultPreparing).
#[test]
fn a_delay_after_the_save_moves_no_takeover() {
    let save_end = T + 8_600 * MS;
    let quit = T + 8_610 * MS;
    let commands = [(quit, Command::Quit)];
    let dir = tempfile::tempdir().unwrap();
    let order = Order::default();
    let mut rig = Tail::with_bgm(all_hits(), &commands, i64::MAX, 500);
    rig.ending = declared(3_000, 500);
    rig.order = std::sync::Arc::clone(&order);
    rig.pause_after_save = 20 * MS;
    let mut sink = NotingSink::new(dir.path(), &order);
    sink.clock = Some(std::sync::Arc::clone(&rig.clock));
    sink.save_for = TimeUs(600 * MS);
    let mut loaded = loaded_sinking(dir.path(), sink);
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let takeover = rig.takeovers.first().copied().expect("a takeover");
    assert!(
        (save_end..quit).contains(&takeover),
        "the takeover at the save's end: {takeover}"
    );
    assert_eq!(rig.fade_end(), Some(FadeEnd::RanOut));
    assert!(rig.skipped.is_empty(), "{:?}", rig.skipped);
    // The result's back, taken in its preparation: it withdraws it.
    assert!(rig.commands.is_empty(), "{:?}", rig.commands);
    assert_eq!(rig.shown_at(), None, "the quit withdrew the result");
}

/// A retry received while a save outlasts its fade phase retries once the
/// save has ended, the play saved and counted once, without showing its
/// result (design.md "State Model", ResultPreparing).
#[test]
fn a_retry_during_a_save_past_the_fade_retries_without_the_result() {
    let (rig, order) = sunk(
        declared(3_000, 500),
        &[(T + 8_300 * MS, Command::Retry)],
        |s| {
            s.save_for = TimeUs(600 * MS);
        },
    );
    assert_eq!(rig.fade_end(), Some(FadeEnd::Cut(AbortCause::Retry)));
    assert_eq!(rig.shown_at(), None);
    assert_eq!(rig.plays, 2);
    assert!(rig.reports[0].saved.as_ref().is_some_and(Result::is_ok));
    let order = order.lock().unwrap();
    assert_eq!(
        order.get(..6),
        Some(
            &[
                "fade phase",
                "saved",
                "counted",
                "play over",
                "ended",
                "fade ended"
            ][..]
        ),
        "{order:?}"
    );
}

/// The result a fade phase activates shows the facts its play's save
/// gave, as a result without a fade phase does (design.md "State Model",
/// ResultPreparing): a failed save, a failed count, a previous best another
/// writer changed, unavailable totals, and a retry in the fade phase (its
/// next play's result counting the play once), each the same with the fade
/// and without it.
#[test]
fn a_fade_phases_result_shows_its_saves_facts() {
    let previous = game::BestScore {
        plays: 9,
        ex: Some(1),
        bp: Some(99),
        max_combo: None,
        clear: Some(game::BestLamp::Failed),
    };
    let written = game::BestView::Counted {
        play: (1, 0),
        previous: Some(previous.clone()),
        current: previous,
        ineligible: None,
    };
    type Setup = Box<dyn Fn(&mut NotingSink)>;
    // A case: its name, its sink's setup, and its retry's time in the fade
    // and in the hold, if it retries.
    type Case = (&'static str, Setup, Option<(i64, i64)>);
    let cases: [Case; 5] = [
        ("save fails", Box::new(|s| s.save_fails = true), None),
        ("count fails", Box::new(|s| s.count = None), None),
        (
            "another writer",
            Box::new(move |s| s.count = Some(written.clone())),
            None,
        ),
        ("no totals", Box::new(|s| s.no_totals = true), None),
        // A retry in the fade, and its counterpart in the hold.
        (
            "retry",
            Box::new(|_| {}),
            Some((T + 8_500 * MS, T + 7_500 * MS)),
        ),
    ];
    for (case, setup, retry) in cases {
        // The last result's attempt, save, count and best, and totals, and
        // how its play's fade phase ended.
        let facts = |fadeout: i64, retry: Option<i64>| {
            let commands: Vec<_> = retry.map(|at| (at, Command::Retry)).into_iter().collect();
            let (rig, _) = sunk(declared(3_000, fadeout), &commands, &setup);
            let (_, report, summary) = rig.shown.last().cloned().expect("a result shown");
            let shown = (
                report.attempt,
                report.saved.as_ref().map(Result::is_ok),
                report.best,
                summary.and_then(|s| s.best),
                rig.totals.last().copied().flatten().flatten(),
            );
            (shown, rig.fade_end())
        };
        let (faded, end) = facts(1_000, retry.map(|r| r.0));
        let (held, none) = facts(0, retry.map(|r| r.1));
        assert!(end.is_some() && none.is_none(), "{case}: {end:?} {none:?}");
        assert_eq!(faded, held, "{case}");
        // Each case's own fact.
        let (attempt, saved, best, _, totals) = &faded;
        match case {
            "save fails" => assert_eq!(*saved, Some(false)),
            "count fails" => assert_eq!(*best, None),
            "another writer" => assert!(
                matches!(best, Some(game::BestView::Counted { previous: Some(p), .. }) if p.plays == 9),
                "{best:?}"
            ),
            "no totals" => assert_eq!(*totals, None),
            _ => {
                assert_eq!(*attempt, 2);
                assert_eq!(totals.map(|t| t.plays), Some(2));
            }
        }
    }
}

/// A retry received at the fade phase's deadline withdraws the result,
/// which never shows, and plays again: the phase's command, read before
/// its end (design.md "State Model", ResultPreparing).
#[test]
fn a_retry_at_the_fade_deadline_never_shows_the_result() {
    let deadline = T + 9_000 * MS;
    let (rig, _) = played_on(
        Tail::with_bgm(all_hits(), &[(deadline, Command::Retry)], i64::MAX, 500),
        |rig| rig.ending = declared(3_000, 1_000),
    );
    assert_eq!(rig.fade_end(), Some(FadeEnd::Cut(AbortCause::Retry)));
    assert_eq!(rig.shown_at(), None);
    assert_eq!(rig.plays, 2);
}

/// A quit once every note is judged but before the final deadline (the
/// last note's late miss window) enters Closing, as a quit with notes
/// left does: Closing is published, a fade-out never is, whatever the skin
/// declares, its record ends midway below the final deadline, and Closing
/// lasts the skin's declared 2 s.
#[test]
fn a_quit_after_the_last_judgment_before_the_final_deadline_closes() {
    let quit = T + 50 * MS;
    let (mut rig, report) = played_on(
        Tail::with_bgm(all_hits(), &[(quit, Command::Quit)], i64::MAX, 500),
        |rig| {
            rig.ending = declared(3_000, 1_000);
            rig.close = Some(TimeUs(2_000 * MS));
        },
    );
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit));
    let closing = report.closing.unwrap();
    assert_eq!(closing.ended_at.0 - closing.started_at.0, 2_000 * MS);
    // Closing was published, and the view kept after it never faded.
    let kept = rig.side.as_mut().unwrap().view();
    assert_eq!(kept.closing, Some(closing.started_at));
    assert_eq!(kept.fading, None);
    // Closing keeps the judgment bins the play counted: its three hits.
    assert!(kept.judge_bins.available);
    assert_eq!(kept.judge_bins.bins[1].iter().sum::<u32>(), 3);
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
    // Every note was judged before the quit, and the play completes at
    // 1.28 s: its record ends midway, before that.
    assert_eq!(report.tally.combo, 3);
    let finalized = rig.seen.iter().filter_map(|s| s.finalized).max().unwrap();
    assert!(finalized < TimeUs(1_280 * MS), "{finalized:?}");
    assert!(rig.seen.iter().all(|s| !s.complete));
    assert_eq!((rig.readies, rig.closings), (1, 1));
}

/// A full play's finish anchor is its last note's end; an autoplayed one's
/// is its last object, a background sound after the last note included.
#[test]
fn the_finish_anchor_is_the_last_note_or_the_last_object() {
    let mut late = chart();
    late.bgm.push(bms_model::BgmEvent {
        time: TimeUs(3_100 * MS),
        sound: SoundId(0),
    });
    let dir = tempfile::tempdir().unwrap();
    let played = loaded_with(dir.path(), late.clone(), None);
    assert_eq!(played.finish_anchor(), TimeUs(T));
    let dir = tempfile::tempdir().unwrap();
    let auto = loaded_with(dir.path(), late, Some(game::AutoKeySounds::Scheduled));
    assert_eq!(auto.finish_anchor(), TimeUs(3_100 * MS));
}

/// The chart with a second BGM event of sound 0 at `T + 2 s`, after the
/// last note: with the BGM 500 ms long the audio ends at `T + 2.5 s`.
fn late_bgm() -> Chart {
    let mut late = chart();
    late.bgm.push(bms_model::BgmEvent {
        time: TimeUs(T + 2_000 * MS),
        sound: SoundId(0),
    });
    late
}

/// An autoplay session of [`late_bgm`], the skin declaring `ending`, run
/// with its real audio until its result shows.
fn autoplayed(ending: PlayEnding) -> (Tail, PlayReport) {
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = loaded_with(dir.path(), late_bgm(), Some(game::AutoKeySounds::Scheduled));
    let mut rig = Tail::autoplay(&loaded, 500);
    rig.ending = ending;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let report = rig.reports[0].clone();
    (rig, report)
}

/// Autoplay's audio ends with its late background sound, at `T + 2.5 s`;
/// its hold runs 5 s from that sound's start, its last object, not from
/// the last note, and with no declaration it shows its result there, with
/// no fade-out and nothing saved.
#[test]
fn autoplay_finishes_after_its_last_background_sound() {
    let (mut rig, report) = autoplayed(PlayEnding::NONE);
    assert_eq!(report.end, PlayEnd::Completed);
    // Every note judged by autoplay.
    assert_eq!(report.tally.combo, 3);
    let done = rig.audio_done_at.unwrap();
    assert!((T + 2_450 * MS..T + 2_550 * MS).contains(&done), "{done}");
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
    assert_eq!(rig.side.as_mut().unwrap().view().fading, None);
    // The played anchor would give `T + 5 s`.
    assert_eq!(rig.shown_at(), Some(T + 7_000 * MS));
    assert!(report.saved.is_none());
}

/// Autoplay's fade-out starts at its hold's end, 5 s after its last
/// background sound, and its result follows after the skin's fadeout.
#[test]
fn autoplays_fade_out_follows_its_last_background_sound() {
    let (rig, report) = autoplayed(declared(0, 1_000));
    assert_eq!(report.end, PlayEnd::Completed);
    assert!(rig.audio_done_at.unwrap() < T + 2_550 * MS);
    assert_eq!(rig.fading(), Some(TimeUs(T + 7_000 * MS)));
    assert_eq!(rig.shown_at(), Some(T + 8_000 * MS));
    assert!(report.saved.is_none());
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
    round_declaring(items, PlayEnding::NONE)
}

/// [`round`] with the skin declaring `ending`, run to its result.
fn round_declaring(items: Vec<ScriptItem>, ending: PlayEnding) -> (Tail, PlayReport) {
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
    rig.ending = ending;
    game::live::run_chart(&mut loaded, &mut rig).unwrap();
    let report = rig.reports[0].clone();
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
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
}

/// A practice round that does not repeat keeps its ring-out whatever the
/// skin declares: it ends at the first wake at or past its completion
/// plus 1.5 s with the audio done, with no fade-out, and its result shows
/// before a full play's hold could end.
#[test]
fn a_non_repeating_round_ignores_the_declared_ending() {
    let (rig, report) = round_declaring(hits(), declared(3_000, 1_000));
    assert_eq!(report.end, PlayEnd::Completed);
    let completed_at = rig.completed_at.unwrap();
    let end_at = completed_at + game::RING_OUT.0;
    let last = rig.tail().last().unwrap();
    assert!(
        last.at >= end_at && last.at < end_at + 50 * MS,
        "{last:?} {end_at}"
    );
    assert!(rig.seen.iter().all(|s| s.fading.is_none()));
    let shown = rig.shown_at().unwrap();
    assert!(shown < completed_at + 5_000 * MS, "{shown} {completed_at}");
}
