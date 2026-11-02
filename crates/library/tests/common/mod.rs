//! Records of self-made charts for the bests tests, saved as the game saves
//! them under `records\<chart sha256>\`, and the library over them. Shared
//! by the integration tests and the scan's unit tests.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bms_model::{Lane, Speed, TimeUs};
use bms_parser::{LoadOptions, load_bms};
use eval::{
    Binding, ControlId, InputKind, InputStart, PhysicalInputEvent, SourceId, SourceProfile,
};
use library::{
    Contribution, Ingested, LN_MODE, Library, PlayId, ScanOptions, ScanReport, Score, Verified,
};
use replay::{
    BuildProvenance, ChartRecord, DeviceKind, DeviceRecord, EndStatus, Incomplete, PASSTHROUGH,
    PlayRecord, PlayType, PresetRecord, ReceiveMethod, SourceRecord, Status, Versions,
};

pub const MS: i64 = 1_000;

/// Taps on key 1 at 2 s and 4 s (120 BPM, a measure is 2 s), and a long
/// note on key 2 from 2 s to 3 s.
pub const CHART: &str = "#BPM 120\n#LNTYPE 1\n#00111:01\n#00211:01\n#00152:0101\n";

/// `CHART` with one tap at 2 s or two at 2 s and 2.5 s on key 3, by the
/// `#RANDOM` draw.
pub const RANDOM_CHART: &str = "#BPM 120\n#00111:01\n#00211:01\n#RANDOM 2\n#IF 1\n#00113:01\n\
     #ENDIF\n#IF 2\n#00113:0101\n#ENDIF\n";

/// A data root: chart files under `charts`, records under `records` and the
/// library database, in a temporary folder.
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub charts: PathBuf,
    pub records: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("bests").tempdir().unwrap();
        let charts = dir.path().join("charts");
        let records = dir.path().join("records");
        fs::create_dir_all(&charts).unwrap();
        Self {
            dir,
            charts,
            records,
        }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.path().join("library.db")
    }

    pub fn open(&self) -> Library {
        Library::open(&self.db()).unwrap().0
    }

    /// Opens the library, waiting while a scan that ended is still letting
    /// go of the lock.
    pub fn reopen(&self) -> Library {
        let begun = std::time::Instant::now();
        loop {
            match Library::open(&self.db()) {
                Ok((lib, _)) => return lib,
                Err(library::LibraryError::AlreadyOpen { .. }) => {
                    assert!(begun.elapsed() < Duration::from_secs(60), "never released");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    /// Writes chart `name` with `text` under the charts root.
    pub fn chart(&self, name: &str, text: &str) -> PathBuf {
        let path = self.charts.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    /// The options of a pass that runs the bests phase over the records.
    pub fn options(&self) -> ScanOptions {
        ScanOptions {
            records: Some(self.records.clone()),
            ..ScanOptions::default()
        }
    }

    /// One pass over the charts and the records, to its end.
    pub fn pass(&self, lib: &Library) -> ScanReport {
        self.pass_with(lib, self.options())
    }

    pub fn pass_with(&self, lib: &Library, options: ScanOptions) -> ScanReport {
        lib.scan(std::slice::from_ref(&self.charts), options)
            .unwrap()
            .finish()
            .unwrap()
    }

    /// Saves `bytes` as record `name` of `record`'s chart; returns its path
    /// relative to `records` and its file.
    pub fn save_bytes(&self, record: &PlayRecord, name: &str, bytes: &[u8]) -> (String, PathBuf) {
        let hex = hex(&record.chart.identity.sha256);
        let dir = self.records.join(&hex);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        (format!("{hex}/{name}"), path)
    }

    /// Saves `record` encoded as the game does, as `name`.
    pub fn save(&self, record: &PlayRecord, name: &str) -> (String, PathBuf) {
        self.save_bytes(record, name, &replay::encode(record))
    }

    /// Counts `record`, saved at `rel`, as the live path does: verified by
    /// its as-played evaluation on the chart in `text`.
    pub fn live(&self, lib: &Library, rel: &str, text: &str, record: &PlayRecord) -> Ingested {
        let c = contribution(&self.records, rel, text, record);
        lib.best_writer().unwrap().ingest(&c).unwrap()
    }
}

/// The contribution of `record`, saved at `rel` under `records`, as the
/// live path makes it from the chart in `text`.
pub fn contribution(records: &Path, rel: &str, text: &str, record: &PlayRecord) -> Contribution {
    let dir = tempfile::tempdir().unwrap();
    let chart = replay::verify::load_recorded_chart(record, text.as_bytes(), dir.path()).unwrap();
    let re = replay::verify::evaluate_record(record, &chart, None).unwrap();
    let meta = fs::metadata(records.join(rel)).unwrap();
    let (size, mtime) = stamp(&meta);
    let preset = replay::verify::check_record(record).unwrap();
    Contribution::of(
        PlayId::of(rel.to_owned(), record, size, mtime),
        record,
        &Verified {
            completed: re.evaluation.completed,
        },
        &re.evaluation,
        &preset,
    )
}

/// Size and modification time, as the library stores them.
pub fn stamp(meta: &fs::Metadata) -> (i64, i64) {
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    (
        i64::try_from(meta.len()).unwrap(),
        i64::try_from(mtime).unwrap(),
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Key `n` (bound to lane `n`) set to `value` at `at` µs.
pub fn key(n: u32, at: i64, value: i32) -> PhysicalInputEvent {
    PhysicalInputEvent {
        received: TimeUs(at),
        source: SourceId(0),
        kind: InputKind::Control {
            control: ControlId(n),
            value,
        },
    }
}

/// A completed play of the chart in `text` loaded with `seed`, under the
/// tapestry preset: `events` as its input, finalized at `finalized` µs.
pub fn record_of(text: &str, seed: u64, events: Vec<PhysicalInputEvent>) -> PlayRecord {
    let dir = tempfile::tempdir().unwrap();
    let opts = LoadOptions {
        seed,
        ..LoadOptions::default()
    };
    let chart = load_bms(text.as_bytes(), dir.path(), &opts).unwrap().chart;
    let finalized = eval::Evaluator::new(
        &chart,
        &rules::tapestry(),
        &profile(),
        &InputStart::default(),
        None,
        0,
    )
    .unwrap()
    .final_deadline();
    PlayRecord {
        versions: Versions {
            evaluator: rules::Implementation::STANDARD_VERSION,
            app: "test".to_owned(),
            provenance: BuildProvenance::Development,
        },
        chart: ChartRecord {
            identity: chart.identity,
            random_seed: seed,
            transform: PASSTHROUGH.record(),
        },
        preset: PresetRecord::of(&rules::tapestry()),
        session_policy: "default".to_owned(),
        devices: DeviceRecord {
            sources: vec![SourceRecord {
                kind: DeviceKind::Keyboard,
                receive: ReceiveMethod::Scripted,
                profile: profile().sources[0].clone(),
                axis_out_of_range: 0,
            }],
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
            wake: replay::WakePolicy::Timer {
                heartbeat_us: 1_000,
            },
        },
        play_type: PlayType::Full,
        speed: Speed::NORMAL,
        start: InputStart {
            opened_at: TimeUs(0),
            held: Vec::new(),
        },
        events,
        arrivals: Vec::new(),
        status: Status {
            end: EndStatus::PlayedToEnd,
            finalized,
            incomplete: Incomplete::default(),
        },
    }
}

/// One keyboard binding keys 1-7 to lanes 1-7.
pub fn profile() -> eval::DeviceProfile {
    eval::DeviceProfile {
        sources: vec![SourceProfile {
            input_offset: TimeUs(0),
            bindings: (1..=7)
                .map(|n| Binding {
                    control: ControlId(n),
                    lane: Lane::new(u8::try_from(n).unwrap()).unwrap(),
                })
                .collect(),
            axes: Vec::new(),
        }],
    }
}

/// Every note of `CHART` hit `late_ms` late: both taps and the long note,
/// held to its end.
pub fn hits(late_ms: i64) -> Vec<PhysicalInputEvent> {
    let late = late_ms * MS;
    vec![
        key(1, 2_000 * MS + late, 1),
        key(2, 2_000 * MS + late, 1),
        key(1, 2_050 * MS + late, 0),
        key(2, 3_010 * MS, 0),
        key(1, 4_000 * MS + late, 1),
        key(1, 4_050 * MS + late, 0),
    ]
}

/// A completed play of `CHART` hitting every note `late_ms` late.
pub fn played(late_ms: i64) -> PlayRecord {
    record_of(CHART, 0, hits(late_ms))
}

/// The score of `record`'s key.
pub fn score_of(lib: &Library, record: &PlayRecord) -> Option<Score> {
    lib.bests(&record.preset.sha256, LN_MODE)
        .unwrap()
        .remove(&record.chart.identity.sha256)
}

/// A row of `counted`: path and body hash.
pub type CountedRow = (String, Vec<u8>);
/// A row of `scores`: chart hash, plays and best EX.
pub type ScoreRow = (Vec<u8>, i64, Option<i64>);

/// The rows of `counted` and `scores`, as plain values, through a
/// connection of its own.
pub fn tables(db: &Path) -> (Vec<CountedRow>, Vec<ScoreRow>) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let counted = conn
        .prepare("SELECT path, body_sha256 FROM counted ORDER BY path, body_sha256")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let scores = conn
        .prepare("SELECT chart_sha256, plays, best_ex FROM scores ORDER BY chart_sha256")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    (counted, scores)
}

/// The paths in `refused`, through a connection of its own.
pub fn refused(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.prepare("SELECT path FROM refused ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Runs `sql` on the database through a connection of its own.
pub fn sql(db: &Path, sql: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch(sql).unwrap();
}
