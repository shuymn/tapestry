//! A full play's ending allocates nothing (design.md "Scope of No
//! Allocation", "Play Lifecycle", the audio tail): through the production
//! lifecycle (`game::live::play`), from the end of the completing step's
//! advance on, the completion and the source set's handoff, the tail
//! stage's set-up (its finish anchor and the skin's declared ending read),
//! every hold wake with its presses' conversion and key sounds, the
//! fade-out's publication (`Logic::enter_fade`), and every fade-out wake
//! with its unconverted presses, up to the final close. The rig's own
//! waits, standing in for the other threads, are not counted. This file
//! holds a single test, so the harness's other threads are not tracked.

mod common;
mod counting;

use std::path::Path;

use bms_model::{Chart, ChartBuilder, SoundId, TimeUs};
use common::{Built, MS, lane, tapestry_settings};
use eval::ControlId;
use game::{
    AudioQueue, Closing, Command, DataRoot, DeviceSettings, InputWaitError, Loaded, Opening, Parts,
    PlayEnd, PlayEnding, PlayReport, RenderSide, Rig, SessionConfig,
};
use input::{DeviceId, DeviceKind, Packet, SourceReader, SourceSet};
use replay::{BuildProvenance, ReceiveMethod};

#[global_allocator]
static GLOBAL: counting::Counting = counting::Counting;

const KEYBOARD: DeviceId = DeviceId(0);

/// A rig on a 1 ms clock whose input side is a source set in the test's
/// thread: each wait publishes a batch, with key 1 pressed or released at
/// the clock times `presses` gives. Counting runs from the end of each
/// advance (`measured`) to the next wait, and through the tail's wakes
/// once the audio is sealed, until the final close.
struct Counted {
    now: i64,
    sources: SourceSet,
    presses: Vec<(i64, i32)>,
    tail: bool,
    calls: u64,
    /// The count once the positive control ran.
    base: u64,
    checked: bool,
    sounds: Option<AudioQueue>,
    screen: Option<RenderSide>,
    /// Key sounds sent and the latest fade-out the view published.
    key_sounds: usize,
    fading: Option<TimeUs>,
}

/// The control the default key config binds to lane `n`.
fn code(n: u8) -> ControlId {
    let keys = DeviceSettings::default().keyboard;
    ControlId(keys.lanes.iter().find(|k| k.lane == n).unwrap().control)
}

impl Counted {
    fn new(presses: Vec<(i64, i32)>) -> Self {
        let mut sources = SourceSet::new(common::sources()).unwrap();
        sources.begin_batch(TimeUs(0));
        sources.connect(KEYBOARD, DeviceKind::Keyboard);
        sources.set_recorded(KEYBOARD, DeviceSettings::default().keyboard.recorded());
        sources.end_batch(TimeUs(0));
        Self {
            now: 0,
            sources,
            presses,
            tail: false,
            calls: 0,
            base: 0,
            checked: false,
            sounds: None,
            screen: None,
            key_sounds: 0,
            fading: None,
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
    type ResultPrep = ();

    fn now(&self) -> TimeUs {
        TimeUs(self.now)
    }

    fn wait(&mut self) {
        counting::track(false);
        self.now += MS;
        self.sources.begin_batch(TimeUs(self.now));
        while let Some(&(at, value)) = self.presses.first()
            && at <= self.now
        {
            self.presses.remove(0);
            self.sources.packet(&Packet {
                device: KEYBOARD,
                control: code(1),
                value,
                background: false,
            });
        }
        self.sources.end_batch(TimeUs(self.now));
        if let Some(screen) = &mut self.screen {
            screen.drain_effects(|_| {});
            self.fading = screen.view().fading;
        }
        if let Some(sounds) = &mut self.sounds {
            while sounds.commands().pop().is_ok() {
                self.key_sounds += 1;
            }
        }
        if self.tail {
            self.on();
        }
    }

    fn measured(&mut self, _: &game::Advance, _: &[eval::JudgeEvent], _: TimeUs) {
        self.on();
    }

    fn take(&mut self) -> Option<Command> {
        None
    }

    fn quitting(&self) -> bool {
        false
    }

    fn declared_ending(&self) -> PlayEnding {
        PlayEnding {
            finish_margin: TimeUs(3_000 * MS),
            fadeout: TimeUs(1_000 * MS),
        }
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

    fn seal_audio(&mut self) {
        self.tail = true;
    }

    fn ring_out(&mut self, _fade_at: TimeUs, _end_at: TimeUs) {}

    fn audio_done(&self) -> bool {
        true
    }

    fn close(&mut self) -> Result<Closing, InputWaitError> {
        panic!("a completed play hands its sources off");
    }

    fn hand_off(&mut self) -> Result<Closing, InputWaitError> {
        self.sources.hand_off();
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
        self.tail = false;
        self.sources.close_recording();
        Ok(())
    }

    fn ended(&mut self, _report: &PlayReport) {}

    fn show_end(&mut self, _report: &PlayReport, _screen: &game::ResultScreen) {}
}

fn loaded(dir: &Path, chart: Chart) -> Loaded {
    let root = DataRoot::new(dir.to_owned());
    let settings = tapestry_settings(&root);
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
fn the_ending_allocates_nothing() {
    // One sounded note on key 1 at 1 s of chart time (2 s on the clock,
    // READY opening at 0): its hold lasts to 9 s of chart time and its
    // fade-out to 10 s.
    let mut chart = ChartBuilder::new(150_000)
        .sound("key.wav")
        .tap(lane(1), TimeUs(1_000 * MS))
        .build()
        .unwrap();
    chart.lanes[1][0].sound = Some(SoundId(0));
    let dir = tempfile::tempdir().unwrap();
    let mut play = loaded(dir.path(), chart);
    let on_clock = |chart_ms: i64| (chart_ms + 1_000) * MS;
    let mut rig = Counted::new(vec![
        (on_clock(1_000), 1),
        (on_clock(1_050), 0),
        // In the hold.
        (on_clock(4_000), 1),
        (on_clock(4_050), 0),
        // In the fade-out.
        (on_clock(9_500), 1),
        (on_clock(9_550), 0),
    ]);
    let report = game::live::play(&mut play, &mut rig).unwrap().unwrap();
    assert_eq!(report.end, PlayEnd::Completed);
    assert_eq!(report.tail_cut, None);
    assert!(rig.checked);
    // The view published the fade-out at the hold's end.
    assert_eq!(rig.fading, Some(TimeUs(9_000 * MS)));
    // The hit's sound and the hold's press; never the fade-out's.
    assert_eq!(rig.key_sounds, 2);
    assert_eq!(rig.calls, 0, "the ending allocated");
}
