//! One video's player: the thread that probes the file the game handed the
//! worker and then decodes it into the frame ring the game created for it.
//!
//! A player answers its open with the probe or a refusal. Once the game
//! hands it the ring and the ring's free-slot event, it decodes from the
//! video's start in the ring's first run, 1, and from a seek's landing in
//! the run the seek names (runs reach it by command alone, never through
//! the ring; a seek drops the frame it held for the run before), publishing
//! each frame whose stamp is not below the run's start and rises above the
//! last it published, and waits on the event while every slot is full. It
//! answers the end of the video, a seek and a failure; a failed or refused
//! player only waits to be closed. Closing unmaps its view of the ring
//! before it answers.

use std::sync::mpsc::{Receiver, TryRecvError};

use skin_model::media::Writer;
use skin_model::media::control::{Failure, Reply};

use crate::ffmpeg::{Input, IoOpen};
use crate::os::{Event, Owned, View};
use crate::{Out, ToPlayer};

/// How long a player with every slot full waits on the free-slot event
/// before it looks for a command.
const SLOT_WAIT_MS: u32 = 50;

/// Plays the video whose file handle the game handed the worker as `file`,
/// until the game closes it.
pub fn run(player: u8, file: Owned, rx: &Receiver<ToPlayer>, out: &Out) {
    let (input, probe) = match Input::open(file.into_file(), IoOpen::Refuse) {
        Ok(opened) => opened,
        Err((why, code)) => {
            out.send(&Reply::Refused { player, why, code });
            idle(player, rx, out);
            return;
        }
    };
    let (width, height) = (probe.width, probe.height);
    out.send(&Reply::Probed {
        player,
        width,
        height,
        duration: probe.duration,
        codec: probe.codec,
        container: probe.container,
    });
    let (section, event) = loop {
        match rx.recv() {
            Ok(ToPlayer::Play { section, event }) => break (section, event),
            Ok(ToPlayer::Seek { .. }) => refuse_command(player, out),
            Ok(ToPlayer::Close) | Err(_) => {
                drop(input);
                out.send(&Reply::Closed { player });
                return;
            }
        }
    };
    let event = Event::adopt(event);
    let view = match View::map(Owned::adopt(section)) {
        Ok(view) => view,
        Err(e) => {
            fail(player, out, Failure::Ring, e.raw_os_error().unwrap_or(0));
            drop((input, event));
            idle(player, rx, out);
            return;
        }
    };
    let writer = match Writer::attach(view.words()) {
        Ok(w) if w.geometry().width() == width && w.geometry().height() == height => w,
        _ => {
            fail(player, out, Failure::Ring, 0);
            drop((input, view, event));
            idle(player, rx, out);
            return;
        }
    };
    out.send(&Reply::Playing { player });
    let mut playing = Playing {
        player,
        input,
        view,
        event,
        writer,
        frame: vec![0; writer.geometry().frame_bytes()],
        run: 1,
        floor: 0,
        last: None,
        seq: 0,
        pending: None,
        ended: false,
    };
    let failed = playing.play(rx, out);
    drop(playing);
    if failed {
        idle(player, rx, out);
    } else {
        out.send(&Reply::Closed { player });
    }
}

/// A player decoding into its ring.
struct Playing {
    player: u8,
    input: Input,
    view: View,
    event: Event,
    writer: Writer,
    /// The next frame to publish, I420 at the ring's size.
    frame: Vec<u8>,
    /// The run the frames go out in.
    run: u64,
    /// The run's start: an earlier frame is not published.
    floor: i64,
    /// The stamp of the last frame published in the run.
    last: Option<i64>,
    /// The sequence of the last frame published in the ring.
    seq: u64,
    /// The stamp of the frame in `frame`, while it waits for a slot.
    pending: Option<i64>,
    /// The run's video has ended.
    ended: bool,
}

impl Playing {
    /// Decodes and publishes until the game closes the player (`false`) or
    /// the player fails (`true`, the failure answered).
    fn play(&mut self, rx: &Receiver<ToPlayer>, out: &Out) -> bool {
        loop {
            let message = if self.ended {
                rx.recv().ok()
            } else {
                match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return false,
                }
            };
            match message {
                Some(ToPlayer::Close) => return false,
                Some(ToPlayer::Play { section, event }) => {
                    drop((Owned::adopt(section), Owned::adopt(event)));
                    refuse_command(self.player, out);
                    continue;
                }
                Some(ToPlayer::Seek { run, at }) => {
                    if let Err((why, code)) = self.seek(run, at, out) {
                        fail(self.player, out, why, code);
                        return true;
                    }
                    continue;
                }
                None if self.ended => return false,
                None => {}
            }
            if let Err((why, code)) = self.step(out) {
                fail(self.player, out, why, code);
                return true;
            }
        }
    }

    /// Positions the video at `at` µs for run `run` and answers where it
    /// landed, or that nothing is at or after it.
    fn seek(&mut self, run: u64, at: i64, out: &Out) -> Result<(), (Failure, i32)> {
        self.run = run;
        self.floor = at;
        self.last = None;
        self.pending = None;
        self.ended = false;
        match self.input.seek(at, &mut self.frame)? {
            Some(landed) => {
                self.pending = Some(landed.stamp);
                out.send(&Reply::Sought {
                    player: self.player,
                    run,
                    landed: landed.stamp,
                    attempts: landed.attempts,
                });
            }
            None => {
                self.ended = true;
                out.send(&Reply::Ended {
                    player: self.player,
                    run,
                });
            }
        }
        Ok(())
    }

    /// Decodes the next frame to publish, or publishes the one waiting, or
    /// waits a while for a free slot.
    fn step(&mut self, out: &Out) -> Result<(), (Failure, i32)> {
        let Some(stamp) = self.pending else {
            match self.input.next(&mut self.frame)? {
                Some(stamp) if stamp >= self.floor && self.last.is_none_or(|l| stamp > l) => {
                    self.pending = Some(stamp);
                }
                Some(_) => {}
                None => {
                    self.ended = true;
                    out.send(&Reply::Ended {
                        player: self.player,
                        run: self.run,
                    });
                }
            }
            return Ok(());
        };
        let words = self.view.words();
        let Some(slot) = self.writer.claim(words) else {
            self.event.wait(SLOT_WAIT_MS);
            return Ok(());
        };
        let seq = self.seq.saturating_add(1);
        self.writer
            .publish(words, slot, &self.frame, seq, self.run, stamp)
            .map_err(|_| (Failure::Ring, 0))?;
        self.seq = seq;
        self.last = Some(stamp);
        self.pending = None;
        Ok(())
    }
}

/// Answers a player's failure.
fn fail(player: u8, out: &Out, why: Failure, code: i32) {
    out.send(&Reply::Failed { player, why, code });
}

/// Answers a command the player cannot take in its state.
fn refuse_command(player: u8, out: &Out) {
    fail(player, out, Failure::Command, 0);
}

/// Waits for the game to close a player that holds nothing more, refusing
/// any other command and closing the handles one names.
fn idle(player: u8, rx: &Receiver<ToPlayer>, out: &Out) {
    loop {
        match rx.recv() {
            Ok(ToPlayer::Play { section, event }) => {
                drop((Owned::adopt(section), Owned::adopt(event)));
                refuse_command(player, out);
            }
            Ok(ToPlayer::Seek { .. }) => refuse_command(player, out),
            Ok(ToPlayer::Close) | Err(_) => break,
        }
    }
    out.send(&Reply::Closed { player });
}
