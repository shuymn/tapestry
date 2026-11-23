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
//! a reply is in flight still strikes it. A player the game closes for a
//! reason that is not its file's (its peer's loss) is spared: a worker the
//! game itself terminates does not strike its file, one that ends on its
//! own (a crash, its memory cap) does.
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

/// The players of one worker, by id, and those spared.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Players<F>(BTreeMap<u8, State<F>>, BTreeSet<u8>);

impl<F> Default for Players<F> {
    fn default() -> Self {
        Self(BTreeMap::new(), BTreeSet::new())
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

    /// `player`, closing, was closed for a reason that is not its file's:
    /// a worker the game terminates does not strike its file.
    pub fn spare(&mut self, player: u8) {
        if matches!(self.0.get(&player), Some(State::Closing { .. })) {
            self.1.insert(player);
        }
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
                self.1.remove(&player);
            }
        }
    }

    /// The worker has ended, `terminated` by the game or on its own: every
    /// player is gone. The files to strike, each once (those it held, but a
    /// spared player's when the game terminated it), and the players it
    /// had.
    pub fn clear(&mut self, terminated: bool) -> (BTreeSet<F>, Vec<u8>) {
        let all = std::mem::take(&mut self.0);
        let spared = std::mem::take(&mut self.1);
        let files = all
            .iter()
            .filter(|(player, _)| !(terminated && spared.contains(player)))
            .filter_map(|(_, state)| state.file())
            .collect();
        (files, all.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn probed(id: u32, player: u8) -> Reply {
        Reply::Probed {
            id,
            player,
            width: 64,
            height: 48,
            duration: 0,
            matrix: skin_model::media::control::Matrix::Bt601,
            range: skin_model::media::control::Range::Limited,
            codec: String::new(),
            container: String::new(),
        }
    }

    /// A spared player's file is struck by a worker that ends on its own
    /// and not by one the game terminates, unless another player that is
    /// not spared holds it too; an id spared and closed, then opened again,
    /// is no longer spared.
    // Covers: T02
    #[test]
    fn a_spared_file_is_struck_only_by_an_end_of_its_own() {
        const A: u8 = 10;
        const B: u8 = 20;
        const C: u8 = 30;
        let mut players = Players::default();
        players.opened(1, 1, A);
        players.opened(2, 2, B);
        players.take(&probed(1, 1));
        players.take(&probed(2, 2));
        // Spared only once closing.
        players.spare(2);
        assert_eq!(players.clone().clear(true).0, BTreeSet::from([A, B]));
        players.closing(2, 3);
        players.spare(2);
        assert_eq!(players.clone().clear(true).0, BTreeSet::from([A]));
        assert_eq!(players.clone().clear(false).0, BTreeSet::from([A, B]));
        // Another player not spared holds B too.
        let mut shared = players.clone();
        shared.opened(3, 4, B);
        assert_eq!(shared.clear(true).0, BTreeSet::from([A, B]));
        // Player 2 closed, its id opened again on C.
        players.take(&Reply::Closed { id: 3, player: 2 });
        players.opened(2, 5, C);
        assert_eq!(players.clear(true).0, BTreeSet::from([A, C]));
    }
}
