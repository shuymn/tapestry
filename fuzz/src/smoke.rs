//! The seeds reach what each target is for: run through the target
//! functions, valid seeds decode, and each malformed seed fails with the
//! error it was built for.

use std::path::Path;

use replay::{DecodeError, DecodeLimits, Section};
use rules::{CanonicalError, RuleError};

use crate::{RawSummary, bms_load, preset_canonical, raw_input, replay_body, replay_file, seeds};

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
            "custom",
            "beatoraja_style",
            "gauge_policies",
            "long",
            "other_version",
            "non_minimal_varint",
            "count_beyond_data",
            "unknown_tag",
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
            "custom" | "beatoraja_style" | "gauge_policies" | "long" => Ok(()),
            "other_version" => Err(CanonicalError::UnsupportedVersion(10)),
            "older_version" => Err(CanonicalError::UnsupportedVersion(8)),
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
