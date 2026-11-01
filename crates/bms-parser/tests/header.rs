//! `read_header` over self-made chart text.

use bms_parser::{Header, Labels, Limits, LoadError, LoadOptions, read_header};

#[test]
fn read_header_resolves_no_time_and_no_random() {
    // Every line the loader would refuse: a negative BPM, a measure of no
    // length, `#RANDOM 0` and `#SWITCH`. The header ignores them all and
    // keeps only what stands outside the `#IF` and `#SWITCH` blocks.
    let text = "\
#PLAYER 1
#TITLE Outer
#ARTIST Someone
#BPM -120
#00102:0
#00111:0101
#RANDOM 0
#IF 1
#TITLE Branch one
#GENRE Hidden
#ENDIF
#IF 2
#ARTIST Branch two
#ENDIF
#ENDRANDOM
#SWITCH 2
#CASE 1
#SUBTITLE Case
#ENDSW
#PLAYLEVEL 12
";
    let dir = tempfile::tempdir().unwrap();
    assert!(
        bms_parser::load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).is_err(),
        "the loader refuses the text"
    );
    let header = read_header(text.as_bytes(), &Limits::default()).unwrap();
    assert_eq!(
        header,
        Header {
            title: Some("Outer".to_owned()),
            artist: Some("Someone".to_owned()),
            playlevel: Some("12".to_owned()),
            bpm: Some("-120".to_owned()),
            ..Header::default()
        }
    );
}

#[test]
fn read_header_decodes_shift_jis_and_keeps_the_last_value() {
    let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS
        .encode("#TITLE 最初\n#TITLE 曲名テスト\n#ARTIST 作者\n#SUBTITLE [ANOTHER]\n#BPM 150.5\n");
    assert!(!unmappable);
    let header = read_header(&bytes, &Limits::default()).unwrap();
    assert_eq!(header.title.as_deref(), Some("曲名テスト"));
    assert_eq!(header.artist.as_deref(), Some("作者"));
    assert_eq!(header.subtitle.as_deref(), Some("[ANOTHER]"));
    assert_eq!(header.bpm.as_deref(), Some("150.5"));
    assert_eq!(header.genre, None);
}

#[test]
fn read_header_takes_the_loaders_file_cap() {
    let limits = Limits {
        max_file_bytes: 8,
        ..Limits::default()
    };
    assert!(read_header(b"#TITLE a", &limits).is_ok());
    assert_eq!(
        read_header(b"#TITLE ab", &limits),
        Err(LoadError::FileTooLarge { bytes: 9 })
    );
}

/// The loader's labels come from the resolved chart: a taken `#IF`
/// branch's declarations count and an untaken one's do not, unlike
/// `read_header`, which skips every block; the last declaration wins.
#[test]
fn the_loaders_labels_come_from_the_taken_branches() {
    let text = "\
#PLAYER 1
#TITLE Outer
#ARTIST Someone
#SUBTITLE First
#SUBTITLE [ANOTHER]
#BPM 120
#SETRANDOM 2
#IF 1
#GENRE Untaken
#ENDIF
#IF 2
#ARTIST Branch two
#SUBARTIST Two
#ENDIF
#ENDRANDOM
#00111:01
";
    let dir = tempfile::tempdir().unwrap();
    let loaded =
        bms_parser::load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).unwrap();
    assert_eq!(loaded.chart.meta.title, "Outer");
    assert_eq!(
        loaded.labels,
        Labels {
            subtitle: "[ANOTHER]".to_owned(),
            artist: "Branch two".to_owned(),
            subartist: "Two".to_owned(),
            genre: String::new(),
        }
    );
    let header = read_header(text.as_bytes(), &Limits::default()).unwrap();
    assert_eq!(header.artist.as_deref(), Some("Someone"));
}
