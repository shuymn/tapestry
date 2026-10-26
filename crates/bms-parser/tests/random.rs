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

fn times(notes: &[bms_model::Note]) -> Vec<i64> {
    notes.iter().map(|n| n.time.0).collect()
}

/// The keys (lane indices) holding a note.
fn keys(loaded: &Loaded) -> Vec<usize> {
    keys_of(&loaded.chart.lanes)
}

fn keys_of(lanes: &[Vec<bms_model::Note>; 8]) -> Vec<usize> {
    (0..8).filter(|&l| !lanes[l].is_empty()).collect()
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
fn nested_blocks_read_by_the_top_flag_only() {
    let text = chart("random_nested.bms");
    for (outer, inner, read) in [
        (1, 1, vec![1, 3]),
        (1, 2, vec![2, 4]),
        (2, 1, vec![1, 3]),
        (2, 2, vec![2, 4]),
    ] {
        // The first #RANDOM is the outer block; both inner ones get `inner`.
        // Each inner #IF is the top flag inside its outer branch, so it is
        // read by the inner value whatever the outer one, as charts expect.
        let fixed = text
            .replacen("#RANDOM 2", &format!("#SETRANDOM {outer}"), 1)
            .replace("#RANDOM 2", &format!("#SETRANDOM {inner}"));
        let loaded = load(&fixed).unwrap();
        assert_eq!(keys(&loaded), read, "outer {outer}, inner {inner}");
        // Both inner blocks are drawn.
        assert_eq!(
            loaded.chart.identity.random_results,
            vec![outer, inner, inner]
        );
        assert_eq!(loaded.warnings, vec![]);
    }
}

#[test]
fn branch_results_are_recorded_in_order() {
    // Seed 1 draws 10451216379200822465, 13757245211066428519,
    // 17911839290282890590 and 8196980753821780235 (an independent
    // computation): % 5 + 1 gives 1, then 5; the #RANDOM 9 inside the branch
    // not read is drawn too (% 9 + 1 = 4), so the final #RANDOM 7 takes the
    // fourth value (% 7 + 1 = 1). The second block has no matching #IF and
    // is recorded anyway; #SETRANDOM values are recorded too.
    let text = "#BPM 120\n#WAV01 click.wav\n\
                #RANDOM 5\n#IF 1\n#00111:01\n#ENDIF\n#ENDRANDOM\n\
                #RANDOM 5\n#IF 1\n#00112:01\n#ENDIF\n\
                #SETRANDOM 7\n#IF 3\n#RANDOM 9\n#ENDIF\n#ENDRANDOM\n\
                #RANDOM 7\n";
    let loaded = load_seeded(text, 1).unwrap();
    assert_eq!(loaded.chart.identity.random_results, vec![1, 5, 7, 4, 1]);
    assert_eq!(keys(&loaded), vec![1]);
}

#[test]
fn deep_blocks_are_bounded_only_by_the_line_limit() {
    // 100 levels of #RANDOM and #IF, never closed: each line costs the same
    // whatever the depth, so nothing refuses it; the note after them is read.
    let mut text = String::from("#BPM 120\n#WAV01 click.wav\n");
    for _ in 0..100 {
        text.push_str("#SETRANDOM 1\n#IF 1\n");
    }
    text.push_str("#00111:01\n");
    let loaded = load(&text).unwrap();
    assert_eq!(loaded.chart.identity.random_results.len(), 100);
    assert_eq!(keys(&loaded), vec![1]);
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
    // #IF without #RANDOM pushes no flag, so both branches are read and
    // #ELSE and #ENDIF find no flag. A second such #IF is not warned again.
    assert_eq!(
        warned(
            "#BPM 120\n#WAV01 click.wav\n#IF 1\n#00111:01\n#ELSE\n#00112:01\n#ENDIF\n#IF 2\n#00113:01\n"
        ),
        (
            vec![1, 2, 3],
            vec![
                WarningKind::UnmatchedDirective("#IF"),
                WarningKind::UnmatchedDirective("#ELSE"),
                WarningKind::UnmatchedDirective("#ENDIF"),
            ]
        )
    );
    // The #ENDIF after a flagless #IF pops whatever flag is on top: here
    // the #IF 2 of value 1, so the last note is read.
    assert_eq!(
        warned(
            "#BPM 120\n#WAV01 click.wav\n#SETRANDOM 1\n#IF 2\n#ENDRANDOM\n#IF 1\n#00111:01\n#ENDIF\n#00112:01\n"
        ),
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
    for bad in [
        "#RANDOM 0",
        "#RANDOM -1",
        "#RANDOM x",
        "#RANDOM 2147483648",
        "#SETRANDOM 0",
        "#IF 0",
        "#IF x",
    ] {
        let text = format!("#BPM 120\n{bad}\n#00111:01\n");
        assert!(
            matches!(load(&text), Err(LoadError::InvalidValue { line: 2, .. })),
            "{bad}"
        );
    }
}

#[test]
fn a_random_inside_a_branch_not_read_still_draws_and_tests() {
    // The inner #RANDOM is drawn whatever the outer value, and its #IF tests
    // it on its own, as charts expect; #ELSE then turns the outer flag.
    for (outer, keys_read, results) in [(1, vec![1], vec![1, 1]), (2, vec![1, 2], vec![2, 1])] {
        let text = format!(
            "#BPM 120\n#WAV01 click.wav\n#SETRANDOM {outer}\n#IF 1\n#RANDOM 1\n#IF 1\n\
             #00111:01\n#ENDIF\n#ELSE\n#00112:01\n#ENDIF\n"
        );
        let loaded = load(&text).unwrap();
        assert_eq!(keys(&loaded), keys_read, "outer {outer}");
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
fn if_flags_stack_as_charts_expect() {
    let warned = |text: &str| {
        let loaded = load(text).unwrap();
        let warnings: Vec<_> = loaded
            .warnings
            .into_iter()
            .map(|w| (w.line, w.kind))
            .collect();
        (keys_of(&loaded.chart.lanes), warnings)
    };
    // Value 2: #IF 2 (line 5) stacks on #IF 1, so its branch (key 2) is
    // read; its #ENDIF makes #IF 1 count again, so key 3 is skipped; the
    // last #ENDIF closes #IF 1 and key 1 is read.
    let text = "#BPM 120\n#WAV01 click.wav\n#SETRANDOM 2\n#IF 1\n#IF 2\n#00112:01\n#ENDIF\n#00113:01\n#ENDIF\n#00111:01\n";
    assert_eq!(
        warned(text),
        (vec![1, 2], vec![(Some(5), WarningKind::UnclosedIf)])
    );
    // The same values in two #RANDOM blocks are still two blocks: an #IF of
    // the second over one of the first is not warned about.
    let two =
        "#BPM 120\n#WAV01 click.wav\n#RANDOM 1\n#IF 1\n#ENDRANDOM\n#RANDOM 1\n#IF 1\n#00111:01\n";
    assert_eq!(
        warned(two),
        (vec![1], vec![(None, WarningKind::UnclosedIf)])
    );
    // An #IF inside a new #RANDOM tests that value alone: its branch is
    // read though the outer #IF 1 is not; after it the outer flag counts
    // again.
    let nested = "#BPM 120\n#WAV01 click.wav\n#SETRANDOM 2\n#IF 1\n#SETRANDOM 3\n#IF 3\n#00112:01\n#ENDIF\n#ENDRANDOM\n#00113:01\n#ENDIF\n#00111:01\n";
    assert_eq!(warned(nested).0, vec![1, 2]);
    // Thirty 45-way blocks, each closed by a single #ENDIF and none by
    // #ENDRANDOM: every block is read by its own value. After the last
    // #ENDIF the last block's #IF 44 counts again, and its value is 31, so
    // the note of measure 31 is skipped. The stacked #IFs are warned about
    // once (line 6, the first #IF 2), and the ones left open at the end.
    let mut blocks = String::from("#BPM 120\n#WAV01 click.wav\n");
    let mut expected = Vec::new();
    for b in 1..=30_usize {
        let value = (b * 7) % 45 + 1;
        blocks.push_str(&format!("#SETRANDOM {value}\n"));
        for k in 1..=45 {
            let slots: String = (1..=45).map(|s| if s == k { "01" } else { "00" }).collect();
            blocks.push_str(&format!("#IF {k}\n#{b:03}11:{slots}\n"));
        }
        blocks.push_str("#ENDIF\n");
        // Measure b starts at 2 s × b; slot value - 1 of 45 is that many
        // 45ths of a 2 s measure later.
        let n = |x: usize| i64::try_from(x).unwrap();
        expected.push(2_000_000 * n(b) + 2_000_000 * n(value - 1) / 45);
    }
    blocks.push_str("#03111:01\n");
    let loaded = load(&blocks).unwrap();
    assert_eq!(times(&loaded.chart.lanes[1]), expected);
    let warnings: Vec<_> = loaded
        .warnings
        .into_iter()
        .map(|w| (w.line, w.kind))
        .collect();
    assert_eq!(
        warnings,
        vec![
            (Some(6), WarningKind::UnclosedIf),
            (None, WarningKind::UnclosedIf)
        ]
    );
}
