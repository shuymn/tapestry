//! The media worker's control pipe (design.md "Threat Model" T02): the
//! commands the game sends the worker and the worker's replies.
//!
//! A message travels as a frame: its length in two little-endian bytes,
//! then that many bytes, at most [`MAX_FRAME`]: a tag byte and the
//! message's fields, integers little-endian, a string as its length in two
//! bytes and then its UTF-8 bytes. A command carries an id after its tag,
//! which the game chooses, never 0, and each reply that answers a command
//! echoes: the open's probe or refusal ([`Reply::Probed`],
//! [`Reply::Refused`]) and its rejection ([`Reply::Rejected`]), the play's
//! start, the seek's landing, the close's [`Reply::Closed`] and any
//! command's rejection, so the game follows each player through the replies
//! to its own commands. A player's failure ([`Reply::Failed`]) and the end
//! of its video answer no command. Decoding reads exactly one message's
//! bytes and refuses anything else ([`Malformed`]): an unknown tag, a field
//! cut short, bytes left over, a string that is not UTF-8 or is longer than
//! [`MAX_TEXT`], a reason the protocol does not have. The game reads the
//! worker's replies as it reads the frame ring: it trusts nothing in them,
//! and a reply it cannot decode fails the worker's players.
//!
//! A handle travels as its value in the worker, into which the game
//! duplicated it with exactly the access [`FILE_ACCESS`], [`SECTION_ACCESS`]
//! or [`EVENT_ACCESS`] names. The worker takes a value only when it holds a
//! handle of that kind with exactly that access that no other player
//! holds, and closes a player's handles when the player closes.

/// The protocol's version, the first field of [`Reply::Hello`]: the game
/// refuses a worker that names another before it hands it anything.
pub const PROTOCOL: u32 = 2;

/// The most bytes a frame holds after its length.
pub const MAX_FRAME: usize = 8_192;

/// The most bytes of a string field.
pub const MAX_TEXT: usize = 4_096;

/// The access of the file handle an [`Command::Open`] names: reading the
/// file's bytes (`FILE_READ_DATA`), and nothing else. The handle still
/// names its file: Windows gives the path of the file a handle refers to,
/// whatever the handle's access (`GetFinalPathNameByHandleW`), so the worker
/// can learn where a video lies; its AppContainer keeps it from opening the
/// user's files by that path.
pub const FILE_ACCESS: u32 = 0x0001;

/// The access of the section handle a [`Command::Play`] names: mapping the
/// frame ring for reading and writing (`SECTION_MAP_READ` and
/// `SECTION_MAP_WRITE`), and nothing else.
pub const SECTION_ACCESS: u32 = 0x0006;

/// The access of the event handle a [`Command::Play`] names: waiting on it
/// (`SYNCHRONIZE`), and nothing else.
pub const EVENT_ACCESS: u32 = 0x0010_0000;

/// A command from the game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Probe the file behind `file`, a read-only file handle, for `player`,
    /// a number the game chooses and no open player has.
    Open { player: u8, file: u64 },
    /// Decode `player`'s probed file from its start into the frame ring in
    /// `section`, as run 1, waiting on `event` while every slot holds a
    /// frame; `looping`, from the start again at each end, in the same
    /// run, the stamps rising on (it then never ends).
    Play {
        player: u8,
        section: u64,
        event: u64,
        looping: bool,
    },
    /// Start run `run` of `player` at media time `at`, in µs.
    Seek { player: u8, run: u64, at: i64 },
    /// Stop `player` and close its handles.
    Close { player: u8 },
    /// End the worker.
    Quit,
}

/// The id of a reply that answers no command of the game's (a player's
/// close at the worker's own end).
pub const NO_COMMAND: u32 = 0;

/// A reply from the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The worker is ready: the version of the protocol it speaks
    /// ([`PROTOCOL`]), its FFmpeg's version and configure line.
    Hello {
        protocol: u32,
        version: String,
        configuration: String,
    },
    /// `player`'s file holds a video of a listed container and codec, of
    /// `width` × `height` pixels, lasting `duration` µs (0 when unknown),
    /// whose published frames are in `matrix` and `range`: the answer to
    /// open `id`.
    Probed {
        id: u32,
        player: u8,
        width: u32,
        height: u32,
        duration: i64,
        matrix: Matrix,
        range: Range,
        codec: String,
        container: String,
    },
    /// `player`'s file is refused at its probe, the answer to open `id`,
    /// and closed; `code` is FFmpeg's error, 0 when FFmpeg reported none.
    /// The player holds nothing more and waits to be closed.
    Refused {
        id: u32,
        player: u8,
        why: Refusal,
        code: i32,
    },
    /// Command `id` for `player` is not taken, its handles closed: an open
    /// of a player that is open, past the players the worker holds, naming
    /// a handle it does not take, or whose thread did not start (the player
    /// is not opened); or a play, a seek or a close of a player that does
    /// not take it. `code` is the system's error, 0 when there is none.
    Rejected { id: u32, player: u8, code: i32 },
    /// `player` decodes into its frame ring: the answer to play `id`.
    Playing { id: u32, player: u8 },
    /// Run `run` of `player` starts at the frame stamped `landed`, found
    /// after `attempts` seeks: the answer to seek `id`.
    Sought {
        id: u32,
        player: u8,
        run: u64,
        landed: i64,
        attempts: u8,
    },
    /// `player` published the last frame of run `run`.
    Ended { player: u8, run: u64 },
    /// `player` failed and decodes nothing more, its file and ring closed;
    /// `code` is FFmpeg's or the system's error, 0 when there is none. It
    /// waits to be closed.
    Failed { player: u8, why: Failure, code: i32 },
    /// `player` is closed, its handles with it: the answer to close `id`
    /// ([`NO_COMMAND`] when the worker closes it at its own end).
    Closed { id: u32, player: u8 },
    /// A line of the worker's or FFmpeg's diagnostics.
    Log { text: String },
    /// The worker's control loop runs: sent at least every
    /// [`BEAT_INTERVAL_MS`], whether or not it holds players.
    Beat,
}

/// The longest the worker's control loop goes without a [`Reply::Beat`].
pub const BEAT_INTERVAL_MS: u64 = 500;

/// The colour matrix of a player's published frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.601 (also SMPTE 170M and BT.470BG).
    Bt601,
}

/// The range of a player's published frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    /// Limited: luma 16-235, chroma 16-240.
    Limited,
    /// Full: 0-255.
    Full,
}

/// Why a file is refused at its probe: an absent picture
/// (design.md "Rendering and Skins").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Its bytes could not be read.
    Read,
    /// No listed container holds it.
    Container,
    /// It holds no video stream.
    NoVideo,
    /// Its video's codec is not on the list.
    Codec,
    /// Its frames are larger than the cap.
    Size,
    /// Its decoder could not be opened.
    Decoder,
}

/// Why a player failed (design.md "Threat Model" T02).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Its frame ring could not be mapped or is not a ring.
    Ring,
    /// Its file could not be read.
    Read,
    /// An allocation was refused.
    Memory,
    /// Its decoder failed.
    Decode,
    /// A seek found no frame.
    Seek,
}

/// A message that is not one the protocol has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    /// A frame longer than [`MAX_FRAME`], or empty.
    #[error("a frame of {0} bytes")]
    Length(usize),
    /// A tag no message has.
    #[error("an unknown tag {0}")]
    Tag(u8),
    /// A field cut short.
    #[error("a message cut short")]
    Short,
    /// Bytes after the last field.
    #[error("{0} bytes after the message")]
    Trailing(usize),
    /// A string that is not UTF-8, or longer than [`MAX_TEXT`].
    #[error("a string field that is not text")]
    Text,
    /// A reason the protocol does not have.
    #[error("an unknown reason {0}")]
    Reason(u8),
}

const OPEN: u8 = 1;
const PLAY: u8 = 2;
const SEEK: u8 = 3;
const CLOSE: u8 = 4;
const QUIT: u8 = 5;

const HELLO: u8 = 64;
const PROBED: u8 = 65;
const REFUSED: u8 = 66;
const PLAYING: u8 = 67;
const SOUGHT: u8 = 68;
const ENDED: u8 = 69;
const FAILED: u8 = 70;
const CLOSED: u8 = 71;
const LOG: u8 = 72;
const REJECTED: u8 = 73;
const BEAT: u8 = 74;

/// The matrices by their code on the wire, their index.
const MATRICES: [Matrix; 2] = [Matrix::Bt709, Matrix::Bt601];
/// The ranges by their code on the wire, their index.
const RANGES: [Range; 2] = [Range::Limited, Range::Full];
/// The refusals by their code on the wire, their index.
const REFUSALS: [Refusal; 6] = [
    Refusal::Read,
    Refusal::Container,
    Refusal::NoVideo,
    Refusal::Codec,
    Refusal::Size,
    Refusal::Decoder,
];
/// The failures by their code on the wire, their index.
const FAILURES: [Failure; 5] = [
    Failure::Ring,
    Failure::Read,
    Failure::Memory,
    Failure::Decode,
    Failure::Seek,
];

/// `v`'s code on the wire: its index in `all`.
fn code<T: PartialEq>(all: &[T], v: &T) -> u8 {
    all.iter()
        .position(|a| a == v)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(u8::MAX)
}

impl Command {
    /// The message's bytes as command `id`, without the frame's length.
    #[must_use]
    pub fn encode(&self, id: u32) -> Vec<u8> {
        let mut b = Bytes::default();
        match *self {
            Self::Open { player, file } => b.u8(OPEN).u32(id).u8(player).u64(file),
            Self::Play {
                player,
                section,
                event,
                looping,
            } => b
                .u8(PLAY)
                .u32(id)
                .u8(player)
                .u64(section)
                .u64(event)
                .u8(u8::from(looping)),
            Self::Seek { player, run, at } => b.u8(SEEK).u32(id).u8(player).u64(run).i64(at),
            Self::Close { player } => b.u8(CLOSE).u32(id).u8(player),
            Self::Quit => b.u8(QUIT).u32(id),
        };
        b.0
    }

    /// The command in `message`, a frame's bytes after its length, and its
    /// id.
    ///
    /// # Errors
    ///
    /// When `message` is not exactly one command.
    pub fn decode(message: &[u8]) -> Result<(u32, Self), Malformed> {
        let mut r = Fields::new(message)?;
        let tag = r.u8()?;
        if !(OPEN..=QUIT).contains(&tag) {
            return Err(Malformed::Tag(tag));
        }
        let id = r.u32()?;
        let command = match tag {
            OPEN => Self::Open {
                player: r.u8()?,
                file: r.u64()?,
            },
            PLAY => Self::Play {
                player: r.u8()?,
                section: r.u64()?,
                event: r.u64()?,
                looping: r.flag()?,
            },
            SEEK => Self::Seek {
                player: r.u8()?,
                run: r.u64()?,
                at: r.i64()?,
            },
            CLOSE => Self::Close { player: r.u8()? },
            QUIT => Self::Quit,
            tag => return Err(Malformed::Tag(tag)),
        };
        r.end()?;
        Ok((id, command))
    }
}

impl Reply {
    /// The message's bytes, without the frame's length, which fit a frame:
    /// a string longer than [`MAX_TEXT`], or than the room the frame has
    /// left for it, is cut at a character boundary.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Bytes::default();
        match self {
            Self::Hello {
                protocol,
                version,
                configuration,
            } => b.u8(HELLO).u32(*protocol).texts(&[version, configuration]),
            Self::Probed {
                id,
                player,
                width,
                height,
                duration,
                matrix,
                range,
                codec,
                container,
            } => b
                .u8(PROBED)
                .u32(*id)
                .u8(*player)
                .u32(*width)
                .u32(*height)
                .i64(*duration)
                .u8(code(&MATRICES, matrix))
                .u8(code(&RANGES, range))
                .texts(&[codec, container]),
            Self::Refused {
                id,
                player,
                why,
                code: err,
            } => b
                .u8(REFUSED)
                .u32(*id)
                .u8(*player)
                .u8(code(&REFUSALS, why))
                .i32(*err),
            Self::Rejected {
                id,
                player,
                code: err,
            } => b.u8(REJECTED).u32(*id).u8(*player).i32(*err),
            Self::Playing { id, player } => b.u8(PLAYING).u32(*id).u8(*player),
            Self::Sought {
                id,
                player,
                run,
                landed,
                attempts,
            } => b
                .u8(SOUGHT)
                .u32(*id)
                .u8(*player)
                .u64(*run)
                .i64(*landed)
                .u8(*attempts),
            Self::Ended { player, run } => b.u8(ENDED).u8(*player).u64(*run),
            Self::Failed {
                player,
                why,
                code: err,
            } => b.u8(FAILED).u8(*player).u8(code(&FAILURES, why)).i32(*err),
            Self::Closed { id, player } => b.u8(CLOSED).u32(*id).u8(*player),
            Self::Log { text } => b.u8(LOG).texts(&[text]),
            Self::Beat => b.u8(BEAT),
        };
        b.0
    }

    /// The reply in `message`, a frame's bytes after its length.
    ///
    /// # Errors
    ///
    /// When `message` is not exactly one reply.
    pub fn decode(message: &[u8]) -> Result<Self, Malformed> {
        let mut r = Fields::new(message)?;
        let reply = match r.u8()? {
            HELLO => Self::Hello {
                protocol: r.u32()?,
                version: r.text()?,
                configuration: r.text()?,
            },
            PROBED => Self::Probed {
                id: r.u32()?,
                player: r.u8()?,
                width: r.u32()?,
                height: r.u32()?,
                duration: r.i64()?,
                matrix: r.reason(&MATRICES)?,
                range: r.reason(&RANGES)?,
                codec: r.text()?,
                container: r.text()?,
            },
            REFUSED => Self::Refused {
                id: r.u32()?,
                player: r.u8()?,
                why: r.reason(&REFUSALS)?,
                code: r.i32()?,
            },
            REJECTED => Self::Rejected {
                id: r.u32()?,
                player: r.u8()?,
                code: r.i32()?,
            },
            PLAYING => Self::Playing {
                id: r.u32()?,
                player: r.u8()?,
            },
            SOUGHT => Self::Sought {
                id: r.u32()?,
                player: r.u8()?,
                run: r.u64()?,
                landed: r.i64()?,
                attempts: r.u8()?,
            },
            ENDED => Self::Ended {
                player: r.u8()?,
                run: r.u64()?,
            },
            FAILED => Self::Failed {
                player: r.u8()?,
                why: r.reason(&FAILURES)?,
                code: r.i32()?,
            },
            CLOSED => Self::Closed {
                id: r.u32()?,
                player: r.u8()?,
            },
            LOG => Self::Log { text: r.text()? },
            BEAT => Self::Beat,
            tag => return Err(Malformed::Tag(tag)),
        };
        r.end()?;
        Ok(reply)
    }
}

/// `message` framed: its length in two little-endian bytes, then its
/// bytes. `None` when it is empty or longer than [`MAX_FRAME`].
#[must_use]
pub fn frame(message: &[u8]) -> Option<Vec<u8>> {
    if message.is_empty() || message.len() > MAX_FRAME {
        return None;
    }
    let len = u16::try_from(message.len()).ok()?;
    let mut out = Vec::with_capacity(message.len() + 2);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(message);
    Some(out)
}

/// The length a frame's first two bytes give.
///
/// # Errors
///
/// When it is 0 or more than [`MAX_FRAME`].
pub fn frame_len(prefix: [u8; 2]) -> Result<usize, Malformed> {
    let len = usize::from(u16::from_le_bytes(prefix));
    if len == 0 || len > MAX_FRAME {
        return Err(Malformed::Length(len));
    }
    Ok(len)
}

/// A message being written.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl Bytes {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Writes `strings`, the message's last fields, each cut to fit the
    /// frame: to at most [`MAX_TEXT`] bytes, and to the room the frame has
    /// left after its own length and those of the strings after it.
    fn texts(&mut self, strings: &[&str]) -> &mut Self {
        for (i, s) in strings.iter().enumerate() {
            let lengths = 2 * (strings.len() - i);
            let room = MAX_FRAME.saturating_sub(self.0.len() + lengths);
            self.text(s, room.min(MAX_TEXT));
        }
        self
    }

    /// Writes `s` cut at a character boundary to at most `max` bytes.
    fn text(&mut self, s: &str, max: usize) -> &mut Self {
        let mut end = s.len().min(max);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let cut = s.get(..end).unwrap_or_default();
        let len = u16::try_from(cut.len()).unwrap_or_default();
        self.0.extend_from_slice(&len.to_le_bytes());
        self.0.extend_from_slice(cut.as_bytes());
        self
    }
}

/// A message being read.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn new(message: &'a [u8]) -> Result<Self, Malformed> {
        if message.is_empty() || message.len() > MAX_FRAME {
            return Err(Malformed::Length(message.len()));
        }
        Ok(Self(message))
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or(Malformed::Short)?;
        self.0 = rest;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        self.take::<1>().map(|[b]| b)
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        self.take().map(u32::from_le_bytes)
    }

    fn i32(&mut self) -> Result<i32, Malformed> {
        self.take().map(i32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        self.take().map(u64::from_le_bytes)
    }

    fn i64(&mut self) -> Result<i64, Malformed> {
        self.take().map(i64::from_le_bytes)
    }

    fn text(&mut self) -> Result<String, Malformed> {
        let len = usize::from(u16::from_le_bytes(self.take()?));
        if len > MAX_TEXT {
            return Err(Malformed::Text);
        }
        let (bytes, rest) = self.0.split_at_checked(len).ok_or(Malformed::Short)?;
        self.0 = rest;
        String::from_utf8(bytes.to_vec()).map_err(|_| Malformed::Text)
    }

    fn flag(&mut self) -> Result<bool, Malformed> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(Malformed::Reason(v)),
        }
    }

    fn reason<T: Copy>(&mut self, all: &[T]) -> Result<T, Malformed> {
        let v = self.u8()?;
        all.get(usize::from(v)).copied().ok_or(Malformed::Reason(v))
    }

    fn end(self) -> Result<(), Malformed> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Malformed::Trailing(self.0.len()))
        }
    }
}
