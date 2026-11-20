//! The game's account of a media worker's players and of how a worker
//! ended (design.md "Threat Model" T02), checked over every ordering within
//! a bound: the host's player states (`media_host::players`) against a
//! model of the worker, and the watcher's verdict
//! (`media_host::verdict`) against every order of its message, reads,
//! terminations and the worker's end.
//!
//! The worker's model follows `tapestry-media`'s control loop and player
//! threads: the control loop reads the commands in order, rejects an open
//! whose handle it does not take, of a player it has, or past its players
//! (two here), and rejects a close of a player it does not have; a player's
//! thread probes its file, answering with the probe or a refusal, may fail
//! once probed, and answers its close; a refusal, a failure or a close's
//! answer is sent after the thread has closed the file, a step of its own.
//! The file's handle is the worker's from the moment the game sends the
//! open. The worker may crash between any two steps, the game taking the
//! replies it sent before it crashed when it reaps it.

#![cfg(windows)]

#[path = "../src/media_host/players.rs"]
#[allow(dead_code, reason = "the test uses the tracker's states alone")]
mod players;
#[path = "../src/media_host/verdict.rs"]
#[allow(dead_code, reason = "the test does not store an end")]
mod verdict;

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use players::Players;
use skin_model::media::control::{Failure, Refusal, Reply};
use verdict::{Cause, ENDED_AT_LINE, ENDED_BY_MESSAGE, End, Verdict};

/// A file, by a number of the test's.
type File = u8;

const A: File = 1;
const B: File = 2;

/// The most players the model's worker holds.
const MAX_PLAYERS: usize = 2;

/// A step of the game's script.
#[derive(Clone, Copy, Debug)]
enum Step {
    Open(u8, File),
    Close(u8),
}

/// A command the game sent and the worker has not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Sent {
    Open { id: u32, player: u8, file: File },
    Close { id: u32, player: u8 },
}

/// A reply, as the model holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Said {
    Probed { id: u32, player: u8 },
    Refused { id: u32, player: u8 },
    Rejected { id: u32, player: u8 },
    Failed { player: u8 },
    Closed { id: u32, player: u8 },
}

impl Said {
    fn reply(self) -> Reply {
        match self {
            Self::Probed { id, player } => Reply::Probed {
                id,
                player,
                width: 64,
                height: 48,
                duration: 0,
                codec: String::new(),
                container: String::new(),
            },
            Self::Refused { id, player } => Reply::Refused {
                id,
                player,
                why: Refusal::Container,
                code: 0,
            },
            Self::Rejected { id, player } => Reply::Rejected {
                id,
                player,
                code: 0,
            },
            Self::Failed { player } => Reply::Failed {
                player,
                why: Failure::Decode,
                code: 0,
            },
            Self::Closed { id, player } => Reply::Closed { id, player },
        }
    }
}

/// Where a player's thread stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Phase {
    Probing,
    Probed,
    /// Refused or failed: it holds nothing and waits to be closed.
    Idle,
    Done,
}

/// A player's thread in the worker.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Thread {
    player: u8,
    open: u32,
    file: File,
    phase: Phase,
    /// It holds its file.
    held: bool,
    /// The closes it has been told, in order.
    inbox: VecDeque<u32>,
    /// The reply it sends next, its file already closed.
    unsent: Option<Said>,
}

/// The game and the worker at one moment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    script: usize,
    next_id: u32,
    host: Players<File>,
    queue: VecDeque<Sent>,
    players: BTreeMap<u8, usize>,
    threads: Vec<Thread>,
    sent: Vec<Said>,
    taken: usize,
}

impl World {
    fn new() -> Self {
        Self {
            script: 0,
            next_id: 1,
            host: Players::default(),
            queue: VecDeque::new(),
            players: BTreeMap::new(),
            threads: Vec::new(),
            sent: Vec::new(),
            taken: 0,
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Every state one step from this one.
    fn next(&self, script: &[Step]) -> Vec<Self> {
        let mut out = Vec::new();
        // The game's next step.
        if let Some(&step) = script.get(self.script) {
            let mut w = self.clone();
            w.script += 1;
            match step {
                Step::Open(player, file) => {
                    if w.host.free(player) {
                        let id = w.id();
                        w.host.opened(player, id, file);
                        w.queue.push_back(Sent::Open { id, player, file });
                    }
                }
                Step::Close(player) => {
                    let id = w.id();
                    w.host.closing(player, id);
                    w.queue.push_back(Sent::Close { id, player });
                }
            }
            out.push(w);
        }
        // The game takes a reply.
        if self.taken < self.sent.len() {
            let mut w = self.clone();
            w.host.take(&w.sent[w.taken].reply());
            w.taken += 1;
            out.push(w);
        }
        // The control loop reads a command.
        if let Some(&command) = self.queue.front() {
            match command {
                Sent::Open { id, player, file } => {
                    // A handle it does not take.
                    let mut w = self.clone();
                    w.queue.pop_front();
                    w.sent.push(Said::Rejected { id, player });
                    out.push(w);
                    let mut w = self.clone();
                    w.queue.pop_front();
                    if w.players.contains_key(&player) || w.players.len() >= MAX_PLAYERS {
                        w.sent.push(Said::Rejected { id, player });
                    } else {
                        w.players.insert(player, w.threads.len());
                        w.threads.push(Thread {
                            player,
                            open: id,
                            file,
                            phase: Phase::Probing,
                            held: true,
                            inbox: VecDeque::new(),
                            unsent: None,
                        });
                    }
                    out.push(w);
                }
                Sent::Close { id, player } => {
                    let mut w = self.clone();
                    w.queue.pop_front();
                    match w.players.remove(&player) {
                        Some(i) => w.threads[i].inbox.push_back(id),
                        None => w.sent.push(Said::Rejected { id, player }),
                    }
                    out.push(w);
                }
            }
        }
        // A player's thread steps.
        for (i, t) in self.threads.iter().enumerate() {
            if let Some(said) = t.unsent {
                let mut w = self.clone();
                w.threads[i].unsent = None;
                w.sent.push(said);
                out.push(w);
                continue;
            }
            let (player, open) = (t.player, t.open);
            match t.phase {
                Phase::Probing => {
                    let mut w = self.clone();
                    w.threads[i].phase = Phase::Probed;
                    w.sent.push(Said::Probed { id: open, player });
                    out.push(w);
                    let mut w = self.clone();
                    let t = &mut w.threads[i];
                    (t.phase, t.held) = (Phase::Idle, false);
                    t.unsent = Some(Said::Refused { id: open, player });
                    out.push(w);
                }
                Phase::Probed => {
                    let mut w = self.clone();
                    let t = &mut w.threads[i];
                    (t.phase, t.held) = (Phase::Idle, false);
                    t.unsent = Some(Said::Failed { player });
                    out.push(w);
                    if let Some(&close) = self.threads[i].inbox.front() {
                        let mut w = self.clone();
                        let t = &mut w.threads[i];
                        t.inbox.pop_front();
                        (t.phase, t.held) = (Phase::Done, false);
                        t.unsent = Some(Said::Closed { id: close, player });
                        out.push(w);
                    }
                }
                Phase::Idle => {
                    if let Some(&close) = t.inbox.front() {
                        let mut w = self.clone();
                        let t = &mut w.threads[i];
                        t.inbox.pop_front();
                        t.phase = Phase::Done;
                        w.sent.push(Said::Closed { id: close, player });
                        out.push(w);
                    }
                }
                Phase::Done => {}
            }
        }
        out
    }

    /// The files the worker holds: each thread's that holds its own, and
    /// each file whose open the control loop has not read (its handle is
    /// the worker's already).
    fn held(&self) -> BTreeSet<File> {
        let threads = self.threads.iter().filter(|t| t.held).map(|t| t.file);
        let queued = self.queue.iter().filter_map(|c| match *c {
            Sent::Open { file, .. } => Some(file),
            Sent::Close { .. } => None,
        });
        threads.chain(queued).collect()
    }

    /// The files closed by a thread whose reply saying so is not sent.
    fn unreported(&self) -> BTreeSet<File> {
        self.threads
            .iter()
            .filter(|t| t.unsent.is_some())
            .map(|t| t.file)
            .collect()
    }

    /// Whether `player` has a thread that is not done, or an open the
    /// control loop has not read.
    fn in_worker(&self, player: u8) -> bool {
        self.threads
            .iter()
            .any(|t| t.player == player && t.phase != Phase::Done)
            || self
                .queue
                .iter()
                .any(|c| matches!(*c, Sent::Open { player: p, .. } if p == player))
    }

    /// Checks this moment: an id free in the game is free in the worker,
    /// and a crash now strikes each file the worker holds, once, and only
    /// those and files whose close is not yet reported.
    fn check(&self, script: &[Step]) {
        for player in 0..4 {
            assert!(
                !self.host.free(player) || !self.in_worker(player),
                "player {player} is free in the game but not in the worker: {self:#?}\n{script:?}"
            );
        }
        let mut host = self.host.clone();
        for said in &self.sent[self.taken..] {
            host.take(&said.reply());
        }
        let (struck, failed) = host.clear();
        let held = self.held();
        let unreported = self.unreported();
        assert!(
            held.is_subset(&struck)
                && struck.is_subset(&held.union(&unreported).copied().collect()),
            "struck {struck:?}, held {held:?}, unreported {unreported:?}: {self:#?}\n{script:?}"
        );
        for t in self.threads.iter().filter(|t| t.held) {
            assert!(failed.contains(&t.player), "{self:#?}\n{script:?}");
        }
    }
}

/// Every state `script` reaches, each checked: how many.
fn explore(script: &[Step]) -> usize {
    let mut seen = HashSet::new();
    let mut stack = vec![World::new()];
    while let Some(w) = stack.pop() {
        if !seen.insert(w.clone()) {
            continue;
        }
        w.check(script);
        stack.extend(w.next(script));
    }
    seen.len()
}

// Covers: T02
#[test]
fn each_loss_strikes_exactly_the_files_open_in_every_ordering() {
    // The bound: two files, at most three players' ids and four steps of
    // the game's, every interleaving of them with the worker's steps and
    // the game's taking of replies, a crash at every moment.
    let scripts: [&[Step]; 5] = [
        // Two closes of one player: the second's rejection is not the
        // open's.
        &[Step::Open(1, A), Step::Close(1), Step::Close(1)],
        // One file open as two players, and an open past the worker's
        // players.
        &[Step::Open(1, A), Step::Open(2, A), Step::Open(3, B)],
        // An open under a taken id, and the id again after a close.
        &[
            Step::Open(1, A),
            Step::Open(1, B),
            Step::Close(1),
            Step::Open(1, B),
        ],
        // A close, then another player.
        &[
            Step::Open(1, A),
            Step::Close(1),
            Step::Open(2, B),
            Step::Close(2),
        ],
        // A close of a player never opened, then one opened.
        &[Step::Close(1), Step::Open(1, A), Step::Close(1)],
    ];
    let mut total = 0;
    for script in scripts {
        let states = explore(script);
        eprintln!("{script:?}: {states} states");
        assert!(states > 50, "{script:?}: {states}");
        total += states;
    }
    eprintln!("{total} states checked");
}

/// How a worker came to its end, in truth.
#[derive(Clone, Copy, Debug)]
enum Truth {
    /// The watcher's accepted termination for this cause.
    Ours(Cause),
    /// The worker itself, with this exit code.
    Own(u32),
}

/// A crash's exit code.
const CRASH: u32 = 0xc000_0005;

/// The line, in the model's units.
const LINE: usize = 448;

/// Every ordering, up to `depth` more events, of the job's message, a read
/// below or at the line, a read while a termination is pending (asking
/// again) and each termination's acceptance or refusal, then the worker's
/// end, each checked: how many ends. `accepted` is the cause of the first
/// termination the system accepted, as the test itself counts it.
fn verdicts(v: Verdict, accepted: Option<Cause>, depth: usize, trace: &mut Vec<String>) -> usize {
    let mut ends = 0;
    // The worker ends: by the watcher's accepted termination, or by
    // itself with a crash's code or either of the watcher's.
    let mut truths = vec![
        Truth::Own(CRASH),
        Truth::Own(ENDED_BY_MESSAGE),
        Truth::Own(ENDED_AT_LINE),
    ];
    if let Some(cause) = accepted {
        truths.push(Truth::Ours(cause));
    }
    for truth in truths {
        let code = match truth {
            Truth::Ours(cause) => cause.code(),
            Truth::Own(code) => code,
        };
        let (end, ambiguous) = v.end(code);
        let proven = match end {
            End::Message => accepted == Some(Cause::Message) && code == ENDED_BY_MESSAGE,
            End::Line => accepted == Some(Cause::Line) && code == ENDED_AT_LINE,
            End::Exited(c) => {
                c == code
                    && !matches!(truth, Truth::Ours(_))
                    && ambiguous == (code == ENDED_BY_MESSAGE || code == ENDED_AT_LINE)
            }
        };
        assert!(
            proven,
            "{end:?} ({ambiguous}) for {truth:?} after {trace:?}"
        );
        ends += 1;
    }
    if depth == 0 {
        return ends;
    }
    // A termination asked for, accepted or refused.
    let attempt = |v: Verdict, cause: Cause, what: &str, trace: &mut Vec<String>| {
        let mut n = 0;
        for ok in [true, false] {
            let mut next = v;
            next.terminated(cause, ok);
            let now = if ok {
                accepted.or(Some(cause))
            } else {
                accepted
            };
            trace.push(format!("{what}: {cause:?} accepted={ok}"));
            n += verdicts(next, now, depth - 1, trace);
            trace.pop();
        }
        n
    };
    let mut next = v;
    match next.message() {
        Some(cause) => ends += attempt(next, cause, "message", trace),
        None => {
            trace.push("message, a cause asked already".to_owned());
            ends += verdicts(next, accepted, depth - 1, trace);
            trace.pop();
        }
    }
    match v.requested() {
        // The watcher waits on the process, and asks again at each read.
        Some(cause) => ends += attempt(v, cause, "read, asking again", trace),
        None => {
            for peak in [LINE - 1, LINE] {
                let mut next = v;
                match next.read(peak, LINE) {
                    Some(cause) => ends += attempt(next, cause, &format!("read {peak}"), trace),
                    None => {
                        trace.push(format!("read {peak}"));
                        ends += verdicts(next, accepted, depth - 1, trace);
                        trace.pop();
                    }
                }
            }
        }
    }
    ends
}

// Covers: T02
#[test]
fn a_memory_end_is_recorded_only_when_proven_in_every_ordering() {
    // The bound: six events before the end, each of the job's message, a
    // read below or at the line (or, once a termination is asked for, a
    // read that asks again) and each termination accepted or refused, and
    // every way the worker can end after each.
    let ends = verdicts(Verdict::default(), None, 6, &mut Vec::new());
    eprintln!("{ends} ends checked");
    assert!(ends > 1_000, "{ends}");
}
