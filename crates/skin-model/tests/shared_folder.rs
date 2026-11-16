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
use std::time::{Duration, Instant, SystemTime};

use image::{Rgba, RgbaImage};
use shared_folder_support::{create, folder_in, png};
use skin_model::{AtlasError, AtlasLimits, FileDef, Filter, ImageDef, ImageError, Sheet, Variant};

/// Names the base folder a writer process places the files under.
const WRITER_BASE: &str = "SKIN_MODEL_SHARED_FOLDER_WRITER_BASE";
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

/// The writer process: places the files under the base folder it is
/// given, again and again for [`WRITING`], and prints how many times.
#[test]
#[ignore = "a writer process of a_reader_never_meets_a_file_being_placed"]
fn writer() {
    assert!(
        std::env::var_os(WRITER_FAILS).is_none(),
        "this writer fails as it was asked"
    );
    let base = PathBuf::from(std::env::var_os(WRITER_BASE).unwrap());
    let files = files();
    let until = Instant::now() + WRITING;
    let mut passes = 0u32;
    while Instant::now() < until {
        folder_in(&base, "shared", &files);
        passes += 1;
    }
    println!("writer passes {passes}");
}

fn spawn_writer(base: &Path, fails: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    if fails {
        command.env(WRITER_FAILS, "1");
    }
    command
        .args(["writer", "--exact", "--ignored", "--nocapture"])
        .env(WRITER_BASE, base)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// What one reader thread saw.
#[derive(Default)]
struct Seen {
    loads: u32,
    missing: u32,
    others: Vec<String>,
    /// The longest round, whatever its outcome.
    longest: Duration,
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

/// Reads `dir` round after round until `done`, and at least once.
fn read_until(dir: &Path, reading: Reading, done: &AtomicBool) -> Seen {
    let mut seen = Seen::default();
    let mut rounds = 0u32;
    while rounds == 0 || !done.load(Ordering::Relaxed) {
        rounds += 1;
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
            None => seen.loads += 1,
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

/// Runs [`WRITERS`] writer processes (failing ones with `fails`) against
/// the reader threads until the writers end; returns each writer's output
/// and what each reader saw.
fn race(base: &Path, fails: bool) -> (Vec<String>, Vec<Seen>) {
    let start = Instant::now();
    let writers = (0..WRITERS).map(|_| spawn_writer(base, fails)).collect();
    // Placed while the writers start, which place it too: the first
    // placements race to create the files.
    let dir = folder_in(base, "shared", &files());
    let done = AtomicBool::new(false);
    let (outputs, seen) = std::thread::scope(|s| {
        let readings = [
            (Reading::Files, FILE_READERS),
            (Reading::Sheet, SHEET_READERS),
            (Reading::Atlas, ATLAS_READERS),
        ];
        let readers: Vec<_> = readings
            .into_iter()
            .flat_map(|(reading, n)| std::iter::repeat_n(reading, n))
            .map(|reading| {
                let (dir, done) = (&dir, &done);
                s.spawn(move || read_until(dir, reading, done))
            })
            .collect();
        // The readers stop even when a writer failed, or the scope would
        // wait for them for good.
        let outputs = std::panic::catch_unwind(AssertUnwindSafe(|| wait_writers(writers, start)));
        done.store(true, Ordering::Relaxed);
        let seen: Vec<Seen> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        (outputs, seen)
    });
    let outputs = outputs.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    (outputs, seen)
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
/// checks in every run that a placement replaces nothing.
#[test]
fn a_reader_never_meets_a_file_being_placed() {
    let base = base();
    let start = Instant::now();
    let (outputs, seen) = race(base.path(), false);

    let passes: Vec<u32> = outputs
        .iter()
        .map(|out| {
            out.lines()
                .find_map(|l| l.strip_prefix("writer passes "))
                .unwrap_or_else(|| panic!("a writer did not report its passes: {out}"))
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    let loads: Vec<u32> = seen.iter().map(|s| s.loads).collect();
    let longest: Vec<u128> = seen.iter().map(|s| s.longest.as_millis()).collect();
    let missing: u32 = seen.iter().map(|s| s.missing).sum();
    let others: Vec<&String> = seen.iter().flat_map(|s| &s.others).collect();
    println!(
        "passes {passes:?}, loads {loads:?}, missing {missing}, other errors {}, \
         longest rounds {longest:?} ms, in {} ms",
        others.len(),
        start.elapsed().as_millis()
    );
    assert_eq!(
        (missing, others.len()),
        (0, 0),
        "loads {loads:?}; first other errors: {:?}",
        &others[..others.len().min(3)]
    );
    // The controls: each writer placed the files and each reader loaded
    // them, so the race ran.
    assert!(passes.iter().all(|&p| p > 0), "passes {passes:?}");
    assert!(loads.iter().all(|&l| l > 0), "loads {loads:?}");
}
