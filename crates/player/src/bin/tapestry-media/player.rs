//! One video's player: the thread that probes the file the game handed the
//! worker and then decodes it into the frame ring the game created for it.
//!
//! A player answers its open with the probe or a refusal, each echoing the
//! open's id, as every answer to a command echoes that command's. Once the game
//! hands it the ring and the ring's free-slot event, it decodes from the
//! video's start in the ring's first run, 1, and from a seek's landing in
//! the run the seek names (runs reach it by command alone, never through
//! the ring; a seek drops the frame it held for the run before), publishing
//! each frame whose stamp is not below the run's start and rises above the
//! last it published, and waits on the event while every slot is full. It
//! answers the end of the video, a seek and a failure; a failed or refused
//! player only waits to be closed. A refusal or a failure is sent once the
//! player has closed its file (and its ring), so a worker that ends before
//! sending it still has the file open as far as the game knows. Closing
//! unmaps its view of the ring before it answers.

use std::sync::mpsc::{Receiver, TryRecvError};

use skin_model::media::Writer;
use skin_model::media::control::{Failure, NO_COMMAND, Reply};

use crate::ffmpeg::{Input, IoOpen};
use crate::os::{Event, Source, View};
use crate::{Out, ToPlayer};

/// How long a player with every slot full waits on the free-slot event
/// before it looks for a command.
const SLOT_WAIT_MS: u32 = 50;

/// How a playing player stopped.
enum Stopped {
    /// The game closed it with command `id` (or the control loop went).
    Closed(u32),
    /// It failed.
    Failed(Failure, i32),
}

/// Plays the video in `file`, the file handle the worker took for
/// `player` by open `open`, until the game closes it.
pub fn run(player: u8, open: u32, file: Source, rx: &Receiver<ToPlayer>, out: &Out) {
    let (input, probe) = match Input::open(file, IoOpen::Refuse) {
        Ok(opened) => opened,
        // The input, and the file with it, is dropped before the refusal.
        Err((why, code)) => {
            out.send(&Reply::Refused {
                id: open,
                player,
                why,
                code,
            });
            idle(player, rx, out);
            return;
        }
    };
    let (width, height) = (probe.width, probe.height);
    out.send(&Reply::Probed {
        id: open,
        player,
        width,
        height,
        duration: probe.duration,
        codec: probe.codec,
        container: probe.container,
    });
    let (play, section, event) = loop {
        match rx.recv() {
            Ok(ToPlayer::Play { id, section, event }) => break (id, section, event),
            Ok(ToPlayer::Seek { id, .. }) => crate::reject(out, id, player, 0),
            Ok(ToPlayer::Close { id }) => {
                drop(input);
                out.send(&Reply::Closed { id, player });
                return;
            }
            Err(_) => {
                drop(input);
                out.send(&Reply::Closed {
                    id: NO_COMMAND,
                    player,
                });
                return;
            }
        }
    };
    let event = Event::new(event);
    let view = match View::map(section) {
        Ok(view) => view,
        Err(e) => {
            drop((input, event));
            fail(player, out, Failure::Ring, e.raw_os_error().unwrap_or(0));
            idle(player, rx, out);
            return;
        }
    };
    let writer = match Writer::attach(view.words()) {
        Ok(w) if w.geometry().width() == width && w.geometry().height() == height => w,
        _ => {
            drop((input, view, event));
            fail(player, out, Failure::Ring, 0);
            idle(player, rx, out);
            return;
        }
    };
    out.send(&Reply::Playing { id: play, player });
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
    let stopped = playing.play(rx, out);
    drop(playing);
    match stopped {
        Stopped::Failed(why, code) => {
            fail(player, out, why, code);
            idle(player, rx, out);
        }
        Stopped::Closed(id) => out.send(&Reply::Closed { id, player }),
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
    /// Decodes and publishes until the game closes the player or the
    /// player fails, which its caller answers once the player is dropped.
    fn play(&mut self, rx: &Receiver<ToPlayer>, out: &Out) -> Stopped {
        loop {
            let message = if self.ended {
                rx.recv().ok()
            } else {
                match rx.try_recv() {
                    Ok(m) => Some(m),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return Stopped::Closed(NO_COMMAND),
                }
            };
            match message {
                Some(ToPlayer::Close { id }) => return Stopped::Closed(id),
                Some(ToPlayer::Play { id, section, event }) => {
                    drop((section, event));
                    crate::reject(out, id, self.player, 0);
                    continue;
                }
                Some(ToPlayer::Seek { id, run, at }) => {
                    if let Err((why, code)) = self.seek(id, run, at, out) {
                        return Stopped::Failed(why, code);
                    }
                    continue;
                }
                None if self.ended => return Stopped::Closed(NO_COMMAND),
                None => {}
            }
            if let Err((why, code)) = self.step(out) {
                return Stopped::Failed(why, code);
            }
        }
    }

    /// Positions the video at `at` µs for run `run` and answers seek `id`
    /// with where it landed, or that nothing is at or after it.
    fn seek(&mut self, id: u32, run: u64, at: i64, out: &Out) -> Result<(), (Failure, i32)> {
        self.run = run;
        self.floor = at;
        self.last = None;
        self.pending = None;
        self.ended = false;
        match self.input.seek(at, &mut self.frame)? {
            Some(landed) => {
                self.pending = Some(landed.stamp);
                out.send(&Reply::Sought {
                    id,
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

/// Answers a player's failure, once it holds nothing.
fn fail(player: u8, out: &Out, why: Failure, code: i32) {
    out.send(&Reply::Failed { player, why, code });
}

/// Waits for the game to close a player that holds nothing more, rejecting
/// any other command and closing the handles one names.
fn idle(player: u8, rx: &Receiver<ToPlayer>, out: &Out) {
    let id = loop {
        match rx.recv() {
            Ok(ToPlayer::Play { id, section, event }) => {
                drop((section, event));
                crate::reject(out, id, player, 0);
            }
            Ok(ToPlayer::Seek { id, .. }) => crate::reject(out, id, player, 0),
            Ok(ToPlayer::Close { id }) => break id,
            Err(_) => break NO_COMMAND,
        }
    };
    out.send(&Reply::Closed { id, player });
}
