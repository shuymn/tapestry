//! Loader tests over self-made charts in `tests/charts` and generated text.
//! Sound files are generated into temporary folders.

use std::fs;
use std::path::{Path, PathBuf};

use bms_model::{JudgeRank, Note, NoteKind, SoundId, TimeUs};
use bms_parser::{LOADER_VERSION, Limits, LoadError, LoadOptions, Loaded, WarningKind, load_bms};
use tempfile::TempDir;

fn chart(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/charts")
        .join(name);
    fs::read(&path).unwrap()
}

/// A short 16-bit mono PCM square-wave burst.
fn write_wav(path: &Path) {
    const RATE: u32 = 8_000;
    let samples: Vec<i16> = (0..400)
        .map(|i| if i % 20 < 10 { 8_000 } else { -8_000 })
        .collect();
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&RATE.to_le_bytes());
    bytes.extend_from_slice(&(RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

/// A temporary chart folder holding the named sound files.
fn folder(sounds: &[&str]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in sounds {
        write_wav(&dir.path().join(name));
    }
    dir
}

fn load_in(bytes: &[u8], dir: &Path) -> Result<Loaded, LoadError> {
    load_bms(bytes, dir, &LoadOptions::default())
}

fn load_text(text: &str) -> Result<Loaded, LoadError> {
    let dir = folder(&["click.wav"]);
    load_in(text.as_bytes(), dir.path())
}

fn times(notes: &[Note]) -> Vec<i64> {
    notes.iter().map(|n| n.time.0).collect()
}

#[test]
fn bpm_change_and_stop_at_one_position_use_new_bpm() {
    let dir = folder(&["click.wav"]);
    let loaded = load_in(&chart("bpm_stop.bms"), dir.path()).unwrap();
    let chart = &loaded.chart;
    // Measure 0: 4 beats at 120 = 2 s. Measure 1 starts with BPM 240 and a
    // 2-beat STOP at 240 (0.5 s; at 120 it would be 1 s): its first note
    // sounds at the STOP's start, the second 2 beats (0.5 s) after the STOP.
    // Measure 2 (3 beats): beat 1 at 240 (0.25 s), then 180.5 from channel
    // 08: beat 2 at 3_500_000 + 250_000 + 60e6 / 180.5 = 4_082_409.97...,
    // a half-beat STOP at 180.5 (166_204.98...) and one more beat
    // (332_409.97...) to measure 3: 4_581_024.93...
    assert_eq!(
        times(&chart.lanes[1]),
        vec![2_000_000, 3_000_000, 4_082_409, 4_581_024]
    );
    assert_eq!(chart.bgm.len(), 1);
    assert_eq!(chart.bgm[0].time, TimeUs(4_581_024));
    assert_eq!(
        chart.bar_lines,
        vec![
            TimeUs(0),
            TimeUs(2_000_000),
            TimeUs(3_500_000),
            TimeUs(4_581_024)
        ]
    );
    // The scroll is flat through each STOP.
    let pos = |t| chart.scroll.position_at(TimeUs(t));
    assert_eq!(pos(2_000_000), pos(2_500_000));
    assert!(pos(2_500_001) > pos(2_500_000));
    assert_eq!(pos(4_082_409), pos(4_248_614));
    assert_eq!(chart.meta.title, "BPM and STOP");
    assert_eq!(chart.meta.rank, Some(JudgeRank::Rank(2)));
    assert_eq!(chart.meta.total_milli, Some(250_500));
    assert_eq!(chart.identity.loader_version, LOADER_VERSION);
    assert_eq!(loaded.warnings, vec![]);
}

#[test]
fn rational_time_does_not_drift() {
    // 1000 measures at BPM 333.333, with a channel 08 change to the same BPM
    // at every measure so each measure is its own segment. The note at the
    // start of measure 999 sits at floor(999 * 4 beats * 60e6 / 333.333)
    // = floor(719_280_719.28...) µs; flooring each 720_000.72 µs measure
    // would give 719_280_000.
    let mut text = String::from("#BPM 333.333\n#BPM01 333.333\n#WAV01 click.wav\n");
    for m in 0..=999 {
        text.push_str(&format!("#{m:03}08:01\n"));
    }
    text.push_str("#99911:01\n");
    let loaded = load_text(&text).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![719_280_719]);
    assert_eq!(loaded.chart.bar_lines.len(), 1000);
}

#[test]
fn lnobj_and_lntype1_build_the_same_spans() {
    let long = |start: i64, end: i64| Note {
        time: TimeUs(start),
        sound: Some(SoundId(0)),
        kind: NoteKind::Long {
            end: TimeUs(end),
            end_sound: None,
        },
    };
    // 120 BPM: 2 s per measure, measure 1 starts at 2 s.
    let mut expected: [Vec<Note>; 8] = Default::default();
    expected[1] = vec![long(2_000_000, 3_000_000)];
    expected[2] = vec![long(2_500_000, 4_000_000)];
    expected[0] = vec![long(2_000_000, 3_500_000)];
    expected[7] = vec![long(5_000_000, 6_000_000)];
    let dir = folder(&["click.wav"]);
    for name in ["long_notes_lnobj.bms", "long_notes_lntype1.bms"] {
        let loaded = load_in(&chart(name), dir.path()).unwrap();
        assert_eq!(loaded.chart.lanes, expected, "{name}");
        assert_eq!(
            loaded.chart.lanes.iter().map(Vec::len).sum::<usize>(),
            4,
            "{name}"
        );
        assert_eq!(loaded.warnings, vec![], "{name}");
    }
}

#[test]
fn shift_jis_title_decodes() {
    let dir = folder(&["click.wav"]);
    let loaded = load_in(&chart("shift_jis_title.bms"), dir.path()).unwrap();
    assert_eq!(
        loaded.chart.meta.title,
        "\u{67b6}\u{7a7a}\u{306e}\u{8a66}\u{9a13}\u{66f2} \u{ff5e}\u{591c}\u{660e}\u{3051}\u{ff5e}"
    );
    assert_eq!(loaded.warnings, vec![]);
}

#[test]
fn bpm_zero_is_rejected() {
    for (text, what) in [
        ("#BPM 0\n#00111:01\n", "#BPM"),
        ("#BPM 0.000000\n#00111:01\n", "#BPM"),
        ("#BPM -120\n#00111:01\n", "#BPM"),
        ("#BPM 120\n#BPM01 0\n#00108:01\n", "#BPMxx"),
    ] {
        match load_text(text) {
            Err(LoadError::InvalidValue { what: w, .. }) => assert_eq!(w, what, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
}

#[test]
fn bpm_keeps_eighteen_fractional_digits_and_cuts_the_rest() {
    let loaded = load_text("#BPM 120.000000000000000003\n#WAV01 click.wav\n#00111:01\n").unwrap();
    // The scroll map takes millionths rounded down; time uses the exact BPM.
    assert_eq!(loaded.chart.scroll.initial_bpm_micro(), 120_000_000);
    // 4 beats: floor(240e6 / 120.000000000000000003) = 1_999_999, where
    // 120 BPM gives exactly 2_000_000.
    assert_eq!(times(&loaded.chart.lanes[1]), vec![1_999_999]);
    // A 19th fractional digit is cut off: exactly 120 BPM.
    let loaded = load_text("#BPM 120.0000000000000000007\n#WAV01 click.wav\n#00111:01\n").unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![2_000_000]);
}

#[test]
fn oversized_measure_count_is_rejected() {
    let dir = folder(&["click.wav"]);
    assert_eq!(
        load_in(&chart("limits_measure.bms"), dir.path()),
        Err(LoadError::MeasureOutOfRange {
            line: 5,
            measure: 1000
        })
    );
    // A lower limit applies the same way.
    let opts = LoadOptions {
        limits: Limits {
            max_measure: 0,
            ..Limits::default()
        },
        ..LoadOptions::default()
    };
    assert_eq!(
        load_bms(&chart("bpm_stop.bms"), dir.path(), &opts),
        Err(LoadError::MeasureOutOfRange {
            line: 15,
            measure: 1
        })
    );
}

#[test]
fn text_limits_name_the_line() {
    let dir = folder(&["click.wav"]);
    let bytes = chart("bpm_stop.bms");
    let with = |limits: Limits| {
        load_bms(
            &bytes,
            dir.path(),
            &LoadOptions {
                limits,
                ..LoadOptions::default()
            },
        )
    };
    let base = Limits::default();
    assert_eq!(
        with(Limits {
            max_file_bytes: 10,
            ..base
        }),
        Err(LoadError::FileTooLarge { bytes: bytes.len() })
    );
    assert_eq!(
        with(Limits {
            max_lines: 3,
            ..base
        }),
        Err(LoadError::TooManyLines { line: 4 })
    );
    // Objects: F0 (line 15), 01 (16), 01 01 (17): the fourth is on line 17.
    assert_eq!(
        with(Limits {
            max_objects: 3,
            ..base
        }),
        Err(LoadError::TooManyObjects { line: 17 })
    );
}

#[test]
fn tiny_bpm_exceeding_duration_cap_is_rejected() {
    // 4 beats at 0.000001 BPM last 2.4e14 µs, far past 60 minutes, though
    // the text is six short lines.
    let dir = folder(&["click.wav"]);
    assert_eq!(
        load_in(&chart("limits_tiny_bpm.bms"), dir.path()),
        Err(LoadError::DurationExceeded { measure: 1 })
    );
}

#[test]
fn chart_ending_exactly_at_duration_cap_loads() {
    // 60 BPM: 4 s per measure, so measure 900 starts at exactly 60 minutes.
    let at_cap = "#BPM 60\n#WAV01 click.wav\n#90011:01\n";
    let loaded = load_text(at_cap).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![3_600_000_000]);
    // A STOP at that position ends past the cap.
    let past = "#BPM 60\n#STOP01 1\n#WAV01 click.wav\n#90011:01\n#90009:01\n";
    assert_eq!(
        load_text(past),
        Err(LoadError::DurationExceeded { measure: 900 })
    );
}

#[test]
fn unsupported_lnmode_and_switch_are_errors() {
    let dir = folder(&["click.wav"]);
    for (name, line, what) in [
        ("unsupported_lnmode.bms", 3, "#LNMODE 2"),
        ("unsupported_switch.bms", 4, "#SWITCH"),
        ("unsupported_lntype2.bms", 3, "#LNTYPE 2"),
    ] {
        assert_eq!(
            load_in(&chart(name), dir.path()),
            Err(LoadError::Unsupported {
                line,
                what: what.to_owned()
            }),
            "{name}"
        );
    }
    assert_eq!(
        load_text("#BPM 120\n#00121:01\n"),
        Err(LoadError::Unsupported {
            line: 2,
            what: "2P lane channel 21".to_owned()
        })
    );
}

#[test]
fn sound_path_escape_is_rejected() {
    let outside = folder(&["x.wav"]);
    let dir = folder(&[]);
    for name in [
        "escape_parent.bms",
        "escape_drive.bms",
        "escape_rooted.bms",
        "escape_unc.bms",
    ] {
        assert_eq!(
            load_in(&chart(name), dir.path()),
            Err(LoadError::SoundPathRejected { line: 2 }),
            "{name}"
        );
    }
    link_dir(&dir.path().join("link"), outside.path());
    assert!(dir.path().join("link").join("x.wav").is_file());
    assert_eq!(
        load_in(&chart("escape_junction.bms"), dir.path()),
        Err(LoadError::SoundPathEscapes { line: 2 })
    );
    // The extension fallback is confined too: `link\a.wav` is missing and
    // `link\a.ogg` lies outside.
    write_wav(&outside.path().join("a.ogg"));
    assert_eq!(
        load_in(&chart("escape_fallback.bms"), dir.path()),
        Err(LoadError::SoundPathEscapes { line: 2 })
    );
}

#[test]
fn case_variant_name_stays_in_folder() {
    let dir = folder(&[]);
    fs::create_dir(dir.path().join("sub")).unwrap();
    write_wav(&dir.path().join("sub").join("click.wav"));
    let loaded = load_in(&chart("case_variant.bms"), dir.path()).unwrap();
    let path = loaded.chart.sounds[0].path.clone();
    if cfg!(windows) {
        // Case-insensitive: `SUB\CLICK.WAV` finds the file inside.
        let path = path.unwrap();
        assert!(path.starts_with(dir.path().canonicalize().unwrap()));
        assert!(path.is_file());
    } else {
        assert_eq!(path, None);
    }
}

/// A directory junction on Windows, a symbolic link elsewhere.
fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[test]
fn extension_fallback_stays_in_folder() {
    let dir = folder(&["a.ogg"]);
    let loaded = load_in(&chart("fallback.bms"), dir.path()).unwrap();
    let expected: PathBuf = dir.path().canonicalize().unwrap().join("a.ogg");
    assert_eq!(loaded.chart.sounds.len(), 1);
    assert_eq!(loaded.chart.sounds[0].name, "a.wav");
    assert_eq!(
        loaded.chart.sounds[0].path.as_deref(),
        Some(expected.as_path())
    );
    assert_eq!(loaded.warnings, vec![]);

    let missing = folder(&[]);
    let loaded = load_in(&chart("fallback.bms"), missing.path()).unwrap();
    assert_eq!(loaded.chart.sounds[0].path, None);
    assert_eq!(loaded.warnings.len(), 1);
    assert_eq!(loaded.warnings[0].kind, WarningKind::MissingSoundFile);
}

#[test]
fn moving_chart_folder_keeps_resolved_hash() {
    let bytes = chart("bpm_stop.bms");
    let a = folder(&["click.wav"]);
    let b = folder(&["click.wav"]);
    let in_a = load_in(&bytes, a.path()).unwrap().chart;
    let in_b = load_in(&bytes, b.path()).unwrap().chart;
    assert_ne!(in_a.sounds[0].path, in_b.sounds[0].path);
    assert!(in_a.sounds[0].path.is_some());
    assert_ne!(in_a.identity.resolved_hash, [0; 32]);
    assert_eq!(in_a.identity.resolved_hash, in_b.identity.resolved_hash);
    // A different chart has a different resolved hash.
    let other = load_in(&chart("fallback.bms"), a.path()).unwrap().chart;
    assert_ne!(other.identity.resolved_hash, in_a.identity.resolved_hash);
}

#[test]
fn same_bytes_same_hashes() {
    // Computed at authoring time with `sha256sum` and `md5sum`.
    const SHA256: &str = "313aa9d953946f8cf35af3900e20b598f3b866aa19517aedb409987d0e0b2fa3";
    const MD5: &str = "f3f28ed064ce1c019ba3b36322ac40ae";
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let dir = folder(&["click.wav"]);
    let bytes = chart("bpm_stop.bms");
    let first = load_in(&bytes, dir.path()).unwrap().chart.identity;
    let second = load_in(&bytes, dir.path()).unwrap().chart.identity;
    assert_eq!(hex(&first.sha256), SHA256);
    assert_eq!(hex(&first.md5), MD5);
    assert_eq!(first, second);
}

#[test]
fn mines_and_conflicts_follow_loader_rules() {
    let dir = folder(&["click.wav", "clack.wav"]);
    let loaded = load_in(&chart("mines.bms"), dir.path()).unwrap();
    let lanes = &loaded.chart.lanes;
    // Key 1: the long note and the mine after it; the mine inside and the
    // mine at its end are dropped.
    assert_eq!(lanes[1].len(), 2);
    assert_eq!(
        lanes[1][0].kind,
        NoteKind::Long {
            end: TimeUs(3_000_000),
            end_sound: None
        }
    );
    assert_eq!(lanes[1][1].time, TimeUs(4_000_000));
    assert_eq!(lanes[1][1].kind, NoteKind::Mine { damage: 10 });
    // Key 2: the long note beats the tap at its start.
    assert_eq!(lanes[2].len(), 1);
    assert!(matches!(lanes[2][0].kind, NoteKind::Long { .. }));
    // Key 3: the later of two taps at one time, with its sound.
    assert_eq!(lanes[3].len(), 1);
    assert_eq!(lanes[3][0].sound, Some(SoundId(1)));
    // Key 4: the tap beats the mine.
    assert_eq!(lanes[4].len(), 1);
    assert_eq!(lanes[4][0].kind, NoteKind::Tap);
    let kinds: Vec<_> = loaded.warnings.iter().map(|w| &w.kind).collect();
    let count = |k: &WarningKind| kinds.iter().filter(|&&w| w == k).count();
    assert_eq!(count(&WarningKind::NoteInsideLongNoteDropped), 2);
    assert_eq!(count(&WarningKind::SameTimeNoteDropped), 3);
    assert_eq!(kinds.len(), 5);
}

#[test]
fn empty_chart_loads_with_a_warning() {
    let loaded = load_text("#BPM 120\n#WAV01 click.wav\n#00101:01\n").unwrap();
    assert!(loaded.chart.lanes.iter().all(Vec::is_empty));
    assert_eq!(loaded.chart.bgm.len(), 1);
    assert_eq!(
        loaded.warnings.iter().map(|w| &w.kind).collect::<Vec<_>>(),
        vec![&WarningKind::NoPlayableNotes]
    );
}

#[test]
fn duplicated_lntype1_line_keeps_one_long_note() {
    // The later line wins at each endpoint: the long note plays sound 02.
    let text = "#BPM 120\n#WAV01 click.wav\n#WAV02 click.wav\n#00151:0101\n#00151:0202\n";
    let loaded = load_text(text).unwrap();
    assert_eq!(
        loaded.chart.lanes[1],
        vec![Note {
            time: TimeUs(2_000_000),
            sound: Some(SoundId(1)),
            kind: NoteKind::Long {
                end: TimeUs(3_000_000),
                end_sound: None
            }
        }]
    );
    assert_eq!(
        loaded.warnings.iter().map(|w| &w.kind).collect::<Vec<_>>(),
        vec![&WarningKind::SameTimeNoteDropped; 2]
    );
}

#[test]
fn notes_rounded_to_one_time_keep_the_later_line() {
    // Measure 0 lasts 2 µs: the first line's note sits at 0.5 µs and the
    // second line's at 0 µs; both floor to 0, and the later line wins even
    // though its note comes first in time.
    let text = "#BPM 120\n#WAV01 click.wav\n#WAV02 click.wav\n#00002:0.000001\n\
                #00011:00010000\n#00011:02\n";
    let loaded = load_text(text).unwrap();
    assert_eq!(
        loaded.chart.lanes[1],
        vec![Note {
            time: TimeUs(0),
            sound: Some(SoundId(1)),
            kind: NoteKind::Tap
        }]
    );
}

#[test]
fn long_note_edge_cases() {
    let text = "#BPM 120\n#LNOBJ ZZ\n#WAV01 click.wav\n#WAV02 click.wav\n\
                #00111:ZZ\n\
                #00152:01\n\
                #00153:0102\n\
                #00114:01\n#00214:ZZ\n#00154:00010001\n";
    let loaded = load_text(text).unwrap();
    let lanes = &loaded.chart.lanes;
    // Key 1: an #LNOBJ end without a start is dropped.
    assert_eq!(lanes[1], vec![]);
    // Key 2: an unterminated start is dropped.
    assert_eq!(lanes[2], vec![]);
    // Key 3: a different end id is the end sound.
    assert_eq!(
        lanes[3][0].kind,
        NoteKind::Long {
            end: TimeUs(3_000_000),
            end_sound: Some(SoundId(1))
        }
    );
    // Key 4: a channel 5x long note inside an #LNOBJ one is dropped.
    assert_eq!(times(&lanes[4]), vec![2_000_000]);
    assert_eq!(
        lanes[4][0].kind,
        NoteKind::Long {
            end: TimeUs(4_000_000),
            end_sound: None
        }
    );
    let warned: Vec<_> = loaded.warnings.iter().map(|w| (w.line, &w.kind)).collect();
    assert_eq!(
        warned,
        vec![
            (Some(5), &WarningKind::UnpairedLongNoteEnd),
            (Some(6), &WarningKind::UnpairedLongNoteStart),
            (Some(10), &WarningKind::NoteInsideLongNoteDropped),
        ]
    );
    // An undefined start sound is reported on the start's line.
    let undefined = load_text("#BPM 120\n#00151:05\n#00251:05\n").unwrap();
    assert_eq!(
        undefined
            .warnings
            .iter()
            .map(|w| (w.line, &w.kind))
            .collect::<Vec<_>>(),
        vec![(Some(2), &WarningKind::UndefinedReference)]
    );
}

#[test]
fn resolved_hash_matches_golden() {
    // Computed at authoring time by an independent script encoding the
    // documented fields of this chart: version 2, 120 BPM, no scroll changes,
    // one tap at 2 s on key 1 with sound 0, bar lines at 0 and 2 s, the sound
    // name `a.wav`, an empty title and no other values.
    const GOLDEN: &str = "b55f7f6c1e864d386d3339be8a60baf1c3283a6a2ea57439c2d81785cf855d6c";
    let loaded = load_text("#BPM 120\n#WAV01 a.wav\n#00111:01\n").unwrap();
    let hex: String = loaded
        .chart
        .identity
        .resolved_hash
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    assert_eq!(hex, GOLDEN);
}

#[test]
fn each_resolved_field_changes_the_resolved_hash() {
    const BASE: &[&str] = &[
        "#TITLE t",
        "#BPM 120",
        "#RANK 2",
        "#TOTAL 200",
        "#WAV01 a.wav",
        "#WAV02 b.wav",
        "#00111:01",
        "#00152:0101",
        "#00101:02",
        "#00211:01",
        // A silent tap (undefined sound) and a mine.
        "#00114:05",
        "#001D5:05",
    ];
    type Variant<'a> = (&'a str, &'a [(usize, &'a str)], &'a [&'a str]);
    // Each variant replaces a line or adds lines to change one field.
    let variants: &[Variant<'_>] = &[
        ("base", &[], &[]),
        ("note time", &[(6, "#00111:0001")], &[]),
        ("note lane", &[(6, "#00113:01")], &[]),
        ("note sound", &[(6, "#00111:02")], &[]),
        ("silent tap to mine", &[(10, "#001D4:05")], &[]),
        ("mine damage", &[(11, "#001D5:06")], &[]),
        ("long-note end", &[(7, "#00152:01000001")], &[]),
        ("end sound", &[(7, "#00152:0102")], &[]),
        ("BGM time", &[(8, "#00101:0002")], &[]),
        ("BGM sound", &[(8, "#00101:01")], &[]),
        ("STOP", &[], &["#STOP01 48", "#00209:0001"]),
        ("BPM change", &[], &["#00203:00F0"]),
        ("bar line", &[], &["#00302:0.5"]),
        ("title", &[(0, "#TITLE u")], &[]),
        ("rank", &[(2, "#RANK 3")], &[]),
        ("total", &[(3, "#TOTAL 201")], &[]),
        ("sound name", &[(5, "#WAV02 c.wav")], &[]),
    ];
    let mut seen: Vec<(&str, [u8; 32])> = Vec::new();
    for &(name, replace, add) in variants {
        let mut lines = BASE.to_vec();
        for &(i, line) in replace {
            lines[i] = line;
        }
        lines.extend_from_slice(add);
        let text = lines.join("\n") + "\n";
        let hash = load_text(&text).unwrap().chart.identity.resolved_hash;
        for (other, h) in &seen {
            assert_ne!(*h, hash, "{name} hashes like {other}");
        }
        seen.push((name, hash));
    }
    assert_eq!(seen.len(), 17);
}
