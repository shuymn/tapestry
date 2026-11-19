//! The media worker (design.md "Threat Model" T02; "Libraries", the FFmpeg
//! row): the one binary that links FFmpeg. It decodes the game's videos
//! into their frame rings (`skin_model::media`) as the game commands it on
//! its control pipe, its standard input and output
//! (`skin_model::media::control`): it answers with its FFmpeg's version
//! and configure line, then probes each file the game hands it by handle,
//! refusing what is not a listed container and codec within the pixel cap,
//! and decodes a probed file into the ring the game created for it, from
//! its start or from a seek, until the game closes it. Each video plays on
//! a thread of its own; FFmpeg's diagnostics go to standard error, a line
//! each.
//!
//! [`ffmpeg`] holds every call into FFmpeg; [`os`] the Windows calls on the
//! handles the game hands the worker.
//!
//! With dev-tools: `--dev-tools-probe` says the build has the development
//! features, `--dev-gen-clip` encodes a synthesized clip through the
//! build's encoders and muxers, and `--dev-secondary-open` reports what a
//! secondary open through an opened file's I/O context gives.

#[cfg(not(windows))]
compile_error!("tapestry-media is Windows-only");

#[cfg(feature = "dev-tools")]
mod dev;
mod ffmpeg;
mod os;
mod player;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::ExitCode;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use skin_model::media::control::{self, Command, Failure, Reply};

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

impl Out {
    /// Writes `reply` as one frame (every reply's encoding fits one). A
    /// pipe the game no longer reads ends the worker: it has nobody to
    /// decode for.
    pub fn send(&self, reply: &Reply) {
        let Some(bytes) = control::frame(&reply.encode()) else {
            eprintln!("tapestry-media: a reply that does not fit a frame");
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

/// What the control loop tells a player's thread.
pub enum ToPlayer {
    Play { section: u64, event: u64 },
    Seek { run: u64, at: i64 },
    Close,
}

/// Reads commands until the game quits or closes the pipe, FFmpeg's largest
/// allocation capped at `max_alloc` bytes.
fn serve(max_alloc: usize) -> ExitCode {
    if let Err(e) = ffmpeg::init(max_alloc) {
        eprintln!("tapestry-media: FFmpeg's hooks: {e}");
        return ExitCode::FAILURE;
    }
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    let (version, configuration) = ffmpeg::version();
    out.send(&Reply::Hello {
        version,
        configuration,
    });
    let mut stdin = std::io::stdin().lock();
    let mut players: BTreeMap<u8, (Sender<ToPlayer>, JoinHandle<()>)> = BTreeMap::new();
    let mut closing: Vec<JoinHandle<()>> = Vec::new();
    while let Some(command) = read_command(&mut stdin) {
        closing.retain(|t| !t.is_finished());
        match command {
            Command::Open { player, file } => {
                if players.contains_key(&player) || players.len() >= MAX_PLAYERS {
                    drop(os::Owned::adopt(file));
                    out.send(&Reply::Failed {
                        player,
                        why: Failure::Command,
                        code: 0,
                    });
                    continue;
                }
                let (tx, rx) = mpsc::channel();
                let thread_out = out.clone();
                // The thread owns the handle: one that does not start closes
                // it with its closure.
                let file = os::Owned::adopt(file);
                let spawned = std::thread::Builder::new()
                    .name(format!("player {player}"))
                    .spawn(move || player::run(player, file, &rx, &thread_out));
                match spawned {
                    Ok(thread) => {
                        players.insert(player, (tx, thread));
                    }
                    Err(_) => out.send(&Reply::Failed {
                        player,
                        why: Failure::Memory,
                        code: 0,
                    }),
                }
            }
            Command::Play {
                player,
                section,
                event,
            } => to_player(
                &players,
                &out,
                player,
                ToPlayer::Play { section, event },
                || {
                    drop(os::Owned::adopt(section));
                    drop(os::Owned::adopt(event));
                },
            ),
            Command::Seek { player, run, at } => {
                to_player(&players, &out, player, ToPlayer::Seek { run, at }, || {});
            }
            Command::Close { player } => match players.remove(&player) {
                Some((tx, thread)) => {
                    let _ = tx.send(ToPlayer::Close);
                    closing.push(thread);
                }
                None => out.send(&Reply::Failed {
                    player,
                    why: Failure::Command,
                    code: 0,
                }),
            },
            Command::Quit => break,
        }
    }
    for (_, (tx, thread)) in std::mem::take(&mut players) {
        let _ = tx.send(ToPlayer::Close);
        closing.push(thread);
    }
    for thread in closing {
        let _ = thread.join();
    }
    ExitCode::SUCCESS
}

/// Sends `message` to `player`'s thread, or answers that no such player
/// takes it, after `refused` closes the handles the command named.
fn to_player(
    players: &BTreeMap<u8, (Sender<ToPlayer>, JoinHandle<()>)>,
    out: &Out,
    player: u8,
    message: ToPlayer,
    refused: impl FnOnce(),
) {
    let sent = players
        .get(&player)
        .is_some_and(|(tx, _)| tx.send(message).is_ok());
    if !sent {
        refused();
        out.send(&Reply::Failed {
            player,
            why: Failure::Command,
            code: 0,
        });
    }
}

/// The next command on `input`, or `None` once the pipe ends or carries a
/// frame the protocol does not have (the game's commands are trusted; one
/// that does not decode ends the worker).
fn read_command(input: &mut impl std::io::Read) -> Option<Command> {
    let mut prefix = [0; 2];
    input.read_exact(&mut prefix).ok()?;
    let len = control::frame_len(prefix).ok()?;
    let mut message = vec![0; len];
    input.read_exact(&mut message).ok()?;
    match Command::decode(&message) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("tapestry-media: a command that does not decode: {e}");
            None
        }
    }
}
