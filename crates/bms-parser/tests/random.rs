//! `#RANDOM` tests over self-made charts and generated text.

use std::fs;
use std::path::Path;

use bms_model::TimeUs;
use bms_parser::{LoadError, LoadOptions, Loaded, WarningKind, load_bms};
use tempfile::TempDir;

fn chart(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/charts")
        .join(name);
    String::from_utf8(fs::read(&path).unwrap()).unwrap()
}

fn load_seeded(text: &str, seed: u64) -> Result<Loaded, LoadError> {
    let dir = TempDir::new().unwrap();
    // The loader only resolves the path; the bytes need not be audio.
    fs::write(dir.path().join("click.wav"), b"RIFF").unwrap();
    load_bms(
        text.as_bytes(),
        dir.path(),
        &LoadOptions {
            seed,
            ..LoadOptions::default()
        },
    )
}

fn load(text: &str) -> Result<Loaded, LoadError> {
    load_seeded(text, 0)
}

/// The keys (lane indices) holding a note.
fn keys(loaded: &Loaded) -> Vec<usize> {
    (0..8)
        .filter(|&l| !loaded.chart.lanes[l].is_empty())
        .collect()
}

#[test]
fn setrandom_selects_each_branch() {
    let text = chart("random_branches.bms");
    let mut hashes = Vec::new();
    for (value, key) in [(1, 2), (2, 3), (3, 4)] {
        let fixed = text.replace("#RANDOM 3", &format!("#SETRANDOM {value}"));
        let loaded = load(&fixed).unwrap();
        assert_eq!(keys(&loaded), vec![1, key], "#SETRANDOM {value}");
        assert_eq!(loaded.chart.lanes[key][0].time, TimeUs(2_000_000));
        assert_eq!(loaded.chart.identity.random_results, vec![value]);
        hashes.push(loaded.chart.identity.resolved_hash);
    }
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);
    assert_ne!(hashes[0], hashes[2]);
}

#[test]
fn seeded_branches_match_reference_values() {
    // From an independent SplitMix64 computation at authoring time: the
    // first output for seed 0 is 16294208416658607535 (% 3 + 1 = 2), for
    // seed 1 10451216379200822465 (3), for seed 42 13679457532755275413 (2).
    let text = chart("random_branches.bms");
    for (seed, value) in [(0, 2), (1, 3), (42, 2)] {
        let loaded = load_seeded(&text, seed).unwrap();
        assert_eq!(
            loaded.chart.identity.random_results,
            vec![value],
            "seed {seed}"
        );
        assert_eq!(keys(&loaded), vec![1, value as usize + 1], "seed {seed}");
    }
}

#[test]
fn nested_if_selects_inner_branch() {
    let text = chart("random_nested.bms");
    for (outer, inner, key) in [(1, 1, 1), (1, 2, 2), (2, 1, 3), (2, 2, 4)] {
        // The first #RANDOM is the outer block; both inner ones get `inner`.
        let fixed = text
            .replacen("#RANDOM 2", &format!("#SETRANDOM {outer}"), 1)
            .replace("#RANDOM 2", &format!("#SETRANDOM {inner}"));
        let loaded = load(&fixed).unwrap();
        assert_eq!(keys(&loaded), vec![key], "outer {outer}, inner {inner}");
        // The inner block of the branch not taken is never read.
        assert_eq!(loaded.chart.identity.random_results, vec![outer, inner]);
        assert_eq!(loaded.warnings, vec![]);
    }
}

#[test]
fn branch_results_are_recorded_in_order() {
    // Seed 1 draws 10451216379200822465, 13757245211066428519, then a third
    // and fourth value (an independent computation): % 5 + 1 gives 1, then
    // 5. The second block has no matching #IF and is recorded anyway;
    // #SETRANDOM values are recorded too; the block inside the branch not
    // taken draws nothing, so the final #RANDOM 7 takes the third value
    // (% 7 + 1 = 2) rather than the fourth (1).
    let text = "#BPM 120\n#WAV01 click.wav\n\
                #RANDOM 5\n#IF 1\n#00111:01\n#ENDIF\n#ENDRANDOM\n\
                #RANDOM 5\n#IF 1\n#00112:01\n#ENDIF\n\
                #SETRANDOM 7\n#IF 3\n#RANDOM 9\n#ENDIF\n#ENDRANDOM\n\
                #RANDOM 7\n";
    let loaded = load_seeded(text, 1).unwrap();
    assert_eq!(loaded.chart.identity.random_results, vec![1, 5, 7, 2]);
    assert_eq!(keys(&loaded), vec![1]);
}

#[test]
fn nesting_deeper_than_cap_is_rejected() {
    // Each level opens a #RANDOM and an #IF: 8 levels are 16 blocks.
    let nested = |levels: usize| {
        let mut text = String::from("#BPM 120\n#WAV01 click.wav\n#00111:01\n");
        for _ in 0..levels {
            text.push_str("#SETRANDOM 1\n#IF 1\n");
        }
        text
    };
    assert_eq!(
        load(&nested(8))
            .unwrap()
            .chart
            .identity
            .random_results
            .len(),
        8
    );
    let too_deep = nested(8) + "#RANDOM 2\n";
    assert_eq!(load(&too_deep), Err(LoadError::NestingTooDeep { line: 20 }));
}

#[test]
fn headers_apply_only_in_the_branch_taken() {
    let text = "#BPM 120\n#WAV01 click.wav\n\
                #RANDOM 2\n\
                #IF 1\n#BPM01 240\n#LNOBJ ZZ\n#WAV02 other.wav\n#ENDIF\n\
                #IF 2\n#BPM01 60\n#ENDIF\n\
                #ENDRANDOM\n\
                #00108:01\n#00111:01ZZ\n#00211:02\n";
    for value in [1, 2] {
        let fixed = text.replace("#RANDOM 2", &format!("#SETRANDOM {value}"));
        let loaded = load(&fixed).unwrap();
        let lane = &loaded.chart.lanes[1];
        if value == 1 {
            // 240 BPM from measure 1, ZZ ends a long note, sound 02 exists.
            assert_eq!(loaded.chart.sounds.len(), 2);
            assert_eq!(lane.len(), 2);
            assert!(matches!(
                lane[0].kind,
                bms_model::NoteKind::Long { end, .. } if end == TimeUs(2_500_000)
            ));
            assert_eq!(lane[1].time, TimeUs(3_000_000));
        } else {
            // 60 BPM, ZZ is a plain note with no sound, 02 is undefined.
            assert_eq!(loaded.chart.sounds.len(), 1);
            assert_eq!(lane.len(), 3);
            assert_eq!(lane[1].time, TimeUs(4_000_000));
            assert_eq!(lane[2].time, TimeUs(6_000_000));
        }
    }
}

#[test]
fn stray_directives_warn_and_bad_values_fail() {
    let warned = |text: &str| {
        let loaded = load(text).unwrap();
        (
            keys(&loaded),
            loaded
                .warnings
                .into_iter()
                .map(|w| w.kind)
                .collect::<Vec<_>>(),
        )
    };
    // #IF without #RANDOM: never taken; #ELSE takes over.
    assert_eq!(
        warned("#BPM 120\n#WAV01 click.wav\n#IF 1\n#00111:01\n#ELSE\n#00112:01\n#ENDIF\n"),
        (vec![2], vec![WarningKind::UnmatchedDirective("#IF")])
    );
    // #ENDIF, #ELSE and #ENDRANDOM without a block: ignored.
    assert_eq!(
        warned("#BPM 120\n#WAV01 click.wav\n#ENDIF\n#ELSE\n#ENDRANDOM\n#00111:01\n"),
        (
            vec![1],
            vec![
                WarningKind::UnmatchedDirective("#ENDIF"),
                WarningKind::UnmatchedDirective("#ELSE"),
                WarningKind::UnmatchedDirective("#ENDRANDOM"),
            ]
        )
    );
    // A missing #ENDIF: the branch ends at the end of the file.
    assert_eq!(
        warned("#BPM 120\n#WAV01 click.wav\n#00111:01\n#SETRANDOM 1\n#IF 1\n#00112:01\n"),
        (vec![1, 2], vec![WarningKind::UnclosedIf])
    );
    // Consecutive blocks without #ENDRANDOM do not nest.
    let mut sequential = String::from("#BPM 120\n#WAV01 click.wav\n#00111:01\n");
    for _ in 0..40 {
        sequential.push_str("#RANDOM 2\n#IF 1\n#ENDIF\n");
    }
    assert_eq!(
        load(&sequential)
            .unwrap()
            .chart
            .identity
            .random_results
            .len(),
        40
    );
    for bad in ["#RANDOM 0", "#RANDOM -1", "#RANDOM x", "#SETRANDOM 0"] {
        let text = format!("#BPM 120\n{bad}\n#00111:01\n");
        assert!(
            matches!(load(&text), Err(LoadError::InvalidValue { line: 2, .. })),
            "{bad}"
        );
    }
}

#[test]
fn else_closes_a_random_left_open_in_its_branch() {
    // The inner block has no #ENDRANDOM; #ELSE still belongs to the outer #IF.
    for (outer, key, results) in [(1, 1, vec![1, 1]), (2, 2, vec![2])] {
        let text = format!(
            "#BPM 120\n#WAV01 click.wav\n#SETRANDOM {outer}\n#IF 1\n#RANDOM 1\n#IF 1\n\
             #00111:01\n#ENDIF\n#ELSE\n#00112:01\n#ENDIF\n"
        );
        let loaded = load(&text).unwrap();
        assert_eq!(keys(&loaded), vec![key], "outer {outer}");
        assert_eq!(
            loaded.chart.identity.random_results, results,
            "outer {outer}"
        );
        assert_eq!(loaded.warnings, vec![], "outer {outer}");
    }
}

#[test]
fn elseif_is_rejected_whether_or_not_its_branch_is_reached() {
    for value in [1, 2] {
        let text = format!(
            "#BPM 120\n#WAV01 click.wav\n#SETRANDOM {value}\n#IF 1\n#00111:01\n\
             #ELSEIF 2\n#00112:01\n#ENDIF\n"
        );
        assert_eq!(
            load(&text),
            Err(LoadError::Unsupported {
                line: 6,
                what: "#ELSEIF".to_owned()
            }),
            "value {value}"
        );
    }
}

#[test]
fn branch_records_do_not_change_the_resolved_hash() {
    // An unused block: seeds 0 and 1 draw 2 and 3, the objects are the same.
    let text = "#BPM 120\n#WAV01 click.wav\n#00111:01\n#RANDOM 3\n";
    let a = load_seeded(text, 0).unwrap().chart.identity;
    let b = load_seeded(text, 1).unwrap().chart.identity;
    assert_eq!(a.random_results, vec![2]);
    assert_eq!(b.random_results, vec![3]);
    assert_eq!(a.resolved_hash, b.resolved_hash);
}

#[test]
fn if_left_open_ends_at_the_next_if_of_its_random() {
    let warned = |text: &str| {
        let loaded = load(text).unwrap();
        let warnings: Vec<_> = loaded.warnings.iter().map(|w| (w.line, &w.kind)).collect();
        (keys(&loaded), format!("{warnings:?}"))
    };
    // #IF 2 ends #IF 1 (line 5), so its branch is read for value 2 and the
    // line after its #ENDIF is outside every block.
    let text = "#BPM 120\n#WAV01 click.wav\n#SETRANDOM 2\n#IF 1\n#IF 2\n#00112:01\n#ENDIF\n#00113:01\n#00111:01\n";
    let expected = vec![(Some(5), &WarningKind::UnclosedIf)];
    assert_eq!(warned(text), (vec![1, 2, 3], format!("{expected:?}")));
    // Twenty #IF lines without #ENDIF stay one block deep.
    let mut open = String::from("#BPM 120\n#WAV01 click.wav\n#SETRANDOM 20\n");
    for k in 1..=20 {
        open.push_str(&format!("#IF {k}\n#{:03}11:01\n", k));
    }
    let loaded = load(&open).unwrap();
    assert_eq!(loaded.chart.lanes[1].len(), 1);
    assert_eq!(loaded.chart.lanes[1][0].time, TimeUs(40_000_000));
    // An #IF inside a new #RANDOM still nests: the outer #IF 1 stays open,
    // so the line after the inner block is read only for value 1.
    let nested = "#BPM 120\n#WAV01 click.wav\n#SETRANDOM 2\n#IF 1\n#SETRANDOM 3\n#IF 3\n#00112:01\n#ENDIF\n#ENDRANDOM\n#00113:01\n#ENDIF\n#00111:01\n";
    assert_eq!(keys(&load(nested).unwrap()), vec![1]);
}
