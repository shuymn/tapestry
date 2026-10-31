//! Personal bests (design.md "Personal Bests"): live plays and the bests
//! phase of a scan pass count saved records of self-made charts, by one
//! ingest, under the stored interpretation.

mod common;

use std::fs;

use bms_model::Speed;
use common::{
    CHART, Fixture, MS, RANDOM_CHART, contribution, hex, hits, key, played, record_of, score_of,
    sql, tables,
};
use library::{Best, BestsState, Ineligible, Ingested, LN_MODE, Score, Tuple};
use replay::{EndStatus, InitialGauge, PlayRecord, PlayType, Practice};

#[test]
fn completed_full_play_updates_the_best() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    // On time: every note PERFECT, a FULL COMBO; both tapestry gauges
    // cleared.
    let first = played(0);
    let (rel, _) = fx.save(&first, "1-1.tprl");
    let Ingested::Counted { previous, current } = fx.live(&lib, &rel, CHART, &first) else {
        panic!("not counted");
    };
    assert_eq!(previous, None);
    assert_eq!(
        current,
        Score {
            plays: 1,
            best: Some(Best {
                ex: 6,
                bp: 0,
                cleared: vec![0b11],
                full_combo: true,
            }),
        }
    );
    assert_eq!(score_of(&lib, &first), Some(current.clone()));
    // A worse play changes only the count; a better EX raises it.
    let mut worse = played(0);
    worse.events.truncate(3);
    worse.events.push(key(2, 3_010 * MS, 0));
    let (rel, _) = fx.save(&worse, "2-1.tprl");
    fx.live(&lib, &rel, CHART, &worse);
    let after = score_of(&lib, &first).unwrap();
    assert_eq!(after.plays, 2);
    assert_eq!(after.best, current.best);
}

#[test]
fn played_to_end_with_overflow_sets_no_best() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let mut record = played(0);
    record.status.incomplete.overflow = true;
    let (rel, _) = fx.save(&record, "1-1.tprl");
    let c = contribution(&fx.records, &rel, CHART, &record);
    assert_eq!(c.ineligible, Some(Ineligible::Incomplete));
    assert_eq!(c.best, None);
    lib.best_writer().unwrap().ingest(&c).unwrap();
    assert_eq!(
        score_of(&lib, &record),
        Some(Score {
            plays: 1,
            best: None
        })
    );
}

/// A round of section repeat of `record`'s chart from 1.5 s to 4.5 s.
fn section_round(mut record: PlayRecord) -> PlayRecord {
    let preset = rules::tapestry();
    record.play_type = PlayType::SectionRepeat(Practice {
        start: bms_model::TimeUs(1_500 * MS),
        end: bms_model::TimeUs(4_500 * MS),
        lead_in: bms_model::TimeUs(1_000 * MS),
        attempt: 1,
        initial_gauges: preset
            .gauges
            .iter()
            .map(|g| InitialGauge {
                id: g.id.clone(),
                value: g.start,
            })
            .collect(),
    });
    record
}

/// `CHART` played at 1.5×, its input on time in real time.
fn at_speed() -> PlayRecord {
    let dir = tempfile::tempdir().unwrap();
    let speed = Speed::FASTEST;
    let opts = bms_parser::LoadOptions {
        speed,
        ..bms_parser::LoadOptions::default()
    };
    let chart = bms_parser::load_bms(CHART.as_bytes(), dir.path(), &opts)
        .unwrap()
        .chart;
    let mut record = played(0);
    record.chart.identity = chart.identity;
    record.speed = speed;
    record.events = hits(0)
        .into_iter()
        .map(|mut e| {
            e.received = bms_model::TimeUs(e.received.0 * 2 / 3);
            e
        })
        .collect();
    record
}

#[test]
fn only_eligible_plays_set_bests() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let mut midway = played(0);
    midway.status.end = EndStatus::EndedMidway;
    midway.status.finalized = bms_model::TimeUs(2_500 * MS);
    let mut suspend = played(0);
    suspend.status.incomplete.suspend = true;
    let cases = [
        (midway, Ineligible::EndedMidway),
        (suspend, Ineligible::Incomplete),
        (section_round(played(0)), Ineligible::NotFullPlay),
        (at_speed(), Ineligible::NotNormalSpeed),
    ];
    for (n, (record, reason)) in cases.into_iter().enumerate() {
        let (rel, _) = fx.save(&record, &format!("{n}-1.tprl"));
        let c = contribution(&fx.records, &rel, CHART, &record);
        assert_eq!(c.ineligible, Some(reason), "{reason:?}");
        assert_eq!(c.best, None, "{reason:?}");
        let before = score_of(&lib, &record).map_or(0, |s| s.plays);
        lib.best_writer().unwrap().ingest(&c).unwrap();
        let after = score_of(&lib, &record).unwrap();
        assert_eq!((after.plays, after.best), (before + 1, None), "{reason:?}");
    }
    // Unverified: refused, so no row at all, and a catch-up does not
    // count it either.
    let mut unverified = played(0);
    unverified.versions.evaluator += 1;
    let other = Fixture::new();
    other.chart("a.bms", CHART);
    other.save(&unverified, "1-1.tprl");
    let lib = other.open();
    let report = other.pass(&lib);
    assert_eq!(report.bests.refused.evaluator, 1);
    assert_eq!(score_of(&lib, &unverified), None);
    assert!(library::ineligible(&unverified, None, true) == Some(Ineligible::Unverified));
}

#[test]
fn played_to_end_claim_without_completion_sets_no_best() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    // It claims to have ended played to the end, yet its watermark stops
    // before the last note: its evaluation does not complete.
    let mut record = played(0);
    record.status.finalized = bms_model::TimeUs(3_000 * MS);
    let (rel, _) = fx.save(&record, "1-1.tprl");
    let c = contribution(&fx.records, &rel, CHART, &record);
    assert_eq!(c.ineligible, Some(Ineligible::EndMismatch));
    // The rebuild agrees.
    lib.request_bests_pass().unwrap();
    fx.pass(&lib);
    assert_eq!(
        score_of(&lib, &record),
        Some(Score {
            plays: 1,
            best: None
        })
    );
}

/// Seeds whose draws give `RANDOM_CHART` one tap on key 3 and two.
fn random_seeds() -> (u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let notes = |seed| {
        let opts = bms_parser::LoadOptions {
            seed,
            ..bms_parser::LoadOptions::default()
        };
        let c = bms_parser::load_bms(RANDOM_CHART.as_bytes(), dir.path(), &opts)
            .unwrap()
            .chart;
        c.lanes.iter().map(Vec::len).sum::<usize>()
    };
    let one = (0..64).find(|&s| notes(s) == 3).unwrap();
    let two = (0..64).find(|&s| notes(s) == 4).unwrap();
    (one, two)
}

fn random_plays() -> [PlayRecord; 2] {
    let (one, two) = random_seeds();
    let press = |at| [key(1, at, 1), key(1, at + 50 * MS, 0)];
    let mut events = Vec::new();
    events.extend(press(2_000 * MS));
    events.extend(press(4_000 * MS));
    [
        record_of(RANDOM_CHART, one, events.clone()),
        record_of(RANDOM_CHART, two, events),
    ]
}

#[test]
fn random_charts_get_no_best_and_still_save() {
    let fx = Fixture::new();
    fx.chart("r.bms", RANDOM_CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let plays = random_plays();
    assert_ne!(
        plays[0].chart.identity.resolved_hash,
        plays[1].chart.identity.resolved_hash
    );
    for (n, record) in plays.iter().enumerate() {
        let (rel, _) = fx.save(record, &format!("{n}-1.tprl"));
        let c = contribution(&fx.records, &rel, RANDOM_CHART, record);
        assert_eq!(c.ineligible, Some(Ineligible::RandomChart));
        lib.best_writer().unwrap().ingest(&c).unwrap();
    }
    let none = Score {
        plays: 2,
        best: None,
    };
    assert_eq!(score_of(&lib, &plays[0]), Some(none.clone()));
    // After invalidation, the pass counts both again, with no best.
    drop(lib);
    sql(&fx.db(), "UPDATE meta SET bests = bests + 1");
    let lib = fx.open();
    assert!(matches!(lib.bests_state(), BestsState::Invalidated { .. }));
    assert_eq!(score_of(&lib, &plays[0]), None);
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 2);
    assert_eq!(score_of(&lib, &plays[0]), Some(none));
}

#[test]
fn random_results_stripped_from_the_record_still_no_best() {
    let fx = Fixture::new();
    fx.chart("r.bms", RANDOM_CHART);
    let [mut record, _] = random_plays();
    record.chart.identity.random_results.clear();
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 1, "{report:?}");
    assert_eq!(
        score_of(&lib, &record),
        Some(Score {
            plays: 1,
            best: None
        })
    );
}

#[test]
fn live_and_rebuild_values_agree() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    // 30 ms late: GREATs, still a FULL COMBO.
    let record = played(30);
    let (rel, _) = fx.save(&record, "1-1.tprl");
    fx.live(&lib, &rel, CHART, &record);
    let live = score_of(&lib, &record).unwrap();
    drop(lib);
    sql(&fx.db(), "UPDATE meta SET bests = bests + 1");
    let lib = fx.open();
    fx.pass(&lib);
    let rebuilt = score_of(&lib, &record).unwrap();
    assert_eq!(rebuilt, live);
    assert!(live.best.as_ref().unwrap().ex < 6);
}

#[test]
fn version_change_invalidates_bests_without_schema_change() {
    for field in ["replay_format", "canonical", "evaluator", "loader", "bests"] {
        let fx = Fixture::new();
        fx.chart("a.bms", CHART);
        let record = played(0);
        fx.save(&record, "1-1.tprl");
        let lib = fx.open();
        fx.pass(&lib);
        let counted = score_of(&lib, &record).unwrap();
        assert_eq!(counted.plays, 1);
        drop(lib);
        sql(&fx.db(), &format!("UPDATE meta SET {field} = {field} + 1"));
        let (lib, opened) = library::Library::open(&fx.db()).unwrap();
        assert_eq!(opened, library::Opened::Current, "{field}");
        let BestsState::Invalidated { old: Some(old) } = lib.bests_state() else {
            panic!("{field}: {:?}", lib.bests_state());
        };
        assert_ne!(old, Tuple::current(), "{field}");
        // Both tables empty before any scan, and a catch-up pending.
        assert_eq!(tables(&fx.db()), (Vec::new(), Vec::new()), "{field}");
        assert!(lib.bests_pending().unwrap());
        fx.pass(&lib);
        assert_eq!(score_of(&lib, &record), Some(counted), "{field}");
        assert!(!lib.bests_pending().unwrap());
        drop(lib);
        let (lib, _) = library::Library::open(&fx.db()).unwrap();
        assert_eq!(lib.bests_state(), BestsState::Current, "{field}");
    }
}

/// `record` encoded with zstd at `level`.
fn compressed(record: &PlayRecord, level: i32) -> Vec<u8> {
    let body = replay::body_bytes(record);
    let mut out = replay::MAGIC.to_vec();
    out.extend_from_slice(&replay::FORMAT_VERSION.to_le_bytes());
    out.push(1);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&zstd::bulk::compress(&body, level).unwrap());
    out
}

#[test]
fn ingest_is_idempotent() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let record = played(0);
    let plays = |lib: &library::Library| score_of(lib, &record).unwrap().plays;
    // Live twice.
    let (rel, path) = fx.save(&record, "1-1.tprl");
    fx.live(&lib, &rel, CHART, &record);
    assert_eq!(
        fx.live(&lib, &rel, CHART, &record),
        Ingested::AlreadyCounted
    );
    assert_eq!(plays(&lib), 1);
    // Live, then a catch-up.
    lib.request_bests_pass().unwrap();
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 0);
    assert_eq!(plays(&lib), 1);
    // Rewritten at its path, compressed otherwise: the same play.
    let other = compressed(&record, 1);
    assert_ne!(other, fs::read(&path).unwrap());
    fs::write(&path, other).unwrap();
    lib.request_bests_pass().unwrap();
    let report = fx.pass(&lib);
    assert_eq!((report.bests.counted, report.bests.already), (0, 1));
    assert_eq!(plays(&lib), 1);
    // An identical play saved as a second file counts.
    fx.save(&record, "2-2.tprl");
    lib.request_bests_pass().unwrap();
    fx.pass(&lib);
    assert_eq!(plays(&lib), 2);
    // A byte copy under another name counts again, as documented.
    fs::copy(&path, path.with_file_name("3-3.tprl")).unwrap();
    lib.request_bests_pass().unwrap();
    fx.pass(&lib);
    assert_eq!(plays(&lib), 3);
    // Another play written at a counted path is another play.
    let late = played(30);
    fx.save(&late, "1-1.tprl");
    lib.request_bests_pass().unwrap();
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 1);
    assert_eq!(plays(&lib), 4);
}

#[test]
fn rebuild_from_records_skips_refused() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    // Resolves to another chart than the record's.
    let edited = "#BPM 120\n#LNTYPE 1\n#00111:01\n#00211:0101\n#00152:0101\n";
    let valid = played(0);
    let (_, truncated) = fx.save(&valid, "1-1.tprl");
    let bytes = fs::read(&truncated).unwrap();
    fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    let mut evaluator = played(0);
    evaluator.versions.evaluator += 1;
    fx.save(&evaluator, "2-1.tprl");
    let missing = record_of("#BPM 120\n#00111:01\n#00211:01\n#00311:01\n", 0, hits(0));
    fx.save(&missing, "1-1.tprl");
    let mut resolved = played(0);
    let dir = tempfile::tempdir().unwrap();
    resolved.chart.identity.resolved_hash = bms_parser::load_bms(
        edited.as_bytes(),
        dir.path(),
        &bms_parser::LoadOptions::default(),
    )
    .unwrap()
    .chart
    .identity
    .resolved_hash;
    fx.save(&resolved, "3-1.tprl");
    fx.save(&valid, "4-1.tprl");
    let lib = fx.open();
    let report = fx.pass(&lib).bests;
    let r = report.refused;
    assert_eq!(
        (r.decode, r.evaluator, r.chart_not_in_library, r.resolved),
        (1, 1, 1, 1),
        "{report:?}"
    );
    assert_eq!(r.total(), 4);
    assert_eq!(report.counted, 1);
    assert!(report.enumeration_complete);
    assert_eq!(score_of(&lib, &valid).unwrap().plays, 1);
    let (counted, _) = tables(&fx.db());
    assert_eq!(counted.len(), 1);
    assert_eq!(
        counted[0].0,
        format!("{}/4-1.tprl", hex(&valid.chart.identity.sha256))
    );
    // The next catch-up skips the terminal refusals by their stamps and
    // retries the record waiting for its chart.
    lib.request_bests_pass().unwrap();
    let again = fx.pass(&lib).bests;
    assert_eq!(again.skipped_refused, 3, "{again:?}");
    assert_eq!(again.refused.chart_not_in_library, 1);
    assert_eq!(again.refused.total(), 1);
    assert_eq!(again.skipped_unchanged, 1);
    assert_eq!(again.counted, 0);
}

#[test]
fn full_combo_needs_no_combo_break_including_empty_poor() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let fc = |record: &PlayRecord, name: &str| {
        let (rel, _) = fx.save(record, name);
        contribution(&fx.records, &rel, CHART, record)
            .best
            .unwrap()
            .full_combo
    };
    assert!(fc(&played(0), "1-1.tprl"));
    // A press with no note near: an empty POOR, which breaks the combo
    // under the tapestry preset.
    let mut empty = played(0);
    empty.events.insert(0, key(3, 500 * MS, 1));
    empty.events.insert(1, key(3, 550 * MS, 0));
    let preset = rules::tapestry();
    assert_eq!(fc(&empty, "2-1.tprl"), !preset.empty_poor.breaks_combo);
    // A missed note breaks it.
    let mut missed = played(0);
    missed.events.retain(|e| e.received.0 < 3_500 * MS);
    assert!(!fc(&missed, "3-1.tprl"));
}

#[test]
fn catch_up_from_current_counts_new_records() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    assert!(!lib.bests_pending().unwrap());
    drop(lib);
    // Saved while no library was open (`tapestry play`).
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    assert_eq!(lib.bests_state(), BestsState::Current);
    // The launch's request makes the pass count it; the charts are
    // unchanged, so nothing else would.
    lib.request_bests_pass().unwrap();
    let report = fx.pass(&lib);
    assert_eq!(report.read, 0);
    assert_eq!(report.bests.counted, 1);
    assert_eq!(score_of(&lib, &record).unwrap().plays, 1);
}

#[test]
fn catch_up_from_pending_resumes() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    lib.request_bests_pass().unwrap();
    // The run ends before any pass: the request stays pending at the next
    // opening, which keeps it.
    drop(lib);
    let lib = fx.open();
    assert!(lib.bests_pending().unwrap());
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 1);
    assert!(!lib.bests_pending().unwrap());
}

#[test]
fn missing_chart_is_retried_after_it_is_scanned() {
    let fx = Fixture::new();
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    let report = fx.pass(&lib).bests;
    assert_eq!(report.refused.chart_not_in_library, 1);
    assert!(!lib.bests_pending().unwrap());
    // The chart is added: the pass that reads it retries the record
    // waiting for it, with no catch-up pending.
    fx.chart("a.bms", CHART);
    let report = fx.pass(&lib);
    assert_eq!(report.read, 1);
    assert!(!report.bests.catch_up);
    assert_eq!((report.bests.retried, report.bests.counted), (1, 1));
    assert_eq!(score_of(&lib, &record).unwrap().plays, 1);
    // A later pass reading no chart retries nothing.
    let report = fx.pass(&lib).bests;
    assert_eq!((report.retried, report.counted), (0, 0));
}

#[test]
fn score_upsert_failure_rolls_back_both_tables() {
    for (trigger, survives) in [
        // Every upsert fails: the first item's.
        ("", 0),
        // The third item's upsert fails, after two were counted in the
        // same transaction.
        ("WHEN (SELECT count(*) FROM counted) >= 3", 0),
    ] {
        let fx = Fixture::new();
        fx.chart("a.bms", CHART);
        for n in 0..5 {
            fx.save(&played(n * 5), &format!("{n}-1.tprl"));
        }
        let lib = fx.open();
        sql(
            &fx.db(),
            &format!(
                "CREATE TRIGGER fail BEFORE INSERT ON scores {trigger}
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;"
            ),
        );
        let ended = lib
            .scan(std::slice::from_ref(&fx.charts), fx.options())
            .unwrap()
            .finish();
        assert!(
            matches!(ended, Err(library::ScanError::Database(_))),
            "{ended:?}"
        );
        drop(lib);
        let lib = fx.reopen();
        let (counted, scores) = tables(&fx.db());
        assert_eq!((counted.len(), scores.len()), (survives, survives));
        assert!(lib.bests_pending().unwrap());
        // Without the fault, the next pass counts all of them.
        sql(&fx.db(), "DROP TRIGGER fail;");
        fx.pass(&lib);
        assert_eq!(score_of(&lib, &played(0)).unwrap().plays, 5);
    }
}

#[test]
fn a_changed_stamp_reads_the_record_again() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    let (_, path) = fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    fx.pass(&lib);
    let again = |lib: &library::Library| {
        lib.request_bests_pass().unwrap();
        fx.pass(lib).bests
    };
    let r = again(&lib);
    assert_eq!((r.skipped_unchanged, r.already), (1, 0));
    // Another modification time: read again, the same play.
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000))
        .unwrap();
    drop(file);
    let r = again(&lib);
    assert_eq!((r.skipped_unchanged, r.already), (0, 1));
    // Its stamp was refreshed: skipped from then on.
    let r = again(&lib);
    assert_eq!((r.skipped_unchanged, r.already), (1, 0));
    // Another size (recompressed): read again, the same play.
    fs::write(&path, compressed(&record, 3)).unwrap();
    let r = again(&lib);
    assert_eq!((r.skipped_unchanged, r.already), (0, 1));
    let r = again(&lib);
    assert_eq!((r.skipped_unchanged, r.already), (1, 0));
    assert_eq!(score_of(&lib, &record).unwrap().plays, 1);
}

#[test]
fn bests_are_listed_by_chart_for_a_preset() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    fx.pass(&lib);
    let all = lib.bests(&record.preset.sha256, LN_MODE).unwrap();
    assert_eq!(all.len(), 1);
    assert!(all.contains_key(&record.chart.identity.sha256));
    assert!(lib.bests(&[0; 32], LN_MODE).unwrap().is_empty());
    assert!(
        lib.bests(&record.preset.sha256, LN_MODE + 1)
            .unwrap()
            .is_empty()
    );
}

/// A trigger refusing every change of `requested`.
const REFUSE_REQUESTS: &str = "CREATE TRIGGER refuse BEFORE UPDATE ON meta
     WHEN new.requested != old.requested BEGIN SELECT RAISE(ABORT, 'injected'); END;";

#[test]
fn a_request_failing_while_idle_is_held_until_written() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    sql(&fx.db(), REFUSE_REQUESTS);
    assert!(lib.request_bests_pass().is_err());
    assert!(lib.bests_pending().unwrap());
    // Still unwritable: the phase runs on the held request and keeps it.
    let report = fx.pass(&lib).bests;
    assert!(report.catch_up);
    assert_eq!(report.counted, 1);
    assert!(lib.bests_pending().unwrap());
    // Writable again: the next phase writes it, completes, forgets it.
    sql(&fx.db(), "DROP TRIGGER refuse;");
    assert!(fx.pass(&lib).bests.catch_up);
    assert!(!lib.bests_pending().unwrap());
    assert_eq!(score_of(&lib, &record).unwrap().plays, 1);
}

#[test]
fn a_failure_reading_the_requests_ends_the_pass_pending() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    sql(&fx.db(), "ALTER TABLE meta RENAME TO hidden;");
    let ended = lib
        .scan(std::slice::from_ref(&fx.charts), fx.options())
        .unwrap()
        .finish();
    assert!(
        matches!(ended, Err(library::ScanError::Database(_))),
        "{ended:?}"
    );
    sql(&fx.db(), "ALTER TABLE hidden RENAME TO meta;");
    assert!(lib.bests_pending().unwrap());
    assert_eq!(score_of(&lib, &record), None);
    assert_eq!(fx.pass(&lib).bests.counted, 1);
    assert!(!lib.bests_pending().unwrap());
}

#[test]
fn a_failure_writing_completion_rolls_back_its_batch() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    sql(
        &fx.db(),
        "CREATE TRIGGER refuse BEFORE UPDATE ON meta WHEN new.completed != old.completed
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    let ended = lib
        .scan(std::slice::from_ref(&fx.charts), fx.options())
        .unwrap()
        .finish();
    assert!(
        matches!(ended, Err(library::ScanError::Database(_))),
        "{ended:?}"
    );
    // The last batch went with it.
    assert_eq!(tables(&fx.db()), (Vec::new(), Vec::new()));
    assert!(lib.bests_pending().unwrap());
    sql(&fx.db(), "DROP TRIGGER refuse;");
    assert_eq!(fx.pass(&lib).bests.counted, 1);
    assert!(!lib.bests_pending().unwrap());
}
