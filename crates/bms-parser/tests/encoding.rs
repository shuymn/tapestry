//! Text encoding detection, through `load_bms` and `read_header`, over
//! charts built here from text in each encoding.

use bms_parser::{Limits, LoadOptions, Loaded, WarningKind, load_bms, read_header};

fn load(bytes: &[u8]) -> Loaded {
    let dir = tempfile::tempdir().unwrap();
    load_bms(bytes, dir.path(), &LoadOptions::default()).unwrap()
}

/// A short chart whose title, artist and sound name are `title`, `artist`
/// and `sound`.
fn chart_text(title: &str, artist: &str, sound: &str) -> String {
    format!(
        "*---------------------- HEADER FIELD\n\
         #PLAYER 1\n#GENRE TEST\n#TITLE {title}\n#ARTIST {artist}\n#BPM 150\n\
         #PLAYLEVEL 12\n#RANK 2\n#TOTAL 300\n#WAV01 {sound}\n#WAV02 kick.wav\n\
         *---------------------- MAIN DATA FIELD\n\
         #00111:01020102\n#00112:00010001\n#00211:0102\n#00311:01\n"
    )
}

fn decode_warned(loaded: &Loaded) -> bool {
    loaded
        .warnings
        .iter()
        .any(|w| w.kind == WarningKind::DecodeErrors)
}

fn encode(encoding: &'static encoding_rs::Encoding, text: &str) -> Vec<u8> {
    let (bytes, _, unmappable) = encoding.encode(text);
    assert!(!unmappable);
    bytes.into_owned()
}

/// Mostly ASCII with a few Japanese characters: a guess over the whole file
/// names a Western encoding, so Shift_JIS has to come first.
#[test]
fn mostly_ascii_shift_jis_decodes_as_shift_jis() {
    let text = chart_text("迎撃-NIGHT SKY PATROL-", "obj: 雨", "音.wav");
    let bytes = encode(encoding_rs::SHIFT_JIS, &text);
    let loaded = load(&bytes);
    assert_eq!(loaded.chart.meta.title, "迎撃-NIGHT SKY PATROL-");
    assert_eq!(loaded.chart.sounds[0].name, "音.wav");
    assert!(!decode_warned(&loaded));
    let header = read_header(&bytes, &Limits::default()).unwrap();
    assert_eq!(header.title.as_deref(), Some("迎撃-NIGHT SKY PATROL-"));
    assert_eq!(header.artist.as_deref(), Some("obj: 雨"));
}

#[test]
fn utf8_decodes_with_and_without_a_bom() {
    let text = chart_text("迎撃-NIGHT SKY PATROL-", "obj: 雨", "音.wav");
    let mut with_bom = b"\xEF\xBB\xBF".to_vec();
    with_bom.extend_from_slice(text.as_bytes());
    for bytes in [text.as_bytes(), &with_bom] {
        let loaded = load(bytes);
        assert_eq!(loaded.chart.meta.title, "迎撃-NIGHT SKY PATROL-");
        assert_eq!(loaded.chart.sounds[0].name, "音.wav");
        let header = read_header(bytes, &Limits::default()).unwrap();
        assert_eq!(header.title.as_deref(), Some("迎撃-NIGHT SKY PATROL-"));
        assert_eq!(header.artist.as_deref(), Some("obj: 雨"));
    }
}

#[test]
fn pure_ascii_is_unchanged() {
    let text = chart_text("Plain Title", "someone", "a.wav");
    let loaded = load(text.as_bytes());
    assert_eq!(loaded.chart.meta.title, "Plain Title");
    assert_eq!(loaded.chart.sounds[0].name, "a.wav");
    assert!(!decode_warned(&loaded));
}

/// Hangul in EUC-KR can be valid Shift_JIS too, as half-width katakana; a
/// guess of EUC-KR keeps it.
#[test]
fn euc_kr_guess_wins_over_valid_shift_jis() {
    let title = "여름 바다의 하늘 [시간]";
    let text = chart_text(title, "작곡가", "소리.wav");
    let bytes = encode(encoding_rs::EUC_KR, &text);
    assert!(
        encoding_rs::SHIFT_JIS
            .decode_without_bom_handling_and_without_replacement(&bytes)
            .is_some(),
        "the case needs bytes that are valid Shift_JIS"
    );
    let loaded = load(&bytes);
    assert_eq!(loaded.chart.meta.title, title);
    assert_eq!(loaded.chart.sounds[0].name, "소리.wav");
}

/// Bytes invalid as UTF-8 and as Shift_JIS fall back to the guess.
#[test]
fn invalid_in_both_falls_back_to_the_guess() {
    let title = "夜明けの試験曲 ～朝焼け～";
    let text = chart_text(title, "作曲者 名前", "音色.wav");
    let bytes = encode(encoding_rs::EUC_JP, &text);
    assert!(std::str::from_utf8(&bytes).is_err());
    assert!(
        encoding_rs::SHIFT_JIS
            .decode_without_bom_handling_and_without_replacement(&bytes)
            .is_none()
    );
    let loaded = load(&bytes);
    assert_eq!(loaded.chart.meta.title, title);
    assert_eq!(loaded.chart.sounds[0].name, "音色.wav");
    assert!(!decode_warned(&loaded));
}

/// Malformed bytes in the chosen encoding become U+FFFD, with a warning.
#[test]
fn malformed_bytes_warn() {
    let text = chart_text("broken \u{fffd} title", "someone", "a.wav");
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(text.replace('\u{fffd}', "\u{1}").as_bytes());
    let at = bytes.iter().position(|&b| b == 1).unwrap();
    bytes[at] = 0xFF;
    let loaded = load(&bytes);
    assert_eq!(loaded.chart.meta.title, "broken \u{fffd} title");
    assert!(decode_warned(&loaded));
    assert_eq!(
        read_header(&bytes, &Limits::default())
            .unwrap()
            .title
            .as_deref(),
        Some("broken \u{fffd} title")
    );
}
