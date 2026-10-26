//! The log file (design.md "Log File"): `logs\tapestry.log` under the data
//! root, one per run, the last two runs kept as `tapestry.1.log` and
//! `tapestry.2.log`.
//!
//! It is best effort. A failed open prints one `log:` line and leaves
//! logging off; a failed write turns it off silently. Neither reaches the
//! command's outcome or exit code. A run that finds the log leased by
//! another running process leaves it alone and logs nothing.
//!
//! Only this crate's records at Info and above are written: every other
//! record is rejected before the lock, so the render thread and the decode
//! pool, whose crates log, never wait on it or format for it. Loading and
//! lifecycle code logs; the in-play files never name the facade (`cargo
//! xtask check-lints`).

use std::fmt::{self, Display, Write as _};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Files kept: this run's and the two before it.
const KEPT: usize = 3;
/// The most a file holds, the exit line included.
const FILE_CAP: u64 = 1 << 20;
/// The most one record's line holds, its newline included.
const LINE_CAP: usize = 1 << 10;
/// Bytes kept back for the exit line.
const RESERVE: u64 = 256;
/// Warnings logged per source and load; the rest are a count.
const WARNINGS_LOGGED: usize = 100;
/// Missing sound files named on stderr; the rest are a count.
const MISSING_NAMED: usize = 3;
/// The target of this crate's records: the binary's crate name.
const TARGET: &str = env!("CARGO_CRATE_NAME");
/// The lease file, never rotated or removed.
const LOCK: &str = "tapestry.lock";

/// The global logger, empty until [`start`] fills it.
static LOG: FileLog<File> = FileLog::new();

/// Starts logging into `dir` for `command`, once per process. On a failed
/// open it prints one `log:` line on stderr and logging stays off; if
/// another process holds the lease, logging stays off silently.
pub fn start(dir: &Path, command: &str) {
    let Some(logged) = open(dir, command, &mut io::stderr().lock()) else {
        return;
    };
    if let Ok(mut state) = LOG.state.lock() {
        *state = Some(logged);
    }
    if log::set_logger(&LOG).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

/// Writes the exit line with `code` into the room kept for it, if logging
/// is on.
pub fn finish(code: u8) {
    LOG.finish(code);
}

/// Prints every warning on stderr as `warning: <w>`, as the command always
/// has, and logs the first [`WARNINGS_LOGGED`] as `<what>: <w>`, then how
/// many more there were.
pub fn warnings(what: &str, items: impl ExactSizeIterator<Item = impl Display>) {
    warnings_to(what, items, &mut io::stderr().lock(), log::logger());
}

/// Sums up a chart's missing sound files in one line: on stderr the count
/// and the first [`MISSING_NAMED`] names, in the log the count alone, so
/// the chart's file names stay off the disk. Nothing when none is missing.
pub fn missing_sounds<'a>(names: impl ExactSizeIterator<Item = &'a str>) {
    missing_sounds_to(names, &mut io::stderr().lock(), log::logger());
}

fn missing_sounds_to<'a>(
    names: impl ExactSizeIterator<Item = &'a str>,
    err: &mut impl Write,
    log: &dyn Log,
) {
    let n = names.len();
    if n == 0 {
        return;
    }
    let count = if n == 1 {
        "1 sound file is missing".to_owned()
    } else {
        format!("{n} sound files are missing")
    };
    let shown: Vec<&str> = names.take(MISSING_NAMED).collect();
    let more = n - shown.len();
    let rest = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    let _ = writeln!(err, "warning: {count}: {}{rest}", shown.join(", "));
    log.log(&own(Level::Warn, format_args!("chart: {count}")));
}

fn warnings_to(
    what: &str,
    items: impl ExactSizeIterator<Item = impl Display>,
    err: &mut impl Write,
    log: &dyn Log,
) {
    let n = items.len();
    for (k, w) in items.enumerate() {
        let _ = writeln!(err, "warning: {w}");
        if k < WARNINGS_LOGGED {
            log.log(&own(Level::Warn, format_args!("{what}: {w}")));
        }
    }
    if n > WARNINGS_LOGGED {
        let more = n - WARNINGS_LOGGED;
        let args = format_args!("{what}: {more} more warnings not logged ({n} in all)");
        log.log(&own(Level::Warn, args));
    }
}

/// A record of this crate's target.
fn own<'a>(level: Level, args: fmt::Arguments<'a>) -> Record<'a> {
    Record::builder()
        .level(level)
        .target(TARGET)
        .args(args)
        .build()
}

/// Whether a record is written: this crate's, at Info or above.
fn taken(m: &Metadata<'_>) -> bool {
    m.level() <= Level::Info
        && (m.target() == TARGET
            || m.target()
                .strip_prefix(TARGET)
                .is_some_and(|rest| rest.starts_with("::")))
}

/// A logger over one [`Logged`] sink, empty until one is put in.
struct FileLog<W> {
    state: Mutex<Option<Logged<W>>>,
}

impl<W> FileLog<W> {
    const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }
}

impl<W: Write> FileLog<W> {
    /// The sink's exit line; the lease stays held.
    fn finish(&self, code: u8) {
        if let Ok(mut state) = self.state.lock()
            && let Some(logged) = state.as_mut()
        {
            logged.sink.finish(code);
        }
    }
}

impl<W: Write + Send> Log for FileLog<W> {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        taken(m)
    }

    fn log(&self, record: &Record<'_>) {
        // Before the lock: a rejected record takes no lock and is never
        // formatted.
        if !taken(record.metadata()) {
            return;
        }
        // A poisoned lock is skipped: the log is best effort.
        if let Ok(mut state) = self.state.lock()
            && let Some(logged) = state.as_mut()
        {
            logged.sink.record(record.level(), *record.args());
        }
    }

    fn flush(&self) {}
}

/// A run's open log: the sink and the lease that keeps other runs from
/// rotating or truncating it, held for the life of the process.
struct Logged<W> {
    sink: Sink<W>,
    _lease: Option<File>,
}

/// Opens this run's log in `dir`: takes the lease, rotates the older runs'
/// files, creates `tapestry.log` and writes the run's first line. A failure
/// prints one `log: <path>: <error>` line on `diag`, whose own failure is
/// ignored; a lease another process holds gives `None` silently.
fn open(dir: &Path, command: &str, diag: &mut impl Write) -> Option<Logged<File>> {
    let failed = |diag: &mut dyn Write, path: &Path, e: &dyn Display| {
        let _ = writeln!(diag, "log: {}: {e}", path.display());
    };
    if let Err(e) = fs::create_dir_all(dir) {
        failed(diag, dir, &e);
        return None;
    }
    let lock = dir.join(LOCK);
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock);
    let lease = match lease {
        Ok(f) => f,
        Err(e) => {
            failed(diag, &lock, &e);
            return None;
        }
    };
    match lease.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return None,
        Err(TryLockError::Error(e)) => {
            failed(diag, &lock, &e);
            return None;
        }
    }
    // `rename` replaces an existing target, so no more than `KEPT` files
    // remain. A failed rename leaves that file where it is.
    for n in (1..KEPT).rev() {
        let _ = fs::rename(dir.join(name(n - 1)), dir.join(name(n)));
    }
    let path = dir.join(name(0));
    let file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            failed(diag, &path, &e);
            return None;
        }
    };
    let mut sink = Sink::new(file, Caps::FILE, Instant::now());
    let build = if cfg!(feature = "dev-tools") {
        "development"
    } else {
        "production"
    };
    let unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    sink.record(
        Level::Info,
        format_args!(
            "tapestry {} ({build}), command {command}, unix {unix}",
            env!("CARGO_PKG_VERSION")
        ),
    );
    Some(Logged {
        sink,
        _lease: Some(lease),
    })
}

/// The file name of the run `n` runs back.
fn name(n: usize) -> String {
    if n == 0 {
        "tapestry.log".to_owned()
    } else {
        format!("tapestry.{n}.log")
    }
}

/// A sink's bounds.
#[derive(Clone, Copy, Debug)]
struct Caps {
    /// The most the output holds, the exit line included.
    file: u64,
    /// The most one line holds, its newline included.
    line: usize,
    /// Bytes of `file` kept back for the exit line.
    reserve: u64,
}

impl Caps {
    const FILE: Self = Self {
        file: FILE_CAP,
        line: LINE_CAP,
        reserve: RESERVE,
    };
}

/// Lines into one writer, within [`Caps`]: a record that would pass
/// `file - reserve` is dropped and counted, and the exit line goes into the
/// reserve with that count. The first failed write turns the writer off.
struct Sink<W> {
    out: Option<W>,
    caps: Caps,
    started: Instant,
    /// Bytes written.
    written: u64,
    /// Records dropped at the size cap.
    dropped: u64,
    /// The line being built, reused.
    line: String,
    /// Records formatted, for the tests that check a rejected record is not.
    #[cfg(test)]
    formatted: u64,
}

impl<W: Write> Sink<W> {
    fn new(out: W, caps: Caps, started: Instant) -> Self {
        Self {
            out: Some(out),
            caps,
            started,
            written: 0,
            dropped: 0,
            line: String::with_capacity(caps.line),
            #[cfg(test)]
            formatted: 0,
        }
    }

    /// Writes one record, unbuffered, if it fits before the reserve.
    fn record(&mut self, level: Level, args: fmt::Arguments<'_>) {
        if self.out.is_none() {
            return;
        }
        self.format(level, args);
        let len = self.line.len() as u64;
        if self.written + len > self.caps.file.saturating_sub(self.caps.reserve) {
            self.dropped += 1;
            return;
        }
        self.emit();
    }

    /// Writes the exit line into the reserve, then turns the writer off,
    /// so it is written once.
    fn finish(&mut self, code: u8) {
        if self.out.is_none() {
            return;
        }
        let dropped = self.dropped;
        if dropped > 0 {
            self.format(
                Level::Info,
                format_args!("exit {code} ({dropped} records dropped at the size cap)"),
            );
        } else {
            self.format(Level::Info, format_args!("exit {code}"));
        }
        if self.written + self.line.len() as u64 <= self.caps.file {
            self.emit();
        }
        self.out = None;
    }

    /// `line` becomes `<ms> <LEVEL> <message>\n`, the message on one line
    /// and cut at a char boundary so the whole stays within `caps.line`.
    fn format(&mut self, level: Level, args: fmt::Arguments<'_>) {
        #[cfg(test)]
        {
            self.formatted += 1;
        }
        self.line.clear();
        let ms = self.started.elapsed().as_millis();
        let _ = write!(self.line, "{ms:>8} {:<5} ", level.as_str());
        let limit = self.caps.line.saturating_sub(1);
        // A cut ends the formatting with an error, which only says so.
        let _ = Cut {
            line: &mut self.line,
            limit,
        }
        .write_fmt(args);
        self.line.push('\n');
    }

    fn emit(&mut self) {
        if let Some(out) = &mut self.out {
            match out.write_all(self.line.as_bytes()) {
                Ok(()) => self.written += self.line.len() as u64,
                Err(_) => self.out = None,
            }
        }
    }
}

/// Appends to `line` up to `limit` bytes, CR and LF as spaces, and fails at
/// the first char that does not fit.
struct Cut<'a> {
    line: &'a mut String,
    limit: usize,
}

impl fmt::Write for Cut<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            let c = if c == '\r' || c == '\n' { ' ' } else { c };
            if self.line.len() + c.len_utf8() > self.limit {
                return Err(fmt::Error);
            }
            self.line.push(c);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions, TryLockError};
    use std::io::{self, Write};
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use log::{Level, Log, Metadata, Record};

    use super::{
        Caps, FileLog, LINE_CAP, LOCK, Logged, Sink, WARNINGS_LOGGED, missing_sounds_to, name,
        open, taken, warnings_to,
    };

    fn sink<W: Write>(out: W, caps: Caps) -> Sink<W> {
        Sink::new(out, caps, Instant::now())
    }

    /// The message of each line, after the time and the level (15 bytes).
    fn messages(text: &str) -> Vec<&str> {
        text.lines().map(|l| &l[15..]).collect()
    }

    /// A line of 24 message bytes is 40 bytes with the prefix and the
    /// newline, so 3 fit before a reserve of 60 in 200 and the 58-byte
    /// exit line fits in the reserve. Without the reserve, 5 would fill all
    /// 200 and leave no room for the exit line.
    #[test]
    fn records_past_the_size_cap_are_dropped_and_counted() {
        let caps = Caps {
            file: 200,
            line: LINE_CAP,
            reserve: 60,
        };
        let mut out = Vec::new();
        let mut s = sink(&mut out, caps);
        let record = "x".repeat(24);
        for _ in 0..6 {
            s.record(Level::Info, format_args!("{record}"));
        }
        s.finish(1);
        // Once: the writer is off after the exit line.
        s.finish(1);
        s.record(Level::Info, format_args!("late"));
        drop(s);
        let text = String::from_utf8(out).unwrap();
        assert!(text.len() <= 200, "{}", text.len());
        assert_eq!(
            messages(&text),
            [
                record.as_str(),
                &record,
                &record,
                "exit 1 (3 records dropped at the size cap)"
            ],
            "{text}"
        );
        // Without drops, the exit line has no count, and with room to
        // spare it is still written once.
        let mut out = Vec::new();
        let mut s = sink(&mut out, caps);
        s.record(Level::Info, format_args!("one"));
        s.finish(0);
        s.finish(0);
        s.record(Level::Info, format_args!("late"));
        drop(s);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(messages(&text), ["one", "exit 0"], "{text}");
        // A reserve too small for the exit line leaves it out rather than
        // pass the cap: 4 records fit before 190, and the exit line would
        // end at 218.
        let small = Caps {
            reserve: 10,
            ..caps
        };
        let mut out = Vec::new();
        let mut s = sink(&mut out, small);
        for _ in 0..6 {
            s.record(Level::Info, format_args!("{record}"));
        }
        s.finish(1);
        drop(s);
        assert_eq!(out.len(), 160);
    }

    /// One record is one line within the line cap: CR and LF become
    /// spaces, and a char that crosses the cut is left out whole.
    #[test]
    fn a_record_is_one_line_of_at_most_line_cap() {
        let caps = Caps {
            file: 1 << 20,
            line: 32,
            reserve: 0,
        };
        // 15 prefix bytes and the newline leave 16 for the message.
        for (message, kept) in [
            ("a\nb", "a b"),
            ("a\rb", "a b"),
            ("a\r\nb", "a  b"),
            ("aaaaaaaaaaaaaaa\u{e9}", "aaaaaaaaaaaaaaa"),
            ("aaaaaaaaaaaaaa\u{e9}x", "aaaaaaaaaaaaaa\u{e9}"),
        ] {
            let mut out = Vec::new();
            sink(&mut out, caps).record(Level::Warn, format_args!("{message}"));
            let text = String::from_utf8(out).unwrap();
            assert!(text.len() <= 32, "{text:?}");
            assert_eq!(text.matches('\n').count(), 1, "{text:?}");
            assert!(text.ends_with('\n'), "{text:?}");
            assert_eq!(messages(&text), [kept], "{text:?}");
        }
        // The real cap, with a 5 KiB message of lines.
        let mut out = Vec::new();
        let long = "line\n".repeat(1024);
        sink(&mut out, Caps::FILE).record(Level::Info, format_args!("{long}"));
        assert!(out.len() <= LINE_CAP, "{}", out.len());
        assert_eq!(out.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(out.last(), Some(&b'\n'));
    }

    /// A writer that follows a script of results, one per `write` call,
    /// and counts the calls.
    struct Scripted {
        script: &'static [Step],
        calls: usize,
    }

    #[derive(Clone, Copy)]
    enum Step {
        All,
        Part,
        Fail,
    }

    impl Scripted {
        fn new(script: &'static [Step]) -> Self {
            Self { script, calls: 0 }
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let step = self.script.get(self.calls).copied().unwrap_or(Step::All);
            self.calls += 1;
            match step {
                Step::All => Ok(buf.len()),
                Step::Part => Ok(buf.len().min(3)),
                Step::Fail => Err(io::Error::other("disk full")),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The first failed write turns the writer off: no later record or
    /// exit line is attempted, and nothing panics or returns an error.
    #[test]
    fn a_failing_writer_turns_the_sink_off() {
        // An immediate error, and a partial write followed by one.
        for (script, calls) in [(&[Step::Fail][..], 1), (&[Step::Part, Step::Fail], 2)] {
            let mut out = Scripted::new(script);
            let mut s = sink(&mut out, Caps::FILE);
            s.record(Level::Info, format_args!("first"));
            s.record(Level::Info, format_args!("second"));
            s.finish(0);
            assert_eq!(s.written, 0);
            drop(s);
            assert_eq!(out.calls, calls);
        }
        // A failure while writing the exit line.
        let mut out = Scripted::new(&[Step::All, Step::Fail]);
        let mut s = sink(&mut out, Caps::FILE);
        s.record(Level::Info, format_args!("first"));
        s.finish(0);
        s.finish(0);
        s.record(Level::Info, format_args!("late"));
        drop(s);
        assert_eq!(out.calls, 2);
    }

    fn metadata(target: &str, level: Level) -> Metadata<'_> {
        Metadata::builder().target(target).level(level).build()
    }

    #[test]
    fn only_own_records_at_info_are_taken() {
        let all = [Level::Error, Level::Warn, Level::Info];
        for target in [
            "wgpu_core::device",
            "symphonia_core",
            "tapestry_x",
            "x::tapestry",
        ] {
            for level in all {
                assert!(!taken(&metadata(target, level)), "{target} {level}");
            }
        }
        for target in ["tapestry", "tapestry::play"] {
            for level in all {
                assert!(taken(&metadata(target, level)), "{target} {level}");
            }
            for level in [Level::Debug, Level::Trace] {
                assert!(!taken(&metadata(target, level)), "{target} {level}");
            }
        }
    }

    fn record_of<'a>(target: &'a str, level: Level, args: std::fmt::Arguments<'a>) -> Record<'a> {
        Record::builder()
            .target(target)
            .level(level)
            .args(args)
            .build()
    }

    /// A rejected record returns without the lock and is never formatted:
    /// with the sink's mutex held here, another thread's rejected records
    /// (another crate's Warn, this crate's Debug) still return.
    #[test]
    fn a_rejected_record_takes_no_lock() {
        let log = FileLog::<Vec<u8>>::new();
        *log.state.lock().unwrap() = Some(Logged {
            sink: sink(Vec::new(), Caps::FILE),
            _lease: None,
        });
        let guard = log.state.lock().unwrap();
        let returned = std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let worker = scope.spawn(|| {
                let tx = tx;
                for (target, level) in [
                    ("wgpu_core::device", Level::Warn),
                    ("tapestry", Level::Debug),
                ] {
                    log.log(&record_of(target, level, format_args!("x")));
                }
                let _ = tx.send(());
            });
            let returned = rx.recv_timeout(Duration::from_secs(10)).is_ok();
            // Released on the timeout path too, so the join returns.
            drop(guard);
            worker.join().unwrap();
            returned
        });
        assert!(returned, "a rejected record waited on the lock");
        let formatted =
            |log: &FileLog<Vec<u8>>| log.state.lock().unwrap().as_ref().unwrap().sink.formatted;
        assert_eq!(formatted(&log), 0);
        // The positive control: a record of this crate is formatted.
        log.log(&record_of("tapestry::play", Level::Info, format_args!("x")));
        assert_eq!(formatted(&log), 1);
    }

    /// A logger that keeps each record's message.
    #[derive(Default)]
    struct Kept(std::sync::Mutex<Vec<String>>);

    impl Log for Kept {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &Record<'_>) {
            assert_eq!((record.target(), record.level()), ("tapestry", Level::Warn));
            self.0.lock().unwrap().push(record.args().to_string());
        }

        fn flush(&self) {}
    }

    #[test]
    fn warnings_past_the_cap_are_a_count() {
        let kept = Kept::default();
        let mut err = Vec::new();
        warnings_to("chart", (0..150).map(|i| format!("w{i}")), &mut err, &kept);
        // Every warning on stderr, as before.
        let err = String::from_utf8(err).unwrap();
        assert_eq!(err.lines().count(), 150);
        assert_eq!(err.lines().last(), Some("warning: w149"));
        let logged = kept.0.into_inner().unwrap();
        assert_eq!(logged.len(), WARNINGS_LOGGED + 1);
        assert_eq!(logged[99], "chart: w99");
        assert_eq!(
            logged[100],
            "chart: 50 more warnings not logged (150 in all)"
        );
        // Within the cap, no count.
        let kept = Kept::default();
        warnings_to("sounds", ["a", "b"].iter(), &mut Vec::new(), &kept);
        assert_eq!(kept.0.into_inner().unwrap(), ["sounds: a", "sounds: b"]);
    }

    #[test]
    fn missing_sounds_are_one_line_with_names_on_stderr_only() {
        let names = ["kick.wav", "snare.ogg", "hat.wav", "bass.flac", "pad.wav"];
        let kept = Kept::default();
        let mut err = Vec::new();
        missing_sounds_to(names.iter().copied(), &mut err, &kept);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: 5 sound files are missing: kick.wav, snare.ogg, hat.wav and 2 more\n"
        );
        assert_eq!(
            kept.0.into_inner().unwrap(),
            ["chart: 5 sound files are missing"]
        );
        // Up to the named count, no remainder; one file reads as one.
        let kept = Kept::default();
        let mut err = Vec::new();
        missing_sounds_to(["kick.wav"].into_iter(), &mut err, &kept);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: 1 sound file is missing: kick.wav\n"
        );
        assert_eq!(
            kept.0.into_inner().unwrap(),
            ["chart: 1 sound file is missing"]
        );
        let mut err = Vec::new();
        missing_sounds_to(names[..3].iter().copied(), &mut err, &Kept::default());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: 3 sound files are missing: kick.wav, snare.ogg, hat.wav\n"
        );
        // None missing: nothing at all.
        let kept = Kept::default();
        let mut err = Vec::new();
        missing_sounds_to(std::iter::empty(), &mut err, &kept);
        assert!(err.is_empty());
        assert!(kept.0.into_inner().unwrap().is_empty());
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A simulated run: opens the log, writes `run <i>`, ends, and releases
    /// its lease.
    fn run(dir: &Path, i: usize) {
        let mut logged = open(dir, "play", &mut io::sink()).unwrap();
        logged.sink.record(Level::Info, format_args!("run {i}"));
        logged.sink.finish(0);
    }

    #[test]
    fn rotation_keeps_three_files() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        for i in 0..5 {
            run(&logs, i);
        }
        assert_eq!(
            files(&logs),
            [
                "tapestry.1.log",
                "tapestry.2.log",
                "tapestry.lock",
                "tapestry.log"
            ]
        );
        for (n, i) in [(0, 4), (1, 3), (2, 2)] {
            let text = fs::read_to_string(logs.join(name(n))).unwrap();
            let m = messages(&text);
            assert!(m[0].starts_with("tapestry "), "{text}");
            assert!(m[0].contains(", command play, unix "), "{text}");
            assert_eq!(m[1..], [format!("run {i}").as_str(), "exit 0"], "{text}");
        }
    }

    /// While one run holds the lease, a second one neither rotates nor
    /// truncates and says nothing; the first keeps writing.
    #[test]
    fn a_held_lease_leaves_the_log_alone() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        run(&logs, 0);
        let mut first = open(&logs, "play", &mut io::sink()).unwrap();
        first.sink.record(Level::Info, format_args!("before"));
        let mut diag = Vec::new();
        assert!(open(&logs, "keys", &mut diag).is_none());
        assert!(diag.is_empty(), "{diag:?}");
        first.sink.record(Level::Info, format_args!("after"));
        first.sink.finish(0);
        let text = fs::read_to_string(logs.join(name(0))).unwrap();
        let m = messages(&text);
        assert!(m[0].contains("command play"), "{text}");
        assert_eq!(m[1..], ["before", "after", "exit 0"], "{text}");
        // Only the first run's rotation happened.
        let previous = fs::read_to_string(logs.join(name(1))).unwrap();
        assert!(previous.contains("run 0"), "{previous}");
        assert!(!logs.join(name(2)).exists());
        // Released, the next run rotates.
        drop(first);
        run(&logs, 1);
        assert!(logs.join(name(2)).exists());
    }

    /// Whether another handle, opened on its own, finds the lease held.
    fn leased(logs: &Path) -> bool {
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(logs.join(LOCK))
            .unwrap();
        match other.try_lock() {
            Err(TryLockError::WouldBlock) => true,
            Ok(()) => false,
            Err(TryLockError::Error(e)) => panic!("{e}"),
        }
    }

    /// The lease outlives the writer: after a failed write turns it off,
    /// and after the exit line, the process still holds it, until the
    /// logger goes.
    #[test]
    fn the_lease_is_held_after_the_writer_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        // A failed write, through the logger: the file is swapped for a
        // read-only handle, which refuses the record.
        let log = FileLog::<File>::new();
        *log.state.lock().unwrap() = open(&logs, "play", &mut io::sink());
        let read_only = File::open(logs.join(name(0))).unwrap();
        log.state.lock().unwrap().as_mut().unwrap().sink.out = Some(read_only);
        log.log(&record_of("tapestry", Level::Info, format_args!("x")));
        assert!(
            log.state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .sink
                .out
                .is_none()
        );
        assert!(leased(&logs));
        drop(log);
        assert!(!leased(&logs));
        // The exit line, through the logger.
        let log = FileLog::<File>::new();
        *log.state.lock().unwrap() = open(&logs, "play", &mut io::sink());
        log.finish(0);
        assert!(
            log.state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .sink
                .out
                .is_none()
        );
        assert!(leased(&logs));
        drop(log);
        assert!(!leased(&logs));
    }

    /// A `logs` path that is a file is an open failure: one `log:` line and
    /// no logger, also when that line cannot be written.
    #[test]
    fn an_open_failure_is_one_line_and_never_fails() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::write(&logs, "").unwrap();
        let mut diag = Vec::new();
        assert!(open(&logs, "play", &mut diag).is_none());
        let diag = String::from_utf8(diag).unwrap();
        assert_eq!(diag.lines().count(), 1, "{diag}");
        let prefix = format!("log: {}: ", logs.display());
        assert!(diag.starts_with(&prefix), "{diag}");
        assert!(open(&logs, "play", &mut Scripted::new(&[Step::Fail])).is_none());
    }
}
