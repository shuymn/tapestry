//! The seeds reach what each target is for: run through the target
//! functions, valid seeds decode, and each malformed seed fails with the
//! error it was built for.

use std::path::Path;

use replay::{DecodeError, DecodeLimits, Section};
use rules::{CanonicalError, RuleError};
use skin_beatoraja::{Header, ImportError, Imported, NoteKind, Rule, Syntax, Usage};

use crate::{
    RawSummary, bms_load, draw, preset_canonical, raw_input, replay_body, replay_file, seeds,
    skin_fnt, skin_json,
};

/// Every seed in `expected` is generated, and no other: a deleted seed
/// fails the test instead of going unchecked.
fn assert_names(seeds: &[seeds::Seed], expected: &[&str]) {
    let mut names: Vec<&str> = seeds.iter().map(|(name, _)| *name).collect();
    let mut expected = expected.to_vec();
    names.sort_unstable();
    expected.sort_unstable();
    assert_eq!(names, expected);
}

#[test]
fn replay_body_seeds_reach_sections_counts_and_successful_decodes() {
    let seeds = seeds::replay_bodies();
    assert_names(
        &seeds,
        &[
            "two_sources",
            "no_events",
            "extremes",
            "practice_round",
            "section_past_end",
            "count_beyond_data",
            "too_many_events",
            "zero_sources",
            "trailing_bytes",
            "truncated",
        ],
    );
    for (name, body) in seeds {
        let result = replay_body(&body).map(|_| ());
        match name {
            "two_sources" | "no_events" | "extremes" | "practice_round" => {
                assert_eq!(result, Ok(()), "{name}");
            }
            "section_past_end" => assert!(
                matches!(
                    result,
                    Err(DecodeError::SectionLength {
                        section: Section::Status,
                        ..
                    })
                ),
                "{name}: {result:?}"
            ),
            "count_beyond_data" => {
                assert_eq!(result, Err(DecodeError::CountBeyondData(2)), "{name}");
            }
            "too_many_events" => {
                assert_eq!(result, Err(DecodeError::TooManyEvents(1 << 23)), "{name}");
            }
            "zero_sources" => assert_eq!(result, Err(DecodeError::NoSources), "{name}"),
            "trailing_bytes" => assert_eq!(result, Err(DecodeError::TrailingBytes), "{name}"),
            "truncated" => assert_eq!(result, Err(DecodeError::Truncated), "{name}"),
            _ => panic!("no expectation for seed {name}"),
        }
    }
}

#[test]
fn replay_file_seeds_decode_or_fail_at_the_envelope() {
    let seeds = seeds::replay_files();
    assert_names(
        &seeds,
        &[
            "two_sources",
            "no_events",
            "extremes",
            "practice_round",
            "bad_magic",
            "unsupported_version",
            "unsupported_encoding",
            "declared_too_large",
            "decompress",
            "header_only",
            "length_mismatch",
        ],
    );
    for (name, file) in seeds {
        let result = replay_file(&file).map(|_| ());
        let expected = match name {
            "two_sources" | "no_events" | "extremes" | "practice_round" => Ok(()),
            "bad_magic" => Err(DecodeError::BadMagic),
            "unsupported_version" => Err(DecodeError::UnsupportedVersion(
                replay::FORMAT_VERSION.wrapping_add(1),
            )),
            "unsupported_encoding" => Err(DecodeError::UnsupportedEncoding(2)),
            "declared_too_large" => Err(DecodeError::DeclaredLengthTooLarge {
                declared: DecodeLimits::STANDARD.max_uncompressed + 1,
                max: DecodeLimits::STANDARD.max_uncompressed,
            }),
            "decompress" => Err(DecodeError::Decompress),
            // An empty frame decompresses to the declared empty body.
            "header_only" => Err(DecodeError::Truncated),
            "length_mismatch" => {
                let declared = u32::from_le_bytes(file[7..11].try_into().unwrap());
                Err(DecodeError::LengthMismatch {
                    declared,
                    actual: declared as usize - 1,
                })
            }
            _ => panic!("no expectation for seed {name}"),
        };
        assert_eq!(result, expected, "{name}");
    }
}

#[test]
fn raw_input_seeds_reach_packets_reports_and_early_stops() {
    let summary = |count, blocks, packets, hid_blocks, reports| RawSummary {
        count,
        blocks,
        packets,
        hid_blocks,
        reports,
    };
    let seeds = seeds::raw_inputs();
    assert_names(
        &seeds,
        &[
            "keyboard_press",
            "hid_reports",
            "mixed_padded",
            "truncated",
            "count_beyond",
        ],
    );
    for (name, data) in seeds {
        let expected = match name {
            "keyboard_press" => summary(1, 1, 1, 0, 0),
            "hid_reports" => summary(1, 1, 0, 1, 3),
            "mixed_padded" => summary(3, 3, 2, 1, 3),
            "truncated" => summary(2, 1, 1, 0, 0),
            "count_beyond" => summary(5, 2, 2, 0, 0),
            _ => panic!("no expectation for seed {name}"),
        };
        assert_eq!(raw_input(&data), expected, "{name}");
    }
    assert_eq!(raw_input(&[]), RawSummary::default());
}

#[test]
fn preset_canonical_seeds_decode_or_fail_as_built() {
    let seeds = seeds::preset_canonicals();
    assert_names(
        &seeds,
        &[
            "tapestry",
            "beatoraja_style",
            "lr2_style",
            "gauge_policies",
            "long",
            "other_version",
            "non_minimal_varint",
            "count_beyond_data",
            "unknown_tag",
            "duplicate_lamp",
            "varint_overflow",
            "invalid_utf8",
            "value_too_large",
            "unknown_implementation",
            "wrong_length",
            "wrong_percent_length",
            "older_version",
            "trailing_bytes",
            "truncated",
            "invalid",
        ],
    );
    for (name, bytes) in seeds {
        let result = preset_canonical(&bytes).map(|_| ());
        let expected = match name {
            "tapestry" | "beatoraja_style" | "lr2_style" | "gauge_policies" | "long" => Ok(()),
            "other_version" => Err(CanonicalError::UnsupportedVersion(12)),
            "older_version" => Err(CanonicalError::UnsupportedVersion(10)),
            "wrong_percent_length" => Err(CanonicalError::WrongLength {
                field: "rank_percent",
                len: 6,
            }),
            "non_minimal_varint" => Err(CanonicalError::NonMinimalVarint),
            "count_beyond_data" => Err(CanonicalError::CountBeyondData {
                field: "gauges",
                count: 16_383,
            }),
            "unknown_tag" => Err(CanonicalError::UnknownTag {
                field: "gauge kind",
                tag: 2,
            }),
            "duplicate_lamp" => Err(CanonicalError::Invalid(RuleError::DuplicateLamp {
                rank: 0,
                first: "normal".to_owned(),
                second: "survival".to_owned(),
            })),
            "varint_overflow" => Err(CanonicalError::VarintOverflow),
            "invalid_utf8" => Err(CanonicalError::InvalidUtf8 { field: "id" }),
            "value_too_large" => Err(CanonicalError::ValueTooLarge { field: "version" }),
            "unknown_implementation" => Err(CanonicalError::UnknownImplementation {
                name: "Standard".to_owned(),
            }),
            "wrong_length" => Err(CanonicalError::WrongLength {
                field: "rank_scale",
                len: 5,
            }),
            "trailing_bytes" => Err(CanonicalError::TrailingBytes(1)),
            "truncated" => Err(CanonicalError::Truncated),
            "invalid" => Err(CanonicalError::Invalid(RuleError::NoGauges)),
            _ => panic!("no expectation for seed {name}"),
        };
        assert_eq!(result, expected, "{name}");
    }
}

#[test]
fn bms_seed_charts_load_and_refuse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/bms-parser/tests/charts");
    let mut ok = 0;
    let mut charts = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let result = bms_load(&std::fs::read(&path).unwrap());
        charts += 1;
        if name.starts_with("unsupported_") {
            assert!(
                matches!(result, Err(bms_parser::LoadError::Unsupported { .. })),
                "{name}"
            );
        } else if result.is_ok() {
            ok += 1;
        }
    }
    assert!(charts >= 3, "{charts} charts in {}", dir.display());
    assert!(ok > 0, "no chart loaded");
}

fn option_names(h: &Header) -> Vec<&str> {
    h.options
        .iter()
        .map(|o| o.name.as_deref().unwrap_or(""))
        .collect()
}

#[test]
fn skin_json_seeds_resolve_or_fail_as_built() {
    let seeds = seeds::skin_jsons();
    assert_names(
        &seeds,
        &[
            "options",
            "choices",
            "lenient",
            "cycle",
            "not_play",
            "size",
            "syntax",
            "missing",
            "configured_fails",
            "images",
            "numbers",
            "text",
            "bitmap_text",
            "tracks",
            "ops",
            "offsets",
            "choices_default",
            "lanes",
            "judge",
            "gauge",
        ],
    );
    let imports: Vec<&str> = seeds::skin_imports().iter().map(|(n, _)| *n).collect();
    for (name, data) in seeds {
        if imports.contains(&name) {
            continue;
        }
        let result = skin_json(&data).map(|(h, _)| h);
        match name {
            "options" => {
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(
                    option_names(&h),
                    ["mode", "included", "when on", "nested"],
                    "{name}"
                );
                assert_eq!(h.files[0].candidates.len(), 4, "{name}");
                assert_eq!(h.offsets.len(), 1, "{name}");
            }
            "choices" => {
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.files.len(), 1, "{name}");
                assert_eq!(h.files[0].name.as_deref(), Some("one"), "{name}");
                assert_eq!(h.offsets.len(), 2, "{name}");
            }
            "lenient" => {
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.name.as_deref(), Some("unquoted name"), "{name}");
                assert_eq!(option_names(&h), ["a", "b"], "{name}");
                assert_eq!(h.offsets[0].id, 10, "{name}");
                assert!(h.offsets[0].x, "{name}");
            }
            "cycle" => assert!(
                matches!(result, Err(ImportError::IncludeCycle { .. })),
                "{name}: {result:?}"
            ),
            "not_play" => assert!(
                matches!(result, Err(ImportError::NotPlay7 { .. })),
                "{name}: {result:?}"
            ),
            "size" => assert!(
                matches!(result, Err(ImportError::Resolution { .. })),
                "{name}: {result:?}"
            ),
            "syntax" => assert!(
                matches!(
                    result,
                    Err(ImportError::Syntax {
                        what: Syntax::ExpectedSeparator,
                        ..
                    })
                ),
                "{name}: {result:?}"
            ),
            // The configured pass's error, after the pass with none
            // enabled replayed and matched.
            "configured_fails" => assert!(
                matches!(result, Err(ImportError::Read { ref name, .. }) if name == "none.json"),
                "{name}: {result:?}"
            ),
            "missing" => assert!(
                matches!(result, Err(ImportError::Read { .. })),
                "{name}: {result:?}"
            ),
            _ => panic!("no expectation for seed {name}"),
        }
    }
}

/// Usage with the given counters, the rest zero.
fn usage(elements: u32, images: u32, frames: u32, files: u32) -> Usage {
    Usage {
        elements,
        images,
        frames,
        files,
        file_attempts: files,
        keys: elements,
        ..Usage::default()
    }
}

fn kinds(i: &Imported) -> Vec<NoteKind> {
    i.report.notes.iter().map(|n| n.kind).collect()
}

#[test]
fn skin_import_seeds_import_and_draw_as_built() {
    for (name, data) in seeds::skin_imports() {
        let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        let i = imported.unwrap_or_else(|e| panic!("{name}: {e}"));
        let u = i.report.usage;
        // Quads at 0, 50, 500 and 5,000 ms, each with a perfect judgment.
        let drawn = draw(&i);
        match name {
            "images" => {
                assert_eq!(u, usage(2, 2, 14, 2), "{name}");
                assert_eq!(i.skin.sheet().frame_count(), 14, "{name}");
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "numbers" => {
                assert_eq!(u.elements, 2, "{name}");
                assert_eq!(u.widget_quads, 4 + 11, "{name}");
                assert_eq!(i.skin.quad_capacity(), 15, "{name}");
                // EX score 150: three glyphs; the perfect count 1: one.
                assert_eq!(drawn, [4, 4, 4, 4], "{name}");
            }
            "bitmap_text" => {
                // One font of 4 glyphs (a space added) and a pair, read
                // once for both texts.
                assert_eq!((u.texts, u.fonts), (2, 1), "{name}");
                assert_eq!((u.glyphs, u.kernings), (5, 1), "{name}");
                assert_eq!(u.file_attempts, 2, "{name}");
                assert_eq!(kinds(&i), [], "{name}");
                // `ABl?` and `t`, the last two as the missing glyph.
                assert_eq!(drawn, [5, 5, 5, 5], "{name}");
            }
            "text" => {
                assert_eq!((u.texts, u.fonts), (2, 1), "{name}");
                assert_eq!(
                    kinds(&i),
                    [NoteKind::Deviation(skin_beatoraja::DeviationId::D6)]
                );
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "tracks" => {
                assert_eq!(u.keys, 3, "{name}");
                assert_eq!(drawn, [1, 1, 1, 1], "{name}");
            }
            "ops" => {
                assert_eq!(u.elements, 3, "{name}");
                assert_eq!(kinds(&i), [NoteKind::UndefinedOp(7_777)], "{name}");
                assert!(i.skin.ops().total() > 0, "{name}");
                // The perfect shows with the 20 % gauge range.
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "offsets" => {
                assert_eq!(kinds(&i), [NoteKind::RuntimeOffset(4)], "{name}");
                assert_eq!(drawn, [1, 1, 1, 1], "{name}");
            }
            "choices_default" => {
                assert_eq!(i.report.files.len(), 1, "{name}");
                assert_eq!(i.report.files[0].rule, Rule::Default, "{name}");
                assert!(!i.report.files[0].ambiguous, "{name}");
                assert_eq!(drawn, [1, 1, 1, 1], "{name}");
            }
            "lanes" => {
                use skin_beatoraja::{DeviationId, LaneApprox};
                // The lanes, SUDDEN+, LIFT and HIDDEN+; the notes, bar,
                // SUDDEN+ and LIFT images over one file.
                assert_eq!((u.elements, u.keys, u.images), (4, 4, 4), "{name}");
                assert!(i.scratch.is_some(), "{name}");
                let d9 = |a| NoteKind::Deviation(DeviationId::D9(a));
                for a in [LaneApprox::BarLine, LaneApprox::CoverAbove] {
                    assert!(kinds(&i).contains(&d9(a)), "{name}: {a:?}");
                }
                assert!(kinds(&i).contains(&NoteKind::Deviation(DeviationId::D11)));
                // At 500 ms the eleven notes from 1 s to 1.1 s are on
                // screen, the last with its lower edge on the region's
                // top. SUDDEN+ at 0 shows the strip above the region in
                // every frame; LIFT at 0 is not drawn.
                assert_eq!(drawn, [1, 1, 12, 1], "{name}");
            }
            "judge" => {
                // Seven grades, combos of slots 0, 1, 2 and 6 (slots 3-5
                // noted): the widest state is a grade and 4 cells.
                assert_eq!(u.widget_quads, 5, "{name}");
                assert_eq!(kinds(&i), [NoteKind::Ignored { field: "numbers" }; 3]);
                // The perfect and its combo of 1.
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "gauge" => {
                assert_eq!(u.widget_quads, 51, "{name}");
                assert_eq!(
                    kinds(&i),
                    [NoteKind::Deviation(skin_beatoraja::DeviationId::D10)]
                );
                assert_eq!(drawn, [51, 51, 51, 51], "{name}");
            }
            _ => panic!("no expectation for seed {name}"),
        }
    }
}

#[test]
fn skin_json_accepts_ops_charged_for_a_dropped_element() {
    // The condition is compiled and charged before the image's missing
    // source drops the element, so the skin keeps fewer ops than charged.
    let data = br#"{"type": 0, "w": 1920, "h": 1080,
        "source": [{"id": "gone", "path": "gone.png"}],
        "image": [{"id": "g", "src": "gone", "w": 4, "h": 4}],
        "destination": [{"id": "g", "op": [241], "dst": [{"w": 4, "h": 4}]}]}"#;
    let (_, imported) = skin_json(data).expect("the header resolves");
    let i = imported.expect("the skin imports");
    assert!(i.report.usage.ops > 0);
    assert_eq!(i.skin.ops().total(), 0);
    assert_eq!(kinds(&i), [NoteKind::MissingAsset]);
}

#[test]
fn skin_fnt_seeds_parse_or_fail_as_built() {
    use skin_model::FntErrorKind as K;
    let seeds = seeds::skin_fnts();
    assert_names(
        &seeds,
        &[
            "font",
            "crlf",
            "two_pages",
            "supplementary",
            "binary",
            "no_size",
            "page_id",
            "glyph_page",
            "duplicate_id",
            "count",
            "trailing",
            "lone_cr",
            "no_glyphs",
        ],
    );
    for (name, bytes) in seeds {
        let result = skin_fnt(&bytes);
        let expected = match name {
            "font" | "crlf" | "supplementary" => {
                let f = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!((f.glyphs().len(), f.kernings().len()), (4, 1), "{name}");
                assert!(f.missing().is_some(), "{name}");
                assert_eq!(f.pages(), ["fonts/font_0.png"], "{name}");
                continue;
            }
            "two_pages" => {
                let f = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(f.pages(), ["fonts/font_0.png", "fonts/sub/font_1.png"]);
                continue;
            }
            "binary" => K::Padding,
            "no_size" => K::Metadata("size"),
            "page_id" => K::PageId,
            "glyph_page" => K::GlyphPage,
            "duplicate_id" => K::DuplicateId,
            "count" => K::Count("chars"),
            "trailing" => K::Unexpected,
            "lone_cr" => K::LoneCr,
            "no_glyphs" => K::NoGlyphs,
            _ => panic!("no expectation for seed {name}"),
        };
        assert_eq!(
            result.map(|_| ()).map_err(|e| e.kind),
            Err(expected),
            "{name}"
        );
    }
}
