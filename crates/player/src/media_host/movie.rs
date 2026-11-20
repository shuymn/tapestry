//! Where a video stands, and how the session's results move it (design.md
//! "Threat Model" T02). The state is one atomic word, which the game
//! thread (its preparation deadline), the render side (its first frame,
//! its ring's fault) and the session (its answers and losses) each change
//! by one compare-and-swap, so no change is made on a state read apart from
//! it. Lost is for good, and carries its first reason.
//!
//! The session's part is pure: [`answer`] takes a player's stage, its
//! video and the screen being shown, and a result the worker sent (matched
//! by its command's id, judged late or in time on its arrival), and gives
//! the stage that follows and what the session does next. The screen shown
//! is a shared word the game changes when it leaves a screen, the
//! cancellation of that screen's videos: [`confirm`] reads it again after
//! the answer's step, and withdraws a step that a screen change raced, so
//! a result never stands for a screen left. A test follows every order of
//! these steps within a bound.
//!
//! The game thread's and the render side's steps are in play: nothing
//! here allocates, logs or panics.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicU8, Ordering};

/// Where a video stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Asked for; no answer yet.
    Pending,
    /// Refused at its probe: an absent picture.
    Absent,
    /// Its ring is set up; its frames may come.
    Live,
    /// Lost, for good.
    Lost,
}

/// Why a video was lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Loss {
    /// The session's queue was full or gone.
    Queue = 0,
    /// The worker could not be launched or checked, or the game has no
    /// processor to keep from it.
    Launch,
    /// The file has two strikes this run.
    Struck,
    /// The file could not be handed in or its command queued.
    Open,
    /// A command write stuck, the writer gone, a reply that does not
    /// decode, one past the queue, or the replies ended.
    Transport,
    /// No beat in time.
    Heartbeat,
    /// No answer in time.
    Answer,
    /// No progress in time.
    Progress,
    /// The worker ended on its own or at its memory cap.
    Ended,
    /// The player failed, a command was rejected, or its player closed.
    Failed,
    /// The screen's preparation deadline passed first.
    Preparation,
    /// The render side found its ring at fault.
    Fault,
}

impl Loss {
    const ALL: [Self; 12] = [
        Self::Queue,
        Self::Launch,
        Self::Struck,
        Self::Open,
        Self::Transport,
        Self::Heartbeat,
        Self::Answer,
        Self::Progress,
        Self::Ended,
        Self::Failed,
        Self::Preparation,
        Self::Fault,
    ];
}

// The word: the state in the low three bits, a loss's reason above.
const PENDING: u8 = 0;
const ABSENT: u8 = 1;
const LIVE: u8 = 2;
/// Live, its first frame staged.
const READY: u8 = 3;
const LOST: u8 = 4;
const STATE: u8 = 0b111;

const fn lost(why: Loss) -> u8 {
    LOST | ((why as u8) << 3)
}

/// A video's state, shared between the game, the render side and the
/// session.
#[derive(Debug)]
pub struct Cell {
    generation: u64,
    word: AtomicU8,
}

impl Cell {
    /// A pending video of the screen of `generation`.
    pub const fn new(generation: u64) -> Self {
        Self {
            generation,
            word: AtomicU8::new(PENDING),
        }
    }

    /// The generation of the screen that asked for it.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    fn word(&self) -> u8 {
        self.word.load(Ordering::Acquire)
    }

    /// Changes the state by `step`, one compare-and-swap at a time: whether
    /// `step` changed it.
    fn change(&self, step: impl Fn(u8) -> Option<u8>) -> bool {
        self.word
            .try_update(Ordering::AcqRel, Ordering::Acquire, |w| step(w & STATE))
            .is_ok()
    }

    /// Where it stands.
    pub fn status(&self) -> Status {
        match self.word() & STATE {
            PENDING => Status::Pending,
            ABSENT => Status::Absent,
            LIVE | READY => Status::Live,
            _ => Status::Lost,
        }
    }

    /// Why it was lost, once it is.
    pub fn loss(&self) -> Option<Loss> {
        let w = self.word();
        if w & STATE != LOST {
            return None;
        }
        Loss::ALL.get(usize::from(w >> 3)).copied()
    }

    /// Whether its first frame was staged, and it is not lost.
    pub fn ready(&self) -> bool {
        self.word() & STATE == READY
    }

    /// Loses it for `why`, unless it is lost already: whether this did.
    /// Writes nothing but the word.
    pub fn lose(&self, why: Loss) -> bool {
        self.change(|s| (s != LOST).then_some(lost(why)))
    }

    /// The screen's preparation deadline has passed (L5): unless its first
    /// frame was staged, it is absent or lost already, it is lost. Whether
    /// this lost it.
    pub fn expire(&self) -> bool {
        self.change(|s| matches!(s, PENDING | LIVE).then_some(lost(Loss::Preparation)))
    }

    /// Whether it reached a terminal outcome for readiness: its first frame
    /// staged, absent, or lost.
    pub fn settled(&self) -> bool {
        !matches!(self.word() & STATE, PENDING | LIVE)
    }

    /// The render side staged a frame of run 1: a live video is ready.
    pub fn staged_first(&self) -> bool {
        self.change(|s| (s == LIVE).then_some(READY))
    }

    /// A pending video's probe refused it.
    pub fn refuse(&self) -> bool {
        self.change(|s| (s == PENDING).then_some(ABSENT))
    }

    /// A pending video's ring is set up and its play answered.
    pub fn go_live(&self) -> bool {
        self.change(|s| (s == PENDING).then_some(LIVE))
    }

    /// Its screen was left before an answer's step stood: what the step
    /// made it (live, ready or absent) is withdrawn, pending again.
    fn withdraw(&self) -> bool {
        self.change(|s| matches!(s, LIVE | READY | ABSENT).then_some(PENDING))
    }
}

/// Where a player stands in the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Waiting for the worker to launch.
    Queued,
    /// Its open sent (the command's id).
    Opening(u32),
    /// Its play sent.
    Playing(u32),
    /// Playing into its ring.
    Live,
    /// Its close sent.
    Closing(u32),
    /// Refused or failed in the worker, which waits to close it.
    Idle,
}

/// A result the worker sent about a player, as the session judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its open's probe (the open's id).
    Probed(u32),
    /// Its open's refusal.
    Refused(u32),
    /// Its play's answer.
    Playing(u32),
    /// A command of its rejected.
    Rejected(u32),
    /// The player failed.
    Failed,
}

/// What the session does after a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Nothing more.
    Nothing,
    /// Set up the ring and send the play.
    Play,
    /// Send the close.
    Close,
    /// The answer came past its deadline: the worker is marked, the player
    /// closed.
    Late,
    /// The answer came past its deadline and left nothing to close: the
    /// worker is marked.
    Mark,
}

/// What `result`, which arrived `late` or in time, does to a player at
/// `stage` whose video is `movie` while the screen of `current` is shown:
/// the stage that follows and what the session does next. A result that
/// does not answer the stage's command changes nothing; the video moves
/// only by its cell's own steps, and only for the screen shown.
pub fn answer(
    stage: Stage,
    movie: &Cell,
    current: u64,
    result: Outcome,
    late: bool,
) -> (Stage, Then) {
    let shown = movie.generation() == current;
    match (stage, result) {
        // The deadline first, whatever the answer says.
        (Stage::Opening(open), Outcome::Probed(id) | Outcome::Refused(id))
        | (Stage::Playing(open), Outcome::Playing(id) | Outcome::Rejected(id))
            if open == id && late =>
        {
            movie.lose(Loss::Answer);
            (stage, Then::Late)
        }
        // The open was not taken up: nothing to close, the id is free.
        (Stage::Opening(open), Outcome::Rejected(id)) if open == id && late => {
            movie.lose(Loss::Answer);
            (Stage::Idle, Then::Mark)
        }
        (Stage::Opening(open), Outcome::Probed(id)) if open == id => {
            if shown && movie.status() == Status::Pending {
                (stage, Then::Play)
            } else {
                (stage, Then::Close)
            }
        }
        (Stage::Opening(open), Outcome::Refused(id)) if open == id => {
            if shown {
                movie.refuse();
            }
            (Stage::Idle, Then::Close)
        }
        (Stage::Playing(play), Outcome::Playing(id)) if play == id => {
            if shown && movie.go_live() {
                (Stage::Live, Then::Nothing)
            } else {
                (stage, Then::Close)
            }
        }
        // The open was not taken up: nothing to close, the id is free.
        (Stage::Opening(open), Outcome::Rejected(id)) if open == id => {
            movie.lose(Loss::Failed);
            (Stage::Idle, Then::Nothing)
        }
        (Stage::Playing(play), Outcome::Rejected(id)) if play == id => {
            movie.lose(Loss::Failed);
            (Stage::Idle, Then::Close)
        }
        (Stage::Playing(_) | Stage::Live | Stage::Idle, Outcome::Failed) => {
            movie.lose(Loss::Failed);
            (Stage::Idle, Then::Close)
        }
        _ => (stage, Then::Nothing),
    }
}

/// After [`answer`] gave `stage` and `then` for a video, with `current` the
/// screen shown read again after it: when the video's screen is no longer
/// shown, what the answer made it is withdrawn and the player closed.
/// Either the screen changed after this read, and the answer stood while
/// its screen was shown, or before it, and the answer is withdrawn: no
/// answer stands for a screen left.
pub fn confirm(stage: Stage, movie: &Cell, current: u64, then: Then) -> (Stage, Then) {
    if movie.generation() == current {
        return (stage, then);
    }
    movie.withdraw();
    match (stage, then) {
        (Stage::Live, Then::Nothing) | (_, Then::Play) => (stage, Then::Close),
        _ => (stage, then),
    }
}

/// Whether the session closes a player at `stage` whose video is `movie`
/// while the screen of `current` is shown: one of another screen, or whose
/// video is lost, that the worker still has.
pub fn unwanted(stage: Stage, movie: &Cell, current: u64) -> bool {
    let gone = movie.generation() != current || movie.status() == Status::Lost;
    match stage {
        Stage::Live | Stage::Idle => gone,
        Stage::Opening(_) | Stage::Playing(_) => movie.generation() != current,
        Stage::Queued | Stage::Closing(_) => false,
    }
}
