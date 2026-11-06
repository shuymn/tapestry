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
/// `read_header`, which skips every block; the last declaration wins,
/// its value trimmed of Unicode whitespace.
#[test]
fn the_loaders_labels_come_from_the_taken_branches() {
    let text = "\
#PLAYER 1
#TITLE Outer
#ARTIST Someone
#SUBTITLE First
#SUBTITLE [ANOTHER]
#PLAYLEVEL 12
#DIFFICULTY 2
#BPM 120
#SETRANDOM 2
#IF 1
#GENRE Untaken
#PLAYLEVEL 3
#DIFFICULTY 5
#BMP01 untaken.bmp
#ENDIF
#IF 2
#ARTIST Branch two
#SUBARTIST Two
#PLAYLEVEL \u{3000}9a \t
#DIFFICULTY 4
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
            playlevel: "9a".to_owned(),
            difficulty: "4".to_owned(),
            bga: false,
        }
    );
    let header = read_header(text.as_bytes(), &Limits::default()).unwrap();
    assert_eq!(header.artist.as_deref(), Some("Someone"));
    assert_eq!(header.difficulty.as_deref(), Some("2"));
    let listing = bms_parser::read_listing(text.as_bytes(), &Limits::default()).unwrap();
    assert!(!listing.bga);
}

/// A `#DIFFICULTY` the reference decoder's `Integer.parseInt` cannot read
/// keeps the one before it, in the loader and the listing alike; one it
/// reads replaces it, whatever its value.
#[test]
fn an_unreadable_difficulty_keeps_the_one_before() {
    let chart = |defs: &str| format!("#PLAYER 1\n#TITLE t\n#BPM 120\n{defs}\n#00111:01\n");
    let read = |text: &str| {
        let dir = tempfile::tempdir().unwrap();
        let loaded =
            bms_parser::load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).unwrap();
        let header = read_header(text.as_bytes(), &Limits::default()).unwrap();
        (loaded.labels.difficulty, header.difficulty)
    };
    let some = |s: &str| (s.to_owned(), Some(s.to_owned()));
    for (defs, want) in [
        ("#DIFFICULTY 4\n#DIFFICULTY broken", some("4")),
        ("#DIFFICULTY 4\n#DIFFICULTY 9", some("9")),
        ("#DIFFICULTY 4\n#DIFFICULTY -1", some("-1")),
        ("#DIFFICULTY 4\n#DIFFICULTY 99999999999", some("4")),
        ("#DIFFICULTY broken", (String::new(), None)),
    ] {
        assert_eq!(read(&chart(defs)), want, "{defs:?}");
    }
    assert_eq!(bms_parser::java_int("+\u{FF13}"), Some(3));
    assert_eq!(bms_parser::java_int("-2147483648"), Some(i32::MIN));
    assert_eq!(bms_parser::java_int("2147483648"), None);
    assert_eq!(bms_parser::java_int("-"), None);
}

/// A chart has a BGA when a line the reference decoder registers as a
/// `#BMPxx` stands in what is read: the loader's taken branches, the
/// listing's top level. The decoder reads it by fixed position: `BMP` in
/// any case, an object id in the next two characters, and at least eight
/// characters in the line, the path from the eighth on, kept even when it
/// is only spaces. Whether a channel shows it does not matter.
#[test]
fn a_bmp_definition_with_a_path_is_a_bga() {
    let chart = |defs: &str| format!("#PLAYER 1\n#TITLE t\n#BPM 120\n{defs}\n#00111:01\n");
    let read = |text: &str| {
        let dir = tempfile::tempdir().unwrap();
        let loaded =
            bms_parser::load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).unwrap();
        let listing = bms_parser::read_listing(text.as_bytes(), &Limits::default()).unwrap();
        (loaded.labels.bga, listing.bga)
    };
    for (defs, want) in [
        ("#BMP00 poor.bmp", (true, true)),
        ("#bmpZZ  a.png ", (true, true)),
        ("#BMP01", (false, false)),
        ("#BMP01 ", (false, false)),
        ("#BMP01  ", (true, true)),
        ("#BmP01xa", (true, true)),
        // Counted in UTF-16 units, as Java's `String.length` counts.
        ("#BMP01\u{e9}", (false, false)),
        ("#BMP01\u{1F600}", (true, true)),
        ("#BMP0! a.bmp", (false, false)),
        ("#BMP001 a.bmp", (true, true)),
        ("#BMP001", (false, false)),
        ("#BGA01 01 0 0 10 10 0 0", (false, false)),
        ("#SETRANDOM 1\n#IF 1\n#BMP01 a.bmp\n#ENDIF", (true, false)),
        ("#SETRANDOM 1\n#IF 2\n#BMP01 a.bmp\n#ENDIF", (false, false)),
    ] {
        assert_eq!(read(&chart(defs)), want, "{defs:?}");
    }
    // A registered definition is no unknown header.
    let dir = tempfile::tempdir().unwrap();
    let text = chart("#BMP001 a.bmp");
    let loaded =
        bms_parser::load_bms(text.as_bytes(), dir.path(), &LoadOptions::default()).unwrap();
    assert!(
        !loaded
            .warnings
            .iter()
            .any(|w| matches!(w.kind, bms_parser::WarningKind::UnknownHeader(_))),
        "{:?}",
        loaded.warnings
    );
}
