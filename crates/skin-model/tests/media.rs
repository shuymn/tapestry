//! The frame ring a video's frames cross from the media worker (design.md
//! "Rendering and Skins", video frames; "Threat Model" T02): frames come out
//! whole, and whatever a worker writes, the game's reads stay inside the
//! ring and a broken slot fails the player. `media_interleavings.rs` checks
//! a cooperating worker against every interleaving with the game's reads.
//!
//! The tests that interleave a reader and a writer that breaks the
//! protocol hold the two threads at a meeting point: one of them asks the
//! ring for a chosen run of words,
//! and waits there while the other acts. Neither waits forever: the test
//! fails at once when that thread finishes without asking for those words,
//! and the stopped thread goes on when the test's thread lets it go or
//! unwinds.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use skin_model::media::{
    CHUNK_WORDS, DUE_WORD, Fault, Frame, Geometry, HEADER_WORDS, PROGRESS_WORD, Reader, Refused,
    SLOT_HEADER_WORDS, SLOTS, STATE_WORD, State, VERSION, WATERMARK_WORD, Window, Words, Writer,
    supervision, watermark,
};

/// Run 1's window: the first minute.
const WINDOW: Window = Window {
    first: 0,
    last: 60_000_000,
};

/// A frame that takes two chunks, at odd sides so the chroma planes round
/// up and the last word has a spare byte.
fn large() -> Geometry {
    let g = Geometry::new(255, 193).unwrap();
    assert!(g.frame_words() > CHUNK_WORDS && g.frame_words() < 2 * CHUNK_WORDS);
    assert_eq!(g.frame_bytes() % 8, 7, "a spare byte in the last word");
    g
}

/// A zeroed section for `g`'s ring.
fn section(g: Geometry) -> Vec<AtomicU64> {
    (0..g.words()).map(|_| AtomicU64::new(0)).collect()
}

/// Frame `seq` of `g`: bytes that differ from every other frame's almost
/// everywhere, so a frame mixed from two shows.
fn frame(g: Geometry, seq: u64) -> Vec<u8> {
    (0..g.frame_bytes() as u64)
        .map(|i| ((seq * 89 + i * 7 + (i >> 8) * 3) % 251) as u8)
        .collect()
}

/// Frame `seq`'s stamp at 30 frames a second, from 0.
fn stamp(seq: u64) -> i64 {
    i64::try_from(seq).unwrap() * 1_000_000 / 30
}

/// What a peek and the take after it did: the frame taken and its bytes,
/// and whether slots were freed.
#[derive(Debug, Default, PartialEq, Eq)]
struct Read {
    taken: Option<(Frame, Vec<u8>)>,
    freed: bool,
}

impl Read {
    /// The frame taken, if any.
    fn frame(&self) -> Option<Frame> {
        self.taken.as_ref().map(|(f, _)| *f)
    }
}

/// The tight pitch of `g`'s planes: rows of the luma and chroma widths.
fn tight(g: Geometry) -> [usize; 2] {
    let w = g.width() as usize;
    [w, w.div_ceil(2)]
}

/// Peeks at `due` and takes the frame due, if any, at the tight pitch into
/// a buffer of the frame's bytes; asserts the take handed it every byte of
/// the frame once and nothing outside it.
fn read<W: Words + ?Sized>(reader: &mut Reader, ring: &W, due: i64) -> Result<Read, Fault> {
    let peek = reader.peek(ring, due)?;
    let Some(d) = peek.due else {
        return Ok(Read {
            taken: None,
            freed: peek.freed,
        });
    };
    let g = reader.geometry();
    let pitch = tight(g);
    assert_eq!(reader.pitched_bytes(pitch), g.frame_bytes());
    let mut out = vec![0_u8; g.frame_bytes()];
    let mut written = vec![false; g.frame_bytes()];
    let mut sink = |at: usize, bytes: &[u8]| {
        let end = at + bytes.len();
        assert!(
            end <= out.len(),
            "a write past the frame: {at}+{}",
            bytes.len()
        );
        for w in &mut written[at..end] {
            assert!(!*w, "a byte written twice, in {at}+{}", bytes.len());
            *w = true;
        }
        out[at..end].copy_from_slice(bytes);
        true
    };
    let f = reader.take(ring, d, pitch, &mut sink)?;
    assert_eq!((f.seq, f.stamp), (d.seq(), d.stamp()));
    assert!(
        written.iter().all(|w| *w),
        "every byte of the frame written"
    );
    Ok(Read {
        taken: Some((f, out)),
        freed: true,
    })
}

/// Asserts `read` took frame `seq` of `g`, whole.
#[track_caller]
fn assert_whole(read: &Read, g: Geometry, seq: u64) {
    let (f, bytes) = read.taken.as_ref().expect("a frame taken");
    assert_eq!(f.seq, seq);
    assert!(*bytes == frame(g, seq), "frame {seq} is taken whole");
}

/// Publishes frame `seq` of run `run` in a slot it claims.
#[track_caller]
fn publish(ring: &[AtomicU64], seq: u64, run: u64, at: i64) -> usize {
    let w = Writer::attach(ring).unwrap();
    let slot = w.claim(ring).expect("a free slot");
    let index = slot.index();
    w.publish(ring, slot, &frame(w.geometry(), seq), seq, run, at)
        .unwrap();
    index
}

/// The word `i` of slot `slot`'s header.
fn slot_word(ring: &[AtomicU64], g: Geometry, slot: usize, i: usize) -> &AtomicU64 {
    &ring[g.frame_start(slot) - SLOT_HEADER_WORDS + i]
}

/// A slot header's words, as the layout orders them.
const STATE: usize = 0;
const SEQ: usize = 1;
const RUN: usize = 2;
const STAMP: usize = 3;
const INDEX: usize = 4;
const BYTES: usize = 5;

/// How long a thread waits at a meeting point before the test fails: far
/// beyond any wait of a run that passes, so it ends only a thread that is
/// stuck. `TAPESTRY_TEST_WATCHDOG` (seconds) overrides it, as in the
/// game's tests.
fn watchdog() -> Duration {
    let secs = std::env::var("TAPESTRY_TEST_WATCHDOG").ok();
    Duration::from_secs(secs.and_then(|s| s.parse().ok()).unwrap_or(600))
}

/// Where a thread that asks a [`Watched`] ring for its chosen words stops
/// while the test's thread acts.
#[derive(Default)]
struct Meeting {
    state: Mutex<Met>,
    changed: Condvar,
}

#[derive(Default)]
struct Met {
    arrived: bool,
    released: bool,
    finished: bool,
}

impl Meeting {
    fn lock(&self) -> MutexGuard<'_, Met> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, f: impl FnOnce(&mut Met)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// The stopped thread's side: waits there until the test's thread lets
    /// it go.
    fn stop(&self) {
        self.set(|m| m.arrived = true);
        let (met, waited) = self
            .changed
            .wait_timeout_while(self.lock(), watchdog(), |m| !m.released)
            .unwrap_or_else(PoisonError::into_inner);
        drop(met);
        assert!(
            !waited.timed_out(),
            "the test's thread never let the stopped thread go"
        );
    }

    /// The test's side: waits until the other thread stops there, failing
    /// at once if it finished without stopping; the returned guard lets it
    /// go when it drops, unwinding included.
    #[track_caller]
    fn meet(&self) -> Release<'_> {
        let (met, _) = self
            .changed
            .wait_timeout_while(self.lock(), watchdog(), |m| !m.arrived && !m.finished)
            .unwrap_or_else(PoisonError::into_inner);
        let (arrived, finished) = (met.arrived, met.finished);
        drop(met);
        assert!(
            arrived,
            "the other thread {} without asking for the meeting point's words",
            if finished { "finished" } else { "stalled" }
        );
        Release(self)
    }
}

/// Marks a [`Meeting`]'s other thread finished when it drops, whether
/// that thread's work returned or unwound.
struct Done<'a>(&'a Meeting);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.set(|m| m.finished = true);
    }
}

/// Lets a [`Meeting`]'s stopped thread go when it drops.
struct Release<'a>(&'a Meeting);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.set(|m| m.released = true);
    }
}

/// A ring's memory seen through the requests made of it: each one is
/// logged, and the one for `stop`'s words (asked for once) stops at its
/// meeting point until the test's thread lets it go, so that thread can act
/// in between.
struct Watched<'a> {
    ring: &'a [AtomicU64],
    log: Mutex<Vec<(usize, usize)>>,
    stop: Option<((usize, usize), &'a Meeting)>,
    stopped: AtomicBool,
}

impl<'a> Watched<'a> {
    fn new(ring: &'a [AtomicU64]) -> Self {
        Self {
            ring,
            log: Mutex::new(Vec::new()),
            stop: None,
            stopped: AtomicBool::new(false),
        }
    }

    fn stopping_at(ring: &'a [AtomicU64], at: (usize, usize), meeting: &'a Meeting) -> Self {
        Self {
            stop: Some((at, meeting)),
            ..Self::new(ring)
        }
    }

    fn take_log(&self) -> Vec<(usize, usize)> {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Words for Watched<'_> {
    fn words(&self, start: usize, len: usize) -> Option<&[AtomicU64]> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((start, len));
        if let Some((at, meeting)) = self.stop
            && at == (start, len)
            && !self.stopped.swap(true, Ordering::SeqCst)
        {
            meeting.stop();
        }
        self.ring.words(start, len)
    }
}

/// The two runs of words a copy of slot `slot`'s frame asks for.
fn copy_chunks(g: Geometry, slot: usize) -> [(usize, usize); 2] {
    let start = g.frame_start(slot);
    [
        (start, CHUNK_WORDS),
        (start + CHUNK_WORDS, g.frame_words() - CHUNK_WORDS),
    ]
}

/// Whether a request is the watermark's word, the due time's (which a peek
/// writes) or lies inside `g`'s slots, the only words a reader asks for.
fn in_ring(g: Geometry, (start, len): (usize, usize)) -> bool {
    (start, len) == (WATERMARK_WORD, 1)
        || (start, len) == (DUE_WORD, 1)
        || (start >= HEADER_WORDS && start + len <= g.words())
}

/// Asserts every request in `log` is the watermark, the due time or lies inside `g`'s
/// slots, and every run of more than one word inside slot `slot`'s frame.
#[track_caller]
fn assert_bounded(log: &[(usize, usize)], g: Geometry, slot: usize) {
    let frame = g.frame_start(slot)..g.frame_start(slot) + g.frame_words();
    for &(start, len) in log {
        assert!(
            in_ring(g, (start, len)),
            "a request outside the slots: {start}+{len}"
        );
        if len > 1 {
            assert!(
                frame.contains(&start) && start + len <= frame.end,
                "a run of words outside slot {slot}'s frame: {start}+{len}, frame {frame:?}"
            );
        }
    }
}

// Covers: T02
// Covers: T02
#[test]
fn ring_supervision_words_read_back_what_the_worker_wrote() {
    let g = Geometry::new(64, 48).unwrap();
    let ring = section(g);
    // A new ring starts its supervision words at zero, whatever the
    // section held.
    ring[PROGRESS_WORD].store(99, Ordering::Relaxed);
    ring[STATE_WORD].store(2, Ordering::Relaxed);
    let mut reader = Reader::new(&ring[..], g, WINDOW).unwrap();
    assert_eq!(supervision(&ring[..]), Some((0, None)));
    assert_eq!(ring[1].load(Ordering::Relaxed), VERSION);
    // The decoder's progress and the player's state, as the worker writes
    // them.
    let w = Writer::attach(&ring[..]).unwrap();
    for _ in 0..3 {
        w.progress(&ring[..]);
    }
    for state in [State::Waiting, State::Decoding, State::Ended] {
        w.state(&ring[..], state);
        assert_eq!(supervision(&ring[..]), Some((3, Some(state))));
    }
    // A word no state has reads as none.
    ring[STATE_WORD].store(9, Ordering::Relaxed);
    assert_eq!(supervision(&ring[..]), Some((3, None)));
    // The reader counts the slots it frees, the game's own count beside
    // the worker's watermark.
    for seq in 1..=3 {
        publish(&ring, seq, 1, stamp(seq - 1));
    }
    assert_eq!(watermark(&ring[..]), Some(3));
    assert_eq!(reader.freed(), 0);
    let r = read(&mut reader, &ring[..], stamp(1)).unwrap();
    assert_eq!((r.frame().map(|f| f.seq), r.freed), (Some(2), true));
    assert_eq!(reader.freed(), 2);
    read(&mut reader, &ring[..], stamp(2)).unwrap();
    assert_eq!(reader.freed(), 3);
    // A ring of the version before is not this layout.
    ring[1].store(VERSION - 1, Ordering::Relaxed);
    assert_eq!(Writer::attach(&ring[..]).err(), Some(Refused::Layout));
    // A section too short for the header has no supervision words.
    assert_eq!(supervision(&ring[..STATE_WORD]), None);
    assert_eq!(watermark(&ring[..WATERMARK_WORD]), None);
}

#[test]
fn ring_round_trip_writer_to_reader() {
    let g = Geometry::new(37, 21).unwrap();
    assert_eq!(g.frame_bytes(), 37 * 21 + 2 * 19 * 11);
    let largest = Geometry::new(2048, 2048).unwrap();
    assert_eq!(
        largest.frame_bytes(),
        6 << 20,
        "4 Mi pixels at even sides fill 6 MiB"
    );
    assert_eq!(
        largest.bytes(),
        (24 << 20) + 8 * (HEADER_WORDS + SLOTS * SLOT_HEADER_WORDS),
        "the section: four frames and the headers, about 24 MiB"
    );
    assert_eq!(largest.bytes(), 8 * largest.words());
    assert!(Geometry::new(8192, 512).is_some(), "4 Mi pixels");
    assert_eq!(Geometry::new(2049, 2048), None, "over 4 Mi pixels");
    assert_eq!(Geometry::new(8193, 1), None, "over the longest side");
    assert_eq!(Geometry::new(0, 21), None);
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    let writer = Writer::attach(ring).unwrap();
    assert_eq!(writer.geometry(), g, "the worker reads the game's geometry");

    // Three frames; the first is not due yet.
    for seq in 1..=3 {
        publish(ring, seq, 1, stamp(seq));
    }
    assert_eq!(read(&mut reader, ring, stamp(1) - 1), Ok(Read::default()));

    // At the second's stamp it is taken whole, freed with the first; the
    // third stays.
    let at_two = read(&mut reader, ring, stamp(2)).unwrap();
    assert_eq!(
        (at_two.frame(), at_two.freed),
        (
            Some(Frame {
                seq: 2,
                stamp: stamp(2)
            }),
            true
        )
    );
    assert_whole(&at_two, g, 2);
    assert_eq!(
        read(&mut reader, ring, stamp(2) + 1),
        Ok(Read::default()),
        "nothing is taken while nothing newer is due"
    );

    // The two freed slots and the first one claimed take three more
    // frames: the ring is full, with frames 3 to 6.
    for seq in 4..=6 {
        publish(ring, seq, 1, stamp(seq));
    }
    assert!(writer.claim(ring).is_none(), "every slot holds a frame");
    let at_five = read(&mut reader, ring, stamp(5) + 1).unwrap();
    assert_whole(&at_five, g, 5);
    let free = writer.claim(ring).expect("frames 3 to 5 freed a slot");

    // A new run: frame 6 of run 1, due as it is, is freed unread.
    let window = Window {
        first: 5_000_000,
        last: 9_000_000,
    };
    assert_eq!(reader.start_run(window), 2);
    assert_eq!(
        read(&mut reader, ring, 9_000_000),
        Ok(Read {
            taken: None,
            freed: true
        })
    );
    writer
        .publish(ring, free, &frame(g, 7), 7, 2, 5_000_000)
        .unwrap();
    let at_seven = read(&mut reader, ring, 5_000_000).unwrap();
    assert_eq!(
        at_seven.frame(),
        Some(Frame {
            seq: 7,
            stamp: 5_000_000
        })
    );
    assert_whole(&at_seven, g, 7);

    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    let claimed = writer.claim(ring).unwrap();
    assert_eq!(
        writer.publish(ring, claimed, &[0; 10], 1, 1, 0),
        Err(Refused::Frame(10)),
        "a frame of another size is not published"
    );
    assert_eq!(read(&mut reader, ring, 0), Ok(Read::default()));
}

/// A ring with frame 1 (stamp 1 000) taken and frame 2 (stamp 2 000) in
/// slot 1, and frame 1's slot free again.
fn taken_one(g: Geometry) -> (Vec<AtomicU64>, Reader) {
    let mem = section(g);
    let mut reader = Reader::new(&*mem, g, WINDOW).unwrap();
    assert_eq!(publish(&mem, 1, 1, 1_000), 0);
    assert_eq!(publish(&mem, 2, 1, 2_000), 1);
    assert_eq!(
        read(&mut reader, &*mem, 1_000)
            .unwrap()
            .frame()
            .unwrap()
            .seq,
        1
    );
    (mem, reader)
}

// Covers: T02
#[test]
fn ring_rejects_bad_index_sequence_stamp_state() {
    let g = Geometry::new(16, 9).unwrap();
    type Corrupt = fn(&[AtomicU64], Geometry);
    let cases: [(&str, Corrupt, Fault); 11] = [
        (
            "another slot's index",
            |r, g| slot_word(r, g, 1, INDEX).store(3, Ordering::Relaxed),
            Fault::Index { slot: 1, value: 3 },
        ),
        (
            "an unknown state in a free slot",
            |r, g| slot_word(r, g, 2, STATE).store(7, Ordering::Relaxed),
            Fault::State { slot: 2, value: 7 },
        ),
        (
            "the largest state",
            |r, g| slot_word(r, g, 1, STATE).store(u64::MAX, Ordering::Relaxed),
            Fault::State {
                slot: 1,
                value: u64::MAX,
            },
        ),
        (
            "a run not started",
            |r, g| slot_word(r, g, 1, RUN).store(2, Ordering::Relaxed),
            Fault::Run { slot: 1, value: 2 },
        ),
        (
            "run 0, which no ring starts",
            |r, g| slot_word(r, g, 1, RUN).store(0, Ordering::Relaxed),
            Fault::Run { slot: 1, value: 0 },
        ),
        (
            "a sequence already taken",
            |r, g| slot_word(r, g, 1, SEQ).store(1, Ordering::Relaxed),
            Fault::Sequence { slot: 1, value: 1 },
        ),
        (
            "a sequence two slots share",
            |r, _| {
                publish(r, 2, 1, 3_000);
            },
            Fault::Sequence { slot: 0, value: 2 },
        ),
        (
            "a stamp not after the staged frame's",
            |r, g| slot_word(r, g, 1, STAMP).store(1_000, Ordering::Relaxed),
            Fault::Sequence { slot: 1, value: 2 },
        ),
        (
            "a later sequence with an earlier stamp",
            |r, _| {
                publish(r, 3, 1, 1_500);
            },
            Fault::Sequence { slot: 1, value: 2 },
        ),
        (
            "a stamp past the window",
            |r, g| slot_word(r, g, 1, STAMP).store(60_000_001, Ordering::Relaxed),
            Fault::Stamp {
                slot: 1,
                value: 60_000_001,
            },
        ),
        (
            "a stamp before the window",
            |r, g| slot_word(r, g, 1, STAMP).store((-1_i64).cast_unsigned(), Ordering::Relaxed),
            Fault::Stamp { slot: 1, value: -1 },
        ),
    ];
    for (what, corrupt, fault) in cases {
        let (mem, mut reader) = taken_one(g);
        // Another player's ring, which keeps serving.
        let (other_mem, mut other) = taken_one(g);
        corrupt(&mem, g);
        let watched = Watched::new(&mem);
        assert_eq!(
            reader.peek(&watched, 5_000),
            Err(fault),
            "{what}: nothing is due"
        );
        assert_eq!(reader.fault(), Some(fault), "{what}: the player failed");
        watched.take_log();
        assert_eq!(
            read(&mut reader, &watched, 6_000),
            Err(fault),
            "{what}: the fault is kept"
        );
        assert_eq!(
            watched.take_log(),
            [],
            "{what}: a failed player reads nothing more"
        );
        assert_eq!(
            read(&mut other, &*other_mem, 5_000)
                .unwrap()
                .frame()
                .map(|f| f.seq),
            Some(2),
            "{what}: another player keeps serving"
        );
    }
}

// Covers: T02
#[test]
fn ring_reader_copies_only_parent_sized_bytes() {
    let g = large();
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    let writer = Writer::attach(ring).unwrap();
    // Slot 0 stays claimed and is never published; frame 1 goes in slot 1,
    // between it and slot 2, whose words hold a sentinel.
    let _held = writer.claim(ring).unwrap();
    assert_eq!(publish(ring, 1, 1, 1_000), 1);
    let sentinel = 0xa5a5_a5a5_a5a5_a5a5;
    for w in &mem[g.frame_start(0)..g.frame_start(1) - SLOT_HEADER_WORDS] {
        w.store(sentinel, Ordering::Relaxed);
    }
    for w in &mem[g.frame_start(2)..g.frame_start(2) + g.frame_words()] {
        w.store(sentinel, Ordering::Relaxed);
    }

    let claim = |bytes: u64| {
        slot_word(ring, g, 1, BYTES).store(bytes, Ordering::Relaxed);
    };
    let frame_bytes = g.frame_bytes() as u64;
    for (what, bytes) in [
        ("larger", frame_bytes + 8 * CHUNK_WORDS as u64),
        ("one word larger", frame_bytes + 8),
        ("one word smaller", frame_bytes - 8),
    ] {
        claim(bytes);
        let mut reader = Reader::new(&*section(g), g, WINDOW).unwrap();
        let watched = Watched::new(ring);
        let polled = read(&mut reader, &watched, 1_000);
        let log = watched.take_log();
        assert_bounded(&log, g, 1);
        assert_eq!(
            polled,
            Err(Fault::Size {
                slot: 1,
                value: bytes
            }),
            "a {what} frame than the game's"
        );
        assert!(
            log.iter().all(|&(_, len)| len == 1),
            "a {what} frame is not copied: {log:?}"
        );
    }

    claim(frame_bytes);
    let watched = Watched::new(ring);
    let one = read(&mut reader, &watched, 1_000).unwrap();
    let log = watched.take_log();
    assert_bounded(&log, g, 1);
    let runs: Vec<_> = log.into_iter().filter(|&(_, len)| len > 1).collect();
    assert_eq!(
        runs,
        copy_chunks(g, 1),
        "the copy reads the slot's frame, word for word"
    );
    // `read` asserts the take wrote nothing outside the frame's bytes.
    assert_whole(&one, g, 1);
}

/// A small generator of words, the same for every run.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// Covers: T02
#[test]
fn ring_arbitrary_writer_stays_safe_and_bounded() {
    let g = large();
    let scribble = 0x5a5a_5a5a_5a5a_5a5a;

    // A writer that rewrites the frame being copied, its slot's words left
    // as they were: the copy stays inside the slot and the sink's frame,
    // and the frame it gives is torn, which no check can tell.
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    assert_eq!(publish(ring, 1, 1, 1_000), 0);
    let meeting = Meeting::default();
    let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &meeting);
    let torn = thread::scope(|s| {
        let reading = s.spawn(|| {
            let _done = Done(&meeting);
            read(&mut reader, &watched, 1_000)
        });
        let release = meeting.meet();
        for w in &mem[g.frame_start(0)..g.frame_start(0) + g.frame_words()] {
            w.store(scribble, Ordering::Relaxed);
        }
        drop(release);
        reading.join().unwrap().unwrap()
    });
    assert_bounded(&watched.take_log(), g, 0);
    let (f, bytes) = torn.taken.unwrap();
    assert_eq!(f.seq, 1);
    let published = frame(g, 1);
    let split = CHUNK_WORDS * 8;
    assert!(
        bytes[..split] == published[..split],
        "the first chunk, copied before"
    );
    assert!(
        bytes[split..].iter().all(|b| *b == 0x5a),
        "the second chunk, copied after the rewrite: a torn frame"
    );

    // A writer that rewrites the slot's own words during the copy fails the
    // player, whichever word it rewrites.
    for (what, i, value) in [
        ("its sequence", SEQ, 9),
        ("its state", STATE, 0),
        ("its run", RUN, 0),
        ("its stamp", STAMP, 999),
        ("its index", INDEX, 3),
        ("its frame size", BYTES, 8),
    ] {
        let mem = section(g);
        let ring = &*mem;
        let mut reader = Reader::new(ring, g, WINDOW).unwrap();
        assert_eq!(publish(ring, 1, 1, 1_000), 0);
        let meeting = Meeting::default();
        let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &meeting);
        thread::scope(|s| {
            let reading = s.spawn(|| {
                let _done = Done(&meeting);
                read(&mut reader, &watched, 1_000)
            });
            let release = meeting.meet();
            slot_word(ring, g, 0, i).store(value, Ordering::Relaxed);
            drop(release);
            assert_eq!(
                reading.join().unwrap(),
                Err(Fault::Changed { slot: 0 }),
                "{what} rewritten during the copy"
            );
        });
        assert_bounded(&watched.take_log(), g, 0);
        assert_eq!(
            reader.fault(),
            Some(Fault::Changed { slot: 0 }),
            "{what}: the player failed"
        );
    }

    // Arbitrary words in every slot, some of them near a valid frame's:
    // each read stays inside the ring, runs of words inside one slot's
    // frame; a frame it takes is due and in the window, and a fault is
    // kept.
    let g = Geometry::new(16, 9).unwrap();
    let mut rng = Xorshift(0x9e37_79b9_7f4a_7c15);
    let mut outcomes = [0_usize; 3];
    for _ in 0..512 {
        let mem = section(g);
        let mut reader = Reader::new(&*mem, g, WINDOW).unwrap();
        for w in &mem[HEADER_WORDS..] {
            w.store(rng.next(), Ordering::Relaxed);
        }
        // The watermark at or above every near-valid sequence, and each
        // slot word near a valid frame's; one in eight of them arbitrary.
        let watermark = if rng.next().is_multiple_of(8) {
            rng.next()
        } else {
            8
        };
        mem[WATERMARK_WORD].store(watermark, Ordering::Relaxed);
        for slot in 0..SLOTS {
            let r = rng.next();
            let fields = [
                (STATE, r % 3),
                (INDEX, slot as u64),
                (BYTES, g.frame_bytes() as u64),
                (RUN, r >> 2 & 1),
                (SEQ, 1 + (r >> 3 & 7)),
                (STAMP, r >> 6 & 0xffff),
            ];
            for (i, near) in fields {
                let v = if rng.next().is_multiple_of(8) {
                    rng.next()
                } else {
                    near
                };
                slot_word(&mem, g, slot, i).store(v, Ordering::Relaxed);
            }
        }
        let due = (rng.next() & 0x1_ffff).cast_signed();
        let watched = Watched::new(&mem);
        let polled = read(&mut reader, &watched, due);
        let log = watched.take_log();
        for &(start, len) in &log {
            assert!(in_ring(g, (start, len)));
            if len > 1 {
                assert!((0..SLOTS).any(|k| {
                    let f = g.frame_start(k);
                    start == f && len == g.frame_words()
                }));
            }
        }
        match polled {
            Ok(Read {
                taken: Some((f, bytes)),
                ..
            }) => {
                assert!(f.stamp <= due && WINDOW.first <= f.stamp && f.stamp <= WINDOW.last);
                assert_eq!(bytes.len(), g.frame_bytes());
                outcomes[0] += 1;
            }
            Ok(_) => outcomes[1] += 1,
            Err(fault) => {
                assert_eq!(read(&mut reader, &watched, due), Err(fault));
                assert_eq!(watched.take_log(), []);
                assert_eq!(reader.fault(), Some(fault));
                outcomes[2] += 1;
            }
        }
    }
    assert!(
        outcomes.iter().all(|n| *n > 0),
        "taken, nothing and faults all reached: {outcomes:?}"
    );
}

/// `peek` then `take` writes the frame due, each plane's rows at the pitch,
/// the planes back to back, touches no row's padding, and frees the frames
/// up to it.
#[test]
fn ring_take_into_writes_the_due_frame_at_the_pitch() {
    let g = large();
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    for seq in 1..=3 {
        publish(ring, seq, 1, stamp(seq));
    }
    let peek = reader.peek(ring, stamp(2)).unwrap();
    assert!(!peek.freed, "no earlier run's frame");
    let due = peek.due.expect("frame 2 is due");
    assert_eq!((due.seq(), due.stamp()), (2, stamp(2)));
    let pitch = [256, 128];
    let (w, h) = (255, 193);
    let (cw, ch) = (128, 97);
    assert_eq!(reader.pitched_bytes(pitch), 256 * h + 2 * 128 * ch);
    let mut out = vec![0xAA_u8; reader.pitched_bytes(pitch)];
    let mut longest_row = 0;
    let mut put = |at: usize, bytes: &[u8]| {
        longest_row = longest_row.max(bytes.len());
        match out.get_mut(at..at + bytes.len()) {
            Some(d) => {
                d.copy_from_slice(bytes);
                true
            }
            None => false,
        }
    };
    let watched = Watched::new(ring);
    let f = reader.take(&watched, due, pitch, &mut put).unwrap();
    assert_eq!(
        f,
        Frame {
            seq: 2,
            stamp: stamp(2)
        }
    );
    assert!(longest_row <= w, "no write runs past a row");
    let runs: Vec<_> = watched
        .take_log()
        .into_iter()
        .filter(|&(_, len)| len > 1)
        .collect();
    assert_eq!(
        runs,
        copy_chunks(g, 1),
        "the take reads the slot's frame in chunks"
    );
    let tight = frame(g, 2);
    let mut want = vec![0xAA_u8; out.len()];
    for r in 0..h {
        want[r * 256..r * 256 + w].copy_from_slice(&tight[r * w..(r + 1) * w]);
    }
    for p in 0..2 {
        let (src, dst) = (w * h + p * cw * ch, 256 * h + p * 128 * ch);
        for r in 0..ch {
            want[dst + r * 128..dst + r * 128 + cw]
                .copy_from_slice(&tight[src + r * cw..src + (r + 1) * cw]);
        }
    }
    assert!(
        out == want,
        "each plane's rows at the pitch, the padding untouched"
    );
    // Frames 1 and 2 are free, 3 is not due.
    assert_eq!(reader.freed(), 2);
    assert_eq!(reader.peek(ring, stamp(2)).unwrap().due, None);
    publish(ring, 4, 1, stamp(4));
    publish(ring, 5, 1, stamp(5));
    let five = read(&mut reader, ring, stamp(5)).unwrap();
    assert_whole(&five, g, 5);
}

/// A take refuses a pitch below its plane's width, a sink that refuses
/// bytes, and a frame peeked before a new run or taken already: each fails
/// the player, which then reads nothing more.
#[test]
fn ring_take_refused_fails_the_player() {
    let g = Geometry::new(16, 9).unwrap();
    type Take = fn(&mut Reader, &[AtomicU64], skin_model::media::Due) -> Result<Frame, Fault>;
    let cases: [(&str, Take); 5] = [
        ("a sink that refuses", |reader, ring, due| {
            let mut n = 0;
            reader.take(ring, due, tight(reader.geometry()), &mut |_, _| {
                n += 1;
                n < 3
            })
        }),
        ("a luma pitch below the width", |reader, ring, due| {
            let [l, c] = tight(reader.geometry());
            reader.take(ring, due, [l - 1, c], &mut |_, _| true)
        }),
        ("a chroma pitch below its width", |reader, ring, due| {
            let [l, c] = tight(reader.geometry());
            reader.take(ring, due, [l, c - 1], &mut |_, _| true)
        }),
        ("a frame of an earlier run", |reader, ring, due| {
            reader.start_run(WINDOW);
            reader.take(ring, due, tight(reader.geometry()), &mut |_, _| true)
        }),
        ("a frame taken already", |reader, ring, due| {
            reader.take(ring, due, tight(reader.geometry()), &mut |_, _| true)?;
            reader.take(ring, due, tight(reader.geometry()), &mut |_, _| true)
        }),
    ];
    for (what, take) in cases {
        let mem = section(g);
        let ring = &*mem;
        let mut reader = Reader::new(ring, g, WINDOW).unwrap();
        assert_eq!(publish(ring, 1, 1, 1_000), 0);
        let due = reader.peek(ring, 1_000).unwrap().due.unwrap();
        let fault = take(&mut reader, ring, due).expect_err(what);
        let want = if what.starts_with("a frame") {
            Fault::Changed { slot: 0 }
        } else {
            Fault::Section
        };
        assert_eq!(fault, want, "{what}");
        assert_eq!(reader.fault(), Some(fault), "{what}: the player failed");
        publish(ring, 2, reader.run(), 2_000);
        let watched = Watched::new(ring);
        assert_eq!(
            reader.peek(&watched, 2_000),
            Err(fault),
            "{what}: the fault is kept"
        );
        assert_eq!(
            reader.take(&watched, due, tight(g), &mut |_, _| true),
            Err(fault),
            "{what}: a take after it returns it too"
        );
        assert_eq!(
            watched.take_log(),
            [],
            "{what}: a failed player reads nothing more"
        );
    }
}

/// Publishes frame `seq` of run 1, stamped `at` and shown until `until`, in
/// a slot it claims.
#[track_caller]
fn publish_until(ring: &[AtomicU64], seq: u64, at: i64, until: i64) {
    let w = Writer::attach(ring).unwrap();
    let slot = w.claim(ring).expect("a free slot");
    w.publish_until(ring, slot, &frame(w.geometry(), seq), seq, 1, at, until)
        .unwrap();
}

/// A frame is shown until its successor is due (design.md "Rendering and
/// Skins", video frames): with the ring holding a video's first four
/// frames and the time 780 ms in, the newest at or before it (its
/// successor at 133 ms due) is behind the clock, so the peek takes
/// nothing, frees all four unread and says so, and writes the time for
/// the worker; the frame due then is taken, and a video's last frame
/// (`UNTIL_END`) is never behind.
// Revert: the peek's behind check dropped -> "a frame behind the clock
// taken"; a last frame counted behind at the latest time -> the last
// read fails.
#[test]
fn ring_frees_a_frame_behind_the_clock_unread() {
    use skin_model::media::{NOTHING_DUE, UNTIL_END, shown};
    let g = Geometry::new(16, 9).unwrap();
    let mem = section(g);
    let mut reader = Reader::new(&*mem, g, WINDOW).unwrap();
    assert_eq!(shown(&*mem), Some(NOTHING_DUE));
    for seq in 1..=4 {
        publish_until(&mem, seq, stamp(seq - 1), stamp(seq));
    }
    let peek = reader.peek(&*mem, 780_000).unwrap();
    assert!(peek.due.is_none(), "a frame behind the clock taken");
    assert!(peek.behind && peek.freed, "{peek:?}");
    assert_eq!(reader.freed(), 4, "the four freed unread");
    assert_eq!(shown(&*mem), Some(780_000), "the time written");
    publish_until(&mem, 5, 766_667, 800_000);
    let r = read(&mut reader, &*mem, 780_000).unwrap();
    assert_whole(&r, g, 5);
    publish_until(&mem, 6, 800_000, UNTIL_END);
    let r = read(&mut reader, &*mem, i64::MAX).unwrap();
    assert_whole(&r, g, 6);
}

/// A frame shown until no later than its own stamp breaks the protocol:
/// the peek fails the player.
#[test]
fn ring_rejects_an_until_not_after_the_stamp() {
    let g = Geometry::new(16, 9).unwrap();
    let mem = section(g);
    let mut reader = Reader::new(&*mem, g, WINDOW).unwrap();
    publish_until(&mem, 1, 1_000, 1_000);
    assert_eq!(
        reader.peek(&*mem, 2_000),
        Err(Fault::Until {
            slot: 0,
            value: 1_000
        })
    );
}
