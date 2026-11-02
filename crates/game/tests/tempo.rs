//! The attempt's tempo provider through the session (design.md "Play
//! Adjustments", tempo; "Practice Speed Changes"): each attempt reads the
//! chart as loaded at its speed, the reducer applies the speed once to the
//! references and to the event-time lookup, and a practice round weighs
//! MAIN by its own targets from the 1× page's bar-line times.

mod common;

use std::fs;

use bms_model::{Speed, TimeUs};
use common::tapestry_settings;
use game::adjustment::{
    Adjustment, ControlId, ControlRoles, Event, Head, Observation, Role, TempoMap,
};
use game::{
    Cover, Covers, DataRoot, HiSpeedSettings, Loaded, Parts, PlayOptions, PracticeInput,
    PracticeScreen, Ratio, References, SessionConfig, SpeedMode, solve,
};
use replay::{BuildProvenance, ReceiveMethod};

/// 150 BPM: a tap at measure 1 (1.6 s); 300 BPM from measure 2 (3.2 s),
/// with taps at 3.2 s and 3.6 s.
const CHART: &str = "#BPM 150\n#BPM01 300\n#WAV01 a.wav\n#00111:01\n#00208:01\n#00211:0101\n";

const SELECT: ControlId = ControlId(1);
const WHITE: ControlId = ControlId(6);
const BLACK: ControlId = ControlId(7);

fn config() -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000_000),
        step: TimeUs(1_000),
        start: TimeUs(0),
        effects: 4_096,
        seed: 7,
        speed: Speed::NORMAL,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

fn load() -> (tempfile::TempDir, Loaded) {
    let dir = tempfile::Builder::new()
        .prefix("tempo")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().to_owned());
    fs::create_dir_all(root.path()).unwrap();
    let path = root.path().join("song.bms");
    fs::write(&path, CHART).unwrap();
    let settings = tapestry_settings(&root);
    let parts = Parts::standard(&root, &settings.player);
    let loaded = Loaded::load(root.clone(), settings, &path, config(), parts).unwrap();
    (dir, loaded)
}

fn bpm(n: u64) -> Ratio {
    Ratio::integer(n)
}

fn sudden() -> Covers {
    Covers {
        sudden: Cover {
            amount: 200,
            on: true,
        },
        ..Covers::default()
    }
}

/// A Floating reducer, target green 300 and SUDDEN+ 200 ‰, prepared from
/// START with the provider's references and speed.
fn prepared(tempo: &game::AttemptTempo) -> Adjustment {
    let mut roles = ControlRoles::default();
    for (c, role) in [
        (SELECT, Role::Select),
        (WHITE, Role::White),
        (BLACK, Role::Black),
    ] {
        assert!(roles.set(c, role));
    }
    let options = PlayOptions::new(
        HiSpeedSettings {
            mode: SpeedMode::Floating,
            green_target: 300,
            ..HiSpeedSettings::default()
        },
        sudden(),
    )
    .unwrap();
    let mut a = Adjustment::new(options, roles, TimeUs(-1_000_000), 0);
    a.observe(
        Observation {
            time: TimeUs(0),
            seq: 0,
            event: Event::Prepare {
                references: tempo.references().unwrap(),
                speed: tempo.speed(),
            },
        },
        tempo,
    );
    a
}

/// At 150 % the change to 300 BPM is at playback time
/// ⌊3 200 000 · 20/30⌋ = 2 133 333 µs. After the speed change the attempt
/// reads the newly loaded timeline: its lookup answers the original 300
/// there (150 at 1×), its references stay original, and the reducer
/// solves from 150 · 1.5 = 225 at preparation and from 300 · 1.5 = 450 at
/// a later event in the 300 BPM part, never scaling twice.
#[test]
fn a_speed_change_brings_the_new_timeline_and_scales_once() {
    let (_dir, mut loaded) = load();
    let change = TimeUs(2_133_333);
    let normal = loaded.attempt().tempo;
    assert_eq!(normal.speed(), Speed::NORMAL);
    assert_eq!(normal.bpm_at(change), Some(bpm(150)));

    let fast = Speed::from_twentieths(30).unwrap();
    loaded.set_speed(fast).unwrap();
    let tempo = loaded.attempt().tempo;
    assert_eq!(tempo.speed(), fast);
    assert_eq!(
        tempo.timeline().points().as_ptr(),
        loaded.chart().timeline.points().as_ptr()
    );
    assert_eq!(tempo.bpm_at(TimeUs(change.0 - 1)), Some(bpm(150)));
    assert_eq!(tempo.bpm_at(change), Some(bpm(300)));
    assert_eq!(tempo.section(), None);
    assert_eq!(
        tempo.references(),
        Ok(References {
            start: bpm(150),
            maximum: bpm(300),
            main: bpm(300),
            minimum: bpm(150),
        })
    );

    let mut a = prepared(&tempo);
    assert_eq!(
        a.snapshot().hispeed,
        Some(solve(bpm(225), 300, &sudden()).unwrap())
    );
    // In the 300 BPM part: a target step and back, each solving at the
    // event's tempo.
    let mut seq = 0;
    let mut ev = |a: &mut Adjustment, at: i64, event: Event| {
        seq += 1;
        a.observe(
            Observation {
                time: TimeUs(at),
                seq,
                event,
            },
            &tempo,
        );
    };
    ev(
        &mut a,
        1_000,
        Event::Phase {
            head: Head::Clock {
                at: TimeUs(1_000),
                chart: change,
            },
            generation: 1,
        },
    );
    ev(&mut a, 2_000, Event::Press(SELECT));
    ev(&mut a, 2_000, Event::Press(BLACK));
    ev(&mut a, 2_000, Event::Release(BLACK));
    ev(&mut a, 3_000, Event::Press(WHITE));
    ev(&mut a, 3_000, Event::Release(WHITE));
    ev(&mut a, 3_000, Event::Release(SELECT));
    ev(&mut a, 4_000, Event::Tick);
    assert_eq!(a.options().hispeed().green_target, 300);
    assert_eq!(
        a.snapshot().hispeed,
        Some(solve(bpm(450), 300, &sudden()).unwrap())
    );
}

/// A practice round from measure 1 to measure 2 at 150 %: the page gives
/// the bar lines' 1× times, the session maps them once, and the round's
/// provider keeps that section and weighs MAIN by its one tap at 150
/// while the whole chart's MAIN is 300; START, MIN and MAX stay the
/// whole chart's.
#[test]
fn a_practice_round_weighs_main_by_its_section() {
    let (_dir, mut loaded) = load();
    let mut page = PracticeScreen::new(loaded.chart(), loaded.preset(), 0);
    assert_eq!(
        page.measures(),
        [TimeUs(0), TimeUs(1_600_000), TimeUs(3_200_000)]
    );
    page.input(PracticeInput::Right);
    page.input(PracticeInput::Down);
    page.input(PracticeInput::Left);
    assert_eq!((page.from(), page.to()), (1, 2));
    let fast = Speed::from_twentieths(30).unwrap();
    loaded.set_speed(fast).unwrap();
    let practice = page.practice(&loaded);
    assert_eq!(
        (practice.start, practice.end),
        (TimeUs(1_600_000), TimeUs(3_200_000))
    );
    loaded.set_practice(Some(practice)).unwrap();
    let section = loaded.section().unwrap();
    assert_eq!(
        (section.start, section.end),
        (TimeUs(1_066_666), TimeUs(2_133_333))
    );
    let tempo = loaded.attempt().tempo;
    assert_eq!(tempo.section(), Some((section.start, section.end)));
    assert_eq!(
        tempo.references(),
        Ok(References {
            start: bpm(150),
            maximum: bpm(300),
            main: bpm(150),
            minimum: bpm(150),
        })
    );
}
