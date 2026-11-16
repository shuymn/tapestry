//! The folder of fixed files the test support modules share across test
//! processes: placing it again never replaces a file, other contents take
//! another folder, and while writer processes place the same files at once,
//! as nextest runs them, a reader never finds one absent or unreadable.

mod shared_folder_support;

use std::io::Read;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Barrier, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use image::{Rgba, RgbaImage};
use shared_folder_support::{create, folder_in, png};
use skin_model::{AtlasError, AtlasLimits, FileDef, Filter, ImageDef, ImageError, Sheet, Variant};

/// Names the base folder a writer process places the files under.
const WRITER_BASE: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_BASE";
/// Names the file whose creation lets a writer process begin placing.
const WRITER_START: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_START";
/// Set, a writer process fails at once.
const WRITER_FAILS: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_FAILS";
/// How many writer processes run at once.
const WRITERS: usize = 4;
/// How long each writer keeps placing the files.
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
/// the files under the base folder it is given, again and again for
/// [`WRITING`]. It prints when it entered, when it began and ended placing,
/// and how many times it placed them.
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
    let waiting = Instant::now();
    while !start.exists() {
        assert!(waiting.elapsed() < LIMIT, "no start file within {LIMIT:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    let files = files();
    let began = unix_us();
    let until = Instant::now() + WRITING;
    let mut passes = 0u32;
    while Instant::now() < until {
        folder_in(&base, "shared", &files);
        passes += 1;
    }
    println!("writer entered {entered}");
    println!("writer placed {began} {}", unix_us());
    println!("writer passes {passes}");
}

fn spawn_writer(base: &Path, start: &Path, fails: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    if fails {
        command.env(WRITER_FAILS, "1");
    }
    command
        .args(["writer", "--exact", "--ignored", "--nocapture"])
        .env(WRITER_BASE, base)
        .env(WRITER_START, start)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// What one writer process reported, its times in [`unix_us`].
struct Wrote {
    entered: u64,
    began: u64,
    ended: u64,
    passes: u32,
}

impl Wrote {
    fn parse(out: &str) -> Self {
        let field = |key: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("a writer did not report `{key}`: {out}"))
                .trim()
                .to_owned()
        };
        let placed = field("writer placed ");
        let (began, ended) = placed.split_once(' ').unwrap();
        Self {
            entered: field("writer entered ").parse().unwrap(),
            began: began.parse().unwrap(),
            ended: ended.parse().unwrap(),
            passes: field("writer passes ").parse().unwrap(),
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
    /// When each round that loaded began.
    loaded: Vec<u64>,
    /// When the reader reached the start barrier, and when it was let go.
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
    /// Every reader and the race meet here before the writers start.
    ready: Barrier,
    /// The folder, set once the race placed it; then the readers go.
    dir: OnceLock<PathBuf>,
    go: AtomicBool,
    /// The writers ended.
    done: AtomicBool,
}

/// Meets the race at the start barrier, waits to be let go, then reads the
/// folder round after round until the writers end, and at least once.
fn read_until(reading: Reading, signals: &Signals) -> Seen {
    let mut seen = Seen {
        ready: unix_us(),
        ..Seen::default()
    };
    signals.ready.wait();
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
        seen.longest = seen.longest.max(round.elapsed());
        match failed {
            None => seen.loaded.push(at),
            Some((true, _)) => seen.missing += 1,
            Some((false, e)) => seen.others.push(e),
        }
    }
    seen
}

/// Waits for every writer, killing them past [`LIMIT`]; returns each
/// one's output.
fn wait_writers(mut writers: Vec<Child>, start: Instant) -> Vec<String> {
    let mut outputs = Vec::new();
    while !writers.is_empty() {
        if start.elapsed() > LIMIT {
            for child in &mut writers {
                let _ = child.kill();
            }
            panic!("the writers did not end within {LIMIT:?}");
        }
        let mut i = 0;
        while i < writers.len() {
            if writers[i].try_wait().unwrap().is_some() {
                let mut child = writers.swap_remove(i);
                let status = child.wait().unwrap();
                let mut out = String::new();
                child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut out)
                    .unwrap();
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut out)
                    .unwrap();
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
/// spawned the writers and wrote their start file, in [`unix_us`].
struct Raced {
    outputs: Vec<String>,
    seen: Vec<Seen>,
    spawned: u64,
    started: u64,
}

/// Runs [`WRITERS`] writer processes (failing ones with `fails`) against
/// the reader threads until the writers end. The readers are ready before
/// any writer places: the writers wait for a start file, written once every
/// reader has reached the start barrier. The race then places the folder
/// itself, its first placements racing the writers' to create the files,
/// and lets the readers go.
fn race(base: &Path, fails: bool) -> Raced {
    let start = Instant::now();
    let start_file = base.join("start");
    let spawned = unix_us();
    let writers = (0..WRITERS)
        .map(|_| spawn_writer(base, &start_file, fails))
        .collect();
    let readings = [
        (Reading::Files, FILE_READERS),
        (Reading::Sheet, SHEET_READERS),
        (Reading::Atlas, ATLAS_READERS),
    ];
    let signals = Signals {
        ready: Barrier::new(FILE_READERS + SHEET_READERS + ATLAS_READERS + 1),
        dir: OnceLock::new(),
        go: AtomicBool::new(false),
        done: AtomicBool::new(false),
    };
    let mut started = 0;
    let (outputs, seen) = std::thread::scope(|s| {
        let readers: Vec<_> = readings
            .into_iter()
            .flat_map(|(reading, n)| std::iter::repeat_n(reading, n))
            .map(|reading| {
                let signals = &signals;
                s.spawn(move || read_until(reading, signals))
            })
            .collect();
        signals.ready.wait();
        // The readers stop even when a writer failed, or the scope would
        // wait for them for good.
        let outputs = std::panic::catch_unwind(AssertUnwindSafe(|| {
            std::fs::write(&start_file, b"").unwrap();
            started = unix_us();
            let dir = folder_in(base, "shared", &files());
            signals.dir.set(dir).unwrap();
            signals.go.store(true, Ordering::Release);
            wait_writers(writers, start)
        }));
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
    }
}

/// A writer that fails fails the race, and the readers stop: the race does
/// not wait for them for good.
#[test]
fn a_failed_writer_ends_the_race() {
    let base = base();
    let (sent, ended) = std::sync::mpsc::channel();
    let path = base.path().to_owned();
    std::thread::spawn(move || {
        let raced = std::panic::catch_unwind(|| race(&path, true));
        let payload = raced
            .err()
            .map(|p| p.downcast_ref::<String>().cloned().unwrap_or_default());
        sent.send(payload).unwrap();
    });
    let payload = ended
        .recv_timeout(LIMIT)
        .expect("the race did not end after its writers failed");
    let message = payload.expect("the race passed with failing writers");
    assert!(
        message.contains("a writer failed") && message.contains("this writer fails"),
        "{message}"
    );
}

/// On Windows a file being replaced is absent, or refuses to open, only in
/// a short window of the replacement, so a placement that replaced files
/// fails this in some runs, not every: [`placing_again_keeps_each_file`]
/// checks in every run that a placement replaces nothing. Only reads while
/// the writers place count: each reader must load the folder in a round
/// begun between the first writer's start of placing and the last one's
/// end.
#[test]
fn a_reader_never_meets_a_file_being_placed() {
    let base = base();
    let begun = Instant::now();
    let raced = race(base.path(), false);
    let wrote: Vec<Wrote> = raced.outputs.iter().map(|o| Wrote::parse(o)).collect();
    let first = wrote.iter().map(|w| w.began).min().unwrap();
    let last = wrote.iter().map(|w| w.ended).max().unwrap();
    let seen = &raced.seen;
    // Milliseconds from `from` to `to`, both in unix_us.
    let ms = |from: u64, to: u64| (i128::from(to) - i128::from(from)) / 1_000;
    let count =
        |s: &Seen, keep: &dyn Fn(u64) -> bool| s.loaded.iter().filter(|&&t| keep(t)).count();

    let passes: Vec<u32> = wrote.iter().map(|w| w.passes).collect();
    let before: Vec<usize> = seen.iter().map(|s| count(s, &|t| t < first)).collect();
    let during: Vec<usize> = seen
        .iter()
        .map(|s| count(s, &|t| (first..last).contains(&t)))
        .collect();
    let after: Vec<usize> = seen.iter().map(|s| count(s, &|t| t >= last)).collect();
    let longest: Vec<u128> = seen.iter().map(|s| s.longest.as_millis()).collect();
    let missing: u32 = seen.iter().map(|s| s.missing).sum();
    let others: Vec<&String> = seen.iter().flat_map(|s| &s.others).collect();
    let from_spawn = |times: &mut dyn Iterator<Item = u64>| {
        times.map(|t| ms(raced.spawned, t)).collect::<Vec<_>>()
    };
    println!(
        "passes {passes:?}, loads before {before:?} during {during:?} after {after:?} the \
         writers placed, missing {missing}, other errors {}, longest rounds {longest:?} ms; \
         ms after the spawn: writers entered {:?}, start file {}, writers began {:?} and \
         ended {:?}, readers ready {:?} and let go {:?}; in {} ms",
        others.len(),
        from_spawn(&mut wrote.iter().map(|w| w.entered)),
        ms(raced.spawned, raced.started),
        from_spawn(&mut wrote.iter().map(|w| w.began)),
        from_spawn(&mut wrote.iter().map(|w| w.ended)),
        from_spawn(&mut seen.iter().map(|s| s.ready)),
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
    // them while the writers placed them, so the race ran.
    assert!(passes.iter().all(|&p| p > 0), "passes {passes:?}");
    assert!(
        during.iter().all(|&d| d > 0),
        "a reader loaded nothing while the writers placed: loads before {before:?} during \
         {during:?} after {after:?}"
    );
}
