//! The control link's protocol (docs/test-device.md "Commands"): one
//! command per line from the host, one answer per line from the device,
//! with trace records ([`crate::trace::Record`]) interleaved. Answers start
//! with a lowercase word, trace records with an uppercase letter.

use core::fmt;

use crate::boot::Cause;
use crate::faults::Fault;
use crate::lease::{Reason, Refusal};
use crate::mode::{Mode, Rate, Reporting};
use crate::trace::Ep;

/// The protocol's version, the first field of `hello`.
pub const VERSION: u32 = 1;
/// The most changes one pattern item makes in its report.
pub const MAX_CHANGES: usize = 8;
/// The longest line either side sends, line ending excluded.
pub const MAX_LINE: usize = 160;
/// The number of pad buttons.
pub const BUTTONS: u8 = 16;

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
    /// `hello`: the build, mode, lease state and last disarm reason.
    Hello,
    /// `mode <1000|125> <change|continuous>`: store the mode and reboot.
    Mode(Mode),
    /// `lease <ms>`.
    Lease { ms: u32 },
    /// `renew <id>`.
    Renew { id: u32 },
    /// `key <usage> <0|1>`, `button <n> <0|1>`, `axis <v>`: one held
    /// change, under the lease.
    Set(Change),
    /// `load`: forget the loaded pattern and start a new one.
    Load,
    /// `item <µs> <change>...`: append an item to the loaded pattern.
    Item(Item),
    /// `arm <lease id> <device µs>`: play the loaded pattern from that time.
    Arm { id: u32, at: u64 },
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
    /// `revoke-test <k|p> <trial>`: one phase-controlled revocation trial
    /// on the endpoint, numbered `trial` in its `V` record
    /// ([`crate::revoke`]).
    RevokeTest { ep: Ep, trial: u32 },
}

impl Command {
    /// The command a line holds.
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
                ms: w.next()?.parse().ok()?,
            },
            "renew" => Self::Renew {
                id: w.next()?.parse().ok()?,
            },
            "key" | "button" => {
                let n = w.next()?.parse().ok()?;
                let control = if word == "key" {
                    Control::Key(n)
                } else {
                    Control::Button(n)
                };
                if !control.valid() {
                    return None;
                }
                Self::Set(Change {
                    control,
                    value: bit(w.next()?)?,
                })
            }
            "axis" => Self::Set(Change {
                control: Control::Axis,
                value: w.next()?.parse().ok()?,
            }),
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
                id: w.next()?.parse().ok()?,
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
                ep: Ep::parse(w.next()?)?,
                trial: w.next()?.parse().ok()?,
            },
            _ => return None,
        };
        w.next().is_none().then_some(cmd)
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Hello => f.write_str("hello"),
            Self::Mode(m) => write!(f, "mode {} {}", m.rate.name(), m.reporting.name()),
            Self::Lease { ms } => write!(f, "lease {ms}"),
            Self::Renew { id } => write!(f, "renew {id}"),
            Self::Set(Change {
                control: Control::Key(u),
                value,
            }) => write!(f, "key {u} {value}"),
            Self::Set(Change {
                control: Control::Button(n),
                value,
            }) => {
                write!(f, "button {n} {value}")
            }
            Self::Set(Change {
                control: Control::Axis,
                value,
            }) => write!(f, "axis {value}"),
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
            Self::RevokeTest { ep, trial } => write!(f, "revoke-test {} {trial}", ep.name()),
        }
    }
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The line is not a command (or not UTF-8). Ends the lease.
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

impl Error {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Overlong => "overlong",
            Self::Lease(Refusal::BadLength) => "bad-length",
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
        [
            Self::Syntax,
            Self::Overlong,
            Self::Lease(Refusal::BadLength),
            Self::Lease(Refusal::Held),
            Self::Lease(Refusal::NoLease),
            Self::Lease(Refusal::WrongId),
            Self::Lease(Refusal::Armed),
            Self::Pattern,
            Self::Playing,
            Self::Unidentified,
            Self::NoFaults,
            Self::Mode,
        ]
        .into_iter()
        .find(|e| e.name() == s)
    }
}

/// The device's state in `hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello<'a> {
    pub version: u32,
    /// The firmware's build id (`TEST_DEVICE_BUILD` at build time).
    pub build: &'a str,
    pub mode: Mode,
    pub lease: Option<u32>,
    pub armed: bool,
    pub reason: Reason,
    pub boot: Cause,
    /// Consecutive watchdog boots.
    pub count: u8,
    /// Whether the pad's endpoint is identified.
    pub identified: bool,
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
    /// `lease <id> <deadline µs>`.
    Leased {
        id: u32,
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
                    Some(id) => write!(f, "{id}")?,
                    None => f.write_str("-")?,
                }
                write!(
                    f,
                    " armed {} reason {} boot {} count {} identified {}",
                    u8::from(h.armed),
                    h.reason.name(),
                    h.boot.name(),
                    h.count,
                    u8::from(h.identified),
                )
            }
            Self::Pong { n, at } => write!(f, "pong {n} {at}"),
            Self::Leased { id, deadline } => write!(f, "lease {id} {deadline}"),
        }
    }
}

impl<'a> Answer<'a> {
    /// The answer a line holds; `None` for a trace record or anything else,
    /// and for a `hello` of a protocol version other than [`VERSION`]: the
    /// host refuses a device it cannot read.
    #[must_use]
    pub fn parse(line: &'a str) -> Option<Self> {
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
                id: v.next()?.parse().ok()?,
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
                    id => Some(id.parse().ok()?),
                };
                let armed = flag(field("armed")?)?;
                let reason = Reason::parse(field("reason")?)?;
                let boot = Cause::parse(field("boot")?)?;
                let count = field("count")?.parse().ok()?;
                let identified = flag(field("identified")?)?;
                if v.next().is_some() {
                    return None;
                }
                return Some(Self::Hello(Hello {
                    version,
                    build,
                    mode,
                    lease,
                    armed,
                    reason,
                    boot,
                    count,
                    identified,
                }));
            }
            _ => return None,
        };
        v.next().is_none().then_some(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(control: Control, value: u8) -> Change {
        Change { control, value }
    }

    #[test]
    fn commands_round_trip_through_their_lines() {
        let item = Item::new(
            125_000,
            &[
                ch(Control::Button(1), 1),
                ch(Control::Axis, 200),
                ch(Control::Button(16), 0),
            ],
        )
        .unwrap();
        let commands = [
            Command::Hello,
            Command::Mode(Mode {
                rate: Rate::Hz125,
                reporting: Reporting::Continuous,
            }),
            Command::Lease { ms: 30_000 },
            Command::Renew { id: 4 },
            Command::Set(ch(Control::Key(0xE1), 1)),
            Command::Set(ch(Control::Button(16), 0)),
            Command::Set(ch(Control::Axis, 255)),
            Command::Load,
            Command::Item(item),
            Command::Arm {
                id: 2,
                at: u64::MAX,
            },
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
                ep: Ep::Pad,
                trial: 20,
            },
            Command::RevokeTest {
                ep: Ep::Keyboard,
                trial: 1,
            },
        ];
        for c in commands {
            let line = c.to_string();
            assert!(line.len() <= MAX_LINE, "{line}");
            assert_eq!(Command::parse(&line), Some(c), "{line}");
        }
    }

    #[test]
    fn malformed_commands_are_refused() {
        for line in [
            "",
            "hello there",
            "key 128 1",
            "key 26 2",
            "button 0 1",
            "button 17 1",
            "mode 500 change",
            "item 5",
            "item 5 k26+ b1+",
            "item 5 b1+ b1-",
            "item 5 b1+ b2+ b3+ b4+ b5+ b6+ b7+ b8+ b9+",
            "fault drop-item",
            "trace maybe",
            "revoke-test x 1",
            "revoke-test p",
            "revoke-test k 1 2",
        ] {
            assert_eq!(Command::parse(line), None, "{line:?}");
        }
        assert!(Command::parse("item 5 b1+ b2+ b3+ b4+ b5+ b6+ b7+ b8+").is_some());
    }

    #[test]
    fn answers_round_trip_through_their_lines() {
        let hello = Hello {
            version: VERSION,
            build: "0123abcd",
            mode: Mode {
                rate: Rate::Hz125,
                reporting: Reporting::OnChange,
            },
            lease: Some(3),
            armed: true,
            reason: Reason::Expired,
            boot: Cause::Watchdog,
            count: 1,
            identified: true,
        };
        let answers = [
            Answer::Ok,
            Answer::Err(Error::Lease(Refusal::WrongId)),
            Answer::Err(Error::Unidentified),
            Answer::Err(Error::Overlong),
            Answer::Err(Error::Mode),
            Answer::Hello(hello),
            Answer::Hello(Hello {
                lease: None,
                ..hello
            }),
            Answer::Pong { n: 7, at: u64::MAX },
            Answer::Leased {
                id: 3,
                deadline: 30_000_000,
            },
        ];
        for a in answers {
            let line = a.to_string();
            assert!(line.len() <= MAX_LINE, "{line}");
            assert_eq!(Answer::parse(&line), Some(a), "{line}");
        }
        assert_eq!(Answer::parse("T 1 p 2 R 3 H 4"), None, "a trace record");
        assert_eq!(Answer::parse("ok then"), None);
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
        })
        .to_string();
        assert!(Answer::parse(&hello).is_some());
        let other = hello.replacen(
            &format!("hello {VERSION} "),
            &format!("hello {} ", VERSION + 1),
            1,
        );
        assert_ne!(other, hello);
        assert_eq!(Answer::parse(&other), None);
    }
}
