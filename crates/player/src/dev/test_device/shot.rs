//! Shots of the player's window (docs/test-device.md "Run Guard"): the
//! player's own copy of a frame it presented, asked for on a named pipe the
//! runner creates before it starts the player (`--dev-shot-pipe`,
//! [`crate::dev::present_shot`]). The runner's end of the pipe lives on a
//! thread of its own, so a player that never answers costs the step thread
//! only its deadline ([`SHOT_WAIT`]), and a writer thread of its own
//! encodes each shot's PNG, writes it as a new file of the run's folder,
//! closes it and only then seals it, so no step waits on a file write.

use std::io::{BufReader, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use super::owner;
use super::win::wide;
use crate::dev::present_shot::{self, Shot};

/// How long a shot may take from its request to its answer: the player's
/// own wait for a frame ([`present_shot::FRAME_WAIT`]) and the transfer of
/// its pixels.
pub const SHOT_WAIT: Duration = Duration::from_secs(5);

/// The pipe's name for run `run`.
#[must_use]
pub fn pipe_name(run: &str) -> String {
    format!(r"\\.\pipe\tapestry-shot-{run}")
}

/// The answer to a request, with the process at the pipe's other end.
#[derive(Debug)]
pub struct Answer {
    pub serial: u64,
    /// The process that connected, as the system names it.
    pub client: u32,
    pub shot: Result<Shot, String>,
}

/// The runner's end of the shot pipe: one instance, local clients only.
pub struct Pipe {
    pub name: String,
    to_io: Option<Sender<u64>>,
    from_io: Receiver<Result<Answer, String>>,
    serial: u64,
}

impl Pipe {
    /// Creates the pipe `name` and its thread, which waits for the player to
    /// connect, then answers each request with what the player sends.
    ///
    /// # Errors
    ///
    /// The pipe cannot be created (another instance of the name exists).
    pub fn create(name: String) -> anyhow::Result<Self> {
        let w = wide(&name);
        // SAFETY: a nul-terminated name; no security attributes.
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(w.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1 << 16,
                1 << 16,
                0,
                None,
            )
        };
        if h.is_invalid() {
            anyhow::bail!(
                "CreateNamedPipe({name}): {}",
                windows::core::Error::from_thread()
            );
        }
        // SAFETY: the handle was just created and is owned by the file.
        let file = unsafe { std::fs::File::from_raw_handle(h.0) };
        let (to_io, requests) = mpsc::channel::<u64>();
        let (answers, from_io) = mpsc::channel();
        std::thread::Builder::new()
            .name("shot pipe".into())
            .spawn(move || serve(file, &requests, &answers))?;
        Ok(Self {
            name,
            to_io: Some(to_io),
            from_io,
            serial: 0,
        })
    }

    /// Asks for a shot: its serial.
    ///
    /// # Errors
    ///
    /// The pipe's thread has ended.
    pub fn request(&mut self) -> Result<u64, String> {
        self.serial += 1;
        self.to_io
            .as_ref()
            .ok_or("the shot pipe is closed")?
            .send(self.serial)
            .map_err(|_| "the shot pipe's thread has ended".to_owned())?;
        Ok(self.serial)
    }

    /// An answer, if one comes within `wait`.
    ///
    /// # Errors
    ///
    /// The pipe failed: its thread's error.
    pub fn answer(&self, wait: Duration) -> Result<Option<Answer>, String> {
        match self.from_io.recv_timeout(wait) {
            Ok(a) => a.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("the shot pipe's thread has ended".into())
            }
        }
    }
}

/// The frame of `answer` to request `serial`, made by process `player` at
/// the rows' client size: whole, and acquired after the request was sent
/// at the counter `requested`, then presented.
///
/// # Errors
///
/// Why the answer is not that frame: another request's, another process's,
/// the player's error, another size, short pixels, a present that did not
/// advance the swap chain's count, or a frame acquired before the request
/// or presented before its acquisition.
pub fn check_answer(
    answer: Answer,
    serial: u64,
    player: u32,
    requested: i64,
) -> Result<Shot, String> {
    if answer.serial != serial {
        return Err(format!(
            "the answer is to request {}, not {serial}",
            answer.serial
        ));
    }
    if answer.client != player {
        return Err(format!(
            "the pipe's client is process {}, not the player {player}",
            answer.client
        ));
    }
    let shot = answer.shot.map_err(|e| format!("the player: {e}"))?;
    let [w, h] = test_device::panel::CLIENT.map(|v| u32::try_from(v).unwrap_or(0));
    if (shot.width, shot.height) != (w, h) {
        return Err(format!(
            "a {}x{} frame, not {w}x{h}",
            shot.width, shot.height
        ));
    }
    if shot.rgba.len() != w as usize * h as usize * 4 {
        return Err(format!("{} bytes of pixels", shot.rgba.len()));
    }
    if shot.present <= shot.previous {
        return Err(format!(
            "a frame whose present did not advance the swap chain's count ({} to {})",
            shot.previous, shot.present
        ));
    }
    if shot.acquired <= requested {
        return Err(format!(
            "a frame acquired at {}, not after the request at {requested}",
            shot.acquired
        ));
    }
    if shot.presented < shot.acquired {
        return Err(format!(
            "a frame presented at {}, before its acquisition at {}",
            shot.presented, shot.acquired
        ));
    }
    Ok(shot)
}

/// The pipe's thread: waits for the client, then for each request writes it
/// and reads the answer. A client that never connects or never answers
/// leaves this thread blocked; the run's end closes the player, which
/// breaks the pipe.
fn serve(file: std::fs::File, requests: &Receiver<u64>, answers: &Sender<Result<Answer, String>>) {
    let h = HANDLE(file.as_raw_handle());
    // SAFETY: the pipe's own handle, waited on synchronously.
    if let Err(e) = unsafe { ConnectNamedPipe(h, None) }
        && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
    {
        let _ = answers.send(Err(format!("ConnectNamedPipe: {e}")));
        return;
    }
    let mut client = 0;
    // SAFETY: one u32 written.
    if let Err(e) = unsafe { GetNamedPipeClientProcessId(h, &mut client) } {
        let _ = answers.send(Err(format!("GetNamedPipeClientProcessId: {e}")));
        return;
    }
    let Ok(read) = file.try_clone() else {
        let _ = answers.send(Err("the pipe's handle".into()));
        return;
    };
    let mut read = BufReader::new(read);
    let mut write = file;
    for serial in requests {
        let sent = write
            .write_all(present_shot::request_line(serial).as_bytes())
            .and_then(|()| write.flush())
            .map_err(|e| format!("writing request {serial}: {e}"));
        let got = sent.and_then(|()| present_shot::read_answer(&mut read));
        let failed = got.is_err();
        let _ = answers.send(got.map(|(serial, shot)| Answer {
            serial,
            client,
            shot,
        }));
        if failed {
            return;
        }
    }
}

/// One shot handed to the writer: its file and its pixels (RGBA rows from
/// the top).
struct Job {
    path: PathBuf,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// The writer thread.
pub struct Shots {
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<Vec<String>>>,
}

/// How long the writer may take to finish once the run ends.
const FINISH_WAIT: Duration = Duration::from_secs(30);

/// The PNG of RGBA pixels, opaque: a swap chain's alpha is not what the
/// screen shows.
#[must_use]
pub fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let opaque: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, _]| [r, g, b, 255])
        .collect();
    crate::dev::png(width, height, &opaque)
}

impl Shots {
    /// Starts the writer for the run whose folder is `dir`: each written
    /// shot's seal (`run`, `role`) goes to `record`.
    pub fn start(
        dir: PathBuf,
        run: String,
        role: String,
        record: Box<dyn Fn(Value) + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let thread = std::thread::spawn(move || {
            let mut errors = Vec::new();
            for j in rx {
                let written = owner::write_new(&j.path, &png(j.width, j.height, &j.rgba))
                    .and_then(|()| owner::seal(&dir, &j.path, &run, &role));
                match written {
                    Ok(seal) => record(seal),
                    Err(e) => errors.push(format!("the shot {}: {e:#}", j.path.display())),
                }
            }
            errors
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    /// Hands a shot to the writer.
    ///
    /// # Errors
    ///
    /// The writer has ended.
    pub fn write(
        &self,
        path: PathBuf,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or("the shot writer has ended")?
            .send(Job {
                path,
                width,
                height,
                rgba,
            })
            .map_err(|_| "the shot writer has ended".to_owned())
    }

    /// Waits for every shot handed over to be written and sealed: the
    /// writer's errors.
    pub fn finish(&mut self) -> Vec<String> {
        drop(self.tx.take());
        let Some(t) = self.thread.take() else {
            return Vec::new();
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(t.join());
        });
        match rx.recv_timeout(FINISH_WAIT) {
            Ok(Ok(errors)) => errors,
            Ok(Err(_)) => vec!["the shot writer panicked".into()],
            Err(_) => vec![format!(
                "the shot writer did not finish within {FINISH_WAIT:?}"
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead as _;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    /// A shot is written as a PNG of its pixels, opaque, and sealed only
    /// once its file is closed: the seal's digest is the file's.
    #[test]
    fn a_shot_is_written_then_sealed() {
        let tmp = tempfile::tempdir().unwrap();
        let seals = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seals);
        let mut shots = Shots::start(
            tmp.path().to_owned(),
            "r".into(),
            "run".into(),
            Box::new(move |v| s2.lock().unwrap().push(v)),
        );
        // 2×1: red, then blue (RGBA), alpha 0.
        let rgba = vec![255, 0, 0, 0, 0, 0, 255, 0];
        let path = tmp.path().join("shot-a.png");
        shots.write(path.clone(), 2, 1, rgba).unwrap();
        assert!(shots.finish().is_empty());
        let img = image::open(&path).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 255]);
        let seals = seals.lock().unwrap();
        assert_eq!(seals.len(), 1);
        assert_eq!(seals[0]["path"], "shot-a.png");
        assert_eq!(seals[0]["sha256"], owner::digest(&path).unwrap());
        // A second shot of the same name is refused, not overwritten.
        let mut again = Shots::start(
            tmp.path().to_owned(),
            "r".into(),
            "run".into(),
            Box::new(|_| {}),
        );
        again.write(path, 1, 1, vec![0; 4]).unwrap();
        assert_eq!(again.finish().len(), 1);
    }

    fn unique(what: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        pipe_name(&format!("test-{what}-{}-{nanos}", std::process::id()))
    }

    /// A client of the pipe that answers as the player does: the runner reads
    /// its answer, with the client's process (this one), and a second
    /// instance of the same name is refused.
    #[test]
    fn the_pipe_carries_a_request_and_its_answer() {
        let name = unique("answer");
        let mut pipe = Pipe::create(name.clone()).unwrap();
        assert!(Pipe::create(name.clone()).is_err(), "one instance");
        let client = std::thread::spawn(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
                .unwrap();
            let mut r = BufReader::new(f.try_clone().unwrap());
            let mut w = f;
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let serial = present_shot::parse_request(&line).unwrap();
            let shot = Shot {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
                previous: 6,
                present: 7,
                acquired: 10,
                presented: 20,
                volumes: Vec::new(),
            };
            present_shot::write_answer(&mut w, serial, &Ok(shot)).unwrap();
        });
        let serial = pipe.request().unwrap();
        let a = pipe.answer(SHOT_WAIT).unwrap().expect("an answer");
        client.join().unwrap();
        assert_eq!(a.serial, serial);
        assert_eq!(a.client, std::process::id());
        assert_eq!(a.shot.unwrap().rgba, vec![1, 2, 3, 4]);
    }

    /// An answer is the requested frame only when it is to that request,
    /// from the player's process, of the rows' size and whole, acquired
    /// after the request and presented after that; each departure is
    /// refused, naming it.
    #[test]
    fn an_answer_is_the_frame_only_when_it_is_bound_to_the_request() {
        let [w, h] = test_device::panel::CLIENT.map(|v| u32::try_from(v).unwrap());
        let good = || Answer {
            serial: 3,
            client: 40,
            shot: Ok(Shot {
                width: w,
                height: h,
                rgba: vec![0; w as usize * h as usize * 4],
                previous: 4,
                present: 5,
                acquired: 1_000,
                presented: 1_200,
                volumes: Vec::new(),
            }),
        };
        assert!(check_answer(good(), 3, 40, 900).is_ok());
        let edit = |f: fn(&mut Answer)| {
            let mut a = good();
            f(&mut a);
            a
        };
        let cases: [(&str, Answer); 8] = [
            ("request 4", edit(|a| a.serial = 4)),
            ("process 41", edit(|a| a.client = 41)),
            ("the player: late", edit(|a| a.shot = Err("late".into()))),
            (
                "a 1919x1080",
                edit(|a| a.shot.as_mut().unwrap().width = 1919),
            ),
            (
                "bytes of pixels",
                edit(|a| {
                    a.shot.as_mut().unwrap().rgba.pop();
                }),
            ),
            (
                "did not advance",
                edit(|a| a.shot.as_mut().unwrap().previous = 5),
            ),
            (
                "not after the request",
                edit(|a| a.shot.as_mut().unwrap().acquired = 900),
            ),
            (
                "before its acquisition",
                edit(|a| a.shot.as_mut().unwrap().presented = 999),
            ),
        ];
        for (why, a) in cases {
            let e = check_answer(a, 3, 40, 900).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        }
    }

    /// A client that connects and never answers (a stalled player) leaves
    /// no answer: the runner's wait for it ends at its deadline, not later.
    #[test]
    fn a_player_that_never_answers_leaves_no_answer() {
        let name = unique("stall");
        let mut pipe = Pipe::create(name.clone()).unwrap();
        let (hold, held) = mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
                .unwrap();
            // Reads nothing, answers nothing, until the test is done.
            let _ = held.recv();
            drop(f);
        });
        pipe.request().unwrap();
        let t = Instant::now();
        assert!(pipe.answer(Duration::from_millis(200)).unwrap().is_none());
        let waited = t.elapsed();
        assert!(
            waited >= Duration::from_millis(200) && waited < Duration::from_secs(2),
            "{waited:?}"
        );
        drop(hold);
        client.join().unwrap();
    }
}
