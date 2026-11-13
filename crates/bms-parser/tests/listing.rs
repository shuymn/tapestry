//! A chart's mode and summary as a song list stores them (design.md "Loading
//! and Song Selection"), against the loader.

use bms_model::{ChartSummary, Tempo, TimeUs};
use bms_parser::{
    ChartMode, Limits, LoadError, LoadOptions, load_bms, read_header, read_listing, read_scanned,
    summarize,
};
use proptest::prelude::*;

fn mode(text: &str) -> ChartMode {
    read_listing(text.as_bytes(), &Limits::default())
        .unwrap()
        .mode
}

fn load(text: &str, seed: u64) -> Result<bms_parser::Loaded, LoadError> {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        seed,
        ..LoadOptions::default()
    };
    load_bms(text.as_bytes(), dir.path(), &opts)
}

/// Whether the chart is refused for a 2P lane.
fn two_player_refusal<T>(loaded: &Result<T, LoadError>) -> bool {
    matches!(loaded, Err(LoadError::Unsupported { what, .. }) if what.starts_with("2P lane"))
}

#[test]
fn the_mode_follows_the_objects_of_every_branch() {
    use ChartMode::{FiveKey, SevenKey, TwoPlayer};
    let cases = [
        // No object at all, and keys 1-5 with the scratch.
        ("#TITLE only a header\n", FiveKey),
        ("#BPM 120\n#00111:01\n#00116:01\n#00115:01\n", FiveKey),
        // Channel 17 and the BGM promote nothing.
        ("#00117:01\n#00101:01\n", FiveKey),
        // Keys 6 and 7: visible, invisible, long and mine.
        ("#00118:01\n", SevenKey),
        ("#00119:01\n", SevenKey),
        ("#00138:01\n", SevenKey),
        ("#00139:01\n", SevenKey),
        ("#00158:01\n", SevenKey),
        ("#001D9:01\n", SevenKey),
        ("#001d8:01\n", SevenKey),
        // The two-player side wins, on each of its channels.
        ("#00118:01\n#00121:01\n", TwoPlayer),
        ("#00141:01\n", TwoPlayer),
        ("#00161:01\n", TwoPlayer),
        ("#001E1:01\n", TwoPlayer),
        ("#001e9:01\n", TwoPlayer),
        // In a branch, taken or not, by any draw or none.
        ("#RANDOM 2\n#IF 1\n#00118:01\n#ENDIF\n", SevenKey),
        ("#RANDOM 2\n#IF 1\n#00121:01\n#ENDIF\n", TwoPlayer),
        ("#RANDOM 1\n#IF 2\n#00118:01\n#ENDIF\n", SevenKey),
        ("#RANDOM 1\n#IF 2\n#00121:01\n#ENDIF\n", TwoPlayer),
        // What the loader does not take as an object is none here.
        ("#00121:00\n#00118:01\n", SevenKey),
        ("#00121:??\n#00118:01\n", SevenKey),
        ("#00121:0\n#00118:01\n", SevenKey),
        ("#00118:00\n", FiveKey),
        ("#00118:??\n", FiveKey),
        ("#00118:0\n", FiveKey),
        // A pair after an odd trailing byte is not cut anew, and spaces go.
        ("#00121: 00 \n", FiveKey),
        ("#00118: 01 \n", SevenKey),
        // `#PLAYER` plays no part.
        ("#PLAYER 3\n#00118:01\n", SevenKey),
        ("#PLAYER 1\n#00121:01\n", TwoPlayer),
        ("#PLAYER 3\n", FiveKey),
    ];
    for (text, expected) in cases {
        assert_eq!(mode(text), expected, "{text:?}");
    }
}

#[test]
fn the_listing_says_whether_a_chart_draws() {
    let random = |text: &str| {
        read_listing(text.as_bytes(), &Limits::default())
            .unwrap()
            .random
    };
    assert!(!random("#00111:01\n"));
    assert!(random("#RANDOM 2\n"));
    assert!(random("#random 2\n"));
    assert!(random("#SETRANDOM 2\n"));
    // Inside a branch too.
    assert!(random("#RANDOM 2\n#IF 1\n#RANDOM 3\n#ENDIF\n"));
    assert!(random("#IF 1\n#RANDOM 3\n#ENDIF\n"));
}

#[test]
fn read_header_is_the_listings_header() {
    let text = "#TITLE Outer\n#00121:01\n#IF 1\n#TITLE Inner\n#ENDIF\n#GENRE g\n";
    let limits = Limits::default();
    assert_eq!(
        read_header(text.as_bytes(), &limits).unwrap(),
        read_listing(text.as_bytes(), &limits).unwrap().header
    );
    let too_small = Limits {
        max_file_bytes: 1,
        ..Limits::default()
    };
    assert!(matches!(
        read_listing(text.as_bytes(), &too_small),
        Err(LoadError::FileTooLarge { .. })
    ));
    assert!(matches!(
        summarize(text.as_bytes(), &too_small),
        Err(LoadError::FileTooLarge { .. })
    ));
}

#[test]
fn an_invalid_pair_on_a_two_player_lane_loads_and_lists() {
    let text = "#BPM 120\n#00111:01\n#00118:01\n#00121:??\n";
    assert!(load(text, 0).is_ok());
    assert_eq!(mode(text), ChartMode::SevenKey);
}

#[test]
fn a_two_player_object_read_on_every_draw_is_refused_and_two_player() {
    let text = "#BPM 120\n#00111:01\n#00121:01\n";
    assert!(two_player_refusal(&load(text, 0)));
    assert_eq!(mode(text), ChartMode::TwoPlayer);
}

#[test]
fn a_four_x_object_is_two_player_though_the_loader_takes_the_chart() {
    let text = "#BPM 120\n#00111:01\n#00141:01\n";
    assert!(load(text, 0).is_ok());
    assert_eq!(mode(text), ChartMode::TwoPlayer);
}

#[test]
fn the_mode_is_no_part_of_general_playability() {
    let valid = "#BPM 120\n#00111:01\n";
    assert_eq!(mode(valid), ChartMode::FiveKey);
    assert!(load(valid, 0).is_ok());
    let refused = "#BPM 0\n#00111:01\n";
    assert_eq!(mode(refused), ChartMode::FiveKey);
    assert!(load(refused, 0).is_err());
    assert!(!two_player_refusal(&load(refused, 0)));
}

/// A summary with counts, a long note, a mine and a tempo change: 120
/// BPM, a measure of 2 s, up to the tempo change at measure 2.
const CHART: &str = "\
#BPM 120
#LNTYPE 1
#WAV01 a.wav
#BPM01 240
#00111:01
#00118:01
#00152:0101
#001D3:01
#00208:01
#00311:01
";

#[test]
fn a_summary_counts_and_times_the_chart() {
    let s = summarize(CHART.as_bytes(), &Limits::default()).unwrap();
    assert_eq!((s.taps, s.longs, s.mines), (3, 1, 1));
    // Measure 2 starts at 4 s; at 240 BPM measure 3 starts 1 s after it.
    assert_eq!(s.length, TimeUs(5_000_000));
    assert_eq!(s.tempo_start, s.tempo_min);
    assert!(s.tempo_max > s.tempo_min);
    assert_eq!(s, ChartSummary::of(&load(CHART, 0).unwrap().chart));
}

/// The length ends at a final long note's tail, and each tempo is its exact
/// value: both written out here, not read through another summary.
#[test]
fn a_summary_ends_at_a_long_notes_tail_with_exact_tempos() {
    // 120 BPM to measure 1, 240 to measure 2, 60 from there: measures 1, 2 and
    // 3 start at 2 s, 3 s and 7 s. The long note holds from 2 s to 7 s, and the
    // last tap is at 3 s.
    let text = "\
#BPM 120
#LNTYPE 1
#WAV01 a.wav
#BPM01 240
#BPM02 60
#00108:01
#00208:02
#00151:01
#00212:01
#00351:01
";
    let exact = |bpm: u128| Tempo::from_units(bpm * Tempo::UNITS_PER_BPM).unwrap();
    let expected = ChartSummary {
        taps: 1,
        longs: 1,
        mines: 0,
        length: TimeUs(7_000_000),
        tempo_start: exact(120),
        tempo_min: exact(60),
        tempo_max: exact(240),
    };
    assert_eq!(
        summarize(text.as_bytes(), &Limits::default()).unwrap(),
        expected
    );
    assert_eq!(ChartSummary::of(&load(text, 0).unwrap().chart), expected);
}

/// The BGM counts toward the length, only when its sound is declared.
#[test]
fn the_length_takes_a_declared_bgm_after_the_last_note() {
    let length = |text: &str| {
        let s = summarize(text.as_bytes(), &Limits::default()).unwrap();
        assert_eq!(s, ChartSummary::of(&load(text, 0).unwrap().chart));
        s.length
    };
    let base = "#BPM 120\n#WAV01 a.wav\n#00011:01\n";
    assert_eq!(length(base), TimeUs(0));
    // Declared: measure 2 starts at 4 s.
    assert_eq!(length(&format!("{base}#00201:01\n")), TimeUs(4_000_000));
    // Undeclared: the loader drops it, and so does the summary.
    assert_eq!(length(&format!("{base}#00201:ZZ\n")), TimeUs(0));
}

#[test]
fn a_summary_needs_no_folder_and_refuses_what_a_name_alone_refuses() {
    let missing = std::env::temp_dir().join("tapestry-no-such-folder-for-a-summary");
    let opts = LoadOptions::default();
    assert!(matches!(
        load_bms(CHART.as_bytes(), &missing, &opts),
        Err(LoadError::ChartDir { .. })
    ));
    assert!(summarize(CHART.as_bytes(), &Limits::default()).is_ok());
    let leaving = "#BPM 120\n#WAV01 ../a.wav\n#00111:01\n";
    let is_rejected =
        |r: Result<(), LoadError>| matches!(r, Err(LoadError::SoundPathRejected { .. }));
    assert!(is_rejected(load(leaving, 0).map(|_| ())));
    assert!(is_rejected(
        summarize(leaving.as_bytes(), &Limits::default()).map(|_| ())
    ));
}

#[test]
fn read_scanned_is_the_listing_and_the_summary() {
    let limits = Limits::default();
    let scanned = read_scanned(CHART.as_bytes(), &limits).unwrap();
    assert_eq!(
        scanned.listing,
        read_listing(CHART.as_bytes(), &limits).unwrap()
    );
    assert_eq!(
        scanned.summary.unwrap(),
        summarize(CHART.as_bytes(), &limits).unwrap()
    );
    let refused = read_scanned(b"#BPM 0\n#00138:01\n", &limits).unwrap();
    assert_eq!(refused.listing.mode, ChartMode::SevenKey);
    assert!(refused.summary.is_err());
}

/// The scan's profile and MAIN tempo are the loader's: the profile a
/// play's graphs draw and the MAIN of every note.
#[test]
fn read_scanned_carries_the_profile_and_main_tempo() {
    let limits = Limits::default();
    let scanned = read_scanned(CHART.as_bytes(), &limits).unwrap();
    let loaded = load(CHART, 0).unwrap();
    assert_eq!(scanned.profile.as_ref(), Some(&loaded.profile));
    let main = bms_model::main_tempo(&loaded.chart, |_| true, bms_model::MAIN_HISTOGRAM_LIMIT).ok();
    assert!(main.is_some());
    assert_eq!(scanned.main, main);
    let refused = read_scanned(b"#BPM 0\n#00138:01\n", &limits).unwrap();
    assert!(refused.profile.is_none() && refused.main.is_none());
}

/// The seed-0 branch's judgment-header declarations come with the scan, in
/// file order, which a song list reads the rank from; a refused branch
/// carries none.
#[test]
fn read_scanned_carries_the_seed_0_judgment_headers() {
    let limits = Limits::default();
    let s = read_scanned(b"#RANK 3\n#DEFEXRANK 120\n#BPM 120\n#00111:01\n", &limits).unwrap();
    assert_eq!(
        rules::HeaderPolicy::DefExRankFirst
            .read(&s.judgment_headers)
            .rank,
        Some(rules::JudgeRank::DefExRankMilli(120_000))
    );
    assert_eq!(s.judgment_headers.len(), 2);
    let refused = read_scanned(b"#RANK 3\n#BPM 0\n#00138:01\n", &limits).unwrap();
    assert!(refused.summary.is_err());
    assert!(refused.judgment_headers.is_empty());
}

fn line() -> impl Strategy<Value = String> {
    let channel = prop::sample::select(vec![
        "11", "16", "18", "19", "38", "52", "58", "D1", "D8", "21", "28", "41", "61", "E1", "01",
    ]);
    let data = prop::sample::select(vec!["01", "00", "??", "0", "0001", "ZZ"]);
    let object = (0..3_u32, channel, data).prop_map(|(m, c, d)| format!("#00{m}{c}:{d}"));
    prop_oneof![
        4 => object,
        1 => (1..4_u32).prop_map(|n| format!("#RANDOM {n}")),
        2 => (1..4_u32).prop_map(|k| format!("#IF {k}")),
        2 => Just("#ENDIF".to_owned()),
        1 => Just("#ELSE".to_owned()),
    ]
}

proptest! {
    // These sample the draws: seeds 0 to 7 of each generated chart.
    #[test]
    fn listing_and_summary_agree_with_the_loader(lines in prop::collection::vec(line(), 0..16)) {
        let text = format!("#BPM 120\n#WAV01 a.wav\n#LNTYPE 1\n{}\n", lines.join("\n"));
        let listed = mode(&text);
        for seed in 0..8 {
            let loaded = load(&text, seed);
            if listed != ChartMode::TwoPlayer {
                prop_assert!(!two_player_refusal(&loaded), "{text}");
            }
            if two_player_refusal(&loaded) {
                prop_assert_eq!(listed, ChartMode::TwoPlayer, "{}", text);
            }
            if seed == 0 {
                let summary = summarize(text.as_bytes(), &Limits::default());
                match &loaded {
                    Ok(l) => prop_assert_eq!(summary, Ok(ChartSummary::of(&l.chart))),
                    Err(e) => prop_assert_eq!(summary.as_ref().err(), Some(e)),
                }
            }
        }
    }
}

/// The seed-0 branch's labels come with the scan, from which a song list
/// keeps the image names; a refused branch carries none.
#[test]
fn read_scanned_carries_the_seed_0_labels() {
    let limits = Limits::default();
    let text = b"#STAGEFILE top.png\n#SETRANDOM 2\n#IF 1\n#BANNER one.png\n#ENDIF\n\
                 #IF 2\n#BANNER two.png\n#ENDIF\n#ENDRANDOM\n#BPM 120\n#00111:01\n";
    let labels = read_scanned(text, &limits).unwrap().labels.unwrap();
    assert_eq!(
        (labels.stagefile.as_str(), labels.banner.as_str()),
        ("top.png", "two.png")
    );
    let refused = read_scanned(b"#STAGEFILE a.png\n#BPM 0\n#00138:01\n", &limits).unwrap();
    assert!(refused.summary.is_err());
    assert_eq!(refused.labels, None);
}

/// The preview profile of `text`'s seed-0 branch.
fn preview(text: &str) -> bms_model::ChartProfile {
    read_scanned(text.as_bytes(), &Limits::default())
        .unwrap()
        .profile
        .unwrap()
}

/// Nine taps in second 0 at 240 BPM (a measure a second).
const NINE: &str = "#BPM 240\n#00011:010101010101010101\n";

/// Fixtures H and BGA: a hidden note (31-39), a BGA event (04) or a layer
/// event (07) at 4.2 s lengthens the preview's seconds to 6, as the
/// reference's last time counts them; a poor-BGA event (06) does not, nor
/// any of them the play profile.
#[test]
fn hidden_notes_and_bga_lengthen_the_preview() {
    for (channel, seconds) in [("31", 6), ("39", 6), ("04", 6), ("07", 6), ("06", 2)] {
        let text = format!("{NINE}#004{channel}:0001000000\n");
        assert_eq!(preview(&text).seconds().len(), seconds, "channel {channel}");
        let loaded = load(&text, 0).unwrap();
        assert_eq!(loaded.profile.seconds().len(), 2, "play keeps its length");
        assert_eq!(loaded.chart.meta.length, TimeUs(888_888));
    }
    assert_eq!(preview(NINE).seconds().len(), 2);
}

/// Fixture SC: the preview's rows carry `#SCROLLxx`'s factors at channel
/// `SC`'s times; an undefined id changes nothing; the play profile and the
/// play chart read no scroll.
#[test]
fn scroll_changes_reach_the_preview_rows() {
    let text = "#BPM 120\n#SCROLL01 0.5\n#SCROLL02 2\n\
         #00011:01010101\n#000SC:0001\n#00111:01\n#001SC:0203\n#00211:01\n";
    let one = bms_model::SCROLL_ONE;
    let at = |ms, scroll| bms_model::SpeedRow {
        ms,
        tempo: Some(Tempo::from_milli(120_000).unwrap()),
        scroll,
    };
    assert_eq!(
        preview(text).speeds(),
        [
            at(0, one),
            at(1_000, one / 2),
            at(2_000, 2 * one),
            at(4_000, 2 * one)
        ]
    );
    let loaded = load(text, 0).unwrap();
    assert_eq!(loaded.profile.speeds(), [at(0, one), at(4_000, one)]);
    let plain = "#BPM 120\n#00011:01010101\n#00111:01\n#00211:01\n";
    assert_eq!(
        load(plain, 0).unwrap().chart.identity.resolved_hash,
        loaded.chart.identity.resolved_hash,
        "the play chart is the same without them"
    );
}

/// The preview's objects are timed apart: one past the duration cap
/// leaves the preview as the play chart's, and the chart still loads.
#[test]
fn a_preview_object_past_the_cap_is_left_out() {
    // 999 measures of 4 s at 60 BPM pass the 60-minute cap.
    let text = "#BPM 60
#00011:01
#99904:01
";
    let scanned = read_scanned(text.as_bytes(), &Limits::default()).unwrap();
    assert!(scanned.summary.is_ok());
    assert_eq!(scanned.profile.unwrap().seconds().len(), 2);
    assert!(load(text, 0).is_ok());
}

fn speed_row(ms: u32, bpm: i64, scroll: i128) -> bms_model::SpeedRow {
    bms_model::SpeedRow {
        ms,
        tempo: Some(Tempo::from_milli(bpm * 1_000).unwrap()),
        scroll,
    }
}

/// Fixture TB: a tempo change after play's last object reaches the preview
/// through its own timeline: a note at 0 at 120 BPM, 240 BPM (03 `F0`) at
/// measure 1 (2 s) and a hidden note at measure 2 (3 s). Play's rows stop
/// at its note.
#[test]
fn a_tempo_change_past_plays_end_reaches_the_preview() {
    let text = "#BPM 120\n#00011:01\n#00103:F0\n#00231:01\n";
    let one = bms_model::SCROLL_ONE;
    let p = preview(text);
    assert_eq!(
        p.speeds(),
        [
            speed_row(0, 120, one),
            speed_row(2_000, 240, one),
            speed_row(3_000, 240, one)
        ]
    );
    assert_eq!(p.length_ms(), 3_000);
    let loaded = load(text, 0).unwrap();
    assert_eq!(loaded.profile.speeds(), [speed_row(0, 120, one)]);
    assert_eq!(loaded.chart.meta.length, TimeUs(0));
}

/// Fixture HS: a later scroll change ends the rows but not the length,
/// which stays at the hidden note's 3 s.
#[test]
fn a_late_scroll_change_does_not_lengthen_the_content() {
    let text = "#BPM 120\n#SCROLL01 2\n#00011:01\n#00103:F0\n#00231:01\n#003SC:01\n";
    let p = preview(text);
    assert_eq!(p.length_ms(), 3_000);
    assert_eq!(
        p.speeds().last(),
        Some(&speed_row(4_000, 240, 2 * bms_model::SCROLL_ONE))
    );
}

/// Fixture Z/N: a scroll of 0 and a negative scroll reach the rows as
/// written.
#[test]
fn zero_and_negative_scroll_reach_the_rows() {
    let text = "#BPM 120\n#SCROLL01 0\n#SCROLL02 -1\n#00011:01010101\n#000SC:0001\n\
         #001SC:02\n#00111:01\n";
    let one = bms_model::SCROLL_ONE;
    assert_eq!(
        preview(text).speeds(),
        [
            speed_row(0, 120, one),
            speed_row(1_000, 120, 0),
            speed_row(2_000, 120, -one)
        ]
    );
}

/// Fixture TT: a tempo change after the last object reaches the preview's
/// rows with no shown object at all, as the source's speed list keeps every
/// timeline: a tap at 0 at 120 BPM, 240 BPM a quarter into the measure
/// (500 ms). Play's rows stop at the tap.
#[test]
fn a_trailing_tempo_change_reaches_the_preview() {
    let text = "#BPM 120\n#00019:01\n#00003:00F00000\n";
    let one = bms_model::SCROLL_ONE;
    assert_eq!(
        preview(text).speeds(),
        [speed_row(0, 120, one), speed_row(500, 240, one)]
    );
    assert_eq!(
        load(text, 0).unwrap().profile.speeds(),
        [speed_row(0, 120, one)]
    );
}

/// Fixture TS: a STOP after the last object stops the preview's rows and
/// ends at its own end: a tap at 0 at 120 BPM, a STOP of 500 ms at 1 s.
#[test]
fn a_trailing_stop_reaches_the_preview() {
    let text = "#BPM 120\n#STOP01 48\n#00019:01\n#00009:0001\n";
    let one = bms_model::SCROLL_ONE;
    let stopped = bms_model::SpeedRow {
        ms: 1_000,
        tempo: None,
        scroll: one,
    };
    assert_eq!(
        preview(text).speeds(),
        [speed_row(0, 120, one), stopped, speed_row(1_500, 120, one)]
    );
    assert_eq!(
        load(text, 0).unwrap().profile.speeds(),
        [speed_row(0, 120, one)]
    );
}

/// Fixture SU: a scroll change naming no `#SCROLLxx` is left out before
/// timing: it adds no point, so the rows and seconds are those without it,
/// and one past the duration cap (999 measures of 4 s at 60 BPM) does not
/// leave the preview as the play chart's, so a hidden note at 4 s still
/// lengthens it to 6 seconds.
#[test]
fn an_undefined_scroll_change_is_no_point() {
    let base = "#BPM 120\n#STOP01 48\n#00011:01\n#00009:00000001\n#00111:00000100\n#00131:01\n";
    let with = format!("{base}#001SC:0001\n");
    assert_eq!(preview(&with).speeds(), preview(base).speeds());
    assert_eq!(preview(&with).seconds(), preview(base).seconds());
    let tail = "#BPM 60\n#00011:01\n#00131:01\n";
    assert_eq!(preview(tail).seconds().len(), 6);
    assert_eq!(preview(&format!("{tail}#999SC:01\n")).seconds().len(), 6);
}
