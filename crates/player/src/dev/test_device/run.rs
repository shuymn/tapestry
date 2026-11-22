//! `tapestry test-device run <row> …` (docs/test-device.md "Run Guard",
//! "Host Commands"): one run of a row. Rows are code: each is a list of
//! typed steps. The runner reserves the run's folder, joins its own job,
//! starts the recorder in it, holds the device's port on its link thread,
//! opens its sink, and runs the row's steps under the guard; every way out
//! goes through the end ([`super::end`]).
//!
//! Rows: `sink` (N1: a focus click on the sink, a lease of 3 s, button 1
//! and F24 held, 1 s, the end) and `sink-kill` (N2: an outer run that starts an inner `sink` in a job
//! of its own, terminates that job with its cut code once the inner's
//! recorder shows the presses, observes the inner's and its recorder's
//! ends, and opens its own connection). The panel rows start the player:
//! `p1` (P1: the device's report mode set, the player started in a
//! 1920×1080 window on the run's monitor, a focus click on it, the mapping
//! calibration, buttons 9 and 10 held under a lease, one guarded click at
//! each volume target, shot A, the buttons released, shot B, the end, then
//! the player closed and its settings copied), `p5` (P1's steps, run
//! against a player whose volume path is broken), `p4` (button 9 only),
//! and `p1-kill` (N2p: `sink-kill`'s outer over an inner `p1` that waits
//! at its kill barrier after shot A, both buttons held). The reference rows
//! attach to the reference player's window (`--attach <pid>`), hold nothing
//! on the device and take screen captures of it: `p1-ref` (a guarded click
//! at each volume target, then two captures of shot A) and `p1-ref-75`
//! (slider 17 at 75 %, the comparison's control).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use test_device::clock::ticks_to_us;
use test_device::evidence::{Process, Recorder as Rec, Reserved};
use test_device::mode::{Mode, Rate, Reporting};
use test_device::panel::Target;
use test_device::protocol::{Change, Command, Control, Token};
use test_device::report::{PAD_REPORT, PadReport};

use super::attach;
use super::capture;
use super::click::{self, Win};
use super::desktop::{self, Real};
use super::devices::{self, System};
use super::end;
use super::guard::{self, Look, Watch};
use super::job::{self, Child, Job, Pipe, Stdio};
use super::launch;
use super::link::{Connection, hello_facts};
use super::owner::{self, Stream};
use super::release::{Refused, Writer};
use super::shot::{self, Shots};
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
    /// The player a panel row starts.
    pub player: Option<launch::Player>,
    /// The sha256 the player's executable must have: the build a row runs
    /// against (P5's mutated player), refused when another is found.
    pub player_sha256: Option<String>,
    /// The process a reference row attaches to: the reference player.
    pub attach: Option<u32>,
    /// An inner `p1` of `p1-kill`: wait at the kill barrier after shot A.
    pub kill_barrier: bool,
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
    /// Waits until the recorder shows the device's buttons all up, from
    /// its bound handles, after `held`, and records `released` with that
    /// report's counter.
    AwaitReleased,
    Wait {
        ms: u64,
    },
    /// Starts the player from the sink in front, lets it take the
    /// foreground, and makes its main window the run's window once it
    /// shows on the run's monitor.
    Launch,
    /// A focus click on the run's window at a client point.
    FocusAt {
        x: i32,
        y: i32,
    },
    /// The mapping calibration: a move-only call to each of the row's
    /// click points, the pointer read back at each.
    Calibrate,
    /// A guarded click on the run's window at a client point.
    Click {
        x: i32,
        y: i32,
    },
    /// A shot of the player's window, as `name` in the run's folder: the
    /// player's own copy of a frame it presented after the request.
    Shot(&'static str),
    /// Writes `barrier` and waits there, holding, for the outer to end
    /// the run: [`BARRIER_WAIT`] later it fails.
    Barrier,
    /// Makes the attached process's main window the run's window (a
    /// reference row).
    Attach,
    /// Two guarded screen captures of the attached window's client area, of
    /// one shot: its file and its second file, at least [`CAPTURE_GAP`]
    /// apart.
    Capture([&'static str; 2]),
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

/// The focus click's point on the player: the client's left edge, middle,
/// where the select skin draws nothing that acts.
pub const FOCUS_POINT: (i32, i32) = (5, test_device::panel::CLIENT[1] / 2);
/// How long the player is given to settle on song selection (its library
/// and skin loaded) before the focus click.
pub const SETTLE_MS: u64 = 20_000;
/// The shots, by their file names in the run's folder.
pub const SHOT_A: &str = "shot-a.png";
pub const SHOT_B: &str = "shot-b.png";
/// START and SELECT, as the panel rows bind them.
pub const START: u8 = 9;
pub const SELECT: u8 = 10;

/// The steps of a panel row holding `buttons` (START and SELECT for
/// panel 3, START alone for panel 1); with `barrier`, the inner of
/// `p1-kill`, which waits at its barrier after shot A.
pub fn panel_steps(buttons: &[u8], barrier: bool) -> Vec<Step> {
    let set = |n, value| {
        Step::Hold(Change {
            control: Control::Button(n),
            value,
        })
    };
    let mut steps = vec![
        Step::Focus,
        Step::Launch,
        Step::Wait { ms: SETTLE_MS },
        Step::FocusAt {
            x: FOCUS_POINT.0,
            y: FOCUS_POINT.1,
        },
        Step::Calibrate,
        Step::Lease { ms: 30_000 },
    ];
    steps.extend(buttons.iter().map(|&b| set(b, 1)));
    steps.extend([Step::AwaitHeld, Step::Wait { ms: 500 }]);
    steps.extend(
        test_device::panel::TARGETS
            .iter()
            .map(|t| Step::Click { x: t.x, y: t.y }),
    );
    steps.extend([Step::Wait { ms: 500 }, Step::Shot(SHOT_A)]);
    if barrier {
        steps.push(Step::Barrier);
    }
    steps.extend(buttons.iter().map(|&b| set(b, 0)));
    steps.extend([
        Step::AwaitReleased,
        Step::Wait { ms: 800 },
        Step::Shot(SHOT_B),
    ]);
    steps
}

/// The client points a panel or reference row clicks: the focus point,
/// then each of its volume targets.
pub fn click_points(targets: &[Target]) -> Vec<(i32, i32)> {
    std::iter::once(FOCUS_POINT)
        .chain(targets.iter().map(|t| (t.x, t.y)))
        .collect()
}

/// The volume targets row `row` clicks: the reference control's for
/// `p1-ref-75`, the panel targets for every other row.
#[must_use]
pub fn targets_of(row: &str) -> &'static [Target; 3] {
    if row == "p1-ref-75" {
        &test_device::panel::REF_75
    } else {
        &test_device::panel::TARGETS
    }
}

/// How far apart a reference row's two captures are at least: three frame
/// intervals at 60 Hz, so a frame the compositor had not settled when the
/// first was copied differs in the second.
pub const CAPTURE_GAP: Duration = Duration::from_millis(50);

/// Shot A's second capture in a reference row.
pub const SHOT_A_2: &str = "shot-a-2.png";

/// The steps of a reference row clicking `targets`: the sink's focus
/// click, the attachment, a focus click on the reference's window, the
/// mapping calibration, one guarded click at each target, and the two
/// captures of shot A. Nothing is held on the device: the reference's
/// panel 3 is opened by its own start.
pub fn ref_steps(targets: &[Target]) -> Vec<Step> {
    let mut steps = vec![
        Step::Focus,
        Step::Attach,
        Step::FocusAt {
            x: FOCUS_POINT.0,
            y: FOCUS_POINT.1,
        },
        Step::Calibrate,
        Step::Wait { ms: 500 },
    ];
    steps.extend(targets.iter().map(|t| Step::Click { x: t.x, y: t.y }));
    steps.extend([Step::Wait { ms: 500 }, Step::Capture([SHOT_A, SHOT_A_2])]);
    steps
}

/// The calls a row's `steps` make, in order: what each sends and the
/// client point it aims at (`None` for the sink's focus click, at the
/// sink's centre whatever its size): the runner's side, which the judge's
/// own table ([`super::judge::panel_calls`]) is tested against.
#[cfg(test)]
pub fn calls_of(steps: &[Step]) -> Vec<(&'static str, Option<(i32, i32)>)> {
    let mut out = Vec::new();
    for s in steps {
        match *s {
            Step::Focus => out.extend([("move", None), ("click", None)]),
            Step::FocusAt { x, y } | Step::Click { x, y } => {
                out.extend([("move", Some((x, y))), ("click", Some((x, y)))]);
            }
            Step::Calibrate => {
                // The focus point and every target the row clicks.
                let targets = steps.iter().filter_map(|s| match *s {
                    Step::Click { x, y } => Some((x, y)),
                    _ => None,
                });
                out.extend(
                    std::iter::once(FOCUS_POINT)
                        .chain(targets)
                        .map(|p| ("calibrate", Some(p))),
                );
            }
            _ => {}
        }
    }
    out
}

/// The rows with steps of their own; `barrier` puts `p1`'s kill barrier in.
/// N1's negative, `sink-early-close`, is not one: closing the sink before
/// the release evidence would let a held input land in another window, so
/// no runner performs it; its expectation is checked on a run folder a
/// test writes (`check::tests`).
pub fn row(name: &str, barrier: bool) -> Option<Vec<Step>> {
    match (name, barrier) {
        ("sink", false) => Some(sink_steps()),
        ("p1" | "p5", _) => Some(panel_steps(&[START, SELECT], barrier)),
        ("p4", false) => Some(panel_steps(&[START], false)),
        ("p1-ref" | "p1-ref-75", false) => Some(ref_steps(targets_of(name))),
        _ => None,
    }
}

/// An outer row: the inner row it reserves and kills, the inner's extra
/// arguments, and the record the outer waits for before the kill.
struct Outer {
    inner: &'static str,
    barrier: bool,
    waits_for: &'static str,
    /// How long it waits for that record.
    wait: Duration,
}

/// The outer rows.
fn outer_of(row: &str) -> Option<Outer> {
    match row {
        "sink-kill" => Some(Outer {
            inner: "sink",
            barrier: false,
            waits_for: "held",
            wait: INNER_HELD_WAIT,
        }),
        "p1-kill" => Some(Outer {
            inner: "p1",
            barrier: true,
            waits_for: "barrier",
            wait: INNER_BARRIER_WAIT,
        }),
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
/// with `faults 0`, and, for a row whose verdicts need the report mode
/// (the panel rows), reports in `mode` on this connection.
///
/// # Errors
///
/// Why not.
pub fn may_hold(build: &str, line: &str, mode: Option<Mode>) -> Result<(), String> {
    if !bare_commit(build) {
        return Err(format!(
            "`{build}` is not a bare commit id (12 hex digits): no hold is taken"
        ));
    }
    let facts = hello_facts(line, Some(build))?;
    match mode {
        Some(m) if facts.mode != m => Err(format!(
            "the device reports in mode {:?} on the driving connection, not {m:?}: no hold is taken",
            facts.mode
        )),
        _ => Ok(()),
    }
}

/// What the steps act through.
pub trait Exec {
    fn focus(&mut self) -> Result<(), String>;
    fn lease(&mut self, ms: u32) -> Result<(), String>;
    fn hold(&mut self, change: Change) -> Result<(), String>;
    fn await_held(&mut self) -> Result<(), String>;
    fn await_released(&mut self) -> Result<(), String>;
    fn wait(&mut self, ms: u64) -> Result<(), String>;
    fn launch(&mut self) -> Result<(), String>;
    fn focus_at(&mut self, point: (i32, i32)) -> Result<(), String>;
    fn calibrate(&mut self) -> Result<(), String>;
    fn click(&mut self, point: (i32, i32)) -> Result<(), String>;
    fn shot(&mut self, name: &'static str) -> Result<(), String>;
    fn barrier(&mut self) -> Result<(), String>;
    fn attach(&mut self) -> Result<(), String>;
    fn capture(&mut self, files: [&'static str; 2]) -> Result<(), String>;
}

/// Runs `steps`: the first error ends them.
///
/// # Errors
///
/// A step's error.
pub fn perform(steps: &[Step], x: &mut dyn Exec) -> Result<(), String> {
    for s in steps {
        match *s {
            Step::Focus => x.focus(),
            Step::Lease { ms } => x.lease(ms),
            Step::Hold(c) => x.hold(c),
            Step::AwaitHeld => x.await_held(),
            Step::AwaitReleased => x.await_released(),
            Step::Wait { ms } => x.wait(ms),
            Step::Launch => x.launch(),
            Step::FocusAt { x: px, y } => x.focus_at((px, y)),
            Step::Calibrate => x.calibrate(),
            Step::Click { x: px, y } => x.click((px, y)),
            Step::Shot(name) => x.shot(name),
            Step::Barrier => x.barrier(),
            Step::Attach => x.attach(),
            Step::Capture(files) => x.capture(files),
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
/// How long a run waits at its kill barrier before it fails.
pub const BARRIER_WAIT: Duration = Duration::from_secs(30);

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

/// The records of a stream another process appends to, read whole lines at
/// a time through one handle, opened once the file exists: opening the
/// file again at every poll costs a step thread's poll up to tens of
/// milliseconds when the file system's filters look at the open, which
/// starved the guard's heartbeat on the development PC.
struct Tail {
    path: PathBuf,
    file: Option<std::fs::File>,
    partial: Vec<u8>,
}

impl Tail {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            file: None,
            partial: Vec::new(),
        }
    }

    /// The records written since the last read.
    fn read(&mut self) -> Vec<Value> {
        use std::io::Read as _;
        if self.file.is_none() {
            self.file = std::fs::File::open(&self.path).ok();
        }
        let Some(f) = self.file.as_mut() else {
            return Vec::new();
        };
        if f.read_to_end(&mut self.partial).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice(line.trim_ascii_end()) {
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

/// Whether recorder record `v` is a neutral report of the device's pad (no
/// button down), from its bound handles.
fn neutral(v: &Value) -> bool {
    v["ours"] == true
        && v["t"] == "pad"
        && v["bytes"]
            .as_str()
            .and_then(pad_buttons)
            .is_some_and(|b| b == 0)
}

/// The run's live state: what the steps and the guard act on.
struct Live {
    writer: Writer,
    /// The guard, started by the row's lease on the run's window then.
    guard: Option<(guard::Handle, JoinHandle<()>)>,
    beat: Arc<guard::Beat>,
    /// The run's window: the sink, then the player's main window once it
    /// shows.
    window: Win,
    monitor: String,
    monitor_then: [i32; 4],
    prefix: u16,
    call: u16,
    log: Log,
    recorder: Tail,
    /// The recorder's process and file, for the guard's looks.
    recorder_process: Process,
    /// The recorder's records read and not yet looked at.
    backlog: Vec<Value>,
    held: Vec<Change>,
    freq: i64,
    /// The run's folder and job, for the player and its shots.
    dir: PathBuf,
    job: &'static Job,
    player: Option<launch::Player>,
    /// The player once started, and its main window once shown.
    launched: Option<Launched>,
    /// The pipe the player's shots are asked for on, for a row that starts
    /// the player.
    shot_pipe: Option<shot::Pipe>,
    shots: Shots,
    /// The row's volume targets.
    targets: &'static [Target; 3],
    /// The process a reference row attaches to, and once attached, that
    /// process and its window's client area then.
    attach: Option<u32>,
    attached: Option<(Child, [i32; 4])>,
}

/// The player a run started.
struct Launched {
    child: Child,
    window: Option<Win>,
}

impl Live {
    fn beat(&self) {
        self.beat.beat(qpc());
    }

    /// The pad buttons the row holds, as report bits.
    fn held_buttons(&self) -> u16 {
        self.held
            .iter()
            .filter_map(|c| match c.control {
                Control::Button(n) => Some(1 << (n - 1)),
                _ => None,
            })
            .fold(0, |a, b| a | b)
    }

    /// A guarded click (`focus`: a focus click) of the run's window at
    /// client `point`.
    fn guarded(&mut self, point: (i32, i32), focus: bool) -> Result<(), String> {
        let log = self.log.clone();
        let mut note = |tag: u32, what: &str, point: (i32, i32)| {
            log.put(json!({
                "t": "call", "q": qpc(), "tag": tag, "what": what, "point": [point.0, point.1],
            }));
        };
        let mut desk = Real {
            monitor: self.monitor.clone(),
            note: &mut note,
        };
        let r = click::click(
            &mut desk,
            self.window,
            point,
            focus,
            self.monitor_then,
            self.prefix,
            &mut self.call,
        );
        self.beat();
        r.map_err(|e| format!("the click at {point:?}: {e}"))
    }

    /// Waits up to `wait` while beating and looking at the link and the
    /// recorder; `done` ends it early.
    fn idle(&mut self, wait: Duration) -> Result<(), String> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            self.beat();
            self.lost()?;
            self.read_recorder()?;
            self.backlog.clear();
            std::thread::sleep(LOOK);
        }
        Ok(())
    }

    fn lost(&self) -> Result<(), String> {
        match self.writer.stopped() {
            Some(why) => Err(format!("holds stopped: {why}")),
            None => Ok(()),
        }
    }

    /// Reads the recorder's new records into the backlog: an `error`
    /// record ends the steps, since the recorder can no longer vouch for
    /// where held input goes.
    fn read_recorder(&mut self) -> Result<(), String> {
        for v in self.recorder.read() {
            if v["t"] == "error" {
                return Err(format!("the recorder reported an error: {v}"));
            }
            self.backlog.push(v);
        }
        Ok(())
    }
}

impl Exec for Live {
    fn focus(&mut self) -> Result<(), String> {
        let client = desktop::client_rect(self.window).ok_or("the run's window is gone")?;
        self.guarded((client[2] / 2, client[3] / 2), true)
    }

    fn lease(&mut self, ms: u32) -> Result<(), String> {
        self.beat();
        if self.guard.is_none() {
            let client = desktop::client_rect(self.window).ok_or("the run's window is gone")?;
            let recorder = job::open(self.recorder_process)
                .map_err(|e| format!("the recorder's process: {e:#}"))?;
            // The heartbeat's gaps count from the guard's start.
            self.beat.arm(qpc());
            self.guard = Some(guard::start(
                self.freq,
                Box::new(Looks {
                    window: self.window,
                    client,
                    monitor: self.monitor.clone(),
                    monitor_then: self.monitor_then,
                    monitor_now: Some(self.monitor_then),
                    read: Instant::now(),
                    beat: Arc::clone(&self.beat),
                    recorder,
                    recorder_file: self.recorder.path.clone(),
                    recorder_growth: Growth::new(Instant::now()),
                }),
                Arc::new(self.writer.clone()),
            ));
        }
        let Some((guard, _)) = &self.guard else {
            return Err("no guard".into());
        };
        guard.want(qpc() + i64::from(ms) * self.freq / 1000);
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
        if change.value != 0 {
            self.held.push(change);
        }
        Ok(())
    }

    fn await_held(&mut self) -> Result<(), String> {
        let buttons = self.held_buttons();
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
            self.read_recorder()?;
            for v in self.backlog.drain(..) {
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

    fn await_released(&mut self) -> Result<(), String> {
        let end = Instant::now() + HELD_WAIT;
        loop {
            self.beat();
            self.lost()?;
            if Instant::now() >= end {
                return Err(format!(
                    "the recorder did not show the buttons released within {HELD_WAIT:?}"
                ));
            }
            self.read_recorder()?;
            let up = self.backlog.drain(..).find(neutral);
            if let Some(v) = up {
                self.log.put(json!({ "t": "released", "report": v["q"] }));
                return Ok(());
            }
            std::thread::sleep(LOOK);
        }
    }

    fn wait(&mut self, ms: u64) -> Result<(), String> {
        self.idle(Duration::from_millis(ms))
    }

    fn launch(&mut self) -> Result<(), String> {
        let spec = self
            .player
            .clone()
            .ok_or("the row starts a player, and none is given")?;
        if desktop::foreground() != self.window {
            return Err("the sink is not in front as the player starts".into());
        }
        let pipe = self
            .shot_pipe
            .as_ref()
            .map(|p| p.name.clone())
            .ok_or("the row starts a player, and the run has no shot pipe")?;
        let child = spec
            .spawn(self.job, &self.dir, &pipe)
            .map_err(|e| format!("starting the player: {e:#}"))?;
        self.log.put(json!({
            "t": "spawn", "name": "player",
            "process": { "pid": child.process.pid, "created": child.process.created },
        }));
        let pid = child.process.pid;
        self.launched = Some(Launched {
            child,
            window: None,
        });
        let beat = || self.beat();
        launch::grant(pid, &beat).map_err(|e| format!("{e:#}"))?;
        let alive = || {
            self.launched
                .as_ref()
                .is_some_and(|l| l.child.wait(Duration::ZERO) == Ok(None))
        };
        let (window, _) =
            launch::window(pid, self.monitor_then, &alive, &beat).map_err(|e| format!("{e:#}"))?;
        self.log.put(json!({ "t": "window", "window": window }));
        if let Some(l) = self.launched.as_mut() {
            l.window = Some(window);
        }
        self.window = window;
        Ok(())
    }

    fn focus_at(&mut self, point: (i32, i32)) -> Result<(), String> {
        self.guarded(point, true)
    }

    fn calibrate(&mut self) -> Result<(), String> {
        let log = self.log.clone();
        let mut note = |tag: u32, what: &str, point: (i32, i32)| {
            log.put(json!({
                "t": "call", "q": qpc(), "tag": tag, "what": what, "point": [point.0, point.1],
            }));
        };
        let mut desk = Real {
            monitor: self.monitor.clone(),
            note: &mut note,
        };
        let r = click::calibrate(
            &mut desk,
            self.window,
            &click_points(self.targets),
            self.prefix,
            &mut self.call,
        );
        self.beat();
        r.map_err(|e| format!("the mapping calibration: {e}"))
    }

    fn click(&mut self, point: (i32, i32)) -> Result<(), String> {
        self.lost()?;
        self.guarded(point, false)
    }

    fn shot(&mut self, name: &'static str) -> Result<(), String> {
        self.lost()?;
        self.beat();
        // The player's own copy of a frame it presented after the request,
        // from the player the run started (the pipe's client process).
        let player = self
            .launched
            .as_ref()
            .filter(|l| l.window.is_some())
            .map(|l| l.child.process)
            .ok_or_else(|| format!("the shot {name}: the run shows no player window"))?;
        let alive = || {
            self.launched
                .as_ref()
                .is_some_and(|l| l.child.wait(Duration::ZERO) == Ok(None))
        };
        if !alive() {
            return Err(format!("the shot {name}: the player has ended"));
        }
        let pipe = self
            .shot_pipe
            .as_mut()
            .ok_or_else(|| format!("the shot {name}: the run has no shot pipe"))?;
        let requested = qpc();
        let serial = pipe
            .request()
            .map_err(|e| format!("the shot {name}: {e}"))?;
        let end = Instant::now() + shot::SHOT_WAIT;
        let answer = loop {
            self.beat();
            self.lost()?;
            if Instant::now() >= end {
                return Err(format!(
                    "the shot {name}: the player answered no frame within {:?}",
                    shot::SHOT_WAIT
                ));
            }
            let Some(pipe) = self.shot_pipe.as_ref() else {
                return Err(format!("the shot {name}: the run has no shot pipe"));
            };
            if let Some(a) = pipe
                .answer(LOOK)
                .map_err(|e| format!("the shot {name}: {e}"))?
            {
                break a;
            }
        };
        self.beat();
        let client = answer.client;
        let frame = shot::check_answer(answer, serial, player.pid, requested)
            .map_err(|e| format!("the shot {name}: {e}"))?;
        if !alive() {
            return Err(format!("the shot {name}: the player has ended"));
        }
        self.log.put(json!({
            "t": "shot", "what": name,
            "process": { "pid": player.pid, "created": player.created },
            "client": client,
            "size": [frame.width, frame.height],
            "requested": requested,
            "frame": {
                "previous": frame.previous, "present": frame.present,
                "acquired": frame.acquired, "presented": frame.presented,
            },
            "volumes": crate::dev::present_shot::volumes_json(&frame.volumes),
        }));
        self.beat();
        self.shots
            .write(self.dir.join(name), frame.width, frame.height, frame.rgba)
    }

    fn barrier(&mut self) -> Result<(), String> {
        self.log.put(json!({ "t": "barrier" }));
        self.idle(BARRIER_WAIT)?;
        Err(format!(
            "the run was not ended within {BARRIER_WAIT:?} of its kill barrier"
        ))
    }

    fn attach(&mut self) -> Result<(), String> {
        let pid = self
            .attach
            .ok_or("the row attaches to a process, and none is given")?;
        let child = job::observe(pid).map_err(|e| format!("the attached process {pid}: {e:#}"))?;
        let window = desktop::main_window_of(pid);
        let owner = window.map_or(0, desktop::process_of);
        let client = window.and_then(desktop::client_rect);
        let w = attach::check(
            pid,
            window,
            owner,
            client,
            test_device::panel::CLIENT,
            self.monitor_then,
        )?;
        let client = client.ok_or("the attached window has no client area")?;
        let p = child.process;
        self.log.put(json!({
            "t": "attach", "process": { "pid": p.pid, "created": p.created },
            "window": w, "client": client,
        }));
        self.window = w;
        self.attached = Some((child, client));
        Ok(())
    }

    fn capture(&mut self, files: [&'static str; 2]) -> Result<(), String> {
        let name = files[0];
        self.lost()?;
        self.beat();
        let window = self.window;
        let (process, client) = match &self.attached {
            Some((child, client)) => {
                if child.wait(Duration::ZERO) != Ok(None) {
                    return Err(format!(
                        "the capture {name}: the attached process has ended"
                    ));
                }
                (child.process, *client)
            }
            None => {
                return Err(format!(
                    "the capture {name}: the run attached to no process"
                ));
            }
        };
        let mut grabs = Vec::new();
        for (i, file) in files.into_iter().enumerate() {
            if i > 0 {
                self.idle(CAPTURE_GAP)?;
            }
            let mut quiet = |_: u32, _: &str, _: (i32, i32)| {};
            let mut screen = Real {
                monitor: self.monitor.clone(),
                note: &mut quiet,
            };
            let before = qpc();
            let bgra = capture::guarded(&mut screen, window, self.monitor_then, client)
                .map_err(|e| format!("the capture {file}: {e}"))?;
            let after = qpc();
            self.beat();
            let (w, h) = (
                u32::try_from(client[2]).unwrap_or(0),
                u32::try_from(client[3]).unwrap_or(0),
            );
            self.shots
                .write(self.dir.join(file), w, h, capture::rgba(&bgra))?;
            grabs.push(json!({ "file": file, "before": before, "after": after }));
        }
        self.log.put(json!({
            "t": "capture", "what": name,
            "process": { "pid": process.pid, "created": process.created },
            "window": window, "client": client, "grabs": grabs,
        }));
        Ok(())
    }
}

/// The guard's real look at the run.
struct Looks {
    window: Win,
    client: [i32; 4],
    monitor: String,
    monitor_then: [i32; 4],
    monitor_now: Option<[i32; 4]>,
    read: Instant,
    beat: Arc<guard::Beat>,
    /// The recorder's process, by a handle of the guard's own.
    recorder: Child,
    /// The recorder's file, and its growth: the recorder writes a `beat`
    /// every 20 ms.
    recorder_file: PathBuf,
    recorder_growth: Growth,
}

/// How recently the recorder's file must have grown for a look to count
/// it as recording: three of its beats.
const RECORDER_FRESH: Duration = Duration::from_millis(60);

/// Whether a file its writer appends to has grown lately: its size at
/// the last look, and when it last grew. A size that could not be read is
/// no growth.
struct Growth {
    size: u64,
    grew: Instant,
}

impl Growth {
    const fn new(now: Instant) -> Self {
        Self { size: 0, grew: now }
    }

    /// Looks at the file's size `size` at `now`: whether it grew within
    /// [`RECORDER_FRESH`].
    fn look(&mut self, size: Option<u64>, now: Instant) -> bool {
        if let Some(n) = size
            && n > self.size
        {
            self.size = n;
            self.grew = now;
        }
        now.saturating_duration_since(self.grew) <= RECORDER_FRESH
    }
}

impl Watch for Looks {
    fn look(&mut self) -> Look {
        if self.read.elapsed() >= guard::MONITOR_EVERY {
            self.monitor_now = desktop::monitor(&self.monitor).ok().map(|m| m.area);
            self.read = Instant::now();
        }
        let size = std::fs::metadata(&self.recorder_file).ok().map(|m| m.len());
        let growing = self.recorder_growth.look(size, Instant::now());
        let at = qpc();
        let client = desktop::client_rect(self.window);
        let placed = self.monitor_now == Some(self.monitor_then)
            && client == Some(self.client)
            && click::contains(self.monitor_then, self.client);
        let front = desktop::foreground() == self.window;
        let recording = self.recorder.wait(Duration::ZERO) == Ok(None) && growing;
        // Last, with the counter read after it: a beat after `at` cannot
        // hide a gap, which the heartbeat latched.
        let beat_age = self.beat.age(qpc);
        Look {
            at,
            front,
            placed,
            beat_age,
            recording,
        }
    }
}

/// How long the guard's thread may take to end once asked.
const GUARD_WAIT: Duration = Duration::from_secs(2);

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

/// Terminates `name`, process `p`, by `terminate` (with the runner's cut
/// code) and, once the call returned, writes its `kill` record: the host
/// counter read just before the call (`requested`) and the record's own
/// after it. A call that failed writes none.
///
/// # Errors
///
/// The call's.
fn kill_confirmed<E>(
    log: &Log,
    name: &str,
    p: Process,
    terminate: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let requested = qpc();
    terminate()?;
    log.put(json!({
        "t": "kill", "name": name, "requested": requested,
        "process": { "pid": p.pid, "created": p.created },
    }));
    Ok(())
}

/// Waits up to `wait` for `c` to end; else records its kill, terminates it
/// with the cut code and waits for that end. The exit is recorded only once
/// observed: its code, or why its end was not observed (a timeout, or a
/// wait that failed).
fn end_child(log: &Log, name: &str, c: &Child, wait: Duration, cut: i32) -> Result<i32, String> {
    let code = match c.wait(wait).map_err(|e| format!("{name}: {e}"))? {
        Some(code) => code,
        None => {
            kill_confirmed(log, name, c.process, || c.kill(cut.cast_unsigned()))
                .map_err(|e| format!("terminating {name}: {e:#}"))?;
            c.wait(KILL_WAIT)
                .map_err(|e| format!("{name}: {e}"))?
                .ok_or_else(|| {
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
    /// The device's bound location.
    location: String,
    /// Its exit code, once its end was observed: only then is its stream
    /// final, and sealed.
    ended: Option<i32>,
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
            location: port.to_owned(),
            ended: None,
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
            if let Some(code) = self
                .child
                .wait(Duration::ZERO)
                .map_err(anyhow::Error::msg)?
            {
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
    fn removed(&mut self) -> Result<bool, String> {
        devices::input_interfaces_at(&self.location).map(|found| found.is_empty())
    }

    fn stop(&mut self) -> Result<(), String> {
        drop(self.input.take());
        let code = end_child(
            &self.log,
            self.name,
            &self.child,
            Duration::from_secs(5),
            self.cut,
        )?;
        self.ended = Some(code);
        match code {
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
    /// The link thread ended within the deadline and closed its stream
    /// without an error: only then is the stream final, and sealed.
    closed: bool,
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
        // A link thread that has not ended keeps its stream: the runner
        // never takes the stream's lock, and leaves it unsealed.
        owner::join_within(t, "the link", deadline)?;
        let s = self.writer.state();
        if let Some(e) = s.log_error {
            return Err(format!("the link's stream: {e}"));
        }
        self.closed = true;
        match s.lost {
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
    fn front(&mut self) -> Result<(), String> {
        desktop::bring_to_front(self.window)
    }
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
    /// Whether the run seals its own files: not when it waits at a kill
    /// barrier, since a file whose writer is cut short is sealed only by the
    /// runner that cut it.
    seals: bool,
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
            let (outer, run, dir) = owner::reserved(set, role, &a.row, &a.build)?;
            (dir, run, role.clone(), Some(outer))
        }
        _ => bail!("a run takes `--out <dir>`, or `--set <dir> --role <role>`"),
    };
    let job: &'static Job = Box::leak(Box::new(Job::join()?));
    let stream = Stream::create(&dir.join(test_device::contract::RUN))?;
    let log = Log(stream.clone());
    // Never 0: an untagged input's extra information is 0.
    let prefix = u16::from_str_radix(&owner::random_hex(2)?, 16).context("the tag prefix")? | 1;
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
        seals: !a.kill_barrier,
    })
}

/// Seals `name` in the run's folder.
fn seal(b: &Begun, dir: &Path, name: &str, run: &str, role: &str, errors: &mut Vec<String>) {
    match owner::seal(dir, &dir.join(name), run, role) {
        Ok(v) => b.log.put(v),
        Err(e) => errors.push(format!("sealing {name}: {e:#}")),
    }
}

/// Seals the run's own file `name`, unless the run waits at a kill barrier.
fn seal_file(b: &Begun, name: &str, errors: &mut Vec<String>) {
    if b.seals {
        seal(b, &b.dir, name, &b.run, &b.role, errors);
    }
}

/// Seals the run's own link and recorder streams, each only once its
/// writer's end was observed; an unsealed one is an error.
fn seal_own(b: &Begun, link_closed: bool, recorder_ended: bool, errors: &mut Vec<String>) {
    for (name, final_) in [
        (test_device::contract::LINK, link_closed),
        (test_device::contract::RECORDER, recorder_ended),
    ] {
        if final_ {
            seal(b, &b.dir, name, &b.run, &b.role, errors);
        } else {
            errors.push(format!(
                "{name} is left unsealed: its writer's end was not observed"
            ));
        }
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

/// The report mode the panel rows run in: 125 Hz, on change.
pub const PANEL_MODE: Mode = Mode {
    rate: Rate::Hz125,
    reporting: Reporting::OnChange,
};
/// The mode step's link stream in the run's folder.
pub const MODE_LINK: &str = "mode.jsonl";
/// How long the device may take to come back after a mode change.
const MODE_BACK: Duration = Duration::from_secs(15);
/// How long the device may take to answer `mode`.
const MODE_ANSWER: Duration = Duration::from_secs(1);

/// The mode step of a panel row, before anything binds the device's
/// handles: a connection of its own reads the device's mode and, when it
/// is not [`PANEL_MODE`], sets it (the device stores it and reboots) and
/// waits for the device to come back in it at the bound location. A mode
/// changed under a running player would make the player find the device
/// after its other devices. The connections' records go to
/// [`MODE_LINK`], sealed; the `hello` that reads the mode goes to the
/// `mode` record.
fn mode_step(b: &Begun, a: &Args) -> anyhow::Result<()> {
    let journal = Stream::create(&b.dir.join(MODE_LINK))?;
    let r = mode_change(b, a, &journal);
    let problems: Vec<String> = r
        .as_ref()
        .err()
        .map(|e| format!("{e:#}"))
        .into_iter()
        .collect();
    journal.write(&json!({ "t": "end", "q": qpc(), "problems": problems }));
    if let Some(e) = journal.close() {
        bail!("{e}");
    }
    let line = r?;
    b.log.put(json!({ "t": "mode", "line": line }));
    let mut errors = Vec::new();
    seal_file(b, MODE_LINK, &mut errors);
    match errors.pop() {
        Some(e) => bail!("{e}"),
        None => Ok(()),
    }
}

/// [`mode_step`]'s connections: the `hello` line that reads
/// [`PANEL_MODE`].
fn mode_change(b: &Begun, a: &Args, journal: &Stream) -> anyhow::Result<String> {
    use super::link::{Event, What};
    let mut c = owner::rediscover(&System, &a.port, &a.build)?;
    c.identify(&b.run, b.process);
    let first = c.facts().context("no checked `hello`")?.clone();
    if first.mode == PANEL_MODE {
        for j in c.take_journal() {
            journal.write(&j);
        }
        return Ok(first.line);
    }
    let answered = (|| -> anyhow::Result<()> {
        c.send(Command::Mode(PANEL_MODE))?;
        let end = Instant::now() + MODE_ANSWER;
        loop {
            for e in c.poll()? {
                if let Event::Answer { line, to, .. } = e
                    && matches!(to.what, What::Command(Command::Mode(_)))
                {
                    if line == "ok" {
                        return Ok(());
                    }
                    bail!("the device answered `mode` with `{line}`");
                }
            }
            if Instant::now() >= end {
                bail!("no answer to `mode` within {MODE_ANSWER:?}");
            }
            std::thread::sleep(LOOK);
        }
    })();
    for j in c.take_journal() {
        journal.write(&j);
    }
    drop(c);
    answered?;
    // The device reboots once it has answered.
    std::thread::sleep(Duration::from_millis(1500));
    let end = Instant::now() + MODE_BACK;
    loop {
        match owner::rediscover(&System, &a.port, &a.build) {
            Ok(mut c) => {
                let facts = c.facts().context("no checked `hello`")?.clone();
                // One stream has one first record: this connection's
                // records go in without its `open`.
                for j in c.take_journal().into_iter().filter(|j| j["t"] != "open") {
                    journal.write(&j);
                }
                if facts.mode != PANEL_MODE {
                    bail!(
                        "the device came back in mode {:?}, not {PANEL_MODE:?}",
                        facts.mode
                    );
                }
                return Ok(facts.line);
            }
            Err(e) if Instant::now() >= end => {
                return Err(e.context(format!(
                    "the device did not come back within {MODE_BACK:?} of its mode change"
                )));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// The folder, in the player's scratch data folder, of the private copy
/// of the player a panel row starts.
pub const PLAYER_COPY: &str = "player-bin";

/// 1970-01-01 as a FILETIME (100 ns since 1601).
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

/// A file's last write as a FILETIME, refused before 1970: no build of the
/// player is that old, so a clock or a file system that says so is wrong.
///
/// # Errors
///
/// A last write before 1970.
pub fn last_write(filetime: u64) -> anyhow::Result<u64> {
    if filetime < UNIX_EPOCH_FILETIME {
        bail!("a last write before 1970 (FILETIME {filetime})");
    }
    Ok(filetime)
}

/// The digest a panel row expects of its player's held copy: the one the
/// row declared (`--player-sha256`, P5's mutated build), or else the
/// runner's own executable's, since every other row runs the build that
/// runs it.
#[must_use]
pub fn expected_player<'a>(declared: Option<&'a str>, runner: &'a str) -> &'a str {
    declared.unwrap_or(runner)
}

/// The held copy's digest against the one its row expects.
///
/// # Errors
///
/// The digests differ: the run must not start that player.
pub fn bind_player(source: &Path, sha: &str, expected: &str) -> anyhow::Result<()> {
    if sha != expected {
        bail!(
            "the player {} has sha256 {sha}, not the expected {expected}",
            source.display()
        );
    }
    Ok(())
}

/// The `player` record: the private copy a panel row starts (`path`) and
/// the executable it was copied from (`source`), the copy's sha256 as read
/// through its held handle, the sha256 the row expected for it (`expected`,
/// [`expected_player`]; one that differs is refused before anything
/// starts), the sha256 the row declared (`--player-sha256`, P5's alone),
/// the runner's own executable's sha256, the commit the copy reports
/// (`<copy> test-device commit`, run in the run's job with its output in
/// `player-commit.txt`) and the source's last write.
fn player_record(
    b: &Begun,
    source: &Path,
    v: &launch::Verified,
    declared: Option<&str>,
) -> anyhow::Result<()> {
    let sha = &v.sha256;
    let runner = owner::digest(&std::env::current_exe().context("the runner's own path")?)?;
    let expected = expected_player(declared, &runner);
    bind_player(source, sha, expected)?;
    let filetime = last_write(std::os::windows::fs::MetadataExt::last_write_time(
        &std::fs::metadata(source).with_context(|| source.display().to_string())?,
    ))?;
    let out = b.dir.join("player-commit.txt");
    let stdio = Stdio {
        input: Stdio::null()?,
        output: Stdio::file(owner::create(&out)?),
        error: Stdio::null()?,
    };
    let args = ["test-device".to_owned(), "commit".to_owned()];
    let child = job::spawn(b.job, &v.exe, &args, &[], &stdio)?;
    drop(stdio);
    match child
        .wait(Duration::from_secs(10))
        .map_err(anyhow::Error::msg)?
    {
        Some(0) => {}
        Some(code) => bail!("`{} test-device commit` exited {code}", v.exe.display()),
        None => {
            let _ = child.kill(b.cut.cast_unsigned());
            bail!(
                "`{} test-device commit` did not end within 10 s",
                v.exe.display()
            );
        }
    }
    let commit = std::fs::read_to_string(&out)
        .with_context(|| out.display().to_string())?
        .trim()
        .to_owned();
    b.log.put(json!({
        "t": "player", "path": v.exe.to_string_lossy(), "source": source.to_string_lossy(),
        "sha256": sha, "expected": expected, "declared": declared,
        "files": v.files.iter().map(|(name, sha256)| json!({ "name": name, "sha256": sha256 })).collect::<Vec<_>>(),
        "runner_sha256": runner, "commit": commit, "modified": filetime,
    }));
    Ok(())
}

/// The player as the end closes it: its window, its process, and the
/// records of what the runner did to it.
struct RealPlayer<'a> {
    child: &'a Child,
    window: Option<Win>,
    log: &'a Log,
    cut: i32,
    /// Its end was observed.
    ended: bool,
}

impl end::Player for RealPlayer<'_> {
    fn ask_close(&mut self) -> Result<(), String> {
        let w = self.window.ok_or("the player showed no window to close")?;
        desktop::close(w).map_err(|e| format!("{e:#}"))
    }

    fn wait(&mut self, wait: Duration) -> Result<Option<i32>, String> {
        let code = self.child.wait(wait)?;
        if let Some(code) = code {
            exit_record(self.log, "player", self.child.process, code);
            self.ended = true;
        }
        Ok(code)
    }

    fn kill(&mut self) -> Result<(), String> {
        kill_confirmed(self.log, "player", self.child.process, || {
            self.child.kill(self.cut.cast_unsigned())
        })
        .map_err(|e| format!("{e:#}"))?;
        match self.wait(KILL_WAIT)? {
            Some(_) => Ok(()),
            None => Err(format!(
                "it did not end within {KILL_WAIT:?} of its termination"
            )),
        }
    }
}

/// The run's windows as the end closes them: the player first (its close
/// asked, its end awaited, its settings copied and its files sealed), then
/// the sink.
struct Windows<'a> {
    sink: &'a mut Sink,
    player: Option<Launched>,
    spec: Option<&'a launch::Player>,
    b: &'a Begun,
}

impl end::Window for Windows<'_> {
    fn front(&mut self) -> Result<(), String> {
        end::Window::front(self.sink)
    }
    fn close(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Some(l) = self.player.take() {
            let mut p = RealPlayer {
                child: &l.child,
                window: l.window,
                log: &self.b.log,
                cut: self.b.cut,
                ended: false,
            };
            if let Err(e) = end::close_player(&mut p) {
                errors.push(e);
            }
            if p.ended {
                if let Some(spec) = self.spec {
                    match spec.collect(&self.b.dir) {
                        Ok(()) => {
                            for name in [launch::DEVICE_AFTER, launch::PLAYER_AFTER] {
                                seal_file(self.b, name, &mut errors);
                            }
                        }
                        Err(e) => errors.push(format!("the player's settings: {e:#}")),
                    }
                }
                for name in [launch::STDOUT, launch::STDERR] {
                    seal_file(self.b, name, &mut errors);
                }
            } else {
                errors
                    .push("the player's files are left unsealed: its end was not observed".into());
            }
        }
        if let Err(e) = self.sink.close() {
            errors.push(format!("closing the sink: {e:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Runs a row: the exit code.
///
/// # Errors
///
/// The run could not begin (nothing of it is held then).
pub fn run(a: &Args) -> anyhow::Result<i32> {
    if let Some(o) = outer_of(&a.row) {
        return outer(a, &o);
    }
    if a.kill_barrier && a.role.as_deref() != Some(test_device::contract::INNER) {
        bail!("--kill-barrier is for the inner run its outer reserves");
    }
    let Some(steps) = row(&a.row, a.kill_barrier) else {
        bail!("no row `{}`", a.row);
    };
    let panel = steps.contains(&Step::Launch);
    if steps.contains(&Step::Attach) != a.attach.is_some() {
        bail!(
            "--attach names the reference player: the reference rows take it, and no other row does"
        );
    }
    if (a.row == "p5") != a.player_sha256.is_some() {
        bail!("--player-sha256 names P5's mutated player: row p5 takes it, and no other row does");
    }
    match (panel, &a.player) {
        (true, None) => bail!(
            "row `{}` starts the player: it takes --player, --data, --library, --skin and --skin-entry",
            a.row
        ),
        (false, Some(_)) => bail!("row `{}` starts no player", a.row),
        _ => {}
    }
    let b = begin(a, None)?;
    let mut errors = Vec::new();
    if panel {
        mode_step(&b, a)?;
    }
    let mut rec = RecChild::spawn(&b, &a.port)?;
    let monitor = desktop::monitor(&a.monitor)?;
    // The player the run starts: a private copy, held unchanged until the
    // run ends, of the one named.
    let verified = match &a.player {
        Some(p) => Some(launch::private_copy(&p.exe, &p.data.join(PLAYER_COPY))?),
        None => None,
    };
    let player = a
        .player
        .as_ref()
        .zip(verified.as_ref())
        .map(|(p, v)| v.player(p));
    if let (Some(p), Some(v)) = (&a.player, &verified) {
        player_record(&b, &p.exe, v, a.player_sha256.as_deref())?;
        p.prepare(&a.monitor, &b.dir)?;
        for name in [launch::DEVICE_BEFORE, launch::PLAYER_BEFORE] {
            seal_file(&b, name, &mut errors);
        }
    }
    let mut sink = Sink::open(sink::place(monitor.area))?;
    b.log.put(json!({ "t": "sink", "window": sink.window }));
    let link_log = Stream::create(&b.dir.join(test_device::contract::LINK))?;
    let mut conn = owner::rediscover(&System, &a.port, &a.build)?;
    conn.identify(&b.run, b.process);
    let hello = conn.facts().context("no checked `hello`")?.line.clone();
    // The panel rows' verdicts read reports in the panel mode: checked
    // again on the connection that drives the run.
    may_hold(&a.build, &hello, panel.then_some(PANEL_MODE)).map_err(anyhow::Error::msg)?;
    let token: Option<Token> = conn.token();
    let (writer, thread) = Writer::start(conn, link_log, b.stream.clone());
    let beat = Arc::new(guard::Beat::new(qpc()));
    let freq = qpc_freq();
    let fresh_beat = Arc::clone(&beat);
    writer.fresh(Box::new(move |check| fresh(&fresh_beat, check, qpc, freq)));
    let shot_log = b.log.clone();
    let seals = b.seals;
    let mut live = Live {
        writer: writer.clone(),
        guard: None,
        beat: Arc::clone(&beat),
        window: sink.window,
        monitor: a.monitor.clone(),
        monitor_then: monitor.area,
        prefix: b.prefix,
        call: 0,
        log: b.log.clone(),
        recorder: Tail::new(&rec.path),
        recorder_process: rec.child.process,
        backlog: Vec::new(),
        held: Vec::new(),
        freq,
        dir: b.dir.clone(),
        job: b.job,
        player: player.clone(),
        launched: None,
        shot_pipe: match &player {
            Some(_) => Some(shot::Pipe::create(shot::pipe_name(&b.run))?),
            None => None,
        },
        shots: Shots::start(
            b.dir.clone(),
            b.run.clone(),
            b.role.clone(),
            Box::new(move |v| {
                if seals {
                    shot_log.put(v);
                }
            }),
        ),
        targets: targets_of(&a.row),
        attach: a.attach,
        attached: None,
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
    let beat_fn = || beat.beat(qpc());
    let mut link = EndLink {
        writer: &writer,
        thread: Some(thread),
        port: &a.port,
        build: &a.build,
        dir: &b.dir,
        beat: &beat_fn,
        closed: false,
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
    let mut windows = Windows {
        sink: &mut sink,
        player: live.launched.take(),
        spec: player.as_ref(),
        b: &b,
    };
    let clock = end::Wall(Instant::now());
    errors.extend(end::end(end::Run {
        link: &mut link,
        recorder: &mut rec,
        sink: Some(&mut windows),
        clock: &clock,
        token,
        granted,
        record: &mut record,
        cause: "end",
    }));
    if let Some((guard, guard_thread)) = live.guard.take() {
        match guard::stop(&guard, guard_thread, GUARD_WAIT) {
            Ok((age, looks)) => eprintln!(
                "guard: the step thread's heartbeat was at most {age} µs old over {looks} judged looks (bound {} µs)",
                guard::BEAT_MAX_US
            ),
            Err(e) => errors.push(e),
        }
    }
    errors.extend(live.shots.finish());
    let link_closed = link.closed;
    seal_own(&b, link_closed, rec.ended.is_some(), &mut errors);
    Ok(finish(&b, errors))
}

/// Whether a hold-bearing command may be submitted at the counter `now()`
/// reads (host counter ticks, `freq` a second): the step thread's
/// heartbeat, gaps latched, no older than [`guard::BEAT_MAX_US`], and a
/// grant's or renewal's check no older than that either. The counter is
/// read after the heartbeat's timestamp, so a submission delayed between
/// the two reads is judged at its later counter.
///
/// # Errors
///
/// Why it is stale.
fn fresh(
    beat: &guard::Beat,
    check: Option<i64>,
    now: impl FnOnce() -> i64,
    freq: i64,
) -> Result<(), String> {
    let (age, now) = beat.age_at(now);
    let age = ticks_to_us(age, freq);
    if age > guard::BEAT_MAX_US {
        return Err(format!("the step thread's heartbeat is {age} µs old"));
    }
    if let Some(c) = check {
        let since = ticks_to_us(now.saturating_sub(c), freq);
        if since > guard::BEAT_MAX_US {
            return Err(format!("its check is {since} µs old"));
        }
    }
    Ok(())
}

/// How long the outer waits for the inner's `held`.
const INNER_HELD_WAIT: Duration = Duration::from_secs(30);
/// How long the outer waits for the inner's kill barrier: the inner's
/// mode step, its player's start and settling come first.
const INNER_BARRIER_WAIT: Duration = Duration::from_secs(120);
/// How long the outer waits for the inner's recorder and player to end.
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

/// The process the inner's `spawn` record names `name`, as it stands in the
/// inner's stream now.
fn spawned_in(path: &Path, name: &str) -> Option<Process> {
    Tail::new(path).read().iter().find_map(|v| {
        (v["t"] == "spawn" && v["name"] == name)
            .then(|| serde_json::from_value::<Process>(v["process"].clone()).ok())
            .flatten()
    })
}

/// An outer row (N2 `sink-kill`, N2p `p1-kill`): it reserves the inner,
/// starts it in a job of its own, waits for the inner's record `o.waits_for`
/// (its presses held), terminates that job with its cut code, observes the
/// inner's end and those of its recorder and (N2p) its player, each up to
/// [`SURVIVOR_WAIT`] (a player still running then is recorded and ended by
/// the outer itself), and opens its own connection, whose first `hello` is
/// the evidence.
fn outer(a: &Args, o: &Outer) -> anyhow::Result<i32> {
    let panel = o.inner != "sink";
    if panel != a.player.is_some() {
        bail!(
            "row `{}`'s inner {} the player's paths",
            a.row,
            if panel { "needs" } else { "takes none of" }
        );
    }
    let inner = Reserved {
        role: test_device::contract::INNER.to_owned(),
        row: o.inner.to_owned(),
        run: owner::random_hex(16)?,
        dir: test_device::contract::INNER.to_owned(),
    };
    let b = begin(a, Some(&inner))?;
    let mut errors = Vec::new();
    let mut rec = RecChild::spawn(&b, &a.port)?;
    let exe = std::env::current_exe().context("the runner's own path")?;
    let set = b.dir.to_string_lossy().into_owned();
    let mut args: Vec<String> = [
        "test-device",
        "run",
        o.inner,
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
    .map(str::to_owned)
    .to_vec();
    if let Some(p) = &a.player {
        for (flag, value) in [
            ("--player", p.exe.to_string_lossy().into_owned()),
            ("--data", p.data.to_string_lossy().into_owned()),
            ("--library", p.library.to_string_lossy().into_owned()),
            ("--skin", p.skin.to_string_lossy().into_owned()),
            ("--skin-entry", p.skin_entry.clone()),
        ] {
            args.extend([flag.to_owned(), value]);
        }
    }
    if o.barrier {
        args.push("--kill-barrier".to_owned());
    }
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
    let reached = await_record(&inner_run, o.wait, |v| {
        (v["t"] == o.waits_for).then_some(())
    });
    if reached.is_none() {
        errors.push(format!(
            "the inner showed no `{}` within {:?}",
            o.waits_for, o.wait
        ));
    }
    // The inner's player, by the inner's spawn record, opened before the
    // kill so that its end can be observed.
    let player = panel.then(|| spawned_in(&inner_run, "player").map(job::open));
    // The inner and its job, its recorder and player with it, end with the
    // cut code; the kill is recorded once the call returned.
    if let Err(e) = kill_confirmed(&b.log, "inner", child.process, || {
        inner_job.terminate(b.cut.cast_unsigned())
    }) {
        errors.push(format!("terminating the inner's job: {e:#}"));
    }
    // Each end counts only once observed: a timeout or a failed wait is
    // none.
    let mut observe = |name: &str, c: &Child, wait: Duration| match c.wait(wait) {
        Ok(Some(code)) => {
            exit_record(&b.log, name, c.process, code);
            true
        }
        Ok(None) => {
            errors.push(format!(
                "{name} did not end within {wait:?} of the termination"
            ));
            false
        }
        Err(e) => {
            errors.push(format!("{name}'s end was not observed: {e}"));
            false
        }
    };
    let inner_ended = observe("inner", &child, KILL_WAIT);
    let recorder_ended = match &survivor {
        Some(Ok(s)) => observe("inner-recorder", s, SURVIVOR_WAIT),
        Some(Err(e)) => {
            errors.push(format!("opening the inner's recorder: {e:#}"));
            false
        }
        None => {
            errors.push("the inner recorded no recorder".into());
            false
        }
    };
    match player {
        None => {}
        Some(None) => errors.push("the inner recorded no player".into()),
        Some(Some(Err(e))) => errors.push(format!("opening the inner's player: {e:#}")),
        // A player still running after the wait outlived the inner's job:
        // recorded, then ended by the outer itself.
        Some(Some(Ok(p))) => match p.wait(SURVIVOR_WAIT) {
            Ok(Some(code)) => exit_record(&b.log, "inner-player", p.process, code),
            Ok(None) => {
                if let Err(e) = end_child(&b.log, "inner-player", &p, Duration::ZERO, b.cut) {
                    errors.push(e);
                }
            }
            Err(e) => errors.push(format!("the inner's player's end was not observed: {e}")),
        },
    }
    drop(inner_job);
    // The inner's streams, each sealed only once its writer's end was
    // observed: the link's and (N2p) the mode link's by the inner's, the
    // recorder's by its own.
    let mut streams = vec![
        (test_device::contract::LINK, inner_ended),
        (test_device::contract::RECORDER, recorder_ended),
    ];
    if panel {
        streams.push((MODE_LINK, inner_ended));
    }
    for (name, ended) in streams {
        if ended {
            seal(&b, &inner_dir, name, &inner.run, &inner.role, &mut errors);
        } else {
            errors.push(format!(
                "the inner's {name} is left unsealed: its writer's end was not observed"
            ));
        }
    }
    // Its own connection: the first `hello` is the evidence.
    let link_log = Stream::create(&b.dir.join(test_device::contract::LINK))?;
    let mut conn = owner::rediscover(&System, &a.port, &a.build)?;
    conn.identify(&b.run, b.process);
    let token = conn.token();
    let (writer, thread) = Writer::start(conn, link_log, b.stream.clone());
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
        closed: false,
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
    }));
    let link_closed = link.closed;
    seal_own(&b, link_closed, rec.ended.is_some(), &mut errors);
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

    impl Steps {
        /// Whether the row's lease lasts.
        fn leased(&self) -> bool {
            self.guard.as_ref().is_some_and(|g| g.wanted(self.at))
        }
    }

    impl Exec for Steps {
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
        fn await_released(&mut self) -> Result<(), String> {
            self.did.push("released".into());
            Ok(())
        }
        fn launch(&mut self) -> Result<(), String> {
            self.did.push("launch".into());
            Ok(())
        }
        fn focus_at(&mut self, p: (i32, i32)) -> Result<(), String> {
            self.did.push(format!("focus {p:?}"));
            Ok(())
        }
        fn calibrate(&mut self) -> Result<(), String> {
            self.did.push("calibrate".into());
            Ok(())
        }
        fn click(&mut self, p: (i32, i32)) -> Result<(), String> {
            self.did.push(format!("click {p:?}"));
            Ok(())
        }
        fn shot(&mut self, name: &'static str) -> Result<(), String> {
            self.did.push(format!("shot {name}"));
            Ok(())
        }
        fn barrier(&mut self) -> Result<(), String> {
            self.did.push("barrier".into());
            Ok(())
        }
        fn attach(&mut self) -> Result<(), String> {
            self.did.push("attach".into());
            Ok(())
        }
        fn capture(&mut self, files: [&'static str; 2]) -> Result<(), String> {
            self.did.push(format!("capture {files:?}"));
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
                        beat_age: 0,
                        recording: true,
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
        let may_hold = |build, line: &str| super::may_hold(build, line, None);
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

    /// A panel row holds only when the driving connection's own `hello`
    /// reads the panel mode: a device of the same build in another mode
    /// (1000 Hz, or continuous reports) is refused; a row that needs no
    /// mode takes either.
    #[test]
    fn a_wrong_mode_on_the_driving_connection_refuses_holds() {
        let line = hello(BUILD, false);
        assert!(line.contains(&format!(" mode {} ", PANEL_MODE.byte())));
        assert_eq!(super::may_hold(BUILD, &line, Some(PANEL_MODE)), Ok(()));
        for other in [1_u8, 0x18] {
            let wrong = line.replace(
                &format!(" mode {} ", PANEL_MODE.byte()),
                &format!(" mode {other} "),
            );
            let e = super::may_hold(BUILD, &wrong, Some(PANEL_MODE)).unwrap_err();
            assert!(e.contains("driving connection"), "{e}");
            assert_eq!(super::may_hold(BUILD, &wrong, None), Ok(()));
        }
    }

    /// A grant, renewal or held change is submitted only while the step
    /// thread's heartbeat (its gaps latched) and the command's check are
    /// within the bound.
    #[test]
    fn a_command_is_fresh_only_within_the_heartbeat_bound() {
        const F: i64 = 1_000_000;
        let max = guard::BEAT_MAX_US;
        let beat = guard::Beat::new(0);
        assert_eq!(fresh(&beat, None, || max, F), Ok(()));
        assert!(fresh(&beat, None, || max + 1, F).is_err());
        // A fresh heartbeat with a check older than the bound.
        let now = 1_000_000;
        let beating = guard::Beat::new(now);
        assert_eq!(fresh(&beating, Some(now - max), || now, F), Ok(()));
        let e = fresh(&beating, Some(now - max - 1), || now, F).unwrap_err();
        assert!(e.contains("check"), "{e}");
        // A gap closed before the submission still counts.
        beat.beat(max + 1);
        assert!(fresh(&beat, None, || max + 2, F).is_err());
    }

    /// A submission's counter is read after the heartbeat's timestamp: the
    /// step thread beats at the meeting point inside the counter's read,
    /// closing a gap within the bound, and the counter then reads past the
    /// bound from the timestamp read before it, so the submission is
    /// stale. Read before the timestamp, that counter would come with the
    /// new beat and pass. The control: a counter within the bound.
    #[test]
    fn a_submission_reads_its_counter_after_the_heartbeat() {
        const F: i64 = 1_000_000;
        let max = guard::BEAT_MAX_US;
        let beat = guard::Beat::new(0);
        let stale = fresh(
            &beat,
            None,
            || {
                beat.beat(max);
                max + 10
            },
            F,
        );
        assert!(stale.is_err(), "{stale:?}");
        let beat = guard::Beat::new(0);
        let control = fresh(
            &beat,
            None,
            || {
                beat.beat(max - 10);
                max
            },
            F,
        );
        assert_eq!(control, Ok(()));
    }

    /// A kill is recorded only once its termination call returned, with the
    /// counter read before the call: inside the call the stream holds no
    /// `kill` yet, after it one whose `requested` precedes the call and
    /// whose own counter follows it. A call that fails writes none.
    #[test]
    fn a_kill_is_recorded_once_its_call_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.jsonl");
        let log = Log(Stream::create(&path).unwrap());
        let p = Process {
            pid: 8,
            created: 80,
        };
        let read = || std::fs::read_to_string(&path).unwrap();
        let mut during = (String::new(), 0);
        kill_confirmed(&log, "inner", p, || -> Result<(), ()> {
            during = (read(), qpc());
            Ok(())
        })
        .unwrap();
        assert!(!during.0.contains("kill"), "{}", during.0);
        let v: Value = serde_json::from_str(read().lines().last().unwrap()).unwrap();
        assert_eq!(v["t"], "kill");
        assert!(v["requested"].as_i64().unwrap() <= during.1, "{v}");
        assert!(v["q"].as_i64().unwrap() >= during.1, "{v}");
        let before = read();
        assert!(kill_confirmed(&log, "player", p, || Err(())).is_err());
        assert_eq!(read(), before);
    }

    /// A last write before 1970 is refused; 1970 itself and later stand.
    /// A panel row expects the runner's own build of its player unless it
    /// declares another (P5's mutated build), and a held copy of any other
    /// digest is refused: P4 pointed at P5's executable does not run.
    #[test]
    fn a_row_runs_only_the_player_it_expects() {
        let (runner, mutated) = ("4e".repeat(32), "5a".repeat(32));
        assert_eq!(expected_player(None, &runner), runner);
        assert_eq!(expected_player(Some(&mutated), &runner), mutated);
        let exe = Path::new("p5/tapestry.exe");
        let e = bind_player(exe, &mutated, expected_player(None, &runner))
            .unwrap_err()
            .to_string();
        assert!(e.contains(&format!("not the expected {runner}")), "{e}");
        bind_player(exe, &runner, expected_player(None, &runner)).unwrap();
        bind_player(exe, &mutated, expected_player(Some(&mutated), &runner)).unwrap();
    }

    #[test]
    fn a_last_write_before_1970_is_refused() {
        assert!(last_write(UNIX_EPOCH_FILETIME - 1).is_err());
        assert!(last_write(0).is_err());
        assert_eq!(
            last_write(UNIX_EPOCH_FILETIME).unwrap(),
            UNIX_EPOCH_FILETIME
        );
        let later = UNIX_EPOCH_FILETIME + 17_000_000_000_000_000;
        assert_eq!(last_write(later).unwrap(), later);
    }

    /// No row step writes a `release`, or any command but a held change
    /// under the grant: the end's `stop_holds` is the only release request.
    #[test]
    fn no_row_step_requests_a_release() {
        let id = LeaseId {
            token: super::super::link::fake::TOKEN,
            n: 1,
        };
        let steps = row("sink", false).unwrap();
        for s in &steps {
            match command(s, id) {
                None => {}
                Some(Command::Set { id: got, .. }) => assert_eq!(got, id),
                Some(c) => panic!("row sink's step {s:?} writes `{c}`"),
            }
        }
        // Positive control: the row does hold.
        assert!(steps.iter().any(|s| command(s, id).is_some()));
    }

    /// The runner refuses N1's negative before it holds, opens or starts
    /// anything: no run closes its sink before the release evidence.
    #[test]
    fn the_runner_refuses_the_early_close_negative() {
        // A temporary folder: a runner that took the row would reserve it
        // there, not in the checkout.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("run");
        let args = |row: &str| Args {
            row: row.into(),
            out: Some(out.clone()),
            set: None,
            role: None,
            build: BUILD.into(),
            port: "L".into(),
            monitor: "M".into(),
            player: None,
            player_sha256: None,
            attach: None,
            kill_barrier: false,
        };
        let e = run(&args("sink-early-close")).unwrap_err();
        assert_eq!(format!("{e:#}"), "no row `sink-early-close`");
        // The row it negates is one.
        assert!(row("sink", false).is_some());
        assert!(!out.exists());
    }

    /// P5 runs only against a player whose digest it declares, and no
    /// other row takes one: both refused before anything is reserved.
    #[test]
    fn only_p5_declares_its_player_s_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("run");
        let args = |row: &str, sha: Option<&str>| Args {
            row: row.into(),
            out: Some(out.clone()),
            set: None,
            role: None,
            build: BUILD.into(),
            port: "L".into(),
            monitor: "M".into(),
            player: None,
            player_sha256: sha.map(str::to_owned),
            attach: None,
            kill_barrier: false,
        };
        for (row, sha) in [("p5", None), ("p1", Some("ab")), ("p4", Some("ab"))] {
            let e = format!("{:#}", run(&args(row, sha)).unwrap_err());
            assert!(e.contains("--player-sha256"), "{row}: {e}");
        }
        assert!(!out.exists());
    }

    /// A recorder counts as recording only while its file grows: a file
    /// that stops growing, or whose size cannot be read, stops counting
    /// within three beats; growth again counts again.
    #[test]
    fn a_recorder_that_stops_writing_is_not_recording() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut g = Growth::new(t0);
        assert!(g.look(Some(100), at(10)));
        assert!(g.look(Some(100), at(70)), "within three beats");
        assert!(!g.look(Some(100), at(71)), "stalled");
        assert!(!g.look(None, at(80)), "unreadable");
        assert!(g.look(Some(120), at(90)), "growing again");
        assert!(!g.look(Some(110), at(151)), "shrinking is no growth");
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

    /// Shot B waits for a neutral report of the device's own pad: a report
    /// with a button down, another device's report or a key is not one.
    #[test]
    fn only_the_device_s_pad_with_no_button_is_neutral() {
        let pad = |buttons: u16, ours: bool| {
            let r = PadReport {
                buttons,
                ..PadReport::default()
            }
            .encode();
            let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
            json!({"t": "pad", "q": 5, "bytes": format!("00{hex}"), "ours": ours})
        };
        assert!(neutral(&pad(0, true)));
        assert!(!neutral(&pad(1 << 8, true)));
        assert!(!neutral(&pad(0, false)));
        assert!(!neutral(&json!({"t": "key", "ours": true, "bytes": "00"})));
    }

    /// `p1-kill`'s inner reaches its barrier after shot A with START and
    /// SELECT both held: every press before it, no release before it; the
    /// releases follow it, then shot B. `p1` itself has no barrier, and
    /// `p4` holds START alone.
    #[test]
    fn p1_kill_waits_at_its_barrier_with_both_buttons_held() {
        let mut x = Steps::default();
        perform(&row("p1", true).unwrap(), &mut x).unwrap();
        let at = |s: &str| {
            x.did
                .iter()
                .position(|d| d == s)
                .unwrap_or_else(|| panic!("{s}: {:?}", x.did))
        };
        let barrier = at("barrier");
        for b in [START, SELECT] {
            assert!(at(&format!("hold b{b}+")) < barrier);
            assert!(at(&format!("hold b{b}-")) > barrier);
        }
        assert!(at("shot shot-a.png") < barrier && barrier < at("shot shot-b.png"));
        // Shot B waits for the recorder's neutral report after both
        // releases.
        for b in [START, SELECT] {
            assert!(at(&format!("hold b{b}-")) < at("released"));
        }
        assert!(at("released") < at("shot shot-b.png"));
        // Every target is clicked once, after the calibration, under the
        // held buttons.
        for t in test_device::panel::TARGETS {
            assert!(at(&format!("click {:?}", (t.x, t.y))) > at("calibrate"));
            assert!(at(&format!("click {:?}", (t.x, t.y))) > at("held"));
        }
        let mut plain = Steps::default();
        perform(&row("p1", false).unwrap(), &mut plain).unwrap();
        assert!(!plain.did.contains(&"barrier".to_owned()));
        let holds = |r: &str| {
            let mut x = Steps::default();
            perform(&row(r, false).unwrap(), &mut x).unwrap();
            x.did
                .into_iter()
                .filter(|d| d.starts_with("hold") && d.ends_with('+'))
                .collect::<Vec<_>>()
        };
        assert_eq!(holds("p4"), [format!("hold b{START}+")]);
        assert_eq!(holds("p5"), holds("p1"));
        assert!(row("sink", true).is_none() && row("p4", true).is_none());
    }

    /// A tail opens its stream once it exists and then reads, through that
    /// one handle, whole lines as another writer appends them, a line cut
    /// mid-way (in a multi-byte character too) waiting for its end.
    #[test]
    fn a_tail_reads_whole_lines_as_they_are_appended() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recorder.jsonl");
        let mut tail = Tail::new(&path);
        assert!(tail.read().is_empty());
        let mut w = std::fs::File::create(&path).unwrap();
        let first = "{\"t\":\"a\",\"s\":\"é\"}\n".as_bytes();
        let cut = first.iter().position(|&b| b == 0xC3).unwrap() + 1;
        w.write_all(&first[..cut]).unwrap();
        assert!(tail.read().is_empty());
        w.write_all(&first[cut..]).unwrap();
        w.write_all(b"{\"t\":\"b\"}\n{\"t\":").unwrap();
        let got = tail.read();
        assert_eq!(got, [json!({"t": "a", "s": "é"}), json!({"t": "b"})]);
        w.write_all(b"\"c\"}\n").unwrap();
        assert_eq!(tail.read(), [json!({"t": "c"})]);
        assert!(tail.read().is_empty());
    }
}
