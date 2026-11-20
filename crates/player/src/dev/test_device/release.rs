//! The runner's link thread (docs/test-device.md "Run Guard"): one thread
//! owns the run's [`Connection`] and writes every command, fed by a bounded
//! queue, so the guard and the step thread only enqueue and never wait on a
//! write. Holds stop at one call, [`Writer::stop_holds`]: it closes the
//! gate, after which no hold-bearing command (`Command::holds_input`) is
//! admitted or written, and requests the one `release`. The link thread
//! then records the fence (no hold-bearing write begun before the gate
//! closed is still in progress: its writes are its own and synchronous, so
//! at its loop's top none is) and writes `release`; a hold-bearing command
//! it takes from the queue after the gate closed is dropped, not written.
//! A hold-bearing write is admitted under the gate's lock, which marks it
//! in flight until the port write returns (bounded by the port's write
//! timeout): the gate cannot close between the admission and the write,
//! and nothing writes a file between them, its records written before the
//! admission and after the write.
//! Only a `hello` written after the `release`'s `ok` was read, itself
//! after the fence, counts as release evidence. A device refusal of a
//! hold-bearing command stops holds as a lost check does.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use test_device::clock::Ping;
use test_device::protocol::{Answer, Command, Token};
use test_device::trace::Body;

use super::link::{Connection, Event, Grant, What};
use super::owner::Stream;
use super::win::qpc;

/// How many commands the queue holds; a full queue is a lost check.
pub const QUEUE: usize = 32;
/// How many queued commands one turn of the link thread writes.
const PER_TURN: usize = 8;
/// How often the link thread pings, for the guard's clock samples.
const PING_EVERY: Duration = Duration::from_millis(25);

/// Why a command was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The queue is full: the guard counts it a lost check.
    Full,
    /// Holds have stopped: no hold-bearing command is admitted.
    Closed,
}

/// A queued command: when it was queued, and for a grant or a renewal the
/// guard's check it is derived from.
#[derive(Clone, Copy, Debug)]
struct Item {
    cmd: Command,
    queued: i64,
    check: Option<i64>,
}

/// What the link thread has seen, for the guard and the step thread.
#[derive(Clone, Debug, Default)]
pub struct State {
    pub token: Option<Token>,
    pub grant: Option<Grant>,
    /// The connection's newest round trips.
    pub pings: Vec<Ping>,
    /// The fence is recorded.
    pub fenced: bool,
    /// The `release` was answered `ok`.
    pub released: bool,
    /// The `hello` answers to hellos written after the `release`'s `ok`
    /// was read: when each was read, and its line.
    pub hellos: Vec<(i64, String)>,
    /// The `F` after a `flush`.
    pub flushed: bool,
    /// Every answer read, with the command it answers.
    pub answers: Vec<(Command, String)>,
    /// The link failed: why.
    pub lost: Option<String>,
    /// The link thread has ended, its stream closed by itself: the close's
    /// first write error, if any.
    pub ended: bool,
    pub log_error: Option<String>,
}

/// The gate: why holds stopped, once they have.
#[derive(Debug, Default)]
struct Gate {
    stopped: Option<String>,
}

struct Shared {
    gate: Mutex<Gate>,
    /// Held by the link thread across a hold-bearing command's submission
    /// to the port, and by `stop_holds` while it closes the gate: a hold is
    /// either submitted whole before the stop or never. The queue's
    /// writers take only the gate, so they never wait on a write.
    submit: Mutex<()>,
    queue: Mutex<VecDeque<Item>>,
    state: Mutex<State>,
    changed: Condvar,
    /// The end asked: flush, then end.
    end: AtomicBool,
    log: Stream,
    run: Stream,
    /// A test's point in a hold-bearing write at which holds stop.
    #[cfg(test)]
    probe: Mutex<Option<&'static str>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The link thread's handle.
#[derive(Clone)]
pub struct Writer {
    shared: Arc<Shared>,
}

impl Writer {
    /// Starts the link thread on `conn` (its `hello` checked), recording
    /// to `log` (the link's stream) and `run` (the runner's, for `stop`).
    pub fn start(conn: Connection, log: Stream, run: Stream) -> (Self, JoinHandle<()>) {
        let shared = Arc::new(Shared {
            gate: Mutex::new(Gate::default()),
            submit: Mutex::new(()),
            queue: Mutex::new(VecDeque::new()),
            state: Mutex::new(State {
                token: conn.token(),
                ..State::default()
            }),
            changed: Condvar::new(),
            end: AtomicBool::new(false),
            log,
            run,
            #[cfg(test)]
            probe: Mutex::new(None),
        });
        let w = Self {
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::spawn(move || Thread::new(conn, shared).run());
        (w, thread)
    }

    /// Queues `cmd`; `check` is the guard's good check a grant or renewal
    /// is derived from.
    ///
    /// # Errors
    ///
    /// The queue is full, or holds have stopped and `cmd` holds input.
    pub fn enqueue(&self, cmd: Command, check: Option<i64>) -> Result<(), Refused> {
        // The gate's lock held while queueing: a hold-bearing command is
        // queued only while holds last.
        let gate = lock(&self.shared.gate);
        if cmd.holds_input() && gate.stopped.is_some() {
            return Err(Refused::Closed);
        }
        let mut q = lock(&self.shared.queue);
        if q.len() >= QUEUE {
            return Err(Refused::Full);
        }
        q.push_back(Item {
            cmd,
            queued: qpc(),
            check,
        });
        Ok(())
    }

    /// Stops holds, once: closes the gate, records the cause, and requests
    /// the one `release` the link thread writes after the fence. A later
    /// call adds nothing.
    pub fn stop_holds(&self, cause: &str) {
        stop_holds(&self.shared, cause);
    }

    /// Whether holds have stopped, and why.
    pub fn stopped(&self) -> Option<String> {
        lock(&self.shared.gate).stopped.clone()
    }

    /// A copy of what the link thread has seen.
    pub fn state(&self) -> State {
        lock(&self.shared.state).clone()
    }

    /// `f` of what the link thread has seen, without a copy.
    pub fn with_state<T>(&self, f: impl FnOnce(&State) -> T) -> T {
        f(&lock(&self.shared.state))
    }

    /// Waits until `done` holds of the state or `wait` passes, calling
    /// `beat` at least every 5 ms: whether it holds.
    pub fn wait(&self, wait: Duration, beat: &dyn Fn(), done: impl Fn(&State) -> bool) -> bool {
        let end = Instant::now() + wait;
        let mut g = lock(&self.shared.state);
        loop {
            if done(&g) {
                return true;
            }
            let now = Instant::now();
            if now >= end || g.ended {
                return done(&g);
            }
            beat();
            let step = (end - now).min(Duration::from_millis(5));
            g = self
                .shared
                .changed
                .wait_timeout(g, step)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Asks the link thread to end: it writes what is queued, then `flush`
    /// and waits for its `F`, and closes the port.
    pub fn end(&self) {
        self.shared.end.store(true, Ordering::Release);
    }
}

fn stop_holds(shared: &Shared, cause: &str) {
    {
        // Waits for a submission in progress (the port's write is bounded
        // by its timeout), so none is cut by the stop.
        let _submit = lock(&shared.submit);
        let mut g = lock(&shared.gate);
        if g.stopped.is_some() {
            return;
        }
        g.stopped = Some(cause.to_owned());
    }
    shared
        .run
        .write(&json!({ "t": "stop", "q": qpc(), "what": cause }));
}

/// How long the end waits for `flush`'s `F`.
const FLUSH_WAIT: Duration = Duration::from_secs(3);

struct Thread {
    conn: Connection,
    shared: Arc<Shared>,
    released: bool,
    /// When the `release`'s `ok` was read (host counter).
    release_ok: Option<i64>,
    next_ping: Instant,
    ping: u32,
    flushing: Option<Instant>,
    problems: Vec<String>,
}

impl Thread {
    fn new(conn: Connection, shared: Arc<Shared>) -> Self {
        Self {
            conn,
            shared,
            released: false,
            release_ok: None,
            next_ping: Instant::now(),
            ping: 0,
            flushing: None,
            problems: Vec::new(),
        }
    }

    /// Writes the connection's records, then `v`.
    fn record(&mut self, v: Option<Value>) {
        for j in self.conn.take_journal() {
            self.shared.log.write(&j);
        }
        if let Some(v) = v {
            self.shared.log.write(&v);
        }
    }

    /// A point in a hold-bearing write: a test may stop holds there, as
    /// another thread could at that moment.
    #[cfg(test)]
    fn probe(&self, at: &'static str) {
        if *lock(&self.shared.probe) == Some(at) {
            stop_holds(&self.shared, at);
        }
    }

    #[cfg(not(test))]
    #[expect(
        clippy::unused_self,
        reason = "a test's probe point, empty outside tests"
    )]
    const fn probe(&self, _: &'static str) {}

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut lock(&self.shared.state));
        self.shared.changed.notify_all();
    }

    /// Writes `cmd` and its `cmd` record with when it was queued and when
    /// its write ended.
    fn send(&mut self, cmd: Command, queued: i64) -> anyhow::Result<()> {
        self.record(None);
        let sent = self.conn.send(cmd);
        let done = qpc();
        self.journal_sent(queued, done);
        sent.map(drop)
    }

    fn run(mut self) {
        let result = self.turns();
        if let Err(e) = &result {
            let why = format!("{e:#}");
            stop_holds(&self.shared, &format!("the link failed: {why}"));
            self.problems.push(why.clone());
            self.update(|s| s.lost = Some(why));
        }
        self.record(Some(
            json!({ "t": "end", "q": qpc(), "problems": self.problems }),
        ));
        // The stream is this thread's: it closes it, so no other thread
        // waits on its lock.
        let log_error = self.shared.log.close();
        self.update(|s| {
            s.log_error = log_error;
            s.ended = true;
        });
    }

    fn turns(&mut self) -> anyhow::Result<()> {
        loop {
            // The fence and the one release: no hold-bearing write is in
            // progress at the loop's top, the writes being this thread's,
            // and none is submitted after the gate closed.
            let fence = lock(&self.shared.gate).stopped.is_some();
            if !self.released && fence {
                self.record(Some(json!({ "t": "fence", "q": qpc() })));
                self.update(|s| s.fenced = true);
                self.released = true;
                self.send(Command::Release, qpc())?;
            }
            for _ in 0..PER_TURN {
                let Some(item) = lock(&self.shared.queue).pop_front() else {
                    break;
                };
                self.write(item)?;
            }
            if Instant::now() >= self.next_ping && self.flushing.is_none() {
                self.send(Command::Ping(self.ping), qpc())?;
                self.ping = self.ping.wrapping_add(1);
                self.next_ping = Instant::now() + PING_EVERY;
            }
            let events = self.conn.poll();
            self.record(None);
            for e in events? {
                self.see(&e);
            }
            if self.shared.end.load(Ordering::Acquire) && lock(&self.shared.queue).is_empty() {
                match self.flushing {
                    None => {
                        self.send(Command::Flush, qpc())?;
                        self.flushing = Some(Instant::now());
                    }
                    Some(_) if lock(&self.shared.state).flushed => return Ok(()),
                    Some(at) if at.elapsed() > FLUSH_WAIT => {
                        self.problems
                            .push(format!("no `F` within {FLUSH_WAIT:?} of `flush`"));
                        return Ok(());
                    }
                    Some(_) => {}
                }
            }
        }
    }

    /// Writes one queued command: a hold-bearing one only when the gate is
    /// open as its submission begins, its records written before; the
    /// submission holds the submit lock, so a stop waits for it to end.
    fn write(&mut self, item: Item) -> anyhow::Result<()> {
        if !item.cmd.holds_input() {
            return self.send(item.cmd, item.queued);
        }
        self.probe("taken");
        let check = item
            .check
            .map(|check| json!({ "t": "check", "q": check, "line": item.cmd.to_string() }));
        self.record(check);
        self.probe("recorded");
        let submit = lock(&self.shared.submit);
        if lock(&self.shared.gate).stopped.is_some() {
            drop(submit);
            self.record(Some(
                json!({ "t": "dropped", "q": qpc(), "line": item.cmd.to_string() }),
            ));
            return Ok(());
        }
        let sent = self.conn.send(item.cmd);
        let done = qpc();
        drop(submit);
        self.journal_sent(item.queued, done);
        self.probe("written");
        sent.map(drop)
    }

    /// Writes the connection's records, the command's with when it was
    /// queued and when its write ended.
    fn journal_sent(&mut self, queued: i64, done: i64) {
        for mut j in self.conn.take_journal() {
            if j["t"] == "cmd" {
                j["queued"] = json!(queued);
                j["done"] = json!(done);
            }
            self.shared.log.write(&j);
        }
    }

    /// What one line from the device changes.
    fn see(&mut self, e: &Event) {
        let grant = self.conn.grant().copied();
        let pings = self.conn.pings();
        match e {
            Event::Answer { q, line, to } => {
                let What::Command(cmd) = to.what else { return };
                if self.released && cmd == Command::Release && line == "ok" {
                    self.release_ok.get_or_insert(*q);
                }
                let refused = line.starts_with("err ") && cmd.holds_input();
                if refused {
                    stop_holds(
                        &self.shared,
                        &format!("the device refused `{cmd}`: `{line}`"),
                    );
                }
                // Written after the release took effect, by the order of
                // the host's counter.
                let after_fence = self.release_ok.is_some_and(|ok| to.q > ok)
                    && matches!(cmd, Command::Hello)
                    && matches!(Answer::parse_canonical(line), Some(Answer::Hello(_)));
                self.update(|s| {
                    s.grant = grant;
                    s.pings = pings;
                    if cmd == Command::Release && line == "ok" {
                        s.released = true;
                    }
                    if after_fence {
                        s.hellos.push((*q, line.clone()));
                    }
                    s.answers.push((cmd, line.clone()));
                });
            }
            Event::Record { record, .. } => {
                if record.body == Body::Flushed {
                    self.update(|s| s.flushed = true);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use test_device::protocol::{Change, Control};

    use super::super::link::fake::{Dev, Host, hello};
    use super::super::link::{Endpoints, Port};
    use super::*;

    const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";

    struct Run {
        _dir: tempfile::TempDir,
        dev: Dev,
        w: Writer,
        thread: Option<JoinHandle<()>>,
        log: Stream,
        run: Stream,
    }

    impl Run {
        fn new() -> Self {
            Self::with(Dev::new(HERE, "COM5", hello("abc", false)), |d| {
                Box::new(d) as Box<dyn Port>
            })
        }

        fn with(
            dev: Dev,
            port: impl Fn(super::super::link::fake::Shared) -> Box<dyn Port>,
        ) -> Self {
            struct H<'a>(
                &'a Host,
                &'a dyn Fn(super::super::link::fake::Shared) -> Box<dyn Port>,
            );
            impl Endpoints for H<'_> {
                fn find(
                    &self,
                    bound: Option<&str>,
                ) -> anyhow::Result<Option<(String, Option<super::super::link::Link>)>>
                {
                    self.0.find(bound)
                }
                fn open(&self, path: &str) -> anyhow::Result<Box<dyn Port>> {
                    let d = self.0.devs.iter().find(|d| d.named(path)).unwrap();
                    Ok((self.1)(d.device.clone()))
                }
                fn location_of(&self, path: &str) -> anyhow::Result<Option<String>> {
                    self.0.location_of(path)
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let host = Host {
                devs: vec![dev.clone()],
                ..Host::default()
            };
            let mut conn = Connection::bind(&H(&host, &port), Some(HERE)).unwrap();
            conn.check_hello(Some("abc")).unwrap();
            let log = Stream::create(&dir.path().join("link.jsonl")).unwrap();
            let run = Stream::create(&dir.path().join("run.jsonl")).unwrap();
            let (w, thread) = Writer::start(conn, log.clone(), run.clone());
            Self {
                _dir: dir,
                dev,
                w,
                thread: Some(thread),
                log,
                run,
            }
        }

        fn lines(&self) -> Vec<String> {
            self.dev.device.with(|d| d.lines())
        }

        fn records(s: &Stream) -> Vec<Value> {
            std::fs::read_to_string(&s.path)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        fn finish(&mut self) {
            self.w.end();
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn wait_for(w: &Writer, done: impl Fn(&State) -> bool) {
        assert!(
            w.wait(Duration::from_secs(2), &|| {}, done),
            "{:?}",
            w.state()
        );
    }

    fn press(w: &Writer) -> Command {
        Command::Set {
            id: w.state().grant.unwrap().id(),
            change: Change {
                control: Control::Button(1),
                value: 1,
            },
        }
    }

    fn grant(r: &Run) {
        let token = r.w.state().token.unwrap();
        r.w.enqueue(
            Command::Lease {
                token,
                ms: 250,
                deadline: 9_000_000_000,
            },
            Some(qpc()),
        )
        .unwrap();
        wait_for(&r.w, |s| s.grant.is_some());
    }

    #[test]
    fn one_request_however_many_callers() {
        let mut r = Run::new();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let w = r.w.clone();
                let b = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    w.stop_holds(&format!("caller {i}"));
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        wait_for(&r.w, |s| s.released);
        r.finish();
        let releases = r.lines().iter().filter(|l| *l == "release").count();
        assert_eq!(releases, 1, "{:?}", r.lines());
        let stops = Run::records(&r.run)
            .iter()
            .filter(|v| v["t"] == "stop")
            .count();
        assert_eq!(stops, 1);
    }

    /// After the request, no hold-bearing command is admitted, and those
    /// still queued are dropped, not written: nothing holding follows the
    /// `release` on the wire.
    #[test]
    fn nothing_holding_is_written_after_the_request() {
        let mut r = Run::new();
        grant(&r);
        let press = press(&r.w);
        // Hold the link thread's writes back while commands queue.
        r.dev.device.with(|d| d.hold_answers = true);
        {
            let mut q = lock(&r.w.shared.queue);
            for _ in 0..3 {
                q.push_back(Item {
                    cmd: press,
                    queued: qpc(),
                    check: None,
                });
            }
            // The request lands while they wait in the queue.
            r.w.stop_holds("lost");
        }
        assert_eq!(r.w.enqueue(press, None), Err(Refused::Closed));
        r.dev
            .device
            .with(super::super::link::fake::Device::release_answers);
        wait_for(&r.w, |s| s.fenced);
        r.finish();
        let lines = r.lines();
        let at = lines.iter().position(|l| l == "release").unwrap();
        assert!(
            !lines[at..]
                .iter()
                .any(|l| Command::parse_canonical(l).is_some_and(|c| c.holds_input())),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("button")),
            "a queued hold was written: {lines:?}"
        );
        let dropped = Run::records(&r.log)
            .iter()
            .filter(|v| v["t"] == "dropped")
            .count();
        assert_eq!(dropped, 3);
    }

    /// Only a `hello` written after the fence counts as release evidence.
    #[test]
    fn only_a_hello_after_the_fence_counts() {
        let mut r = Run::new();
        r.w.enqueue(Command::Hello, None).unwrap();
        wait_for(&r.w, |s| {
            s.answers.iter().any(|(c, _)| *c == Command::Hello)
        });
        assert!(r.w.state().hellos.is_empty());
        r.w.stop_holds("end");
        wait_for(&r.w, |s| s.released);
        r.w.enqueue(Command::Hello, None).unwrap();
        wait_for(&r.w, |s| !s.hellos.is_empty());
        r.finish();
        let s = r.w.state();
        assert_eq!(s.hellos.len(), 1);
        // The fence precedes the hello it lets count, in the link's stream.
        let recs = Run::records(&r.log);
        let fence = recs.iter().position(|v| v["t"] == "fence").unwrap();
        let hellos: Vec<usize> = recs
            .iter()
            .enumerate()
            // The link thread's writes (the bind's `hello` has no `queued`).
            .filter(|(_, v)| v["t"] == "cmd" && v["line"] == "hello" && v.get("queued").is_some())
            .map(|(i, _)| i)
            .collect();
        assert!(hellos[0] < fence && fence < hellos[1], "{recs:?}");
    }

    /// A `hello` written after the fence but before the `release`'s `ok`
    /// was read does not count, though answered after it; one written
    /// after that read does (the control).
    #[test]
    fn a_hello_counts_only_once_the_release_is_answered() {
        let mut r = Run::new();
        r.dev.device.with(|d| d.hold_answers = true);
        r.w.stop_holds("end");
        wait_for(&r.w, |s| s.fenced);
        r.w.enqueue(Command::Hello, None).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while !r
            .lines()
            .iter()
            .skip_while(|l| *l != "release")
            .any(|l| l == "hello")
        {
            assert!(Instant::now() < end, "{:?}", r.lines());
            std::thread::sleep(Duration::from_millis(1));
        }
        r.dev
            .device
            .with(super::super::link::fake::Device::release_answers);
        wait_for(&r.w, |s| {
            s.released && s.answers.iter().any(|(c, _)| *c == Command::Hello)
        });
        assert!(r.w.state().hellos.is_empty(), "{:?}", r.w.state());
        r.w.enqueue(Command::Hello, None).unwrap();
        wait_for(&r.w, |s| !s.hellos.is_empty());
        r.finish();
        assert_eq!(r.w.state().hellos.len(), 1);
    }

    /// The device's refusal of a hold-bearing command stops holds.
    #[test]
    fn a_device_refusal_of_a_hold_is_a_loss() {
        let mut r = Run::new();
        grant(&r);
        r.dev.device.with(|d| d.refuse = vec!["button".into()]);
        r.w.enqueue(press(&r.w), None).unwrap();
        wait_for(&r.w, |s| s.released);
        r.finish();
        let cause = r.w.stopped().unwrap();
        assert!(cause.contains("the device refused `button"), "{cause}");
    }

    /// A port whose write of a line starting with `block` waits until it
    /// is let go.
    struct Blocking {
        inner: super::super::link::fake::Shared,
        block: &'static str,
        gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
    }

    impl Port for Blocking {
        fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
            if bytes.starts_with(self.block.as_bytes()) {
                let (m, c) = &*self.gate;
                let mut g = lock(m);
                g.0 = true;
                c.notify_all();
                while !g.1 {
                    g = c.wait(g).unwrap();
                }
            }
            self.inner.write(bytes)
        }
        fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            self.inner.read(buf)
        }
        fn touch_bootsel(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn setup(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A link thread stalled in its `flush` keeps its stream: the bounded
    /// join gives up within its deadline without the stream's lock, the
    /// thread has not ended; once it runs again it writes its `end` and
    /// closes the stream itself.
    #[test]
    fn a_stalled_link_thread_keeps_its_stream() {
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let g2 = Arc::clone(&gate);
        let mut r = Run::with(Dev::new(HERE, "COM5", hello("abc", false)), move |d| {
            Box::new(Blocking {
                inner: d,
                block: "flush",
                gate: Arc::clone(&g2),
            })
        });
        r.w.end();
        {
            let (m, c) = &*gate;
            let mut g = lock(m);
            while !g.0 {
                g = c.wait(g).unwrap();
            }
        }
        // The write is let go 2 s on whatever the join does, so a join that
        // waits past its bound ends too, late.
        let g3 = Arc::clone(&gate);
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            let (m, c) = &*g3;
            lock(m).1 = true;
            c.notify_all();
        });
        let begun = Instant::now();
        let joined = super::super::owner::join_within(
            r.thread.take().unwrap(),
            "the link",
            Duration::from_millis(100),
        );
        let took = begun.elapsed();
        let ended_then = r.w.state().ended;
        unblock.join().unwrap();
        assert!(joined.is_err(), "the join waited for the stalled thread");
        assert!(took < Duration::from_secs(1), "{took:?}");
        assert!(!ended_then);
        let until = Instant::now() + Duration::from_secs(5);
        while !r.w.state().ended {
            assert!(Instant::now() < until, "the link thread did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(r.w.state().log_error, None);
        let records = Run::records(&r.log);
        assert_eq!(records.last().unwrap()["t"], "end", "{records:?}");
        // The thread closed it: a later record is refused.
        r.log.write(&json!({ "t": "late" }));
        assert!(r.log.failed().is_some());
    }

    /// A renewal whose write blocks mid-way when another thread stops
    /// holds: the stop waits for that write to end (the gate stays open
    /// meanwhile, so no fence), and `release` follows it.
    #[test]
    fn the_fence_waits_for_a_hold_bearing_write_in_progress() {
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let g2 = Arc::clone(&gate);
        let mut r = Run::with(Dev::new(HERE, "COM5", hello("abc", false)), move |d| {
            Box::new(Blocking {
                inner: d,
                block: "renew",
                gate: Arc::clone(&g2),
            })
        });
        grant(&r);
        let id = r.w.state().grant.unwrap().id();
        r.w.enqueue(
            Command::Renew {
                id,
                deadline: 9_100_000_000,
            },
            Some(qpc()),
        )
        .unwrap();
        // The renewal's write has begun and waits.
        {
            let (m, c) = &*gate;
            let mut g = lock(m);
            while !g.0 {
                g = c.wait(g).unwrap();
            }
        }
        let w = r.w.clone();
        let stopper = std::thread::spawn(move || w.stop_holds("lost"));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            r.w.stopped().is_none() && !stopper.is_finished(),
            "the stop came while a write was in progress"
        );
        assert!(
            !r.w.state().fenced,
            "the fence came while a write was in progress"
        );
        {
            let (m, c) = &*gate;
            lock(m).1 = true;
            c.notify_all();
        }
        stopper.join().unwrap();
        wait_for(&r.w, |s| s.released);
        r.finish();
        let lines = r.lines();
        let renew = lines.iter().position(|l| l.starts_with("renew")).unwrap();
        let release = lines.iter().position(|l| l == "release").unwrap();
        assert!(renew < release, "{lines:?}");
    }

    /// A port that, while it writes a hold-bearing line, has another
    /// thread stop holds, and notes whether that stop returned before the
    /// write ended (it must not).
    struct Stopping {
        inner: super::super::link::fake::Shared,
        writer: Arc<std::sync::OnceLock<Writer>>,
        /// The stop returned while the write was in progress.
        early: Arc<AtomicBool>,
        stopper: Option<JoinHandle<()>>,
    }

    impl Port for Stopping {
        fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
            if bytes.starts_with(b"button")
                && let Some(w) = self.writer.get()
            {
                let (w, done) = (w.clone(), Arc::new(AtomicBool::new(false)));
                let d2 = Arc::clone(&done);
                let stopper = std::thread::spawn(move || {
                    w.stop_holds("writing");
                    d2.store(true, Ordering::Release);
                });
                // Long enough for an unserialized stop to return.
                std::thread::sleep(Duration::from_millis(50));
                if done.load(Ordering::Acquire) {
                    self.early.store(true, Ordering::Release);
                }
                self.stopper = Some(stopper);
            }
            self.inner.write(bytes)
        }
        fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            self.inner.read(buf)
        }
        fn touch_bootsel(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn setup(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Holds stop at every point of a hold-bearing command's way to the
    /// wire: before it is queued, once the link thread took it, once its
    /// records are written, from another thread while the port writes it,
    /// and once it is written; and, the control, never. The command
    /// reaches the wire exactly when its submission began before the stop,
    /// a stop during the write waits for the write to end, the write comes
    /// before the fence and the `release`, no line holding input follows
    /// the `release`, and every hold not written is refused or dropped.
    #[test]
    fn a_hold_is_written_only_when_submitted_before_the_stop() {
        for at in [
            Some("queue"),
            Some("taken"),
            Some("recorded"),
            Some("writing"),
            Some("written"),
            None,
        ] {
            let writer = Arc::new(std::sync::OnceLock::new());
            let early = Arc::new(AtomicBool::new(false));
            let (w2, e2) = (Arc::clone(&writer), Arc::clone(&early));
            let mut r = Run::with(Dev::new(HERE, "COM5", hello("abc", false)), move |d| {
                Box::new(Stopping {
                    inner: d,
                    writer: Arc::clone(&w2),
                    early: Arc::clone(&e2),
                    stopper: None,
                })
            });
            grant(&r);
            if at == Some("writing") {
                assert!(writer.set(r.w.clone()).is_ok());
            }
            *lock(&r.w.shared.probe) = at.filter(|a| *a != "queue" && *a != "writing");
            let press = press(&r.w);
            if at == Some("queue") {
                r.w.stop_holds("queue");
                assert_eq!(r.w.enqueue(press, Some(qpc())), Err(Refused::Closed));
            } else {
                r.w.enqueue(press, Some(qpc())).unwrap();
            }
            if at.is_some() {
                wait_for(&r.w, |s| s.released);
            } else {
                wait_for(&r.w, |s| {
                    s.answers
                        .iter()
                        .any(|(c, _)| matches!(c, Command::Set { .. }))
                });
            }
            r.finish();
            let lines = r.lines();
            let written = lines.iter().position(|l| l.starts_with("button"));
            let release = lines.iter().position(|l| l == "release");
            // The oracle.
            let submitted_first = matches!(at, Some("writing" | "written") | None);
            assert_eq!(written.is_some(), submitted_first, "{at:?}: {lines:?}");
            assert!(
                !early.load(Ordering::Acquire),
                "{at:?}: the stop returned during the write"
            );
            if let Some(release) = release {
                assert!(written.is_none_or(|w| w < release), "{at:?}: {lines:?}");
                assert!(
                    !lines[release..]
                        .iter()
                        .any(|l| Command::parse_canonical(l).is_some_and(|c| c.holds_input())),
                    "{at:?}: {lines:?}"
                );
            }
            assert_eq!(release.is_some(), at.is_some(), "{at:?}: {lines:?}");
            let dropped = Run::records(&r.log)
                .iter()
                .filter(|v| v["t"] == "dropped")
                .count();
            let want = usize::from(matches!(at, Some("taken" | "recorded")));
            assert_eq!(dropped, want, "{at:?}");
        }
    }

    /// A full queue refuses rather than waits: the link thread is held in
    /// its first ping's write while the queue fills.
    #[test]
    fn a_full_queue_refuses() {
        let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let g2 = Arc::clone(&gate);
        let mut r = Run::with(Dev::new(HERE, "COM5", hello("abc", false)), move |d| {
            Box::new(Blocking {
                inner: d,
                block: "ping",
                gate: Arc::clone(&g2),
            })
        });
        {
            let (m, c) = &*gate;
            let mut g = lock(m);
            while !g.0 {
                g = c.wait(g).unwrap();
            }
        }
        for _ in 0..QUEUE {
            r.w.enqueue(Command::Hello, None).unwrap();
        }
        assert_eq!(r.w.enqueue(Command::Hello, None), Err(Refused::Full));
        {
            let (m, c) = &*gate;
            lock(m).1 = true;
            c.notify_all();
        }
        r.finish();
        let hellos = r.lines().iter().filter(|l| *l == "hello").count();
        // The bind's own and the queued ones, the refused one not.
        assert_eq!(hellos, QUEUE + 1);
    }
}
