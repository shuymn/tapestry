//! The seeds reach what each target is for: run through the target
//! functions, valid seeds decode, and each malformed seed fails with the
//! error it was built for.

use std::path::Path;

use replay::{DecodeError, Section};

use crate::{RawSummary, bms_load, raw_input, replay_body, replay_file, seeds};

/// Whether `e` is refused before the body: what `replay_body` must never
/// see, since it builds a valid file around every input.
const fn is_envelope_error(e: &DecodeError) -> bool {
    matches!(
        e,
        DecodeError::FileTooLarge { .. }
            | DecodeError::BadMagic
            | DecodeError::UnsupportedVersion(_)
            | DecodeError::UnsupportedEncoding(_)
            | DecodeError::DeclaredLengthTooLarge { .. }
            | DecodeError::Decompress
            | DecodeError::LengthMismatch { .. }
    )
}

#[test]
fn replay_body_seeds_reach_sections_counts_and_successful_decodes() {
    let mut ok = 0;
    for (name, body) in seeds::replay_bodies() {
        let result = replay_body(&body);
        if let Err(e) = &result {
            assert!(!is_envelope_error(e), "{name}: {e:?}");
        }
        let result = result.map(|_| ());
        match name {
            "two_sources" | "no_events" | "extremes" => {
                assert_eq!(result, Ok(()), "{name}");
                ok += 1;
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
    assert_eq!(ok, 3);
}

#[test]
fn replay_file_seeds_decode_or_fail_at_the_envelope() {
    for (name, file) in seeds::replay_files() {
        let result = replay_file(&file).map(|_| ());
        let expected = match name {
            "two_sources" | "no_events" | "extremes" => Ok(()),
            "bad_magic" => Err(DecodeError::BadMagic),
            "unsupported_version" => Err(DecodeError::UnsupportedVersion(
                replay::FORMAT_VERSION.wrapping_add(1),
            )),
            "unsupported_encoding" => Err(DecodeError::UnsupportedEncoding(2)),
            "declared_too_large" => Err(DecodeError::DeclaredLengthTooLarge {
                declared: (64 << 20) + 1,
                max: 64 << 20,
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
    for (name, data) in seeds::raw_inputs() {
        let expected = match name {
            "keyboard_press" => summary(1, 1, 1, 0, 0),
            "hid_reports" => summary(1, 1, 0, 1, 3),
            "mixed_padded" => summary(3, 3, 2, 1, 3),
            "truncated" => summary(2, 1, 1, 0, 0),
            "count_beyond" => summary(5, 2, 2, 0, 0),
            _ => panic!("no expectation for seed {name}"),
        };
        let found = raw_input(&data);
        assert_eq!(found, expected, "{name}");
        assert_eq!(
            found.stopped_early(),
            matches!(name, "truncated" | "count_beyond"),
            "{name}"
        );
    }
    assert_eq!(raw_input(&[]), RawSummary::default());
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
