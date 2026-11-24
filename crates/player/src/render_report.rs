//! The render thread's report lines (design.md "Log File"): a refusal or
//! failure the render thread meets is printed on stderr as it happens and
//! copied to the log by a reporter thread of its own, so the render thread
//! never waits on the log's lock or its file. The render thread formats
//! each line into a fixed record ([`Line`], cut at [`LINE`] bytes) and
//! pushes it onto a preallocated single-producer ring of [`CAPACITY`]
//! records ([`rtrb`], as the measurement's queues are), then wakes the
//! reporter by unparking it: the copy to the log neither waits, locks nor
//! allocates on the render thread. The print on stderr before it takes
//! stderr's own lock and writes, as every print does, and may stall the
//! render thread while stderr is blocked. A line that finds the ring full is dropped and counted,
//! and the count is written before the reporter's next line or as it ends
//! (a lossy non-blocking writer, as tracing-appender's and spdlog's are).
//!
//! The producer is the render thread's own ([`install`]), so any code it
//! runs reports through [`report`] without carrying it; a thread with
//! none only prints. The reporter ends once its producer is dropped with
//! the render thread, and [`Reporter::join`] waits for it within a bound:
//! a sink blocked in a write holds up neither the exit nor the rest of the
//! run.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::cell::RefCell;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

/// The records the ring holds.
pub(crate) const CAPACITY: usize = 64;
/// The most one record holds, in bytes: a longer line is cut.
pub(crate) const LINE: usize = 512;

/// One line, fixed in size, so a push moves it with nothing allocated.
pub(crate) struct Line {
    len: usize,
    bytes: [u8; LINE],
}

impl Line {
    /// `args` formatted, cut at the last char boundary within [`LINE`].
    pub(crate) fn format(args: fmt::Arguments<'_>) -> Self {
        let mut line = Self {
            len: 0,
            bytes: [0; LINE],
        };
        // The cut is the writer's: it never fails.
        let _ = line.write_fmt(args);
        line
    }

    /// The line's text.
    pub(crate) fn as_str(&self) -> &str {
        self.bytes
            .get(..self.len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default()
    }
}

impl fmt::Write for Line {
    /// Copies what fits of `s`, cut at a char boundary; the rest is
    /// dropped, and so is every later piece once one was cut.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = LINE.saturating_sub(self.len);
        let mut take = s.len().min(room);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        if let (Some(to), Some(from)) = (
            self.bytes.get_mut(self.len..self.len + take),
            s.as_bytes().get(..take),
        ) {
            to.copy_from_slice(from);
            self.len += take;
        }
        if take < s.len() {
            // Nothing after a cut: a later short piece must not follow it.
            self.len = LINE.min(self.len);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

/// The render thread's side: the ring's producer, the count of the lines
/// dropped, and the reporter to wake.
pub(crate) struct Producer {
    /// `None` only inside [`Producer`]'s drop, which drops it before it
    /// wakes the reporter.
    ring: Option<rtrb::Producer<Line>>,
    dropped: Arc<AtomicU64>,
    reporter: Thread,
}

impl Producer {
    /// Pushes `args`'s line, or counts it when the ring is full, and wakes
    /// the reporter.
    fn push(&mut self, args: fmt::Arguments<'_>) {
        let Some(ring) = &mut self.ring else {
            return;
        };
        if ring.push(Line::format(args)).is_err() {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
        self.reporter.unpark();
    }
}

impl Drop for Producer {
    /// Abandons the ring, then wakes the reporter, which looks once more
    /// and ends. In this order: a wake before the ring's producer is gone
    /// could find the ring not yet abandoned, and the reporter would park
    /// with nothing left to wake it.
    fn drop(&mut self) {
        drop(self.ring.take());
        self.reporter.unpark();
    }
}

/// The reporter thread, joined within a bound ([`Reporter::join`]).
pub(crate) struct Reporter {
    handle: JoinHandle<()>,
    /// Disconnected when the thread ends, however it ends.
    ended: mpsc::Receiver<()>,
}

impl Reporter {
    /// Waits at most `bound` for the reporter to end, which it does once
    /// its producer is dropped and every line is written; whether it did.
    /// One still writing past the bound is left detached.
    pub(crate) fn join(self, bound: Duration) -> bool {
        match self.ended.recv_timeout(bound) {
            Err(RecvTimeoutError::Timeout) => false,
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = self.handle.join();
                true
            }
        }
    }
}

/// Where a reporter writes each line, on its own thread.
pub(crate) type Sink = Box<dyn FnMut(&str) + Send>;

/// Starts a reporter writing every line to `sink`.
///
/// # Errors
///
/// The thread could not be started.
pub(crate) fn start(mut sink: Sink) -> std::io::Result<(Producer, Reporter)> {
    let (ring, mut lines) = rtrb::RingBuffer::<Line>::new(CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&dropped);
    let (ended_tx, ended) = mpsc::sync_channel::<()>(0);
    let handle = std::thread::Builder::new()
        .name("render report".into())
        .spawn(move || {
            let _ended = ended_tx;
            let owed = |sink: &mut dyn FnMut(&str)| {
                let n = counted.swap(0, Ordering::AcqRel);
                if n > 0 {
                    sink(&format!("render: {n} report lines dropped"));
                }
            };
            loop {
                while let Ok(line) = lines.pop() {
                    owed(&mut *sink);
                    sink(line.as_str());
                }
                // Looked at after the writes, never before them: the sink
                // may park this thread itself (a lock's or a channel's own
                // wait) and take the wake a push or the producer's drop
                // left. Abandoned, the lines pushed before the drop are
                // written, then the reporter ends.
                if lines.is_abandoned() {
                    if lines.is_empty() {
                        break;
                    }
                    continue;
                }
                std::thread::park();
            }
            owed(&mut *sink);
        })?;
    let reporter = handle.thread().clone();
    Ok((
        Producer {
            ring: Some(ring),
            dropped,
            reporter,
        },
        Reporter { handle, ended },
    ))
}

/// Where a thread's report lines go besides stderr.
enum Route {
    /// No reporter was set up for it (the game thread's render-side lines):
    /// the log directly, through [`set_direct`]'s function, if any.
    Direct,
    /// Its reporter ([`install`]): the render thread.
    Reporter(Producer),
    /// Stderr only: the render thread when its reporter could not start,
    /// which never takes the log's lock itself.
    StderrOnly,
}

thread_local! {
    /// This thread's route ([`install`], [`stderr_only`]).
    static ROUTE: RefCell<Route> = const { RefCell::new(Route::Direct) };
}

/// The function that logs a line of a thread with no reporter (the log's
/// own, set once the log is open).
static DIRECT: OnceLock<fn(fmt::Arguments<'_>)> = OnceLock::new();

/// Sets the function that logs the lines of threads with no reporter; the
/// first one set stays.
pub(crate) fn set_direct(log: fn(fmt::Arguments<'_>)) {
    let _ = DIRECT.set(log);
}

/// Makes `producer` this thread's: its reports go to its reporter until
/// the thread ends, which drops it.
pub(crate) fn install(producer: Producer) {
    set_route(Route::Reporter(producer));
}

/// This thread's reports go to stderr only (a render thread whose reporter
/// could not start).
pub(crate) fn stderr_only() {
    set_route(Route::StderrOnly);
}

fn set_route(route: Route) {
    ROUTE.with(|r| {
        if let Ok(mut r) = r.try_borrow_mut() {
            *r = route;
        }
    });
}

/// Prints `args` on stderr (under stderr's lock), and copies it to the
/// log: through this thread's reporter, if it has one, without a lock,
/// else directly unless the thread is stderr-only. Allocates nothing on a
/// thread with a reporter once it is installed.
pub(crate) fn report(args: fmt::Arguments<'_>) {
    eprintln!("{args}");
    let _ = ROUTE.try_with(|r| {
        let Ok(mut r) = r.try_borrow_mut() else {
            return;
        };
        match &mut *r {
            Route::Reporter(p) => p.push(args),
            Route::Direct => {
                if let Some(log) = DIRECT.get() {
                    log(args);
                }
            }
            Route::StderrOnly => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{CAPACITY, LINE, Line, install, report, start};

    /// How long a test waits on the reporter's end: a bound that only
    /// catches a reporter that never ends.
    const BOUND: Duration = Duration::from_secs(60);

    type Seen = Arc<Mutex<Vec<String>>>;

    /// A sink that records each line it writes.
    fn recording() -> (super::Sink, Seen) {
        let seen = Seen::default();
        let kept = Arc::clone(&seen);
        (
            Box::new(move |line| kept.lock().unwrap().push(line.to_owned())),
            seen,
        )
    }

    /// A sink that records each line and holds its first one until
    /// `release` sends, telling `entered` once it holds it.
    fn gated() -> (super::Sink, Seen, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (mut inner, seen) = recording();
        let mut first = true;
        (
            Box::new(move |line| {
                if std::mem::take(&mut first) {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }
                inner(line);
            }),
            seen,
            entered,
            release,
        )
    }

    /// W1.1: the lines a render thread reports reach the sink in order.
    #[test]
    fn a_render_line_reaches_the_sink_in_order() {
        let (sink, seen) = recording();
        let (sender, reporter) = start(sink).unwrap();
        std::thread::spawn(move || {
            install(sender);
            for n in 1..=3 {
                report(format_args!("render: line {n}"));
            }
        })
        .join()
        .unwrap();
        assert!(reporter.join(BOUND), "the reporter did not end");
        assert_eq!(
            *seen.lock().unwrap(),
            ["render: line 1", "render: line 2", "render: line 3"]
        );
    }

    /// W1.3: the reporter ends once its producer is dropped with the
    /// thread that held it, and not before: a join while the producer
    /// lives gives up at its bound.
    #[test]
    fn the_reporter_ends_with_its_producer() {
        let (sink, _seen) = recording();
        let (producer, reporter) = start(sink).unwrap();
        let (drop_tx, drop_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            install(producer);
            drop_rx.recv().unwrap();
        });
        let (joined_tx, joined) = mpsc::channel();
        let joiner = std::thread::spawn(move || {
            joined_tx.send(reporter.join(BOUND)).unwrap();
        });
        // The producer lives: the reporter has not ended.
        assert!(joined.recv_timeout(Duration::from_millis(200)).is_err());
        drop_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(
            joined.recv_timeout(BOUND).unwrap(),
            "the reporter did not end"
        );
        joiner.join().unwrap();
    }

    /// W1.3: a sink that parks its thread itself (a lock's or a channel's
    /// wait may) takes the wake the producer's drop left while the sink
    /// held its line; the reporter still ends, since it looks at the ring
    /// after its writes.
    #[test]
    fn a_sink_that_parks_does_not_lose_the_end() {
        let (release, release_rx) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel::<()>();
        let mut first = true;
        let sink: super::Sink = Box::new(move |_line| {
            if std::mem::take(&mut first) {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(BOUND);
                // Takes the wake the drop left, if the wait above did not.
                std::thread::park_timeout(Duration::from_millis(50));
            }
        });
        let (producer, reporter) = start(sink).unwrap();
        std::thread::spawn(move || {
            install(producer);
            report(format_args!("render: held"));
            entered.recv_timeout(BOUND).unwrap();
        })
        .join()
        .unwrap();
        // The producer was dropped, and its wake left, before the sink
        // goes on.
        release.send(()).unwrap();
        assert!(
            reporter.join(Duration::from_secs(10)),
            "the reporter missed its producer's end"
        );
    }

    /// W1.3: a sink blocked in a write holds the reporter, and the join
    /// gives up at its bound, so the exit goes on; released, the reporter
    /// ends.
    #[test]
    fn a_blocked_sink_does_not_hold_the_exit() {
        let (sink, seen, entered, release) = gated();
        let (producer, reporter) = start(sink).unwrap();
        std::thread::spawn(move || {
            install(producer);
            report(format_args!("render: held"));
            entered.recv_timeout(BOUND).unwrap();
        })
        .join()
        .unwrap();
        let asked = std::time::Instant::now();
        let (joined_tx, joined) = mpsc::channel();
        let joiner = std::thread::spawn(move || {
            let _ = joined_tx.send(reporter.join(Duration::from_millis(200)));
        });
        let gave_up = joined.recv_timeout(BOUND);
        // Released either way, so a join that waits on still ends.
        release.send(()).unwrap();
        assert_eq!(gave_up, Ok(false), "the join waited on a blocked sink");
        assert!(asked.elapsed() < BOUND, "{:?}", asked.elapsed());
        joiner.join().unwrap();
        let begun = std::time::Instant::now();
        while seen.lock().unwrap().is_empty() {
            assert!(begun.elapsed() < BOUND, "the released line never came");
            std::thread::yield_now();
        }
        assert_eq!(*seen.lock().unwrap(), ["render: held"]);
    }

    /// W1.2: while the queue is full a report is dropped, never waited
    /// for; the count of the dropped lines is written before the next line
    /// the reporter writes. The control fills the queue exactly and drops
    /// nothing, so no count is written.
    #[test]
    fn a_full_queue_drops_and_counts() {
        for extra in [0, 3] {
            let (sink, seen, entered, release) = gated();
            let (sender, reporter) = start(sink).unwrap();
            let render = std::thread::spawn(move || {
                install(sender);
                report(format_args!("render: held"));
                // The reporter holds the first line: the queue is empty.
                entered.recv_timeout(BOUND).unwrap();
                for n in 0..CAPACITY + extra {
                    report(format_args!("render: queued {n}"));
                }
                // Every report returned with the reporter still held.
                release.send(()).unwrap();
            });
            render.join().unwrap();
            assert!(reporter.join(BOUND), "the reporter did not end");
            let seen = seen.lock().unwrap();
            let mut expected = vec!["render: held".to_owned()];
            if extra > 0 {
                expected.push(format!("render: {extra} report lines dropped"));
            }
            expected.extend((0..CAPACITY).map(|n| format!("render: queued {n}")));
            assert_eq!(*seen, expected, "{extra} past the capacity");
        }
    }

    /// W1.2: a count still owed when the render thread ends is written
    /// as the reporter ends.
    #[test]
    fn a_count_owed_at_the_end_is_written() {
        let (sink, seen, entered, release) = gated();
        let (sender, reporter) = start(sink).unwrap();
        std::thread::spawn(move || {
            install(sender);
            report(format_args!("render: held"));
            entered.recv_timeout(BOUND).unwrap();
            for n in 0..CAPACITY + 2 {
                report(format_args!("render: queued {n}"));
            }
            release.send(()).unwrap();
        })
        .join()
        .unwrap();
        assert!(reporter.join(BOUND));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), CAPACITY + 2, "{seen:?}");
        assert_eq!(seen[1], "render: 2 report lines dropped");
    }

    /// A line longer than a record is cut at a char boundary, the rest
    /// dropped; a short one is kept whole.
    #[test]
    fn a_long_line_is_cut_at_a_char_boundary() {
        let long = "é".repeat(LINE);
        let line = Line::format(format_args!("render: {long}"));
        let kept = line.as_str();
        assert!(
            kept.len() <= LINE && kept.len() > LINE - 2,
            "{}",
            kept.len()
        );
        assert!(format!("render: {long}").starts_with(kept));
        assert_eq!(
            Line::format(format_args!("render: {}", 7)).as_str(),
            "render: 7"
        );
    }

    /// A thread with no reporter installed (the key config, a test's
    /// screen on its own thread) still prints the line, and nothing fails.
    #[test]
    fn a_report_without_a_reporter_only_prints() {
        std::thread::spawn(|| report(format_args!("render: alone")))
            .join()
            .unwrap();
    }

    /// W1.2: a report allocates nothing on the reporting thread, a dropped
    /// one included, once the thread's first report warmed it up; the
    /// control shows the counter counts a formatted string.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_report_allocates_nothing_alloc() {
        let (sink, _seen, entered, release) = gated();
        let (sender, reporter) = start(sink).unwrap();
        let counted = std::thread::spawn(move || {
            install(sender);
            report(format_args!("render: warm {}", 1));
            entered.recv_timeout(BOUND).unwrap();
            let e = render::RenderError::SurfaceLost;
            crate::alloc_count::room_for_output();
            let before = crate::alloc_count::this_thread();
            // Past the capacity: the last ones are dropped.
            for n in 0..CAPACITY + 4 {
                report(format_args!("render: {n}: {}", game::Chain(&e)));
            }
            let reported = crate::alloc_count::this_thread() - before;
            let before = crate::alloc_count::this_thread();
            let control = format!("render: {}", game::Chain(&e));
            let formatted = crate::alloc_count::this_thread() - before;
            drop(control);
            release.send(()).unwrap();
            (reported, formatted)
        });
        let (reported, formatted) = counted.join().unwrap();
        assert!(reporter.join(BOUND));
        assert_eq!(reported, 0);
        assert!(formatted > 0, "the control counted nothing");
    }
}
