//! The bests phase held at its steps through the scan's test seams:
//! admission before evaluation, pausing between the load and the
//! evaluation, an exit mid-phase, listing failures, requests made during
//! the phase or failing to be written, panics, and what a later catch-up
//! reads again.

#[path = "../../../tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{CHART, Fixture, played, score_of, sql};

use crate::scan::tests::{Fault, Hooks, Step};
use crate::{BEST_BATCH, Library, ScanOptions};

const LONG: Duration = Duration::from_secs(60);
const SHORT: Duration = Duration::from_millis(300);

/// The fixture's options with the bests hook `hook`.
fn hooked(fx: &Fixture, hook: impl Fn(Step<'_>) + Send + Sync + 'static) -> ScanOptions {
    ScanOptions {
        test: Hooks {
            bests: Some(Arc::new(hook)),
            ..Hooks::default()
        },
        ..fx.options()
    }
}

/// Counts the steps of one kind.
fn counter(kind: fn(&Step<'_>) -> bool) -> (Arc<AtomicUsize>, impl Fn(Step<'_>) + Send + Sync) {
    let n = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&n);
    (n, move |s: Step<'_>| {
        if kind(&s) {
            seen.fetch_add(1, Ordering::SeqCst);
        }
    })
}

/// A hook that, at the first step of one kind, reports it and waits for
/// one go.
struct Gate {
    held: Receiver<()>,
    go: Sender<()>,
}

fn gate(kind: fn(&Step<'_>) -> bool) -> (Gate, impl Fn(Step<'_>) + Send + Sync) {
    let (held_tx, held) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let once = AtomicUsize::new(0);
    let (held_tx, go_rx) = (Mutex::new(held_tx), Mutex::new(go_rx));
    let hook = move |s: Step<'_>| {
        if kind(&s) && once.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = held_tx.lock().unwrap().send(());
            let _ = go_rx.lock().unwrap().recv_timeout(LONG);
        }
    };
    (Gate { held, go }, hook)
}

fn is_load(s: &Step<'_>) -> bool {
    matches!(s, Step::Load(_))
}

fn is_evaluate(s: &Step<'_>) -> bool {
    matches!(s, Step::Evaluate(_))
}

fn is_decode(s: &Step<'_>) -> bool {
    matches!(s, Step::Decode(_))
}

fn is_commit(s: &Step<'_>) -> bool {
    matches!(s, Step::Commit(_))
}

fn scan(fx: &Fixture, lib: &Library, options: ScanOptions) -> crate::Scan {
    lib.scan(std::slice::from_ref(&fx.charts), options).unwrap()
}

#[test]
fn over_budget_record_is_refused_before_evaluation() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let lib = fx.open();
    let (evaluated, hook) = counter(is_evaluate);
    let options = ScanOptions {
        rebuild_memory: Some(1 << 10),
        ..hooked(&fx, hook)
    };
    let report = scan(&fx, &lib, options).finish().unwrap().bests;
    assert_eq!(report.refused.too_large, 1, "{report:?}");
    assert_eq!(report.refused.total(), 1);
    assert_eq!(evaluated.load(Ordering::SeqCst), 0);
    assert_eq!(score_of(&lib, &record), None);
    // Within the budget it is evaluated once and counted.
    lib.request_bests_pass().unwrap();
    let (evaluated, hook) = counter(is_evaluate);
    // The refusal stands for the file as it is: a new name is read.
    std::fs::remove_file(
        fx.records
            .join(common::hex(&record.chart.identity.sha256))
            .join("1-1.tprl"),
    )
    .unwrap();
    fx.save(&record, "2-1.tprl");
    let report = scan(&fx, &lib, hooked(&fx, hook)).finish().unwrap().bests;
    assert_eq!((report.counted, evaluated.load(Ordering::SeqCst)), (1, 1));
}

#[test]
fn rebuild_acknowledges_between_load_and_evaluation() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    fx.save(&played(0), "1-1.tprl");
    let lib = fx.open();
    let (held, at_load) = gate(is_load);
    let (evaluated, count) = counter(is_evaluate);
    let options = hooked(&fx, move |s| {
        at_load(s);
        count(s);
    });
    let scan = scan(&fx, &lib, options);
    held.held.recv_timeout(LONG).unwrap();
    // The load is in hand: no acknowledgment until it is done.
    scan.request_pause();
    assert!(!scan.wait_paused(SHORT));
    held.go.send(()).unwrap();
    assert!(scan.wait_paused(LONG));
    assert_eq!(
        evaluated.load(Ordering::SeqCst),
        0,
        "acknowledged after evaluating"
    );
    std::thread::sleep(SHORT);
    assert_eq!(
        evaluated.load(Ordering::SeqCst),
        0,
        "evaluated while paused"
    );
    scan.release();
    assert_eq!(scan.finish().unwrap().bests.counted, 1);
    assert_eq!(evaluated.load(Ordering::SeqCst), 1);
}

#[test]
fn rebuild_resumes_after_exit_without_clearing() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    let total = BEST_BATCH + 6;
    for n in 0..total {
        fx.save(&record, &format!("{n}-1.tprl"));
    }
    let lib = fx.open();
    let (held, at_commit) = gate(is_commit);
    let scan = scan(&fx, &lib, hooked(&fx, at_commit));
    held.held.recv_timeout(LONG).unwrap();
    // Exit while the first batch commits: it lands, and the phase ends at
    // the next checkpoint.
    assert!(!scan.stop_within(Duration::ZERO));
    held.go.send(()).unwrap();
    drop(lib);
    let lib = fx.reopen();
    let first = score_of(&lib, &record).unwrap().plays;
    assert_eq!(first, u64::try_from(BEST_BATCH).unwrap());
    assert!(lib.bests_pending().unwrap());
    let report = fx.pass(&lib).bests;
    assert_eq!(
        report.already + report.skipped_unchanged,
        u64::try_from(BEST_BATCH).unwrap()
    );
    assert_eq!(report.counted, 6);
    assert_eq!(
        score_of(&lib, &record).unwrap().plays,
        u64::try_from(total).unwrap()
    );
    assert!(!lib.bests_pending().unwrap());
}

#[test]
fn listing_error_is_not_completion() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    fx.chart("b.bms", common::RANDOM_CHART);
    let a = played(0);
    fx.save(&a, "1-1.tprl");
    let [b, _] = random_plays();
    let (_, b_file) = fx.save(&b, "1-1.tprl");
    let b_folder: PathBuf = b_file.parent().unwrap().to_owned();
    let lib = fx.open();
    let options = ScanOptions {
        test: Hooks {
            fault: Some(Arc::new(move |p: &Path| {
                (p == b_folder).then_some(Fault::Listing)
            })),
            ..Hooks::default()
        },
        ..fx.options()
    };
    let report = scan(&fx, &lib, options).finish().unwrap().bests;
    // The rest is processed; completion is not recorded.
    assert!(!report.enumeration_complete);
    assert_eq!(report.counted, 1);
    assert_eq!(score_of(&lib, &a).unwrap().plays, 1);
    assert!(lib.bests_pending().unwrap());
    let report = fx.pass(&lib).bests;
    assert!(report.enumeration_complete);
    assert_eq!(report.counted, 1);
    assert!(!lib.bests_pending().unwrap());
}

/// Two plays of `RANDOM_CHART`, as the integration tests make them.
fn random_plays() -> [replay::PlayRecord; 2] {
    let rec = |seed| common::record_of(common::RANDOM_CHART, seed, Vec::new());
    [rec(0), rec(1)]
}

#[test]
fn request_during_rebuild_survives() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    fx.save(&played(0), "1-1.tprl");
    let lib = fx.open();
    let db = fx.db();
    let once = AtomicUsize::new(0);
    let options = hooked(&fx, move |s| {
        if is_load(&s) && once.fetch_add(1, Ordering::SeqCst) == 0 {
            sql(&db, "UPDATE meta SET requested = requested + 1");
        }
    });
    let report = scan(&fx, &lib, options).finish().unwrap().bests;
    assert!(report.catch_up && report.enumeration_complete);
    assert!(lib.bests_pending().unwrap(), "the request was consumed");
    let report = fx.pass(&lib).bests;
    assert!(report.catch_up);
    assert!(!lib.bests_pending().unwrap());
}

/// A trigger refusing every change of `requested`.
const REFUSE_REQUESTS: &str = "CREATE TRIGGER refuse BEFORE UPDATE ON meta
     WHEN new.requested != old.requested BEGIN SELECT RAISE(ABORT, 'injected'); END;";

#[test]
fn a_request_failing_while_paused_is_held_until_written() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    fx.save(&played(0), "1-1.tprl");
    let lib = fx.open();
    let mut writer = lib.best_writer().unwrap();
    let (held, at_load) = gate(is_load);
    let scan = scan(&fx, &lib, hooked(&fx, at_load));
    held.held.recv_timeout(LONG).unwrap();
    scan.request_pause();
    held.go.send(()).unwrap();
    assert!(scan.wait_paused(LONG));
    // Paused in the phase, which holds no transaction: a live play is
    // counted.
    let live = played(20);
    let (rel, _) = fx.save(&live, "9-1.tprl");
    let c = common::contribution(&fx.records, &rel, CHART, &live);
    assert!(matches!(
        writer.ingest(&c).unwrap(),
        crate::Ingested::Counted { .. }
    ));
    // Then a live play's request fails to be written.
    sql(&fx.db(), REFUSE_REQUESTS);
    assert!(writer.request_pass().is_err());
    sql(&fx.db(), "DROP TRIGGER refuse;");
    scan.release();
    let report = scan.finish().unwrap().bests;
    assert!(report.enumeration_complete);
    // The phase completed what it started from; the held request stays.
    assert!(lib.bests_pending().unwrap());
    let report = fx.pass(&lib).bests;
    assert!(report.catch_up);
    assert!(!lib.bests_pending().unwrap());
}

#[test]
fn a_forced_phase_interrupted_stays_pending() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let lib = fx.open();
    fx.pass(&lib);
    fx.save(&played(0), "1-1.tprl");
    sql(&fx.db(), REFUSE_REQUESTS);
    assert!(lib.request_bests_pass().is_err());
    sql(&fx.db(), "DROP TRIGGER refuse;");
    assert!(lib.bests_pending().unwrap());
    // The phase writes the held request, then the run exits mid-phase.
    let (held, at_load) = gate(is_load);
    let scan = scan(&fx, &lib, hooked(&fx, at_load));
    held.held.recv_timeout(LONG).unwrap();
    assert!(!scan.stop_within(Duration::ZERO));
    held.go.send(()).unwrap();
    drop(lib);
    // A new run holds nothing in memory: the request was written.
    let lib = fx.reopen();
    assert!(lib.bests_pending().unwrap());
    assert_eq!(fx.pass(&lib).bests.counted, 1);
    assert!(!lib.bests_pending().unwrap());
}

/// A record gone between the listing and its read is counted missing, and
/// an entry that is not a file unreadable; neither is stored, so the next
/// catch-up looks at them again.
#[test]
fn missing_and_unreadable_records_are_counted_and_retried() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    fx.save(&record, "1-1.tprl");
    let (_, gone) = fx.save(&record, "2-1.tprl");
    let (_, folder) = fx.save(&record, "4-1.tprl");
    std::fs::remove_file(&folder).unwrap();
    std::fs::create_dir(&folder).unwrap();
    let lib = fx.open();
    let options = hooked(&fx, move |s| {
        if let Step::Decode(p) = s
            && p.ends_with("1-1.tprl")
        {
            let _ = std::fs::remove_file(&gone);
        }
    });
    let report = scan(&fx, &lib, options).finish().unwrap().bests;
    assert_eq!(
        (report.refused.missing, report.refused.unreadable),
        (1, 1),
        "{report:?}"
    );
    assert_eq!(report.counted, 1);
    lib.request_bests_pass().unwrap();
    let report = fx.pass(&lib).bests;
    assert_eq!(
        (report.refused.unreadable, report.skipped_refused),
        (1, 0),
        "{report:?}"
    );
}

#[test]
fn a_panic_is_a_refusal_and_the_phase_goes_on() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let record = played(0);
    let (_, bad) = fx.save(&record, "1-1.tprl");
    fx.save(&record, "2-1.tprl");
    let lib = fx.open();
    let options = hooked(&fx, move |s| {
        if let Step::Decode(p) = s
            && p == bad
        {
            panic!("injected");
        }
    });
    let report = scan(&fx, &lib, options).finish().unwrap().bests;
    assert_eq!((report.refused.panicked, report.counted), (1, 1));
    assert!(report.enumeration_complete);
}

#[test]
fn refused_records_are_not_read_again_and_waiting_ones_are_retried() {
    let fx = Fixture::new();
    fx.chart("a.bms", CHART);
    let mut old = played(0);
    old.versions.evaluator += 1;
    let refused = 1_000;
    let bytes = replay::encode(&old);
    for n in 0..refused {
        fx.save_bytes(&old, &format!("{n}-1.tprl"), &bytes);
    }
    // Waits for a chart not yet in the library.
    let waiting = common::record_of(common::RANDOM_CHART, 0, Vec::new());
    fx.save(&waiting, "1-1.tprl");
    let lib = fx.open();
    let (decoded, hook) = counter(is_decode);
    let report = scan(&fx, &lib, hooked(&fx, hook)).finish().unwrap().bests;
    assert_eq!(report.refused.evaluator, refused);
    assert_eq!(report.refused.chart_not_in_library, 1);
    assert_eq!(decoded.load(Ordering::SeqCst), 1_001);
    // The next catch-up decodes only the waiting record.
    lib.request_bests_pass().unwrap();
    let (decoded, hook) = counter(is_decode);
    let report = scan(&fx, &lib, hooked(&fx, hook)).finish().unwrap().bests;
    assert_eq!(report.skipped_refused, refused);
    assert_eq!(decoded.load(Ordering::SeqCst), 1);
    // Its chart is indexed: the pass retries it alone.
    fx.chart("r.bms", common::RANDOM_CHART);
    let (decoded, hook) = counter(is_decode);
    let report = scan(&fx, &lib, hooked(&fx, hook)).finish().unwrap().bests;
    assert!(!report.catch_up);
    assert_eq!((report.retried, report.counted), (1, 1));
    assert_eq!(decoded.load(Ordering::SeqCst), 1);
}
