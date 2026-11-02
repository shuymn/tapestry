//! The start gate in headless sessions (design.md "Play Lifecycle", the
//! start gate, and ending without a played attempt): each READY is a
//! candidate a hold of START or SELECT cancels before its transport
//! commits, keeping its opening and input in the record's history; a
//! release rearms a fresh, full READY with a new origin; exactly one
//! candidate commits; and whatever ends the attempt before the commit
//! saves the capture cancelled before playback.
//!
//! Every test drives `Session::play_until`, the production lifecycle
//! (`game::live::play_and_next`) through the scripted rig's input side
//! (the source set and its adjustment tap) and audio side (the commit
//! before the engine pass reaching the commit point). Script times are on
//! the first attempted opening's timeline: its READY opens at −1 s.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, count_judged, lane, tapestry_settings};
use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
use game::{
    AbortCause, AdjustmentConfig, ChartIngest, Command, DataRoot, DeviceSettings, HiSpeedSettings,
    IngestError, Ingested, KeyConfigs, Loaded, Parts, PlayEnd, PlayOptions, PlayReport, PlayScript,
    Ratio, RunnerAudio, Session, SessionConfig, SessionEvent, SessionEventSink, SpeedMode,
};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};
use replay::{BuildProvenance, CancelReason, DecodeLimits, EndStatus, PlayRecord, ReceiveMethod};
use rules::Judgment;

type Out = audio::RecordingOutput<audio::SimDevice>;

const Q: ControlId = ControlId(0x10);
const W: ControlId = ControlId(0x11);
const F1: ControlId = ControlId(0x3B);

fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("start-gate")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    (dir, root)
}

fn code(n: u8) -> ControlId {
    let keys = DeviceSettings::default().keyboard;
    ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control)
}

/// Taps on lanes 1 to 3 at 1.0, 1.2 and 1.4 s, and a BGM sound at 0: the
/// transport's start.
fn chart() -> Chart {
    ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(2), TimeUs(1_200 * MS))
        .tap(lane(3), TimeUs(1_400 * MS))
        .bgm(TimeUs(0), SoundId(0))
        .build()
        .unwrap()
}

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

#[derive(Clone, Default)]
struct Events(Rc<RefCell<Vec<SessionEvent>>>);

impl SessionEventSink for Events {
    fn on_event(&mut self, ev: &SessionEvent) {
        self.0.borrow_mut().push(*ev);
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(0),
        effects: 4_096,
        seed: 7,
        speed: bms_model::Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

fn chart_file(root: &DataRoot) -> PathBuf {
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("play.chart");
    fs::write(&path, b"built in code").unwrap();
    path
}

/// Classic at 1.00 with a 0.25 step: F1 and F2 need no tempo.
fn options() -> PlayOptions {
    PlayOptions::new(
        HiSpeedSettings {
            mode: SpeedMode::Classic,
            ..HiSpeedSettings::default()
        },
        game::Covers::default(),
    )
    .unwrap()
}

/// A session of `chart()` with the start gate on the default bindings:
/// START on Q, SELECT on W.
fn session(root: &DataRoot, events: &Events) -> Session<Out> {
    session_with_sources(root, events, common::sources())
}

/// [`session`] with the source set's limits.
fn session_with_sources(
    root: &DataRoot,
    events: &Events,
    sources: input::SourceConfig,
) -> Session<Out> {
    let device = audio::SimDevice::new(audio::SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let out = audio::RecordingOutput::new(device, 0);
    session_of(root, events, &chart(), sources, out, true)
}

/// A session of `chart` on `out`, with the start gate when `gated`.
fn session_of<O: audio::AudioOutput + game::Wakes>(
    root: &DataRoot,
    events: &Events,
    chart: &Chart,
    sources: input::SourceConfig,
    out: O,
    gated: bool,
) -> Session<O> {
    let chart = chart.clone();
    let parts = Parts {
        ingest: Box::new(Built(chart.clone())),
        events: Box::new(events.clone()),
        ..Parts::standard(root, &tapestry_settings(root).player)
    };
    let loaded = Loaded::load(
        root.clone(),
        tapestry_settings(root),
        &chart_file(root),
        config(),
        parts,
    )
    .unwrap();
    let engine = audio::AudioEngine::new(
        common::bank(&chart),
        &chart.bgm,
        RATE,
        PERIOD,
        audio::MAX_VOICES,
        64,
    )
    .unwrap();
    let audio = RunnerAudio::new(engine, out);
    let mut s = loaded.start(audio, SourceSet::new(sources).unwrap());
    if !gated {
        return s;
    }
    let d = DeviceSettings::default();
    let keys = KeyConfigs {
        keyboard: d.keyboard,
        controller: d.controller,
        controller_device: None,
    };
    let plan = AdjustmentConfig::default().plan(&keys, None, None).unwrap();
    s.with_start_gate(&plan, options()).unwrap();
    s
}

fn item(at: i64, action: Action) -> ScriptItem {
    ScriptItem {
        at: TimeUs(at),
        action,
    }
}

/// Keyboard `d` sets `control` to `value` at `at` ms.
fn raw(at: i64, d: u64, control: ControlId, value: i32) -> ScriptItem {
    item(
        at * MS,
        Action::Packet(Packet {
            device: DeviceId(d),
            control,
            value,
            background: false,
        }),
    )
}

fn connect(d: u64) -> ScriptItem {
    item(i64::MIN, Action::Connect(DeviceId(d), DeviceKind::Keyboard))
}

/// Keyboard 0 pressing lane `n` at `at` ms and releasing it 40 ms later.
fn tap(n: u8, at: i64) -> [ScriptItem; 2] {
    [raw(at, 0, code(n), 1), raw(at + 40, 0, code(n), 0)]
}

/// Every note pressed on time for READY whose chart time 0 is at `zero`
/// ms of the script.
fn notes_from(zero: i64) -> Vec<ScriptItem> {
    [(1, 1_000), (2, 1_200), (3, 1_400)]
        .into_iter()
        .flat_map(|(n, t)| tap(n, zero + t))
        .collect()
}

fn script(items: Vec<ScriptItem>, commands: &[(i64, Command)]) -> PlayScript {
    let recorded = DeviceSettings::default().keyboard.recorded();
    let mut input = ScriptedInput::new(items);
    for d in 0..2 {
        input = input.with_recorded(DeviceId(d), recorded.clone());
    }
    PlayScript {
        input,
        commands: commands
            .iter()
            .map(|&(at, c)| (TimeUs(at * MS), c))
            .collect(),
    }
}

fn record(report: &PlayReport) -> &PlayRecord {
    report.record.as_ref().unwrap()
}

/// The saved file decodes to the report's record.
fn saved_decodes(report: &PlayReport) {
    let path = &report.saved.as_ref().unwrap().as_ref().unwrap().path;
    let decoded = replay::decode(&fs::read(path).unwrap(), &DecodeLimits::STANDARD).unwrap();
    assert_eq!(Some(&decoded), report.record.as_ref());
}

/// The BGM starts the engine scheduled: the transport's runs.
fn bgm_starts<O: audio::AudioOutput + game::Wakes>(s: &mut Session<O>) -> usize {
    let mut n = 0;
    s.drain_scheduled(|e| n += usize::from(matches!(e.kind, audio::ScheduledKind::Bgm { .. })));
    n
}

/// Lane presses and releases of a list of events, as `(ms, lane key, value)`.
fn lane_events(events: &[PhysicalInputEvent]) -> Vec<(i64, u32, i32)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            InputKind::Control { control, value } => Some((e.received.0 / MS, control.0, value)),
            InputKind::Disconnected => None,
        })
        .collect()
}

#[test]
fn released_ready_commits_once_and_plays_with_an_empty_history() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = session(&root, &events);
    let mut items = vec![connect(0)];
    items.extend(notes_from(0));
    let report = s.play(script(items, &[])).unwrap();

    assert_eq!(report.end, PlayEnd::Completed);
    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    assert!(r.history.openings.is_empty());
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
    assert!(report.result.as_ref().unwrap().matches_live);
    assert_eq!(bgm_starts(&mut s), 1, "the transport started once");
    saved_decodes(&report);
}

#[test]
fn a_hold_cancels_ready_keeps_its_input_and_a_release_rearms_a_fresh_one() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = session(&root, &events);
    // An early lane press in the first READY, START held at −300 ms, a lane
    // press while waiting, released at 200 ms: the next READY opens there,
    // its chart time 0 at 1.2 s of the script.
    let mut items = vec![connect(0)];
    items.extend(tap(4, -500));
    items.push(raw(-300, 0, Q, 1));
    items.extend(tap(5, 0));
    items.push(raw(200, 0, Q, 0));
    items.extend(notes_from(1_200));
    let report = s.play(script(items, &[])).unwrap();

    assert_eq!(report.end, PlayEnd::Completed);
    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    // The winning READY: its own timeline, a fresh snapshot opened at
    // −1 s, every note judged once.
    assert_eq!(r.start.opened_at, TimeUs(-1_000 * MS));
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
    assert!(report.result.as_ref().unwrap().matches_live);
    // The cancelled READY: kept whole on its own timeline, 1.2 s before
    // the winner's, closed at the rearm, its input all there: the early
    // press and the one while waiting.
    let h = &r.history;
    assert_eq!(h.openings.len(), 1);
    let o = &h.openings[0];
    assert_eq!(o.reason, CancelReason::Held);
    assert_eq!(o.origin, TimeUs(-1_200 * MS));
    assert_eq!(o.start.opened_at, TimeUs(-1_000 * MS));
    assert_eq!(o.closed, TimeUs(200 * MS));
    let (l4, l5) = (code(4).0, code(5).0);
    assert_eq!(
        lane_events(&h.events[o.events.start as usize..o.events.end as usize]),
        vec![(-500, l4, 1), (-460, l4, 0), (0, l5, 1), (40, l5, 0)]
    );
    // Its scheduled chart start never ran: one transport, the winner's.
    assert_eq!(bgm_starts(&mut s), 1);
    // A rearm is no retry and no new admission.
    assert_eq!(
        *events.0.borrow(),
        vec![
            SessionEvent::Load,
            SessionEvent::Start { attempt: 1 },
            SessionEvent::End {
                attempt: 1,
                status: EndStatus::PlayedToEnd
            },
        ]
    );
    saved_decodes(&report);
}

#[test]
fn every_rearm_takes_a_fresh_origin_and_keeps_every_opening() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = session(&root, &events);
    // SELECT and START alternately held and released three times.
    let mut items = vec![connect(0)];
    let mut zero = 0;
    for k in 0..3 {
        let at = zero - 400;
        let role = if k % 2 == 0 { W } else { Q };
        items.push(raw(at, 0, role, 1));
        items.extend(tap(6, at + 50));
        items.push(raw(at + 150, 0, role, 0));
        zero = at + 150 + 1_000;
    }
    items.extend(notes_from(zero));
    let report = s.play(script(items, &[])).unwrap();

    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    let origins: Vec<i64> = r.history.openings.iter().map(|o| o.origin.0 / MS).collect();
    // Each READY 750 ms after the previous: never an origin reused.
    assert_eq!(origins, vec![-2_250, -1_500, -750]);
    for o in &r.history.openings {
        assert_eq!(o.reason, CancelReason::Held);
        assert_eq!(o.events.end - o.events.start, 2, "each kept its press");
    }
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
    assert!(report.result.as_ref().unwrap().matches_live);
    assert_eq!(bgm_starts(&mut s), 1);
    saved_decodes(&report);
}

#[test]
fn a_hold_in_the_last_100_ms_before_the_commit_cancels_it() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let mut items = vec![connect(0), raw(-50, 0, Q, 1), raw(100, 0, Q, 0)];
    items.extend(notes_from(1_100));
    let report = s.play(script(items, &[])).unwrap();

    let r = record(&report);
    assert_eq!(r.history.openings.len(), 1);
    assert_eq!(r.history.openings[0].closed, TimeUs(100 * MS));
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    assert_eq!(bgm_starts(&mut s), 1);
}

#[test]
fn a_hold_after_the_commit_is_an_ordinary_adjustment() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    // START held from just after chart time 0 through the notes, F1
    // pressed under it: the play goes on, judged as usual, and the
    // adjustment reaches the reducer.
    let mut items = vec![connect(0), raw(1, 0, Q, 1)];
    items.extend(notes_from(0));
    items.push(raw(500, 0, F1, 1));
    items.push(raw(520, 0, F1, 0));
    let report = s.play(script(items, &[])).unwrap();

    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    assert!(r.history.openings.is_empty());
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
    // Under START the white keys 1 and 3 step slower and the black key 2
    // faster ("Play Adjustments", dispatch), and F1 slower: 1.00 − 0.25 +
    // 0.25 − 0.25 − 0.25, while each press still judged its note.
    let snap = s.presentation().unwrap();
    assert_eq!(snap.hispeed, Some(Ratio::from_hundredths(50)));
    assert!(snap.adjusting, "START is still held");
}

#[test]
fn f1_and_f2_go_through_the_reducer() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    // F2 twice, F1 once, in READY: 1.00 + 0.25 + 0.25 − 0.25.
    let mut items = vec![connect(0)];
    for at in [-800, -700, -600] {
        let key = if at == -600 { F1 } else { ControlId(0x3C) };
        items.push(raw(at, 0, key, 1));
        items.push(raw(at + 20, 0, key, 0));
    }
    items.extend(notes_from(0));
    s.play(script(items, &[])).unwrap();
    let snap = s.presentation().unwrap();
    assert_eq!(snap.hispeed, Some(Ratio::from_hundredths(125)));
    assert!(!snap.adjusting);
}

/// The capture of an attempt that ended before its commit: no winning
/// opening, no watermark, no score; its last opening at origin 0.
fn assert_cancelled(report: &PlayReport, reason: CancelReason) {
    let r = record(report);
    assert_eq!(r.status.end, EndStatus::CancelledBeforePlayback);
    assert!(!report.played());
    assert!(r.events.is_empty() && r.arrivals.is_empty());
    assert_eq!(r.start, eval::InputStart::default());
    assert!(report.result.is_none() && report.best.is_none());
    let last = r.history.openings.last().unwrap();
    assert_eq!(last.origin, TimeUs(0));
    assert_eq!(last.reason, reason);
    saved_decodes(report);
}

#[test]
fn a_quit_before_the_commit_saves_the_capture_and_leaves_without_a_result() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = session(&root, &events);
    let mut items = vec![connect(0)];
    items.extend(tap(4, -500));
    let report = s.play(script(items, &[(-200, Command::Quit)])).unwrap();

    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_cancelled(&report, CancelReason::Quit);
    let r = record(&report);
    // The early press advanced the speculative evaluator, and is kept.
    let l4 = code(4).0;
    assert_eq!(
        lane_events(&r.history.events),
        vec![(-500, l4, 1), (-460, l4, 0)]
    );
    assert_eq!(bgm_starts(&mut s), 0, "no transport ran");
    assert_eq!(
        events.0.borrow().last(),
        Some(&SessionEvent::End {
            attempt: 1,
            status: EndStatus::CancelledBeforePlayback
        })
    );
}

#[test]
fn a_quit_while_waiting_keeps_the_cancelled_opening_and_its_input() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let mut items = vec![connect(0), raw(-300, 0, Q, 1)];
    items.extend(tap(4, 0));
    let report = s.play(script(items, &[(300, Command::Quit)])).unwrap();

    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit));
    // The opening was cancelled by the hold; the quit ended the wait.
    assert_cancelled(&report, CancelReason::Held);
    let r = record(&report);
    assert_eq!(r.history.openings.len(), 1);
    assert_eq!(r.history.openings[0].closed, TimeUs(300 * MS));
    let l4 = code(4).0;
    assert_eq!(
        lane_events(&r.history.events),
        vec![(0, l4, 1), (40, l4, 0)]
    );
    assert_eq!(bgm_starts(&mut s), 0);
}

#[test]
fn a_retry_before_the_commit_goes_on_to_the_next_attempt() {
    let (_dir, root) = temp_root();
    let events = Events::default();
    let mut s = session(&root, &events);
    let one = s
        .play(script(vec![connect(0)], &[(-500, Command::Retry)]))
        .unwrap();
    assert_eq!(one.end, PlayEnd::Aborted(AbortCause::Retry));
    assert_cancelled(&one, CancelReason::Retry);
    let two = s.play(script(notes_from(0), &[])).unwrap();
    assert_eq!(record(&two).status.end, EndStatus::PlayedToEnd);
    assert_eq!(two.attempt, 2);
}

#[test]
fn a_close_before_the_commit_saves_the_capture() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let report = s
        .play_until(script(vec![connect(0)], &[]), Some(TimeUs(-400 * MS)))
        .unwrap()
        .unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit));
    assert_cancelled(&report, CancelReason::Close);
}

#[test]
fn focus_loss_before_the_commit_saves_the_capture() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let items = vec![connect(0), item(-400 * MS, Action::FocusLoss)];
    let report = s.play(script(items, &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::FocusLoss));
    assert_cancelled(&report, CancelReason::FocusLost);
}

#[test]
fn a_failure_while_waiting_saves_the_capture() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    // A suspend while waiting after a cancellation.
    let items = vec![connect(0), raw(-300, 0, W, 1)];
    let report = s.play(script(items, &[(0, Command::Suspend)])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Suspend));
    assert_cancelled(&report, CancelReason::Held);
    assert!(record(&report).status.incomplete.suspend);
}

#[test]
fn after_the_commit_every_end_is_ended_midway() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let mut items = vec![connect(0)];
    items.extend(notes_from(0));
    let report = s.play(script(items, &[(1_100, Command::Quit)])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit));
    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::EndedMidway);
    assert!(r.history.openings.is_empty());
    assert!(report.result.is_some());
}

#[test]
fn a_hold_before_the_opening_defers_it_to_its_release() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    // SELECT held before READY would open, released at −600 ms: READY
    // opens there, chart time 0 at 400 ms of the script.
    let mut items = vec![connect(0), raw(-2_000, 0, W, 1), raw(-600, 0, W, 0)];
    items.extend(notes_from(400));
    let report = s.play(script(items, &[])).unwrap();
    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    assert!(
        r.history.openings.is_empty(),
        "a deferred opening opened nothing"
    );
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
}

#[test]
fn a_close_while_deferred_opens_nothing_and_saves_nothing() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let items = vec![connect(0), raw(-2_000, 0, Q, 1)];
    let report = s.play_until(script(items, &[]), Some(TimeUs(0))).unwrap();
    assert!(report.is_none());
}

#[test]
fn a_device_removed_and_reconnected_across_a_rearm_keeps_its_source_id() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    // Two keyboards; START held on keyboard 0; keyboard 1 removed and
    // connected again while waiting; START released.
    let mut items = vec![connect(0), connect(1), raw(-300, 0, Q, 1)];
    items.push(item(-100 * MS, Action::Disconnect(DeviceId(1))));
    items.push(item(
        -50 * MS,
        Action::Connect(DeviceId(1), DeviceKind::Keyboard),
    ));
    items.push(raw(100, 0, Q, 0));
    items.extend(notes_from(1_100));
    let report = s.play(script(items, &[])).unwrap();

    let r = record(&report);
    assert_eq!(r.status.end, EndStatus::PlayedToEnd);
    // One source table for every opening: keyboard 1 keeps source 1 and
    // reports absent in the winning READY; its reconnection is an
    // arrival, not a participant.
    assert_eq!(r.devices.sources.len(), 2);
    assert!(r.events.iter().any(|e| e.source == SourceId(1)
        && e.kind == InputKind::Disconnected
        && e.received == TimeUs(-1_000 * MS)));
    let o = &r.history.openings[0];
    let cancelled = &r.history.events[o.events.start as usize..o.events.end as usize];
    assert!(
        cancelled
            .iter()
            .any(|e| e.source == SourceId(1) && e.kind == InputKind::Disconnected)
    );
    assert_eq!(
        r.history.arrivals[o.arrivals.start as usize..o.arrivals.end as usize].len(),
        1
    );
    assert_eq!(count_judged(&report.live, Judgment::Perfect), 3);
    saved_decodes(&report);
}

/// An ending and the commit observed in the same step are settled on the
/// start gate: whichever of the quit's cancellation and the transport's
/// commit won decides the capture, so a play whose transport started is
/// never saved as cancelled before playback, and one saved so never
/// started it. Quits around chart time 0, one per millisecond, go through
/// `game::live::play_and_next` with the scripted audio side committing.
#[test]
fn an_ending_in_the_commits_step_is_settled_by_the_gate() {
    let (mut played, mut unplayed) = (0, 0);
    for at in -40..=5 {
        let (_dir, root) = temp_root();
        let mut s = session(&root, &Events::default());
        let report = s
            .play(script(vec![connect(0)], &[(at, Command::Quit)]))
            .unwrap();
        let started = bgm_starts(&mut s) > 0;
        assert_eq!(
            record(&report).status.end != EndStatus::CancelledBeforePlayback,
            started,
            "quit at {at} ms"
        );
        if started {
            played += 1;
        } else {
            unplayed += 1;
        }
    }
    assert!(played > 0 && unplayed > 0, "the quits straddle the commit");
}

/// A cancelled opening whose arrivals run the history out seals the
/// capture: no candidate follows, and the attempt ends cancelled before
/// playback, incomplete (overflow), with what fit. Through
/// `game::live::play_and_next` and the scripted rig's rearm.
#[test]
fn a_history_run_out_while_waiting_ends_the_attempt() {
    let (_dir, root) = temp_root();
    let mut s = session_with_sources(
        &root,
        &Events::default(),
        input::SourceConfig {
            max_devices: 32,
            max_sources: 4,
            max_held: 16,
            queue_capacity: 1_024,
            max_arrivals: 32,
        },
    );
    // START held; 17 keyboards arrive while waiting; START released.
    let mut items = vec![connect(0), raw(-300, 0, Q, 1)];
    for d in 1..=17_i64 {
        items.push(item(
            (-250 + d) * MS,
            Action::Connect(DeviceId(d.unsigned_abs()), DeviceKind::Keyboard),
        ));
    }
    items.push(raw(0, 0, Q, 0));
    items.extend(notes_from(1_000));
    let report = s.play(script(items, &[])).unwrap();

    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Overflow));
    assert_cancelled(&report, CancelReason::Held);
    let r = record(&report);
    assert!(r.status.incomplete.overflow);
    assert_eq!(r.history.openings.len(), 1);
    assert_eq!(r.history.arrivals.len(), replay::MAX_OPENING_ARRIVALS);
    assert_eq!(bgm_starts(&mut s), 0, "no further candidate ran");
}

/// The four keyboards of the admission fixtures, and the held-snapshot
/// storage the input side reserves for them (`common::sources()`: 16 held
/// controls and the axes per source).
const KINDS: [DeviceKind; 4] = [DeviceKind::Keyboard; 4];
const HELD: usize = 4 * (16 + input::hid::MAX_AXES);

/// Two taps, at 1 s and at `last_s` s.
fn two_notes(last_s: i64) -> Chart {
    ChartBuilder::new(150_000)
        .tap(lane(1), TimeUs(1_000 * MS))
        .tap(lane(1), TimeUs(last_s * 1_000 * MS))
        .build()
        .unwrap()
}

/// Whether `chart` under `config` is admitted for four keyboards without
/// the gate, and with its storage (`Loaded::plan_gated`).
fn admits(root: &DataRoot, chart: Chart, config: SessionConfig) -> (bool, bool) {
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        ..Parts::standard(root, &tapestry_settings(root).player)
    };
    let loaded = Loaded::load(
        root.clone(),
        tapestry_settings(root),
        &chart_file(root),
        config,
        parts,
    )
    .unwrap();
    let axes = vec![Vec::new(); KINDS.len()];
    (
        loaded.plan_gated(&KINDS, &axes, None).is_ok(),
        loaded.plan_gated(&KINDS, &axes, Some(HELD)).is_ok(),
    )
}

/// The largest `x` in `lo..hi` for which `ungated(x)` holds, `ungated`
/// holding at `lo` and not at `hi`, by bisection.
fn last_admitted(mut lo: i64, mut hi: i64, ungated: impl Fn(i64) -> bool) -> i64 {
    assert!(ungated(lo) && !ungated(hi));
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if ungated(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo
}

/// A chart length whose recording window four keyboards fit only without
/// the gate's storage.
fn fits_only_ungated(root: &DataRoot) -> i64 {
    let at = |s| admits(root, two_notes(s), config());
    let lo = last_admitted(2, 4_000, |s| at(s).0);
    let t = (lo - 40..=lo)
        .find(|&t| at(t) == (true, false))
        .unwrap_or_else(|| panic!("no window fits only without the gate below {lo} s"));
    assert_eq!(at(lo - 60), (true, true));
    t
}

/// A gated play admits its own storage with its recording: a window that
/// fits the reservation only without the gate's history and snapshots is
/// refused for a gated play, while the same play without the gate is
/// admitted (`Loaded::plan_gated`).
#[test]
fn the_gates_storage_counts_in_the_recording_admission() {
    let (_dir, root) = temp_root();
    fits_only_ungated(&root);
}

/// Through the lifecycle: a gated play whose window fits only without the
/// gate's storage is refused right after recording opens, before it is
/// admitted or handed to audio (`game::live::play_and_next` planning with
/// the input side's snapshot storage).
#[test]
fn a_gated_play_over_the_reservation_is_refused_before_it_starts() {
    let (_dir, root) = temp_root();
    let last = fits_only_ungated(&root);
    let events = Events::default();
    let device = audio::SimDevice::new(audio::SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let out = audio::RecordingOutput::new(device, 0);
    let mut s = session_of(
        &root,
        &events,
        &two_notes(last),
        common::sources(),
        out,
        true,
    );
    let items = (0..4).map(connect).collect();
    // A quit early in READY keeps a play that was wrongly admitted short.
    let ran = s.play(script(items, &[(-500, Command::Quit)]));
    assert!(
        matches!(
            ran,
            Err(game::SessionPlayError::Logic(
                game::LogicError::RecordingTooLarge { .. }
            ))
        ),
        "{ran:?}"
    );
    assert!(
        !events
            .0
            .borrow()
            .iter()
            .any(|e| matches!(e, SessionEvent::Start { .. })),
        "nothing was admitted"
    );
}

/// An autoplay session's gated plan charges the gate's storage beside its
/// own buffers: at the largest render effect queue an ungated autoplay
/// plan admits, the gated one is refused, and with a smaller queue both
/// fit; ungated admission is unchanged.
#[test]
fn a_gated_autoplay_plan_counts_the_gates_storage() {
    let (_dir, root) = temp_root();
    let auto = |effects: i64| SessionConfig {
        autoplay: Some(game::AutoKeySounds::Scheduled),
        effects: usize::try_from(effects).unwrap(),
        ..config()
    };
    let at = |effects| admits(&root, chart(), auto(effects));
    let most = i64::try_from(game::RECORDING_MEMORY / eval::EVENT_BYTES).unwrap();
    let lo = last_admitted(0, most, |e| at(e).0);
    assert_eq!(at(lo), (true, false));
    assert_eq!(at(lo - 200_000), (true, true));
}

/// An output that renders a block and then reports a failure: the order
/// an output may take, the samples written before the release fails.
/// It fails on the first block that sounds, the BGM at chart time 0 that
/// the transport's commit lets start.
struct FailsAfterSounding {
    inner: audio::SimDevice,
    failed: bool,
}

impl audio::AudioOutput for FailsAfterSounding {
    fn rate(&self) -> u32 {
        self.inner.rate()
    }

    fn period(&self) -> u32 {
        self.inner.period()
    }

    fn wait(&mut self) -> audio::Wake {
        self.inner.wait()
    }

    fn write(&mut self, f: impl FnOnce(&mut [f32])) -> Result<(), audio::OutputError> {
        let mut sounded = false;
        self.inner.write(|buf| {
            f(buf);
            sounded = buf.iter().any(|&s| s != 0.0);
        })?;
        if sounded && !self.failed {
            self.failed = true;
            return Err(audio::OutputError::DeviceLost);
        }
        Ok(())
    }
}

impl game::Wakes for FailsAfterSounding {
    fn next_wake(&self) -> TimeUs {
        self.inner.next_wake()
    }
}

/// The audio pass after the step's look at the gate commits the transport,
/// renders its block and then fails: the attempt was played, so it is
/// saved ended midway, never as cancelled before playback. Every way out
/// of `game::live::play_and_next` settles the gate before recording
/// closes.
#[test]
fn a_failure_after_the_committing_block_leaves_a_played_attempt() {
    let (_dir, root) = temp_root();
    let device = audio::SimDevice::new(audio::SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let out = FailsAfterSounding {
        inner: device,
        failed: false,
    };
    let mut s = session_of(
        &root,
        &Events::default(),
        &chart(),
        common::sources(),
        out,
        true,
    );
    let report = s.play(script(vec![connect(0)], &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::OutputFailed));
    assert!(report.played());
    assert_eq!(record(&report).status.end, EndStatus::EndedMidway);
}

/// A quit queued with a retry at the release that ends a deferred opening
/// belongs to Preparing: the attempt leaves without opening recording,
/// whatever came first.
#[test]
fn a_quit_at_the_release_of_a_deferred_opening_leaves_before_it_opens() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let items = vec![connect(0), raw(-2_000, 0, Q, 1), raw(-600, 0, Q, 0)];
    let ran = s.play(script(
        items,
        &[(-600, Command::Retry), (-600, Command::Quit)],
    ));
    assert!(matches!(ran, Err(game::SessionPlayError::Left)), "{ran:?}");
    assert_eq!(bgm_starts(&mut s), 0);
}

/// A quit while a hold defers the opening is answered through the public
/// method: nothing opened, nothing saved.
#[test]
fn a_quit_while_the_opening_is_deferred_plays_nothing() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let items = vec![connect(0), raw(-2_000, 0, Q, 1)];
    let ran = s.play(script(items, &[(-500, Command::Quit)]));
    assert!(matches!(ran, Err(game::SessionPlayError::Left)), "{ran:?}");
}

/// Each attempt numbers its candidates after the last one's, through the
/// lifecycle: an attempt cancelled before playback, then one that plays,
/// whose candidate is the gate's second.
#[test]
fn a_second_attempt_numbers_its_candidate_after_the_first() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &Events::default());
    let one = s
        .play(script(vec![connect(0)], &[(-500, Command::Retry)]))
        .unwrap();
    assert_cancelled(&one, CancelReason::Retry);
    assert_eq!(s.presentation().unwrap().generation, 1);
    let two = s.play(script(notes_from(0), &[])).unwrap();
    assert!(two.played());
    assert_eq!(s.presentation().unwrap().generation, 2);
}

/// The scheduled BGM before chart time 0 waits for the transport's commit
/// in a gated play (design.md "Play Lifecycle", the start gate), where an
/// ungated play starts it at its time.
#[test]
fn pre_zero_bgm_waits_for_the_commit_in_a_gated_play() {
    let chart = ChartBuilder::new(150_000)
        .sound("bgm.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .bgm(TimeUs(-300 * MS), SoundId(0))
        .build()
        .unwrap();
    let mut frames = Vec::new();
    for gated in [false, true] {
        let (_dir, root) = temp_root();
        let device = audio::SimDevice::new(audio::SimConfig::steady(RATE, PERIOD, TimeUs(0)));
        let out = audio::RecordingOutput::new(device, 0);
        let mut s = session_of(
            &root,
            &Events::default(),
            &chart,
            common::sources(),
            out,
            gated,
        );
        let mut items = vec![connect(0)];
        items.extend(tap(1, 1_000));
        s.play(script(items, &[])).unwrap();
        let mut frame = None;
        s.drain_scheduled(|e| {
            if matches!(e.kind, audio::ScheduledKind::Bgm { .. }) {
                frame = Some(e.frame);
            }
        });
        frames.push(frame.unwrap());
    }
    // 300 ms is 14 400 frames at 48 kHz: the gated start is that much
    // later, within a pass.
    let late = frames[1] - frames[0];
    assert!(
        (14_400 - u64::from(PERIOD)..=14_400 + u64::from(PERIOD)).contains(&late),
        "{frames:?}"
    );
}
