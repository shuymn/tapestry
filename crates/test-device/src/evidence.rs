//! The strict reader of a run's record files (docs/test-device.md
//! "Verdicts"): every record stream, verdict file and reservation is read
//! here, and only here, so no judge reads a looser form than the checker.
//!
//! A file is UTF-8, one JSON object per line, each line ending in a line
//! feed. A line with a carriage return, an empty line, a duplicate key, a
//! number that is not an integer in its field's range (`NaN`, infinities and
//! `1e999` included), a record of an unknown kind or with an unknown field
//! fails, naming the line. Each stream type declares the kind it starts with
//! and the kind it ends with ([`Kinds`]): the first exactly once and first,
//! the end exactly once and last, nothing after it. A stream whose writer
//! was killed may lack its end and hold one unterminated last line, which
//! is dropped and noted; whether the writer was killed is the reader's
//! caller's to say, from the writer's observed exit ([`Stream::finished`]).
//! Nothing follows an end, a dropped line included.

use std::fmt;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// A stream type's records and the places of its kinds.
pub trait Kinds: DeserializeOwned {
    /// The kind its first record has, exactly once; `None` when any kind
    /// may come first (verdict lines).
    const FIRST: Option<&'static str>;
    /// The kind that ends it, exactly once and last; `None` when it has no
    /// end record.
    const END: Option<&'static str>;

    /// This record's kind, its `t`.
    fn kind(&self) -> &'static str;
}

/// What a file held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream<R> {
    pub records: Vec<R>,
    /// The last record is the stream's end ([`Kinds::END`]).
    pub ended: bool,
    /// The unterminated last line, dropped (lossily decoded), if there was
    /// one.
    pub dropped: Option<String>,
}

/// Why a file is not evidence. Lines count from 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The line's bytes are not UTF-8.
    Utf8 { line: usize },
    /// The line is empty or holds a carriage return.
    Framing { line: usize },
    /// The line is not one record of this stream: not JSON, a duplicate
    /// key, an unknown kind or field, a value of another type or out of
    /// range.
    Record { line: usize, error: String },
    /// The first record is not of the stream's first kind (or there is
    /// none).
    First { want: &'static str },
    /// A second record of the first kind.
    SecondFirst { line: usize },
    /// A second end record.
    SecondEnd { line: usize },
    /// A record after the end.
    AfterEnd { line: usize },
    /// The stream has no end, or an unterminated last line, and its
    /// writer was not cut short; or it has an unterminated line after its
    /// end.
    Unfinished { ended: bool, dropped: bool },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 { line } => write!(f, "line {line} is not UTF-8"),
            Self::Framing { line } => {
                write!(f, "line {line} is empty or holds a carriage return")
            }
            Self::Record { line, error } => write!(f, "line {line}: {error}"),
            Self::First { want } => write!(f, "the first record is not `{want}`"),
            Self::SecondFirst { line } => write!(f, "line {line} repeats the first record"),
            Self::SecondEnd { line } => write!(f, "line {line} repeats the end record"),
            Self::AfterEnd { line } => write!(f, "line {line} follows the end record"),
            Self::Unfinished { ended, dropped } => {
                let what = match (ended, dropped) {
                    (false, true) => "has no end record and an unterminated last line",
                    (false, false) => "has no end record",
                    _ => "has an unterminated last line",
                };
                write!(f, "the stream {what}, and its writer was not cut short")
            }
        }
    }
}

/// Reads one file strictly. A missing end and an unterminated last line
/// are not errors here: [`Stream::finished`] judges them by the writer's
/// observed exit.
///
/// # Errors
///
/// The first line, or the first place rule, the file breaks.
pub fn read<R: Kinds>(bytes: &[u8]) -> Result<Stream<R>, Error> {
    let (whole, dropped) = match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) if i + 1 == bytes.len() => (bytes, None),
        Some(i) => (&bytes[..=i], Some(&bytes[i + 1..])),
        None if bytes.is_empty() => (bytes, None),
        None => (&bytes[..0], Some(bytes)),
    };
    let mut records: Vec<R> = Vec::new();
    let mut ended = false;
    for (i, raw) in whole.split_inclusive(|&b| b == b'\n').enumerate() {
        let line = i + 1;
        let raw = &raw[..raw.len() - 1];
        if raw.is_empty() || raw.contains(&b'\r') {
            return Err(Error::Framing { line });
        }
        let text = std::str::from_utf8(raw).map_err(|_| Error::Utf8 { line })?;
        let record: R = serde_json::from_str(text).map_err(|e| Error::Record {
            line,
            error: e.to_string(),
        })?;
        let kind = record.kind();
        if ended {
            return Err(if Some(kind) == R::END {
                Error::SecondEnd { line }
            } else {
                Error::AfterEnd { line }
            });
        }
        if let Some(first) = R::FIRST {
            if records.is_empty() && kind != first {
                return Err(Error::First { want: first });
            }
            if !records.is_empty() && kind == first {
                return Err(Error::SecondFirst { line });
            }
        }
        ended = Some(kind) == R::END;
        records.push(record);
    }
    if let (Some(first), true) = (R::FIRST, records.is_empty()) {
        return Err(Error::First { want: first });
    }
    Ok(Stream {
        records,
        ended,
        dropped: dropped.map(|d| String::from_utf8_lossy(d).into_owned()),
    })
}

impl<R: Kinds> Stream<R> {
    /// Whether the stream is complete for a writer that was `cut` short (a
    /// process the row killed, or one its killed tree's job ended, whose
    /// exit another process observed): a stream with an end ([`Kinds::END`])
    /// and no unterminated last line is complete whatever its writer did;
    /// one without its end, or with an unterminated last line before any
    /// end, is complete only when its writer was cut short, and the line is
    /// then dropped. A line after the end is never complete: nothing
    /// follows an end.
    ///
    /// # Errors
    ///
    /// The stream is unfinished and its writer was not cut short, or it has
    /// an unterminated line after its end.
    pub const fn finished(&self, cut: bool) -> Result<(), Error> {
        let ended = self.ended || R::END.is_none();
        let dropped = self.dropped.is_some();
        if (ended && !dropped) || (cut && !self.ended) {
            Ok(())
        } else {
            Err(Error::Unfinished { ended, dropped })
        }
    }

    /// The records of kind `kind`.
    pub fn of<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a R> + 'a {
        self.records.iter().filter(move |r| r.kind() == kind)
    }
}

/// A process as its first record names it: its id and its creation time
/// (the system's, so a reused id is another process).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub pid: u32,
    pub created: u64,
}

/// A run reserved for a role: the role, the row it runs, its run id and its
/// folder (relative to the reserving folder).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reserved {
    pub role: String,
    pub row: String,
    pub run: String,
    pub dir: String,
}

/// The runner's stream, `run.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Run {
    Start(Start),
    Seal(Seal),
    Exit(Exit),
    End(RunEnd),
}

/// The runner's first record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    /// The run id: 128 bits from the system's random source, 32 lowercase
    /// hex digits.
    pub run: String,
    pub row: String,
    pub role: String,
    /// The device's build the run expects.
    pub build: String,
    pub process: Process,
    pub args: Vec<String>,
    /// The set whose role this run is, if any.
    pub set: Option<String>,
    /// The inner run this run reserved, if any.
    pub inner: Option<Reserved>,
}

/// A finalized artifact: its run, its role, its path inside the role's
/// folder and its sha256 (64 lowercase hex digits).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seal {
    pub run: String,
    pub role: String,
    pub path: String,
    pub sha256: String,
}

/// A process's exit as the runner observed it: the process its first
/// record named, its exit code, and whether this runner killed it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exit {
    /// The process's name in the row (`recorder`, `player`, `inner`).
    pub name: String,
    pub process: Process,
    pub code: i32,
    pub killed: bool,
}

/// The runner's last record: the code it exits with and every error it
/// kept.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEnd {
    pub exit: i32,
    pub errors: Vec<String>,
}

impl Kinds for Run {
    const FIRST: Option<&'static str> = Some("start");
    const END: Option<&'static str> = Some("end");

    fn kind(&self) -> &'static str {
        match self {
            Self::Start(_) => "start",
            Self::Seal(_) => "seal",
            Self::Exit(_) => "exit",
            Self::End(_) => "end",
        }
    }
}

/// The link's stream, `link.jsonl` (and `trace --out`), on the host's
/// counter `q`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Link {
    /// The port opened: the bound location, the COM port, the interface
    /// path it was opened by, and the counter's frequency.
    Open(LinkOpen),
    /// A command written.
    Cmd(LinkLine),
    /// An answer read; `sent` and `cmd` name the command it answers (none
    /// for the `hello` read while the link opens).
    Ans(LinkAnswer),
    /// A ping paired with its `pong`: when it was sent, the device's time
    /// in the `pong`, and when the `pong` was read.
    Ping(LinkPing),
    /// A trace record read.
    Rec(LinkLine),
    /// A malformed case written (`trace --framing-probe`).
    Malformed(LinkMalformed),
    /// The link failed.
    Lost(LinkError),
    /// The device was refused before any other command.
    Refused(LinkError),
    /// The link's end and what it found wrong.
    End(LinkEnd),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkOpen {
    pub q: i64,
    pub freq: i64,
    pub location: String,
    pub com: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkLine {
    pub q: i64,
    pub line: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkAnswer {
    pub q: i64,
    pub line: String,
    pub sent: Option<i64>,
    pub cmd: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkPing {
    pub n: u32,
    pub sent: i64,
    pub device: u64,
    pub recv: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkMalformed {
    pub q: i64,
    pub case: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkError {
    pub q: i64,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkEnd {
    pub q: i64,
    pub problems: Vec<String>,
}

impl Kinds for Link {
    const FIRST: Option<&'static str> = Some("open");
    const END: Option<&'static str> = Some("end");

    fn kind(&self) -> &'static str {
        match self {
            Self::Open(_) => "open",
            Self::Cmd(_) => "cmd",
            Self::Ans(_) => "ans",
            Self::Ping(_) => "ping",
            Self::Rec(_) => "rec",
            Self::Malformed(_) => "malformed",
            Self::Lost(_) => "lost",
            Self::Refused(_) => "refused",
            Self::End(_) => "end",
        }
    }
}

/// The recorder's stream, `recorder.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Recorder {
    /// The recorder's first record: its process.
    Ready(RecorderReady),
    /// A recorder error: what failed and the stretch of the stream it
    /// affects, from and to on the host's counter.
    Error(RecorderError),
    /// The recorder's end, after draining its queue.
    End(RecorderEnd),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderReady {
    pub q: i64,
    pub process: Process,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderError {
    pub from: i64,
    pub to: i64,
    pub what: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderEnd {
    pub q: i64,
}

impl Kinds for Recorder {
    const FIRST: Option<&'static str> = Some("ready");
    const END: Option<&'static str> = Some("end");

    fn kind(&self) -> &'static str {
        match self {
            Self::Ready(_) => "ready",
            Self::Error(_) => "error",
            Self::End(_) => "end",
        }
    }
}

/// A set's `reservation.jsonl`: its one record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Reservation {
    Reserved(Set),
}

/// A set reserved before any of its runs starts: its id, its row, the
/// device's build its runs expect, and each role's run.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Set {
    pub set: String,
    pub row: String,
    pub build: String,
    pub roles: Vec<Reserved>,
}

impl Kinds for Reservation {
    const FIRST: Option<&'static str> = Some("reserved");
    const END: Option<&'static str> = Some("reserved");

    fn kind(&self) -> &'static str {
        "reserved"
    }
}

/// One verdict line of `verdicts-<producer>.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub row: String,
    pub check: String,
    pub outcome: Outcome,
    pub detail: String,
    /// Each file the check read.
    pub inputs: Vec<Input>,
    pub producer: Producer,
}

/// A check's outcome. `Incomplete`: evidence it needs is missing or
/// unreadable, so it can neither pass nor fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    Incomplete,
}

/// A file a check read: the role's run, its path in that run's folder,
/// and the sha256 of what was read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub role: String,
    pub path: String,
    pub sha256: String,
}

/// The program that wrote a verdict and its version (a commit or a
/// sha256).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub name: String,
    pub version: String,
}

impl Kinds for Verdict {
    const FIRST: Option<&'static str> = None;
    const END: Option<&'static str> = None;

    fn kind(&self) -> &'static str {
        "verdict"
    }
}

#[cfg(test)]
mod tests;
