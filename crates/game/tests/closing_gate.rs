//! Closing and the start gate in headless sessions (design.md "Play
//! Lifecycle", Closing and the start gate): a displayed survival failure
//! before the transport is seen committed is retained, scoring frozen,
//! and acts on the commit at its watermark; a cancellation and its rearm
//! discard it; a quit at or after the chart's start is classified by the
//! gate's settled outcome.
//!
//! Every test drives `Session::play`, the production lifecycle through the
//! scripted rig's real start gate. The audio output's first wake is held
//! back to a chosen session time, so the transport commits there: what
//! the play does before it is uncommitted.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{MS, PERIOD, RATE, lane, tapestry_settings};
use eval::ControlId;
use game::{
    AbortCause, AdjustmentConfig, ChartIngest, ClosingEnd, Command, DataRoot, DeviceSettings,
    FailureAction, HiSpeedSettings, IngestError, Ingested, KeyConfigs, Loaded, Parts, PlayEnd,
    PlayOptions, PlayReport, PlayScript, RunnerAudio, Session, SessionConfig, SpeedMode,
};
use input::{Action, DeviceId, DeviceKind, Packet, ScriptItem, ScriptedInput, SourceSet};
use replay::{BuildProvenance, EndStatus, ReceiveMethod};

const Q: ControlId = ControlId(0x10);

/// Where the session clock has a play's chart time 0: its READY opens at
/// session time 0, 1 s before it.
const ZERO: i64 = 1_000;

fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("closing-gate")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    (dir, root)
}

fn code(n: u8) -> ControlId {
    let keys = DeviceSettings::default().keyboard;
    ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control)
}

/// `n` notes on lane 1, 100 ms apart from 1 s, and a BGM sound at 0: left
/// unpressed, the tapestry preset's survival gauge (its second) fails at
/// the fifteenth miss, at 2.58 s.
fn notes(n: i64) -> Chart {
    (0..n)
        .fold(
            ChartBuilder::new(150_000)
                .sound("bgm.wav")
                .bgm(TimeUs(0), SoundId(0)),
            |b, i| b.tap(lane(1), TimeUs((1_000 + i * 100) * MS)),
        )
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

fn chart_file(root: &DataRoot) -> PathBuf {
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("play.chart");
    fs::write(&path, b"built in code").unwrap();
    path
}

/// A steady device whose buffer events are held back until session time
/// `until`: the engine renders nothing before it, so a held transport
/// commits at the first pass from then on.
struct Stalled {
    inner: audio::SimDevice,
    until: TimeUs,
}

impl audio::AudioOutput for Stalled {
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
        self.inner.write(f)
    }
}

impl game::Wakes for Stalled {
    fn next_wake(&self) -> TimeUs {
        TimeUs(self.inner.next_wake().0.max(self.until.0))
    }
}

/// A gated session of `chart` with the survival gauge shown under
/// `on_failure`, its audio stalled until `until_ms` of session time.
fn session(root: &DataRoot, chart: &Chart, until_ms: i64) -> Session<Stalled> {
    session_with_sources(root, chart, until_ms, common::sources(), 1)
}

/// [`session`] with the source set's limits.
fn session_with_sources(
    root: &DataRoot,
    chart: &Chart,
    until_ms: i64,
    sources: input::SourceConfig,
    displayed: usize,
) -> Session<Stalled> {
    let mut settings = tapestry_settings(root);
    settings.player.displayed_gauge = displayed;
    settings.player.on_failure = FailureAction::Close;
    let parts = Parts {
        ingest: Box::new(Built(chart.clone())),
        ..Parts::standard(root, &settings.player)
    };
    let config = SessionConfig {
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
    };
    let loaded = Loaded::load(root.clone(), settings, &chart_file(root), config, parts).unwrap();
    let engine = audio::AudioEngine::new(
        common::bank(chart),
        &chart.bgm,
        RATE,
        PERIOD,
        audio::MAX_VOICES,
        64,
    )
    .unwrap();
    let out = Stalled {
        inner: audio::SimDevice::new(audio::SimConfig::steady(RATE, PERIOD, TimeUs(0))),
        until: TimeUs(until_ms * MS),
    };
    let mut s = loaded.start(
        RunnerAudio::new(engine, out),
        SourceSet::new(sources).unwrap(),
    );
    let d = DeviceSettings::default();
    let keys = KeyConfigs {
        keyboard: d.keyboard,
        controller: d.controller,
        controller_device: None,
    };
    let plan = AdjustmentConfig::default().plan(&keys, None, None).unwrap();
    let options = PlayOptions::new(
        HiSpeedSettings {
            mode: SpeedMode::Classic,
            ..HiSpeedSettings::default()
        },
        game::Covers::default(),
    )
    .unwrap();
    s.with_start_gate(&plan, options).unwrap();
    s
}

fn item(at: i64, action: Action) -> ScriptItem {
    ScriptItem {
        at: TimeUs(at),
        action,
    }
}

/// Keyboard 0 sets `control` to `value` at `at` ms.
fn raw(at: i64, control: ControlId, value: i32) -> ScriptItem {
    item(
        at * MS,
        Action::Packet(Packet {
            device: DeviceId(0),
            control,
            value,
            background: false,
        }),
    )
}

fn connect() -> ScriptItem {
    item(i64::MIN, Action::Connect(DeviceId(0), DeviceKind::Keyboard))
}

/// Times in ms on the script's timeline (the first opening's chart time).
fn script(items: Vec<ScriptItem>, commands: &[(i64, Command)]) -> PlayScript {
    let recorded = DeviceSettings::default().keyboard.recorded();
    PlayScript {
        input: ScriptedInput::new(items).with_recorded(DeviceId(0), recorded),
        commands: commands
            .iter()
            .map(|&(at, c)| (TimeUs(at * MS), c))
            .collect(),
    }
}

fn status(report: &PlayReport) -> replay::Status {
    report.record.as_ref().unwrap().status
}

/// The finalized watermark of the play that fails with its transport
/// committed from the start.
fn failing_watermark(chart: &Chart) -> TimeUs {
    let (_dir, root) = temp_root();
    let mut s = session(&root, chart, 0);
    let report = s.play(script(vec![connect()], &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
    status(&report).finalized
}

/// A failure while the transport is not yet seen committed is retained:
/// the frozen steps that follow score nothing, so the sixteenth miss,
/// which would complete the play at 2.68 s, never overtakes it; at the
/// commit (3 s) the play enters Closing, ended midway at the failing
/// watermark, and re-evaluates to its live result.
#[test]
fn a_failure_before_the_commit_closes_the_play_at_the_commit() {
    let chart = notes(16);
    let watermark = failing_watermark(&chart);
    let (_dir, root) = temp_root();
    let mut s = session(&root, &chart, ZERO + 3_000);
    let report = s.play(script(vec![connect()], &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(AbortCause::GaugeFailed));
    let closing = report.closing.unwrap();
    // Seen committed in the step after the pass that committed.
    assert_eq!(closing.started_at, TimeUs(3_001 * MS));
    assert_eq!(closing.end, ClosingEnd::RanOut);
    assert_eq!(closing.ended_at, TimeUs(4_001 * MS));
    let st = status(&report);
    assert_eq!(st.end, EndStatus::EndedMidway);
    assert_eq!(st.finalized, watermark);
    assert!(!st.incomplete.any());
    let result = report.result.as_ref().unwrap();
    assert!(result.matches_live);
    let gauges = &result.evaluation.as_ref().unwrap().gauges;
    assert_eq!(gauges[1].failed_at, Some(TimeUs(2_580 * MS)));
}

/// The same failure in a play of fifteen notes, whose last miss both
/// completes the play and fails the gauge in one advance: completion wins
/// even with the transport committed late.
#[test]
fn completion_and_failure_in_one_advance_complete_the_play() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &notes(15), ZERO + 3_000);
    let report = s.play(script(vec![connect()], &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(report.closing, None);
}

/// A hold of START after the retained failure cancels the candidate and
/// its release rearms a fresh one: the failure is discarded with the
/// cancelled evaluation, and the next candidate, every note hit, plays to
/// its end with no Closing.
#[test]
fn a_cancellation_and_its_rearm_discard_a_retained_failure() {
    let (_dir, root) = temp_root();
    let mut s = session(&root, &notes(16), ZERO + 3_000);
    let mut items = vec![connect(), raw(2_700, Q, 1), raw(2_800, Q, 0)];
    // The rearmed READY opens at the release: its chart time 0 is 3.8 s.
    for i in 0..16 {
        let at = 3_800 + 1_000 + i * 100;
        items.push(raw(at, code(1), 1));
        items.push(raw(at + 40, code(1), 0));
    }
    let report = s.play(script(items, &[])).unwrap();
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(report.closing, None);
    let record = report.record.as_ref().unwrap();
    assert_eq!(record.status.end, EndStatus::PlayedToEnd);
    assert_eq!(record.history.openings.len(), 1);
}

/// The last device removed during the frozen steps aborts the play as an
/// advance would: removal before the commit leaves no played attempt;
/// removal in the step that sees the commit acts before the retained
/// failure.
#[test]
fn a_removal_during_the_frozen_steps_acts_before_the_retained_failure() {
    for (removed, played) in [(2_800, false), (3_001, true)] {
        let (_dir, root) = temp_root();
        let mut s = session(&root, &notes(16), ZERO + 3_000);
        let items = vec![
            connect(),
            item(removed * MS, Action::Disconnect(DeviceId(0))),
        ];
        let report = s.play(script(items, &[])).unwrap();
        assert_eq!(
            report.end,
            PlayEnd::Aborted(AbortCause::LastSourceRemoved),
            "{removed}"
        );
        assert_eq!(report.closing, None, "{removed}");
        assert_eq!(report.played(), played, "{removed}");
    }
}

/// The quit key at or after the chart's start while the transport is
/// cached uncommitted: the gate's settled outcome decides. Settled
/// committed (the commit came in the quit's own wait), the play enters
/// Closing; settled cancelled, it leaves with no Closing, cancelled
/// before playback.
#[test]
fn a_quit_while_cached_uncommitted_follows_the_settled_gate() {
    for (quit, closes) in [(1_500, false), (3_001, true)] {
        let (_dir, root) = temp_root();
        // One note at 5 s: nothing completes or fails meanwhile.
        let chart = ChartBuilder::new(150_000)
            .sound("bgm.wav")
            .bgm(TimeUs(0), SoundId(0))
            .tap(lane(1), TimeUs(5_000 * MS))
            .build()
            .unwrap();
        let mut s = session(&root, &chart, ZERO + 3_000);
        let report = s
            .play(script(vec![connect()], &[(quit, Command::Quit)]))
            .unwrap();
        assert_eq!(report.end, PlayEnd::Aborted(AbortCause::Quit), "{quit}");
        assert_eq!(report.closing.is_some(), closes, "{quit}");
        assert_eq!(
            status(&report).end,
            if closes {
                EndStatus::EndedMidway
            } else {
                EndStatus::CancelledBeforePlayback
            },
            "{quit}"
        );
    }
}

/// An adjustment during Closing works as in the tail, with no scene
/// change at Closing's entry: a fine hi-speed gesture (START with the
/// scratch held, +0.01 at the press and every 50 ms after) begun at 2.1 s,
/// before the failure, goes on repeating through Closing until its
/// release at 3.4 s: the press and 25 repeats, 1.26. A scene change at the
/// entry would end its context, and the held scratch would act no more.
#[test]
fn an_adjustment_in_closing_keeps_its_gesture() {
    let scratch = ControlId(DeviceSettings::default().keyboard.scratch_positive.unwrap());
    let (_dir, root) = temp_root();
    let mut s = session(&root, &notes(16), 0);
    let items = vec![
        connect(),
        raw(2_000, Q, 1),
        raw(2_100, scratch, 1),
        raw(3_400, scratch, 0),
    ];
    let report = s.play(script(items, &[])).unwrap();
    let closing = report.closing.unwrap();
    assert!(closing.started_at < TimeUs(2_700 * MS), "{closing:?}");
    assert!(TimeUs(3_400 * MS) < closing.ended_at, "{closing:?}");
    let snap = s.presentation().unwrap();
    assert!(snap.adjusting, "START is still held");
    assert_eq!(snap.hispeed, Some(game::Ratio::from_hundredths(126)));
}

/// The record's source-limit flag is the one recording had when Closing
/// sealed it, whatever arrives during Closing: with one source allowed, a
/// second keyboard connected before the opening marks it and the mark
/// stays through Closing; one arriving during Closing marks nothing.
#[test]
fn the_source_limit_flag_is_frozen_in_closing() {
    for (second_at, marked) in [(i64::MIN, true), (3_000 * MS, false)] {
        let (_dir, root) = temp_root();
        let sources = input::SourceConfig {
            max_sources: 1,
            ..common::sources()
        };
        let mut s = session_with_sources(&root, &notes(16), 0, sources, 1);
        let items = vec![
            connect(),
            item(
                second_at,
                Action::Connect(DeviceId(1), DeviceKind::Keyboard),
            ),
            // And a third during Closing, in both.
            item(
                3_100 * MS,
                Action::Connect(DeviceId(2), DeviceKind::Keyboard),
            ),
        ];
        let report = s.play(script(items, &[])).unwrap();
        let closing = report.closing.unwrap();
        assert!(closing.started_at < TimeUs(3_000 * MS), "{second_at}");
        assert_eq!(
            status(&report).incomplete.source_limit,
            marked,
            "{second_at}"
        );
    }
}
