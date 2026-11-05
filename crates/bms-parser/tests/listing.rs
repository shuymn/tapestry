//! A chart's mode and summary as a song list stores them (design.md "Loading
//! and Song Selection"), against the loader.

use bms_model::{ChartSummary, TimeUs};
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
