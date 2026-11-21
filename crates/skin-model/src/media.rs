//! The frame ring a video's frames cross from the media worker to the game
//! (design.md "Rendering and Skins", video frames; "Threat Model" T02).
//!
//! The game creates one shared section a video, sizes it from the probed
//! frame ([`Geometry`]) and writes its header once ([`Reader::new`]); the
//! worker writes frames into its [`SLOTS`] slots ([`Writer`]); the render
//! thread finds the newest frame that is due ([`Reader::peek`]) and copies
//! it straight into its upload buffer, each plane's rows at the pitch the
//! texture copy asks ([`Reader::take`]). Both sides see the section as
//! 64-bit words through [`AtomicU64`]s ([`Words`]), so neither holds a
//! plain reference into memory the other may write.
//!
//! The reader trusts nothing it reads back. It keeps the geometry and the
//! run it started itself and never reads back the header words it wrote. A
//! slot it takes must hold a known state, its own index, the game's frame
//! size, a run already started, a sequence above every frame taken before
//! it, and in the current run a stamp inside the run's window that rises
//! with the sequence, and those words, read again once its frame is
//! copied, must be what they were before the copy (a change undone before
//! that second read goes unseen). Anything else is a [`Fault`], which fails
//! the video's player: the reader keeps it and reads nothing more. A copy
//! reads exactly the slot's frame, at the size the game gave it, and hands
//! each of its bytes once to the caller's sink, at an offset inside
//! [`Reader::pitched_bytes`].
//!
//! The worker publishes frames in sequence order and, after each, raises
//! the ring's watermark, the highest sequence it has published. The reader
//! reads the watermark before it looks at any slot and leaves a frame above
//! it for the next peek, so a frame published while it looks, perhaps into
//! a slot it has already passed, waits instead of overtaking an earlier one.
//! The watermark only defers: whatever value it holds, the slots' checks
//! still apply.
//!
//! Runs pass from the game to the worker by command, never through the
//! ring: the ring starts in run 1, which the worker plays from the video's
//! start once the game hands it the ring, and each later run
//! ([`Reader::start_run`]) begins at the seek that names it. The worker
//! publishes a run's frames only after the command that starts the run,
//! and once it has published a frame of a run it publishes none of an
//! earlier run: a seek drops the frame it held for the run before. So
//! every frame of an earlier run has a lower sequence than every frame of
//! the current one, and freeing one, unread, never passes over a frame the
//! game may still show.
//!
//! A worker that keeps the protocol (frames written only into slots it
//! claimed free, published once whole, in sequence order, and runs handed
//! over as above) always yields whole frames, in order, and never fails its
//! player, however its writes interleave with the game's reads. One that
//! does not cannot make the game read outside the ring or write outside the
//! region its sink takes, and what it shows stays inside its own panel: frames
//! with plausible words are shown, and the first slot that breaks a check
//! fails its player.
//!
//! A frame is shown until its successor is due: the worker publishes each
//! frame with the stamp of the frame after it (its `until`, once it has
//! decoded that frame; [`UNTIL_END`] for a video's last), and the game
//! writes the media time it shows at each peek in [`DUE_WORD`], then the
//! run it shows in [`DUE_RUN_WORD`], so the worker reads a time only of
//! the run it plays (a seek's run never drops frames by the time an
//! earlier run reached). A frame
//! whose successor is already due is behind the clock: the worker drops
//! it rather than publish it, and a peek frees one still in the ring
//! unread and finds nothing due ([`Peek::behind`]), so a frame behind the
//! clock is never shown, at a video's start, after a stall, or ever.
//!
//! The layout, in words: a header of [`HEADER_WORDS`] (a magic word, the
//! version, the slot count, the width, the height, a frame's bytes and, at
//! [`WATERMARK_WORD`], the watermark, then the worker's supervision words,
//! the decoder's progress at [`PROGRESS_WORD`] and its [`State`] at
//! [`STATE_WORD`], which the game reads only to supervise the worker
//! ([`supervision`]), then the game's due time at [`DUE_WORD`] and its
//! run at [`DUE_RUN_WORD`]), then the
//! slots, each [`SLOT_HEADER_WORDS`] (its state, sequence, run, stamp, its
//! own index, its frame's bytes and its `until`) followed by its frame: the
//! luma plane, then
//! the two chroma planes, each half the width and height rounded up (I420),
//! packed little-endian into words, the last word's spare bytes zero.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub mod control;

use std::sync::atomic::{AtomicU64, Ordering, fence};

/// The slots of a ring: the frames the worker may publish ahead of the
/// render thread.
pub const SLOTS: usize = 4;

/// The largest frame, in pixels (4 Mi): the media worker refuses a larger
/// one at its header ("Threat Model" T02), and a ring has no slot for it.
pub const MAX_PIXELS: u64 = 4_194_304;

/// The longest side of a frame.
pub const MAX_SIDE: u32 = 8_192;

/// The words of a ring's header.
pub const HEADER_WORDS: usize = 16;

/// The words before each slot's frame.
pub const SLOT_HEADER_WORDS: usize = 8;

/// The words a copy reads, or a write writes, at a time.
pub const CHUNK_WORDS: usize = 8_192;

/// The layout's version, in the header.
pub const VERSION: u64 = 3;

/// The header's first word.
const MAGIC: u64 = u64::from_le_bytes(*b"TAPRING1");

/// The header's words.
const H_MAGIC: usize = 0;
const H_VERSION: usize = 1;
const H_SLOTS: usize = 2;
const H_WIDTH: usize = 3;
const H_HEIGHT: usize = 4;
const H_BYTES: usize = 5;

/// The header word holding the watermark: the highest sequence the worker
/// has published, which it raises after each publication and the game reads
/// before each scan of the slots.
pub const WATERMARK_WORD: usize = 6;

/// The header word holding the decoder's progress: the frames its decoder
/// has returned, raised for each before it is converted or skipped. The
/// worker writes it; the game reads it only to judge whether the player
/// stalls ("Threat Model" T02).
pub const PROGRESS_WORD: usize = 7;

/// The header word holding the player's [`State`], which the worker
/// writes and the game reads only to supervise it.
pub const STATE_WORD: usize = 8;

/// The header word holding the media time the game showed last (µs, as
/// a two's-complement `i64`): the game writes it at each peek, before it
/// looks at the slots, and never reads it back; the worker reads it to drop
/// a frame whose successor is already due rather than publish it.
/// [`NOTHING_DUE`] until the first peek.
pub const DUE_WORD: usize = 9;

/// The header word holding the run of the time in [`DUE_WORD`]: the game
/// writes it after that time at each peek, 0 until the first.
pub const DUE_RUN_WORD: usize = 10;

/// [`DUE_WORD`]'s value before the game's first peek: no frame is due.
pub const NOTHING_DUE: i64 = i64::MIN;

/// The `until` of a video's last frame: no frame follows it.
pub const UNTIL_END: i64 = i64::MAX;

/// The media time the game showed last in run `run`, from `ring`'s
/// [`DUE_WORD`]; `None` when the game's last peek was of another run (or
/// none yet) or the ring does not hold the header. The game writes the
/// time before its run, so a run read here is never older than the time
/// read after it.
#[must_use]
pub fn shown<W: Words + ?Sized>(ring: &W, run: u64) -> Option<i64> {
    if word(ring, DUE_RUN_WORD)?.load(Ordering::Acquire) != run {
        return None;
    }
    Some(word(ring, DUE_WORD)?.load(Ordering::Acquire).cast_signed())
}

/// Whether a frame shown `until` its successor's stamp is behind the media
/// time `due`: its successor is due, so it is not the frame due. A video's
/// last frame ([`UNTIL_END`]) has no successor and is never behind.
#[must_use]
pub const fn behind(until: i64, due: i64) -> bool {
    until != UNTIL_END && until <= due
}

/// What a player is doing, as its worker says in [`STATE_WORD`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Decoding: its progress is to rise.
    Decoding,
    /// Waiting for the game to free a slot: every slot holds a frame.
    Waiting,
    /// Its video ended, for good (a looping play never ends).
    Ended,
}

impl State {
    /// The state's value in its word.
    #[must_use]
    pub const fn word(self) -> u64 {
        match self {
            Self::Decoding => 1,
            Self::Waiting => 2,
            Self::Ended => 3,
        }
    }

    /// The state `word` holds; `None` for a value no state has (0 before
    /// the worker wrote one).
    #[must_use]
    pub const fn of(word: u64) -> Option<Self> {
        match word {
            1 => Some(Self::Decoding),
            2 => Some(Self::Waiting),
            3 => Some(Self::Ended),
            _ => None,
        }
    }
}

/// What the worker last wrote in `ring`'s supervision words: its decoder's
/// progress and its state (`None` before it wrote one, or for a value no
/// state has). Untrusted, as every word the worker writes; `None` when the
/// ring does not hold the header.
#[must_use]
pub fn supervision<W: Words + ?Sized>(ring: &W) -> Option<(u64, Option<State>)> {
    let progress = word(ring, PROGRESS_WORD)?.load(Ordering::Acquire);
    let state = word(ring, STATE_WORD)?.load(Ordering::Acquire);
    Some((progress, State::of(state)))
}

/// The highest sequence the worker says it has published: `ring`'s
/// watermark, untrusted; `None` when the ring does not hold the header.
#[must_use]
pub fn watermark<W: Words + ?Sized>(ring: &W) -> Option<u64> {
    Some(word(ring, WATERMARK_WORD)?.load(Ordering::Acquire))
}

/// A slot's words before its frame.
const S_STATE: usize = 0;
const S_SEQ: usize = 1;
const S_RUN: usize = 2;
const S_STAMP: usize = 3;
const S_INDEX: usize = 4;
const S_BYTES: usize = 5;
const S_UNTIL: usize = 6;

/// A slot holding no frame, which the worker may claim.
const FREE: u64 = 0;
/// A slot the worker claimed and is writing.
const WRITING: u64 = 1;
/// A slot holding a published frame, the reader's until it frees it.
const READY: u64 = 2;

/// A ring's memory: its words, each read and written atomically.
pub trait Words {
    /// The words `start..start + len`, or `None` when the memory does not
    /// hold them all.
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]>;
}

impl Words for [AtomicU64] {
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]> {
        self.get(start..start.checked_add(len)?)
    }
}

/// The word `i` of `ring`.
fn word<W: Words + ?Sized>(ring: &W, i: usize) -> Option<&AtomicU64> {
    ring.words(i, 1)?.first()
}

/// `n` as a word's value.
fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The frame size of a ring, which the game fixes from the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    width: u32,
    height: u32,
}

impl Geometry {
    /// The geometry of `width` × `height` frames, or `None` for an empty
    /// one or one past [`MAX_PIXELS`] or [`MAX_SIDE`].
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return None;
        }
        if width as u64 * height as u64 > MAX_PIXELS {
            return None;
        }
        Some(Self { width, height })
    }

    /// A frame's width in pixels.
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// A frame's height in pixels.
    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// A frame's bytes: the luma plane, then the two chroma planes, each
    /// half the width and height rounded up (I420): 6 MiB at 4 Mi pixels of
    /// even sides, a little more at odd ones. The section holds four of them
    /// and the headers, [`Geometry::bytes`] in all.
    #[must_use]
    pub const fn frame_bytes(self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
    }

    /// A frame's words.
    #[must_use]
    pub const fn frame_words(self) -> usize {
        self.frame_bytes().div_ceil(8)
    }

    /// The first word of slot `slot`.
    const fn slot_start(self, slot: usize) -> usize {
        HEADER_WORDS + slot * (SLOT_HEADER_WORDS + self.frame_words())
    }

    /// The first word of slot `slot`'s frame.
    #[must_use]
    pub const fn frame_start(self, slot: usize) -> usize {
        self.slot_start(slot) + SLOT_HEADER_WORDS
    }

    /// The words of the whole ring, the size of the section that holds it.
    #[must_use]
    pub const fn words(self) -> usize {
        self.slot_start(SLOTS)
    }

    /// The bytes of the section that holds the ring.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.words() * 8
    }
}

/// The stamps a run's frames may carry, inclusive, in µs of media time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub first: i64,
    pub last: i64,
}

impl Window {
    const fn holds(self, stamp: i64) -> bool {
        self.first <= stamp && stamp <= self.last
    }
}

/// A frame the reader took: its sequence and its stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub seq: u64,
    pub stamp: i64,
}

/// What a peek found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Peek {
    /// The frame due, if any, for [`Reader::take`].
    pub due: Option<Due>,
    /// Whether it freed slots (of earlier runs, or behind the clock): the
    /// game then signals the worker, which may be waiting for one.
    pub freed: bool,
    /// Whether the newest frame at or before the time was behind it (its
    /// successor due) and was freed unread with the frames before it: no
    /// frame is due, and the game shows none of this video until one is.
    pub behind: bool,
}

/// A frame [`Reader::peek`] found due, which [`Reader::take`] copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due {
    chosen: Found,
    /// The current run's frames up to it, freed once it is taken.
    up_to: [Option<Found>; SLOTS],
}

impl Due {
    /// The frame's stamp.
    #[must_use]
    pub const fn stamp(&self) -> i64 {
        self.chosen.stamp
    }

    /// The frame's sequence.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.chosen.seq
    }
}

/// What failed a video's player: a slot that breaks the protocol, named by
/// its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    /// The section holds fewer words than the ring's geometry.
    #[error("the ring's section is smaller than its geometry")]
    Section,
    /// A state the protocol has not.
    #[error("slot {slot}: unknown state {value}")]
    State { slot: u8, value: u64 },
    /// Another slot's index.
    #[error("slot {slot}: names index {value}")]
    Index { slot: u8, value: u64 },
    /// A frame size other than the game's.
    #[error("slot {slot}: a frame of {value} bytes")]
    Size { slot: u8, value: u64 },
    /// A run the game has not started.
    #[error("slot {slot}: run {value}, not started")]
    Run { slot: u8, value: u64 },
    /// A sequence that does not follow: not above every frame taken
    /// before, shared with another slot, or in the current run with a stamp
    /// that does not rise with it.
    #[error("slot {slot}: sequence {value} out of order")]
    Sequence { slot: u8, value: u64 },
    /// A stamp outside the run's window.
    #[error("slot {slot}: stamp {value} outside the run")]
    Stamp { slot: u8, value: i64 },
    /// An `until` not above the frame's own stamp.
    #[error("slot {slot}: shown until {value}, not after its stamp")]
    Until { slot: u8, value: i64 },
    /// A slot that changed while its frame was copied.
    #[error("slot {slot}: changed while it was read")]
    Changed { slot: u8 },
}

/// Why a ring could not be set up.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Setup {
    /// The section holds fewer words than the geometry's ring.
    #[error("the section holds fewer than the ring's {needs} words")]
    Section { needs: usize },
}

/// A published frame as the reader found it in its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Found {
    slot: usize,
    seq: u64,
    run: u64,
    stamp: i64,
    until: i64,
}

impl Found {
    fn index(&self) -> u8 {
        u8::try_from(self.slot).unwrap_or(u8::MAX)
    }
}

/// The ring words a take loads at a time into its stack buffer, before it
/// hands their bytes to the sink.
const COPY_WORDS: usize = 512;

/// A plane of a frame as a take lays it out: its first byte in the frame
/// and in the sink, its rows' bytes and the sink's pitch between them.
#[derive(Clone, Copy)]
struct Plane {
    frame: usize,
    sink: usize,
    row: usize,
    pitch: usize,
}

/// The game's side of a ring: its geometry, the run it started last and
/// what it has taken.
#[derive(Debug)]
pub struct Reader {
    geometry: Geometry,
    run: u64,
    window: Window,
    /// The highest sequence taken out of the ring, 0 before the first.
    taken: u64,
    /// The slots it has freed, over the ring's life.
    freed: u64,
    /// The stamp of the last frame taken in the current run.
    last_stamp: Option<i64>,
    /// The frame the last peek found due, until it is taken or a run
    /// starts: the only one a take copies.
    peeked: Option<Found>,
    fault: Option<Fault>,
}

impl Reader {
    /// Sets up a ring of `geometry` in `ring`: writes its header, every
    /// slot free and a watermark of 0, and starts run 1, whose frames are
    /// stamped inside `window`. The game does this at preparation, before it
    /// hands the section to the worker, whose frames are run 1's until a
    /// seek names another run.
    ///
    /// # Errors
    ///
    /// When `ring` holds fewer words than the geometry needs.
    pub fn new<W: Words + ?Sized>(
        ring: &W,
        geometry: Geometry,
        window: Window,
    ) -> Result<Self, Setup> {
        let needs = geometry.words();
        if ring.words(0, needs).is_none() {
            return Err(Setup::Section { needs });
        }
        let header = [
            (H_MAGIC, MAGIC),
            (H_VERSION, VERSION),
            (H_SLOTS, wide(SLOTS)),
            (H_WIDTH, u64::from(geometry.width)),
            (H_HEIGHT, u64::from(geometry.height)),
            (H_BYTES, wide(geometry.frame_bytes())),
            (WATERMARK_WORD, 0),
            (PROGRESS_WORD, 0),
            (STATE_WORD, 0),
            (DUE_WORD, NOTHING_DUE.cast_unsigned()),
            (DUE_RUN_WORD, 0),
        ];
        for (i, v) in header {
            if let Some(w) = word(ring, i) {
                w.store(v, Ordering::Relaxed);
            }
        }
        for slot in 0..SLOTS {
            if let Some(w) = word(ring, geometry.slot_start(slot) + S_STATE) {
                w.store(FREE, Ordering::Relaxed);
            }
        }
        fence(Ordering::Release);
        Ok(Self {
            geometry,
            run: 1,
            window,
            taken: 0,
            freed: 0,
            last_stamp: None,
            peeked: None,
            fault: None,
        })
    }

    /// The ring's geometry.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// The run the game started last.
    #[must_use]
    pub fn run(&self) -> u64 {
        self.run
    }

    /// The slots it has freed over the ring's life: the game's own count,
    /// which its supervision of the worker compares with the frames the
    /// worker says it published ("Threat Model" T02).
    #[must_use]
    pub fn freed(&self) -> u64 {
        self.freed
    }

    /// The fault that failed the player, if one has.
    #[must_use]
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Starts the next run, whose frames are stamped inside `window`, and
    /// returns its id for the seek that hands it to the worker. No frame of
    /// an earlier run is taken any more, a [`Due`] peeked before included:
    /// the next peek frees those still in the ring unread.
    pub fn start_run(&mut self, window: Window) -> u64 {
        self.run = self.run.saturating_add(1);
        self.window = window;
        self.last_stamp = None;
        self.peeked = None;
        self.run
    }

    /// The bytes a take at `pitch` writes through its sink: the luma rows at
    /// `pitch[0]`, then each chroma plane's rows at `pitch[1]`.
    #[must_use]
    pub fn pitched_bytes(&self, pitch: [usize; 2]) -> usize {
        let [luma, chroma] = pitch;
        let h = self.geometry.height as usize;
        luma.saturating_mul(h)
            .saturating_add(chroma.saturating_mul(h.div_ceil(2)).saturating_mul(2))
    }

    /// Reads the ring at `due`, the media time the screen shows: frees the
    /// frames of earlier runs unread and finds the newest frame of the run
    /// stamped at or before `due`, which [`Self::take`] then copies. A frame
    /// not due yet stays in its slot, and so does one above the watermark
    /// read before the scan, until a later peek.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that fails the player. The reader keeps it: every later
    /// peek or take returns it without reading the ring.
    pub fn peek<W: Words + ?Sized>(&mut self, ring: &W, due: i64) -> Result<Peek, Fault> {
        if let Some(f) = self.fault {
            return Err(f);
        }
        if let Some(w) = word(ring, DUE_WORD) {
            w.store(due.cast_unsigned(), Ordering::Release);
        }
        if let Some(w) = word(ring, DUE_RUN_WORD) {
            w.store(self.run, Ordering::Release);
        }
        let peeked = self.scan(ring, due);
        match peeked {
            Ok(p) => self.peeked = p.due.map(|d| d.chosen),
            Err(f) => {
                self.peeked = None;
                self.fault = Some(f);
            }
        }
        peeked
    }

    /// Copies the frame `due` names into `sink`, each plane's rows at its
    /// pitch, then frees it with the run's frames before it. The luma rows
    /// go at `pitch[0]`, then the two chroma planes' rows at `pitch[1]`, the
    /// planes back to back from offset 0. `sink(offset, bytes)` takes a run
    /// of bytes, never past the end of a row, and returns whether it wrote
    /// them; it is asked for nothing at or past
    /// [`Self::pitched_bytes`]`(pitch)` and never for a row's padding. The
    /// ring's words are loaded [`CHUNK_WORDS`] at a time, and the slot's
    /// words are read again after the copy, as the module says: what the
    /// sink took is the frame only when this returns `Ok`. A take frees at
    /// least the frame's slot, so the game then signals the worker.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that fails the player, which the reader keeps as
    /// [`Self::peek`] does. [`Fault::Section`] when a pitch is below its
    /// plane's width or `sink` refuses bytes, and [`Fault::Changed`] when
    /// `due` is not the frame the last peek found due, or a run started or
    /// a take took it since.
    pub fn take<W: Words + ?Sized>(
        &mut self,
        ring: &W,
        due: Due,
        pitch: [usize; 2],
        sink: &mut dyn FnMut(usize, &[u8]) -> bool,
    ) -> Result<Frame, Fault> {
        if let Some(f) = self.fault {
            return Err(f);
        }
        let taken = self.take_due(ring, &due, pitch, sink);
        if let Err(f) = taken {
            self.fault = Some(f);
        }
        taken
    }

    fn take_due<W: Words + ?Sized>(
        &mut self,
        ring: &W,
        due: &Due,
        pitch: [usize; 2],
        sink: &mut dyn FnMut(usize, &[u8]) -> bool,
    ) -> Result<Frame, Fault> {
        let c = &due.chosen;
        if self.peeked.take() != Some(*c) {
            return Err(Fault::Changed { slot: c.index() });
        }
        self.copy(ring, c, pitch, sink)?;
        for a in due.up_to.iter().flatten() {
            self.free(ring, a)?;
        }
        self.last_stamp = Some(c.stamp);
        Ok(Frame {
            seq: c.seq,
            stamp: c.stamp,
        })
    }

    fn scan<W: Words + ?Sized>(&mut self, ring: &W, due: i64) -> Result<Peek, Fault> {
        let peek_due = due;
        // Every frame at or below it was published before this load, so the
        // scan finds each one still in its slot: none is passed over while a
        // later one is taken.
        let committed = word(ring, WATERMARK_WORD)
            .ok_or(Fault::Section)?
            .load(Ordering::Acquire);
        let mut found = [None; SLOTS];
        for (slot, f) in found.iter_mut().enumerate() {
            *f = self.look(ring, slot)?;
        }
        let mut seen = found.iter().flatten();
        while let Some(a) = seen.next() {
            let shared = seen.clone().any(|b| b.seq == a.seq);
            if a.seq <= self.taken || shared {
                return Err(Fault::Sequence {
                    slot: a.index(),
                    value: a.seq,
                });
            }
        }
        let run = self.run;
        let current = || found.iter().flatten().filter(move |a| a.run == run);
        for a in current() {
            if !self.window.holds(a.stamp) {
                return Err(Fault::Stamp {
                    slot: a.index(),
                    value: a.stamp,
                });
            }
            let after_taken = self.last_stamp.is_none_or(|s| a.stamp > s);
            let rises = current().all(|b| b.seq <= a.seq || b.stamp > a.stamp);
            if !after_taken || !rises {
                return Err(Fault::Sequence {
                    slot: a.index(),
                    value: a.seq,
                });
            }
        }
        let mut freed = false;
        for a in found
            .iter()
            .flatten()
            .filter(|a| a.run < run && a.seq <= committed)
        {
            self.free(ring, a)?;
            freed = true;
        }
        let mut due = current()
            .filter(|a| a.seq <= committed && a.stamp <= due)
            .max_by_key(|a| a.seq)
            .copied()
            .map(|chosen| {
                let mut up_to = [None; SLOTS];
                for (u, a) in up_to.iter_mut().zip(&found) {
                    *u = a.filter(|a| a.run == run && a.seq <= chosen.seq);
                }
                Due { chosen, up_to }
            });
        // Its successor due: behind the clock, freed unread with the frames
        // before it, and nothing is due.
        let behind = due
            .as_ref()
            .is_some_and(|d| behind(d.chosen.until, peek_due));
        if behind && let Some(d) = due.take() {
            for a in d.up_to.iter().flatten() {
                self.free(ring, a)?;
            }
            freed = true;
        }
        Ok(Peek { due, freed, behind })
    }

    /// Slot `slot`'s published frame, checked, or `None` while it holds
    /// none.
    fn look<W: Words + ?Sized>(&self, ring: &W, slot: usize) -> Result<Option<Found>, Fault> {
        let base = self.geometry.slot_start(slot);
        let load = |i: usize| {
            word(ring, base + i)
                .map(|w| w.load(Ordering::Relaxed))
                .ok_or(Fault::Section)
        };
        let state = word(ring, base + S_STATE)
            .ok_or(Fault::Section)?
            .load(Ordering::Acquire);
        let index = u8::try_from(slot).unwrap_or(u8::MAX);
        match state {
            FREE | WRITING => return Ok(None),
            READY => {}
            value => return Err(Fault::State { slot: index, value }),
        }
        let value = load(S_INDEX)?;
        if value != wide(slot) {
            return Err(Fault::Index { slot: index, value });
        }
        let value = load(S_BYTES)?;
        if value != wide(self.geometry.frame_bytes()) {
            return Err(Fault::Size { slot: index, value });
        }
        let run = load(S_RUN)?;
        if run == 0 || run > self.run {
            return Err(Fault::Run {
                slot: index,
                value: run,
            });
        }
        let stamp = load(S_STAMP)?.cast_signed();
        let until = load(S_UNTIL)?.cast_signed();
        if until <= stamp {
            return Err(Fault::Until {
                slot: index,
                value: until,
            });
        }
        Ok(Some(Found {
            slot,
            seq: load(S_SEQ)?,
            run,
            stamp,
            until,
        }))
    }

    /// Copies `c`'s frame into `sink` at `pitch`, word by word through a
    /// stack buffer, then reads its slot's words again and checks they still
    /// name that frame: the same state, sequence, run, stamp, index and frame
    /// size. A change undone before this second read goes unseen.
    fn copy<W: Words + ?Sized>(
        &self,
        ring: &W,
        c: &Found,
        pitch: [usize; 2],
        sink: &mut dyn FnMut(usize, &[u8]) -> bool,
    ) -> Result<(), Fault> {
        let (w, h) = (self.geometry.width as usize, self.geometry.height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let [luma, chroma] = pitch;
        if luma < w || chroma < cw {
            return Err(Fault::Section);
        }
        // Every offset the sink is given lies below the last plane's end,
        // which must not overflow.
        let u = luma.checked_mul(h).ok_or(Fault::Section)?;
        let plane = chroma.checked_mul(ch).ok_or(Fault::Section)?;
        let v = u.checked_add(plane).ok_or(Fault::Section)?;
        v.checked_add(plane).ok_or(Fault::Section)?;
        let planes = [
            Plane {
                frame: 0,
                sink: 0,
                row: w,
                pitch: luma,
            },
            Plane {
                frame: w * h,
                sink: u,
                row: cw,
                pitch: chroma,
            },
            Plane {
                frame: w * h + cw * ch,
                sink: v,
                row: cw,
                pitch: chroma,
            },
        ];
        let total = self.geometry.frame_bytes();
        let mut buffer = [[0_u8; 8]; COPY_WORDS];
        let mut at = self.geometry.frame_start(c.slot);
        let mut left = self.geometry.frame_words();
        // The frame's bytes handed to the sink so far.
        let mut done = 0;
        while left > 0 {
            let len = left.min(CHUNK_WORDS);
            let src = ring.words(at, len).ok_or(Fault::Section)?;
            if src.len() != len {
                return Err(Fault::Section);
            }
            for words in src.chunks(COPY_WORDS) {
                for (d, s) in buffer.iter_mut().zip(words) {
                    *d = s.load(Ordering::Relaxed).to_le_bytes();
                }
                // The last word's spare bytes are not the frame's.
                let n = (words.len() * 8).min(total - done);
                let bytes = buffer.as_flattened().get(..n).ok_or(Fault::Section)?;
                scatter(&planes, done, bytes, sink)?;
                done += n;
            }
            at += len;
            left -= len;
        }
        fence(Ordering::Acquire);
        let base = self.geometry.slot_start(c.slot);
        let load = |i: usize| word(ring, base + i).map(|w| w.load(Ordering::Relaxed));
        let same = load(S_STATE) == Some(READY)
            && load(S_SEQ) == Some(c.seq)
            && load(S_RUN) == Some(c.run)
            && load(S_STAMP) == Some(c.stamp.cast_unsigned())
            && load(S_UNTIL) == Some(c.until.cast_unsigned())
            && load(S_INDEX) == Some(wide(c.slot))
            && load(S_BYTES) == Some(wide(self.geometry.frame_bytes()));
        if same {
            Ok(())
        } else {
            Err(Fault::Changed { slot: c.index() })
        }
    }

    /// Frees `a`'s slot for the worker.
    fn free<W: Words + ?Sized>(&mut self, ring: &W, a: &Found) -> Result<(), Fault> {
        word(ring, self.geometry.slot_start(a.slot) + S_STATE)
            .ok_or(Fault::Section)?
            .store(FREE, Ordering::Release);
        self.taken = self.taken.max(a.seq);
        self.freed = self.freed.saturating_add(1);
        Ok(())
    }
}

/// Hands `bytes`, the frame's from byte `from`, to `sink`, one run per row
/// they cross, each at its offset in `planes`' layout.
fn scatter(
    planes: &[Plane; 3],
    from: usize,
    bytes: &[u8],
    sink: &mut dyn FnMut(usize, &[u8]) -> bool,
) -> Result<(), Fault> {
    let mut rest = bytes;
    let mut b = from;
    while !rest.is_empty() {
        let p = planes
            .iter()
            .rev()
            .find(|p| p.frame <= b)
            .ok_or(Fault::Section)?;
        let i = b - p.frame;
        let (row, x) = (i / p.row, i % p.row);
        let (run, after) = rest.split_at(rest.len().min(p.row - x));
        if !sink(p.sink + row * p.pitch + x, run) {
            return Err(Fault::Section);
        }
        b += run.len();
        rest = after;
    }
    Ok(())
}

/// Why a worker refused a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// Not a ring of this layout: its magic word or version differs.
    #[error("not a frame ring of version {VERSION}")]
    Layout,
    /// A slot count, frame size or frame bytes no ring has.
    #[error("a frame ring of no valid geometry")]
    Geometry,
    /// A section smaller than its header's ring.
    #[error("a section smaller than its ring")]
    Section,
    /// A frame of another size than the ring's.
    #[error("a frame of {0} bytes for this ring")]
    Frame(usize),
}

/// A slot the worker claimed, to publish one frame in.
#[derive(Debug, PartialEq, Eq)]
pub struct Slot(usize);

impl Slot {
    /// The slot's index.
    #[must_use]
    pub fn index(&self) -> usize {
        self.0
    }
}

/// The worker's side of a ring: the geometry its header gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Writer {
    geometry: Geometry,
}

impl Writer {
    /// The ring the game set up in `ring`, from its header.
    ///
    /// # Errors
    ///
    /// When the header is not a ring's of this layout, or `ring` holds
    /// fewer words than it says.
    pub fn attach<W: Words + ?Sized>(ring: &W) -> Result<Self, Refused> {
        let load = |i: usize| {
            word(ring, i)
                .map(|w| w.load(Ordering::Acquire))
                .ok_or(Refused::Section)
        };
        if load(H_MAGIC)? != MAGIC || load(H_VERSION)? != VERSION {
            return Err(Refused::Layout);
        }
        let side = |i| load(i).map(|v| u32::try_from(v).unwrap_or(0));
        let geometry = Geometry::new(side(H_WIDTH)?, side(H_HEIGHT)?).ok_or(Refused::Geometry)?;
        if load(H_SLOTS)? != wide(SLOTS) || load(H_BYTES)? != wide(geometry.frame_bytes()) {
            return Err(Refused::Geometry);
        }
        if ring.words(0, geometry.words()).is_none() {
            return Err(Refused::Section);
        }
        Ok(Self { geometry })
    }

    /// The ring's geometry.
    #[must_use]
    pub fn geometry(self) -> Geometry {
        self.geometry
    }

    /// Raises the decoder's progress by one: a frame its decoder returned.
    pub fn progress<W: Words + ?Sized>(self, ring: &W) {
        if let Some(w) = word(ring, PROGRESS_WORD) {
            w.fetch_add(1, Ordering::Release);
        }
    }

    /// Says the player is in `state`.
    pub fn state<W: Words + ?Sized>(self, ring: &W, state: State) {
        if let Some(w) = word(ring, STATE_WORD) {
            w.store(state.word(), Ordering::Release);
        }
    }

    /// A free slot, claimed for the next frame, or `None` while every slot
    /// holds a frame: the worker then waits for the game to free one.
    #[must_use]
    pub fn claim<W: Words + ?Sized>(self, ring: &W) -> Option<Slot> {
        (0..SLOTS).find_map(|slot| {
            let state = word(ring, self.geometry.slot_start(slot) + S_STATE)?;
            (state.load(Ordering::Acquire) == FREE).then(|| {
                state.store(WRITING, Ordering::Relaxed);
                Slot(slot)
            })
        })
    }

    /// Publishes `frame` in `slot` as sequence `seq` of run `run`, stamped
    /// `stamp`, its successor not known ([`UNTIL_END`]): as
    /// [`publish_until`](Self::publish_until).
    ///
    /// # Errors
    ///
    /// As [`publish_until`](Self::publish_until).
    pub fn publish<W: Words + ?Sized>(
        self,
        ring: &W,
        slot: Slot,
        frame: &[u8],
        seq: u64,
        run: u64,
        stamp: i64,
    ) -> Result<(), Refused> {
        self.publish_until(ring, slot, frame, seq, run, stamp, UNTIL_END)
    }

    /// Publishes `frame` in `slot` as sequence `seq` of run `run`, stamped
    /// `stamp` and shown until `until`, the next frame's stamp
    /// ([`UNTIL_END`] for the video's last): writes its bytes and then the
    /// slot's words, marks it ready
    /// once all are written, and then raises the watermark to `seq`. Frames
    /// are published in sequence order, a run's after the command that
    /// starts it and never one of an earlier run after it (the module's
    /// handover of runs); a publication never moves the watermark back.
    ///
    /// # Errors
    ///
    /// When `frame` is not the ring's frame size, or the section does not
    /// hold the slot; the slot stays claimed.
    #[expect(
        clippy::too_many_arguments,
        reason = "a frame's slot words, each its own field of the protocol"
    )]
    pub fn publish_until<W: Words + ?Sized>(
        self,
        ring: &W,
        slot: Slot,
        frame: &[u8],
        seq: u64,
        run: u64,
        stamp: i64,
        until: i64,
    ) -> Result<(), Refused> {
        if frame.len() != self.geometry.frame_bytes() {
            return Err(Refused::Frame(frame.len()));
        }
        let mut at = self.geometry.frame_start(slot.0);
        for bytes in frame.chunks(CHUNK_WORDS * 8) {
            let dst = ring
                .words(at, bytes.len().div_ceil(8))
                .ok_or(Refused::Section)?;
            let (whole, rest) = bytes.as_chunks::<8>();
            for (d, b) in dst.iter().zip(whole) {
                d.store(u64::from_le_bytes(*b), Ordering::Relaxed);
            }
            if let Some(d) = dst.get(whole.len())
                && !rest.is_empty()
            {
                let mut le = [0; 8];
                for (l, b) in le.iter_mut().zip(rest) {
                    *l = *b;
                }
                d.store(u64::from_le_bytes(le), Ordering::Relaxed);
            }
            at += dst.len();
        }
        let base = self.geometry.slot_start(slot.0);
        let fields = [
            (S_SEQ, seq),
            (S_RUN, run),
            (S_STAMP, stamp.cast_unsigned()),
            (S_INDEX, wide(slot.0)),
            (S_BYTES, wide(frame.len())),
            (S_UNTIL, until.cast_unsigned()),
        ];
        for (i, v) in fields {
            word(ring, base + i)
                .ok_or(Refused::Section)?
                .store(v, Ordering::Relaxed);
        }
        word(ring, base + S_STATE)
            .ok_or(Refused::Section)?
            .store(READY, Ordering::Release);
        word(ring, WATERMARK_WORD)
            .ok_or(Refused::Section)?
            .fetch_max(seq, Ordering::Release);
        Ok(())
    }
}
