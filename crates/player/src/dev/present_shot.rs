//! The player's own shots of its window (`--dev-shot-pipe <name>`,
//! docs/test-device.md "Run Guard", shots): the engine's screenshot. A
//! runner asks over a named pipe it created; the render thread copies the
//! frame it is about to present ([`render::PresentTap`]) and this side
//! answers with that frame's pixels, its present count and the host
//! counter's readings at its acquisition and its present, the clock the
//! test device's recorder stamps its reports with.
//!
//! A request is answered with a frame whose work began after the request
//! was seen: the first frame acquired after it is passed over (its content
//! may have been built before), and the next is copied. Only the selection
//! screen's frames are copied: a request any other frame meets (play's,
//! whose processing allocates nothing, design.md "Scope of No
//! Allocation") is refused there without a copy, and the refusal is
//! answered and logged from the pipe's thread. A copy whose present did not
//! advance the swap chain's present count is not a shot.
//!
//! Each answer also carries the volume arm's witness: every volume the
//! select screen's pointer dispatch reached (the slider's number, its
//! percent, and the host counter when it was dispatched), so a check can
//! tell a press that reached the arm from one lost before it.

use std::io::{BufReader, Read as _};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::test_device::win::qpc;

/// How long the player waits for its render thread to copy a requested
/// frame before it answers with an error: well inside the runner's own
/// deadline ([`super::test_device::shot::SHOT_WAIT`]).
pub const FRAME_WAIT: Duration = Duration::from_secs(3);
/// How long the player tries to connect to the runner's pipe.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// A copied frame, as the player answers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The swap chain's present count right before its present.
    pub previous: u32,
    /// The swap chain's present count right after its present: above
    /// `previous`.
    pub present: u32,
    /// The host counter when the render thread took the frame for the copy
    /// (its passes encoded, before its submission).
    pub acquired: i64,
    /// The host counter right after its present returned.
    pub presented: i64,
    /// Every volume the pointer's volume arm reached in the run so far, in
    /// order, read when the answer is written.
    pub volumes: Vec<VolumeSet>,
}

/// A volume the select screen's pointer volume arm reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeSet {
    /// The skin's slider number of its channel: 17 system, 18 key, 19
    /// background.
    pub slider: u8,
    /// The whole percent it set.
    pub percent: u32,
    /// The host counter when the arm took it.
    pub at: i64,
    /// The pointer input that brought it to the arm.
    pub origin: VolumeOrigin,
}

/// Which pointer input brought a volume to the arm: a press answered by
/// the render thread, a drag of the held press, or another call of the arm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VolumeOrigin {
    Press,
    Drag,
    #[default]
    Other,
}

impl VolumeOrigin {
    /// Its name in an answer and a `shot` record.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Drag => "drag",
            Self::Other => "other",
        }
    }

    const fn code(self) -> u32 {
        match self {
            Self::Press => 1,
            Self::Drag => 2,
            Self::Other => 0,
        }
    }

    const fn of_code(code: u32) -> Self {
        match code {
            1 => Self::Press,
            2 => Self::Drag,
            _ => Self::Other,
        }
    }

    fn of_name(name: &str) -> Option<Self> {
        [Self::Press, Self::Drag, Self::Other]
            .into_iter()
            .find(|o| o.name() == name)
    }
}

/// The most volume settings the witness keeps: far more than a panel row's
/// three clicks and their drags make; past it, answers are errors.
const VOLUME_LOG: usize = 64;
// The witness, written by the one thread that dispatches the select
// screen's pointer acts and read by the pipe's thread: each entry is
// written before the count that publishes it.
static VOLUME_AT: [AtomicI64; VOLUME_LOG] = [const { AtomicI64::new(0) }; VOLUME_LOG];
static VOLUME_WHAT: [AtomicU32; VOLUME_LOG] = [const { AtomicU32::new(0) }; VOLUME_LOG];
static VOLUME_N: AtomicUsize = AtomicUsize::new(0);

/// The select screen's pointer volume arm took `percent` for `channel`,
/// brought by `origin` (dev-tools): kept for the shots' answers. Allocates
/// nothing.
pub fn volume_reached(channel: game::Channel, percent: u32, origin: VolumeOrigin) {
    let slider: u32 = match channel {
        game::Channel::System => 17,
        game::Channel::Key => 18,
        game::Channel::Background => 19,
    };
    let i = VOLUME_N.load(Ordering::Relaxed);
    if let (Some(at), Some(what)) = (VOLUME_AT.get(i), VOLUME_WHAT.get(i)) {
        at.store(qpc(), Ordering::Relaxed);
        what.store(
            origin.code() << 16 | slider << 8 | percent.min(0xff),
            Ordering::Relaxed,
        );
    }
    VOLUME_N.store(i.saturating_add(1), Ordering::Release);
}

/// The witness so far.
///
/// # Errors
///
/// More settings than it keeps.
pub(crate) fn volumes_reached() -> Result<Vec<VolumeSet>, String> {
    let n = VOLUME_N.load(Ordering::Acquire);
    if n > VOLUME_LOG {
        return Err(format!(
            "the volume arm was reached {n} times, more than the {VOLUME_LOG} its witness keeps"
        ));
    }
    Ok(VOLUME_AT
        .iter()
        .zip(&VOLUME_WHAT)
        .take(n)
        .map(|(at, what)| {
            let what = what.load(Ordering::Relaxed);
            VolumeSet {
                slider: u8::try_from(what >> 8 & 0xff).unwrap_or(0),
                percent: what & 0xff,
                at: at.load(Ordering::Relaxed),
                origin: VolumeOrigin::of_code(what >> 16),
            }
        })
        .collect())
}

/// Where a request is.
#[derive(Debug)]
enum Phase {
    Idle,
    /// Asked: the next frame acquired is passed over.
    Asked,
    /// One frame passed over: the next is copied.
    Primed,
    /// A frame is being copied: acquired at this counter, presented at the
    /// second once known, with the chain's present counts before and after.
    Copying(i64, Option<(i64, Option<u32>, Option<u32>)>),
    /// A frame other than the selection screen's met the request: refused
    /// on the render thread, without a copy or an allocation.
    Refused,
    Done(Result<Shot, String>),
}

struct State {
    serial: u64,
    phase: Phase,
}

/// Why a request met by a frame other than the selection screen's is not
/// answered with a frame.
pub const REFUSED: &str =
    "refused: the frame drawn was not the selection screen's (no copy is made outside it)";

/// The render thread's tap and the pipe's side of it.
pub struct Tap {
    state: Mutex<State>,
    done: Condvar,
    clock: fn() -> i64,
}

impl Tap {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_clock(qpc)
    }

    fn with_clock(clock: fn() -> i64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                serial: 0,
                phase: Phase::Idle,
            }),
            done: Condvar::new(),
            clock,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks for the next fresh frame: its serial. An earlier request not
    /// yet answered is dropped.
    pub fn request(&self) -> u64 {
        let mut s = self.lock();
        s.serial += 1;
        s.phase = Phase::Asked;
        s.serial
    }

    /// Waits up to `wait` for request `serial`'s frame.
    ///
    /// # Errors
    ///
    /// No frame within `wait`, a copy that failed, or a newer request.
    pub fn wait(&self, serial: u64, wait: Duration) -> Result<Shot, String> {
        let end = Instant::now() + wait;
        let mut s = self.lock();
        loop {
            if s.serial != serial {
                return Err(format!("request {serial} was replaced by {}", s.serial));
            }
            if let Phase::Refused = s.phase {
                s.phase = Phase::Idle;
                return Err(REFUSED.to_owned());
            }
            if let Phase::Done(_) = &s.phase {
                let Phase::Done(r) = std::mem::replace(&mut s.phase, Phase::Idle) else {
                    unreachable!("matched above");
                };
                return r;
            }
            let now = Instant::now();
            if now >= end {
                s.phase = Phase::Idle;
                return Err(format!("no frame was presented within {wait:?}"));
            }
            s = self
                .done
                .wait_timeout(s, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl render::PresentTap for Tap {
    fn wanted(&self, selection: bool) -> bool {
        let mut s = self.lock();
        match s.phase {
            Phase::Asked | Phase::Primed | Phase::Copying(..) if !selection => {
                s.phase = Phase::Refused;
                drop(s);
                self.done.notify_all();
                false
            }
            Phase::Asked => {
                s.phase = Phase::Primed;
                false
            }
            // A copy that never came back (its frame failed before its
            // present) is taken again from this frame.
            Phase::Primed | Phase::Copying(..) => {
                s.phase = Phase::Copying((self.clock)(), None);
                true
            }
            Phase::Idle | Phase::Refused | Phase::Done(_) => false,
        }
    }

    fn presented(&self, before: Option<u32>, after: Option<u32>) {
        let now = (self.clock)();
        let mut s = self.lock();
        if let Phase::Copying(at, None) = s.phase {
            s.phase = Phase::Copying(at, Some((now, before, after)));
        }
    }

    fn deliver(&self, frame: Result<render::PresentFrame, String>) {
        let mut s = self.lock();
        let Phase::Copying(acquired, presented) = s.phase else {
            return;
        };
        s.phase = Phase::Done(match (frame, presented) {
            (Ok(f), Some((presented, Some(previous), Some(present)))) if present > previous => {
                Ok(Shot {
                    width: f.width,
                    height: f.height,
                    rgba: f.rgba,
                    previous,
                    present,
                    acquired,
                    presented,
                    volumes: Vec::new(),
                })
            }
            (Ok(_), Some((_, before, after))) => Err(format!(
                "the copied frame's present did not advance the swap chain's present count \
                 (before {before:?}, after {after:?})"
            )),
            (Ok(_), None) => Err("the copied frame was not presented".into()),
            (Err(e), _) => Err(e),
        });
        drop(s);
        self.done.notify_all();
    }
}

/// The request line for shot `serial`.
#[must_use]
pub fn request_line(serial: u64) -> String {
    format!("shot {serial}\n")
}

/// The serial of a request line.
#[must_use]
pub fn parse_request(line: &str) -> Option<u64> {
    line.trim_end().strip_prefix("shot ")?.parse().ok()
}

/// Writes the answer to request `serial`: a header line, then a shot's
/// pixels.
///
/// # Errors
///
/// The write failed.
pub fn write_answer(
    out: &mut dyn std::io::Write,
    serial: u64,
    answer: &Result<Shot, String>,
) -> std::io::Result<()> {
    let header = match answer {
        Ok(s) => json!({
            "serial": serial, "width": s.width, "height": s.height,
            "previous": s.previous, "present": s.present,
            "acquired": s.acquired, "presented": s.presented,
            "volumes": volumes_json(&s.volumes),
            "bytes": s.rgba.len(),
        }),
        Err(e) => json!({ "serial": serial, "error": e }),
    };
    out.write_all(format!("{header}\n").as_bytes())?;
    if let Ok(s) = answer {
        out.write_all(&s.rgba)?;
    }
    out.flush()
}

/// The witness as an answer and a `shot` record carry it: one array per
/// volume, `[slider, percent, counter, origin]`.
#[must_use]
pub fn volumes_json(volumes: &[VolumeSet]) -> Value {
    Value::Array(
        volumes
            .iter()
            .map(|v| json!([v.slider, v.percent, v.at, v.origin.name()]))
            .collect(),
    )
}

/// The longest answer a runner reads: a frame of 8192×8192.
const MAX_BYTES: u64 = 8192 * 8192 * 4;

/// Reads an answer: its serial and the shot or the player's error.
///
/// # Errors
///
/// A header that does not read, or fewer bytes than it announces.
pub fn read_answer(
    input: &mut dyn std::io::BufRead,
) -> Result<(u64, Result<Shot, String>), String> {
    let mut line = String::new();
    let n = input
        .read_line(&mut line)
        .map_err(|e| format!("reading the answer: {e}"))?;
    if n == 0 {
        return Err("the player closed its pipe".into());
    }
    let h: Value =
        serde_json::from_str(line.trim_end()).map_err(|e| format!("the answer's header: {e}"))?;
    let serial = h["serial"].as_u64().ok_or("the answer names no serial")?;
    if let Some(e) = h["error"].as_str() {
        return Ok((serial, Err(e.to_owned())));
    }
    let num = |k: &str| {
        h[k].as_u64()
            .ok_or_else(|| format!("the answer has no `{k}`"))
    };
    let bytes = num("bytes")?;
    if bytes > MAX_BYTES {
        return Err(format!("the answer announces {bytes} bytes"));
    }
    let mut rgba = Vec::new();
    input
        .take(bytes)
        .read_to_end(&mut rgba)
        .map_err(|e| format!("reading the answer's pixels: {e}"))?;
    if rgba.len() as u64 != bytes {
        return Err(format!("the answer holds {} of {bytes} bytes", rgba.len()));
    }
    let int = |k: &str| {
        h[k].as_i64()
            .ok_or_else(|| format!("the answer has no `{k}`"))
    };
    let volumes = h["volumes"]
        .as_array()
        .ok_or("the answer has no `volumes`")?
        .iter()
        .map(|v| {
            let slider = v[0].as_u64().and_then(|n| u8::try_from(n).ok());
            let percent = v[1].as_u64().and_then(|n| u32::try_from(n).ok());
            let origin = v[3].as_str().and_then(VolumeOrigin::of_name);
            match (
                slider,
                percent,
                v[2].as_i64(),
                origin,
                v.as_array().map(Vec::len),
            ) {
                (Some(slider), Some(percent), Some(at), Some(origin), Some(4)) => Ok(VolumeSet {
                    slider,
                    percent,
                    at,
                    origin,
                }),
                _ => Err(format!("the answer's volume {v} does not read")),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        serial,
        Ok(Shot {
            width: u32::try_from(num("width")?).map_err(|e| e.to_string())?,
            height: u32::try_from(num("height")?).map_err(|e| e.to_string())?,
            rgba,
            previous: u32::try_from(num("previous")?).map_err(|e| e.to_string())?,
            present: u32::try_from(num("present")?).map_err(|e| e.to_string())?,
            acquired: int("acquired")?,
            presented: int("presented")?,
            volumes,
        }),
    ))
}

/// Answers each request read from `input` on `output` with `tap`'s next
/// fresh frame, until `input` ends.
///
/// # Errors
///
/// A request that does not read, or a failed write.
pub fn serve(
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
    tap: &Tap,
    wait: Duration,
) -> Result<(), String> {
    loop {
        let mut line = String::new();
        let n = input
            .read_line(&mut line)
            .map_err(|e| format!("reading a request: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        let serial = parse_request(&line).ok_or_else(|| format!("a bad request {line:?}"))?;
        let mine = tap.request();
        let answer = tap.wait(mine, wait).and_then(|mut s| {
            s.volumes = volumes_reached()?;
            Ok(s)
        });
        if let Err(e) = &answer {
            eprintln!("dev: shot {serial}: {e}");
        }
        write_answer(output, serial, &answer).map_err(|e| format!("answering: {e}"))?;
    }
}

/// Connects to the runner's pipe `name` and answers its requests on a thread
/// of its own; what goes wrong is said on standard error.
pub fn start(name: String, tap: Arc<Tap>) {
    let spawned = std::thread::Builder::new()
        .name("dev shots".into())
        .spawn(move || {
            let r = connect(&name).and_then(|pipe| {
                let read = pipe
                    .try_clone()
                    .map_err(|e| format!("the pipe's handle: {e}"))?;
                let mut write = pipe;
                serve(&mut BufReader::new(read), &mut write, &tap, FRAME_WAIT)
            });
            if let Err(e) = r {
                eprintln!("dev: the shot pipe {name}: {e}");
            }
        });
    if let Err(e) = spawned {
        eprintln!("dev: the shot pipe's thread: {e}");
    }
}

/// Opens the runner's pipe `name`, waiting up to [`CONNECT_WAIT`] for it.
fn connect(name: &str) -> Result<std::fs::File, String> {
    let end = Instant::now() + CONNECT_WAIT;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(f) => return Ok(f),
            Err(e) if Instant::now() >= end => {
                return Err(format!("not connected within {CONNECT_WAIT:?}: {e}"));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use render::PresentTap as _;

    use super::*;

    static NOW: AtomicI64 = AtomicI64::new(0);

    fn now() -> i64 {
        NOW.fetch_add(10, Ordering::SeqCst)
    }

    fn frame(v: u8) -> render::PresentFrame {
        render::PresentFrame {
            width: 2,
            height: 1,
            rgba: vec![v; 8],
        }
    }

    /// A request is answered with the second frame acquired after it, not
    /// the first (whose content may predate it), with that frame's counter
    /// readings and present counts; a frame acquired with no request is not
    /// copied.
    #[test]
    fn a_request_takes_the_second_frame_after_it() {
        let tap = Tap::with_clock(now);
        assert!(!tap.wanted(true), "no request");
        let serial = tap.request();
        assert!(!tap.wanted(true), "the first frame after the request");
        assert!(tap.wanted(true), "the second");
        tap.presented(Some(40), Some(41));
        tap.deliver(Ok(frame(7)));
        let shot = tap.wait(serial, Duration::from_secs(1)).unwrap();
        assert_eq!(shot.rgba, vec![7; 8]);
        assert_eq!((shot.previous, shot.present), (40, 41));
        assert!(shot.presented > shot.acquired, "{shot:?}");
        assert!(!tap.wanted(true), "answered");
    }

    /// A request that a frame other than the selection screen's meets (a
    /// play frame), before its passed-over frame or at the copy, is refused
    /// there: no copy is asked for, and the wait answers the refusal at
    /// once, not at its deadline. With no request, such a frame is not
    /// refused anything.
    #[test]
    fn a_request_during_play_is_refused_without_a_copy() {
        let tap = Tap::with_clock(now);
        assert!(!tap.wanted(false), "no request: nothing to refuse");
        for passed in [false, true] {
            let s = tap.request();
            if passed {
                assert!(!tap.wanted(true), "the selection frame passed over");
            }
            assert!(!tap.wanted(false), "a play frame is never copied");
            let t = Instant::now();
            assert_eq!(tap.wait(s, Duration::from_secs(5)), Err(REFUSED.to_owned()));
            assert!(t.elapsed() < Duration::from_secs(1), "refused at once");
            assert!(!tap.wanted(true), "the refused request is over");
        }
    }

    /// A copy whose present did not advance the swap chain's present count
    /// (the same count before and after, or a count that could not be read)
    /// is not a shot.
    #[test]
    fn a_present_that_did_not_advance_is_not_a_shot() {
        for (before, after) in [(Some(9), Some(9)), (Some(9), None), (None, Some(10))] {
            let tap = Tap::with_clock(now);
            let s = tap.request();
            assert!(!tap.wanted(true));
            assert!(tap.wanted(true));
            tap.presented(before, after);
            tap.deliver(Ok(frame(3)));
            let e = tap.wait(s, Duration::from_secs(1)).unwrap_err();
            assert!(e.contains("did not advance"), "{before:?} {after:?}: {e}");
        }
    }

    /// A copy whose frame never came back is taken again from the next
    /// frame; a failed copy, or one that was never presented, answers an
    /// error.
    #[test]
    fn a_lost_or_failed_copy_is_not_a_shot() {
        let tap = Tap::with_clock(now);
        let s = tap.request();
        assert!(!tap.wanted(true));
        assert!(tap.wanted(true));
        // That frame failed before its present: the next is copied.
        assert!(tap.wanted(true));
        tap.presented(Some(2), Some(3));
        tap.deliver(Ok(frame(9)));
        assert_eq!(tap.wait(s, Duration::from_secs(1)).unwrap().present, 3);
        let s = tap.request();
        assert!(!tap.wanted(true));
        assert!(tap.wanted(true));
        tap.presented(None, None);
        tap.deliver(Err("map".into()));
        assert_eq!(tap.wait(s, Duration::from_secs(1)), Err("map".into()));
        let s = tap.request();
        assert!(!tap.wanted(true));
        assert!(tap.wanted(true));
        tap.deliver(Ok(frame(1)));
        assert!(tap.wait(s, Duration::from_secs(1)).is_err());
    }

    /// A render thread that presents nothing leaves the request unanswered
    /// until the wait's end: an error, not a frame.
    #[test]
    fn no_frame_within_the_wait_is_an_error() {
        let tap = Tap::with_clock(now);
        let s = tap.request();
        let t = Instant::now();
        let r = tap.wait(s, Duration::from_millis(50));
        assert!(r.unwrap_err().contains("no frame"));
        assert!(t.elapsed() >= Duration::from_millis(50));
        assert!(!tap.wanted(true), "the request is dropped");
    }

    /// The volume arm's witness keeps each setting it reached, in order,
    /// with its slider's number, its percent and its counter, and refuses
    /// once it has kept all it can.
    #[test]
    fn the_volume_witness_keeps_each_setting_in_order() {
        let from = VOLUME_N.load(Ordering::Acquire);
        volume_reached(game::Channel::Key, 50, VolumeOrigin::Press);
        volume_reached(game::Channel::System, 25, VolumeOrigin::Drag);
        volume_reached(game::Channel::Background, 75, VolumeOrigin::Other);
        let got = volumes_reached().unwrap();
        let got: Vec<(u8, u32, VolumeOrigin)> = got
            .get(from..)
            .unwrap()
            .iter()
            .map(|v| (v.slider, v.percent, v.origin))
            .collect();
        assert_eq!(
            got,
            [
                (18, 50, VolumeOrigin::Press),
                (17, 25, VolumeOrigin::Drag),
                (19, 75, VolumeOrigin::Other)
            ]
        );
        while VOLUME_N.load(Ordering::Acquire) <= VOLUME_LOG {
            volume_reached(game::Channel::Key, 1, VolumeOrigin::Press);
        }
        assert!(volumes_reached().unwrap_err().contains("more than the 64"));
    }

    /// An answer reads back as written, its pixels whole; an error answer
    /// reads as the error; a short answer is refused.
    #[test]
    fn an_answer_reads_back_as_written() {
        let shot = Shot {
            width: 2,
            height: 1,
            rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
            previous: 8,
            present: 9,
            acquired: 100,
            presented: 120,
            volumes: vec![
                VolumeSet {
                    slider: 17,
                    percent: 25,
                    at: 90,
                    origin: VolumeOrigin::Press,
                },
                VolumeSet {
                    slider: 19,
                    percent: 75,
                    at: 95,
                    origin: VolumeOrigin::Drag,
                },
            ],
        };
        let mut buf = Vec::new();
        write_answer(&mut buf, 5, &Ok(shot.clone())).unwrap();
        let first = buf.len();
        write_answer(&mut buf, 6, &Err("late".into())).unwrap();
        let mut input = std::io::Cursor::new(buf.clone());
        assert_eq!(read_answer(&mut input).unwrap(), (5, Ok(shot)));
        assert_eq!(read_answer(&mut input).unwrap(), (6, Err("late".into())));
        // The first answer with its last three pixel bytes cut off.
        let short = buf.get(..first - 3).unwrap().to_vec();
        let mut input = std::io::Cursor::new(short);
        assert!(read_answer(&mut input).unwrap_err().contains("of 8 bytes"));
        assert_eq!(parse_request(&request_line(12)), Some(12));
    }
}
