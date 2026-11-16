//! The chart's BGA structure (`Chart::bga`; design.md "Chart Model", "What
//! the Loader Reads"): `#BMPxx` and channels 04, 06 and 07 read for
//! presentation alone, timed apart from the play chart, which keeps its
//! identity, length and stored values.

use bms_model::{
    BgaEvent, BgaImage, BgaImageId, Chart, ChartBga, ChartIdentity, ChartSummary, PoorSequence,
    Speed, TimeUs,
};
use bms_parser::{
    Limits, LoadError, LoadOptions, Loaded, load_bms, read_listing, read_scanned, summarize,
};

fn load_with(text: &str, opts: &LoadOptions) -> Result<Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    load_bms(text.as_bytes(), dir.path(), opts)
}

fn load_at(text: &str, speed: Speed) -> Loaded {
    let opts = LoadOptions {
        speed,
        ..LoadOptions::default()
    };
    load_with(text, &opts).unwrap()
}

fn load(text: &str) -> Loaded {
    load_at(text, Speed::NORMAL)
}

fn bga(text: &str) -> ChartBga {
    load(text).chart.bga.expect("the chart has BGA")
}

fn event(time: i64, image: Option<u16>) -> BgaEvent {
    BgaEvent {
        time: TimeUs(time),
        image: image.map(BgaImageId),
    }
}

fn image(id: u16, name: &str) -> BgaImage {
    BgaImage {
        id,
        name: name.to_owned(),
    }
}

/// 120 BPM; a STOP of two beats (1 s) at measure 1's start; 7 BPM from
/// measure 1's half (4 s) on, a beat 60/7 s. A key note at 0 and at two
/// thirds of measure 2 (with a base event there), a base event at a third
/// of measure 0, a layer event at three quarters of measure 1 and a poor
/// line on measure 2.
///
/// Exact times: base 0 at 2/3 s = 666 666.6 µs; layer at 4 s + 60/7 s =
/// 12 571 428.571 µs; measure 2 starts at 4 s + 120/7 s = 148/7 s, the
/// poor sequence's time, 21 142 857.142 µs, and the base event at two
/// thirds of it at 4 s + 280/7 s = 44 000 000 µs exactly, where flooring
/// the measure's start and the offset apart gives 43 999 999.
const TIMED: &str = "\
#BPM 120
#BPM01 7
#STOP01 96
#BMP01 a.bmp
#BMP02 b.bmp
#00011:01
#00004:000100
#00109:01
#00108:0001
#00107:00000001
#00204:000002
#00206:0102
#00211:000001
";

#[test]
fn base_and_layer_events_time_through_bpm_and_stop() {
    let bga = bga(TIMED);
    assert_eq!(
        bga.images(),
        [image(1, "a.bmp"), image(2, "b.bmp")].as_slice()
    );
    assert_eq!(
        bga.base(),
        [event(666_666, Some(0)), event(44_000_000, Some(1))].as_slice(),
        "base events at their exact times floored once"
    );
    assert_eq!(bga.layer(), [event(12_571_428, Some(0))].as_slice());
    assert_eq!(
        bga.poor(),
        [PoorSequence {
            time: TimeUs(21_142_857),
            frames: vec![Some(BgaImageId(0)), Some(BgaImageId(1))].into(),
        }]
        .as_slice()
    );
}

#[test]
fn practice_speed_maps_bga_like_notes() {
    for k in [10, 15, 27, 30] {
        let speed = Speed::from_twentieths(k).unwrap();
        let loaded = load_at(TIMED, speed);
        let bga = loaded.chart.bga.unwrap();
        // floor(T × 20 / k) from the exact T: 2/3 s, 88/7 s, 44 s, 148/7 s.
        let at = |numer: i128, denom: i128| {
            i64::try_from((numer * 20).div_euclid(denom * i128::from(k))).unwrap()
        };
        let times = |events: &[BgaEvent]| events.iter().map(|e| e.time.0).collect::<Vec<_>>();
        assert_eq!(
            times(bga.base()),
            [at(2_000_000, 3), at(44_000_000, 1)],
            "base at {k}/20"
        );
        assert_eq!(times(bga.layer()), [at(88_000_000, 7)], "layer at {k}/20");
        let poor: Vec<_> = bga.poor().iter().map(|p| p.time.0).collect();
        assert_eq!(poor, [at(148_000_000, 7)], "poor at {k}/20");
        // The note at the second base event's position has its time.
        assert_eq!(loaded.chart.lanes[1][1].time, bga.base()[1].time);
    }
}

#[test]
fn undefined_bmp_id_clears_the_layer() {
    let bga = bga("\
#BPM 120
#BMP01 a.bmp
#00011:01
#00007:01
#00107:02
#00204:03
");
    assert_eq!(
        bga.layer(),
        [event(0, Some(0)), event(2_000_000, None)].as_slice(),
        "an id no #BMPxx defines is a clearing event"
    );
    assert_eq!(bga.base(), [event(4_000_000, None)].as_slice());
}

/// Measure 1 is 0.000001 of a measure long, 2 µs at 120 BPM, so its
/// first quarter (0.5 µs) floors to the µs of its start.
#[test]
fn at_one_position_the_later_object_wins_and_close_positions_stay_apart() {
    let bga = bga("#BPM 120
#BMP01 a.bmp
#BMP02 b.bmp
#BMP03 c.bmp
#00011:01
#00102:0.000001
#00104:01000000
#00104:02
#00104:00030000
#00207:01
#00207:0300
");
    assert_eq!(
        bga.base(),
        [event(2_000_000, Some(1)), event(2_000_000, Some(2))].as_slice(),
        "the later line at one position wins; a distinct position on the same µs stays, in position order"
    );
    assert_eq!(bga.layer(), [event(2_000_002, Some(2))].as_slice());
}

#[test]
fn last_definition_of_an_id_wins() {
    let bga = bga("\
#BPM 120
#00011:01
#BMP01 first.bmp
#00004:01
#BMP01 second.bmp
#00104:01
");
    assert_eq!(bga.images(), [image(1, "second.bmp")].as_slice());
    assert_eq!(
        bga.base(),
        [event(0, Some(0)), event(2_000_000, Some(0))].as_slice()
    );
}

#[test]
fn bmp_definitions_follow_the_flag_rule() {
    let limits = Limits::default();
    for (header, defined) in [
        ("#BMP01   ", None),
        ("#BMP001 a.bmp", None),
        ("#BMP01xa", None),
        ("#bmp0z  a b.bmp  ", Some(image(35, "a b.bmp"))),
    ] {
        let text = format!("#BPM 120\n{header}\n#00011:01\n#00004:0Z\n");
        let loaded = load(&text);
        let bga = loaded.chart.bga.unwrap();
        assert_eq!(bga.images(), defined.as_slice(), "{header:?}");
        assert_eq!(
            bga.base(),
            [event(0, defined.as_ref().map(|_| 0))].as_slice(),
            "{header:?}"
        );
        // The BGA flag reads the same rule, in the load and the listing.
        assert_eq!(loaded.labels.bga, defined.is_some(), "{header:?}");
        let listing = read_listing(text.as_bytes(), &limits).unwrap();
        assert_eq!(listing.bga, defined.is_some(), "{header:?}");
    }
}

#[test]
fn channel_06_line_becomes_a_poor_sequence_at_the_measure_start() {
    let bga = bga("\
#BPM 120
#BMP01 a.bmp
#BMP02 b.bmp
#00011:01
#00206:00010002
#00306:0102
#00306:00000100
#00406:0000
#00506:01000300
");
    let poor = |time: i64, frames: &[Option<u16>]| PoorSequence {
        time: TimeUs(time),
        frames: frames.iter().map(|f| f.map(BgaImageId)).collect(),
    };
    assert_eq!(
        bga.poor(),
        [
            // Several ids: every pair a frame, at the measure's start.
            poor(4_000_000, &[None, Some(0), None, Some(1)]),
            // One id, after gaps: one frame; the later line of the
            // measure replaces the earlier.
            poor(6_000_000, &[Some(0)]),
            // A line of 00 holds no object. An undefined id shows
            // nothing.
            poor(10_000_000, &[Some(0), None, None, None]),
        ]
        .as_slice()
    );
}

#[test]
fn a_00_gap_in_a_poor_line_shows_nothing_with_bmp00_defined() {
    let bga = bga("\
#BPM 120
#BMP00 zero.bmp
#BMP01 a.bmp
#BMP02 b.bmp
#00011:01
#00106:01000200
#00206:00
");
    assert_eq!(bga.images()[0], image(0, "zero.bmp"));
    // `#BMP00` is index 0; a gap is not looked up as id 00, and a line of
    // 00 alone is no sequence.
    assert_eq!(
        bga.poor(),
        [PoorSequence {
            time: TimeUs(2_000_000),
            frames: vec![Some(BgaImageId(1)), None, Some(BgaImageId(2)), None].into(),
        }]
        .as_slice()
    );
}

// Covers: T01
#[test]
fn too_many_raw_objects_refuses_the_chart() {
    let opts = |max_objects| LoadOptions {
        limits: Limits {
            max_objects,
            ..Limits::default()
        },
        ..LoadOptions::default()
    };
    let notes = "#BPM 120\n#BMP01 a.bmp\n#00011:010101\n";
    // Three notes and two BGA objects: five objects, one past the cap.
    let text = format!("{notes}#00104:0001\n#00206:01\n");
    assert!(load_with(notes, &opts(4)).is_ok());
    assert!(load_with(&text, &opts(5)).is_ok());
    assert!(matches!(
        load_with(&text, &opts(4)),
        Err(LoadError::TooManyObjects { line: 5 })
    ));
}

// Covers: T01
#[test]
fn a_sidecar_event_past_the_playable_end_leaves_bga_absent() {
    // At 60 BPM a measure is 4 s, so measure 999 starts past 60 minutes.
    let play = "#BPM 60\n#BMP01 a.bmp\n#00011:01\n#00104:01\n";
    let far = format!("{play}#99904:01\n");
    let within = load(play);
    let loaded = load(&far);
    assert!(within.chart.bga.is_some());
    assert_eq!(loaded.chart.bga, None, "the BGA timing failed");
    assert_eq!(loaded.chart.timeline, within.chart.timeline);
    assert_eq!(loaded.chart.meta.length, within.chart.meta.length);
    assert_eq!(
        loaded.chart.identity.resolved_hash,
        within.chart.identity.resolved_hash
    );
    assert_eq!(loaded.labels, within.labels);
}

/// A chart with base, layer and poor events and `#BMPxx` definitions, its
/// last object a channel 06 line, and with `#WAV`, BGM and a BPM change.
const WITH_BGA: &str = "\
#TITLE t
#BPM 150
#BPM01 133.5
#WAV01 a.wav
#BMP01 a.bmp
#BMP02 b.bmp
#BMP0A c.bmp
#00011:0101
#00104:0102
#00107:000A
#00108:0001
#00201:01
#00212:000001
#00304:02
#00406:01020A
";

/// [`WITH_BGA`] read without the BGA structure's events: its base and
/// layer events moved to invisible notes (31, 32), which the preview times
/// as it times BGA and layer events and the play chart ignores, and its
/// 06 line to channel 0A, which nothing reads but the written-bar rule, so
/// every read but the BGA structure's is the same.
fn without_events(text: &str) -> String {
    text.replace("04:", "31:")
        .replace("07:", "32:")
        .replace("06:", "0A:")
}

/// The chart's played part: every field but the BGA structure and the
/// file's own hashes, which differ with the bytes.
fn played_part(chart: &Chart) -> Chart {
    Chart {
        bga: None,
        identity: ChartIdentity {
            sha256: [0; 32],
            md5: [0; 16],
            ..chart.identity.clone()
        },
        ..chart.clone()
    }
}

#[test]
fn bga_sidecar_leaves_identity_and_stored_values_unchanged() {
    let limits = Limits::default();
    let permuted = WITH_BGA
        .replace("#BMP01 a.bmp", "#BMP01 other.png")
        .replace("04:0102", "04:0A01")
        .replace("06:01020A", "06:0A0201");
    for other in [without_events(WITH_BGA), permuted] {
        // Stored values, from one scan of each.
        let (a, b) = (
            read_scanned(WITH_BGA.as_bytes(), &limits).unwrap(),
            read_scanned(other.as_bytes(), &limits).unwrap(),
        );
        assert!(a.listing.bga, "the chart defines a BGA image");
        assert_eq!(a.listing, b.listing, "the listing and its BGA flag");
        assert_eq!(a.labels, b.labels, "the labels and their BGA flag");
        assert_eq!(a.profile, b.profile, "the stored previews");
        assert_eq!(a.summary, b.summary, "the summary");
        assert_eq!(a.judgment_headers, b.judgment_headers);
        assert_eq!(a.main, b.main);
        assert_eq!(
            summarize(WITH_BGA.as_bytes(), &limits).unwrap(),
            summarize(other.as_bytes(), &limits).unwrap()
        );
        for speed in [Speed::NORMAL, Speed::from_twentieths(15).unwrap()] {
            let (a, b) = (load_at(WITH_BGA, speed), load_at(&other, speed));
            let (bga_a, bga_b) = (a.chart.bga.clone().unwrap(), b.chart.bga.clone().unwrap());
            // The two reads differ in their BGA structure.
            assert!(!bga_a.base().is_empty() && !bga_a.poor().is_empty());
            assert_ne!(bga_a, bga_b);
            assert_eq!(a.labels.bga, b.labels.bga, "the BGA flag");
            assert_eq!(
                (a.chart.identity.resolved_hash, a.chart.identity.played_hash),
                (b.chart.identity.resolved_hash, b.chart.identity.played_hash),
                "the hashes"
            );
            assert_eq!(
                a.chart.meta.length, b.chart.meta.length,
                "ChartMeta::length"
            );
            assert_eq!(
                played_part(&a.chart),
                played_part(&b.chart),
                "the evaluation subset and every other played field"
            );
            assert_eq!(
                ChartSummary::of(&a.chart),
                ChartSummary::of(&b.chart),
                "the summary"
            );
            assert_eq!(a.profile, b.profile, "the play profile");
        }
    }
}
