//! The seeds reach what each target is for: run through the target
//! functions, valid seeds decode, and each malformed seed fails with the
//! error it was built for.

use std::path::Path;

use replay::{DecodeError, DecodeLimits, Section};
use rules::{CanonicalError, RuleError};
use skin_beatoraja::{Header, ImportError, Imported, NoteKind, Rule, Syntax, Usage};
use skin_model::ScreenKind;

use crate::{
    RawSummary, SEEDED_BYTES, bms_load, draw, preset_canonical, raw_input, replay_body,
    replay_file, seeded_readouts, seeds, skin_fnt, skin_json, skin_lua, skin_png,
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
            "ready_history",
            "cancelled_capture",
            "adjustment_controls",
            "section_past_end",
            "too_many_openings",
            "history_events_beyond_limit",
            "history_range",
            "history_event_count",
            "history_origin",
            "history_unknown_source",
            "cancelled_with_winner",
            "cancelled_without_opening",
            "count_beyond_data",
            "too_many_events",
            "zero_sources",
            "tuning_tag",
            "trailing_bytes",
            "truncated",
        ],
    );
    for (name, body) in seeds {
        let result = replay_body(&body).map(|_| ());
        match name {
            "two_sources"
            | "no_events"
            | "extremes"
            | "practice_round"
            | "ready_history"
            | "cancelled_capture"
            | "adjustment_controls" => {
                assert_eq!(result, Ok(()), "{name}");
            }
            "section_past_end" => assert!(
                matches!(
                    result,
                    Err(DecodeError::SectionLength {
                        section: Section::History,
                        ..
                    })
                ),
                "{name}: {result:?}"
            ),
            "too_many_openings" => {
                assert_eq!(result, Err(DecodeError::TooManyOpenings(65)), "{name}");
            }
            "history_events_beyond_limit" => assert_eq!(
                result,
                Err(DecodeError::TooManyHistoryEvents(65_537)),
                "{name}"
            ),
            "history_range" => {
                assert_eq!(result, Err(DecodeError::InvalidHistory("range")), "{name}");
            }
            "history_event_count" => assert_eq!(
                result,
                Err(DecodeError::InvalidHistory("event count")),
                "{name}"
            ),
            "history_origin" => {
                assert_eq!(result, Err(DecodeError::InvalidHistory("origin")), "{name}");
            }
            "history_unknown_source" => {
                assert_eq!(result, Err(DecodeError::UnknownSource(9)), "{name}");
            }
            "cancelled_with_winner" => {
                assert_eq!(result, Err(DecodeError::TrailingBytes), "{name}");
            }
            "cancelled_without_opening" => assert_eq!(
                result,
                Err(DecodeError::InvalidHistory("no opening")),
                "{name}"
            ),
            "count_beyond_data" => {
                assert_eq!(result, Err(DecodeError::CountBeyondData(2)), "{name}");
            }
            "too_many_events" => {
                assert_eq!(result, Err(DecodeError::TooManyEvents(1 << 23)), "{name}");
            }
            "zero_sources" => assert_eq!(result, Err(DecodeError::NoSources), "{name}"),
            "tuning_tag" => assert_eq!(
                result,
                Err(DecodeError::InvalidValue("speed mode")),
                "{name}"
            ),
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
            "ready_history",
            "cancelled_capture",
            "adjustment_controls",
            "bad_magic",
            "unsupported_version",
            "older_version",
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
            "two_sources"
            | "no_events"
            | "extremes"
            | "practice_round"
            | "ready_history"
            | "cancelled_capture"
            | "adjustment_controls" => Ok(()),
            "bad_magic" => Err(DecodeError::BadMagic),
            "unsupported_version" => Err(DecodeError::UnsupportedVersion(
                replay::FORMAT_VERSION.wrapping_add(1),
            )),
            "older_version" => Err(DecodeError::UnsupportedVersion(replay::FORMAT_VERSION - 1)),
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
            "other_version" => Err(CanonicalError::UnsupportedVersion(13)),
            "older_version" => Err(CanonicalError::UnsupportedVersion(11)),
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

#[test]
fn bms_load_keeps_a_chart_whose_endpoints_conflict() {
    // At 150 BPM, 1000/2001 and 1001/2003 of a measure floor to one µs at
    // two positions: the target loads it and its oracle agrees.
    let line = |slots: usize, index: usize| -> String {
        (0..slots)
            .map(|i| if i == index { "01" } else { "00" })
            .collect()
    };
    let text = format!(
        "#BPM 150\n#WAV01 a.wav\n#00011:{}\n#00012:{}\n",
        line(2001, 1000),
        line(2003, 1001)
    );
    let loaded = bms_load(text.as_bytes()).unwrap();
    assert!(matches!(
        loaded.chart.validate_presentation(),
        Err(bms_model::PresentationError::EndpointConflict { .. })
    ));
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
            "header_selection",
            "header_decision",
            "header_result",
            "size",
            "syntax",
            "missing",
            "configured_fails",
            "bar_group_limit",
            "note_parts_null",
            "note_parts_unsized",
            "note_parts_broken",
            "images",
            "numbers",
            "text",
            "bitmap_text",
            "tracks",
            "ops",
            "offsets",
            "choices_default",
            "lanes",
            "bar_groups",
            "paints",
            "followers",
            "trims",
            "judge",
            "gauge",
            "rates",
            "score figures",
            "readouts",
            "selectors",
            "note_fallbacks",
            "digit_offsets",
            "digit_offset_fraction",
            "digit_offset_unselected",
            "digit_offset_stretch",
            "digit_offset_rows_limit",
            "grade_anchors",
            "grade_anchor_trimmed",
            "grade_anchor_turning",
            "gauge_max_still",
            "gauge_max_flicker",
            "gauge_shared",
            "gauge_over_budget",
            "gauge_too_many_parts",
            "gauge_fractional_parts",
            "gauge_node_count",
            "lanes_overlap",
            "lanes_scratch_overlap",
            "result",
            "selection",
        ],
    );
    let imports: Vec<&str> = seeds::skin_imports().iter().map(|(n, _)| *n).collect();
    for (name, data) in seeds {
        if imports.contains(&name) {
            continue;
        }
        if name == "bar_group_limit" {
            // Within the runner's 64 KiB inputs (docs/setup.md "Fuzzing").
            assert!(data.len() <= 64 * 1_024, "{name}: {} bytes", data.len());
            // The header resolves; the import stops at the groups' count.
            let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                matches!(
                    imported,
                    Err(ImportError::Limit {
                        limit: skin_beatoraja::ImportLimit::BarGroups,
                        ..
                    })
                ),
                "{name}: {:?}",
                imported.map(|i| i.report)
            );
            continue;
        }
        if let Some(why) = note_part_refusal(name) {
            // The header resolves; the lanes refuse before any part is
            // admitted or generated, never with fallback paint.
            let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            match imported {
                Err(ImportError::Critical {
                    part: skin_beatoraja::CriticalPart::Lanes,
                    why: got,
                    ..
                }) => match why {
                    Some(want) => assert_eq!(got, want, "{name}"),
                    None => assert!(
                        !matches!(
                            got,
                            NoteKind::NotePart { .. } | NoteKind::NotePartFallback { .. }
                        ),
                        "{name}: {got:?}"
                    ),
                },
                other => panic!("{name}: {:?}", other.map(|i| i.report)),
            }
            continue;
        }
        if let Some(want) = digit_offset_refusal(name) {
            // The header resolves; the import refuses the table.
            assert!(data.len() <= 64 * 1_024, "{name}: {} bytes", data.len());
            let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            match imported {
                Err(e) => assert!(want(&e), "{name}: {e:?}"),
                Ok(i) => panic!("{name}: {:?}", i.report),
            }
            continue;
        }
        if let Some(want) = gauge_refusal(name) {
            // The header resolves; the gauge refuses as built.
            let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            match imported {
                Err(e) => assert!(want(&e), "{name}: {e:?}"),
                Ok(i) => panic!("{name}: {:?}", i.report),
            }
            continue;
        }
        if let Some(want) = grade_anchor_refusal(name) {
            // A trimmed or branch-changing grade stretch stays a located
            // critical refusal.
            let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
            match imported {
                Err(ImportError::Critical {
                    part: skin_beatoraja::CriticalPart::Judge,
                    why,
                    ..
                }) => assert_eq!(why, NoteKind::Stretch(want), "{name}"),
                other => panic!("{name}: {:?}", other.map(|i| i.report)),
            }
            continue;
        }
        let (result, imported) = match skin_json(&data) {
            Ok((h, i)) => (Ok(h), Some(i)),
            Err(e) => (Err(e), None),
        };
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
                matches!(
                    result,
                    Err(ImportError::UnsupportedKind {
                        at: Some(_),
                        ty: Some(1)
                    })
                ),
                "{name}: {result:?}"
            ),
            "header_selection" => {
                // Imported into its own slot: a header alone has no song
                // list, an essential part.
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.kind, ScreenKind::Selection, "{name}");
                match imported {
                    Some(Err(ImportError::Essential {
                        part: skin_beatoraja::EssentialPart::SongList,
                        why: None,
                        ..
                    })) => {}
                    other => panic!("{name}: {:?}", other.map(|r| r.map(|i| i.report))),
                }
            }
            "header_result" => {
                // Imported into its own slot: a header alone has no EX
                // score, an essential part.
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.kind, ScreenKind::Result, "{name}");
                match imported {
                    Some(Err(ImportError::Essential {
                        part: skin_beatoraja::EssentialPart::ResultScore,
                        why: None,
                        ..
                    })) => {}
                    other => panic!("{name}: {:?}", other.map(|r| r.map(|i| i.report))),
                }
            }
            "header_decision" => {
                // Imported into its own slot: with no essential part, an
                // empty decision skin imports.
                let h = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.kind, ScreenKind::Decision, "{name}");
                match imported {
                    Some(Ok(i)) => assert_eq!(i.skin.kind(), ScreenKind::Decision, "{name}"),
                    other => panic!("{name}: {:?}", other.map(|r| r.map(|i| i.report))),
                }
            }
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

/// The refusal a malformed note part seed meets: its reason, or `None`
/// for a broken image that refuses as any broken part does.
fn note_part_refusal(name: &str) -> Option<Option<NoteKind>> {
    use skin_beatoraja::PartWhy;
    let part = |field, why| Some(Some(NoteKind::NotePart { field, why }));
    match name {
        "note_parts_null" => part("mine", PartWhy::NullEntry { index: 7 }),
        "note_parts_unsized" => part("note", PartWhy::MissingSize { lane: 2 }),
        "note_parts_broken" => Some(None),
        _ => None,
    }
}

/// The refusal a gauge seed meets: a flicker's tip past the widget budget
/// at its destination, parts past the most segments, a fractional count
/// and an unsupported node count.
fn gauge_refusal(name: &str) -> Option<fn(&ImportError) -> bool> {
    use skin_beatoraja::{CriticalPart, ImportLimit};
    match name {
        "gauge_over_budget" => Some(|e| {
            matches!(
                e,
                ImportError::Limit {
                    limit: ImportLimit::WidgetQuads,
                    at: Some(_),
                }
            )
        }),
        "gauge_too_many_parts" => Some(|e| {
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::Gauge,
                    why: NoteKind::UnsupportedField { field: "parts" },
                    ..
                }
            )
        }),
        "gauge_fractional_parts" => Some(|e| {
            matches!(
                e,
                ImportError::Type {
                    field: "parts",
                    expected: "an integer",
                    ..
                }
            )
        }),
        "gauge_node_count" => Some(|e| {
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::Gauge,
                    why: NoteKind::UnsupportedField { field: "nodes" },
                    ..
                }
            )
        }),
        // The scratch stays wholly beside the keys, which may overlap.
        "lanes_scratch_overlap" => Some(|e| {
            matches!(
                e,
                ImportError::Model {
                    at: Some(_),
                    source: skin_model::LoadError::Lanes(skin_model::LanesError::ScratchNotAtSide),
                }
            )
        }),
        _ => None,
    }
}

/// The stretch mode a refused grade anchor seed names.
fn grade_anchor_refusal(name: &str) -> Option<i32> {
    match name {
        "grade_anchor_trimmed" => Some(3),
        "grade_anchor_turning" => Some(1),
        _ => None,
    }
}

/// The refusal a malformed digit offset seed meets: a row read exactly,
/// a choice that must select a row, a row breaking its aspect stretch,
/// and the resolved rows' cap.
fn digit_offset_refusal(name: &str) -> Option<fn(&ImportError) -> bool> {
    use skin_beatoraja::{CriticalPart, InputLimit};
    match name {
        "digit_offset_fraction" => Some(|e| {
            matches!(
                e,
                ImportError::Type {
                    field: "y",
                    expected: "an integer",
                    ..
                }
            )
        }),
        "digit_offset_unselected" => Some(|e| {
            matches!(
                e,
                ImportError::Type {
                    field: "offset",
                    expected: "a row",
                    ..
                }
            )
        }),
        "digit_offset_stretch" => Some(|e| {
            matches!(
                e,
                ImportError::Critical {
                    part: CriticalPart::ExScore,
                    why: NoteKind::Stretch(1),
                    ..
                }
            )
        }),
        "digit_offset_rows_limit" => Some(|e| {
            matches!(
                e,
                ImportError::Input {
                    limit: InputLimit::DigitOffsets,
                    at: Some(_),
                }
            )
        }),
        _ => None,
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

/// The quads of the `selection` seed's frames ([`crate::draw`]), two for
/// each cursor: on the chart, the bars of its slot and the folder's below,
/// the chart's lamp, the panel image and the slider (its level's
/// difficulty has no style, so it draws nothing); on the folder, the three
/// bars, the chart's lamp and the two; on the "all charts" row, two bars
/// and the two.
const SELECTION_DRAWN: [usize; 6] = [5, 5, 6, 6, 4, 4];

/// The quads of the `result` seed's frames ([`crate::draw`]), two for each
/// view. Ready: the EX score's four digits, the mean's, the level's two,
/// the status image under option 90, the clear type's image and the lane
/// image; the BP difference is hidden (no previous BP), so its number
/// draws nothing; the grade image (AA, option 301) on the fade-out only.
/// With no result: the level's two digits and the lane image alone. The
/// mean, −1.49 ms, shows its whole ms truncated toward 0: one digit.
///
/// The graphs: the views hold no gauge history, timelines, deviations or
/// judge areas, so the gauge graph and the judge graph draw nothing, and
/// the ready view's distribution draws its ticks and lines alone. Its
/// trimmed covering stretch (3) shows 30 of its 301 columns, 10 pixels
/// each, around the centre: the ticks of columns 140, 150 and 160, and
/// the average (149) and ±σ lines (137, 161) in two pieces each.
const RESULT_DRAWN: [usize; 4] = [
    4 + 1 + 2 + 3 + 3 + 3 * 2,
    4 + 1 + 2 + 3 + 1 + 3 + 3 * 2,
    3,
    3,
];

fn kinds(i: &Imported) -> Vec<NoteKind> {
    i.report.notes.iter().map(|n| n.kind).collect()
}

#[test]
fn skin_import_seeds_import_and_draw_as_built() {
    for (name, data) in seeds::skin_imports() {
        let (_, imported) = skin_json(&data).unwrap_or_else(|e| panic!("{name}: {e}"));
        let i = imported.unwrap_or_else(|e| panic!("{name}: {e}"));
        let u = i.report.usage;
        // A play skin's quads at 0, 50, 500 and 5,000 ms, each with a
        // perfect judgment; a result skin's per [`draw`].
        let drawn = draw(&i, &[]);
        match name {
            "result" => {
                // Every graph imports, with no note.
                assert_eq!(kinds(&i), [], "{name}");
                assert_eq!(i.skin.kind(), ScreenKind::Result, "{name}");
                assert_eq!(drawn, RESULT_DRAWN, "{name}");
            }
            "selection" => {
                // The list, the EX score, the two ratio bars and the panel
                // image; three placements of a bar, a lamp, the level's
                // two cells and three labels each.
                assert_eq!(u.elements, 5, "{name}");
                assert_eq!(u.placements, 3, "{name}");
                assert_eq!(u.widget_quads, 3 * (2 + 1 + 2 + 3) + 4, "{name}");
                assert_eq!(kinds(&i), [], "{name}");
                assert_eq!(i.skin.kind(), ScreenKind::Selection, "{name}");
                // The chart's EX score is unknown (no best), its rate bar
                // likewise.
                assert_eq!(drawn, SELECTION_DRAWN, "{name}");
            }
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
                // Both gauge conditions hold at 20 %.
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "offsets" => {
                // Offset 4 follows SUDDEN+, at rest without lanes.
                let motion = NoteKind::Deviation(skin_beatoraja::DeviationId::D9(
                    skin_beatoraja::LaneApprox::CoverMotion,
                ));
                assert_eq!(kinds(&i), [motion], "{name}");
                assert_eq!(drawn, [2, 2, 2, 2], "{name}");
            }
            "choices_default" => {
                assert_eq!(i.report.files.len(), 1, "{name}");
                assert_eq!(i.report.files[0].rule, Rule::Default, "{name}");
                assert!(!i.report.files[0].ambiguous, "{name}");
                assert_eq!(drawn, [1, 1, 1, 1], "{name}");
            }
            "lanes" | "lanes_overlap" => {
                use skin_beatoraja::{DeviationId, LaneApprox};
                // The lanes, SUDDEN+'s paint, LIFT and HIDDEN+; the notes,
                // bar, SUDDEN+, LIFT and HIDDEN+ images over one file.
                assert_eq!((u.elements, u.keys, u.images), (4, 5, 5), "{name}");
                assert!(i.scratch.is_some(), "{name}");
                let d9 = |a| NoteKind::Deviation(DeviationId::D9(a));
                for a in [LaneApprox::BarLine, LaneApprox::CoverMotion] {
                    assert!(kinds(&i).contains(&d9(a)), "{name}: {a:?}");
                }

                // At 500 ms the eleven notes from 1 s to 1.1 s are on
                // screen, the last with its lower edge on the region's
                // top. SUDDEN+'s paint is drawn whole in every frame;
                // LIFT's and HIDDEN+'s at 0 lie at their lines and are not
                // drawn.
                assert_eq!(drawn, [1, 1, 12, 1], "{name}");
                let paint = i
                    .skin
                    .quad_capacity()
                    .checked_sub(skin_model::LANE_QUADS + skin_model::LANE_RESERVED);
                assert_eq!(paint, Some(3), "{name}");
            }
            "bar_groups" => {
                use skin_beatoraja::{DeviationId, LaneApprox};
                // Four groups charged with their keys, three kept, each
                // noted once; the follower and SUDDEN+'s paint followed,
                // and LIFT's and HIDDEN+'s paint moved, once each; the
                // undefined image dropped; the image's `len` and `ref`
                // unused.
                assert_eq!(u.bar_groups, 4, "{name}");
                assert_eq!(i.skin.bar_group_count(), 3, "{name}");
                assert_eq!((u.elements, u.keys, u.images), (4, 8, 6), "{name}");
                let d9 = |a| NoteKind::Deviation(DeviationId::D9(a));
                let k = kinds(&i);
                let count = |n: NoteKind| k.iter().filter(|&&m| m == n).count();
                assert_eq!(count(d9(LaneApprox::BarLine)), 3, "{name}: {k:?}");
                assert_eq!(count(d9(LaneApprox::CoverMotion)), 4, "{name}: {k:?}");
                assert_eq!(count(NoteKind::UndefinedObject), 1, "{name}: {k:?}");
                assert!(
                    !k.iter()
                        .any(|n| matches!(n, NoteKind::UnsupportedProperty { .. })),
                    "{name}: {k:?}"
                );
                // The draw's chart has no bars: as the lanes.
                assert_eq!(drawn, [1, 1, 12, 1], "{name}");
            }
            "paints" => {
                // The lanes, LIFT's and HIDDEN+'s paint, four paints and
                // the progress slider.
                assert_eq!(u.elements, 8, "{name}");
                assert_eq!(
                    i.skin.quad_capacity(),
                    7 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED,
                    "{name}"
                );
                // Each paint and the slider in every frame beside the
                // notes on screen.
                assert_eq!(drawn, [5, 5, 16, 5], "{name}");
            }
            "followers" => {
                use skin_beatoraja::{DeviationId, LaneApprox};
                // SUDDEN+'s paint, the image, the number and the grade
                // follow, and LIFT's and HIDDEN+'s paint move, each noted
                // once; the combo takes its offsets silently, and HIDDEN+
                // its repeated ones.
                let motion = NoteKind::Deviation(DeviationId::D9(LaneApprox::CoverMotion));
                let k = kinds(&i);
                assert_eq!(
                    k.iter().filter(|&&n| n == motion).count(),
                    6,
                    "{name}: {k:?}"
                );
                assert!(
                    !k.iter().any(|n| matches!(n, NoteKind::RuntimeOffset(_))),
                    "{name}: {k:?}"
                );
                assert_eq!(u.elements, 7, "{name}");
                // The lanes' frames, then the image, the number's two
                // digits, the grade and its combo of 1.
                assert_eq!(drawn, [1 + 5, 1 + 5, 12 + 5, 1 + 5], "{name}");
            }
            "trims" => {
                use skin_beatoraja::{DeviationId, LaneApprox};
                // The lanes, SUDDEN+'s paint and eight of LIFT's and
                // HIDDEN+'s, each moving with the covers; twelve keys and
                // one for the gate track; the fading HIDDEN+ noted.
                assert_eq!((u.elements, u.keys), (10, 13), "{name}");
                assert_eq!(
                    i.skin.quad_capacity(),
                    9 + skin_model::LANE_QUADS + skin_model::LANE_RESERVED,
                    "{name}"
                );
                let d9 = |a| NoteKind::Deviation(DeviationId::D9(a));
                let k = kinds(&i);
                let count = |n: NoteKind| k.iter().filter(|&&m| m == n).count();
                assert_eq!(count(d9(LaneApprox::HiddenDisable)), 1, "{name}: {k:?}");
                assert_eq!(count(d9(LaneApprox::CoverMotion)), 9, "{name}: {k:?}");
                // At rest SUDDEN+'s paint and three trimmed paints draw;
                // the rest lie at or past their lines.
                assert_eq!(drawn, [4, 4, 15, 4], "{name}");
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
            "rates" => {
                use skin_beatoraja::DeviationId;
                // Each rate number once D4, its conditions with it; the
                // sets selecting by gauge type and by EX score D4 too.
                assert_eq!(
                    kinds(&i),
                    [NoteKind::Deviation(DeviationId::D4); 8],
                    "{name}"
                );
                assert_eq!(u.elements, 9, "{name}");
                assert_eq!(u.choice_refs, 3, "{name}");
                // A normal gauge: the groove, not-hard and not-ex numbers
                // draw. EX 150 over one judged of 100 targets: 750 000 bp
                // now, its whole percent past 3 cells; 75.00 overall,
                // "00" twice. Each set has one image: gauge type 2, EX 150
                // and the unresolved value all select it.
                assert_eq!(drawn, [3 + 2 + 2 + 3; 4], "{name}");
            }
            "score figures" => {
                use skin_beatoraja::DeviationId;
                // The rates (122, 123, 183, 184) and the paced best's
                // difference (152) each D4.
                assert_eq!(
                    kinds(&i),
                    [NoteKind::Deviation(DeviationId::D4); 5],
                    "{name}"
                );
                assert_eq!(u.elements, 15, "{name}");
                // No best and no target (0, not `#RANDOM`), EX 150 over one
                // judged of 100 targets: bars 110 (0.75) and 111 (past 1,
                // drawn at 1) and the four at 0 as empty quads; 150, 151
                // and 121 "0"; 152 under 201 hidden (band AAA) and 153
                // under -225 hidden (rank D reached); 122 "0" on 351 and
                // 123 "00" on 352 (both reached at 0); 183 under 207
                // hidden, 184 "00" under 227 (rank F reached).
                assert_eq!(drawn, [2 + 4 + 3 + 1 + 2 + 2; 4], "{name}");
            }
            "readouts" => {
                use skin_beatoraja::{DeviationId, PropertyKind};
                // Each readout number D4, and none for the adjustment
                // condition, which reads the snapshot; 15 is no value property, so `ref` 71 shows; 131
                // resolves to nothing; 92 is unsupported.
                let mut k = kinds(&i);
                k.sort_by_key(|n| format!("{n:?}"));
                let mut want = vec![NoteKind::Deviation(DeviationId::D4); 16];
                want.push(NoteKind::Ignored { field: "value" });
                want.push(NoteKind::UnsupportedProperty {
                    kind: PropertyKind::Integer,
                    id: Some(92),
                });
                want.sort_by_key(|n| format!("{n:?}"));
                assert_eq!(k, want, "{name}");
                assert_eq!(u.elements, 24, "{name}");
                // No tempo, hi-speed or duration: their numbers stay
                // hidden. Played: 0 min and "00" s; left: 0 min and 2 s;
                // four covers at 0; EX 150 by `ref`; the covers on, not
                // adjusting.
                let images = 1 + 1 + 1;
                assert_eq!(drawn, [1 + 2 + 1 + 1 + 4 + 3 + images; 4], "{name}");
            }
            "selectors" => {
                use skin_beatoraja::{DeviationId, PropertyKind};
                // Each selector but the perfect count and no property
                // D4; the group past its frames never drawn; each set's
                // undefined id ignored; 508-515, 1P keys past 7 and 2P, refused by
                // their id.
                let mut k = kinds(&i);
                k.sort_by_key(|n| format!("{n:?}"));
                let mut want = vec![NoteKind::Deviation(DeviationId::D4); 6];
                want.push(NoteKind::Track(skin_beatoraja::TrackWhy::NeverDrawn));
                want.extend([NoteKind::UndefinedObject; 5]);
                want.extend((508..=515).map(|id| NoteKind::UnsupportedProperty {
                    kind: PropertyKind::ImageIndex,
                    id: Some(id),
                }));
                want.sort_by_key(|n| format!("{n:?}"));
                assert_eq!(k, want, "{name}");
                // Three groups of 4 frames and the sets' three images; 2,
                // 2 and 4 group entries and 6 per set; the wider image's
                // own track in each set.
                assert_eq!(u.elements, 17, "{name}");
                assert_eq!((u.images, u.frames, u.files), (6, 15, 1), "{name}");
                assert_eq!(u.choice_refs, 2 + 2 + 4 + 5 * 6, "{name}");
                assert_eq!(i.skin.choice_ref_count(), 38, "{name}");
                assert_eq!(u.keys, 3 + 5 + 5, "{name}");
                // The gauge's type 34 ops twice, a switch 4 thrice, the
                // rest 1.
                assert_eq!(u.ops, 2 * 34 + 3 * 4 + 3, "{name}");
                assert_eq!(u.widget_quads, 0, "{name}");
                // A normal gauge (2) is past the groups' two entries and
                // selects the set's hole; EX 150 is past the sets,
                // selecting `a`, and the perfect count 1 selects `b`; every
                // switch is on.
                assert_eq!(drawn, [3 + 4; 4], "{name}");
            }
            "note_fallbacks" => {
                use skin_beatoraja::{NoteFallback, Severity};
                // Eight unresolved taps and tails and eight empty mine
                // ids: 24 parts on three generated images beside `p`,
                // no file but `p`'s; the active body aliases selected.
                assert_eq!(u.note_fallbacks, 24, "{name}");
                assert_eq!((u.images, u.frames, u.files), (4, 4, 1), "{name}");
                let fallbacks: Vec<(&str, NoteFallback)> = i
                    .report
                    .notes
                    .iter()
                    .filter(|n| n.severity == Severity::Fallback)
                    .filter_map(|n| match n.kind {
                        NoteKind::NotePartFallback { field, paint, .. } => Some((field, paint)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(fallbacks.len(), 24, "{name}");
                for (field, paint) in [
                    ("note", NoteFallback::Tap),
                    ("lnend", NoteFallback::Long),
                    ("mine", NoteFallback::Mine),
                ] {
                    let n = fallbacks.iter().filter(|&&f| f == (field, paint)).count();
                    assert_eq!(n, 8, "{name}: {field}");
                }
                // The 100-tap chart: at 500 ms the eleven taps from 1 s to
                // 1.1 s are on screen, in white.
                assert_eq!(drawn, [0, 0, 11, 0], "{name}");
            }
            "digit_offsets" => {
                // Four rows of the EX score's, a choice's and an included
                // file's; one of the perfect count's; the combo's three,
                // its trailing zero row dropped.
                assert_eq!(u.resolved_digit_offsets, 4 + 1 + 3, "{name}");
                assert_eq!(u.digit_offsets, 4 + 1 + 2, "{name}");
                assert_eq!(i.skin.digit_offset_count(), 7, "{name}");
                assert_eq!(kinds(&i), [], "{name}");
                // "150", "1", and the grade with its combo of 1.
                assert_eq!(drawn, [3 + 1 + 2; 4], "{name}");
            }
            "gauge_max_still" | "gauge_max_flicker" | "gauge_shared" => {
                // The whole widget budget, one D10 note per destination,
                // the nodes' four images whatever the parts; every quad
                // drawn on each frame at 20 %, the flicker's tip among
                // them.
                let elements = if name == "gauge_shared" { 2 } else { 1 };
                assert_eq!(u.widget_quads, 16_384, "{name}");
                assert_eq!(u.elements, elements, "{name}");
                assert_eq!((u.images, u.frames), (4, 4), "{name}");
                let d10 = NoteKind::Deviation(skin_beatoraja::DeviationId::D10);
                assert_eq!(kinds(&i), vec![d10; elements as usize], "{name}");
                assert_eq!(i.skin.quad_capacity(), 16_384, "{name}");
                assert_eq!(drawn, [16_384; 4], "{name}");
            }
            "grade_anchors" => {
                // PERFECT's two keys and the full-gauge slot's one kept
                // again as attachments; BAD has no combo.
                assert_eq!(u.grade_anchor_keys, 3, "{name}");
                assert_eq!(u.keys, 1 + 2 + 1 + 1 + 1 + 3, "{name}");
                let motion = NoteKind::Deviation(skin_beatoraja::DeviationId::D9(
                    skin_beatoraja::LaneApprox::CoverMotion,
                ));
                assert_eq!(kinds(&i), [motion], "{name}");
                // The grade and the "1" of its combo.
                assert_eq!(drawn, [2; 4], "{name}");
                // "12" on the destination's bottom-left at (600, 580):
                // cells from 10 − 10; the 20×20 paint at 630 shifted half
                // the 20 px baseline width. With the gauge full the
                // full-gauge grade's 80×80 paint from 530 keeps the combo
                // on 580 as well.
                assert_eq!(
                    crate::judged_rects(&i, false),
                    [
                        [600.0, 560.0, 10.0, 20.0],
                        [610.0, 560.0, 10.0, 20.0],
                        [620.0, 560.0, 20.0, 20.0]
                    ],
                    "{name}"
                );
                assert_eq!(
                    crate::judged_rects(&i, true),
                    [
                        [600.0, 560.0, 10.0, 20.0],
                        [610.0, 560.0, 10.0, 20.0],
                        [590.0, 530.0, 80.0, 80.0]
                    ],
                    "{name}"
                );
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
        "destination": [{"id": "g", "op": [232], "dst": [{"w": 4, "h": 4}]}]}"#;
    let (_, imported) = skin_json(data).expect("the header resolves");
    let i = imported.expect("the skin imports");
    assert!(i.report.usage.ops > 0);
    assert_eq!(i.skin.ops().total(), 0);
    assert_eq!(kinds(&i), [NoteKind::MissingAsset]);
}

#[test]
fn replaced_callbacks_past_the_report_s_cap_are_counted() {
    use skin_beatoraja::lua::Disposition;
    // 5 000 replaced callbacks: the report keeps 4 096 notes and counts
    // the rest as suppressed.
    let entry = "local skin = { type = 0, name = \"many\", w = 1920, h = 1080 }
if not skin_config then return skin end
local ms = require(\"main_state\")
skin.source = { { id = \"s\", path = \"strip.png\" } }
skin.image = { { id = \"i\", src = \"s\", x = 0, y = 0, w = 10, h = 10 } }
skin.destination = {}
for k = 1, 5000 do
  skin.destination[k] = { id = \"i\", draw = function() return ms.number(426) > 0 end, dst = { { w = 10, h = 10 } } }
end
return skin";
    let input = [
        &[seeds::LUA_AUTOPLAY_OFF, seeds::LUA_CLASS_NORMAL],
        entry.as_bytes(),
    ]
    .concat();
    let (_, imported) = skin_lua(&input).unwrap();
    let i = imported_lua(imported).unwrap();
    assert_eq!(i.callbacks.len(), 5_000);
    assert!(
        i.callbacks
            .iter()
            .all(|d| matches!(d, Disposition::Replaced(_)))
    );
    assert!(i.imported.report.suppressed > 0);
}

/// A `skin_lua` result that is not pending.
fn imported_lua(
    r: Result<skin_beatoraja::LuaOutcome, ImportError>,
) -> Result<skin_beatoraja::LuaImported, ImportError> {
    match r? {
        skin_beatoraja::LuaOutcome::Imported(i) => Ok(i),
        skin_beatoraja::LuaOutcome::Pending(p) => panic!("pending: {p:?}"),
    }
}

#[test]
fn skin_lua_seeds_import_or_fail_as_built() {
    use skin_beatoraja::{LuaLimit, LuaWhy};
    use skin_model::RootError;

    let seeds = seeds::skin_luas();
    assert_names(
        &seeds,
        &[
            "data_only",
            "config",
            "modules",
            "isolation",
            "callback_lowered",
            "callback_program",
            "callback_dropped",
            "facts",
            "result_facts",
            "callback_replaced",
            "callback_effect",
            "host_write",
            "loop",
            "protected_loop",
            "memory",
            "escape",
            "coroutine",
            "config_local",
            "syntax",
            "not_table",
            "lua_selection",
            "slot_scene",
            "slot_retype",
        ],
    );
    let lua = |e: &ImportError| match e {
        ImportError::Lua { why, .. } => Some(*why),
        _ => None,
    };
    for (name, data) in seeds {
        assert!(data.len() <= 64 * 1_024, "{name}: {} bytes", data.len());
        let result = skin_lua(&data);
        match name {
            "data_only" | "config" | "config_local" | "modules" | "isolation" | "facts"
            | "result_facts" => {
                let (h, imported) = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(h.options.len(), 1, "{name}");
                let i = imported_lua(imported).unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert!(i.imported.report.usage.elements >= 1, "{name}");
                if name == "result_facts" {
                    // 154 and 178 read from the view, the hidden one as
                    // `i32::MIN`.
                    use skin_beatoraja::{FactId, FactRead, FactValue};
                    let read = |n, v| FactRead {
                        fact: FactId::ResultNumber(n),
                        value: FactValue::Int(v),
                    };
                    assert_eq!(
                        i.facts.reads,
                        [read(154, 17), read(178, i32::MIN)],
                        "{name}"
                    );
                    assert_eq!(i.imported.skin.kind(), ScreenKind::Result, "{name}");
                }
                if name == "modules" {
                    assert_eq!(
                        i.dependencies,
                        ["s.luaskin", "m.lua", "parts/p.lua"],
                        "{name}"
                    );
                }
            }
            "callback_lowered" | "callback_program" | "callback_dropped" | "callback_replaced"
            | "callback_effect" => {
                use skin_beatoraja::lua::{Disposition, Unsupported};
                let (_, imported) = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                let i = imported_lua(imported).unwrap_or_else(|e| panic!("{name}: {e:?}"));
                let want: &[Disposition] = match name {
                    "callback_lowered" => &[Disposition::Lowered; 4],
                    "callback_program" => &[Disposition::Compiled],
                    // By callable: the draw's closure, then the `op`'s.
                    "callback_dropped" => &[
                        Disposition::Replaced(Unsupported::Construct(
                            "a draw after a program draw",
                        )),
                        Disposition::Compiled,
                    ],
                    // A decorative writer never lowers.
                    "callback_effect" => &[Disposition::Replaced(Unsupported::Writes)],
                    _ => &[Disposition::Replaced(Unsupported::Unmapped {
                        api: 1,
                        id: 426,
                    })],
                };
                assert_eq!(i.callbacks, want, "{name}");
            }
            "host_write" => {
                let (_, imported) = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                let e = imported_lua(imported).expect_err(name);
                assert_eq!(lua(&e), Some(LuaWhy::HostWrite), "{name}");
            }
            "lua_selection" | "slot_scene" | "slot_retype" => {
                let (h, imported) = result.unwrap_or_else(|e| panic!("{name}: {e}"));
                let e = imported_lua(imported).expect_err(name);
                match (name, &e) {
                    (
                        "lua_selection",
                        ImportError::Essential {
                            part: skin_beatoraja::EssentialPart::SongList,
                            why: None,
                            ..
                        },
                    ) => {
                        assert_eq!(h.kind, ScreenKind::Selection);
                    }
                    (
                        "slot_scene",
                        ImportError::Slot {
                            at: None,
                            slot: ScreenKind::Result,
                            found: ScreenKind::Play,
                        },
                    )
                    | (
                        "slot_retype",
                        ImportError::Slot {
                            at: Some(_),
                            slot: ScreenKind::Play,
                            found: ScreenKind::Result,
                        },
                    ) => {}
                    _ => panic!("{name}: {e:?}"),
                }
            }
            _ => {
                let e = result.expect_err(name);
                match name {
                    "loop" | "protected_loop" => assert_eq!(
                        lua(&e),
                        Some(LuaWhy::Limit(LuaLimit::Instructions)),
                        "{name}"
                    ),
                    "memory" => {
                        assert_eq!(lua(&e), Some(LuaWhy::Limit(LuaLimit::Memory)), "{name}");
                    }
                    "escape" => assert!(
                        matches!(
                            e,
                            ImportError::Read {
                                source: RootError::Underflow,
                                ..
                            }
                        ),
                        "{name}: {e:?}"
                    ),
                    "coroutine" | "syntax" | "not_table" => {
                        let want = match name {
                            "coroutine" => LuaWhy::Error,
                            "syntax" => LuaWhy::Syntax,
                            _ => LuaWhy::Returned,
                        };
                        assert_eq!(lua(&e), Some(want), "{name}");
                    }
                    _ => unreachable!("{name}"),
                }
            }
        }
    }
}

#[test]
fn skin_png_seeds_build_or_fail_as_built() {
    use skin_model::{AtlasError, ImageError};
    let seeds = seeds::skin_pngs();
    let banded = [
        "banded_grey1",
        "banded_indexed8",
        "banded_bad_text",
        "cancelled_early",
        "cancelled_late",
    ];
    assert_names(
        &seeds,
        &[
            "grey8",
            "grey16",
            "grey_alpha8",
            "rgb8",
            "rgb16_trns",
            "rgba8",
            "rgba16",
            "indexed8_trns",
            "grey1_trns",
            "bad_text",
            banded[0],
            banded[1],
            banded[2],
            banded[3],
            banded[4],
        ],
    );
    for (name, bytes) in seeds {
        // `skin_png` checks every texel of its cells and the second build.
        let o = skin_png(&bytes);
        assert_eq!(o.banded, banded.contains(&name), "{name}");
        let fails = name == "banded_bad_text";
        assert_eq!(
            o.built.is_err(),
            fails,
            "{name}: {:?}",
            o.built.as_ref().err()
        );
        if fails {
            assert!(
                matches!(o.built, Err(AtlasError::Image(ImageError::Png { .. }))),
                "{name}"
            );
        }
        assert_eq!(o.cancelled, name.starts_with("cancelled"), "{name}");
    }
}

#[test]
fn present_seeds_verify_or_fail_as_built() {
    let seeds = seeds::presents();
    assert_names(&seeds, &["accepted", "jump_back", "enum_read"]);
    for (name, data) in &seeds {
        let want = (*name == "accepted").then_some(2);
        assert_eq!(crate::present(data), want, "{name}");
    }
}

/// The program decoder's newer bytes: a conversion byte's high bit a
/// timer callback, op bytes from 240 the clock and a slot's load and
/// store, wrapping every three.
#[test]
fn present_decodes_timers_the_clock_and_slots() {
    use skin_model::present::{Conv, Insn};
    let data = [
        1, // one callback
        0, 4, 2, 0x80, 0, 255, 255, // start 0, length 4, a timer
        240, 0, 0, 0, // the clock into r0
        241, 1, 5, 0, // r1 <- slot 1
        242, 1, 6, 7, // slot 2 <- r1, domain 3
        243, 0, 0, 0, // the clock again
    ];
    let def = crate::present_def(&data);
    assert_eq!(def.callbacks.len(), 1);
    assert_eq!(def.callbacks[0].conv, Conv::Timer);
    assert_eq!(
        def.code,
        [
            Insn::StateUs { dst: 0 },
            Insn::LoadSlot { dst: 1, slot: 1 },
            Insn::StoreSlot {
                slot: 2,
                src: 1,
                domain: 3
            },
            Insn::StateUs { dst: 0 },
        ]
    );
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

/// The seeded readout frame reads every one of its bytes: a seed whose
/// ten ratios each have a nonzero numerator gives ten known, nonzero
/// durations, the source duration last at bytes 321 to 352; and a short
/// nonempty seed, read cyclically, still names every ratio.
#[test]
fn every_seeded_ratio_can_be_nonzero() {
    let mut seed = vec![1_u8; SEEDED_BYTES - 10 * 32];
    for i in 1..=10_u128 {
        seed.extend((1_000 * i).to_be_bytes());
        seed.extend(3_u128.to_be_bytes());
    }
    assert_eq!(seed.len(), SEEDED_BYTES);
    let ratios = |seed: &[u8]| {
        let (_, r) = seeded_readouts(seed);
        let mut all = r.duration_variants.to_vec();
        all.extend([r.native.duration, r.duration]);
        all
    };
    let read = ratios(&seed);
    for (i, d) in (1..=10_u128).zip(&read) {
        let d = d.expect("known");
        assert_eq!((d.numerator, d.denominator), (1_000 * i, 3), "ratio {i}");
    }
    // Ratios of the repeated bytes 0x0101…: each exactly 1, so known and
    // nonzero.
    assert!(
        ratios(&[1])
            .iter()
            .all(|d| d.is_some_and(|d| d.numerator > 0))
    );
}
