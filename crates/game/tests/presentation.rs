//! Play admission of a chart's presentation (design.md "Chart Model",
//! "Timeline"): a chart whose note endpoints at one µs sit at different
//! positions at its loaded speed is refused when it is loaded for play or
//! moved to that speed, the session unchanged, while reading it for
//! analysis (`Loaded::chart_at`, `ChartSource::read`) still works.

mod common;

use std::path::Path;

use bms_model::{
    BEAT, EndpointLocation, EndpointPart, Lane, Position, PresentationError, Speed, TimeUs,
    TimelineId,
};
use common::{MS, tapestry_settings};
use game::{DataRoot, Loaded, Parts, Practice, SessionConfig, SessionError};
use replay::{BuildProvenance, ReceiveMethod};

fn config(speed: Speed) -> SessionConfig {
    SessionConfig {
        lead_in: TimeUs(1_000 * MS),
        step: TimeUs(MS),
        start: TimeUs(1_000 * MS),
        effects: 4_096,
        seed: 0,
        speed,
        app_version: "0.0.0-test".to_owned(),
        provenance: BuildProvenance::Development,
        receive: ReceiveMethod::Scripted,
        autoplay: None,
    }
}

/// `text` (a BMS chart) loaded for play at `speed` under the tapestry
/// preset, with the standard parts.
fn load(dir: &Path, text: &str, speed: Speed) -> Result<Loaded, SessionError> {
    let root = DataRoot::new(dir.to_owned());
    let settings = tapestry_settings(&root);
    let chart_path = dir.join("chart.bms");
    std::fs::write(&chart_path, text).unwrap();
    let parts = Parts::standard(&root, &settings.player);
    Loaded::load(root, settings, &chart_path, config(speed), parts)
}

fn at(k: u32) -> Speed {
    Speed::from_twentieths(k).unwrap()
}

/// `slots` object pairs with `01` at `index` and `00` elsewhere.
fn line(slots: usize, index: usize) -> String {
    (0..slots)
        .map(|i| if i == index { "01" } else { "00" })
        .collect()
}

/// Two taps in measure 0 at 150 BPM (1.6 s a measure): key 1 at `first`,
/// key 2 at `second`, each `(index, slots)`. Row 0 is the bar line, rows 1
/// and 2 the taps, the earlier first.
fn pair(first: (usize, usize), second: (usize, usize)) -> String {
    format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{}\n#00012:{}\n",
        line(first.1, first.0),
        line(second.1, second.0)
    )
}

/// The conflict of [`pair`] at `at` µs.
fn conflict(at: i64, first: (i128, i128), second: (i128, i128)) -> PresentationError {
    let head = |lane, row| EndpointLocation {
        lane: Lane::new(lane).unwrap(),
        index: 0,
        part: EndpointPart::Head,
        timeline_id: TimelineId(row),
    };
    let pos = |(i, n): (i128, i128)| Position(BEAT * 4 * i / n);
    PresentationError::EndpointConflict {
        at: TimeUs(at),
        first: head(1, 1),
        second: head(2, 2),
        first_pos: pos(first),
        second_pos: pos(second),
    }
}

fn refused(speed: Speed, reason: PresentationError) -> SessionError {
    SessionError::Presentation {
        speed,
        reason: Box::new(reason),
    }
}

#[test]
fn a_chart_conflicting_at_its_speed_is_refused_at_the_choice() {
    // 1000/2001 and 1001/2003 both floor to 799 600 µs at 100 %; at 50 %
    // they are 1 599 200 and 1 599 201.
    let text = pair((1000, 2001), (1001, 2003));
    let dir = tempfile::tempdir().unwrap();
    let e = load(dir.path(), &text, Speed::NORMAL).err();
    let reason = conflict(799_600, (1000, 2001), (1001, 2003));
    assert_eq!(e, Some(refused(Speed::NORMAL, reason)));
    // The message names the speed; the reason, its source, both endpoints
    // and their µs.
    let e = e.unwrap();
    assert_eq!(e.to_string(), "the chart cannot be played at 100% speed");
    let source = std::error::Error::source(&e).unwrap().to_string();
    assert!(
        source.contains("lane 1 note 0 head (timeline row 1)"),
        "{source}"
    );
    assert!(
        source.contains("lane 2 note 0 head (timeline row 2)"),
        "{source}"
    );
    assert!(source.contains("799600 µs"), "{source}");
}

#[test]
fn analysis_reads_a_chart_play_admits_only_at_another_speed() {
    let text = pair((1000, 2001), (1001, 2003));
    let dir = tempfile::tempdir().unwrap();
    let mut loaded = load(dir.path(), &text, Speed::SLOWEST).unwrap();
    // The loudness analysis reads the normal-speed chart, which play would
    // refuse, through both readers.
    let normal = loaded.chart_at(Speed::NORMAL).unwrap();
    assert_eq!(
        normal.validate_presentation(),
        Err(conflict(799_600, (1000, 2001), (1001, 2003)))
    );
    let parts = Parts::standard(loaded.root(), &loaded.settings().player);
    let read = loaded.source().read(&parts, Speed::NORMAL).unwrap();
    assert_eq!(read, normal);

    // Moving play to that speed is refused whole: the chart, the speed and
    // the practice set up all stay.
    let practice = Practice {
        start: TimeUs(0),
        end: TimeUs(2_000 * MS),
        lead_in: TimeUs(1_000 * MS),
        initial_gauges: loaded.preset().gauges.iter().map(|g| g.start).collect(),
        repeat: true,
    };
    loaded.set_practice(Some(practice.clone())).unwrap();
    let before = loaded.chart().clone();
    assert_eq!(
        loaded.set_speed(Speed::NORMAL),
        Err(refused(
            Speed::NORMAL,
            conflict(799_600, (1000, 2001), (1001, 2003))
        ))
    );
    assert_eq!(loaded.chart(), &before);
    assert_eq!(loaded.config().speed, Speed::SLOWEST);
    assert_eq!(loaded.practice(), Some(&practice));
    // Another speed it can be presented at is taken.
    loaded.set_speed(at(12)).unwrap();
    assert_eq!(loaded.config().speed, at(12));
}

#[test]
fn a_normal_speed_chart_can_conflict_at_a_practice_speed() {
    // At 150 BPM: 1003/2003 and 1002/2001 are 801 198 and 801 199 µs at
    // 100 %, both 534 132 at 150 %; 8/10009 and 8/10007 are 1 278 and
    // 1 279 µs, both 1 598 at 80 %.
    for (first, second, speed, at_speed) in [
        ((1003, 2003), (1002, 2001), at(30), 534_132),
        ((8, 10009), (8, 10007), at(16), 1_598),
    ] {
        let text = pair(first, second);
        let dir = tempfile::tempdir().unwrap();
        let mut loaded = load(dir.path(), &text, Speed::NORMAL).unwrap();
        let frac =
            |(i, n): (usize, usize)| (i128::try_from(i).unwrap(), i128::try_from(n).unwrap());
        let reason = conflict(at_speed, frac(first), frac(second));
        assert_eq!(
            loaded.set_speed(speed),
            Err(refused(speed, reason)),
            "{}/20",
            speed.twentieths()
        );
        assert_eq!(loaded.config().speed, Speed::NORMAL);
        // Analysis still reads it at that speed.
        let mapped = loaded.chart_at(speed).unwrap();
        assert_eq!(mapped.validate_presentation(), Err(reason));
        // A load straight at that speed is refused too.
        assert_eq!(
            load(dir.path(), &text, speed).err(),
            Some(refused(speed, reason))
        );
    }
}
