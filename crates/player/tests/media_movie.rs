//! A video's state against the session's results (design.md "Threat Model"
//! T02), over every order of the steps that change it within a bound: the
//! game's preparation deadline and its entering another screen, the render
//! side's staged frames, and the session's looks and the worker's results
//! for the player (the open's probe or refusal, the play's answer, a
//! rejection, each in time or late; a failure; the close's answer; the
//! replies' end). Each step is one call that changes the video's word by
//! one atomic step, so the orders of the calls are the interleavings of the
//! threads.
//!
//! The session takes a result in three steps, between which the game and
//! the render side may move: it reads the screen shown, applies its rule
//! for the result (`movie::answer`), then reads the screen shown again and
//! confirms (`movie::confirm`). It takes one result at a time and looks
//! between results.
//!
//! What a look closes is this test's own reading of the session's rule,
//! not the session's code: a player of a screen no longer shown, other
//! than one closing or not yet opened; and a live or idle player whose
//! video is lost.

#![cfg(windows)]

#[path = "../src/media_host/movie.rs"]
#[allow(dead_code, reason = "the session's queued stage is before any result")]
mod movie;

use movie::{Cell, Loss, Outcome, Stage, Status, Then};

/// The open's, the play's and the close's command ids.
const OPEN: u32 = 1;
const PLAY: u32 = 2;
const CLOSE: u32 = 3;

/// The run the player plays: no seek is sent in these orders.
const RUN: u64 = 1;

/// The screen that asked for the video.
const SCREEN: u64 = 1;

/// How many looks and staged frames an order holds at most.
const LOOKS: u8 = 2;
const FRAMES: u8 = 2;

/// `movie::answer`: a result, in time or late, against the stage, the video,
/// the screen shown and the newest run sought.
type Answer = fn(Stage, &Cell, u64, Outcome, bool, u64) -> (Stage, Then);

/// The session's rules for a result: `movie::answer` and `movie::confirm`,
/// or broken ones for the controls.
#[derive(Clone, Copy)]
struct Rules {
    answer: Answer,
    confirm: fn(Stage, &Cell, u64, Then) -> (Stage, Then),
}

const SESSION: Rules = Rules {
    answer: movie::answer,
    confirm: movie::confirm,
};

/// A result the worker sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Result {
    /// The open's probe, or its refusal.
    Probe { refused: bool, late: bool },
    /// The play's answer.
    Playing { late: bool },
    /// The open's or the play's rejection.
    Rejected { late: bool },
    /// The player's failure.
    Failed,
}

impl Result {
    fn late(self) -> bool {
        match self {
            Self::Probe { late, .. } | Self::Playing { late } | Self::Rejected { late } => late,
            Self::Failed => false,
        }
    }
}

/// A step of an order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The game's preparation deadline passes.
    Expire,
    /// The game enters another screen.
    Enter,
    /// The render side stages a frame from the ring.
    Frame,
    /// The session looks.
    Look,
    /// The session takes a result: it reads the screen shown.
    Take(Result),
    /// It applies its rule to the result.
    Answer,
    /// It reads the screen shown again and confirms.
    Confirm,
    /// The close's answer.
    Closed,
    /// The worker's replies end.
    Eof,
}

/// A result being taken.
#[derive(Clone, Copy, Debug)]
struct Taking {
    result: Result,
    /// The screen shown when the session took it.
    read: u64,
    /// What its rule gave, once applied.
    gave: Option<(Stage, Then)>,
    /// The video's status when the session took it.
    status: Status,
}

/// Where an order stands.
struct World {
    cell: Cell,
    /// The player's stage, `None` once the session dropped it.
    stage: Option<Stage>,
    /// The screen shown.
    shown: u64,
    /// The ring is set up (the play sent): the render side may stage.
    ring: bool,
    taking: Option<Taking>,
    expired: bool,
    entered: bool,
    eof: bool,
    probed: bool,
    rejected: bool,
    failed: bool,
    looks: u8,
    frames: u8,
}

/// Whether a look closes a player at `stage` whose video is `cell` while
/// `shown` is shown (the module's doc).
fn closes(stage: Stage, cell: &Cell, shown: u64) -> bool {
    let left = cell.generation() != shown;
    let lost = cell.status() == Status::Lost;
    match stage {
        Stage::Queued | Stage::Closing(_) => false,
        Stage::Opening(_) | Stage::Playing(_) => left,
        Stage::Live | Stage::Seeking { .. } | Stage::Idle => left || lost,
    }
}

impl World {
    fn new() -> Self {
        Self {
            cell: Cell::new(SCREEN),
            stage: Some(Stage::Opening(OPEN)),
            shown: SCREEN,
            ring: false,
            taking: None,
            expired: false,
            entered: false,
            eof: false,
            probed: false,
            rejected: false,
            failed: false,
            looks: 0,
            frames: 0,
        }
    }

    /// The steps that may come next.
    fn enabled(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        if !self.expired {
            steps.push(Step::Expire);
        }
        if !self.entered {
            steps.push(Step::Enter);
        }
        if self.ring && self.frames < FRAMES {
            steps.push(Step::Frame);
        }
        // The session finishes a result before anything else of its own.
        if let Some(taking) = self.taking {
            steps.push(if taking.gave.is_none() {
                Step::Answer
            } else {
                Step::Confirm
            });
            return steps;
        }
        // Looks beyond the bound only while one would close the player:
        // the session looks for as long as it runs.
        if let Some(stage) = self.stage
            && (self.looks < LOOKS || closes(stage, &self.cell, self.shown))
        {
            steps.push(Step::Look);
        }
        match self.stage {
            Some(Stage::Opening(_)) if !self.probed => {
                for refused in [false, true] {
                    for late in [false, true] {
                        steps.push(Step::Take(Result::Probe { refused, late }));
                    }
                }
            }
            Some(Stage::Playing(_)) => {
                for late in [false, true] {
                    steps.push(Step::Take(Result::Playing { late }));
                }
            }
            Some(Stage::Closing(_)) => steps.push(Step::Closed),
            _ => {}
        }
        if !self.rejected
            && matches!(self.stage, Some(Stage::Opening(_) | Stage::Playing(_)))
            && !(self.probed && matches!(self.stage, Some(Stage::Opening(_))))
        {
            for late in [false, true] {
                steps.push(Step::Take(Result::Rejected { late }));
            }
        }
        if !self.failed && matches!(self.stage, Some(Stage::Playing(_) | Stage::Live)) {
            steps.push(Step::Take(Result::Failed));
        }
        if self.stage.is_some() && !self.eof {
            steps.push(Step::Eof);
        }
        steps
    }

    /// The session sends the close.
    fn close(&mut self) {
        if !matches!(self.stage, Some(Stage::Closing(_))) {
            self.stage = Some(Stage::Closing(CLOSE));
        }
    }

    /// The session acts on its rule's `then`.
    fn then(&mut self, then: Then) {
        match then {
            Then::Nothing | Then::Mark | Then::End(_) => {}
            Then::Play => {
                self.ring = true;
                self.stage = Some(Stage::Playing(PLAY));
            }
            Then::Close | Then::Late => self.close(),
        }
    }

    fn apply(&mut self, step: Step, rules: Rules) {
        match step {
            Step::Expire => {
                self.expired = true;
                self.cell.expire();
            }
            Step::Enter => {
                self.entered = true;
                self.shown = SCREEN + 1;
            }
            Step::Frame => {
                self.frames += 1;
                self.cell.staged_first();
            }
            Step::Look => {
                self.looks = self.looks.saturating_add(1);
                if let Some(stage) = self.stage
                    && closes(stage, &self.cell, self.shown)
                {
                    self.close();
                }
            }
            Step::Take(result) => {
                match result {
                    Result::Probe { .. } => self.probed = true,
                    Result::Rejected { .. } => self.rejected = true,
                    Result::Failed => self.failed = true,
                    Result::Playing { .. } => {}
                }
                self.taking = Some(Taking {
                    result,
                    read: self.shown,
                    gave: None,
                    status: self.cell.status(),
                });
            }
            Step::Answer => {
                let (Some(stage), Some(taking)) = (self.stage, self.taking.as_mut()) else {
                    return;
                };
                let outcome = match taking.result {
                    Result::Probe { refused: true, .. } => Outcome::Refused(OPEN),
                    Result::Probe { .. } => Outcome::Probed(OPEN),
                    Result::Playing { .. } => Outcome::Playing(PLAY),
                    Result::Rejected { .. } => Outcome::Rejected(match stage {
                        Stage::Playing(_) => PLAY,
                        _ => OPEN,
                    }),
                    Result::Failed => Outcome::Failed,
                };
                taking.gave = Some((rules.answer)(
                    stage,
                    &self.cell,
                    taking.read,
                    outcome,
                    taking.result.late(),
                    RUN,
                ));
            }
            Step::Confirm => {
                let Some(Taking {
                    result,
                    gave: Some((next, then)),
                    ..
                }) = self.taking.take()
                else {
                    return;
                };
                let (next, then) = (rules.confirm)(next, &self.cell, self.shown, then);
                // An open rejected was never taken up: the tracker frees the
                // id, and the session drops the player.
                self.stage = if matches!(result, Result::Rejected { .. })
                    && matches!(self.stage, Some(Stage::Opening(_)))
                {
                    self.probed = true;
                    None
                } else {
                    Some(next)
                };
                self.then(then);
            }
            Step::Closed => {
                // The tracker frees the id: a video of the screen shown
                // whose player is gone is lost.
                if self.cell.generation() == self.shown && self.cell.status() != Status::Absent {
                    self.cell.lose(Loss::Failed);
                }
                self.stage = None;
                self.ring = false;
            }
            Step::Eof => {
                self.eof = true;
                self.cell.lose(Loss::Transport);
                self.stage = None;
                self.ring = false;
                self.taking = None;
            }
        }
    }
}

/// What a video's word says, for the checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seen {
    status: Status,
    ready: bool,
    loss: Option<Loss>,
    settled: bool,
}

impl Seen {
    fn of(cell: &Cell) -> Self {
        Self {
            status: cell.status(),
            ready: cell.ready(),
            loss: cell.loss(),
            settled: cell.settled(),
        }
    }
}

/// The first rule an order breaks, if any, at `step` from `before` to
/// `after` in `world` (after the step); `taking` is the result being taken
/// before the step.
fn broken(
    step: Step,
    before: Seen,
    after: Seen,
    taking: Option<Taking>,
    world: &World,
) -> Option<String> {
    use Status::{Absent, Live, Lost, Pending};
    let left = world.cell.generation() != world.shown;
    // What an answer made it may be withdrawn, once its screen is left.
    let withdrawn = step == Step::Confirm && left && after.status == Pending;
    let edge = match (before.status, after.status) {
        (a, b) if a == b => before.ready <= after.ready || after.status == Lost,
        (Pending, Absent | Live | Lost) | (Live | Absent, Lost) => true,
        _ => withdrawn,
    };
    if !edge {
        return Some(format!("{before:?} -> {after:?}"));
    }
    if before.loss.is_some() && after.loss != before.loss {
        return Some(format!("a loss changed: {before:?} -> {after:?}"));
    }
    if before.settled && !after.settled && !withdrawn {
        return Some(format!("unsettled: {before:?} -> {after:?}"));
    }
    if after.ready && after.status != Live {
        return Some(format!("ready while {:?}", after.status));
    }
    if step == Step::Expire && before.ready && after.status == Lost {
        return Some("a staged video expired".to_owned());
    }
    if let (Step::Confirm, Some(taking)) = (step, taking)
        && left
        && !matches!(taking.status, Live | Absent)
        && matches!(after.status, Live | Absent)
    {
        return Some(format!(
            "an answer stood for a screen left: {:?}",
            after.status
        ));
    }
    if let (Step::Answer, Some(taking)) = (step, taking)
        && taking.result.late()
        && before.loss.is_none()
        && after.loss != Some(Loss::Answer)
    {
        return Some(format!("a late answer left it {after:?}"));
    }
    if step == Step::Eof && after.status != Lost {
        return Some(format!("{:?} after the replies ended", after.status));
    }
    None
}

/// The rule a terminal order breaks, if any: the player is gone, or its
/// video plays for the screen shown.
fn stuck(world: &World) -> Option<String> {
    match world.stage {
        None => None,
        Some(Stage::Live)
            if world.cell.generation() == world.shown && world.cell.status() == Status::Live =>
        {
            None
        }
        Some(stage) => Some(format!(
            "left at {stage:?} with {:?}",
            Seen::of(&world.cell)
        )),
    }
}

/// Follows every order from `path`, replayed from the start: the orders
/// followed, and the first rule broken with its order.
fn explore(path: &mut Vec<Step>, rules: Rules, count: &mut usize) -> Option<String> {
    let replay = |path: &[Step]| {
        let mut world = World::new();
        for &step in path {
            world.apply(step, rules);
        }
        world
    };
    let steps = replay(path).enabled();
    if steps.is_empty() {
        *count += 1;
        return stuck(&replay(path)).map(|why| format!("{why}: {path:?}"));
    }
    for step in steps {
        let mut world = replay(path);
        let before = Seen::of(&world.cell);
        let taking = world.taking;
        world.apply(step, rules);
        let after = Seen::of(&world.cell);
        path.push(step);
        if let Some(why) = broken(step, before, after, taking, &world) {
            return Some(format!("{why}: {path:?}"));
        }
        if let Some(why) = explore(path, rules, count) {
            return Some(why);
        }
        path.pop();
    }
    None
}

// Covers: T02
#[test]
fn a_video_follows_its_rules_in_every_order() {
    let mut count = 0;
    let broken = explore(&mut Vec::new(), SESSION, &mut count);
    assert_eq!(broken, None);
    assert!(count > 10_000, "only {count} orders");
}

/// The control: the order a screen meets when all goes well ends ready,
/// settled and never lost.
// Covers: T02
#[test]
fn the_ordinary_order_ends_ready() {
    let mut world = World::new();
    for step in [
        Step::Take(Result::Probe {
            refused: false,
            late: false,
        }),
        Step::Answer,
        Step::Confirm,
        Step::Look,
        Step::Take(Result::Playing { late: false }),
        Step::Answer,
        Step::Confirm,
        Step::Frame,
        Step::Expire,
        Step::Look,
    ] {
        let before = Seen::of(&world.cell);
        let taking = world.taking;
        world.apply(step, SESSION);
        assert_eq!(
            broken(step, before, Seen::of(&world.cell), taking, &world),
            None
        );
    }
    assert_eq!(world.stage, Some(Stage::Live));
    assert!(world.cell.ready() && world.cell.settled());
    assert_eq!(world.cell.loss(), None);
    assert_eq!(
        world.enabled(),
        [
            Step::Enter,
            Step::Frame,
            Step::Take(Result::Failed),
            Step::Eof
        ]
    );
}

/// A confirmation that withdraws nothing: an answer stands on the screen
/// read before it.
fn without_the_confirmation(stage: Stage, _: &Cell, _: u64, then: Then) -> (Stage, Then) {
    (stage, then)
}

/// The control: the exploration finds an answer standing for a screen
/// left when the session does not read the screen again after it.
// Covers: T02
#[test]
fn the_exploration_catches_an_answer_standing_for_a_screen_left() {
    let rules = Rules {
        answer: movie::answer,
        confirm: without_the_confirmation,
    };
    let broken = explore(&mut Vec::new(), rules, &mut 0);
    let broken = broken.expect("no order broke a rule");
    assert!(
        broken.starts_with("an answer stood for a screen left"),
        "{broken}"
    );
}

/// A rule that interprets a rejection before its deadline.
fn rejection_before_the_deadline(
    stage: Stage,
    movie: &Cell,
    current: u64,
    result: Outcome,
    late: bool,
    sought: u64,
) -> (Stage, Then) {
    let late = late && !matches!(result, Outcome::Rejected(_));
    movie::answer(stage, movie, current, result, late, sought)
}

/// The control: the exploration finds a late rejection taken as in time.
// Covers: T02
#[test]
fn the_exploration_catches_a_late_rejection_taken_in_time() {
    let rules = Rules {
        answer: rejection_before_the_deadline,
        confirm: movie::confirm,
    };
    let broken = explore(&mut Vec::new(), rules, &mut 0);
    let broken = broken.expect("no order broke a rule");
    assert!(broken.starts_with("a late answer left it"), "{broken}");
}
