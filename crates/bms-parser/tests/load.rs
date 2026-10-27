//! Loader tests over self-made charts in `tests/charts` and generated text.
//! Sound files are generated into temporary folders.

use std::fs;
use std::path::{Path, PathBuf};

use bms_model::{BEAT, JudgeRank, Note, NoteKind, Position, SoundId, TimeUs};
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

/// The display position `beats` beats in.
fn beats(beats: i128) -> Position {
    Position(beats * BEAT)
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
    // Bar lines at 0, 4, 8 and 11 beats: STOPs and BPM changes add none.
    assert_eq!(chart.bar_lines, [0, 4, 8, 11].map(beats));
    let positions: Vec<_> = chart.lanes[1].iter().map(|n| n.pos).collect();
    assert_eq!(positions, [4, 6, 10, 11].map(beats));
    // The scroll is flat through each STOP, from the first whole µs of its
    // exact start (4_082_409.97... for the second) to the last before its
    // exact end (4_248_614.95...), on the STOP's beat.
    let pos = |t| chart.scroll.position_at(TimeUs(t));
    assert_eq!(pos(2_000_000), beats(4));
    assert_eq!(pos(2_500_000), beats(4));
    assert!(pos(2_500_001) > pos(2_500_000));
    assert!(pos(4_082_409) < beats(10));
    assert_eq!(pos(4_082_410), beats(10));
    assert_eq!(pos(4_248_614), beats(10));
    assert!(pos(4_248_615) > beats(10));
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
    // 120 BPM: 2 beats a second, every endpoint on a whole beat.
    let long = |start: i64, end: i64| Note {
        time: TimeUs(start),
        pos: beats(i128::from(start / 500_000)),
        sound: Some(SoundId(0)),
        kind: NoteKind::Long {
            end: TimeUs(end),
            end_pos: beats(i128::from(end / 500_000)),
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
    // Time and the scroll rate use the exact BPM: 2^80 × 2.000...00005e-6
    // units per µs, floored, one more than 120 gives.
    let rate = loaded.chart.scroll.initial_rate();
    assert_eq!(rate, BEAT / 500_000);
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
fn time_past_the_duration_cap_by_less_than_a_microsecond_is_refused() {
    // A 0.000001-unit STOP at 120 BPM lasts 1.25 / 120 µs: the time floors
    // to 0, the cap, but lies past it.
    let dir = folder(&["click.wav"]);
    let opts = LoadOptions {
        limits: Limits {
            max_duration: TimeUs(0),
            ..Limits::default()
        },
        ..LoadOptions::default()
    };
    let text = "#BPM 120\n#STOP01 0.000001\n#00009:01\n";
    assert_eq!(
        load_bms(text.as_bytes(), dir.path(), &opts),
        Err(LoadError::DurationExceeded { measure: 0 })
    );
    // Without the STOP the chart ends at 0 and loads.
    assert!(load_bms(b"#BPM 120\n", dir.path(), &opts).is_ok());
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
            end_pos: beats(6),
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
            pos: beats(4),
            sound: Some(SoundId(1)),
            kind: NoteKind::Long {
                end: TimeUs(3_000_000),
                end_pos: beats(6),
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
            pos: beats(0),
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
            end_pos: beats(6),
            end_sound: Some(SoundId(1))
        }
    );
    // Key 4: a channel 5x long note inside an #LNOBJ one is dropped.
    assert_eq!(times(&lanes[4]), vec![2_000_000]);
    assert_eq!(
        lanes[4][0].kind,
        NoteKind::Long {
            end: TimeUs(4_000_000),
            end_pos: beats(8),
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
    // documented fields of this chart: version 5, the 120 BPM rate
    // (floor(2^80 / 500_000) units per µs) before time 0 and as the one
    // segment from 0, one tap at 2 s and 4 beats on key 1 with sound 0, bar
    // lines at 0 and 4 beats, the sound name `a.wav`, an empty title and no
    // other values.
    const GOLDEN: &str = "b7cab4b435c8a5ff08dbefecc536620b7b338527c2a2023a322b4154b2c0d2c8";
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
fn equal_times_on_other_beats_hash_differently() {
    // Measure 0 is 1e-12 longer and the BPM 1e-12 faster in proportion, so
    // the note at measure 1 sounds at exactly the same time and the one at
    // measure 2 within the same µs, but both sit on other beats; the BPM
    // differs below a millionth.
    let base = load_text("#BPM 7\n#WAV01 a.wav\n#00111:01\n#00211:01\n").unwrap();
    let moved = load_text(
        "#BPM 7.000000000007\n#WAV01 a.wav\n#00002:1.000000000001\n#00111:01\n#00211:01\n",
    )
    .unwrap();
    let (a, b) = (&base.chart.lanes[1], &moved.chart.lanes[1]);
    assert_eq!(times(a), times(b));
    assert_eq!(a[0].pos, beats(4));
    assert!(b[0].pos > a[0].pos);
    assert_ne!(
        base.chart.identity.resolved_hash,
        moved.chart.identity.resolved_hash
    );
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
        // A last note, so bar lines in between are kept.
        "#00412:01",
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
        // Measures 2 and 3 keep 8 beats together: only bar line 3 moves.
        ("bar line", &[], &["#00202:0.5", "#00302:1.5"]),
        // Measure 1 runs 8 beats at 240 BPM: every time stays, positions
        // after measure 0 move.
        (
            "same times, other beats",
            &[],
            &["#00103:F0", "#00102:2", "#00203:78"],
        ),
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
    assert_eq!(seen.len(), 18);
}

#[test]
fn trailing_measures_after_the_last_object_are_dropped() {
    // At 60 BPM each measure lasts 4 s, and from measure 500 (2000 s) the
    // trailing change to 15 BPM makes it 16 s, so measure 999 would start
    // at 9984 s, past the 60-minute cap. Only the bar lines through the last
    // note (measure 2) stay, and the BPM change after it is dropped.
    let text = "#BPM 60\n#WAV01 click.wav\n#00011:01\n#00211:0001\n#50003:0F\n#99902:0.5\n";
    let loaded = load_text(text).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), vec![0, 10_000_000]);
    assert_eq!(loaded.chart.bar_lines, [0, 4, 8].map(beats));
    let pos = |t| loaded.chart.scroll.position_at(TimeUs(t));
    // Still 60 BPM after where the dropped change was: 1 beat a second at
    // the floored rate, not the quarter beat 15 BPM would give.
    assert_eq!(
        pos(2_101_000_000).0 - pos(2_100_000_000).0,
        BEAT / 1_000_000 * 1_000_000
    );
    // A BGM object counts as the last object too.
    let bgm = "#BPM 60\n#WAV01 click.wav\n#00011:01\n#00401:01\n#99902:0.5\n";
    assert_eq!(load_text(bgm).unwrap().chart.bar_lines.len(), 5);
}

#[test]
fn negative_bpm_is_refused_as_reverse_scroll() {
    for (text, what) in [
        ("#BPM -150\n#00111:01\n", "#BPM"),
        ("#BPM 120\n#BPM01 -7.5\n#00108:01\n", "#BPMxx"),
    ] {
        match load_text(text) {
            Err(LoadError::InvalidValue {
                what: w, reason, ..
            }) => assert_eq!(
                (w, reason),
                (what, "negative BPM: reverse scroll is not supported yet"),
                "{text}"
            ),
            other => panic!("{text}: {other:?}"),
        }
    }
}

#[test]
fn empty_lnobj_and_lntype_without_a_type_are_skipped() {
    let long = |text: &str| {
        let loaded = load_text(text).unwrap();
        let ends: Vec<_> = loaded.chart.lanes[1]
            .iter()
            .map(|n| match n.kind {
                NoteKind::Long { end, .. } => (n.time.0, end.0),
                _ => (n.time.0, -1),
            })
            .collect();
        (ends, loaded.warnings)
    };
    let ignored = |what| {
        vec![bms_parser::Warning {
            line: Some(2),
            kind: WarningKind::IgnoredValue(what),
        }]
    };
    // An empty #LNOBJ: the 1x notes stay taps. Repeated, it is warned once.
    assert_eq!(
        long("#BPM 120\n#LNOBJ\n#WAV01 click.wav\n#LNOBJ \n#00111:0101\n"),
        (vec![(2_000_000, -1), (3_000_000, -1)], ignored("#LNOBJ"))
    );
    // #LNTYPE 0 and ZZ read as 1; 01 is 1 without a warning.
    let pair = "#WAV01 click.wav\n#00151:0101\n";
    for (header, warnings) in [
        ("#LNTYPE 0", ignored("#LNTYPE")),
        ("#LNTYPE ZZ", ignored("#LNTYPE")),
        ("#LNTYPE 01", vec![]),
        // Repeated defaulted values are warned once.
        ("#LNTYPE 00\n#LNTYPE ZZ", ignored("#LNTYPE")),
    ] {
        assert_eq!(
            long(&format!("#BPM 120\n{header}\n{pair}")),
            (vec![(2_000_000, 3_000_000)], warnings),
            "{header}"
        );
    }
    // Any other number is a type the loader does not support, one too
    // large for u32 included (u32::MAX + 1, and zero-padded).
    for value in ["3", "4294967296", "0004294967296"] {
        assert_eq!(
            load_text(&format!("#BPM 120\n#LNTYPE {value}\n#00111:01\n")),
            Err(LoadError::Unsupported {
                line: 2,
                what: format!("#LNTYPE {value}")
            }),
            "{value}"
        );
    }
}

#[test]
fn headers_and_channels_stage_1_does_not_use_are_silent() {
    // Header names are case-insensitive.
    let quiet = "#BPM 120\n#BASEBPM 150\n#CharFile c.chp\n#EXTCHR 01 02\n\
                 #maker someone\n#BGA01 01 0 0 8 8 0 0\n#Text0z hello\n#EXRANK01 50\n\
                 #WAV01 click.wav\n#00111:01\n#00131:01\n#00135:01\n#00139:01\n";
    let loaded = load_text(quiet).unwrap();
    assert_eq!(loaded.warnings, vec![]);
    // Invisible notes are not notes.
    assert_eq!(loaded.chart.lanes.iter().map(Vec::len).sum::<usize>(), 1);
    // Other unknown headers and channels, judge-rank changes (A0) included,
    // still warn: a background MIDI file (its music would be missing), and
    // numbered definitions without a two-character id.
    let loud = "#BPM 120\n#WAV01 click.wav\n#FOO 1\n#MIDIFILE m.mid\n#EXRANK 50\n\
                #BGA001 01 0 0 8 8 0 0\n#TEXT0! hi\n#00111:01\n#001A0:01\n#00117:01\n";
    assert_eq!(
        load_text(loud)
            .unwrap()
            .warnings
            .into_iter()
            .map(|w| w.kind)
            .collect::<Vec<_>>(),
        vec![
            WarningKind::UnknownHeader("FOO".to_owned()),
            WarningKind::UnknownHeader("MIDIFILE".to_owned()),
            WarningKind::UnknownHeader("EXRANK".to_owned()),
            WarningKind::UnknownHeader("BGA001".to_owned()),
            WarningKind::UnknownHeader("TEXT0!".to_owned()),
            WarningKind::IgnoredChannel("A0".to_owned()),
            WarningKind::IgnoredChannel("17".to_owned()),
        ]
    );
}

#[test]
fn defexrank_below_one_is_ignored_so_rank_stays() {
    let loaded =
        load_text("#RANK 3\n#DEFEXRANK 0\n#BPM 120\n#WAV01 click.wav\n#00111:01\n").unwrap();
    assert_eq!(loaded.chart.meta.rank, Some(JudgeRank::Rank(3)));
    assert_eq!(
        loaded.warnings,
        vec![bms_parser::Warning {
            line: Some(2),
            kind: WarningKind::IgnoredValue("#DEFEXRANK")
        }]
    );
    // Fractional below 1, negative and non-numeric values are ignored the
    // same way, before or after #RANK, and each is warned.
    let rank = |text: &str| {
        let loaded =
            load_text(&format!("{text}\n#BPM 120\n#WAV01 click.wav\n#00111:01\n")).unwrap();
        let warned = loaded
            .warnings
            .iter()
            .filter(|w| w.kind == WarningKind::IgnoredValue("#DEFEXRANK"))
            .count();
        (loaded.chart.meta.rank, warned)
    };
    for (text, expected) in [
        ("#RANK 3\n#DEFEXRANK 0.999", (Some(JudgeRank::Rank(3)), 1)),
        ("#RANK 3\n#DEFEXRANK -2", (Some(JudgeRank::Rank(3)), 1)),
        ("#RANK 3\n#DEFEXRANK easy", (Some(JudgeRank::Rank(3)), 1)),
        ("#DEFEXRANK 0\n#RANK 2", (Some(JudgeRank::Rank(2)), 1)),
        ("#DEFEXRANK 0", (None, 1)),
        (
            "#DEFEXRANK 120\n#DEFEXRANK 0\n#DEFEXRANK 0.5",
            (Some(JudgeRank::DefExRankMilli(120_000)), 2),
        ),
    ] {
        assert_eq!(rank(text), expected, "{text}");
    }
    // 1 and above replace it, as before.
    let loaded =
        load_text("#RANK 3\n#DEFEXRANK 1\n#BPM 120\n#WAV01 click.wav\n#00111:01\n").unwrap();
    assert_eq!(
        loaded.chart.meta.rank,
        Some(JudgeRank::DefExRankMilli(1_000))
    );
}
