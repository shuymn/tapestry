//! The frame ring a video's frames cross from the media worker (design.md
//! "Rendering and Skins", video frames; "Threat Model" T02): a cooperating
//! worker's frames come out whole, and whatever a worker writes, the game's
//! reads stay inside the ring and a broken slot fails the player.
//!
//! The tests that interleave a reader and a writer hold the two threads at
//! a meeting point: one of them asks the ring for a chosen run of words,
//! and waits there while the other acts.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Barrier, Mutex, PoisonError};
use std::thread;

use skin_model::media::{
    CHUNK_WORDS, Fault, Frame, Geometry, HEADER_WORDS, Poll, Reader, Refused, SLOT_HEADER_WORDS,
    SLOTS, Window, Words, Writer,
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

/// The staged frame and the bytes of its words, its last word's spare
/// bytes included.
fn staged_bytes(reader: &Reader) -> (Frame, Vec<u8>) {
    let (frame, words) = reader.staged().expect("a staged frame");
    assert_eq!(words.len(), reader.geometry().frame_words());
    (frame, words.iter().flat_map(|w| w.to_le_bytes()).collect())
}

/// Asserts the staged frame is frame `seq`, whole, with its spare bytes
/// zero.
#[track_caller]
fn assert_whole(reader: &Reader, seq: u64) {
    let (f, bytes) = staged_bytes(reader);
    let len = reader.geometry().frame_bytes();
    assert_eq!(f.seq, seq);
    assert!(
        bytes[..len] == frame(reader.geometry(), seq),
        "frame {seq} is staged whole"
    );
    assert!(bytes[len..].iter().all(|b| *b == 0), "the spare bytes");
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

/// A ring's memory seen through the requests made of it: each one is
/// logged, and the one for `stop`'s words (asked for once) waits at its
/// barrier twice, on arriving and before going on, so the other thread can
/// act in between.
struct Watched<'a> {
    ring: &'a [AtomicU64],
    log: Mutex<Vec<(usize, usize)>>,
    stop: Option<((usize, usize), &'a Barrier)>,
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

    fn stopping_at(ring: &'a [AtomicU64], at: (usize, usize), barrier: &'a Barrier) -> Self {
        Self {
            stop: Some((at, barrier)),
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
        if let Some((at, barrier)) = self.stop
            && at == (start, len)
            && !self.stopped.swap(true, Ordering::SeqCst)
        {
            barrier.wait();
            barrier.wait();
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

/// Asserts every request in `log` lies inside `g`'s ring, below its
/// header, and every run of more than one word inside slot `slot`'s frame.
#[track_caller]
fn assert_bounded(log: &[(usize, usize)], g: Geometry, slot: usize) {
    let frame = g.frame_start(slot)..g.frame_start(slot) + g.frame_words();
    for &(start, len) in log {
        assert!(
            start >= HEADER_WORDS && start + len <= g.words(),
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
#[test]
fn ring_round_trip_writer_to_reader() {
    let g = Geometry::new(37, 21).unwrap();
    assert_eq!(g.frame_bytes(), 37 * 21 + 2 * 19 * 11);
    assert_eq!(
        Geometry::new(2048, 2048).unwrap().frame_bytes(),
        6 << 20,
        "4 Mi pixels at even sides fill 6 MiB"
    );
    assert!(Geometry::new(8192, 512).is_some(), "4 Mi pixels");
    assert_eq!(Geometry::new(2049, 2048), None, "over 4 Mi pixels");
    assert_eq!(Geometry::new(8193, 1), None, "over the longest side");
    assert_eq!(Geometry::new(0, 21), None);
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    let writer = Writer::attach(ring).unwrap();
    assert_eq!(writer.geometry(), g, "the worker reads the game's geometry");
    assert_eq!(writer.run(ring), 1);

    // Three frames; the first is not due yet.
    for seq in 1..=3 {
        publish(ring, seq, 1, stamp(seq));
    }
    assert_eq!(reader.poll(ring, stamp(1) - 1), Ok(Poll::default()));
    assert_eq!(reader.staged(), None);

    // At the second's stamp it is staged whole, freed with the first; the
    // third stays.
    let at_two = reader.poll(ring, stamp(2)).unwrap();
    assert_eq!(
        at_two,
        Poll {
            staged: Some(Frame {
                seq: 2,
                stamp: stamp(2)
            }),
            freed: true
        }
    );
    assert_whole(&reader, 2);
    assert_eq!(
        reader.poll(ring, stamp(2) + 1),
        Ok(Poll::default()),
        "the staged frame stays while nothing newer is due"
    );
    assert_whole(&reader, 2);

    // The two freed slots and the first one claimed take three more
    // frames: the ring is full, with frames 3 to 6.
    for seq in 4..=6 {
        publish(ring, seq, 1, stamp(seq));
    }
    assert!(writer.claim(ring).is_none(), "every slot holds a frame");
    let at_five = reader.poll(ring, stamp(5) + 1).unwrap();
    assert_eq!(at_five.staged.map(|f| f.seq), Some(5));
    assert_whole(&reader, 5);
    let free = writer.claim(ring).expect("frames 3 to 5 freed a slot");

    // A new run: the staged frame is no longer shown, and frame 6 of run 1,
    // due as it is, is freed unread.
    let window = Window {
        first: 5_000_000,
        last: 9_000_000,
    };
    assert_eq!(reader.start_run(ring, window), 2);
    assert_eq!(writer.run(ring), 2, "the worker sees the new run");
    assert_eq!(reader.staged(), None);
    assert_eq!(
        reader.poll(ring, 9_000_000),
        Ok(Poll {
            staged: None,
            freed: true
        })
    );
    writer
        .publish(ring, free, &frame(g, 7), 7, 2, 5_000_000)
        .unwrap();
    assert_eq!(
        reader.poll(ring, 5_000_000).unwrap().staged,
        Some(Frame {
            seq: 7,
            stamp: 5_000_000
        })
    );
    assert_whole(&reader, 7);

    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    let claimed = writer.claim(ring).unwrap();
    assert_eq!(
        writer.publish(ring, claimed, &[0; 10], 1, 1, 0),
        Err(Refused::Frame(10)),
        "a frame of another size is not published"
    );
    assert_eq!(reader.poll(ring, 0), Ok(Poll::default()));
}

/// A ring with frame 1 (stamp 1 000) taken and frame 2 (stamp 2 000) in
/// slot 1, and frame 1's slot free again.
fn taken_one(g: Geometry) -> (Vec<AtomicU64>, Reader) {
    let mem = section(g);
    let mut reader = Reader::new(&*mem, g, WINDOW).unwrap();
    assert_eq!(publish(&mem, 1, 1, 1_000), 0);
    assert_eq!(publish(&mem, 2, 1, 2_000), 1);
    assert_eq!(reader.poll(&*mem, 1_000).unwrap().staged.unwrap().seq, 1);
    (mem, reader)
}

// Covers: T02
#[test]
fn ring_rejects_bad_index_sequence_stamp_state() {
    let g = Geometry::new(16, 9).unwrap();
    type Corrupt = fn(&[AtomicU64], Geometry);
    let cases: [(&str, Corrupt, Fault); 10] = [
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
        assert_eq!(reader.poll(&watched, 5_000), Err(fault), "{what}");
        assert_eq!(reader.fault(), Some(fault), "{what}: the player failed");
        assert_eq!(reader.staged(), None, "{what}: nothing is shown");
        watched.take_log();
        assert_eq!(
            reader.poll(&watched, 6_000),
            Err(fault),
            "{what}: the fault is kept"
        );
        assert_eq!(
            watched.take_log(),
            [],
            "{what}: a failed player reads nothing more"
        );
        assert_eq!(
            other
                .poll(&*other_mem, 5_000)
                .unwrap()
                .staged
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
        let polled = reader.poll(&watched, 1_000);
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
    assert_eq!(
        reader.poll(&watched, 1_000).unwrap().staged.map(|f| f.seq),
        Some(1)
    );
    let log = watched.take_log();
    assert_bounded(&log, g, 1);
    let runs: Vec<_> = log.into_iter().filter(|&(_, len)| len > 1).collect();
    assert_eq!(
        runs,
        copy_chunks(g, 1),
        "the copy reads the slot's frame, word for word"
    );
    assert_whole(&reader, 1);
    assert_eq!(
        reader.staged().unwrap().1.len(),
        g.frame_words(),
        "the staging buffer keeps the game's size"
    );
}

// Covers: T02
#[test]
fn ring_cooperating_writer_never_tears() {
    let g = large();

    // A writer stopped between its two chunks: a poll then takes nothing
    // new and finds nothing wrong; the frame comes out whole once
    // published. The slot it writes held frame 1, whose words are still
    // there.
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    assert_eq!(publish(ring, 1, 1, 1_000), 0);
    assert_eq!(reader.poll(ring, 1_000).unwrap().staged.unwrap().seq, 1);
    let barrier = Barrier::new(2);
    let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &barrier);
    thread::scope(|s| {
        let writer = s.spawn(|| {
            let w = Writer::attach(&watched).unwrap();
            let slot = w.claim(&watched).unwrap();
            assert_eq!(slot.index(), 0, "the slot frame 1 was taken from");
            w.publish(&watched, slot, &frame(g, 2), 2, 1, 2_000)
        });
        barrier.wait();
        assert_eq!(
            reader.poll(ring, 2_000),
            Ok(Poll::default()),
            "a poll while the writer is between its chunks takes nothing"
        );
        assert_whole(&reader, 1);
        barrier.wait();
        writer.join().unwrap().unwrap();
    });
    assert_eq!(reader.poll(ring, 2_000).unwrap().staged.unwrap().seq, 2);
    assert_whole(&reader, 2);

    // A reader stopped between its two chunks while the writer publishes
    // into every other slot: the writer cannot claim the slot being copied,
    // and the copy comes out whole.
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    assert_eq!(publish(ring, 1, 1, 1_000), 0);
    let barrier = Barrier::new(2);
    let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &barrier);
    thread::scope(|s| {
        let read = s.spawn(|| {
            let polled = reader.poll(&watched, 1_000);
            (polled, staged_bytes(&reader))
        });
        barrier.wait();
        for seq in 2..=SLOTS as u64 {
            assert_ne!(publish(ring, seq, 1, 1_000 * seq as i64), 0);
        }
        assert!(
            Writer::attach(ring).unwrap().claim(ring).is_none(),
            "the slot being copied is not free"
        );
        barrier.wait();
        let (polled, (staged, bytes)) = read.join().unwrap();
        assert_eq!(polled.unwrap().staged, Some(staged));
        assert_eq!(staged.seq, 1);
        assert!(
            bytes[..g.frame_bytes()] == frame(g, 1),
            "frame 1 is copied whole"
        );
    });
    let last = SLOTS as u64;
    assert_eq!(
        reader
            .poll(ring, 1_000 * last as i64)
            .unwrap()
            .staged
            .unwrap()
            .seq,
        last
    );
    assert_whole(&reader, last);
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
    // as they were: the copy stays inside the slot and the staging buffer,
    // and the frame it gives is torn, which no check can tell.
    let mem = section(g);
    let ring = &*mem;
    let mut reader = Reader::new(ring, g, WINDOW).unwrap();
    assert_eq!(publish(ring, 1, 1, 1_000), 0);
    let barrier = Barrier::new(2);
    let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &barrier);
    thread::scope(|s| {
        let read = s.spawn(|| reader.poll(&watched, 1_000));
        barrier.wait();
        for w in &mem[g.frame_start(0)..g.frame_start(0) + g.frame_words()] {
            w.store(scribble, Ordering::Relaxed);
        }
        barrier.wait();
        assert_eq!(read.join().unwrap().unwrap().staged.unwrap().seq, 1);
    });
    assert_bounded(&watched.take_log(), g, 0);
    let (_, bytes) = staged_bytes(&reader);
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
    ] {
        let mem = section(g);
        let ring = &*mem;
        let mut reader = Reader::new(ring, g, WINDOW).unwrap();
        assert_eq!(publish(ring, 1, 1, 1_000), 0);
        let barrier = Barrier::new(2);
        let watched = Watched::stopping_at(ring, copy_chunks(g, 0)[1], &barrier);
        thread::scope(|s| {
            let read = s.spawn(|| reader.poll(&watched, 1_000));
            barrier.wait();
            slot_word(ring, g, 0, i).store(value, Ordering::Relaxed);
            barrier.wait();
            assert_eq!(
                read.join().unwrap(),
                Err(Fault::Changed { slot: 0 }),
                "{what} rewritten during the copy"
            );
        });
        assert_bounded(&watched.take_log(), g, 0);
        assert_eq!(reader.staged(), None, "{what}: nothing is shown");
    }

    // Arbitrary words in every slot, some of them near a valid frame's:
    // each poll stays inside the ring, runs of words inside one slot's
    // frame; a frame it stages is due and in the window, and a fault is
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
        // Each slot word near a valid frame's, and one in eight arbitrary.
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
        let polled = reader.poll(&watched, due);
        let log = watched.take_log();
        for &(start, len) in &log {
            assert!(start >= HEADER_WORDS && start + len <= g.words());
            if len > 1 {
                assert!((0..SLOTS).any(|k| {
                    let f = g.frame_start(k);
                    start == f && len == g.frame_words()
                }));
            }
        }
        match polled {
            Ok(Poll {
                staged: Some(f), ..
            }) => {
                assert!(f.stamp <= due && WINDOW.first <= f.stamp && f.stamp <= WINDOW.last);
                assert_eq!(reader.staged().unwrap().1.len(), g.frame_words());
                outcomes[0] += 1;
            }
            Ok(_) => outcomes[1] += 1,
            Err(fault) => {
                assert_eq!(reader.poll(&watched, due), Err(fault));
                assert_eq!(watched.take_log(), []);
                assert_eq!(reader.staged(), None);
                outcomes[2] += 1;
            }
        }
    }
    assert!(
        outcomes.iter().all(|n| *n > 0),
        "staged, nothing and faults all reached: {outcomes:?}"
    );
}
