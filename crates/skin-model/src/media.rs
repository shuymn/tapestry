//! The frame ring a video's frames cross from the media worker to the game
//! (design.md "Rendering and Skins", video frames; "Threat Model" T02).
//!
//! The game creates one shared section a video, sizes it from the probed
//! frame ([`Geometry`]) and writes its header once ([`Reader::new`]); the
//! worker writes frames into its [`SLOTS`] slots ([`Writer`]); the render
//! thread copies the newest frame that is due ([`Reader::poll`]). Both
//! sides see the section as 64-bit words through [`AtomicU64`]s
//! ([`Words`]), so neither holds a plain reference into memory the other
//! may write.
//!
//! The reader trusts nothing it reads back. It keeps the geometry and the
//! run it started itself and never reads back the header words it wrote. A
//! slot it takes must hold a known state, its own index, the game's frame
//! size, a run already started, a sequence above every frame taken before
//! it, and in the current run a stamp inside the run's window that rises
//! with the sequence, and none of those words may change while its frame
//! is copied. Anything else is a [`Fault`], which fails the video's player:
//! the reader keeps it and reads nothing more. A copy reads exactly the
//! slot's frame, at the size the game gave it.
//!
//! The worker publishes frames in sequence order and, after each, raises
//! the ring's watermark, the highest sequence it has published. The reader
//! reads the watermark before it looks at any slot and leaves a frame above
//! it for the next poll, so a frame published while it looks, perhaps into
//! a slot it has already passed, waits instead of overtaking an earlier one.
//! The watermark only defers: whatever value it holds, the slots' checks
//! still apply.
//!
//! A worker that keeps the protocol (frames written only into slots it
//! claimed free, published once whole, in sequence order) always yields
//! whole frames and never fails its player. One that does not cannot make
//! the game read outside the ring or write outside its staging buffer, and
//! what it shows stays inside its own panel: frames with plausible words
//! are shown, and the first slot that breaks a check fails its player.
//!
//! The layout, in words: a header of [`HEADER_WORDS`] (a magic word, the
//! version, the slot count, the width, the height, a frame's bytes, the
//! current run and, at [`WATERMARK_WORD`], the watermark), then the slots,
//! each [`SLOT_HEADER_WORDS`] (its state, sequence, run, stamp, its own
//! index and its frame's bytes) followed by its frame: the luma plane, then
//! the two chroma planes, each half the width and height rounded up (I420),
//! packed little-endian into words, the last word's spare bytes zero.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

pub mod control;

use std::collections::TryReserveError;
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
pub const VERSION: u64 = 1;

/// The header's first word.
const MAGIC: u64 = u64::from_le_bytes(*b"TAPRING1");

/// The header's words.
const H_MAGIC: usize = 0;
const H_VERSION: usize = 1;
const H_SLOTS: usize = 2;
const H_WIDTH: usize = 3;
const H_HEIGHT: usize = 4;
const H_BYTES: usize = 5;
/// The run the game started last, for the worker; the game never reads it
/// back.
const H_RUN: usize = 6;

/// The header word holding the watermark: the highest sequence the worker
/// has published, which it raises after each publication and the game reads
/// before each scan of the slots.
pub const WATERMARK_WORD: usize = 7;

/// A slot's words before its frame.
const S_STATE: usize = 0;
const S_SEQ: usize = 1;
const S_RUN: usize = 2;
const S_STAMP: usize = 3;
const S_INDEX: usize = 4;
const S_BYTES: usize = 5;

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

/// What a poll did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// The frame it copied into the staging buffer, if any.
    pub staged: Option<Frame>,
    /// Whether it freed slots: the game then signals the worker, which may
    /// be waiting for one.
    pub freed: bool,
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
    /// The staging buffer could not be allocated.
    #[error("the staging buffer: {0}")]
    Memory(#[from] TryReserveError),
}

/// A published frame as the reader found it in its slot.
#[derive(Clone, Copy, Debug)]
struct Found {
    slot: usize,
    seq: u64,
    run: u64,
    stamp: i64,
}

impl Found {
    fn index(&self) -> u8 {
        u8::try_from(self.slot).unwrap_or(u8::MAX)
    }
}

/// The game's side of a ring: its geometry, the run it started last, what
/// it has taken, and the staging buffer the render thread uploads from.
#[derive(Debug)]
pub struct Reader {
    geometry: Geometry,
    run: u64,
    window: Window,
    /// The highest sequence taken out of the ring, 0 before the first.
    taken: u64,
    /// The stamp of the last frame staged in the current run.
    last_stamp: Option<i64>,
    staged: Option<Frame>,
    staging: Box<[u64]>,
    fault: Option<Fault>,
}

impl Reader {
    /// Sets up a ring of `geometry` in `ring`: writes its header, every
    /// slot free, run 1, whose frames are stamped inside `window`, and a
    /// watermark of 0, and allocates the staging buffer. The game does this at preparation,
    /// before it passes the section to the worker.
    ///
    /// # Errors
    ///
    /// When `ring` holds fewer words than the geometry needs, or the
    /// staging buffer cannot be allocated.
    pub fn new<W: Words + ?Sized>(
        ring: &W,
        geometry: Geometry,
        window: Window,
    ) -> Result<Self, Setup> {
        let needs = geometry.words();
        if ring.words(0, needs).is_none() {
            return Err(Setup::Section { needs });
        }
        let mut staging = Vec::new();
        staging.try_reserve_exact(geometry.frame_words())?;
        staging.resize(geometry.frame_words(), 0);
        let header = [
            (H_MAGIC, MAGIC),
            (H_VERSION, VERSION),
            (H_SLOTS, wide(SLOTS)),
            (H_WIDTH, u64::from(geometry.width)),
            (H_HEIGHT, u64::from(geometry.height)),
            (H_BYTES, wide(geometry.frame_bytes())),
            (H_RUN, 1),
            (WATERMARK_WORD, 0),
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
            last_stamp: None,
            staged: None,
            staging: staging.into_boxed_slice(),
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

    /// The fault that failed the player, if one has.
    #[must_use]
    pub fn fault(&self) -> Option<Fault> {
        self.fault
    }

    /// Starts the next run, whose frames are stamped inside `window`, and
    /// returns its id for the worker's command. The staged frame and every
    /// frame of an earlier run are no longer shown; the next poll frees
    /// those still in the ring unread.
    pub fn start_run<W: Words + ?Sized>(&mut self, ring: &W, window: Window) -> u64 {
        self.run = self.run.saturating_add(1);
        self.window = window;
        self.last_stamp = None;
        self.staged = None;
        if let Some(w) = word(ring, H_RUN) {
            w.store(self.run, Ordering::Release);
        }
        self.run
    }

    /// The staged frame and its words, while it belongs to the current run
    /// and no fault has failed the player.
    #[must_use]
    pub fn staged(&self) -> Option<(Frame, &[u64])> {
        self.staged.map(|f| (f, &*self.staging))
    }

    /// Reads the ring at `due`, the media time the screen shows: frees the
    /// frames of earlier runs unread, copies the newest frame of the run
    /// stamped at or before `due` into the staging buffer, and frees it with
    /// the frames before it. A frame not due yet stays in its slot, and so
    /// does one above the watermark read before the scan, until a later
    /// poll.
    ///
    /// # Errors
    ///
    /// The [`Fault`] that fails the player. The reader keeps it: every later
    /// poll returns it without reading the ring, and nothing is staged.
    pub fn poll<W: Words + ?Sized>(&mut self, ring: &W, due: i64) -> Result<Poll, Fault> {
        if let Some(f) = self.fault {
            return Err(f);
        }
        let read = self.read(ring, due);
        if let Err(f) = read {
            self.fault = Some(f);
            self.staged = None;
        }
        read
    }

    fn read<W: Words + ?Sized>(&mut self, ring: &W, due: i64) -> Result<Poll, Fault> {
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
            let after_staged = self.last_stamp.is_none_or(|s| a.stamp > s);
            let rises = current().all(|b| b.seq <= a.seq || b.stamp > a.stamp);
            if !after_staged || !rises {
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
        let chosen = current()
            .filter(|a| a.seq <= committed && a.stamp <= due)
            .max_by_key(|a| a.seq)
            .copied();
        let Some(c) = chosen else {
            return Ok(Poll {
                staged: None,
                freed,
            });
        };
        self.copy(ring, &c)?;
        for a in current().filter(|a| a.seq <= c.seq) {
            self.free(ring, a)?;
        }
        let frame = Frame {
            seq: c.seq,
            stamp: c.stamp,
        };
        self.staged = Some(frame);
        self.last_stamp = Some(c.stamp);
        Ok(Poll {
            staged: Some(frame),
            freed: true,
        })
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
        Ok(Some(Found {
            slot,
            seq: load(S_SEQ)?,
            run,
            stamp: load(S_STAMP)?.cast_signed(),
        }))
    }

    /// Copies `c`'s frame into the staging buffer, then checks that its
    /// slot still holds that frame: the same state, sequence, run, stamp,
    /// index and frame size.
    fn copy<W: Words + ?Sized>(&mut self, ring: &W, c: &Found) -> Result<(), Fault> {
        let mut at = self.geometry.frame_start(c.slot);
        for dst in self.staging.chunks_mut(CHUNK_WORDS) {
            let src = ring.words(at, dst.len()).ok_or(Fault::Section)?;
            if src.len() != dst.len() {
                return Err(Fault::Section);
            }
            for (d, s) in dst.iter_mut().zip(src) {
                *d = s.load(Ordering::Relaxed);
            }
            at += dst.len();
        }
        fence(Ordering::Acquire);
        let base = self.geometry.slot_start(c.slot);
        let load = |i: usize| word(ring, base + i).map(|w| w.load(Ordering::Relaxed));
        let same = load(S_STATE) == Some(READY)
            && load(S_SEQ) == Some(c.seq)
            && load(S_RUN) == Some(c.run)
            && load(S_STAMP) == Some(c.stamp.cast_unsigned())
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
        Ok(())
    }
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

    /// The run the game started last.
    #[must_use]
    pub fn run<W: Words + ?Sized>(self, ring: &W) -> u64 {
        word(ring, H_RUN).map_or(0, |w| w.load(Ordering::Acquire))
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
    /// `stamp`: writes its bytes and then the slot's words, marks it ready
    /// once all are written, and then raises the watermark to `seq`. Frames
    /// are published in sequence order; a publication never moves the
    /// watermark back.
    ///
    /// # Errors
    ///
    /// When `frame` is not the ring's frame size, or the section does not
    /// hold the slot; the slot stays claimed.
    pub fn publish<W: Words + ?Sized>(
        self,
        ring: &W,
        slot: Slot,
        frame: &[u8],
        seq: u64,
        run: u64,
        stamp: i64,
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
