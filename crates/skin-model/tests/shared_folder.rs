//! The folder of fixed files the test support modules share across test
//! processes: placing it again never replaces a file, other contents take
//! another folder, and while writer processes place the same files at once,
//! as nextest runs them, a reader never finds one absent or unreadable.

mod shared_folder_support;

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use image::{Rgba, RgbaImage};
use shared_folder_support::{create, folder_in, png};
use skin_model::{AtlasError, AtlasLimits, FileDef, Filter, ImageDef, ImageError, Sheet, Variant};

/// Names the base folder a writer process places the files under.
const WRITER_BASE: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_BASE";
/// Names the file whose creation lets a writer process begin placing.
const WRITER_START: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_START";
/// Names the file whose creation ends a writer process's placing.
const WRITER_STOP: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_STOP";
/// Set, a writer process fails at once.
const WRITER_FAILS: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_FAILS";
/// How many writer processes run at once.
const WRITERS: usize = 4;
/// How long the writers keep placing the files once the race lets the
/// readers go: a first placement slowed by the machine shortens none of it.
const WRITING: Duration = Duration::from_millis(3_000);
/// Reader threads that open each file as the loader does (its canonical
/// path, then the file, read whole), that load the sheet (each file's path
/// and header), and that also build its atlas (each file again, and its
/// pixels).
const FILE_READERS: usize = 2;
const SHEET_READERS: usize = 2;
const ATLAS_READERS: usize = 1;
/// Past this the writers are killed and the test fails.
const LIMIT: Duration = Duration::from_secs(60);
/// How long a failing race may take to end: well under [`LIMIT`], which a
/// writer left waiting for its start file takes to end by itself.
const ENDING: Duration = Duration::from_secs(20);
const FILES: u32 = 16;
const LIMITS: AtlasLimits = AtlasLimits {
    max_side: 256,
    max_layers: 4,
    reserved_layers: 1,
};

/// The files: `FILES` PNG images of their own sizes.
fn files() -> Vec<(String, Vec<u8>)> {
    (0..FILES)
        .map(|i| {
            let image = RgbaImage::from_pixel(4 + i, 2 + i % 5, Rgba([255; 4]));
            (format!("{i}.png"), png(&image))
        })
        .collect()
}

/// A fresh base folder for one test's placements.
fn base() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(root).unwrap();
    tempfile::Builder::new()
        .prefix("skin-model-shared-folder-")
        .tempdir_in(root)
        .unwrap()
}

/// Loads the folder as the tests do: the sheet, then, with `atlas`, its
/// atlas.
fn load(dir: &Path, atlas: bool) -> Result<(), AtlasError> {
    let sheet = Sheet::new(
        dir,
        (0..FILES)
            .map(|i| FileDef {
                path: format!("{i}.png"),
                hi: None,
            })
            .collect(),
        (0..FILES)
            .map(|file| ImageDef {
                file,
                rect: None,
                grid: [1, 1],
                cycle: None,
                filter: Filter::Linear,
            })
            .collect(),
    )?;
    if atlas {
        sheet.atlas(Variant::Lo, LIMITS, &|| false)?;
    }
    Ok(())
}

/// A modified time no placement now gives a file.
fn aged() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn age(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(aged())
        .unwrap();
}

/// `path` is still the aged file and holds `bytes`.
fn assert_kept(path: &Path, bytes: &[u8]) {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    assert_eq!(modified, aged(), "{path:?} was replaced");
    assert_eq!(std::fs::read(path).unwrap(), bytes, "{path:?}");
}

#[test]
fn placing_again_keeps_each_file() {
    let base = base();
    let files = files();
    let dir = folder_in(base.path(), "shared", &files);
    for (name, _) in &files {
        age(&dir.join(name));
    }
    assert_eq!(folder_in(base.path(), "shared", &files), dir);
    for (name, bytes) in &files {
        assert_kept(&dir.join(name), bytes);
    }
}

/// Placing a placed folder again writes nothing into it, not even a file
/// set aside: the folder's own modified time, which a file created and
/// removed there moves, stays.
#[test]
fn placing_a_placed_folder_writes_nothing() {
    let base = base();
    let files = files();
    let dir = folder_in(base.path(), "shared", &files);
    let modified = || std::fs::metadata(&dir).unwrap().modified().unwrap();
    let before = modified();
    std::thread::sleep(Duration::from_millis(50));
    folder_in(base.path(), "shared", &files);
    assert_eq!(modified(), before, "the placement wrote into the folder");
    // The control: a file created and removed there moves the time.
    drop(tempfile::NamedTempFile::new_in(&dir).unwrap());
    assert_ne!(modified(), before, "a file set aside left the time");
}

/// A placement that found the name free, and another process's file there
/// once it moves its own, keeps that file.
#[test]
fn creating_over_a_file_keeps_it() {
    let base = base();
    let files = files();
    let dir = folder_in(base.path(), "shared", &files);
    let (name, bytes) = &files[0];
    age(&dir.join(name));
    create(&dir, name, bytes);
    assert_kept(&dir.join(name), bytes);
}

#[test]
fn other_contents_take_their_own_folder() {
    let base = base();
    let files = files();
    let mut other = files.clone();
    other[0].1 = png(&RgbaImage::from_pixel(9, 9, Rgba([1, 2, 3, 255])));
    let dir = folder_in(base.path(), "shared", &files);
    let other_dir = folder_in(base.path(), "shared", &other);
    assert_ne!(dir, other_dir);
    for (dir, files) in [(&dir, &files), (&other_dir, &other)] {
        for (name, bytes) in files {
            assert_eq!(&std::fs::read(dir.join(name)).unwrap(), bytes, "{name}");
        }
    }
}

#[test]
fn a_file_with_other_bytes_is_refused() {
    let base = base();
    let files = files();
    let dir = folder_in(base.path(), "shared", &files);
    std::fs::write(dir.join("0.png"), b"not the image").unwrap();
    let placed = std::panic::catch_unwind(|| folder_in(base.path(), "shared", &files));
    let payload = placed.expect_err("the folder was placed over other bytes");
    let message = payload.downcast_ref::<String>().map_or("", String::as_str);
    assert!(
        message.contains("0.png") && message.contains("holds other bytes"),
        "{message}"
    );
}

/// Microseconds since the Unix epoch: a time the writer processes and the
/// race read alike.
fn unix_us() -> u64 {
    let since = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    u64::try_from(since.as_micros()).unwrap()
}

/// The writer process: waits for the start file it is given, then places
/// the files under the base folder it is given, again and again until the
/// stop file it is given exists. It prints, once it stopped, when it
/// entered, how many times it placed the files, and when each placement
/// began and ended.
#[test]
#[ignore = "a writer process of a_reader_never_meets_a_file_being_placed"]
fn writer() {
    let entered = unix_us();
    assert!(
        std::env::var_os(WRITER_FAILS).is_none(),
        "this writer fails as it was asked"
    );
    let base = PathBuf::from(std::env::var_os(WRITER_BASE).unwrap());
    let start = PathBuf::from(std::env::var_os(WRITER_START).unwrap());
    let stop = PathBuf::from(std::env::var_os(WRITER_STOP).unwrap());
    let waiting = Instant::now();
    while !start.exists() {
        assert!(waiting.elapsed() < LIMIT, "no start file within {LIMIT:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    let files = files();
    let mut placed = Vec::new();
    while !stop.exists() {
        assert!(waiting.elapsed() < LIMIT, "no stop file within {LIMIT:?}");
        let began = unix_us();
        folder_in(&base, "shared", &files);
        placed.push((began, unix_us()));
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "writer entered {entered}").unwrap();
    writeln!(out, "writer passes {}", placed.len()).unwrap();
    for (began, ended) in placed {
        writeln!(out, "writer placed {began} {ended}").unwrap();
    }
}

/// Spawns a writer process that writes its output, a line for each
/// placement, into the file `out`: a pipe nobody reads until the writer
/// ends would hold it up once full.
fn spawn_writer(base: &Path, start: &Path, stop: &Path, fails: bool, out: &Path) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    if fails {
        command.env(WRITER_FAILS, "1");
    }
    let out = std::fs::File::create(out).unwrap();
    command
        .args(["writer", "--exact", "--ignored", "--nocapture"])
        .env(WRITER_BASE, base)
        .env(WRITER_START, start)
        .env(WRITER_STOP, stop)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap()
}

/// What one writer process reported, its times in [`unix_us`].
struct Wrote {
    entered: u64,
    /// When each placement began and when it ended.
    placed: Vec<(u64, u64)>,
}

impl Wrote {
    fn parse(out: &str) -> Self {
        let field = |key: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("a writer did not report `{key}`: {out}"))
                .trim()
                .parse::<u64>()
                .unwrap()
        };
        let placed: Vec<(u64, u64)> = out
            .lines()
            .filter_map(|l| l.strip_prefix("writer placed "))
            .map(|times| {
                let (began, ended) = times.trim().split_once(' ').unwrap();
                (began.parse().unwrap(), ended.parse().unwrap())
            })
            .collect();
        assert_eq!(
            u64::try_from(placed.len()).unwrap(),
            field("writer passes "),
            "a writer did not report each placement"
        );
        Self {
            entered: field("writer entered "),
            placed,
        }
    }
}

/// What one reader thread saw, its times in [`unix_us`].
#[derive(Default)]
struct Seen {
    missing: u32,
    others: Vec<String>,
    /// The longest round, whatever its outcome.
    longest: Duration,
    /// When each round that loaded began and when it ended.
    loaded: Vec<(u64, u64)>,
    /// When the reader was ready, and when it saw it was let go.
    ready: u64,
    went: u64,
}

/// What a reader does each round.
#[derive(Clone, Copy)]
enum Reading {
    Files,
    Sheet,
    Atlas,
}

/// Opens each file of `dir` by its canonical path and reads it whole: the
/// loader's steps, without its checks between them.
fn open_files(dir: &Path) -> std::io::Result<()> {
    for i in 0..FILES {
        let path = dir.join(format!("{i}.png")).canonicalize()?;
        std::fs::File::open(path)?.read_to_end(&mut Vec::new())?;
    }
    Ok(())
}

/// `e` and its sources, joined by `: `.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text = format!("{text}: {s}");
        source = s.source();
    }
    text
}

/// The race's signals to its readers.
struct Signals {
    /// How many readers are ready; the writers start once all are.
    ready: AtomicUsize,
    /// The folder, set once the race placed it; then the readers go.
    dir: OnceLock<PathBuf>,
    go: AtomicBool,
    /// The writers ended.
    done: AtomicBool,
}

/// Counts itself ready, waits to be let go, then reads the folder round
/// after round until the writers end, and at least once.
fn read_until(reading: Reading, signals: &Signals) -> Seen {
    let mut seen = Seen {
        ready: unix_us(),
        ..Seen::default()
    };
    signals.ready.fetch_add(1, Ordering::Release);
    while !signals.go.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(1));
    }
    seen.went = unix_us();
    // No folder: the race failed before placing it.
    let Some(dir) = signals.dir.get() else {
        return seen;
    };
    let mut rounds = 0u32;
    while rounds == 0 || !signals.done.load(Ordering::Relaxed) {
        rounds += 1;
        let at = unix_us();
        let round = Instant::now();
        let failed = match reading {
            Reading::Files => open_files(dir)
                .err()
                .map(|e| (e.kind() == std::io::ErrorKind::NotFound, chain(&e))),
            Reading::Sheet | Reading::Atlas => {
                load(dir, matches!(reading, Reading::Atlas)).err().map(|e| {
                    let missing = matches!(e, AtlasError::Image(ImageError::Missing { .. }));
                    (missing, chain(&e))
                })
            }
        };
        let ended = unix_us();
        seen.longest = seen.longest.max(round.elapsed());
        match failed {
            None => seen.loaded.push((at, ended)),
            Some((true, _)) => seen.missing += 1,
            Some((false, e)) => seen.others.push(e),
        }
    }
    seen
}

/// Waits for every writer, each with the file it writes its output into,
/// writing the stop file `stop` at `stop_at`, and fails past [`LIMIT`];
/// takes each writer that ended from `writers` and returns its output.
fn wait_writers(
    writers: &mut Vec<(Child, PathBuf)>,
    start: Instant,
    stop: &Path,
    stop_at: Instant,
) -> Vec<String> {
    let mut outputs = Vec::new();
    while !writers.is_empty() {
        if Instant::now() >= stop_at && !stop.exists() {
            std::fs::write(stop, b"").unwrap();
        }
        assert!(
            start.elapsed() <= LIMIT,
            "the writers did not end within {LIMIT:?}"
        );
        let mut i = 0;
        while i < writers.len() {
            if let Some(status) = writers[i].0.try_wait().unwrap() {
                let (_, out) = writers.swap_remove(i);
                let out = std::fs::read_to_string(out).unwrap();
                assert!(status.success(), "a writer failed: {out}");
                outputs.push(out);
            } else {
                i += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    outputs
}

/// One race: each writer's output, what each reader saw, and when the race
/// spawned the writers, wrote their start file and let the readers go, in
/// [`unix_us`].
struct Raced {
    outputs: Vec<String>,
    seen: Vec<Seen>,
    spawned: u64,
    started: u64,
    let_go: u64,
}

/// What a race is made to fail by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// Every writer fails at once.
    Writers,
    /// The reader of this index, counted from 0, does not spawn.
    Reader(usize),
}

/// Runs [`WRITERS`] writer processes against the reader threads until the
/// writers end, made to fail by `fault`. The readers are ready before any
/// writer places: the writers wait for a start file, written once every
/// reader counted itself ready. The race then places the folder itself, its
/// first placements racing the writers' to create the files, lets the
/// readers go, and [`WRITING`] later writes the stop file that ends the
/// writers' placing.
fn race(base: &Path, fault: Fault) -> Raced {
    let start = Instant::now();
    let (start_file, stop_file) = (base.join("start"), base.join("stop"));
    let readings: Vec<Reading> = [
        (Reading::Files, FILE_READERS),
        (Reading::Sheet, SHEET_READERS),
        (Reading::Atlas, ATLAS_READERS),
    ]
    .into_iter()
    .flat_map(|(reading, n)| std::iter::repeat_n(reading, n))
    .collect();
    let signals = Signals {
        ready: AtomicUsize::new(0),
        dir: OnceLock::new(),
        go: AtomicBool::new(false),
        done: AtomicBool::new(false),
    };
    let mut writers = Vec::new();
    let (spawned, mut started, mut let_go) = (unix_us(), 0, 0);
    let (outputs, seen) = std::thread::scope(|s| {
        let mut readers = Vec::new();
        // Whatever fails, from the first spawn on, the cleanup below runs.
        let outputs = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for i in 0..WRITERS {
                let out = base.join(format!("writer-{i}.out"));
                let fails = fault == Fault::Writers;
                let writer = spawn_writer(base, &start_file, &stop_file, fails, &out);
                writers.push((writer, out));
            }
            for (i, reading) in readings.into_iter().enumerate() {
                let signals = &signals;
                let reader = if fault == Fault::Reader(i) {
                    Err(std::io::Error::other("this reader fails as it was asked"))
                } else {
                    std::thread::Builder::new()
                        .spawn_scoped(s, move || read_until(reading, signals))
                };
                readers.push(reader.unwrap_or_else(|e| panic!("a reader did not spawn: {e}")));
            }
            while signals.ready.load(Ordering::Acquire) < readers.len() {
                assert!(
                    start.elapsed() <= LIMIT,
                    "the readers were not ready within {LIMIT:?}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            std::fs::write(&start_file, b"").unwrap();
            started = unix_us();
            let dir = folder_in(base, "shared", &files());
            signals.dir.set(dir).unwrap();
            let_go = unix_us();
            signals.go.store(true, Ordering::Release);
            let stop_at = Instant::now() + WRITING;
            wait_writers(&mut writers, start, &stop_file, stop_at)
        }));
        // The cleanup: the writers left end, and the readers stop, or the
        // scope would wait for them for good.
        let _ = std::fs::write(&stop_file, b"");
        for (writer, _) in &mut writers {
            let _ = writer.kill();
            let _ = writer.wait();
        }
        signals.go.store(true, Ordering::Release);
        signals.done.store(true, Ordering::Relaxed);
        let seen: Vec<Seen> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        (outputs, seen)
    });
    let outputs = outputs.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    Raced {
        outputs,
        seen,
        spawned,
        started,
        let_go,
    }
}

/// Runs a race made to fail by `fault` on a thread of its own and returns
/// the message it failed with, which must come within [`ENDING`]: the
/// cleanup ends and waits for the writers and stops the readers.
fn failed_race(fault: Fault) -> String {
    let base = base();
    let (sent, ended) = std::sync::mpsc::channel();
    let path = base.path().to_owned();
    std::thread::spawn(move || {
        let raced = std::panic::catch_unwind(|| race(&path, fault));
        let payload = raced
            .err()
            .map(|p| p.downcast_ref::<String>().cloned().unwrap_or_default());
        sent.send(payload).unwrap();
    });
    let payload = ended
        .recv_timeout(ENDING)
        .unwrap_or_else(|_| panic!("the race did not end within {ENDING:?} of {fault:?}"));
    payload.unwrap_or_else(|| panic!("the race passed with {fault:?}"))
}

/// A writer that fails fails the race, and the readers stop: the race does
/// not wait for them for good.
#[test]
fn a_failed_writer_ends_the_race() {
    let message = failed_race(Fault::Writers);
    assert!(
        message.contains("a writer failed") && message.contains("this writer fails"),
        "{message}"
    );
}

/// A reader that does not spawn fails the race, and what was spawned before
/// it ends: the writers, still waiting for their start file, and the
/// readers, waiting to be let go.
#[test]
fn a_reader_not_spawned_ends_the_race() {
    let last = FILE_READERS + SHEET_READERS + ATLAS_READERS - 1;
    let message = failed_race(Fault::Reader(last));
    assert!(
        message.contains("a reader did not spawn") && message.contains("this reader fails"),
        "{message}"
    );
}

/// On Windows a file being replaced is absent, or refuses to open, only in
/// a short window of the replacement, so a placement that replaced files
/// fails this in some runs, not every: [`placing_again_keeps_each_file`]
/// checks in every run that a placement replaces nothing. Only reads while
/// a writer places count: each reader must load the folder in a round that
/// lies wholly within the writers' placements, some writer placing the
/// files at every moment from its start to its end.
#[test]
fn a_reader_never_meets_a_file_being_placed() {
    let base = base();
    let begun = Instant::now();
    let raced = race(base.path(), Fault::None);
    let wrote: Vec<Wrote> = raced.outputs.iter().map(|o| Wrote::parse(o)).collect();
    // The placings: every writer's placements, those that meet merged.
    let mut placings: Vec<(u64, u64)> = wrote.iter().flat_map(|w| w.placed.clone()).collect();
    placings.sort_unstable();
    placings.dedup_by(|next, merged| {
        let meets = next.0 <= merged.1;
        if meets {
            merged.1 = merged.1.max(next.1);
        }
        meets
    });
    let first = placings.first().map_or(u64::MAX, |p| p.0);
    let last = placings.last().map_or(0, |p| p.1);
    let within = |b: u64, e: u64| {
        let i = placings.partition_point(|p| p.0 <= b);
        i > 0 && e <= placings[i - 1].1
    };
    let seen = &raced.seen;
    // Milliseconds from `from` to `to`, both in unix_us.
    let ms = |from: u64, to: u64| (i128::from(to) - i128::from(from)) / 1_000;
    let count = |keep: &dyn Fn(u64, u64) -> bool| {
        seen.iter()
            .map(|s| s.loaded.iter().filter(|&&(b, e)| keep(b, e)).count())
            .collect::<Vec<_>>()
    };

    // A loading round's place: begun before the first placement, wholly
    // within the placings, across a moment no writer placed between the
    // first placement and the last, or ended after the last.
    let passes: Vec<usize> = wrote.iter().map(|w| w.placed.len()).collect();
    let before = count(&|b, _| b < first);
    let during = count(&|b, e| within(b, e));
    let across = count(&|b, e| b >= first && e <= last && !within(b, e));
    let after = count(&|b, e| b >= first && e > last);
    let gaps = placings.windows(2).map(|p| p[1].0 - p[0].1);
    let longest_gap = gaps.max().unwrap_or(0);
    let longest: Vec<u128> = seen.iter().map(|s| s.longest.as_millis()).collect();
    let missing: u32 = seen.iter().map(|s| s.missing).sum();
    let others: Vec<&String> = seen.iter().flat_map(|s| &s.others).collect();
    let from_spawn = |times: &mut dyn Iterator<Item = u64>| {
        times.map(|t| ms(raced.spawned, t)).collect::<Vec<_>>()
    };
    let ends = |w: &Wrote| {
        w.placed
            .first()
            .map(|f| f.0)
            .zip(w.placed.last().map(|l| l.1))
    };
    println!(
        "passes {passes:?}, loads before {before:?} during {during:?} across {across:?} \
         after {after:?} the placings, missing {missing}, other errors {}, longest rounds \
         {longest:?} ms; {} placings, the longest gap between them {longest_gap} us; ms after \
         the spawn: writers entered {:?}, start file {}, writers began {:?} and ended {:?}, \
         readers ready {:?}, let go {} and saw it {:?}; in {} ms",
        others.len(),
        placings.len(),
        from_spawn(&mut wrote.iter().map(|w| w.entered)),
        ms(raced.spawned, raced.started),
        from_spawn(&mut wrote.iter().filter_map(ends).map(|(began, _)| began)),
        from_spawn(&mut wrote.iter().filter_map(ends).map(|(_, ended)| ended)),
        from_spawn(&mut seen.iter().map(|s| s.ready)),
        ms(raced.spawned, raced.let_go),
        from_spawn(&mut seen.iter().map(|s| s.went)),
        begun.elapsed().as_millis()
    );
    assert_eq!(
        (missing, others.len()),
        (0, 0),
        "first other errors: {:?}",
        &others[..others.len().min(3)]
    );
    // The controls: each writer placed the files, and each reader loaded
    // them in a round wholly within the placings, so the race ran.
    assert!(passes.iter().all(|&p| p > 0), "passes {passes:?}");
    assert!(
        during.iter().all(|&d| d > 0),
        "a reader loaded nothing within the placings: loads before {before:?} during \
         {during:?} across {across:?} after {after:?}"
    );
}
