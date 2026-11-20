//! The media worker (design.md "Threat Model" T02; "Libraries", the FFmpeg
//! row): the one binary that links FFmpeg. It decodes the game's videos
//! into their frame rings (`skin_model::media`) as the game commands it on
//! its control pipe, its standard input and output
//! (`skin_model::media::control`): it answers with its FFmpeg's version
//! and configure line, then probes each file the game hands it by handle,
//! refusing what is not a listed container and codec within the pixel cap,
//! and decodes a probed file into the ring the game created for it, from
//! its start or from a seek, until the game closes it. Each video plays on
//! a thread of its own. Its and FFmpeg's diagnostics go to the game as log
//! replies, a line each, and to standard error in a dev-tools command.
//!
//! [`ffmpeg`] holds every call into FFmpeg; [`os`] the Windows calls on the
//! handles the game hands the worker, which it takes only as [`os`] checks
//! them.
//!
//! With dev-tools: `--dev-tools-probe` says the build has the development
//! features, `--dev-gen-clip` encodes a synthesized clip through the
//! build's encoders and muxers, `--dev-secondary-open` reports what a
//! secondary open through an opened file's I/O context gives, and
//! `--dev-probe` tries what the sandbox denies the worker.

#[cfg(not(windows))]
compile_error!("tapestry-media is Windows-only");

#[cfg(feature = "dev-tools")]
mod dev;
mod ffmpeg;
mod handles;
mod os;
mod player;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use skin_model::media::control::{self, Command, Reply};

/// The most players the worker plays at once: four a screen, and those of a
/// generation the game is closing.
const MAX_PLAYERS: usize = 8;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return serve(ffmpeg::MAX_ALLOC);
    }
    #[cfg(feature = "dev-tools")]
    if let Some(code) = dev::run(&args) {
        return code;
    }
    eprintln!(
        "unknown argument: {}",
        args.first()
            .map_or_else(String::new, |a| a.to_string_lossy().into_owned())
    );
    ExitCode::from(2)
}

/// The control pipe's writing end, shared by the players' threads.
#[derive(Clone)]
pub struct Out(Arc<Mutex<std::io::Stdout>>);

/// The control pipe once the worker serves: diagnostics go there.
static OUT: OnceLock<Out> = OnceLock::new();

/// Reports `line`, a diagnostic: to the game as a log reply while the
/// worker serves, else to standard error.
pub fn say(line: &str) {
    match OUT.get() {
        Some(out) => out.send(&Reply::Log {
            text: line.to_owned(),
        }),
        None => eprintln!("{line}"),
    }
}

impl Out {
    /// Writes `reply` as one frame (every reply's encoding fits one). A
    /// pipe the game no longer reads ends the worker: it has nobody to
    /// decode for.
    pub fn send(&self, reply: &Reply) {
        let Some(bytes) = control::frame(&reply.encode()) else {
            say("tapestry-media: a reply that does not fit a frame");
            return;
        };
        let written = match self.0.lock() {
            Ok(mut out) => out.write_all(&bytes).and_then(|()| out.flush()),
            Err(_) => return,
        };
        if written.is_err() {
            std::process::exit(0);
        }
    }
}

/// What the control loop tells a player's thread, with the id of the
/// command it carries out.
pub enum ToPlayer {
    Play {
        id: u32,
        section: os::Owned,
        event: os::Owned,
    },
    Seek {
        id: u32,
        run: u64,
        at: i64,
    },
    Close {
        id: u32,
    },
}

/// Answers the game's launch with the hello on the control pipe, to which
/// diagnostics go from then on: the pipe. A worker whose hooks in FFmpeg
/// cannot be set ends instead, before it answers.
fn hello(max_alloc: usize) -> Out {
    if let Err(e) = ffmpeg::init(max_alloc) {
        say(&format!("tapestry-media: FFmpeg's hooks: {e}"));
        std::process::exit(1);
    }
    let out = OUT.get_or_init(|| Out(Arc::new(Mutex::new(std::io::stdout()))));
    let (version, configuration) = ffmpeg::version();
    out.send(&Reply::Hello {
        version,
        configuration,
    });
    if let Err(e) = os::learn() {
        say(&format!(
            "tapestry-media: every handle a command names is refused: {e}"
        ));
    }
    out.clone()
}

/// Reads commands until the game quits or closes the pipe, FFmpeg's largest
/// allocation capped at `max_alloc` bytes.
fn serve(max_alloc: usize) -> ExitCode {
    serve_on(&hello(max_alloc))
}

/// Reads commands until the game quits or closes the pipe, after the hello
/// (`out`).
fn serve_on(out: &Out) -> ExitCode {
    let mut stdin = std::io::stdin().lock();
    let mut players: BTreeMap<u8, (Sender<ToPlayer>, JoinHandle<()>)> = BTreeMap::new();
    let mut closing: Vec<JoinHandle<()>> = Vec::new();
    let mut quit = control::NO_COMMAND;
    while let Some((id, command)) = read_command(&mut stdin) {
        closing.retain(|t| !t.is_finished());
        match command {
            Command::Open { player, file } => {
                let file = match os::Owned::take(file, os::Kind::File) {
                    Ok(file) => file,
                    Err(code) => {
                        refuse_handle(out, id, player, code);
                        continue;
                    }
                };
                if players.contains_key(&player) || players.len() >= MAX_PLAYERS {
                    drop(file);
                    reject(out, id, player, 0);
                    continue;
                }
                let (tx, rx) = mpsc::channel();
                let thread_out = out.clone();
                // The thread owns the handle: one that does not start closes
                // it with its closure, before the rejection is sent.
                let source = os::Source::taken(file);
                let spawned = std::thread::Builder::new()
                    .name(format!("player {player}"))
                    .spawn(move || player::run(player, id, source, &rx, &thread_out));
                match spawned {
                    Ok(thread) => {
                        players.insert(player, (tx, thread));
                    }
                    Err(e) => reject(out, id, player, e.raw_os_error().unwrap_or(0)),
                }
            }
            Command::Play {
                player,
                section,
                event,
            } => {
                let taken = os::Owned::take(section, os::Kind::Section).and_then(|section| {
                    os::Owned::take(event, os::Kind::Event).map(|event| (section, event))
                });
                match taken {
                    Ok((section, event)) => to_player(
                        &players,
                        out,
                        id,
                        player,
                        ToPlayer::Play { id, section, event },
                    ),
                    Err(code) => refuse_handle(out, id, player, code),
                }
            }
            Command::Seek { player, run, at } => {
                to_player(&players, out, id, player, ToPlayer::Seek { id, run, at });
            }
            Command::Close { player } => match players.remove(&player) {
                Some((tx, thread)) => {
                    let _ = tx.send(ToPlayer::Close { id });
                    closing.push(thread);
                }
                None => reject(out, id, player, 0),
            },
            Command::Quit => {
                quit = id;
                break;
            }
        }
    }
    for (_, (tx, thread)) in std::mem::take(&mut players) {
        let _ = tx.send(ToPlayer::Close { id: quit });
        closing.push(thread);
    }
    for thread in closing {
        let _ = thread.join();
    }
    ExitCode::SUCCESS
}

/// Sends `message`, command `id`'s, to `player`'s thread, or rejects the
/// command when no such player takes it, the handles it carries closed.
fn to_player(
    players: &BTreeMap<u8, (Sender<ToPlayer>, JoinHandle<()>)>,
    out: &Out,
    id: u32,
    player: u8,
    message: ToPlayer,
) {
    let sent = players
        .get(&player)
        .is_some_and(|(tx, _)| tx.send(message).is_ok());
    if !sent {
        reject(out, id, player, 0);
    }
}

/// Rejects command `id` for `player`, with the system's error `code` (0
/// when there is none).
pub fn reject(out: &Out, id: u32, player: u8, code: i32) {
    out.send(&Reply::Rejected { id, player, code });
}

/// Rejects command `id`, which names a handle the worker does not take
/// ([`os`]) for `player`, with the system's error `code`.
fn refuse_handle(out: &Out, id: u32, player: u8, code: u32) {
    say(&format!(
        "tapestry-media: a command for player {player} names a handle the worker does not take (error {code})"
    ));
    reject(out, id, player, code.cast_signed());
}

/// The next command on `input` and its id, or `None` once the pipe ends or
/// carries a frame the protocol does not have (the game's commands are
/// trusted; one that does not decode ends the worker).
fn read_command(input: &mut impl std::io::Read) -> Option<(u32, Command)> {
    let mut prefix = [0; 2];
    input.read_exact(&mut prefix).ok()?;
    let len = control::frame_len(prefix).ok()?;
    let mut message = vec![0; len];
    input.read_exact(&mut message).ok()?;
    match Command::decode(&message) {
        Ok(c) => Some(c),
        Err(e) => {
            say(&format!(
                "tapestry-media: a command that does not decode: {e}"
            ));
            None
        }
    }
}
