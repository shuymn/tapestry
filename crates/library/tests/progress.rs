//! The best play's progress (design.md "Personal Bests"): the judgment of
//! each judged target, kept beside the breakdown of the play that set the
//! best EX score, written by the one ingest on both paths and read with
//! the best in one statement.

mod common;

use std::path::Path;

use bms_model::TimeUs;
use common::{CHART, Fixture, MS, contribution, hex, key, record_of, sql};
use library::{BestsState, Key, LN_MODE, PlayBest, Progress, ProgressQuery};
use replay::{PlayRecord, PresetRecord};
use rules::{Judgment, RulePreset};

/// 120 BPM, a measure 2 s:
/// - a long note on key 2 from 2 s to 3 s, its head before a tap on key 1
///   at 2.5 s;
/// - a mine on key 3 at 2.5 s;
/// - a tap on key 4 at 6 s;
/// - taps on key 5 at 8 s and key 6 at 8.0625 s;
/// - a long note on key 7 from 10 s to 11 s, and a tap on key 1 at 10.5 s;
/// - a tap on key 2 at 12 s.
const ORDER_CHART: &str = "#BPM 120\n#LNTYPE 1\n\
    #00152:0101\n#00111:00010000\n#001D3:00010000\n\
    #00314:01\n\
    #00415:01\n#00418:0001000000000000000000000000000000000000000000000000000000000000\n\
    #00559:0101\n#00511:00010000\n\
    #00612:01\n";

/// The tapestry preset with GREAT weighted 3 and empty POORs counted as
/// POORs.
fn custom_preset() -> RulePreset {
    let mut p = rules::tapestry();
    p.score.ex.great = 3;
    p.empty_poor.counted = true;
    p
}

/// The play of `ORDER_CHART` under [`custom_preset`]:
/// - the long note on key 2 pressed on time and held past its end:
///   PERFECT, judged at its end, after the tap;
/// - the tap at 2.5 s pressed 30 ms late: GREAT;
/// - key 3 held over the mine;
/// - key 4 pressed 300 ms early (an empty POOR), then on time: PERFECT;
/// - keys 6 and 5 pressed at 8 s, key 6 first: GOOD (62.5 ms early), then
///   PERFECT;
/// - the long note on key 7 pressed 150 ms late: BAD, decided at the
///   press, before the tap at 10.5 s: PERFECT;
/// - the tap at 12 s missed: POOR.
fn order_record() -> PlayRecord {
    let events = vec![
        key(2, 2_000 * MS, 1),
        key(3, 2_400 * MS, 1),
        key(1, 2_530 * MS, 1),
        key(1, 2_580 * MS, 0),
        key(3, 2_600 * MS, 0),
        key(2, 3_100 * MS, 0),
        key(4, 5_700 * MS, 1),
        key(4, 5_750 * MS, 0),
        key(4, 6_000 * MS, 1),
        key(4, 6_050 * MS, 0),
        key(6, 8_000 * MS, 1),
        key(5, 8_000 * MS, 1),
        key(5, 8_050 * MS, 0),
        key(6, 8_050 * MS, 0),
        key(7, 10_150 * MS, 1),
        key(7, 10_300 * MS, 0),
        key(1, 10_500 * MS, 1),
        key(1, 10_550 * MS, 0),
    ];
    let mut record = record_of(ORDER_CHART, 0, events);
    record.preset = PresetRecord::of(&custom_preset());
    record
}

/// `[GREAT, PERFECT, PERFECT, GOOD, PERFECT, BAD, PERFECT, POOR]`: emitted
/// order, the tap before the long note judged at its end, the GOOD before
/// the PERFECT of the same µs, the BAD head before the tap after it, and no
/// entry for the mine or the empty POOR.
const ORDER_PROGRESS: [u8; 8] = [1, 0, 0, 2, 0, 3, 0, 4];

/// A play of `CHART` (taps on key 1 at 2 s and 4 s, a long note on key 2
/// from 2 s to 3 s): each tap pressed `first_ms` and `second_ms` late, the
/// long note on time and held to its end.
fn taps(first_ms: i64, second_ms: i64) -> PlayRecord {
    record_of(
        CHART,
        0,
        vec![
            key(2, 2_000 * MS, 1),
            key(1, 2_000 * MS + first_ms * MS, 1),
            key(1, 2_050 * MS + first_ms * MS, 0),
            key(2, 3_010 * MS, 0),
            key(1, 4_000 * MS + second_ms * MS, 1),
            key(1, 4_050 * MS + second_ms * MS, 0),
        ],
    )
}

/// PERFECT, PERFECT, GREAT: EX 5.
fn play_a() -> PlayRecord {
    taps(0, 30)
}

/// GREAT, PERFECT, PERFECT: EX 5, another body than A's.
fn play_b() -> PlayRecord {
    taps(30, 0)
}

fn key_of(record: &PlayRecord) -> Key {
    Key {
        preset_sha256: record.preset.sha256,
        ln_mode: LN_MODE,
        chart_sha256: record.chart.identity.sha256,
    }
}

fn body(record: &PlayRecord) -> [u8; 32] {
    library::PlayId::of(String::new(), record, 0, 0).body_sha256
}

/// The stored progress row of `record`'s key: its play's hash and bytes.
fn row(db: &Path, record: &PlayRecord) -> Option<(Vec<u8>, Vec<u8>)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let k = key_of(record);
    rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT play_sha256, judgments FROM progress
         WHERE preset_sha256 = ?1 AND ln_mode = ?2 AND chart_sha256 = ?3",
        rusqlite::params![k.preset_sha256, k.ln_mode, k.chart_sha256],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))
    .unwrap()
}

/// `record`'s best and progress as a play reads them.
fn read(lib: &library::Library, record: &PlayRecord, q: ProgressQuery) -> PlayBest {
    lib.best_writer()
        .unwrap()
        .play_best(&key_of(record), Some(q))
        .unwrap()
}

const ORDER_QUERY: ProgressQuery = ProgressQuery {
    targets: 8,
    empty_poor_counted: true,
};

const TAPS_QUERY: ProgressQuery = ProgressQuery {
    targets: 3,
    empty_poor_counted: false,
};

fn judgments(bytes: &[u8]) -> Box<[Judgment]> {
    bytes
        .iter()
        .map(|&b| Judgment::ALL[usize::from(b)])
        .collect()
}

/// A library with `text` scanned, `record` saved as `name` and counted
/// live.
fn counted_live(fx: &Fixture, text: &str, record: &PlayRecord, name: &str) -> library::Library {
    fx.chart("a.bms", text);
    let lib = fx.open();
    fx.pass(&lib);
    let (rel, _) = fx.save(record, name);
    fx.live(&lib, &rel, text, record);
    lib
}

#[test]
fn the_live_count_keeps_the_judgment_order() {
    let fx = Fixture::new();
    let record = order_record();
    let lib = counted_live(&fx, ORDER_CHART, &record, "1-1.tprl");
    let (sha, bytes) = row(&fx.db(), &record).unwrap();
    assert_eq!(
        (sha, bytes),
        (body(&record).to_vec(), ORDER_PROGRESS.to_vec())
    );
    // Read back with its best: the breakdown counts the empty POOR as a
    // POOR, and the progress has none, which the preset explains.
    let read = read(&lib, &record, ORDER_QUERY);
    let best = read.score.unwrap().best.unwrap();
    assert_eq!(
        (best.ex, best.play.counts, best.play.empty_poor),
        (11, [4, 1, 1, 1, 2], 1)
    );
    assert_eq!(
        read.progress,
        Some(Progress::Known(judgments(&ORDER_PROGRESS)))
    );
    // Read as if the preset did not count it: the POOR count disagrees.
    let uncounted = ProgressQuery {
        empty_poor_counted: false,
        ..ORDER_QUERY
    };
    assert_eq!(
        self::read(&lib, &record, uncounted).progress,
        Some(Progress::Refused("its counts are not the breakdown's"))
    );
}

/// The catch-up counts the saved record with the same bytes as the live
/// count.
#[test]
fn the_catch_up_keeps_the_same_progress() {
    let fx = Fixture::new();
    fx.chart("a.bms", ORDER_CHART);
    let record = order_record();
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 1);
    assert_eq!(
        row(&fx.db(), &record),
        Some((body(&record).to_vec(), ORDER_PROGRESS.to_vec()))
    );
}

/// A record finalized far past the chart's end is counted with the same
/// progress as at its own final deadline.
#[test]
fn a_far_watermark_keeps_the_progress() {
    let fx = Fixture::new();
    fx.chart("a.bms", ORDER_CHART);
    let mut record = order_record();
    record.status.finalized = TimeUs(i64::MAX);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    let report = fx.pass(&lib);
    assert_eq!(report.bests.counted, 1);
    assert_eq!(
        row(&fx.db(), &record),
        Some((body(&record).to_vec(), ORDER_PROGRESS.to_vec()))
    );
}

/// The lower body hash of A and B, equal in EX.
fn lower_and_higher() -> (PlayRecord, PlayRecord) {
    let (a, b) = (play_a(), play_b());
    if body(&a) < body(&b) { (a, b) } else { (b, a) }
}

fn progress_of(record: &PlayRecord) -> Vec<u8> {
    if *record == play_a() {
        vec![0, 0, 1]
    } else {
        vec![1, 0, 0]
    }
}

#[test]
fn the_tie_keeps_the_lower_hash_in_any_order() {
    let (low, high) = lower_and_higher();
    let expected = Some((body(&low).to_vec(), progress_of(&low)));
    // Live, in both orders.
    for order in [[&low, &high], [&high, &low]] {
        let fx = Fixture::new();
        fx.chart("a.bms", CHART);
        let lib = fx.open();
        fx.pass(&lib);
        for (i, r) in order.iter().enumerate() {
            let (rel, _) = fx.save(r, &format!("{}-1.tprl", i + 1));
            fx.live(&lib, &rel, CHART, r);
        }
        assert_eq!(row(&fx.db(), &low), expected);
        // A rebuild after a bump keeps it.
        drop(lib);
        sql(&fx.db(), "UPDATE meta SET bests = bests - 1");
        let lib = fx.open();
        assert!(matches!(lib.bests_state(), BestsState::Invalidated { .. }));
        assert_eq!(row(&fx.db(), &low), None);
        assert_eq!(fx.pass(&lib).bests.counted, 2);
        assert_eq!(row(&fx.db(), &low), expected);
    }
    // Both in one catch-up batch.
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    fx.save(&high, "1-1.tprl");
    fx.save(&low, "2-1.tprl");
    let lib = fx.open();
    assert_eq!(fx.pass(&lib).bests.counted, 2);
    assert_eq!(row(&fx.db(), &low), expected);
}

/// A lower play leaves the row alone; a higher one replaces it whole.
#[test]
fn only_a_new_best_replaces_the_progress() {
    let fx = Fixture::new();
    let a = play_a();
    let lib = counted_live(&fx, CHART, &a, "1-1.tprl");
    let a_row = Some((body(&a).to_vec(), vec![0, 0, 1]));
    assert_eq!(row(&fx.db(), &a), a_row);
    // GREAT, PERFECT, GREAT: EX 4.
    let c = taps(30, 30);
    let (rel, _) = fx.save(&c, "2-1.tprl");
    fx.live(&lib, &rel, CHART, &c);
    assert_eq!(row(&fx.db(), &a), a_row);
    // Every note PERFECT: EX 6.
    let d = taps(0, 0);
    let (rel, _) = fx.save(&d, "3-1.tprl");
    fx.live(&lib, &rel, CHART, &d);
    assert_eq!(row(&fx.db(), &a), Some((body(&d).to_vec(), vec![0, 0, 0])));
    let read = read(&lib, &a, TAPS_QUERY);
    assert_eq!(read.score.unwrap().best.unwrap().ex, 6);
    assert_eq!(read.progress, Some(Progress::Known(judgments(&[0, 0, 0]))));
}

/// A stored progress inconsistent with its best is refused for its own
/// reason, and the best still reads.
#[test]
fn an_inconsistent_progress_is_refused_and_the_best_stays() {
    let fx = Fixture::new();
    let record = order_record();
    let lib = counted_live(&fx, ORDER_CHART, &record, "1-1.tprl");
    let sha = hex(&body(&record));
    let good = hex(&ORDER_PROGRESS);
    let set = |what: &str| sql(&fx.db(), &format!("UPDATE progress SET {what}"));
    let check = |q: ProgressQuery, expected: Progress| {
        let read = read(&lib, &record, q);
        assert_eq!(read.score.unwrap().best.unwrap().ex, 11);
        assert_eq!(read.progress, Some(expected));
    };
    // The positive control: the stored row is accepted.
    check(ORDER_QUERY, Progress::Known(judgments(&ORDER_PROGRESS)));
    for (what, q, why) in [
        (
            format!("play_sha256 = x'{}'", hex(&[9; 32])),
            ORDER_QUERY,
            "it is another play's than the breakdown's",
        ),
        (
            // 32 characters, which the length check takes.
            "play_sha256 = '0123456789abcdef0123456789abcdef'".to_owned(),
            ORDER_QUERY,
            "its play hash is not a hash",
        ),
        // One more judgment, and a query of that many targets.
        (
            format!("judgments = x'{good}00'"),
            ProgressQuery {
                targets: 9,
                ..ORDER_QUERY
            },
            "its length is not the breakdown's notes",
        ),
        // The stored row, read by a play of another target count.
        (
            format!("judgments = x'{good}'"),
            ProgressQuery {
                targets: 7,
                ..ORDER_QUERY
            },
            "its length is not the chart's judged targets",
        ),
        (
            "judgments = x'0100000200030005'".to_owned(),
            ORDER_QUERY,
            "a judgment is out of range",
        ),
        // PERFECT and GREAT swapped once: same length, other counts.
        (
            "judgments = x'0000000200030004'".to_owned(),
            ORDER_QUERY,
            "its counts are not the breakdown's",
        ),
        (
            "judgments = 'abcdefgh'".to_owned(),
            ORDER_QUERY,
            "its judgments are not bytes",
        ),
    ] {
        set(&what);
        check(q, Progress::Refused(why));
        set(&format!("play_sha256 = x'{sha}', judgments = x'{good}'"));
        check(ORDER_QUERY, Progress::Known(judgments(&ORDER_PROGRESS)));
    }
    // No row: missing, the best still read.
    sql(&fx.db(), "DELETE FROM progress");
    check(ORDER_QUERY, Progress::Missing);
}

/// The best and its progress come from one statement: after B (equal EX,
/// lower hash) replaces A, the read pairs B's breakdown with B's progress,
/// and a progress naming A beside B's breakdown is refused, though its
/// counts are the same.
#[test]
fn the_best_and_its_progress_name_one_play() {
    let (low, high) = lower_and_higher();
    let fx = Fixture::new();
    let lib = counted_live(&fx, CHART, &high, "1-1.tprl");
    let read_high = read(&lib, &high, TAPS_QUERY);
    assert_eq!(
        read_high.score.unwrap().best.unwrap().play.body_sha256,
        body(&high)
    );
    assert_eq!(
        read_high.progress,
        Some(Progress::Known(judgments(&progress_of(&high))))
    );
    let (rel, _) = fx.save(&low, "2-1.tprl");
    fx.live(&lib, &rel, CHART, &low);
    let read_low = read(&lib, &low, TAPS_QUERY);
    assert_eq!(
        read_low.score.unwrap().best.unwrap().play.body_sha256,
        body(&low)
    );
    assert_eq!(
        read_low.progress,
        Some(Progress::Known(judgments(&progress_of(&low))))
    );
    sql(
        &fx.db(),
        &format!(
            "UPDATE progress SET play_sha256 = x'{}', judgments = x'{}'",
            hex(&body(&high)),
            hex(&progress_of(&high)),
        ),
    );
    assert_eq!(
        read(&lib, &low, TAPS_QUERY).progress,
        Some(Progress::Refused(
            "it is another play's than the breakdown's"
        ))
    );
}

/// Opening the bests of the previous version drops the populated
/// progress with `scores`, unread, and creates both in this layout; the
/// next catch-up fills it again.
#[test]
fn a_bests_bump_drops_and_refills_the_progress() {
    let fx = Fixture::new();
    let record = order_record();
    let lib = counted_live(&fx, ORDER_CHART, &record, "1-1.tprl");
    drop(lib);
    sql(
        &fx.db(),
        &format!(
            "UPDATE progress SET judgments = x'ffffffff';
             UPDATE meta SET bests = {};",
            library::BESTS_VERSION - 1
        ),
    );
    let (lib, _) = library::Library::open(&fx.db()).unwrap();
    let BestsState::Invalidated { old: Some(old) } = lib.bests_state() else {
        panic!("{:?}", lib.bests_state());
    };
    assert_eq!(old.bests, library::BESTS_VERSION - 1);
    // Before any catch-up: empty, in this layout.
    assert_eq!(row(&fx.db(), &record), None);
    let conn = rusqlite::Connection::open(fx.db()).unwrap();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('progress') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "preset_sha256",
            "ln_mode",
            "chart_sha256",
            "play_sha256",
            "judgments"
        ]
    );
    drop(conn);
    assert_eq!(fx.pass(&lib).bests.counted, 1);
    assert_eq!(
        row(&fx.db(), &record),
        Some((body(&record).to_vec(), ORDER_PROGRESS.to_vec()))
    );
}

/// An ineligible play stores no progress, and a contribution built
/// without one (no evaluation of its own) leaves none in place of the
/// play it replaces.
#[test]
fn an_ineligible_play_stores_no_progress() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    let mut record = play_a();
    record.status.end = replay::EndStatus::EndedMidway;
    let (rel, _) = fx.save(&record, "1-1.tprl");
    let c = contribution(&fx.records, &rel, CHART, &record);
    assert_eq!((c.best.is_none(), c.progress.is_none()), (true, true));
    let a = play_a();
    let (rel, _) = fx.save(&a, "2-1.tprl");
    let c = contribution(&fx.records, &rel, CHART, &a);
    assert_eq!(c.progress.as_deref(), Some(&judgments(&[0, 0, 1])[..]));
    let mut writer = lib.best_writer().unwrap();
    writer.ingest(&c).unwrap();
    // D, every note PERFECT, without its progress.
    let d = taps(0, 0);
    let (rel, _) = fx.save(&d, "3-1.tprl");
    let mut c = contribution(&fx.records, &rel, CHART, &d);
    c.progress = None;
    writer.ingest(&c).unwrap();
    assert_eq!(row(&fx.db(), &d), None);
    assert_eq!(read(&lib, &d, TAPS_QUERY).progress, Some(Progress::Missing));
}
