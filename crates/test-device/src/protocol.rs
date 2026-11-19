//! The control link's protocol (docs/test-device.md "Commands"): one
//! command per line from the host, one answer per line from the device,
//! with trace records ([`crate::trace::Record`]) interleaved. Answers start
//! with a lowercase word, trace records with an uppercase letter.
//!
//! A line ends at a line feed and nowhere else, and each side reads only
//! the form its type writes: a line is a command, an answer or a record
//! exactly when it parses and the value's `Display` writes the same bytes
//! back (`parse_canonical`). The host writes commands only through
//! [`encode`].

use core::fmt;

use crate::boot::Cause;
use crate::faults::Fault;
use crate::lease::{Reason, Refusal};
use crate::mode::{Mode, Rate, Reporting};
use crate::trace::Ep;

/// The protocol's version, the first field of `hello`.
pub const VERSION: u32 = 2;
/// The most changes one pattern item makes in its report.
pub const MAX_CHANGES: usize = 8;
/// The longest command line, line feed excluded.
pub const MAX_LINE: usize = 160;
/// The longest answer or trace record line, line feed included.
pub const MAX_ANSWER: usize = 255;
/// The number of pad buttons.
pub const BUTTONS: u8 = 16;

/// A [`fmt::Write`] that only compares what is written with a line.
struct Same<'a> {
    rest: &'a [u8],
}

impl fmt::Write for Same<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self.rest.strip_prefix(s.as_bytes()) {
            Some(rest) => {
                self.rest = rest;
                Ok(())
            }
            None => Err(fmt::Error),
        }
    }
}

/// Whether `v`'s `Display` writes exactly `line`.
pub(crate) fn writes_back(v: &impl fmt::Display, line: &str) -> bool {
    let mut same = Same {
        rest: line.as_bytes(),
    };
    fmt::write(&mut same, format_args!("{v}")).is_ok() && same.rest.is_empty()
}

/// A connection's session token: 64 bits of the boot ROM's per-boot
/// random number and the connection's count within that boot, written as 24
/// lowercase hex digits (docs/test-device.md "Lease and Disarm").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    pub boot: u64,
    pub count: u32,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}{:08x}", self.boot, self.count)
    }
}

impl Token {
    /// 24 lowercase hex digits.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 24 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        Some(Self {
            boot: u64::from_str_radix(s.get(..16)?, 16).ok()?,
            count: u32::from_str_radix(s.get(16..)?, 16).ok()?,
        })
    }
}

/// A lease's id: its connection's token and its number among that
/// connection's grants, written `<token>:<n>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseId {
    pub token: Token,
    pub n: u32,
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.token, self.n)
    }
}

impl LeaseId {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (token, n) = s.split_once(':')?;
        Some(Self {
            token: Token::parse(token)?,
            n: n.parse().ok()?,
        })
    }
}

/// A control of either HID interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Control {
    /// A keyboard usage: a modifier 0xE0-0xE7 or a usage below 128.
    Key(u8),
    /// A pad button, 1 to [`BUTTONS`].
    Button(u8),
    /// The pad's X axis (the turntable).
    Axis,
}

impl Control {
    #[must_use]
    pub const fn ep(self) -> Ep {
        match self {
            Self::Key(_) => Ep::Keyboard,
            Self::Button(_) | Self::Axis => Ep::Pad,
        }
    }

    const fn valid(self) -> bool {
        match self {
            Self::Key(u) => u < 0x80 || (u >= 0xE0 && u <= 0xE7),
            Self::Button(n) => n >= 1 && n <= BUTTONS,
            Self::Axis => true,
        }
    }
}

/// One change: a key or button down (1) or up (0), or the axis' value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub control: Control,
    pub value: u8,
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.value == 0 { '-' } else { '+' };
        match self.control {
            Control::Key(u) => write!(f, "k{u}{sign}"),
            Control::Button(n) => write!(f, "b{n}{sign}"),
            Control::Axis => write!(f, "x{}", self.value),
        }
    }
}

impl Change {
    /// `k<usage>+`, `k<usage>-`, `b<n>+`, `b<n>-` or `x<value>`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, rest) = s.split_at_checked(1)?;
        let change = if kind == "x" {
            Self {
                control: Control::Axis,
                value: rest.parse().ok()?,
            }
        } else {
            let (n, sign) = rest.split_at_checked(rest.len().checked_sub(1)?)?;
            let value = match sign {
                "+" => 1,
                "-" => 0,
                _ => return None,
            };
            let n = n.parse().ok()?;
            let control = match kind {
                "k" => Control::Key(n),
                "b" => Control::Button(n),
                _ => return None,
            };
            Self { control, value }
        };
        change.control.valid().then_some(change)
    }
}

/// A pattern item: up to [`MAX_CHANGES`] changes on one endpoint, sent as
/// one report `at` µs after the pattern's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub at: u64,
    changes: [Change; MAX_CHANGES],
    len: u8,
}

impl Item {
    /// An item of `changes`, all on one endpoint, none of them twice.
    #[must_use]
    pub fn new(at: u64, changes: &[Change]) -> Option<Self> {
        let first = changes.first()?;
        if changes.len() > MAX_CHANGES {
            return None;
        }
        let mut out = [*first; MAX_CHANGES];
        for (i, c) in changes.iter().enumerate() {
            if c.control.ep() != first.control.ep()
                || changes.get(..i)?.iter().any(|d| d.control == c.control)
            {
                return None;
            }
            *out.get_mut(i)? = *c;
        }
        Some(Self {
            at,
            changes: out,
            len: u8::try_from(changes.len()).ok()?,
        })
    }

    #[must_use]
    pub fn changes(&self) -> &[Change] {
        self.changes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    #[must_use]
    pub const fn ep(&self) -> Ep {
        self.changes[0].control.ep()
    }
}

/// A command from the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// `hello`: the build, mode, lease state, last disarm reason, fault
    /// capability, quiet and session.
    Hello,
    /// `mode <1000|125> <change|continuous>`: store the mode and reboot.
    Mode(Mode),
    /// `lease <token> <ms> <deadline µs>`: a lease of `ms` for the
    /// connection named by `token`, its first deadline at `deadline` on the
    /// device's clock.
    Lease {
        token: Token,
        ms: u32,
        deadline: u64,
    },
    /// `renew <id> <deadline µs>`: the lease's new deadline on the device's
    /// clock ([`crate::lease::Lease::renew`]).
    Renew { id: LeaseId, deadline: u64 },
    /// `key <id> <usage> <0|1>`, `button <id> <n> <0|1>`, `axis <id> <v>`:
    /// one held change, under lease `id`.
    Set { id: LeaseId, change: Change },
    /// `load`: forget the loaded pattern and start a new one.
    Load,
    /// `item <µs> <change>...`: append an item to the loaded pattern.
    Item(Item),
    /// `arm <id> <device µs>`: play the loaded pattern from that time.
    Arm { id: LeaseId, at: u64 },
    /// `ping <n>`.
    Ping(u32),
    /// `trace on|off`.
    Trace(bool),
    /// `flush`: wait for pending reports, then send the trace and `F`.
    Flush,
    /// `release`: end the lease; release everything.
    Release,
    /// `bootsel`: reboot into the boot ROM's drive.
    Bootsel,
    /// `detach <ms>`: leave the bus for `ms`, then come back.
    Detach { ms: u32 },
    /// `fault <name> [arg]`: a debug fault (`debug-faults` builds only).
    Fault { fault: Fault, arg: i64 },
    /// `revoke-test <id> <k|p> <trial>`: one phase-controlled revocation
    /// trial on the endpoint under lease `id`, numbered `trial` in its `V`
    /// record ([`crate::revoke`]).
    RevokeTest { id: LeaseId, ep: Ep, trial: u32 },
}

impl Command {
    /// The command a line's words hold, whatever their spacing and number
    /// form; the device and the host read [`Command::parse_canonical`].
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let mut w = line.split_ascii_whitespace();
        let word = w.next()?;
        let bit = |s: &str| match s {
            "0" => Some(0),
            "1" => Some(1),
            _ => None,
        };
        let cmd = match word {
            "hello" => Self::Hello,
            "mode" => Self::Mode(Mode {
                rate: Rate::parse(w.next()?)?,
                reporting: Reporting::parse(w.next()?)?,
            }),
            "lease" => Self::Lease {
                token: Token::parse(w.next()?)?,
                ms: w.next()?.parse().ok()?,
                deadline: w.next()?.parse().ok()?,
            },
            "renew" => Self::Renew {
                id: LeaseId::parse(w.next()?)?,
                deadline: w.next()?.parse().ok()?,
            },
            "key" | "button" => {
                let id = LeaseId::parse(w.next()?)?;
                let n = w.next()?.parse().ok()?;
                let control = if word == "key" {
                    Control::Key(n)
                } else {
                    Control::Button(n)
                };
                if !control.valid() {
                    return None;
                }
                Self::Set {
                    id,
                    change: Change {
                        control,
                        value: bit(w.next()?)?,
                    },
                }
            }
            "axis" => Self::Set {
                id: LeaseId::parse(w.next()?)?,
                change: Change {
                    control: Control::Axis,
                    value: w.next()?.parse().ok()?,
                },
            },
            "load" => Self::Load,
            "item" => {
                let at = w.next()?.parse().ok()?;
                let mut changes = [Change {
                    control: Control::Axis,
                    value: 0,
                }; MAX_CHANGES];
                let mut len = 0;
                for token in w.by_ref() {
                    *changes.get_mut(len)? = Change::parse(token)?;
                    len += 1;
                }
                return Item::new(at, changes.get(..len)?).map(Self::Item);
            }
            "arm" => Self::Arm {
                id: LeaseId::parse(w.next()?)?,
                at: w.next()?.parse().ok()?,
            },
            "ping" => Self::Ping(w.next()?.parse().ok()?),
            "trace" => Self::Trace(match w.next()? {
                "on" => true,
                "off" => false,
                _ => return None,
            }),
            "flush" => Self::Flush,
            "release" => Self::Release,
            "bootsel" => Self::Bootsel,
            "detach" => Self::Detach {
                ms: w.next()?.parse().ok()?,
            },
            "fault" => {
                let fault = Fault::parse(w.next()?)?;
                let arg = if fault.takes_arg() {
                    w.next()?.parse().ok()?
                } else {
                    0
                };
                Self::Fault { fault, arg }
            }
            "revoke-test" => Self::RevokeTest {
                id: LeaseId::parse(w.next()?)?,
                ep: Ep::parse(w.next()?)?,
                trial: w.next()?.parse().ok()?,
            },
            _ => return None,
        };
        w.next().is_none().then_some(cmd)
    }

    /// The command a line holds in the one form the host writes: the line
    /// parses and the command's `Display` writes it back byte for byte.
    #[must_use]
    pub fn parse_canonical(line: &str) -> Option<Self> {
        Self::parse(line).filter(|c| writes_back(c, line))
    }

    /// Whether the command grants, extends or acts under a lease: the
    /// commands a host stops sending once it stops holding input.
    #[must_use]
    pub const fn holds_input(&self) -> bool {
        matches!(
            self,
            Self::Lease { .. }
                | Self::Renew { .. }
                | Self::Set { .. }
                | Self::Arm { .. }
                | Self::RevokeTest { .. }
        )
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Hello => f.write_str("hello"),
            Self::Mode(m) => write!(f, "mode {} {}", m.rate.name(), m.reporting.name()),
            Self::Lease {
                token,
                ms,
                deadline,
            } => write!(f, "lease {token} {ms} {deadline}"),
            Self::Renew { id, deadline } => write!(f, "renew {id} {deadline}"),
            Self::Set {
                id,
                change:
                    Change {
                        control: Control::Key(u),
                        value,
                    },
            } => write!(f, "key {id} {u} {value}"),
            Self::Set {
                id,
                change:
                    Change {
                        control: Control::Button(n),
                        value,
                    },
            } => write!(f, "button {id} {n} {value}"),
            Self::Set {
                id,
                change:
                    Change {
                        control: Control::Axis,
                        value,
                    },
            } => write!(f, "axis {id} {value}"),
            Self::Load => f.write_str("load"),
            Self::Item(item) => {
                write!(f, "item {}", item.at)?;
                item.changes().iter().try_for_each(|c| write!(f, " {c}"))
            }
            Self::Arm { id, at } => write!(f, "arm {id} {at}"),
            Self::Ping(n) => write!(f, "ping {n}"),
            Self::Trace(on) => f.write_str(if on { "trace on" } else { "trace off" }),
            Self::Flush => f.write_str("flush"),
            Self::Release => f.write_str("release"),
            Self::Bootsel => f.write_str("bootsel"),
            Self::Detach { ms } => write!(f, "detach {ms}"),
            Self::Fault { fault, arg } if fault.takes_arg() => {
                write!(f, "fault {} {arg}", fault.name())
            }
            Self::Fault { fault, .. } => write!(f, "fault {}", fault.name()),
            Self::RevokeTest { id, ep, trial } => {
                write!(f, "revoke-test {id} {} {trial}", ep.name())
            }
        }
    }
}

/// A command line as the host writes it: the command's `Display` and its
/// line feed.
#[derive(Clone, Copy)]
pub struct Line {
    buf: [u8; MAX_LINE + 1],
    len: usize,
}

impl Line {
    /// The bytes to write, line feed included.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or(&[])
    }

    /// The command's text, without the line feed.
    #[must_use]
    pub fn text(&self) -> &str {
        let text = self.buf.get(..self.len.saturating_sub(1)).unwrap_or(&[]);
        core::str::from_utf8(text).unwrap_or_default()
    }
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.text(), f)
    }
}

/// A command whose line would pass [`MAX_LINE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overlong;

impl fmt::Display for Overlong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the command's line passes {MAX_LINE} bytes")
    }
}

/// Fills a [`Line`] up to [`MAX_LINE`].
struct Fill<'a> {
    line: &'a mut Line,
}

impl fmt::Write for Fill<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.line.len + s.len();
        if end > MAX_LINE {
            return Err(fmt::Error);
        }
        let dst = self
            .line
            .buf
            .get_mut(self.line.len..end)
            .ok_or(fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.line.len = end;
        Ok(())
    }
}

/// The line the host writes for `command`: its `Display` and a line feed.
///
/// # Errors
///
/// [`Overlong`] when the text passes [`MAX_LINE`].
pub fn encode(command: &Command) -> Result<Line, Overlong> {
    let mut line = Line {
        buf: [0; MAX_LINE + 1],
        len: 0,
    };
    fmt::write(&mut Fill { line: &mut line }, format_args!("{command}")).map_err(|_| Overlong)?;
    let lf = line.buf.get_mut(line.len).ok_or(Overlong)?;
    *lf = b'\n';
    line.len += 1;
    Ok(line)
}

/// The malformed lines the framing probe sends (docs/test-device.md
/// "Commands"), each answered exactly once with [`Malformed::answer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// A tab between the words.
    Tab,
    /// Two spaces between the words.
    TwoSpaces,
    LeadingSpace,
    TrailingSpace,
    /// An integer with a leading zero.
    LeadingZero,
    /// An integer with a `+` sign.
    PlusSign,
    /// A no-break space (U+00A0) between the words.
    NonAsciiSpace,
    /// A carriage return between the words.
    CrInside,
    /// A carriage return before the line feed.
    CrBeforeLf,
    EmptyLine,
    /// A NUL byte in the line.
    Nul,
    /// Bytes that are not UTF-8.
    NotUtf8,
    /// 161 bytes before the line feed.
    Overlong,
}

const OVERLONG_LINE: [u8; MAX_LINE + 2] = {
    let mut b = [b'a'; MAX_LINE + 2];
    b[MAX_LINE + 1] = b'\n';
    b
};

impl Malformed {
    pub const ALL: [Self; 13] = [
        Self::Tab,
        Self::TwoSpaces,
        Self::LeadingSpace,
        Self::TrailingSpace,
        Self::LeadingZero,
        Self::PlusSign,
        Self::NonAsciiSpace,
        Self::CrInside,
        Self::CrBeforeLf,
        Self::EmptyLine,
        Self::Nul,
        Self::NotUtf8,
        Self::Overlong,
    ];

    /// The case's bytes, its line feed included.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Tab => b"ping\t1\n",
            Self::TwoSpaces => b"ping  1\n",
            Self::LeadingSpace => b" ping 1\n",
            Self::TrailingSpace => b"ping 1 \n",
            Self::LeadingZero => b"ping 01\n",
            Self::PlusSign => b"ping +1\n",
            Self::NonAsciiSpace => b"ping\xc2\xa01\n",
            Self::CrInside => b"ping\r1\n",
            Self::CrBeforeLf => b"ping 1\r\n",
            Self::EmptyLine => b"\n",
            Self::Nul => b"ping 1\0\n",
            Self::NotUtf8 => b"ping \xff\n",
            Self::Overlong => &OVERLONG_LINE,
        }
    }

    /// The device's answer to the case.
    #[must_use]
    pub const fn answer(self) -> Error {
        match self {
            Self::Overlong => Error::Overlong,
            _ => Error::Syntax,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tab => "tab",
            Self::TwoSpaces => "two-spaces",
            Self::LeadingSpace => "leading-space",
            Self::TrailingSpace => "trailing-space",
            Self::LeadingZero => "leading-zero",
            Self::PlusSign => "plus-sign",
            Self::NonAsciiSpace => "non-ascii-space",
            Self::CrInside => "cr-inside",
            Self::CrBeforeLf => "cr-before-lf",
            Self::EmptyLine => "empty-line",
            Self::Nul => "nul",
            Self::NotUtf8 => "not-utf8",
            Self::Overlong => "overlong",
        }
    }
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The line is not a command in its one form (or not UTF-8). Ends the
    /// lease.
    Syntax,
    /// The line is longer than [`MAX_LINE`]. Ends the lease.
    Overlong,
    Lease(Refusal),
    /// The pattern is full, or an item is earlier than the one before it
    /// on its endpoint.
    Pattern,
    /// A pattern is armed: `load`, `item` and held changes wait for it.
    Playing,
    /// Timing commands wait until the pad's endpoint is identified (its
    /// buffer's LENGTH equals the pad report's).
    Unidentified,
    /// This build has no debug faults.
    NoFaults,
    /// The revocation trial needs 125 Hz reporting on change.
    Mode,
}

const ERRORS: [Error; 14] = [
    Error::Syntax,
    Error::Overlong,
    Error::Lease(Refusal::BadLength),
    Error::Lease(Refusal::Stale),
    Error::Lease(Refusal::Session),
    Error::Lease(Refusal::Held),
    Error::Lease(Refusal::NoLease),
    Error::Lease(Refusal::WrongId),
    Error::Lease(Refusal::Armed),
    Error::Pattern,
    Error::Playing,
    Error::Unidentified,
    Error::NoFaults,
    Error::Mode,
];

impl Error {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Overlong => "overlong",
            Self::Lease(Refusal::BadLength) => "bad-length",
            Self::Lease(Refusal::Stale) => "stale",
            Self::Lease(Refusal::Session) => "session",
            Self::Lease(Refusal::Held) => "held",
            Self::Lease(Refusal::NoLease) => "no-lease",
            Self::Lease(Refusal::WrongId) => "wrong-id",
            Self::Lease(Refusal::Armed) => "armed",
            Self::Pattern => "pattern",
            Self::Playing => "playing",
            Self::Unidentified => "unidentified",
            Self::NoFaults => "no-faults",
            Self::Mode => "mode",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        ERRORS.into_iter().find(|e| e.name() == s)
    }
}

/// The device's state in `hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello<'a> {
    pub version: u32,
    /// The firmware's build id (`TEST_DEVICE_BUILD` at build time).
    pub build: &'a str,
    pub mode: Mode,
    /// The held lease's number within the connection.
    pub lease: Option<u32>,
    pub armed: bool,
    pub reason: Reason,
    pub boot: Cause,
    /// Consecutive watchdog boots.
    pub count: u8,
    /// Whether the pad's endpoint is identified.
    pub identified: bool,
    /// Whether the firmware was built with `debug-faults`.
    pub faults: bool,
    /// Whether no lease is held, nothing plays, and on each endpoint
    /// nothing is held, queued, pending or owed and the last report the
    /// host took was neutral.
    pub quiet: bool,
    /// The connection's token; `None` when the boot's random read failed.
    pub session: Option<Token>,
}

/// An answer from the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer<'a> {
    Ok,
    Err(Error),
    Hello(Hello<'a>),
    /// `pong <n> <µs>`: the device's 64-bit µs, sampled after the ping was
    /// read and before the pong was queued.
    Pong {
        n: u32,
        at: u64,
    },
    /// `lease <id> <deadline µs>`: a grant's or renewal's answer.
    Leased {
        id: LeaseId,
        deadline: u64,
    },
}

impl fmt::Display for Answer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Err(e) => write!(f, "err {}", e.name()),
            Self::Hello(h) => {
                write!(
                    f,
                    "hello {} build {} mode {} lease ",
                    h.version,
                    h.build,
                    h.mode.byte()
                )?;
                match h.lease {
                    Some(n) => write!(f, "{n}")?,
                    None => f.write_str("-")?,
                }
                write!(
                    f,
                    " armed {} reason {} boot {} count {} identified {} faults {} quiet {} session ",
                    u8::from(h.armed),
                    h.reason.name(),
                    h.boot.name(),
                    h.count,
                    u8::from(h.identified),
                    u8::from(h.faults),
                    u8::from(h.quiet),
                )?;
                match h.session {
                    Some(token) => write!(f, "{token}"),
                    None => f.write_str("-"),
                }
            }
            Self::Pong { n, at } => write!(f, "pong {n} {at}"),
            Self::Leased { id, deadline } => write!(f, "lease {id} {deadline}"),
        }
    }
}

impl<'a> Answer<'a> {
    /// The answer a line's words hold; `None` for a trace record or
    /// anything else, and for a `hello` of a protocol version other than
    /// [`VERSION`]: the host refuses a device it cannot read.
    fn parse(line: &'a str) -> Option<Self> {
        let flag = |s: &str| match s {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let mut v = line.split_ascii_whitespace();
        let a = match v.next()? {
            "ok" => Self::Ok,
            "err" => Self::Err(Error::parse(v.next()?)?),
            "pong" => Self::Pong {
                n: v.next()?.parse().ok()?,
                at: v.next()?.parse().ok()?,
            },
            "lease" => Self::Leased {
                id: LeaseId::parse(v.next()?)?,
                deadline: v.next()?.parse().ok()?,
            },
            "hello" => {
                let version = v.next()?.parse().ok()?;
                if version != VERSION {
                    return None;
                }
                let mut field = |name: &str| {
                    (v.next()? == name).then_some(())?;
                    v.next()
                };
                let build = field("build")?;
                let mode = Mode::from_byte(field("mode")?.parse().ok()?)?;
                let lease = match field("lease")? {
                    "-" => None,
                    n => Some(n.parse().ok()?),
                };
                let armed = flag(field("armed")?)?;
                let reason = Reason::parse(field("reason")?)?;
                let boot = Cause::parse(field("boot")?)?;
                let count = field("count")?.parse().ok()?;
                let identified = flag(field("identified")?)?;
                let faults = flag(field("faults")?)?;
                let quiet = flag(field("quiet")?)?;
                let session = match field("session")? {
                    "-" => None,
                    token => Some(Token::parse(token)?),
                };
                Self::Hello(Hello {
                    version,
                    build,
                    mode,
                    lease,
                    armed,
                    reason,
                    boot,
                    count,
                    identified,
                    faults,
                    quiet,
                    session,
                })
            }
            _ => return None,
        };
        v.next().is_none().then_some(a)
    }

    /// The answer a line (without its line feed) holds in the one form the
    /// device writes; any other line is malformed: `None`.
    #[must_use]
    pub fn parse_canonical(line: &'a str) -> Option<Self> {
        Self::parse(line).filter(|a| writes_back(a, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{Body, Record, TrialRun};

    fn ch(control: Control, value: u8) -> Change {
        Change { control, value }
    }

    const TOKEN: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 2,
    };
    const ID: LeaseId = LeaseId { token: TOKEN, n: 4 };
    const LARGEST_TOKEN: Token = Token {
        boot: u64::MAX,
        count: u32::MAX,
    };
    const LARGEST_ID: LeaseId = LeaseId {
        token: LARGEST_TOKEN,
        n: u32::MAX,
    };

    fn commands() -> Vec<Command> {
        let item = Item::new(
            125_000,
            &[
                ch(Control::Button(1), 1),
                ch(Control::Axis, 200),
                ch(Control::Button(16), 0),
            ],
        )
        .unwrap();
        vec![
            Command::Hello,
            Command::Mode(Mode {
                rate: Rate::Hz125,
                reporting: Reporting::Continuous,
            }),
            Command::Lease {
                token: TOKEN,
                ms: 30_000,
                deadline: 5_000_000,
            },
            Command::Renew {
                id: ID,
                deadline: u64::MAX,
            },
            Command::Set {
                id: ID,
                change: ch(Control::Key(0xE1), 1),
            },
            Command::Set {
                id: ID,
                change: ch(Control::Button(16), 0),
            },
            Command::Set {
                id: ID,
                change: ch(Control::Axis, 255),
            },
            Command::Load,
            Command::Item(item),
            Command::Arm { id: ID, at: 0 },
            Command::Ping(9),
            Command::Trace(true),
            Command::Trace(false),
            Command::Flush,
            Command::Release,
            Command::Bootsel,
            Command::Detach { ms: 500 },
            Command::Fault {
                fault: Fault::Hang,
                arg: 0,
            },
            Command::Fault {
                fault: Fault::PongOffset,
                arg: -50_000,
            },
            Command::RevokeTest {
                id: ID,
                ep: Ep::Pad,
                trial: 20,
            },
            Command::RevokeTest {
                id: ID,
                ep: Ep::Keyboard,
                trial: 1,
            },
        ]
    }

    #[test]
    fn commands_round_trip_through_their_lines() {
        for c in commands() {
            let line = encode(&c).unwrap();
            assert_eq!(line.as_bytes().last(), Some(&b'\n'));
            assert_eq!(line.text(), c.to_string());
            assert_eq!(Command::parse_canonical(line.text()), Some(c), "{line:?}");
        }
    }

    #[test]
    fn malformed_commands_are_refused() {
        let id = ID.to_string();
        for line in [
            "".to_owned(),
            "hello there".to_owned(),
            format!("key {id} 128 1"),
            format!("key {id} 26 2"),
            format!("button {id} 0 1"),
            format!("button {id} 17 1"),
            "mode 500 change".to_owned(),
            "item 5".to_owned(),
            "item 5 k26+ b1+".to_owned(),
            "item 5 b1+ b1-".to_owned(),
            "item 5 b1+ b2+ b3+ b4+ b5+ b6+ b7+ b8+ b9+".to_owned(),
            "fault drop-item".to_owned(),
            "trace maybe".to_owned(),
            format!("revoke-test {id} x 1"),
            format!("revoke-test {id} p"),
            format!("revoke-test {id} k 1 2"),
        ] {
            assert_eq!(Command::parse(&line), None, "{line:?}");
        }
        assert!(Command::parse("item 5 b1+ b2+ b3+ b4+ b5+ b6+ b7+ b8+").is_some());
    }

    /// A line is a command only in the form the host writes: each framing
    /// probe case and every second form of a valid command is refused.
    #[test]
    fn only_the_canonical_form_parses() {
        for c in commands() {
            let line = c.to_string();
            assert_eq!(Command::parse_canonical(&line), Some(c), "{line}");
        }
        assert_eq!(
            Command::parse_canonical("fault pong-offset -50000"),
            Some(Command::Fault {
                fault: Fault::PongOffset,
                arg: -50_000
            })
        );
        for case in Malformed::ALL {
            let bytes = case.bytes();
            let line = bytes.strip_suffix(b"\n").unwrap();
            if let Ok(text) = core::str::from_utf8(line) {
                assert_eq!(Command::parse_canonical(text), None, "{case:?}");
            }
        }
        let token = TOKEN.to_string();
        let id = ID.to_string();
        for line in [
            "ping 007".to_owned(),
            "ping +7".to_owned(),
            "ping -0".to_owned(),
            "lease 250 5000".to_owned(),
            format!("lease {token} 0250 5000"),
            "renew 4".to_owned(),
            format!("renew {token}:4 5 6"),
            format!("renew {token}:04 5"),
            "button 1 1".to_owned(),
            format!("button {id} 1 1 "),
            format!("lease {} 250 5000", token.to_uppercase()),
            format!("lease {}0 250 5000", token),
            "fault pong-offset +50000".to_owned(),
            "fault pong-offset -0".to_owned(),
            "item 05 b1+".to_owned(),
            "item 5 b01+".to_owned(),
            "item 5 x+3".to_owned(),
        ] {
            assert_eq!(Command::parse_canonical(&line), None, "{line:?}");
        }
        // The canonical forms of the same commands parse.
        assert!(Command::parse_canonical(&format!("lease {token} 250 5000")).is_some());
        assert!(Command::parse_canonical(&format!("button {id} 1 1")).is_some());
        assert!(Command::parse_canonical("item 5 x3").is_some());
    }

    #[test]
    fn every_command_at_its_largest_fits_a_line() {
        let keys: Vec<Change> = (0..MAX_CHANGES)
            .map(|i| ch(Control::Key(100 + u8::try_from(i).unwrap()), 1))
            .collect();
        let largest = [
            Command::Mode(Mode {
                rate: Rate::Hz1000,
                reporting: Reporting::Continuous,
            }),
            Command::Lease {
                token: LARGEST_TOKEN,
                ms: u32::MAX,
                deadline: u64::MAX,
            },
            Command::Renew {
                id: LARGEST_ID,
                deadline: u64::MAX,
            },
            Command::Set {
                id: LARGEST_ID,
                change: ch(Control::Key(0xE7), 1),
            },
            Command::Set {
                id: LARGEST_ID,
                change: ch(Control::Button(16), 1),
            },
            Command::Set {
                id: LARGEST_ID,
                change: ch(Control::Axis, 255),
            },
            Command::Item(Item::new(u64::MAX, &keys).unwrap()),
            Command::Arm {
                id: LARGEST_ID,
                at: u64::MAX,
            },
            Command::Ping(u32::MAX),
            Command::Detach { ms: u32::MAX },
            Command::Fault {
                fault: Fault::PongOffset,
                arg: i64::MIN,
            },
            Command::Fault {
                fault: Fault::EveryOtherPoll,
                arg: 0,
            },
            Command::RevokeTest {
                id: LARGEST_ID,
                ep: Ep::Keyboard,
                trial: u32::MAX,
            },
        ];
        for c in largest {
            let line = encode(&c).unwrap_or_else(|e| panic!("{c}: {e}"));
            assert!(line.text().len() <= MAX_LINE, "{line:?}");
            assert_eq!(Command::parse_canonical(line.text()), Some(c), "{line:?}");
        }
    }

    #[test]
    fn only_the_lease_commands_hold_input() {
        for c in commands() {
            let word = c.to_string();
            let word = word.split(' ').next().unwrap();
            let holds = matches!(
                word,
                "lease" | "renew" | "key" | "button" | "axis" | "arm" | "revoke-test"
            );
            assert_eq!(c.holds_input(), holds, "{c}");
        }
    }

    fn hello() -> Hello<'static> {
        Hello {
            version: VERSION,
            build: "0123abcd",
            mode: Mode {
                rate: Rate::Hz125,
                reporting: Reporting::OnChange,
            },
            lease: Some(3),
            armed: true,
            reason: Reason::Unconfigured,
            boot: Cause::Watchdog,
            count: 1,
            identified: true,
            faults: true,
            quiet: false,
            session: Some(TOKEN),
        }
    }

    #[test]
    fn answers_round_trip_through_their_lines() {
        let hello = hello();
        let answers = [
            Answer::Ok,
            Answer::Err(Error::Lease(Refusal::WrongId)),
            Answer::Err(Error::Lease(Refusal::Stale)),
            Answer::Err(Error::Lease(Refusal::Session)),
            Answer::Err(Error::Unidentified),
            Answer::Err(Error::Overlong),
            Answer::Err(Error::Mode),
            Answer::Hello(hello),
            Answer::Hello(Hello {
                lease: None,
                faults: false,
                quiet: true,
                session: None,
                ..hello
            }),
            Answer::Pong { n: 7, at: u64::MAX },
            Answer::Leased {
                id: ID,
                deadline: 30_000_000,
            },
        ];
        for a in answers {
            let line = a.to_string();
            assert!(line.len() < MAX_ANSWER, "{line}");
            assert_eq!(Answer::parse_canonical(&line), Some(a), "{line}");
        }
        for e in ERRORS {
            assert_eq!(Error::parse(e.name()), Some(e));
        }
        assert_eq!(
            Answer::parse_canonical("T 1 p 2 R 3 H 4"),
            None,
            "a trace record"
        );
        assert_eq!(Answer::parse_canonical("ok then"), None);
    }

    /// The device's lines are read in the one form it writes: a second
    /// form of a valid answer is malformed.
    #[test]
    fn a_non_canonical_answer_is_malformed() {
        let hello = Answer::Hello(hello()).to_string();
        let leased = Answer::Leased {
            id: ID,
            deadline: 30_000_000,
        }
        .to_string();
        let mut bad = vec![
            "ok ".to_owned(),
            " ok".to_owned(),
            "ok\r".to_owned(),
            "err  stale".to_owned(),
            "pong 07 5".to_owned(),
            "pong 7 +5".to_owned(),
            "pong\t7 5".to_owned(),
            hello.replace(" count 1 ", " count 01 "),
            hello.replace(" faults 1 ", " faults  1 "),
            leased.replace(":4 ", ":04 "),
            leased.replace(" 30000000", " +30000000"),
        ];
        bad.push(format!("{hello} "));
        for line in &bad {
            assert_eq!(Answer::parse_canonical(line), None, "{line:?}");
        }
        // Each stands for a valid answer read leniently.
        for line in &bad {
            assert!(Answer::parse(line).is_some(), "{line:?}");
        }
        let upper = hello.replace(&TOKEN.to_string(), &TOKEN.to_string().to_uppercase());
        assert_eq!(Answer::parse_canonical(&upper), None, "{upper}");
    }

    /// Every answer and trace record at its largest fits [`MAX_ANSWER`]
    /// with its line feed, counting the `hang` field `hello` gains in
    /// protocol 3.
    #[test]
    fn every_answer_and_record_at_its_largest_fits_with_its_line_feed() {
        const HANG: &str = " hang 18446744073709551615 18446744073709551615";
        // A dirty build of the debug faults: 12 hex digits, `-dirty`,
        // `-faults`.
        let build = "0123456789ab-dirty-faults";
        assert_eq!(build.len(), 25);
        let hello = Answer::Hello(Hello {
            version: u32::MAX,
            build,
            mode: Mode {
                rate: Rate::Hz125,
                reporting: Reporting::Continuous,
            },
            lease: Some(u32::MAX),
            armed: true,
            reason: Reason::Unconfigured,
            boot: Cause::PowerOn,
            count: u8::MAX,
            identified: true,
            faults: true,
            quiet: true,
            session: Some(LARGEST_TOKEN),
        })
        .to_string();
        let mut lines = vec![
            format!("{hello}{HANG}"),
            Answer::Leased {
                id: LARGEST_ID,
                deadline: u64::MAX,
            }
            .to_string(),
            Answer::Pong {
                n: u32::MAX,
                at: u64::MAX,
            }
            .to_string(),
        ];
        lines.extend(ERRORS.map(|e| Answer::Err(e).to_string()));
        let (n, seq, at) = (u32::MAX, u32::MAX, u64::MAX);
        let ep = Ep::Keyboard;
        for body in [
            Body::Handoff {
                ep,
                seq,
                ready: at,
                handoff: at,
            },
            Body::Completion { ep, seq, at },
            Body::Revoked {
                ep,
                seq,
                available: true,
                len: u16::MAX,
                at,
            },
            Body::Unacknowledged { ep, seq, at },
            Body::Dropped { ep, seq, at },
            Body::Trial(TrialRun {
                ep,
                trial: u32::MAX,
                k0: seq,
                k1: seq,
                probe: at,
            }),
            Body::Lost { count: u32::MAX },
            Body::Pending { ep, seq },
            Body::Flushed,
        ] {
            lines.push(Record { n, body }.to_string());
        }
        for line in lines {
            assert!(line.len() < MAX_ANSWER, "{} bytes: {line}", line.len());
        }
        // The largest is longer than a command line.
        assert!(format!("{hello}{HANG}").len() >= MAX_LINE);
    }

    /// A `hello` of another protocol version is refused, so the host never
    /// reads a device whose lines mean something else.
    #[test]
    fn a_hello_of_another_version_is_refused() {
        let hello = Answer::Hello(Hello {
            version: VERSION,
            build: "b",
            mode: Mode {
                rate: Rate::Hz1000,
                reporting: Reporting::OnChange,
            },
            lease: None,
            armed: false,
            reason: Reason::None,
            boot: Cause::PowerOn,
            count: 0,
            identified: false,
            faults: false,
            quiet: true,
            session: None,
        })
        .to_string();
        assert!(Answer::parse_canonical(&hello).is_some());
        let other = hello.replacen(
            &format!("hello {VERSION} "),
            &format!("hello {} ", VERSION - 1),
            1,
        );
        assert_ne!(other, hello);
        assert_eq!(Answer::parse_canonical(&other), None);
    }

    #[test]
    fn a_token_is_24_lowercase_hex_digits() {
        assert_eq!(TOKEN.to_string(), "0123456789abcdef00000002");
        assert_eq!(Token::parse("0123456789abcdef00000002"), Some(TOKEN));
        for s in [
            "0123456789ABCDEF00000002",
            "0123456789abcdef0000002",
            "0123456789abcdef000000002",
            "+123456789abcdef00000002",
            "0123456789abcdef0000000g",
        ] {
            assert_eq!(Token::parse(s), None, "{s}");
        }
        assert_eq!(ID.to_string(), "0123456789abcdef00000002:4");
        assert_eq!(LeaseId::parse("0123456789abcdef00000002:4"), Some(ID));
    }
}
