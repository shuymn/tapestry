//! Where each player id stands in the worker, as the replies to the game's
//! own commands tell (design.md "Threat Model" T02): the files a worker
//! that ends without the game asking has open, each of which gets a strike
//! ([`super::strikes`]).
//!
//! Each command carries an id, which the replies that answer it echo
//! (`skin_model::media::control`). A player id moves through explicit
//! states, each change made by a reply that answers one of that player's
//! commands, matched by its id, or by its own failure:
//!
//! - **Opening**: the open is sent, and the worker holds the file's handle
//!   from then (the game hands it in before sending the command).
//! - **Open**: the worker answered the open with the probe; it holds the
//!   file.
//! - **Refused**: it answered with a refusal, or the player failed later;
//!   either is sent only once the player has closed the file, so the worker
//!   holds nothing for it. The id stays the player's until it is closed.
//! - **Closing**: the close is sent; the worker holds the file until it
//!   answers that the player closed, if it held it then. A second close is
//!   rejected by the worker, and its rejection, echoing that close's id,
//!   changes nothing.
//! - **Closed**: the worker answered the close; the id is free. An open the
//!   worker rejects (one it never took up) frees the id too.
//!
//! So a file is open in the worker, as far as the game can know, from its
//! open until the reply that tells it was closed; a worker that ends while
//! a reply is in flight still strikes it.
//!
//! A failure carries no command's id: a player's thread sends one only after
//! its probe and nothing after its close's answer, and the replies come in
//! the order the worker sent them, so a failure is always the current
//! player's.
//!
//! The code here is pure, generic over the file's identity, so a test can
//! follow every ordering of commands and replies.

use std::collections::{BTreeMap, BTreeSet};

use skin_model::media::control::Reply;

/// Where a player id stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State<F> {
    /// Open `open` is sent: the worker holds `file`.
    Opening { open: u32, file: F },
    /// Open `open` is answered with the probe: the worker holds `file`.
    Open { open: u32, file: F },
    /// Open `open` is answered with a refusal, or the player failed: the
    /// worker holds nothing for it.
    Refused { open: u32 },
    /// Close `close` is sent: the worker holds `file`, if any, until it
    /// answers.
    Closing {
        open: u32,
        close: u32,
        file: Option<F>,
    },
}

impl<F: Copy> State<F> {
    /// The file the worker holds for the player, if any.
    pub const fn file(&self) -> Option<F> {
        match *self {
            Self::Opening { file, .. } | Self::Open { file, .. } => Some(file),
            Self::Refused { .. } => None,
            Self::Closing { file, .. } => file,
        }
    }

    const fn open(&self) -> u32 {
        match *self {
            Self::Opening { open, .. }
            | Self::Open { open, .. }
            | Self::Refused { open }
            | Self::Closing { open, .. } => open,
        }
    }
}

/// The players of one worker, by id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Players<F>(BTreeMap<u8, State<F>>);

impl<F> Default for Players<F> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<F: Copy + Ord> Players<F> {
    /// Where `player` stands, `None` once it is closed (or never opened).
    pub fn state(&self, player: u8) -> Option<State<F>> {
        self.0.get(&player).copied()
    }

    /// Whether an open of `player` may be sent: the id is free.
    pub fn free(&self, player: u8) -> bool {
        !self.0.contains_key(&player)
    }

    /// Open `open` of `file` as `player`, a free id, is sent.
    pub fn opened(&mut self, player: u8, open: u32, file: F) {
        self.0
            .entry(player)
            .or_insert(State::Opening { open, file });
    }

    /// Close `close` of `player` is sent. A player already closing stays
    /// as it is: its first close's answer is the one that closes it.
    pub fn closing(&mut self, player: u8, close: u32) {
        if let Some(state) = self.0.get_mut(&player)
            && !matches!(state, State::Closing { .. })
        {
            *state = State::Closing {
                open: state.open(),
                close,
                file: state.file(),
            };
        }
    }

    /// Follows `reply`, which the game has taken.
    pub fn take(&mut self, reply: &Reply) {
        let player = match *reply {
            Reply::Probed { player, .. }
            | Reply::Refused { player, .. }
            | Reply::Rejected { player, .. }
            | Reply::Failed { player, .. }
            | Reply::Closed { player, .. } => player,
            _ => return,
        };
        let Some(&state) = self.0.get(&player) else {
            return;
        };
        let next = match (reply, state) {
            (Reply::Probed { id, .. }, State::Opening { open, file }) if *id == open => {
                Some(State::Open { open, file })
            }
            (Reply::Refused { id, .. }, State::Opening { open, .. }) if *id == open => {
                Some(State::Refused { open })
            }
            (Reply::Refused { id, .. }, State::Closing { open, close, .. }) if *id == open => {
                Some(State::Closing {
                    open,
                    close,
                    file: None,
                })
            }
            // The worker never took the open up: the id is free.
            (
                Reply::Rejected { id, .. },
                State::Opening { open, .. } | State::Closing { open, .. },
            ) if *id == open => None,
            (Reply::Failed { .. }, State::Open { open, .. }) => Some(State::Refused { open }),
            (Reply::Failed { .. }, State::Closing { open, close, .. }) => Some(State::Closing {
                open,
                close,
                file: None,
            }),
            (Reply::Closed { id, .. }, State::Closing { close, .. }) if *id == close => None,
            _ => Some(state),
        };
        match next {
            Some(next) => {
                self.0.insert(player, next);
            }
            None => {
                self.0.remove(&player);
            }
        }
    }

    /// The worker has ended: every player is gone. The files it held, each
    /// once, and the players it had.
    pub fn clear(&mut self) -> (BTreeSet<F>, Vec<u8>) {
        let all = std::mem::take(&mut self.0);
        let files = all.values().filter_map(State::file).collect();
        (files, all.into_keys().collect())
    }
}
