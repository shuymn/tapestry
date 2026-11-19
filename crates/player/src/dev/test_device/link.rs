//! The control link: the device's CDC ACM function as a COM port
//! (docs/test-device.md "Commands", "Host Commands"), one connection in
//! this process. A [`Connection`] is bound to the device at one USB
//! location: the location resolves to the COM port interface of interface
//! 2, the port is opened by that interface path, and the opened path must
//! resolve back to the same location before the port is set up. The device's `hello` must be of this protocol version (and,
//! when a build is expected, name it with `faults 0`) before anything else
//! is sent. The connection owns its port, its token (from that `hello`),
//! its clock samples (the newest pongs), its queue of unanswered commands
//! and its grant: a [`Grant`] is made only by reading this connection's
//! answer to its own `lease`, and a held change takes its lease id from
//! it. Every command is written through [`protocol::encode`]; every line
//! the device sends must be an answer or a trace record in the one form the
//! crate writes. A `pong` answers its ping by number; any other answer the
//! oldest unanswered command that is not a ping.
//!
//! Closing the port drops DTR, which ends the device's connection, its
//! lease and its trace (docs/test-device.md "Lease and Disarm").

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use serde_json::{Value, json};
use test_device::clock::{self, Bounds};
use test_device::protocol::{
    self, Answer, Change, Command, LeaseId, MAX_ANSWER, Malformed, Token, VERSION,
};
use test_device::trace::Record;
use windows::Win32::Devices::Communication::{
    COMMTIMEOUTS, DCB, EscapeCommFunction, GetCommState, NOPARITY, ONESTOPBIT, PURGE_RXCLEAR,
    PURGE_TXCLEAR, PurgeComm, SETDTR, SetCommState, SetCommTimeouts,
};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::core::PCWSTR;

use super::win::{Owned, qpc, qpc_freq, wide};

/// How long the device may take to answer the first `hello`.
pub const HELLO_WAIT: Duration = Duration::from_secs(3);
/// How many of the newest round trips the clock samples keep.
pub const PINGS_KEPT: usize = 16;

/// A link's byte stream: the COM port, or a stand-in in tests.
pub trait Port: Send {
    /// Writes all of `bytes`.
    ///
    /// # Errors
    ///
    /// The write failed or was short.
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()>;

    /// Reads what has arrived, waiting at most about 1 ms: 0 bytes when
    /// nothing did.
    ///
    /// # Errors
    ///
    /// The read failed: the device has gone.
    fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize>;

    /// Sets the line coding to 1200 baud, which reboots the device into its
    /// boot ROM's drive without its command parser (docs/test-device.md
    /// "Recovery").
    ///
    /// # Errors
    ///
    /// The line coding could not be set; it fails as well when the device
    /// reboots before the rest of the port's state is set.
    fn touch_bootsel(&mut self) -> anyhow::Result<()>;

    /// Sets the port up for the link: its line coding with DTR raised, a
    /// 1 ms read bound, its queues cleared.
    ///
    /// # Errors
    ///
    /// The port cannot be set up.
    fn setup(&mut self) -> anyhow::Result<()>;
}

/// The device's link as the configuration manager names it: the interface
/// path of interface 2's COM port interface, and its COM name (shown only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub path: String,
    pub com: Option<String>,
}

impl Link {
    /// The name to show: the COM name, else the path.
    pub fn name(&self) -> &str {
        self.com.as_deref().unwrap_or(&self.path)
    }
}

/// What resolves a USB location to the device's link: the configuration
/// manager, or a stand-in in tests.
pub trait Endpoints {
    /// The one device's location and its [`Link`] (none while the port is
    /// not there yet), on location `bound` when given: `None` when no device
    /// is present.
    ///
    /// # Errors
    ///
    /// The device list cannot be read, or a [`super::devices::Refusal`]:
    /// several devices, an unreadable location, or a device elsewhere.
    fn find(&self, bound: Option<&str>) -> anyhow::Result<Option<(String, Option<Link>)>>;

    /// Opens the port at interface path `path`, without setting it up
    /// ([`Port::setup`]).
    ///
    /// # Errors
    ///
    /// The port cannot be opened.
    fn open(&self, path: &str) -> anyhow::Result<Box<dyn Port>>;

    /// The location of the test device whose link's interface path is
    /// `path`: `None` when the path is not a test device's link.
    ///
    /// # Errors
    ///
    /// The location cannot be read.
    fn location_of(&self, path: &str) -> anyhow::Result<Option<String>>;
}

/// Why a connection was refused before or at its `hello`.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("no test device{}", .0.as_deref().map(|l| format!(" at location {l}")).unwrap_or_default())]
    Absent(Option<String>),
    #[error("the test device at location {0} has no COM port on interface 2 yet")]
    NoPort(String),
    #[error("{com} resolves back to location {found:?}, not the bound location {bound}")]
    Endpoint {
        com: String,
        found: Option<String>,
        bound: String,
    },
    #[error("{0}")]
    Hello(String),
}

/// What the host sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Command(Command),
    /// A malformed line of the framing probe.
    Malformed(Malformed),
}

impl std::fmt::Display for What {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(c) => write!(f, "{c}"),
            Self::Malformed(m) => write!(f, "<malformed {}>", m.name()),
        }
    }
}

/// A command sent, with the host counter just before its write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub what: What,
    pub q: i64,
}

impl Sent {
    fn ping(&self) -> Option<u32> {
        match self.what {
            What::Command(Command::Ping(n)) => Some(n),
            _ => None,
        }
    }
}

/// Takes the command `answer` answers from `outstanding`: a `pong <n>`
/// answers the ping numbered `n` alone, and the pings sent before it are
/// left unanswered for good (the device answers in order; `unanswered`
/// counts them); any other answer takes the oldest command that is not a
/// ping. `None` when nothing it could answer is outstanding.
pub fn pair(
    outstanding: &mut VecDeque<Sent>,
    answer: &Answer<'_>,
    unanswered: &mut u32,
) -> Option<Sent> {
    let at = if let Answer::Pong { n, .. } = *answer {
        let at = outstanding.iter().position(|s| s.ping() == Some(n))?;
        let mut i = 0;
        outstanding.retain(|s| {
            let earlier_ping = i < at && s.ping().is_some();
            i += 1;
            if earlier_ping {
                *unanswered += 1;
            }
            !earlier_ping
        });
        outstanding.iter().position(|s| s.ping() == Some(n))?
    } else {
        outstanding.iter().position(|s| s.ping().is_none())?
    };
    outstanding.remove(at)
}

/// A lease the device granted on this connection, read from its answer
/// to this connection's own `lease`. Only [`Connection`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    id: LeaseId,
    deadline: u64,
}

impl Grant {
    pub const fn id(&self) -> LeaseId {
        self.id
    }

    /// The lease's deadline on the device's clock, as last answered.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "read by the runner's guard; tests only until it lands"
        )
    )]
    pub const fn deadline(&self) -> u64 {
        self.deadline
    }
}

/// What the checked `hello` said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    pub line: String,
    pub build: String,
    pub mode: test_device::mode::Mode,
    pub lease: Option<u32>,
    pub identified: bool,
    pub faults: bool,
    pub quiet: bool,
    pub session: Option<Token>,
}

/// Checks a `hello` answer: one of this protocol version in its one form,
/// and, when `build` is given, naming that build with `faults 0`.
///
/// # Errors
///
/// What is wrong with it.
pub fn hello_facts(line: &str, build: Option<&str>) -> Result<Facts, String> {
    let Some(Answer::Hello(h)) = Answer::parse_canonical(line) else {
        return Err(format!(
            "the device answered `hello` with `{line}`, not a hello of protocol version {VERSION}"
        ));
    };
    if let Some(b) = build {
        if h.build != b {
            return Err(format!("the device is build {}, not {b}", h.build));
        }
        if h.faults {
            return Err(format!(
                "the device's build {b} has debug faults (`faults 1`)"
            ));
        }
    }
    Ok(Facts {
        line: line.to_owned(),
        build: h.build.to_owned(),
        mode: h.mode,
        lease: h.lease,
        identified: h.identified,
        faults: h.faults,
        quiet: h.quiet,
        session: h.session,
    })
}

/// Splits the device's bytes into lines at line feeds, refusing a line
/// that is not UTF-8 or that passes [`MAX_ANSWER`].
#[derive(Debug, Default)]
pub struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    /// Adds `bytes`, appending every completed line, without its line
    /// feed, to `out`.
    ///
    /// # Errors
    ///
    /// A line is not UTF-8 or is too long.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<String>) -> anyhow::Result<()> {
        for &b in bytes {
            if b == b'\n' {
                let line = std::mem::take(&mut self.partial);
                let line = String::from_utf8(line).map_err(|e| {
                    anyhow!("a line from the device is not UTF-8: {:?}", e.as_bytes())
                })?;
                out.push(line);
            } else {
                // The byte and the line feed still to come.
                if self.partial.len() + 2 > MAX_ANSWER {
                    bail!("a line from the device passes {MAX_ANSWER} bytes");
                }
                self.partial.push(b);
            }
        }
        Ok(())
    }
}

/// A line the device sent, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An answer, with the command it answers.
    Answer { q: i64, line: String, to: Sent },
    /// A trace record.
    Record {
        q: i64,
        line: String,
        record: Record,
    },
}

/// One connection to the device.
pub struct Connection {
    port: Box<dyn Port>,
    com: String,
    hello: Option<Facts>,
    lines: Lines,
    /// Events read with the `hello`, for the next poll.
    pending: Vec<Event>,
    outstanding: VecDeque<Sent>,
    unanswered_pings: u32,
    pings: VecDeque<clock::Ping>,
    freq: i64,
    grant: Option<Grant>,
    journal: Vec<Value>,
}

impl Connection {
    /// Binds to the device at `bound` (the one device present when `None`):
    /// the COM port interface path of its location's interface 2 is opened,
    /// and the opened path must resolve back to that location before the
    /// port is set up. Nothing is written.
    ///
    /// # Errors
    ///
    /// A [`Refused`], a [`super::devices::Refusal`], or the port cannot be
    /// opened.
    pub fn bind<E: Endpoints + ?Sized>(ep: &E, bound: Option<&str>) -> anyhow::Result<Self> {
        let Some((location, com)) = ep.find(bound)? else {
            return Err(Refused::Absent(bound.map(str::to_owned)).into());
        };
        if let Some(b) = bound
            && b != location
        {
            return Err(Refused::Endpoint {
                com: com.map(|l| l.name().to_owned()).unwrap_or_default(),
                found: Some(location),
                bound: b.to_owned(),
            }
            .into());
        }
        let link = com.ok_or_else(|| Refused::NoPort(location.clone()))?;
        let com = link.name().to_owned();
        // By the interface path, which names the function's device node,
        // never by the COM name, which another device may take; checked
        // back to the location before the port is set up at all.
        let mut port = ep.open(&link.path)?;
        let back = ep.location_of(&link.path)?;
        if back.as_deref() != Some(location.as_str()) {
            return Err(Refused::Endpoint {
                com,
                found: back,
                bound: location,
            }
            .into());
        }
        port.setup()?;
        let freq = qpc_freq();
        let journal = vec![json!({
            "t": "open", "q": qpc(), "freq": freq, "location": location, "com": com,
            "path": link.path,
        })];
        Ok(Self {
            port,
            com,
            hello: None,
            lines: Lines::default(),
            pending: Vec::new(),
            outstanding: VecDeque::new(),
            unanswered_pings: 0,
            pings: VecDeque::new(),
            freq,
            grant: None,
            journal,
        })
    }

    pub fn com(&self) -> &str {
        &self.com
    }

    /// The connection's token, from its checked `hello`.
    pub fn token(&self) -> Option<Token> {
        self.hello.as_ref().and_then(|h| h.session)
    }

    /// The grant read on this connection, while it holds.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the runner's guard reads it; tests only until it lands"
        )
    )]
    pub const fn grant(&self) -> Option<&Grant> {
        self.grant.as_ref()
    }

    /// The commands not answered yet.
    pub fn outstanding(&self) -> impl Iterator<Item = &Sent> {
        self.outstanding.iter()
    }

    /// The pings whose pong never came (a later ping was answered first).
    pub const fn unanswered_pings(&self) -> u32 {
        self.unanswered_pings
    }

    /// The clock bounds of the connection's newest round trips.
    ///
    /// # Errors
    ///
    /// No round trip yet, or the samples are inconsistent.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the runner's guard derives deadlines from it")
    )]
    pub fn bounds(&self) -> Result<Bounds, clock::Error> {
        let pings: Vec<clock::Ping> = self.pings.iter().copied().collect();
        Bounds::fit(&pings)
    }

    /// The records of what happened since the last call: the open, each
    /// command, answer and trace record with the host counter.
    pub fn take_journal(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.journal)
    }

    /// Sends `hello` and checks its answer ([`hello_facts`]); nothing else
    /// is sent before it passes. Every other line of the reads up to it is
    /// checked as [`Self::poll`] checks it, and its events are kept for the
    /// next poll.
    ///
    /// # Errors
    ///
    /// [`Refused::Hello`], a malformed line or an answer to nothing in the
    /// same reads, no answer within [`HELLO_WAIT`], or the link failed.
    pub fn check_hello(&mut self, build: Option<&str>) -> anyhow::Result<&Facts> {
        self.send(Command::Hello)?;
        let end = Instant::now() + HELLO_WAIT;
        let mut facts = None;
        while facts.is_none() && Instant::now() < end {
            for (line, q) in self.read_lines()? {
                if facts.is_none() && line.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) {
                    facts = match hello_facts(&line, build) {
                        Ok(f) => Some(f),
                        Err(e) => {
                            self.journal
                                .push(json!({ "t": "ans", "q": q, "line": line }));
                            return Err(Refused::Hello(e).into());
                        }
                    };
                    self.take(line, q)?;
                } else {
                    let event = self.take(line, q)?;
                    self.pending.push(event);
                }
            }
        }
        match facts {
            Some(f) => Ok(self.hello.insert(f)),
            None => bail!("no answer to `hello` within {HELLO_WAIT:?}"),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<i64> {
        let q = qpc();
        self.port.write(bytes)?;
        Ok(q)
    }

    /// Writes `cmd` through [`protocol::encode`] and returns the host
    /// counter just before the write.
    ///
    /// # Errors
    ///
    /// Anything but `hello` before the `hello` is checked, a held change
    /// without a token, a line too long, or the write failed.
    pub fn send(&mut self, cmd: Command) -> anyhow::Result<i64> {
        if self.hello.is_none() && cmd != Command::Hello {
            bail!("`{cmd}` before the device's `hello` was checked");
        }
        let line = protocol::encode(&cmd).map_err(|e| anyhow!("{e}"))?;
        let q = self.write(line.as_bytes())?;
        self.journal
            .push(json!({ "t": "cmd", "q": q, "line": line.text() }));
        // `flush` is answered by the trace's `F`, not by an answer line.
        if cmd != Command::Flush {
            self.outstanding.push_back(Sent {
                what: What::Command(cmd),
                q,
            });
        }
        Ok(q)
    }

    /// Writes one malformed case of the framing probe as it is.
    ///
    /// # Errors
    ///
    /// Before the `hello` is checked, or the write failed.
    pub fn send_malformed(&mut self, m: Malformed) -> anyhow::Result<i64> {
        if self.hello.is_none() {
            bail!("a malformed line before the device's `hello` was checked");
        }
        let q = self.write(m.bytes())?;
        self.journal
            .push(json!({ "t": "malformed", "q": q, "case": m.name() }));
        self.outstanding.push_back(Sent {
            what: What::Malformed(m),
            q,
        });
        Ok(q)
    }

    /// Asks for a lease of `ms` for this connection, its first deadline at
    /// device time `deadline`. The grant is read from its answer.
    ///
    /// # Errors
    ///
    /// No token, or as [`Self::send`].
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the runner grants through it; tests only until it lands"
        )
    )]
    pub fn lease(&mut self, ms: u32, deadline: u64) -> anyhow::Result<i64> {
        let token = self
            .token()
            .context("the device's `hello` gave no session")?;
        self.send(Command::Lease {
            token,
            ms,
            deadline,
        })
    }

    /// Sends one held change under the grant read on this connection.
    ///
    /// # Errors
    ///
    /// No grant has been read, or as [`Self::send`].
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the runner holds through it; tests only until it lands"
        )
    )]
    pub fn hold(&mut self, change: Change) -> anyhow::Result<i64> {
        let id = self
            .grant
            .context("no lease granted on this connection")?
            .id();
        self.send(Command::Set { id, change })
    }

    fn read_lines(&mut self) -> anyhow::Result<Vec<(String, i64)>> {
        let mut buf = [0_u8; 4096];
        let n = match self.port.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                self.journal
                    .push(json!({ "t": "lost", "q": qpc(), "error": format!("{e:#}") }));
                return Err(e);
            }
        };
        let q = qpc();
        let mut lines = Vec::new();
        self.lines
            .push(buf.get(..n).unwrap_or_default(), &mut lines)?;
        Ok(lines.into_iter().map(|l| (l, q)).collect())
    }

    /// Reads what has arrived, waiting at most about 1 ms, after the events
    /// read with the `hello`.
    ///
    /// # Errors
    ///
    /// The read failed, or the device sent a malformed line or an answer to
    /// nothing outstanding.
    pub fn poll(&mut self) -> anyhow::Result<Vec<Event>> {
        let mut out = std::mem::take(&mut self.pending);
        for (line, q) in self.read_lines()? {
            out.push(self.take(line, q)?);
        }
        Ok(out)
    }

    fn take(&mut self, line: String, q: i64) -> anyhow::Result<Event> {
        match line.bytes().next() {
            Some(b'a'..=b'z') => {
                let answer = Answer::parse_canonical(&line)
                    .with_context(|| format!("a malformed answer from the device: {line:?}"))?;
                let to = pair(&mut self.outstanding, &answer, &mut self.unanswered_pings)
                    .with_context(|| format!("`{line}` answers no command outstanding"))?;
                self.note(&answer, to, q)?;
                self.journal.push(json!({
                    "t": "ans", "q": q, "sent": to.q, "cmd": to.what.to_string(), "line": line,
                }));
                if let Answer::Pong { n, at } = answer {
                    self.journal.push(json!({
                        "t": "ping", "n": n, "sent": to.q, "device": at, "recv": q,
                    }));
                }
                Ok(Event::Answer { q, line, to })
            }
            Some(b'A'..=b'Z') => {
                let record = Record::parse_canonical(&line)
                    .with_context(|| format!("a malformed trace record: {line:?}"))?;
                self.journal
                    .push(json!({ "t": "rec", "q": q, "line": line }));
                Ok(Event::Record { q, line, record })
            }
            _ => bail!("a malformed line from the device: {line:?}"),
        }
    }

    /// What an answer changes in the connection: a clock sample from a
    /// pong, the grant from the answer to this connection's own `lease`.
    fn note(&mut self, answer: &Answer<'_>, to: Sent, q: i64) -> anyhow::Result<()> {
        match (*answer, to.what) {
            (Answer::Pong { at, .. }, _) => {
                self.pings
                    .push_back(clock::Ping::from_ticks(to.q, at, q, self.freq));
                while self.pings.len() > PINGS_KEPT {
                    self.pings.pop_front();
                }
            }
            (Answer::Leased { id, deadline }, What::Command(Command::Lease { token, .. })) => {
                if Some(token) != self.token() || id.token != token {
                    bail!("the grant {id} is not of this connection's token");
                }
                self.grant = Some(Grant { id, deadline });
            }
            (Answer::Leased { id, deadline }, What::Command(Command::Renew { id: asked, .. })) => {
                match &mut self.grant {
                    Some(g) if g.id == id && asked == id => g.deadline = deadline,
                    _ => bail!("the renewal's answer names {id}, not the lease held"),
                }
            }
            (Answer::Leased { id, .. }, what) => {
                bail!("`lease {id}` answers `{what}`");
            }
            (Answer::Err(_), What::Command(c)) if c.holds_input() => self.grant = None,
            // The device ends the lease on a malformed line.
            (_, What::Malformed(_)) | (Answer::Ok, What::Command(Command::Release)) => {
                self.grant = None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Sets 1200 baud on the bound port: the device reboots into its boot
    /// ROM's drive. Nothing is written.
    ///
    /// # Errors
    ///
    /// As [`Port::touch_bootsel`].
    pub fn touch_bootsel(mut self) -> anyhow::Result<()> {
        self.port.touch_bootsel()
    }
}

/// The link's baud rate. It means nothing to a CDC ACM device, but must
/// never be [`BOOTSEL_BAUD`].
const BAUD: u32 = 115_200;
/// The line coding that reboots the device into its boot ROM's drive.
const BOOTSEL_BAUD: u32 = 1200;
/// `DCB` flags: fBinary, DTR_CONTROL_ENABLE (bits 4-5) and
/// RTS_CONTROL_ENABLE (bits 12-13).
const DCB_FLAGS: u32 = 1 | (1 << 4) | (1 << 12);

/// An open COM port.
pub struct ComPort {
    handle: Owned,
}

impl ComPort {
    /// Opens the port at interface path `name` without setting it up
    /// ([`Port::setup`]).
    ///
    /// # Errors
    ///
    /// The port cannot be opened.
    pub fn open(name: &str) -> anyhow::Result<Self> {
        let path = wide(name);
        // SAFETY: a nul-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .with_context(|| format!("cannot open {name}"))?;
        Ok(Self {
            handle: Owned(handle),
        })
    }

    fn set_baud(&self, baud: u32) -> anyhow::Result<()> {
        let mut dcb = DCB {
            DCBlength: u32::try_from(size_of::<DCB>()).unwrap_or(0),
            ..DCB::default()
        };
        // SAFETY: the handle is an open COM port; one DCB written.
        unsafe { GetCommState(self.handle.0, &mut dcb) }.context("GetCommState")?;
        dcb.BaudRate = baud;
        dcb._bitfield = DCB_FLAGS;
        dcb.ByteSize = 8;
        dcb.Parity = NOPARITY;
        dcb.StopBits = ONESTOPBIT;
        // SAFETY: as above; the DCB is read.
        unsafe { SetCommState(self.handle.0, &dcb) }.context("SetCommState")
    }
}

impl Port for ComPort {
    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut written = 0;
        // SAFETY: the buffer outlives the synchronous write.
        unsafe { WriteFile(self.handle.0, Some(bytes), Some(&mut written), None) }
            .context("writing to the link")?;
        if written as usize != bytes.len() {
            bail!("the link took {written} of {} bytes", bytes.len());
        }
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let mut got = 0;
        // SAFETY: the buffer outlives the synchronous read.
        unsafe { ReadFile(self.handle.0, Some(buf), Some(&mut got), None) }
            .context("reading the link")?;
        Ok(got as usize)
    }

    fn touch_bootsel(&mut self) -> anyhow::Result<()> {
        self.set_baud(BOOTSEL_BAUD)
    }

    /// Sets the line coding with DTR raised, and reads with a 1 ms bound: a
    /// read returns as soon as bytes arrive, or with none after 1 ms.
    fn setup(&mut self) -> anyhow::Result<()> {
        self.set_baud(BAUD)?;
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: u32::MAX,
            ReadTotalTimeoutMultiplier: u32::MAX,
            ReadTotalTimeoutConstant: 1,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 500,
        };
        // SAFETY: the handle is an open COM port; the struct is read.
        unsafe { SetCommTimeouts(self.handle.0, &timeouts) }.context("SetCommTimeouts")?;
        // SAFETY: as above.
        unsafe { EscapeCommFunction(self.handle.0, SETDTR) }.context("SETDTR")?;
        // SAFETY: as above.
        let _ = unsafe { PurgeComm(self.handle.0, PURGE_RXCLEAR | PURGE_TXCLEAR) };
        Ok(())
    }
}

/// A simulated device and host for the commands' tests: the device
/// answers the canonical commands it reads, as the firmware does, and
/// `err syntax` or `err overlong` to any other line.
#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, PoisonError};

    use test_device::protocol::{Command, MAX_LINE, Token};

    use super::{Endpoints, Link, Port};
    use crate::dev::test_device::devices;

    pub const TOKEN: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 3,
    };

    /// A `hello` of this protocol version naming `build`.
    pub fn hello(build: &str, faults: bool) -> String {
        format!(
            "hello {} build {build} mode 8 lease - armed 0 reason none boot power-on count 0 \
             identified 1 faults {} quiet 1 session {TOKEN} draw 0 0 0",
            test_device::protocol::VERSION,
            u8::from(faults)
        )
    }

    /// The simulated device's side of one link.
    #[derive(Debug, Default)]
    pub struct Device {
        pub hello: String,
        /// Every byte the host wrote.
        pub written: Vec<u8>,
        partial: Vec<u8>,
        pub out: VecDeque<u8>,
        /// Answers wait in `held` until released.
        pub hold_answers: bool,
        pub held: Vec<String>,
        /// Reads lines with the lenient parse.
        pub lenient: bool,
        /// The number its next grant gets.
        pub lease_n: u32,
        pub touched: bool,
        /// How many times the port was set up.
        pub setups: u32,
        clock: u64,
        records: u32,
    }

    impl Device {
        pub fn new(hello: String) -> Self {
            Self {
                hello,
                lease_n: 7,
                clock: 1_000_000,
                ..Self::default()
            }
        }

        /// The lines the host wrote, as text.
        pub fn lines(&self) -> Vec<String> {
            String::from_utf8_lossy(&self.written)
                .split_terminator('\n')
                .map(str::to_owned)
                .collect()
        }

        pub fn release_answers(&mut self) {
            for a in std::mem::take(&mut self.held) {
                self.out.extend(a.bytes());
            }
            self.hold_answers = false;
        }

        fn answer(&mut self, line: &[u8]) -> String {
            let text = std::str::from_utf8(line).ok();
            let cmd = text.and_then(|t| {
                if self.lenient {
                    Command::parse(t)
                } else {
                    Command::parse_canonical(t)
                }
            });
            let Some(cmd) = cmd else {
                return if line.len() > MAX_LINE {
                    "err overlong".to_owned()
                } else {
                    "err syntax".to_owned()
                };
            };
            // The device's µs: the host's counter, as a device clock of
            // the same rate would read it.
            let now = i128::from(crate::dev::test_device::win::qpc()) * 1_000_000
                / i128::from(crate::dev::test_device::win::qpc_freq());
            self.clock = u64::try_from(now).unwrap_or(0).max(self.clock + 1);
            match cmd {
                Command::Hello => self.hello.clone(),
                Command::Ping(n) => format!("pong {n} {}", self.clock),
                Command::Flush => {
                    self.records += 1;
                    format!("F {}", self.records - 1)
                }
                Command::Lease {
                    token, deadline, ..
                } => {
                    if token == TOKEN {
                        self.lease_n += 1;
                        format!("lease {token}:{} {deadline}", self.lease_n - 1)
                    } else {
                        "err session".to_owned()
                    }
                }
                _ => "ok".to_owned(),
            }
        }
    }

    /// A shared simulated device.
    #[derive(Clone, Debug)]
    pub struct Shared(pub Arc<Mutex<Device>>);

    impl Shared {
        pub fn new(hello: String) -> Self {
            Self(Arc::new(Mutex::new(Device::new(hello))))
        }

        pub fn with<T>(&self, f: impl FnOnce(&mut Device) -> T) -> T {
            f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl Port for Shared {
        fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
            self.with(|d| {
                d.written.extend_from_slice(bytes);
                for &b in bytes {
                    if b == b'\n' {
                        let line = std::mem::take(&mut d.partial);
                        let mut a = d.answer(&line);
                        a.push('\n');
                        if d.hold_answers {
                            d.held.push(a);
                        } else {
                            d.out.extend(a.bytes());
                        }
                    } else {
                        d.partial.push(b);
                    }
                }
            });
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let n = self.with(|d| {
                let n = d.out.len().min(buf.len());
                for (slot, b) in buf.iter_mut().zip(d.out.drain(..n)) {
                    *slot = b;
                }
                n
            });
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(n)
        }

        fn touch_bootsel(&mut self) -> anyhow::Result<()> {
            self.with(|d| d.touched = true);
            Ok(())
        }

        fn setup(&mut self) -> anyhow::Result<()> {
            self.with(|d| d.setups += 1);
            Ok(())
        }
    }

    /// A device present at a location with its link's COM port, whose
    /// interface path names it by its location.
    #[derive(Clone, Debug)]
    pub struct Dev {
        pub location: String,
        pub com: String,
        pub device: Shared,
    }

    impl Dev {
        pub fn new(location: &str, com: &str, hello: String) -> Self {
            Self {
                location: location.to_owned(),
                com: com.to_owned(),
                device: Shared::new(hello),
            }
        }

        /// Its link's interface path.
        pub fn path(&self) -> String {
            format!(
                r"\\?\USB#VID_1209&PID_0001&MI_02#{}#{{comport}}",
                self.location
            )
        }

        /// Whether `name` names its link, by path or by COM name.
        pub fn named(&self, name: &str) -> bool {
            name == self.path() || name == self.com
        }
    }

    /// [`Endpoints::find`] over `devs`.
    pub fn find(
        devs: &[Dev],
        bound: Option<&str>,
    ) -> anyhow::Result<Option<(String, Option<Link>)>> {
        let candidates = devs
            .iter()
            .map(|d| {
                let link = Link {
                    path: d.path(),
                    com: Some(d.com.clone()),
                };
                (Some(d.location.clone()), link)
            })
            .collect();
        Ok(devices::pick(candidates, bound)?.map(|(l, c)| (l, Some(c))))
    }

    /// [`Endpoints::open`] over `devs`.
    pub fn open(devs: &[Dev], name: &str) -> anyhow::Result<Box<dyn Port>> {
        let d = devs
            .iter()
            .find(|d| d.named(name))
            .ok_or_else(|| anyhow::anyhow!("no {name}"))?;
        Ok(Box::new(d.device.clone()))
    }

    /// [`Endpoints::location_of`] over `devs`.
    pub fn location_of(devs: &[Dev], name: &str) -> Option<String> {
        devs.iter()
            .find(|d| d.named(name))
            .map(|d| d.location.clone())
    }

    /// A host with fixed devices; `back` overrides what an opened port
    /// resolves back to, and a COM name in `aliases` opens the device given
    /// there instead (a name that passed to another device).
    #[derive(Debug, Default)]
    pub struct Host {
        pub devs: Vec<Dev>,
        pub back: Option<Option<String>>,
        pub aliases: Vec<(String, Dev)>,
        pub opened: Mutex<Vec<String>>,
    }

    impl Endpoints for Host {
        fn find(&self, bound: Option<&str>) -> anyhow::Result<Option<(String, Option<Link>)>> {
            find(&self.devs, bound)
        }

        fn open(&self, name: &str) -> anyhow::Result<Box<dyn Port>> {
            self.opened
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(name.to_owned());
            if let Some((_, d)) = self.aliases.iter().find(|(a, _)| a == name) {
                return Ok(Box::new(d.device.clone()));
            }
            open(&self.devs, name)
        }

        fn location_of(&self, name: &str) -> anyhow::Result<Option<String>> {
            if let Some(back) = &self.back {
                return Ok(back.clone());
            }
            Ok(location_of(&self.devs, name))
        }
    }
}

#[cfg(test)]
mod tests {
    use test_device::protocol::Control;

    use super::fake::{Dev, Host, TOKEN, hello};
    use super::*;

    const HERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
    const ELSEWHERE: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";

    fn sent(what: Command, q: i64) -> Sent {
        Sent {
            what: What::Command(what),
            q,
        }
    }

    fn ok() -> Answer<'static> {
        Answer::Ok
    }

    /// A pong answers its own ping, and another answer the oldest command
    /// that is not a ping: one left unanswered never shifts the others.
    #[test]
    fn an_unanswered_ping_shifts_no_pairing() {
        let release = sent(Command::Release, 2);
        let trace = sent(Command::Trace(true), 4);
        let mut out: VecDeque<Sent> = [
            sent(Command::Ping(0), 1),
            release,
            sent(Command::Ping(1), 3),
            trace,
            sent(Command::Ping(2), 5),
        ]
        .into();
        let mut unanswered = 0;
        // `ping 0`'s answer never came; `release` is answered after `pong 1`.
        let pong = |n| Answer::Pong { n, at: 77 };
        assert_eq!(
            pair(&mut out, &pong(1), &mut unanswered),
            Some(sent(Command::Ping(1), 3))
        );
        assert_eq!(unanswered, 1);
        assert_eq!(pair(&mut out, &ok(), &mut unanswered), Some(release));
        assert_eq!(pair(&mut out, &pong(9), &mut unanswered), None);
        assert_eq!(pair(&mut out, &ok(), &mut unanswered), Some(trace));
        // Only a ping is left, and an `ok` does not answer it.
        assert_eq!(pair(&mut out, &ok(), &mut unanswered), None);
        assert_eq!(
            pair(&mut out, &pong(2), &mut unanswered),
            Some(sent(Command::Ping(2), 5))
        );
        assert!(out.is_empty());
        assert_eq!(unanswered, 1);
    }

    fn connected(device: &Dev) -> Connection {
        let host = Host {
            devs: vec![device.clone()],
            ..Host::default()
        };
        let mut c = Connection::bind(&host, Some(HERE)).unwrap();
        c.check_hello(Some("abc")).unwrap();
        c
    }

    fn poll_until(c: &mut Connection, done: impl Fn(&Connection) -> bool) {
        let end = Instant::now() + Duration::from_secs(2);
        while !done(c) {
            assert!(Instant::now() < end, "timed out");
            c.poll().unwrap();
        }
    }

    /// A held change takes its lease id from the grant read on this
    /// connection: while the grant's answer is delayed nothing holding is
    /// written, and then the id is the one the answer gave, not one the
    /// host would expect.
    #[test]
    fn a_held_change_takes_the_grant_read_on_this_connection() {
        let dev = Dev::new(HERE, "COM5", hello("abc", false));
        let mut c = connected(&dev);
        let press = Change {
            control: Control::Button(1),
            value: 1,
        };
        assert!(c.hold(press).is_err(), "a hold before any lease");
        dev.device.with(|d| {
            d.hold_answers = true;
            d.lease_n = 41;
        });
        c.lease(250, 5_000_000).unwrap();
        for _ in 0..20 {
            c.poll().unwrap();
        }
        assert!(c.grant().is_none());
        assert!(c.hold(press).is_err(), "a hold before the grant's answer");
        let before = dev.device.with(|d| d.lines());
        assert_eq!(
            before.last().map(String::as_str),
            Some(format!("lease {TOKEN} 250 5000000").as_str())
        );
        assert!(
            !before.iter().any(|l| l.starts_with("button")),
            "{before:?}"
        );
        dev.device.with(fake::Device::release_answers);
        poll_until(&mut c, |c| c.grant().is_some());
        let grant = *c.grant().unwrap();
        assert_eq!(
            grant.id(),
            LeaseId {
                token: TOKEN,
                n: 41
            }
        );
        assert_eq!(grant.deadline(), 5_000_000);
        c.hold(press).unwrap();
        let lines = dev.device.with(|d| d.lines());
        assert_eq!(
            lines.last().map(String::as_str),
            Some(format!("button {TOKEN}:41 1 1").as_str())
        );
    }

    /// The clock samples are this connection's pongs, paired by number.
    #[test]
    fn pongs_give_the_connection_its_clock_samples() {
        let dev = Dev::new(HERE, "COM5", hello("abc", false));
        let mut c = connected(&dev);
        assert!(c.bounds().is_err());
        c.send(Command::Ping(0)).unwrap();
        c.send(Command::Ping(1)).unwrap();
        poll_until(&mut c, |c| c.outstanding().next().is_none());
        assert!(c.bounds().is_ok());
        assert_eq!(c.unanswered_pings(), 0);
    }

    #[test]
    fn nothing_but_hello_goes_before_the_hello_is_checked() {
        let dev = Dev::new(HERE, "COM5", hello("abc", false));
        let host = Host {
            devs: vec![dev.clone()],
            ..Host::default()
        };
        let mut c = Connection::bind(&host, None).unwrap();
        assert!(c.send(Command::Ping(1)).is_err());
        assert!(c.send_malformed(Malformed::Tab).is_err());
        assert!(dev.device.with(|d| d.written.is_empty()));
        assert_eq!(
            c.take_journal().first().map(|r| r["location"].clone()),
            Some(json!(HERE))
        );
    }

    #[test]
    fn a_line_from_the_device_is_split_at_line_feeds_only() {
        let mut lines = Lines::default();
        let mut out = Vec::new();
        lines.push(b"ok\r\nT 1 p 4 R", &mut out).unwrap();
        assert_eq!(out, ["ok\r"]);
        lines.push(b" 10 H 20\npong 3 99\n", &mut out).unwrap();
        assert_eq!(out, ["ok\r", "T 1 p 4 R 10 H 20", "pong 3 99"]);
        assert!(Lines::default().push(b"\xff\n", &mut out).is_err());
        assert!(
            Lines::default()
                .push(&[b'a'; MAX_ANSWER], &mut out)
                .is_err()
        );
        let mut fits = Lines::default();
        fits.push(&[b'a'; MAX_ANSWER - 1], &mut out).unwrap();
        fits.push(b"\n", &mut out).unwrap();
    }

    /// The link is opened by the interface path resolved from the device
    /// at the location and checked there before any serial setup: a COM
    /// name that passed to another device is never opened, and a path whose
    /// device is off the location gets no setup.
    #[test]
    fn the_link_is_checked_before_any_serial_setup() {
        let dev = Dev::new(HERE, "COM5", hello("abc", false));
        let other = Dev::new(ELSEWHERE, "COM9", hello("abc", false));
        let host = Host {
            devs: vec![dev.clone()],
            aliases: vec![("COM5".to_owned(), other.clone())],
            ..Host::default()
        };
        let mut c = Connection::bind(&host, Some(HERE)).unwrap();
        c.check_hello(Some("abc")).unwrap();
        assert_eq!(other.device.with(|d| d.setups), 0);
        assert!(other.device.with(|d| d.written.is_empty()));
        assert_eq!(dev.device.with(|d| d.setups), 1);
        assert_eq!(*host.opened.lock().unwrap(), [dev.path()]);
        let off = Dev::new(HERE, "COM5", hello("abc", false));
        let host = Host {
            devs: vec![off.clone()],
            back: Some(Some(ELSEWHERE.to_owned())),
            ..Host::default()
        };
        let Err(e) = Connection::bind(&host, Some(HERE)) else {
            panic!("a path off the location was bound");
        };
        assert!(format!("{e:#}").contains("resolves back"), "{e:#}");
        assert_eq!(off.device.with(|d| d.setups), 0);
    }

    /// The batch the `hello` arrives in is checked whole: a valid line
    /// after it is kept for the next poll, and a malformed one fails the
    /// link.
    #[test]
    fn the_hello_batch_is_checked_whole_and_its_events_kept() {
        let dev = Dev::new(HERE, "COM5", format!("{}\nF 0", hello("abc", false)));
        let mut c = connected(&dev);
        let mut events = Vec::new();
        let end = Instant::now() + Duration::from_millis(200);
        while events.is_empty() && Instant::now() < end {
            events.extend(c.poll().unwrap());
        }
        assert!(
            matches!(events.as_slice(), [Event::Record { line, .. }] if line == "F 0"),
            "{events:?}"
        );
        let bad = Dev::new(HERE, "COM5", format!("{}\nok\r", hello("abc", false)));
        let host = Host {
            devs: vec![bad],
            ..Host::default()
        };
        let mut c = Connection::bind(&host, Some(HERE)).unwrap();
        let e = c.check_hello(Some("abc")).unwrap_err();
        assert!(format!("{e:#}").contains("malformed"), "{e:#}");
    }

    #[test]
    fn a_malformed_answer_fails_the_link() {
        let dev = Dev::new(HERE, "COM5", hello("abc", false));
        let mut c = connected(&dev);
        c.send(Command::Ping(1)).unwrap();
        dev.device.with(|d| d.out.extend(b"ok\r\n"));
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < end);
            if c.poll().is_err() {
                break;
            }
        }
    }
}
