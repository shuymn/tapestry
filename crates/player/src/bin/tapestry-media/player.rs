//! One video's player: the thread that probes the file the game handed the
//! worker and then decodes it into the frame ring the game created for it.
//!
//! A player answers its open with the probe or a refusal, each echoing the
//! open's id, as every answer to a command echoes that command's. Once the game
//! hands it the ring and the ring's free-slot event, it plays the video
//! from its start in the ring's first run, 1, and from a seek's landing in
//! the run the seek names (runs reach it by command alone, never through
//! the ring; a seek drops the frames it held for the run before). The ring
//! is its frame queue, as a video player's decoder feeds one: it publishes
//! each frame whose stamp is not below the run's start and rises above the
//! last it published, in order, as soon as the frame is converted and a
//! slot is free, and waits on the event while every slot is full. A run
//! starts at the video's start (its first frame with a time) or at a
//! seek's landing (the first frame at or after the seek's time), and the
//! player reads nothing of the game's time: it drops no frame, and the
//! game drops one behind its clock only for a newer frame that is also
//! due. A frame is published before the next one is converted, so a
//! blocked conversion leaves the frame due in the ring. It answers the end
//! of the video, every seek (one that finds no frame with no landing, then
//! the run's end) and a failure; a failed or refused player only
//! waits to be closed. A looping play starts again at the video's start at
//! each end, in the same run, its stamps rising on, and never ends. It
//! counts every frame its decoder returns in the ring's progress word and
//! says in its state word whether it decodes, waits for a free slot or has
//! ended, which the game reads to supervise it. A refusal or a failure is
//! sent once the player has closed its file (and its ring), so a worker
//! that ends before sending it still has the file open as far as the game
//! knows. Closing unmaps its view of the ring before it answers.

use std::sync::mpsc::{Receiver, TryRecvError};

use skin_model::media::control::{Failure, NO_COMMAND, Reply};
use skin_model::media::{PROGRESS_WORD, Stamp, State, Writer};

use crate::ffmpeg::{Input, IoOpen};
use crate::os::{Event, Source, View};
use crate::{Out, ToPlayer};

/// How long a player with every slot full waits on the ring's event
/// before it looks for a command.
const SLOT_WAIT_MS: u32 = 50;

/// Where a player freezes for good (dev-tools): its thread stops, answering
/// nothing more, as a hung decoder would.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freeze {
    /// Before it answers its play.
    Play,
    /// Once it has answered its play, before its first frame, saying it
    /// decodes.
    Start,
    /// Once it has published a frame, saying it decodes.
    Decoding,
    /// Once it has published a frame, saying it waits for a free slot.
    Waiting,
}

/// The player a development fault names (dev-tools): by its number, or by
/// its video's codec as its probe names it, since the game numbers its
/// players in the order their opens reach it.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Player(u8),
    Codec(String),
}

#[cfg(feature = "dev-tools")]
impl Target {
    /// `<number>`, or a codec's name.
    pub fn parse(s: &str) -> Self {
        s.parse()
            .map_or_else(|_| Self::Codec(s.to_owned()), Self::Player)
    }

    /// Whether it names `player`, the calling player's thread's.
    fn names(&self, player: u8) -> bool {
        match self {
            Self::Player(p) => *p == player,
            Self::Codec(c) => CODEC.with(|codec| *codec.borrow() == *c),
        }
    }
}

#[cfg(feature = "dev-tools")]
thread_local! {
    /// The codec of the calling player's video, once probed (dev-tools).
    static CODEC: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The player that freezes, and where (dev-tools).
#[cfg(feature = "dev-tools")]
static FREEZE: std::sync::Mutex<Option<(Target, Freeze)>> = std::sync::Mutex::new(None);

/// Has the player `target` names freeze at `at` (dev-tools).
#[cfg(feature = "dev-tools")]
pub fn freeze(target: Target, at: Freeze) {
    if let Ok(mut f) = FREEZE.lock() {
        *f = Some((target, at));
    }
}

/// The faults `--dev-serve` names, fired at their own seams (dev-tools).
/// With `fire`, every one of them, `freeze` included, waits for the game's
/// [`skin_model::media::control::Command::Fire`], which a test sends once
/// its positive capture is in; without, each fires as soon as its seam is
/// reached. A fault firing answers [`Reply::Fired`] with the performance
/// counter read at its seam, just before it takes effect.
#[cfg(feature = "dev-tools")]
mod faults {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::Target;

    /// The faults wait for the game's `Fire`.
    static ON_FIRE: AtomicBool = AtomicBool::new(false);
    /// The game's `Fire` came.
    static FIRED: AtomicBool = AtomicBool::new(false);
    /// The player whose next decode is refused an allocation.
    static REFUSE: Mutex<Option<Target>> = Mutex::new(None);
    /// The player whose next frame goes out in a run not started.
    static GARBAGE: Mutex<Option<Target>> = Mutex::new(None);
    /// The balloon started at the fire: its step and total, MiB.
    static BALLOON: Mutex<Option<(usize, usize)>> = Mutex::new(None);

    pub fn on_fire() {
        ON_FIRE.store(true, Ordering::Release);
    }

    pub fn refuse_alloc(target: Target) {
        if let Ok(mut r) = REFUSE.lock() {
            *r = Some(target);
        }
    }

    pub fn garbage(target: Target) {
        if let Ok(mut g) = GARBAGE.lock() {
            *g = Some(target);
        }
    }

    pub fn balloon(step: usize, total: usize) {
        if let Ok(mut b) = BALLOON.lock() {
            *b = Some((step, total));
        }
    }

    /// Whether the faults may fire: the game's `Fire` came, or none is
    /// awaited.
    pub fn armed() -> bool {
        !ON_FIRE.load(Ordering::Acquire) || FIRED.load(Ordering::Acquire)
    }

    /// The game's `Fire` (the control loop's): arms the faults, once, and
    /// starts the balloon if one is set, whose seam is its thread's start.
    pub fn fire() {
        if FIRED.swap(true, Ordering::AcqRel) {
            return;
        }
        crate::say("tapestry-media: the faults are armed");
        if let Some((step, total)) = BALLOON.lock().ok().and_then(|b| *b) {
            let _ = std::thread::Builder::new()
                .name("balloon".to_owned())
                .spawn(move || {
                    crate::dev::fired();
                    crate::dev::inflate(step, total, 0, false);
                });
        }
    }

    /// Takes the fault `slot` holds for `player` once armed: true once.
    fn take(slot: &Mutex<Option<Target>>, player: u8) -> bool {
        armed()
            && slot
                .lock()
                .is_ok_and(|mut s| s.take_if(|t| t.names(player)).is_some())
    }

    pub fn refused(player: u8) -> bool {
        take(&REFUSE, player)
    }

    pub fn garbled(player: u8) -> bool {
        take(&GARBAGE, player)
    }
}

#[cfg(feature = "dev-tools")]
pub use faults::{balloon, fire, garbage, on_fire, refuse_alloc};

/// Freezes the calling player's thread for good when `player` is to
/// freeze at `at` (dev-tools) and the faults may fire, the fire stamped
/// first; returns at once otherwise. One player freezes, once; the control
/// loop goes on beating.
#[cfg(feature = "dev-tools")]
fn freeze_at(player: u8, at: Freeze) {
    let due = faults::armed()
        && FREEZE
            .lock()
            .is_ok_and(|mut f| f.take_if(|(t, a)| *a == at && t.names(player)).is_some());
    if due {
        crate::dev::fired();
        crate::say(&format!(
            "tapestry-media: player {player} freezes at {at:?}"
        ));
        loop {
            std::thread::park();
        }
    }
}

#[cfg(not(feature = "dev-tools"))]
const fn freeze_at(_: u8, _: Freeze) {}

/// Where a player may freeze (dev-tools; nothing without).
#[cfg(not(feature = "dev-tools"))]
#[derive(Clone, Copy)]
enum Freeze {
    Play,
    Start,
    Decoding,
    Waiting,
}

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
    #[cfg(feature = "dev-tools")]
    CODEC.with(|codec| codec.borrow_mut().clone_from(&probe.codec));
    out.send(&Reply::Probed {
        id: open,
        player,
        width,
        height,
        duration: probe.duration,
        matrix: probe.matrix,
        range: probe.range,
        codec: probe.codec,
        container: probe.container,
    });
    let (play, section, event, looping) = loop {
        match rx.recv() {
            Ok(ToPlayer::Play {
                id,
                section,
                event,
                looping,
            }) => break (id, section, event, looping),
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
    let mut input = input;
    if let Some(progress) = view.words().get(PROGRESS_WORD) {
        // SAFETY: the word lies in the view, which `Playing` drops after the
        // input (its field order), and the mapping does not move.
        unsafe { input.count_into(progress) };
    }
    writer.state(view.words(), State::Decoding);
    freeze_at(player, Freeze::Play);
    out.send(&Reply::Playing { id: play, player });
    freeze_at(player, Freeze::Start);
    let mut playing = Playing {
        player,
        looping,
        input,
        view,
        event,
        writer,
        frame: vec![0; writer.geometry().frame_bytes()],
        run: 1,
        floor: 0,
        last: None,
        seq: 0,
        held: None,
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

/// A player decoding into its ring. The input drops before the view: it
/// counts its decoded frames in the view's progress word.
struct Playing {
    player: u8,
    /// From the start again at each end.
    looping: bool,
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
    /// The stamp of the frame in `frame`, converted and not published yet.
    held: Option<Stamp>,
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
                Some(ToPlayer::Play {
                    id, section, event, ..
                }) => {
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
    /// with where it landed, or, when nothing is at or after it, with no
    /// landing and then the run's end: every seek is answered.
    fn seek(&mut self, id: u32, run: u64, at: i64, out: &Out) -> Result<(), (Failure, i32)> {
        self.run = run;
        self.floor = at;
        self.last = None;
        self.held = None;
        self.ended = false;
        self.writer.state(self.view.words(), State::Decoding);
        match self.input.seek(at, &mut self.frame)? {
            Some(landed) => {
                self.held = Some(Stamp {
                    at: landed.stamp,
                    duration: self.input.duration(),
                });
                out.send(&Reply::Sought {
                    id,
                    player: self.player,
                    run,
                    landed: Some(landed.stamp),
                    attempts: landed.attempts,
                });
            }
            None => {
                out.send(&Reply::Sought {
                    id,
                    player: self.player,
                    run,
                    landed: None,
                    attempts: 0,
                });
                self.ended = true;
                self.writer.state(self.view.words(), State::Ended);
                out.send(&Reply::Ended {
                    player: self.player,
                    run,
                });
            }
        }
        Ok(())
    }

    /// Decodes the next frame, not converted yet: a frame to publish when
    /// it is not below the run's start and rises above `after`, one skipped
    /// otherwise, or the video's end (a looping play starts again instead).
    fn decode(&mut self, after: Option<i64>) -> Result<Decoded, (Failure, i32)> {
        let floor = self.floor;
        let keep = |stamp: i64| stamp >= floor && after.is_none_or(|l| stamp > l);
        let timed = |input: &Input, at: i64| Stamp {
            at,
            duration: input.duration(),
        };
        Ok(match self.input.next()? {
            Some(stamp) if keep(stamp) => Decoded::Frame(timed(&self.input, stamp)),
            Some(_) => Decoded::Skipped,
            None if self.looping => {
                let stamp = self.input.restart()?;
                if keep(stamp) {
                    Decoded::Frame(timed(&self.input, stamp))
                } else {
                    Decoded::Skipped
                }
            }
            None => Decoded::End,
        })
    }

    /// The run's video ended: said once, in its state word and a reply.
    fn end(&mut self, out: &Out) {
        self.ended = true;
        self.writer.state(self.view.words(), State::Ended);
        out.send(&Reply::Ended {
            player: self.player,
            run: self.run,
        });
    }

    /// One step: decodes and converts a frame while it holds none, or
    /// publishes the frame held, or waits a while for a free slot.
    fn step(&mut self, out: &Out) -> Result<(), (Failure, i32)> {
        let words = self.view.words();
        let Some(stamp) = self.held else {
            #[cfg(feature = "dev-tools")]
            if faults::refused(self.player) {
                // The seam: the decode below finds the refusal recorded for
                // this player, as one its decoder met.
                crate::dev::fired();
                crate::say(&format!(
                    "tapestry-media: player {} is refused an allocation",
                    self.player
                ));
                crate::ffmpeg::refuse_now();
            }
            match self.decode(self.last)? {
                Decoded::Frame(stamp) => {
                    self.input.convert(&mut self.frame)?;
                    self.held = Some(stamp);
                }
                Decoded::Skipped => {}
                Decoded::End => self.end(out),
            }
            return Ok(());
        };
        let Some(slot) = self.writer.claim(words) else {
            self.writer.state(words, State::Waiting);
            if self.seq > 0 {
                freeze_at(self.player, Freeze::Waiting);
            }
            self.event.wait(SLOT_WAIT_MS);
            return Ok(());
        };
        self.writer.state(words, State::Decoding);
        #[cfg(feature = "dev-tools")]
        let run = if faults::garbled(self.player) {
            crate::dev::fired();
            crate::say(&format!(
                "tapestry-media: player {} writes a run not started",
                self.player
            ));
            self.run.saturating_add(1_000_000)
        } else {
            self.run
        };
        #[cfg(not(feature = "dev-tools"))]
        let run = self.run;
        let seq = self.seq.saturating_add(1);
        self.writer
            .publish(words, slot, &self.frame, seq, run, stamp)
            .map_err(|_| (Failure::Ring, 0))?;
        self.seq = seq;
        self.last = Some(stamp.at);
        self.held = None;
        freeze_at(self.player, Freeze::Decoding);
        Ok(())
    }
}

/// What a decode gave.
enum Decoded {
    /// A frame to publish, by its stamp and duration, for the input to
    /// convert.
    Frame(Stamp),
    /// A frame not to publish (below the run's start, or not rising).
    Skipped,
    /// The video's end.
    End,
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
            Ok(ToPlayer::Play {
                id, section, event, ..
            }) => {
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
