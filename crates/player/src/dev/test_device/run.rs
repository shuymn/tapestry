//! `tapestry test-device run <row> …` (docs/test-device.md "Run Guard",
//! "Host Commands"): one run of a row. Rows are code: each is a list of
//! typed steps. The runner reserves the run's folder, joins its own job,
//! starts the recorder in it, holds the device's port on its link thread,
//! opens its sink, and runs the row's steps under the guard; every way out
//! goes through the end ([`super::end`]).
//!
//! Rows: `sink` (N1: a focus click on the sink, a lease of 3 s, button 1
//! and F24 held, 1 s, the end), `sink-early-close` (N1's negative: the
//! same, with the sink closed straight after the end's release request),
//! and `sink-kill` (N2: an outer run that starts an inner `sink` in a job
//! of its own, terminates that job with its cut code once the inner's
//! recorder shows the presses, observes the inner's and its recorder's
//! ends, and opens its own connection).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use test_device::evidence::{Process, Recorder as Rec, Reserved};
use test_device::protocol::{Change, Command, Control, Token};
use test_device::report::{PAD_REPORT, PadReport};

use super::click::{self, Win};
use super::desktop::{self, Real};
use super::devices::System;
use super::end;
use super::guard::{self, Look, Watch};
use super::job::{self, Child, Job, Pipe, Stdio};
use super::link::{Connection, hello_facts};
use super::owner::{self, Stream};
use super::release::{Refused, Writer};
use super::sink::{self, Sink};
use super::win::{qpc, qpc_freq};

#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub row: String,
    /// A single run's folder.
    pub out: Option<PathBuf>,
    /// The reserving run's folder, and the role reserved there.
    pub set: Option<PathBuf>,
    pub role: Option<String>,
    /// The device's build: a bare commit id.
    pub build: String,
    /// The device's USB location path.
    pub port: String,
    /// The run's monitor, by its device name.
    pub monitor: String,
}

/// A row's step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A focus click on the run's window.
    Focus,
    /// The row's lease, of `ms`: the guard grants it in slices.
    Lease {
        ms: u32,
    },
    /// A held change under the lease.
    Hold(Change),
    /// Waits until the recorder shows every press held, from the device's
    /// bound handles, and records `held`.
    AwaitHeld,
    Wait {
        ms: u64,
    },
    /// A step that blocks the step thread (a link's start, a rediscovery,
    /// a flash): refused under a lease.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the sink rows block before their lease only; B3's rows take it"
        )
    )]
    Blocking(&'static str),
}

/// F24's keyboard usage: a key that types nothing.
pub const F24: u8 = 0x73;
/// F24's scan code, as the recorder reads it.
pub const F24_SCAN: u64 = 0x76;

/// The steps of row `sink`.
pub fn sink_steps() -> Vec<Step> {
    let press = |control| Change { control, value: 1 };
    vec![
        Step::Focus,
        Step::Lease { ms: 3000 },
        Step::Hold(press(Control::Button(1))),
        Step::Hold(press(Control::Key(F24))),
        Step::AwaitHeld,
        Step::Wait { ms: 1000 },
    ]
}

/// The rows with steps of their own, and whether the row closes its sink
/// straight after the release request (the negative).
pub fn row(name: &str) -> Option<(Vec<Step>, bool)> {
    match name {
        "sink" => Some((sink_steps(), false)),
        "sink-early-close" => Some((sink_steps(), true)),
        _ => None,
    }
}

/// The command a step writes itself: none but a held change does, so no
/// row step can request a release or a lease of its own length.
pub const fn command(step: &Step, grant: test_device::protocol::LeaseId) -> Option<Command> {
    match *step {
        Step::Hold(change) => Some(Command::Set { id: grant, change }),
        _ => None,
    }
}

/// Whether `s` is a bare commit id: 12 lowercase hex digits.
pub fn bare_commit(s: &str) -> bool {
    s.len() == 12
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether the run may hold input on the device whose `hello` is `line`:
/// only when the run's build is a bare commit id and the device names it
/// with `faults 0`.
///
/// # Errors
///
/// Why not.
pub fn may_hold(build: &str, line: &str) -> Result<(), String> {
    if !bare_commit(build) {
        return Err(format!(
            "`{build}` is not a bare commit id (12 hex digits): no hold is taken"
        ));
    }
    hello_facts(line, Some(build)).map(drop)
}

/// What the steps act through.
pub trait Exec {
    /// Whether the row's lease lasts.
    fn leased(&self) -> bool;
    fn focus(&mut self) -> Result<(), String>;
    fn lease(&mut self, ms: u32) -> Result<(), String>;
    fn hold(&mut self, change: Change) -> Result<(), String>;
    fn await_held(&mut self) -> Result<(), String>;
    fn wait(&mut self, ms: u64) -> Result<(), String>;
    fn blocking(&mut self, what: &str) -> Result<(), String>;
}

/// Runs `steps`: the first error ends them.
///
/// # Errors
///
/// A step's error, or a blocking step under a lease.
pub fn perform(steps: &[Step], x: &mut dyn Exec) -> Result<(), String> {
    for s in steps {
        match *s {
            Step::Focus => x.focus(),
            Step::Lease { ms } => x.lease(ms),
            Step::Hold(c) => x.hold(c),
            Step::AwaitHeld => x.await_held(),
            Step::Wait { ms } => x.wait(ms),
            Step::Blocking(what) if x.leased() => Err(format!(
                "`{what}` blocks the step thread, which a lease forbids: it goes before the lease"
            )),
            Step::Blocking(what) => x.blocking(what),
        }?;
    }
    Ok(())
}

/// How long a grant may take to come.
const GRANT_WAIT: Duration = Duration::from_secs(5);
/// How long the recorder may take to show the presses.
const HELD_WAIT: Duration = Duration::from_secs(2);
/// How often a waiting step thread looks (and beats).
const LOOK: Duration = Duration::from_millis(5);
/// How long the recorder may take to be ready.
const READY_WAIT: Duration = Duration::from_secs(5);

/// The run's records, each stamped with the host counter.
#[derive(Clone)]
struct Log(Stream);

impl Log {
    fn put(&self, mut v: Value) {
        if v.get("q").is_none() && v["t"] != "seal" && v["t"] != "end" && v["t"] != "start" {
            v["q"] = json!(qpc());
        }
        self.0.write(&v);
    }
}

/// The recorder's records so far, read whole lines at a time.
struct Tail {
    path: PathBuf,
    at: u64,
    partial: String,
}

impl Tail {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            at: 0,
            partial: String::new(),
        }
    }

    /// The records written since the last read.
    fn read(&mut self) -> Vec<Value> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if f.seek(SeekFrom::Start(self.at)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        let Ok(n) = f.read_to_string(&mut text) else {
            return Vec::new();
        };
        self.at += n as u64;
        self.partial.push_str(&text);
        let mut out = Vec::new();
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            if let Ok(v) = serde_json::from_str(line.trim_end()) {
                out.push(v);
            }
        }
        out
    }
}

/// The pad report's buttons in a recorder `pad` record's bytes (a report
/// id 0 in front, as Windows reads a device without report ids).
pub fn pad_buttons(hex: &str) -> Option<u16> {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let body = match bytes.as_slice() {
        [0, rest @ ..] if rest.len() == PAD_REPORT => rest,
        b => b,
    };
    PadReport::decode(body).map(|r| r.buttons)
}

/// The run's live state: what the steps and the guard act on.
struct Live {
    writer: Writer,
    guard: guard::Handle,
    beat: Arc<AtomicI64>,
    sink: Win,
    monitor: String,
    monitor_then: [i32; 4],
    prefix: u16,
    call: u16,
    log: Log,
    recorder: Tail,
    held: Vec<Change>,
    freq: i64,
}

impl Live {
    fn beat(&self) {
        self.beat.store(qpc(), Ordering::Release);
    }

    fn lost(&self) -> Result<(), String> {
        match self.writer.stopped() {
            Some(why) => Err(format!("holds stopped: {why}")),
            None => Ok(()),
        }
    }
}

impl Exec for Live {
    fn leased(&self) -> bool {
        self.guard.wanted(qpc())
    }

    fn focus(&mut self) -> Result<(), String> {
        let log = self.log.clone();
        let mut note = |tag: u32, what: &str| {
            log.put(json!({ "t": "call", "q": qpc(), "tag": tag, "what": what }));
        };
        let mut desk = Real {
            monitor: self.monitor.clone(),
            note: &mut note,
        };
        let client = desktop::client_rect(self.sink).ok_or("the sink is gone")?;
        let point = (client[2] / 2, client[3] / 2);
        let r = click::click(
            &mut desk,
            self.sink,
            point,
            true,
            self.monitor_then,
            self.prefix,
            &mut self.call,
        );
        self.beat();
        r.map_err(|e| format!("the focus click: {e:?}"))
    }

    fn lease(&mut self, ms: u32) -> Result<(), String> {
        self.beat();
        self.guard.want(qpc() + i64::from(ms) * self.freq / 1000);
        let beat = || self.beat();
        if !self
            .writer
            .wait(GRANT_WAIT, &beat, |s| s.grant.is_some() || s.lost.is_some())
        {
            return Err(format!("no grant within {GRANT_WAIT:?}"));
        }
        self.lost()?;
        if self.writer.state().grant.is_none() {
            return Err("the link failed before the grant".into());
        }
        Ok(())
    }

    fn hold(&mut self, change: Change) -> Result<(), String> {
        self.lost()?;
        let grant = self.writer.state().grant.ok_or("no grant")?;
        let cmd = command(&Step::Hold(change), grant.id()).ok_or("not a held change")?;
        self.writer.enqueue(cmd, None).map_err(|e| match e {
            Refused::Full => {
                self.writer.stop_holds("the link's queue is full");
                "the link's queue is full".to_owned()
            }
            Refused::Closed => "holds stopped".to_owned(),
        })?;
        self.held.push(change);
        Ok(())
    }

    fn await_held(&mut self) -> Result<(), String> {
        let buttons: u16 = self
            .held
            .iter()
            .filter_map(|c| match c.control {
                Control::Button(n) => Some(1 << (n - 1)),
                _ => None,
            })
            .fold(0, |a, b| a | b);
        let key = self.held.iter().any(|c| c.control == Control::Key(F24));
        let (mut pad_seen, mut key_seen) = (buttons == 0, !key);
        let end = Instant::now() + HELD_WAIT;
        while !(pad_seen && key_seen) {
            self.beat();
            self.lost()?;
            if Instant::now() >= end {
                return Err(format!(
                    "the recorder did not show the presses within {HELD_WAIT:?}"
                ));
            }
            for v in self.recorder.read() {
                if v["ours"] != true {
                    continue;
                }
                if v["t"] == "pad"
                    && v["bytes"]
                        .as_str()
                        .and_then(pad_buttons)
                        .is_some_and(|b| b & buttons == buttons)
                {
                    pad_seen = true;
                }
                if v["t"] == "key" && v["code"] == F24_SCAN && v["down"] == true {
                    key_seen = true;
                }
            }
            std::thread::sleep(LOOK);
        }
        self.log.put(json!({ "t": "held" }));
        Ok(())
    }

    fn wait(&mut self, ms: u64) -> Result<(), String> {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.beat();
            self.lost()?;
            std::thread::sleep(LOOK);
        }
        Ok(())
    }

    fn blocking(&mut self, what: &str) -> Result<(), String> {
        Err(format!("no B2b row has the blocking step `{what}`"))
    }
}

/// The guard's real look at the run.
struct Looks {
    sink: Win,
    client: [i32; 4],
    monitor: String,
    monitor_then: [i32; 4],
    monitor_now: Option<[i32; 4]>,
    read: Instant,
    beat: Arc<AtomicI64>,
}

impl Watch for Looks {
    fn look(&mut self) -> Look {
        if self.read.elapsed() >= guard::MONITOR_EVERY {
            self.monitor_now = desktop::monitor(&self.monitor).ok().map(|m| m.area);
            self.read = Instant::now();
        }
        let at = qpc();
        let client = desktop::client_rect(self.sink);
        let placed = self.monitor_now == Some(self.monitor_then)
            && client == Some(self.client)
            && click::contains(self.monitor_then, self.client);
        Look {
            at,
            front: desktop::foreground() == self.sink,
            placed,
            beat: self.beat.load(Ordering::Acquire),
        }
    }
}

/// How long a terminated process may take to end.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// Writes the `exit` record of `name`, process `p`, once its end was
/// observed: the code it ended with.
fn exit_record(log: &Log, name: &str, p: Process, code: i32) {
    log.put(json!({
        "t": "exit", "name": name, "code": code,
        "process": { "pid": p.pid, "created": p.created },
    }));
}

/// Writes the `kill` record of `name`, process `p`, before the runner
/// terminates it with its cut code.
fn kill_record(log: &Log, name: &str, p: Process) {
    log.put(json!({
        "t": "kill", "name": name,
        "process": { "pid": p.pid, "created": p.created },
    }));
}

/// Waits up to `wait` for `c` to end; else records its kill, terminates it
/// with the cut code and waits for that end. The exit is recorded only once
/// observed: its code, or why its end was not observed.
fn end_child(log: &Log, name: &str, c: &Child, wait: Duration, cut: i32) -> Result<i32, String> {
    let code = match c.wait(wait) {
        Some(code) => code,
        None => {
            kill_record(log, name, c.process);
            c.kill(cut.cast_unsigned())
                .map_err(|e| format!("terminating {name}: {e:#}"))?;
            c.wait(KILL_WAIT).ok_or_else(|| {
                format!("{name} did not end within {KILL_WAIT:?} of its termination")
            })?
        }
    };
    exit_record(log, name, c.process, code);
    Ok(code)
}

/// The recorder child: its process, its input (whose end ends it), and
/// its stream.
struct RecChild {
    child: Child,
    input: Option<std::fs::File>,
    path: PathBuf,
    tail: Tail,
    log: Log,
    name: &'static str,
    run: String,
    cut: i32,
}

impl RecChild {
    fn spawn(b: &Begun, port: &str) -> anyhow::Result<Self> {
        let (job, dir, log) = (b.job, &b.dir, &b.log);
        let exe = std::env::current_exe().context("the runner's own path")?;
        let path = dir.join(test_device::contract::RECORDER);
        let input = Pipe::new(true)?;
        let err = owner::create(&dir.join("recorder-stderr.txt"))?;
        let args = [
            "test-device",
            "listen",
            "--out",
            &path.to_string_lossy(),
            "--port",
            port,
            "--run",
            &b.run,
        ]
        .map(str::to_owned);
        let stdio = Stdio {
            input: input.child,
            output: Stdio::null()?,
            error: Stdio::file(err),
        };
        let child = job::spawn(job, &exe, &args, &[], &stdio)?;
        log.put(json!({
            "t": "spawn", "name": "recorder",
            "process": { "pid": child.process.pid, "created": child.process.created },
        }));
        let mut me = Self {
            child,
            input: Some(input.own),
            tail: Tail::new(&path),
            path,
            log: log.clone(),
            name: "recorder",
            run: b.run.clone(),
            cut: b.cut,
        };
        me.ready()?;
        Ok(me)
    }

    /// Waits for the recorder's `ready` and checks it is the child's.
    fn ready(&mut self) -> anyhow::Result<()> {
        let end = Instant::now() + READY_WAIT;
        loop {
            if let Some(v) = self.tail.read().into_iter().next() {
                let Ok(Rec::Ready(r)) = serde_json::from_value::<Rec>(v.clone()) else {
                    bail!("the recorder's first record is not `ready`: {v}");
                };
                let owner = desktop::process_of(r.window);
                owner::ready_is_the_child_s(&r, &self.run, self.child.process, owner)
                    .map_err(anyhow::Error::msg)?;
                return Ok(());
            }
            if let Some(code) = self.child.wait(Duration::ZERO) {
                bail!("the recorder ended with {code} before it was ready");
            }
            if Instant::now() >= end {
                bail!("the recorder was not ready within {READY_WAIT:?}");
            }
            std::thread::sleep(LOOK);
        }
    }
}

impl end::Recorder for RecChild {
    fn removed(&mut self) -> bool {
        self.tail
            .read()
            .iter()
            .any(|v| v["t"] == "dev" && v["change"] == "removal" && v["ours"] == true)
    }

    fn stop(&mut self) -> Result<(), String> {
        drop(self.input.take());
        match end_child(
            &self.log,
            self.name,
            &self.child,
            Duration::from_secs(5),
            self.cut,
        )? {
            0 => Ok(()),
            c if c == self.cut => {
                Err("it did not end within 5 s of its input's end, and was terminated".into())
            }
            c => Err(format!("it exited {c}")),
        }
    }
}

/// The end's link: the link thread, and the bound location for recovery.
struct EndLink<'a> {
    writer: &'a Writer,
    thread: Option<JoinHandle<()>>,
    port: &'a str,
    build: &'a str,
    dir: &'a Path,
    beat: &'a dyn Fn(),
}

impl end::Link for EndLink<'_> {
    fn stop_holds(&mut self, cause: &str) {
        self.writer.stop_holds(cause);
    }

    fn released(&mut self, wait: Duration) -> Result<(), String> {
        let ok = self.writer.wait(wait, self.beat, |s| {
            (s.fenced && s.released) || s.lost.is_some()
        });
        let s = self.writer.state();
        if let Some(why) = s.lost {
            return Err(format!("the link failed: {why}"));
        }
        if !ok {
            return Err(format!(
                "the `release` was not answered `ok` within {wait:?}"
            ));
        }
        Ok(())
    }

    fn hello(&mut self, wait: Duration) -> Option<String> {
        let before = self.writer.state().hellos.len();
        self.writer.enqueue(Command::Hello, None).ok()?;
        self.writer.wait(wait, self.beat, |s| {
            s.hellos.len() > before || s.lost.is_some()
        });
        self.writer
            .state()
            .hellos
            .get(before)
            .map(|(_, l)| l.clone())
    }

    fn finish(&mut self, deadline: Duration) -> Result<(), String> {
        self.writer.end();
        let Some(t) = self.thread.take() else {
            return Ok(());
        };
        let end = Instant::now() + deadline;
        while !t.is_finished() {
            if Instant::now() >= end {
                return Err(format!("the link did not end within {deadline:?}"));
            }
            std::thread::sleep(LOOK);
        }
        let _ = t.join();
        match self.writer.state().lost {
            Some(why) => Err(why),
            None => Ok(()),
        }
    }

    fn reopen_release(&mut self) -> Result<String, String> {
        let journal =
            Stream::create(&self.dir.join("recovery-link.jsonl")).map_err(|e| format!("{e:#}"))?;
        let mut c = Connection::bind(&System, Some(self.port)).map_err(|e| format!("{e:#}"))?;
        let r = (|| -> anyhow::Result<String> {
            c.check_hello(Some(self.build))?;
            c.send(Command::Release)?;
            c.send(Command::Hello)?;
            let end = Instant::now() + Duration::from_secs(1);
            let mut last = None;
            while Instant::now() < end && last.is_none() {
                for e in c.poll()? {
                    if let super::link::Event::Answer { line, to, .. } = e
                        && to.what == super::link::What::Command(Command::Hello)
                    {
                        last = Some(line);
                    }
                }
            }
            last.context("no answer to `hello`")
        })();
        for j in c.take_journal() {
            journal.write(&j);
        }
        let _ = journal.close();
        r.map_err(|e| format!("{e:#}"))
    }

    fn touch_bootsel(&mut self) -> Result<(), String> {
        Connection::bind(&System, Some(self.port))
            .and_then(Connection::touch_bootsel)
            .map_err(|e| format!("{e:#}"))
    }
}

impl end::Window for Sink {
    fn close(&mut self) -> Result<(), String> {
        Sink::close(self).map_err(|e| format!("{e:#}"))
    }
}

/// What a run of either kind starts with.
struct Begun {
    dir: PathBuf,
    run: String,
    role: String,
    log: Log,
    stream: Stream,
    /// The run's job, kept until this process exits: closing its handle
    /// would end this process too, before it reports.
    job: &'static Job,
    prefix: u16,
    /// This runner's process and its cut code: `0x40000000` plus 30 random
    /// bits, the exit code it gives every process it terminates.
    process: Process,
    cut: i32,
}

/// Reserves the run, joins its job and writes its `start`.
fn begin(a: &Args, inner: Option<&Reserved>) -> anyhow::Result<Begun> {
    desktop::dpi_aware()?;
    desktop::unswapped(desktop::swapped())?;
    let (dir, run, role, set) = match (&a.out, &a.set, &a.role) {
        (Some(out), None, None) => {
            owner::reserve(out)?;
            (
                out.clone(),
                owner::random_hex(16)?,
                test_device::contract::SINGLE.to_owned(),
                None,
            )
        }
        (None, Some(set), Some(role)) => {
            let (outer, run, dir) = owner::reserved(set, role, &a.row)?;
            (dir, run, role.clone(), Some(outer))
        }
        _ => bail!("a run takes `--out <dir>`, or `--set <dir> --role <role>`"),
    };
    let job: &'static Job = Box::leak(Box::new(Job::join()?));
    let stream = Stream::create(&dir.join(test_device::contract::RUN))?;
    let log = Log(stream.clone());
    let prefix_hex = owner::random_hex(2)?;
    let prefix = u16::from_str_radix(&prefix_hex, 16).context("the tag prefix")?;
    let me = job::this_process()?;
    let bits = u32::from_str_radix(&owner::random_hex(4)?, 16).context("the cut code")?;
    let cut = (0x4000_0000 | (bits & 0x3FFF_FFFF)).cast_signed();
    log.put(json!({
        "t": "start", "run": run, "row": a.row, "role": role, "build": a.build,
        "process": { "pid": me.pid, "created": me.created }, "commit": super::COMMIT,
        "tag": prefix, "cut": cut, "args": std::env::args().skip(1).collect::<Vec<_>>(),
        "set": set,
        "inner": inner.map(|r| json!({ "role": r.role, "row": r.row, "run": r.run, "dir": r.dir })),
    }));
    Ok(Begun {
        dir,
        run,
        role,
        log,
        stream,
        job,
        prefix,
        process: me,
        cut,
    })
}

/// Seals `name` in the run's folder.
fn seal(b: &Begun, dir: &Path, name: &str, run: &str, role: &str, errors: &mut Vec<String>) {
    match owner::seal(dir, &dir.join(name), run, role) {
        Ok(v) => b.log.put(v),
        Err(e) => errors.push(format!("sealing {name}: {e:#}")),
    }
}

/// Writes the `end` record and closes the stream: the exit code.
fn finish(b: &Begun, mut errors: Vec<String>) -> i32 {
    if let Some(e) = b.stream.failed() {
        errors.push(e);
    }
    let exit = i32::from(!errors.is_empty());
    for e in &errors {
        eprintln!("test-device run: {e}");
    }
    b.log
        .put(json!({ "t": "end", "exit": exit, "errors": errors }));
    if let Some(e) = b.stream.close() {
        eprintln!("test-device run: {e}");
        return 1;
    }
    exit
}

/// Runs a row: the exit code.
///
/// # Errors
///
/// The run could not begin (nothing of it is held then).
pub fn run(a: &Args) -> anyhow::Result<i32> {
    if a.row == "sink-kill" {
        return outer(a);
    }
    let Some((steps, early_close)) = row(&a.row) else {
        bail!("no row `{}`", a.row);
    };
    let b = begin(a, None)?;
    let mut errors = Vec::new();
    let mut rec = RecChild::spawn(&b, &a.port)?;
    let monitor = desktop::monitor(&a.monitor)?;
    let mut sink = Sink::open(sink::place(monitor.area))?;
    b.log.put(json!({ "t": "sink", "window": sink.window }));
    let link_log = Stream::create(&b.dir.join(test_device::contract::LINK))?;
    let mut conn = owner::rediscover(&System, &a.port, &a.build)?;
    conn.identify(&b.run, b.process);
    let hello = conn.facts().context("no checked `hello`")?.line.clone();
    may_hold(&a.build, &hello).map_err(anyhow::Error::msg)?;
    let token: Option<Token> = conn.token();
    let (writer, thread) = Writer::start(conn, link_log.clone(), b.stream.clone());
    let beat = Arc::new(AtomicI64::new(qpc()));
    let client = desktop::client_rect(sink.window).context("the sink's client area")?;
    let (guard, guard_thread) = guard::start(
        qpc_freq(),
        Box::new(Looks {
            sink: sink.window,
            client,
            monitor: a.monitor.clone(),
            monitor_then: monitor.area,
            monitor_now: Some(monitor.area),
            read: Instant::now(),
            beat: Arc::clone(&beat),
        }),
        Arc::new(writer.clone()),
    );
    let mut live = Live {
        writer: writer.clone(),
        guard: guard.clone(),
        beat: Arc::clone(&beat),
        sink: sink.window,
        monitor: a.monitor.clone(),
        monitor_then: monitor.area,
        prefix: b.prefix,
        call: 0,
        log: b.log.clone(),
        recorder: Tail::new(&rec.path),
        held: Vec::new(),
        freq: qpc_freq(),
    };
    let stepped =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| perform(&steps, &mut live)));
    match stepped {
        Ok(Ok(())) => {}
        Ok(Err(e)) => errors.push(e),
        Err(_) => errors.push("a step panicked".into()),
    }
    let granted = writer
        .state()
        .answers
        .iter()
        .any(|(c, l)| matches!(c, Command::Lease { .. }) && l.starts_with("lease "));
    let beat_fn = || beat.store(qpc(), Ordering::Release);
    let mut link = EndLink {
        writer: &writer,
        thread: Some(thread),
        port: &a.port,
        build: &a.build,
        dir: &b.dir,
        beat: &beat_fn,
    };
    let log = b.log.clone();
    let window = sink.window;
    let mut record = |mut v: Value| {
        // The sink's close names the window it closed.
        if v["t"] == "close" {
            v["window"] = json!(window);
        }
        log.put(v);
    };
    let clock = end::Wall(Instant::now());
    errors.extend(end::end(end::Run {
        link: &mut link,
        recorder: &mut rec,
        sink: Some(&mut sink),
        clock: &clock,
        token,
        granted,
        record: &mut record,
        cause: "end",
        early_close,
    }));
    guard.end();
    let _ = guard_thread.join();
    if let Some(e) = link_log.close() {
        errors.push(e);
    }
    for name in [test_device::contract::LINK, test_device::contract::RECORDER] {
        seal(&b, &b.dir, name, &b.run, &b.role, &mut errors);
    }
    Ok(finish(&b, errors))
}

/// How long the outer waits for the inner's `held`.
const INNER_HELD_WAIT: Duration = Duration::from_secs(30);
/// How long the outer waits for the inner's recorder to end.
const SURVIVOR_WAIT: Duration = Duration::from_secs(10);

/// Waits until `path` holds a record that `pick` takes, or `wait` passes.
fn await_record<T>(
    path: &Path,
    wait: Duration,
    mut pick: impl FnMut(&Value) -> Option<T>,
) -> Option<T> {
    let mut tail = Tail::new(path);
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if let Some(t) = tail.read().iter().find_map(&mut pick) {
            return Some(t);
        }
        std::thread::sleep(LOOK);
    }
    None
}

/// The outer of `sink-kill` (N2).
fn outer(a: &Args) -> anyhow::Result<i32> {
    let inner = Reserved {
        role: test_device::contract::INNER.to_owned(),
        row: "sink".to_owned(),
        run: owner::random_hex(16)?,
        dir: test_device::contract::INNER.to_owned(),
    };
    let b = begin(a, Some(&inner))?;
    let mut errors = Vec::new();
    let mut rec = RecChild::spawn(&b, &a.port)?;
    let exe = std::env::current_exe().context("the runner's own path")?;
    let set = b.dir.to_string_lossy().into_owned();
    let args = [
        "test-device",
        "run",
        "sink",
        "--set",
        &set,
        "--role",
        &inner.role,
        "--build",
        &a.build,
        "--port",
        &a.port,
        "--monitor",
        &a.monitor,
    ]
    .map(str::to_owned);
    let stdio = Stdio {
        input: Stdio::null()?,
        output: Stdio::file(owner::create(&b.dir.join("inner-stdout.txt"))?),
        error: Stdio::file(owner::create(&b.dir.join("inner-stderr.txt"))?),
    };
    // In a job of its own, so that its end takes everything it started.
    let (inner_job, child) = job::spawn_in_own_job(b.job, &exe, &args, &[], &stdio)?;
    b.log.put(json!({
        "t": "spawn", "name": "inner",
        "process": { "pid": child.process.pid, "created": child.process.created },
    }));
    let inner_dir = b.dir.join(&inner.dir);
    let inner_run = inner_dir.join(test_device::contract::RUN);
    // The inner's recorder, by the inner's spawn record.
    let recorder = await_record(&inner_run, READY_WAIT * 2, |v| {
        (v["t"] == "spawn" && v["name"] == "recorder")
            .then(|| serde_json::from_value::<Process>(v["process"].clone()).ok())
            .flatten()
    });
    let survivor = recorder.map(job::open);
    let held = await_record(&inner_run, INNER_HELD_WAIT, |v| {
        (v["t"] == "held").then_some(())
    });
    if held.is_none() {
        errors.push(format!(
            "the inner showed no `held` within {INNER_HELD_WAIT:?}"
        ));
    }
    // The inner and its job, its recorder with it, end with the cut code.
    kill_record(&b.log, "inner", child.process);
    inner_job
        .terminate(b.cut.cast_unsigned())
        .context("terminating the inner's job")?;
    let mut observed = true;
    match child.wait(KILL_WAIT) {
        Some(code) => exit_record(&b.log, "inner", child.process, code),
        None => {
            observed = false;
            errors.push(format!(
                "the inner did not end within {KILL_WAIT:?} of its termination"
            ));
        }
    }
    match survivor {
        Some(Ok(s)) => match s.wait(SURVIVOR_WAIT) {
            Some(code) => exit_record(&b.log, "inner-recorder", s.process, code),
            None => {
                observed = false;
                errors.push(format!(
                    "the inner's recorder did not end within {SURVIVOR_WAIT:?} of the termination"
                ));
            }
        },
        Some(Err(e)) => errors.push(format!("opening the inner's recorder: {e:#}")),
        None => errors.push("the inner recorded no recorder".into()),
    }
    drop(inner_job);
    // The inner's streams, sealed only once their writers' ends were seen.
    if observed {
        for name in [test_device::contract::LINK, test_device::contract::RECORDER] {
            seal(&b, &inner_dir, name, &inner.run, &inner.role, &mut errors);
        }
    }
    // Its own connection: the first `hello` is N2's evidence.
    let link_log = Stream::create(&b.dir.join(test_device::contract::LINK))?;
    let mut conn = owner::rediscover(&System, &a.port, &a.build)?;
    conn.identify(&b.run, b.process);
    let token = conn.token();
    let (writer, thread) = Writer::start(conn, link_log.clone(), b.stream.clone());
    // Let the outer's recorder see what the device sends now.
    std::thread::sleep(Duration::from_millis(500));
    let beat_fn = || {};
    let mut link = EndLink {
        writer: &writer,
        thread: Some(thread),
        port: &a.port,
        build: &a.build,
        dir: &b.dir,
        beat: &beat_fn,
    };
    let log = b.log.clone();
    let mut record = |v: Value| log.put(v);
    let clock = end::Wall(Instant::now());
    errors.extend(end::end(end::Run {
        link: &mut link,
        recorder: &mut rec,
        sink: None,
        clock: &clock,
        token,
        granted: false,
        record: &mut record,
        cause: "end",
        early_close: false,
    }));
    if let Some(e) = link_log.close() {
        errors.push(e);
    }
    for name in [test_device::contract::LINK, test_device::contract::RECORDER] {
        seal(&b, &b.dir, name, &b.run, &b.role, &mut errors);
    }
    Ok(finish(&b, errors))
}

#[cfg(test)]
mod tests {
    use test_device::protocol::LeaseId;

    use super::super::guard::{Act, Guard, SLICE_MS, View};
    use super::super::link::Grant;
    use super::*;

    const BUILD: &str = "0123456789ab";

    fn hello(build: &str, faults: bool) -> String {
        super::super::link::fake::hello(build, faults)
    }

    /// A stand-in whose lease is the guard's, over looks 5 ms apart.
    #[derive(Default)]
    struct Steps {
        did: Vec<String>,
        guard: Option<Guard>,
        sent: Vec<Command>,
        at: i64,
    }

    impl Exec for Steps {
        fn leased(&self) -> bool {
            self.guard.as_ref().is_some_and(|g| g.wanted(self.at))
        }
        fn focus(&mut self) -> Result<(), String> {
            self.did.push("focus".into());
            Ok(())
        }
        fn lease(&mut self, ms: u32) -> Result<(), String> {
            self.did.push(format!("lease {ms}"));
            let mut g = Guard::new(1_000_000);
            g.want(self.at + i64::from(ms) * 1000);
            self.guard = Some(g);
            Ok(())
        }
        fn hold(&mut self, c: Change) -> Result<(), String> {
            self.did.push(format!("hold {c}"));
            Ok(())
        }
        fn await_held(&mut self) -> Result<(), String> {
            self.did.push("held".into());
            Ok(())
        }
        fn wait(&mut self, ms: u64) -> Result<(), String> {
            self.did.push(format!("wait {ms}"));
            // The guard over the wait, looks 5 ms apart.
            let token = super::super::link::fake::TOKEN;
            let pings: Vec<_> = (0..4)
                .map(|k| test_device::clock::Ping {
                    sent: k * 1000 - 50,
                    device: 9_000_000 + u64::try_from(k * 1000).unwrap(),
                    received: k * 1000 + 50,
                })
                .collect();
            let end = self.at + i64::try_from(ms).unwrap() * 1000;
            let mut granted = None;
            while self.at < end {
                self.at += 5000;
                if let Some(g) = self.guard.as_mut() {
                    let view = View {
                        token: Some(token),
                        grant: granted,
                        pings: pings.clone(),
                        stopped: false,
                    };
                    let look = Look {
                        at: self.at,
                        front: true,
                        placed: true,
                        beat: self.at,
                    };
                    if let Act::Send { cmd, .. } = g.decide(&look, &view) {
                        if matches!(cmd, Command::Lease { .. }) {
                            granted = Some(Grant::test(LeaseId { token, n: 1 }, 0));
                        }
                        self.sent.push(cmd);
                    }
                }
            }
            Ok(())
        }
        fn blocking(&mut self, what: &str) -> Result<(), String> {
            self.did.push(format!("blocking {what}"));
            Ok(())
        }
    }

    /// The row's 3 s lease goes out as slices of 250 ms, renewed until the
    /// row's lease ends, and never as one lease of the row's length.
    #[test]
    fn every_lease_is_sliced() {
        let mut x = Steps::default();
        let steps = [Step::Lease { ms: 3000 }, Step::Wait { ms: 4000 }];
        perform(&steps, &mut x).unwrap();
        let leases: Vec<u32> = x
            .sent
            .iter()
            .filter_map(|c| match c {
                Command::Lease { ms, .. } => Some(*ms),
                _ => None,
            })
            .collect();
        assert_eq!(leases, [SLICE_MS]);
        let renewals = x
            .sent
            .iter()
            .filter(|c| matches!(c, Command::Renew { .. }))
            .count();
        // About 3 s / 83 ms of renewals, none after the row's lease.
        assert!((30..=40).contains(&renewals), "{renewals}");
        assert!(!x.leased());
    }

    #[test]
    fn only_a_bare_commit_build_without_faults_takes_a_hold() {
        assert_eq!(may_hold(BUILD, &hello(BUILD, false)), Ok(()));
        // The H2 control: the run's bare id with `faults 1`.
        assert!(may_hold(BUILD, &hello(BUILD, true)).is_err());
        for build in [
            "0123456789ab-dirty",
            "0123456789ab-faults",
            "dev",
            "nogit",
            "0123456789AB",
            "0123456789a",
        ] {
            assert!(may_hold(build, &hello(build, false)).is_err(), "{build}");
        }
        // Another bare id than the run's, with `faults 0`.
        assert!(may_hold(BUILD, &hello("ba9876543210", false)).is_err());
    }

    #[test]
    fn a_blocking_step_is_refused_under_a_lease() {
        let mut x = Steps::default();
        perform(&[Step::Blocking("rediscovery")], &mut x).unwrap();
        assert_eq!(x.did, ["blocking rediscovery"]);
        let mut x = Steps::default();
        let e = perform(
            &[
                Step::Lease { ms: 3000 },
                Step::Blocking("rediscovery"),
                Step::Focus,
            ],
            &mut x,
        )
        .unwrap_err();
        assert!(e.contains("before the lease"), "{e}");
        assert_eq!(x.did, ["lease 3000"]);
        // After the lease has run out, it is taken again.
        let mut x = Steps::default();
        perform(
            &[
                Step::Lease { ms: 100 },
                Step::Wait { ms: 200 },
                Step::Blocking("rediscovery"),
            ],
            &mut x,
        )
        .unwrap();
    }

    /// No row step writes a `release`, or any command but a held change
    /// under the grant: the end's `stop_holds` is the only release request.
    #[test]
    fn no_row_step_requests_a_release() {
        let id = LeaseId {
            token: super::super::link::fake::TOKEN,
            n: 1,
        };
        for name in ["sink", "sink-early-close"] {
            let (steps, _) = row(name).unwrap();
            for s in &steps {
                match command(s, id) {
                    None => {}
                    Some(Command::Set { id: got, .. }) => assert_eq!(got, id),
                    Some(c) => panic!("row {name}'s step {s:?} writes `{c}`"),
                }
            }
            // Positive control: the row does hold.
            assert!(steps.iter().any(|s| command(s, id).is_some()));
        }
    }

    #[test]
    fn a_pad_record_s_buttons_are_read_with_or_without_its_report_id() {
        let r = PadReport {
            buttons: 0b101,
            ..PadReport::default()
        }
        .encode();
        let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(pad_buttons(&hex), Some(0b101));
        assert_eq!(pad_buttons(&format!("00{hex}")), Some(0b101));
        assert_eq!(pad_buttons("0102"), None);
    }
}
