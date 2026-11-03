//! Closing allocates nothing (design.md "Scope of No Allocation", "Play
//! Lifecycle", Closing): through the production lifecycle
//! (`game::live::play`), from the end of each step's advance on, the
//! failing step's decisions, the logic's abort, Closing's admission and
//! publication (`Logic::enter_closing`), the input side's seal
//! (`SourceSet::seal_recording`, as the input thread runs it for the
//! abort close), the handshake's drain, the record's drain after the
//! close, and every Closing wake's drain of commands, its failure polls
//! and its exit, up to the final close. A quit admitted to Closing,
//! repeated in it and ended by a retry, takes the same path. The rig's
//! own waits, standing in for the other threads, are not counted. This
//! file holds a single test, so the harness's other threads are not
//! tracked.

mod common;
mod counting;

use std::path::Path;

use bms_model::{Chart, ChartBuilder, TimeUs};
use common::{MS, lane, tapestry_settings};
use game::{
    AudioQueue, ChartIngest, Closing, ClosingEnd, Command, DataRoot, FailureAction, IngestError,
    Ingested, InputWaitError, Loaded, Opening, Parts, PlayEnd, PlayReport, RenderSide, Rig,
    SessionConfig,
};
use input::{DeviceId, DeviceKind, SourceReader, SourceSet};
use replay::{BuildProvenance, ReceiveMethod};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

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

/// A rig on a 1 ms clock whose input side is a source set in the test's
/// thread: each wait publishes an empty batch. Counting runs from the end
/// of each advance (`measured`) to the next wait, and through Closing's
/// wakes, until the final close.
struct Counted {
    now: i64,
    sources: SourceSet,
    commands: [(i64, Command); 4],
    next: usize,
    closing: bool,
    calls: u64,
    /// The count once the positive control ran.
    base: u64,
    sounds: Option<AudioQueue>,
    screen: Option<RenderSide>,
    checked: bool,
}

impl Counted {
    fn new(commands: [(i64, Command); 4]) -> Self {
        let mut sources = SourceSet::new(common::sources()).unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(DeviceId(0), DeviceKind::Keyboard);
        sources.set_recorded(
            DeviceId(0),
            game::DeviceSettings::default().keyboard.recorded(),
        );
        sources.end_batch(TimeUs(0));
        Self {
            now: 0,
            sources,
            commands,
            next: 0,
            closing: false,
            calls: 0,
            base: 0,
            sounds: None,
            screen: None,
            checked: false,
        }
    }

    fn on(&mut self) {
        if !self.checked {
            counting::track(true);
            counting::self_check();
            self.checked = true;
            self.base = counting::calls();
        }
        counting::track(true);
    }
}

impl Rig for Counted {
    type Reader = SourceReader;
    type ScreenError = std::convert::Infallible;

    fn now(&self) -> TimeUs {
        TimeUs(self.now)
    }

    fn wait(&mut self) {
        counting::track(false);
        self.now += MS;
        self.sources.begin_batch(TimeUs(self.now));
        self.sources.end_batch(TimeUs(self.now));
        if let Some(screen) = &mut self.screen {
            screen.drain_effects(|_| {});
        }
        if let Some(sounds) = &mut self.sounds {
            while sounds.commands().pop().is_ok() {}
        }
        if self.closing {
            self.on();
        }
    }

    fn measured(&mut self, _: &game::Advance, _: &[eval::JudgeEvent], _: TimeUs) {
        self.on();
    }

    fn take(&mut self) -> Option<Command> {
        let &(at, c) = self.commands.get(self.next)?;
        (at <= self.now).then(|| {
            self.next += 1;
            c
        })
    }

    fn quitting(&self) -> bool {
        false
    }

    fn prepare_screen(&mut self) -> Result<(), Self::ScreenError> {
        Ok(())
    }

    fn open(&mut self, lead_in: TimeUs) -> Result<Opening<SourceReader>, InputWaitError> {
        let origin = TimeUs(self.now + lead_in.0);
        let start = self.sources.open_play(TimeUs(self.now), origin);
        Ok(Opening {
            origin,
            start,
            readers: self.sources.take_readers(),
            kinds: vec![DeviceKind::Keyboard],
            axes: vec![Vec::new()],
        })
    }

    fn start(&mut self, _origin: TimeUs, sounds: AudioQueue, screen: RenderSide) {
        self.sounds = Some(sounds);
        self.screen = Some(screen);
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

    fn cancel_audio(&mut self) {}

    fn seal_audio(&mut self) {}

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {}

    fn audio_done(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        panic!("a play in Closing is sealed, not closed");
    }

    fn abort_close(&mut self) -> Result<Closing, InputWaitError> {
        self.sources.seal_recording();
        self.closing = true;
        Ok(Closing {
            arrivals: Vec::new(),
            overflowed: self.sources.overflowed(),
            source_limited: self.sources.source_limited(),
            axis_out_of_range: Vec::new(),
        })
    }

    fn end_feedback(&mut self) -> Result<(), InputWaitError> {
        self.calls = counting::calls() - self.base;
        counting::track(false);
        self.closing = false;
        self.sources.close_recording();
        Ok(())
    }

    fn ended(&mut self, _report: &PlayReport) {}

    fn show_end(&mut self, _report: &PlayReport, _screen: &game::ResultScreen) {}
}

fn loaded(dir: &Path, chart: Chart) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let mut settings = tapestry_settings(&root);
    settings.player.displayed_gauge = 1;
    settings.player.on_failure = FailureAction::Close;
    let path = dir.join("play.chart");
    std::fs::write(&path, b"built in code").unwrap();
    let parts = Parts {
        ingest: Box::new(Built(chart)),
        ..Parts::unsaved(&settings.player)
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
    Loaded::load(root, settings, &path, config, parts).unwrap()
}

#[test]
fn closing_allocates_nothing() {
    // Sixteen unpressed notes: the survival gauge fails at 2.58 s.
    let sixteen = (0..16)
        .fold(ChartBuilder::new(150_000), |b, i| {
            b.tap(lane(1), TimeUs((1_000 + i * 100) * MS))
        })
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut failed = loaded(dir.path(), sixteen.clone());
    let never = (i64::MAX, Command::Quit);
    let mut rig = Counted::new([never; 4]);
    let report = game::live::play(&mut failed, &mut rig).unwrap().unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(game::AbortCause::GaugeFailed));
    assert_eq!(report.closing.map(|c| c.end), Some(ClosingEnd::RanOut));
    assert!(rig.checked);
    assert_eq!(rig.calls, 0, "the failure's Closing allocated");

    // The quit key at 1.5 s of play time (READY opens at 0), again in
    // Closing, then a retry beside a suspend.
    let dir = tempfile::tempdir().unwrap();
    let mut quit = loaded(dir.path(), sixteen);
    let mut rig = Counted::new([
        (2_500 * MS, Command::Quit),
        (2_800 * MS, Command::Quit),
        (3_000 * MS, Command::Suspend),
        (3_000 * MS, Command::Retry),
    ]);
    let report = game::live::play(&mut quit, &mut rig).unwrap().unwrap();
    assert_eq!(report.end, PlayEnd::Aborted(game::AbortCause::Quit));
    assert_eq!(report.closing.map(|c| c.end), Some(ClosingEnd::Retry));
    assert_eq!(rig.calls, 0, "the quit's Closing allocated");
}
