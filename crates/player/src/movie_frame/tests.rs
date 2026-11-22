//! The hook against a portable ring: the worker's side written with
//! [`Writer`] into words of the test's own, and planes that keep what each
//! take wrote.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skin_model::media::{Geometry, Reader, SLOT_HEADER_WORDS, Window, Writer};
use skin_model::movie::MovieShow;

use super::*;

/// A movie the test moves by hand, with a ring of its own.
struct Fake {
    status: Mutex<Status>,
    loss: Mutex<Option<Loss>>,
    words: Vec<AtomicU64>,
    reader: Mutex<Option<Reader>>,
    geometry: Geometry,
    staged: AtomicBool,
    freed: AtomicU64,
}

impl Fake {
    fn new(width: u32, height: u32) -> Arc<Self> {
        let geometry = Geometry::new(width, height).unwrap();
        let words: Vec<AtomicU64> = (0..geometry.words()).map(|_| AtomicU64::new(0)).collect();
        let window = Window {
            first: 0,
            last: i64::MAX,
        };
        let reader = Reader::new(words.as_slice(), geometry, window).unwrap();
        Arc::new(Self {
            status: Mutex::new(Status::Pending),
            loss: Mutex::new(None),
            words,
            reader: Mutex::new(Some(reader)),
            geometry,
            staged: AtomicBool::new(false),
            freed: AtomicU64::new(0),
        })
    }

    fn set(&self, status: Status) {
        *self.status.lock().unwrap() = status;
    }

    fn loss(&self) -> Option<Loss> {
        *self.loss.lock().unwrap()
    }

    /// Publishes a frame of run 1 whose every byte is `id`.
    fn publish(&self, seq: u64, stamp: i64, id: u8) {
        let writer = Writer::attach(self.words.as_slice()).unwrap();
        let slot = writer.claim(self.words.as_slice()).unwrap();
        let frame = vec![id; self.geometry.frame_bytes()];
        writer
            .publish(
                self.words.as_slice(),
                slot,
                &frame,
                seq,
                1,
                skin_model::media::Stamp {
                    at: stamp,
                    duration: 33_333,
                },
            )
            .unwrap();
    }
}

impl Source for Fake {
    fn status(&self) -> Status {
        *self.status.lock().unwrap()
    }

    fn live(&self) -> Option<LiveView<'_>> {
        (self.status() == Status::Live).then(|| LiveView {
            words: &self.words,
            reader: &self.reader,
            width: self.geometry.width(),
            height: self.geometry.height(),
            bt601: true,
            full: false,
        })
    }

    fn lose(&self, why: Loss) {
        let mut status = self.status.lock().unwrap();
        if *status != Status::Lost {
            *status = Status::Lost;
            *self.loss.lock().unwrap() = Some(why);
        }
    }

    fn staged_first(&self) {
        self.staged.store(true, Ordering::SeqCst);
    }

    fn freed(&self, total: u64) {
        self.freed.store(total, Ordering::SeqCst);
    }

    fn expire(&self) {
        if !self.settled() {
            self.lose(Loss::Preparation);
        }
    }

    fn settled(&self) -> bool {
        self.staged.load(Ordering::SeqCst) || matches!(self.status(), Status::Absent | Status::Lost)
    }

    fn refused(&self) -> bool {
        self.loss() == Some(Loss::OverTotal)
    }
}

/// Planes that keep the luma rows each write took, by slot; a slot's
/// reservation or release empties that slot's. With `slow`, each take
/// sleeps that long inside the write.
#[derive(Default)]
struct Kept {
    reserved: Vec<[u32; 3]>,
    released: Vec<u32>,
    luma: [Vec<u8>; 2],
    refuse: bool,
    slow: Option<Duration>,
}

impl Planes for Kept {
    fn reserve(&mut self, slot: u32, width: u32, height: u32) {
        self.reserved.push([slot, width, height]);
        self.luma[slot as usize].clear();
    }

    fn release(&mut self, slot: u32) {
        self.released.push(slot);
        self.luma[slot as usize].clear();
    }

    fn write(&mut self, layer: u32, width: u32, height: u32, fill: &mut Fill<'_>) -> bool {
        if self.refuse {
            return false;
        }
        let pitch = [width as usize, width.div_ceil(2) as usize];
        let len = pitch[0] * height as usize + 2 * pitch[1] * height.div_ceil(2) as usize;
        let mut bytes = vec![0; len];
        if let Some(d) = self.slow {
            std::thread::sleep(d);
        }
        let whole = fill(
            &mut |at, b| {
                bytes[at..at + b.len()].copy_from_slice(b);
                true
            },
            pitch,
        );
        if whole {
            self.luma[layer as usize] = bytes[..pitch[0] * height as usize].to_vec();
        }
        whole
    }
}

fn movies(a: Option<&Arc<Fake>>, b: Option<&Arc<Fake>>, start: Instant) -> Movies<Fake> {
    Movies::new([a.cloned(), b.cloned()], start)
}

/// A selection frame's movie work at frame time `at`: the admission
/// boundary, the frame's hook, and the frame presented, drawing every slot
/// that shows a frame.
fn tick(m: &mut Movies<Fake>, planes: &mut dyn Planes, at: i64, now: Instant) -> MovieShows {
    m.admit(planes, now);
    let shows = m.frame(planes, at, now);
    m.presented(true, drawn(shows));
    shows
}

/// The slots `shows` gives a frame to: those a build draws.
fn drawn(shows: MovieShows) -> [bool; MAX_MOVIES] {
    shows.map(|s| matches!(s, MovieShow::Frame { .. }))
}

/// Whether slot `slot`'s planes hold a frame whose every byte is `id`.
fn holds(planes: &Kept, slot: usize, id: u8) -> bool {
    !planes.luma[slot].is_empty() && planes.luma[slot].iter().all(|&v| v == id)
}

#[test]
fn each_state_shows_what_it_should() {
    let start = Instant::now();
    let (a, b) = (Fake::new(64, 32), Fake::new(16, 8));
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    // Both pending: nothing.
    assert_eq!(
        tick(&mut m, &mut planes, 0, start),
        [MovieShow::Nothing, MovieShow::Nothing]
    );
    // Slot 1 live while slot 0 is pending (the session judged its total):
    // admitted, its frame taken and shown.
    b.set(Status::Live);
    b.publish(1, 0, 9);
    let shows = tick(&mut m, &mut planes, 0, start);
    assert_eq!(shows[0], MovieShow::Nothing);
    assert_eq!(
        shows[1],
        MovieShow::Frame {
            uv: render::VIDEO_UV,
            layer: 1,
            size: [16, 8],
            bt601: true,
            full: false,
        }
    );
    assert!(b.staged.load(Ordering::SeqCst));
    assert_eq!(b.freed.load(Ordering::SeqCst), 1);
    assert_eq!(planes.reserved, [[1, 16, 8]]);
    assert!(planes.luma[1].iter().all(|&v| v == 9));
    assert!(b.reader.lock().unwrap().is_none(), "its reader taken");
    // Slot 0 absent: nothing.
    a.set(Status::Absent);
    assert_eq!(tick(&mut m, &mut planes, 0, start)[0], MovieShow::Nothing);
    // Live with no new frame: the last one still shows.
    assert!(matches!(
        tick(&mut m, &mut planes, 10, start)[1],
        MovieShow::Frame { .. }
    ));
    // Lost: black.
    b.lose(Loss::Failed);
    assert_eq!(tick(&mut m, &mut planes, 20, start)[1], MovieShow::Lost);
}

/// A live movie shows nothing before its first frame, and that frame is
/// the run's first whatever its stamp (design.md "Rendering and Skins",
/// skin movies): a clip whose first frame is stamped 1 ms shows it at once,
/// and its clock then runs from that stamp.
#[test]
fn a_live_movie_shows_nothing_before_its_first_frame() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    assert_eq!(tick(&mut m, &mut planes, 0, start)[0], MovieShow::Nothing);
    a.publish(1, 1_000, 3);
    // Planes that refuse the write keep it in the ring.
    planes.refuse = true;
    assert_eq!(tick(&mut m, &mut planes, 500, start)[0], MovieShow::Nothing);
    assert!(!a.staged.load(Ordering::SeqCst));
    planes.refuse = false;
    assert!(matches!(
        tick(&mut m, &mut planes, 999, start)[0],
        MovieShow::Frame { .. }
    ));
    assert!(holds(&planes, 0, 3), "the first frame, stamped 1 ms");
    // Its clock: 1 ms at frame time 999 µs.
    a.publish(2, 1_600, 4);
    let _ = tick(&mut m, &mut planes, 1_598, start);
    assert!(holds(&planes, 0, 3), "not due yet");
    let _ = tick(&mut m, &mut planes, 1_599, start);
    assert!(
        holds(&planes, 0, 4),
        "due at its stamp on the movie's clock"
    );
}

#[test]
fn movie_picks_newest_frame_at_or_before_screen_time() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    for (seq, stamp, id) in [(1, 0, 1), (2, 1_000, 2), (3, 2_000, 3)] {
        a.publish(seq, stamp, id);
    }
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    let _ = tick(&mut m, &mut planes, 10_000, start);
    assert!(holds(&planes, 0, 1), "its first frame first");
    let _ = tick(&mut m, &mut planes, 11_500, start);
    assert!(holds(&planes, 0, 2));
    let _ = tick(&mut m, &mut planes, 12_000, start);
    assert!(holds(&planes, 0, 3));
}

/// A movie the session refused at the pixel total draws nothing, never
/// black, in a frame and in a preparation's poll, and no planes are made
/// for it; its peer plays.
#[test]
fn a_refused_movie_draws_nothing_and_takes_no_planes() {
    let start = Instant::now();
    let (a, b) = (Fake::new(2560, 1440), Fake::new(1280, 720));
    a.set(Status::Live);
    a.publish(1, 0, 4);
    b.lose(Loss::OverTotal);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    let shows = tick(&mut m, &mut planes, 0, start);
    assert!(matches!(shows[0], MovieShow::Frame { .. }));
    assert_eq!(shows[1], MovieShow::Nothing);
    let (_, shows) = m.prepare(&mut planes, start);
    assert_eq!(shows[1], MovieShow::Nothing, "a poll too");
    assert_eq!(planes.reserved, [[0, 2560, 1440]]);
}

#[test]
fn a_ring_fault_loses_the_movie_in_that_frame_and_it_stays_black() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(16, 8));
    a.set(Status::Live);
    b.set(Status::Live);
    a.publish(1, 0, 1);
    b.publish(1, 0, 1);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    assert!(matches!(
        tick(&mut m, &mut planes, 0, start),
        [MovieShow::Frame { .. }, MovieShow::Frame { .. }]
    ));
    // Slot 1's state word set to one the protocol has not.
    let state = a.geometry.frame_start(1) - SLOT_HEADER_WORDS;
    a.words[state].store(99, Ordering::SeqCst);
    // The fault frame and the next 120 are black; the peer keeps
    // taking its frames.
    for k in 1..=121_u8 {
        let due = i64::from(k) * 1_000;
        b.publish(u64::from(k) + 1, due, k);
        let shows = tick(&mut m, &mut planes, due, start);
        assert_eq!(shows[0], MovieShow::Lost, "frame {k} after the fault");
        assert!(matches!(shows[1], MovieShow::Frame { .. }));
        assert!(planes.luma[1].iter().all(|&v| v == k), "peer frame {k}");
    }
    assert_eq!(a.loss(), Some(Loss::Fault));
    assert_eq!(b.loss(), None);
}

/// A movie keeps its planes and its shown frame when a larger peer is
/// admitted: only the peer's slot is reserved, and the movie's frame
/// stays shown on the frame of the admission and after it.
#[test]
fn a_peer_s_admission_leaves_a_movie_s_planes_and_frame() {
    let start = Instant::now();
    let (a, b) = (Fake::new(640, 360), Fake::new(1280, 720));
    a.set(Status::Live);
    a.publish(1, 0, 7);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    assert!(matches!(
        tick(&mut m, &mut planes, 0, start)[0],
        MovieShow::Frame { .. }
    ));
    b.set(Status::Live);
    b.publish(1, 10, 9);
    let shows = tick(&mut m, &mut planes, 10, start);
    assert!(matches!(shows[0], MovieShow::Frame { .. }), "{shows:?}");
    assert!(matches!(shows[1], MovieShow::Frame { .. }), "{shows:?}");
    assert_eq!(planes.reserved, [[0, 640, 360], [1, 1_280, 720]]);
    assert!(
        !planes.luma[0].is_empty() && planes.luma[0].iter().all(|&v| v == 7),
        "the movie's frame stays in its planes"
    );
    assert!(matches!(
        tick(&mut m, &mut planes, 20, start)[0],
        MovieShow::Frame { .. }
    ));
}

/// L5 at the frame's acceptance: a first frame whose take begins before
/// the preparation deadline and ends after it loses its movie, never
/// ready; the control, the same take ending before the deadline, is
/// ready.
#[test]
fn a_take_crossing_the_preparation_deadline_is_lost() {
    for crosses in [true, false] {
        let a = Fake::new(16, 8);
        a.set(Status::Live);
        a.publish(1, 0, 5);
        let left = Duration::from_millis(200);
        let now = Instant::now();
        let start = now - (preparation_deadline(now) - now) + left;
        let mut m = movies(Some(&a), None, start);
        let mut planes = Kept {
            slow: Some(if crosses { left * 2 } else { Duration::ZERO }),
            ..Kept::default()
        };
        m.admit(&mut planes, now);
        let (settled, shows) = m.prepare(&mut planes, now);
        if crosses {
            assert_eq!(shows[0], MovieShow::Lost, "crossing");
            assert_eq!(a.loss(), Some(Loss::Preparation));
            assert!(!a.staged.load(Ordering::SeqCst), "never ready");
        } else {
            assert!(matches!(shows[0], MovieShow::Frame { .. }), "control");
            assert!(a.staged.load(Ordering::SeqCst));
            assert_eq!(a.loss(), None);
        }
        assert!(settled, "{crosses}");
    }
}

/// A play's preparation takes each movie's first frame once and starts no
/// clock (design.md "Rendering and Skins", skin movies): polled again and
/// again over a ring holding frames 0 to 3, it holds frame 0 and frees
/// nothing more; a loading frame 1.2 s later shows frame 0, and the frames
/// then advance from it on the frame clock.
// Revert: a staged movie peeking at every poll -> "frame 0 held through
// the polls"; the clock started at the preparation's take -> "frame 0 at
// the first loading frame"; a preparation poll peeking a running movie at
// its own time -> "no frame ahead of the frame clock".
#[test]
fn a_preparation_takes_the_first_frame_once_and_starts_no_clock() {
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    let period = 100_000;
    for seq in 1..=4_u64 {
        let i = i64::try_from(seq - 1).unwrap();
        a.publish(seq, i * period, u8::try_from(seq).unwrap());
    }
    let now = Instant::now();
    let mut m = movies(Some(&a), None, now);
    let mut planes = Kept::default();
    m.admit(&mut planes, now);
    for _ in 0..5 {
        let (settled, shows) = m.prepare(&mut planes, now);
        assert!(settled);
        assert!(matches!(shows[0], MovieShow::Frame { .. }));
        m.presented(true, drawn(shows));
    }
    assert!(holds(&planes, 0, 1), "frame 0 held through the polls");
    assert_eq!(a.freed.load(Ordering::SeqCst), 1, "frame 0 alone freed");
    // The first loading frame, 1.2 s on.
    let at = 1_200_000;
    let (_, shows) = m.poll(&mut planes, at, now);
    m.presented(true, drawn(shows));
    assert!(holds(&planes, 0, 1), "frame 0 at the first loading frame");
    // A preparation poll after the clock started takes nothing, however
    // late its own clock: no second clock takes a frame ahead of the
    // screen's.
    let (_, shows) = m.prepare(&mut planes, now + Duration::from_secs(2));
    m.presented(true, drawn(shows));
    assert!(holds(&planes, 0, 1), "no frame ahead of the frame clock");
    let (_, shows) = m.poll(&mut planes, at + period, now);
    m.presented(true, drawn(shows));
    assert!(holds(&planes, 0, 2), "frame 1 a period on");
}

/// A movie's clock starts at the first presented frame that draws it
/// (design.md "Rendering and Skins", skin movies): a frame whose build drew
/// no quad of it (a destination gated off), a frame not presented, and a
/// presentation with no hook before it start nothing; the frame then
/// presented that draws it does, and frames advance from there.
// Revert: the clock started whatever the frame drew -> "frame 1 a period
// after the frame that drew it"; the clock started by a frame not
// presented -> the same.
#[test]
fn a_movie_s_clock_starts_only_at_a_presented_frame_that_draws_it() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    let period = 100_000;
    for seq in 1..=4_u64 {
        let i = i64::try_from(seq - 1).unwrap();
        a.publish(seq, i * period, u8::try_from(seq).unwrap());
    }
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    m.admit(&mut planes, start);
    let shows = m.frame(&mut planes, 1_000_000, start);
    assert!(matches!(shows[0], MovieShow::Frame { .. }));
    m.presented(true, [false, false]);
    let _ = m.frame(&mut planes, 1_100_000, start);
    m.presented(false, [true, false]);
    m.presented(true, [true, false]);
    let _ = m.frame(&mut planes, 2_000_000, start);
    assert!(holds(&planes, 0, 1), "frame 0 until its clock starts");
    m.presented(true, [true, false]);
    let _ = m.frame(&mut planes, 2_000_000 + period - 1, start);
    assert!(holds(&planes, 0, 1));
    let _ = m.frame(&mut planes, 2_000_000 + period, start);
    assert!(
        holds(&planes, 0, 2),
        "frame 1 a period after the frame that drew it"
    );
}

/// A movie drawn in a loading frame keeps its clock through READY, and its
/// peer, its destination gated off in the loading frames, starts at READY's
/// first presented frame that draws it (design.md "Rendering and Skins",
/// skin movies): one frame clock through loading, READY and play.
#[test]
fn a_movie_keeps_its_clock_through_ready_while_its_peer_starts_there() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(16, 8));
    a.set(Status::Live);
    b.set(Status::Live);
    let period = 100_000;
    for seq in 1..=4_u64 {
        let i = i64::try_from(seq - 1).unwrap();
        let id = u8::try_from(seq).unwrap();
        a.publish(seq, i * period, id);
        b.publish(seq, i * period, id + 10);
    }
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    m.admit(&mut planes, start);
    let (_, shows) = m.poll(&mut planes, 1_000_000, start);
    assert!(matches!(
        shows,
        [MovieShow::Frame { .. }, MovieShow::Frame { .. }]
    ));
    m.presented(true, [true, false]);
    // READY, 150 ms on.
    let ready = 1_150_000;
    let shows = m.frame(&mut planes, ready, start);
    m.presented(true, drawn(shows));
    assert!(holds(&planes, 0, 2), "a moves on its loading clock");
    assert!(holds(&planes, 1, 11), "b's first frame at READY");
    let _ = m.frame(&mut planes, ready + period, start);
    assert!(holds(&planes, 0, 3));
    assert!(holds(&planes, 1, 12), "b a period after READY");
}

/// A clip that is not looping and ended before the first take keeps its
/// queued frames and its last one (design.md "Rendering and Skins", video
/// frames): its first frame is taken, then the next, and the last stays.
#[test]
fn a_short_clip_that_ended_before_its_first_take_plays_its_frames() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    a.publish(1, 0, 1);
    a.publish(2, 40_000, 2);
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    m.admit(&mut planes, start);
    let (settled, _) = m.prepare(&mut planes, start);
    assert!(settled);
    let _ = tick(&mut m, &mut planes, 500_000, start);
    assert!(holds(&planes, 0, 1));
    let _ = tick(&mut m, &mut planes, 540_000, start);
    assert!(holds(&planes, 0, 2));
    let _ = tick(&mut m, &mut planes, 5_000_000, start);
    assert!(holds(&planes, 0, 2), "the last frame kept");
}

/// A poll ends with a fresh clock sample: a first frame's take that runs
/// past the preparation deadline loses its movie, and a peer still pending
/// is lost and shows black in the same poll, the preparation settled,
/// rather than at the next poll.
#[test]
fn a_take_crossing_the_deadline_expires_a_pending_peer_in_the_same_poll() {
    let (a, b) = (Fake::new(16, 8), Fake::new(16, 8));
    a.set(Status::Live);
    a.publish(1, 0, 5);
    let left = Duration::from_millis(200);
    let now = Instant::now();
    let start = now - (preparation_deadline(now) - now) + left;
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept {
        slow: Some(left * 2),
        ..Kept::default()
    };
    m.admit(&mut planes, now);
    let (settled, shows) = m.prepare(&mut planes, now);
    assert_eq!(a.loss(), Some(Loss::Preparation));
    assert_eq!(b.loss(), Some(Loss::Preparation), "the pending peer");
    assert_eq!(shows, [MovieShow::Lost, MovieShow::Lost]);
    assert!(settled);
}

/// A movie lost after its admission gives its planes back at the next
/// boundary, its peer's left as they are; the lost one shows black.
#[test]
fn a_lost_movie_s_planes_go_at_the_next_boundary() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(32, 16));
    a.set(Status::Live);
    b.set(Status::Live);
    a.publish(1, 0, 3);
    b.publish(1, 0, 4);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    let _ = tick(&mut m, &mut planes, 0, start);
    a.lose(Loss::Fault);
    let shows = tick(&mut m, &mut planes, 10, start);
    assert_eq!(shows[0], MovieShow::Lost);
    assert!(matches!(shows[1], MovieShow::Frame { .. }));
    assert_eq!(planes.released, [0]);
    assert!(planes.luma[1].iter().all(|&v| v == 4), "the peer's frame");
}

/// A preparation's polls are readiness only (design.md "Scope of No
/// Allocation", the admission boundary): a live movie not admitted shows
/// nothing and stays unsettled however often it is polled, and no planes
/// are reserved; once a boundary admitted it, the next poll takes its
/// first frame.
// Revert: the boundary run in `Movies::prepare` -> "a poll reserved".
#[test]
fn a_preparation_poll_reserves_no_planes() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    a.publish(1, 0, 6);
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    for _ in 0..3 {
        let (settled, shows) = m.prepare(&mut planes, start);
        assert!(planes.reserved.is_empty(), "a poll reserved");
        assert!(!settled);
        assert_eq!(shows[0], MovieShow::Nothing);
    }
    m.admit(&mut planes, start);
    assert_eq!(planes.reserved, [[0, 16, 8]]);
    let (settled, shows) = m.prepare(&mut planes, start);
    assert!(settled);
    assert!(matches!(shows[0], MovieShow::Frame { .. }));
}

/// A movie whose screen never finds slack for the boundary is admitted at
/// the turn after the one that first saw it waiting, once: the first turn
/// leaves the boundary a frame's interval, the second reserves its
/// planes, and later turns reserve nothing more. A movie still pending at
/// a turn is not marked, so it too waits one turn once live.
// Revert: a turn admitting at first sight (`std::mem::replace(..)`
// dropped) -> "the first turn left it to the boundary".
#[test]
fn a_movie_without_slack_is_admitted_at_its_second_turn() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(32, 16));
    a.set(Status::Live);
    a.publish(1, 0, 7);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    m.turn(&mut planes, start);
    assert!(
        planes.reserved.is_empty(),
        "the first turn left it to the boundary"
    );
    assert_eq!(m.frame(&mut planes, 0, start)[0], MovieShow::Nothing);
    m.turn(&mut planes, start);
    assert_eq!(planes.reserved, [[0, 16, 8]], "admitted at the second");
    assert!(matches!(
        m.frame(&mut planes, 0, start)[0],
        MovieShow::Frame { .. }
    ));
    // The peer goes live now: one turn to see it, one to admit it.
    b.set(Status::Live);
    b.publish(1, 0, 8);
    m.turn(&mut planes, start);
    assert_eq!(planes.reserved, [[0, 16, 8]]);
    m.turn(&mut planes, start);
    for _ in 0..5 {
        m.turn(&mut planes, start);
    }
    assert_eq!(
        planes.reserved,
        [[0, 16, 8], [1, 32, 16]],
        "each admitted once"
    );
}

#[test]
fn the_preparation_deadline_loses_what_is_not_settled() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(16, 8));
    b.set(Status::Live);
    b.publish(1, 0, 1);
    let mut m = movies(Some(&a), Some(&b), start);
    assert!(!m.settle(start));
    let deadline = preparation_deadline(start);
    assert!(m.settle(deadline));
    assert_eq!(a.loss(), Some(Loss::Preparation));
    assert_eq!(b.loss(), Some(Loss::Preparation));
    let mut planes = Kept::default();
    assert_eq!(
        tick(&mut m, &mut planes, 0, deadline + Duration::from_millis(1)),
        [MovieShow::Lost, MovieShow::Lost]
    );
}

/// Planes whose writes land in a buffer made beforehand.
#[cfg(feature = "dev-tools")]
struct Preallocated {
    bytes: Vec<u8>,
}

#[cfg(feature = "dev-tools")]
impl Planes for Preallocated {
    fn reserve(&mut self, _slot: u32, _width: u32, _height: u32) {}

    fn release(&mut self, _slot: u32) {}

    fn write(&mut self, _layer: u32, width: u32, _height: u32, fill: &mut Fill<'_>) -> bool {
        let pitch = [width as usize, width.div_ceil(2) as usize];
        let bytes = &mut self.bytes;
        fill(
            &mut |at, b| match bytes.get_mut(at..at + b.len()) {
                Some(d) => {
                    d.copy_from_slice(b);
                    true
                }
                None => false,
            },
            pitch,
        )
    }
}

/// The movie hook allocates nothing (design.md "Scope of No Allocation"):
/// frames that peek a movie admitted at the boundary before them (its first
/// frame by a first peek), take its due frames into the planes, show them
/// and start its clock at the first presented, each counted on this thread
/// whole, the take's sink included; with a positive control.
#[cfg(feature = "dev-tools")]
#[test]
fn movie_upload_hook_alloc() {
    use crate::alloc_count::this_thread;
    let start = Instant::now();
    let a = Fake::new(64, 32);
    a.set(Status::Live);
    // The worker's four slots, published before the count.
    for seq in 1..=4_u64 {
        a.publish(
            seq,
            i64::try_from(seq).unwrap() * 16_667,
            u8::try_from(seq).unwrap(),
        );
    }
    let mut m = movies(Some(&a), None, start);
    let mut planes = Preallocated {
        bytes: vec![0; 64 * 32 * 2],
    };
    m.admit(&mut planes, start);
    let before = this_thread();
    let mut shown = 0;
    for k in 1..=4_i64 {
        let shows = m.frame(&mut planes, k * 16_667, start);
        m.presented(true, drawn(shows));
        if matches!(shows[0], MovieShow::Frame { .. }) {
            shown += 1;
        }
    }
    let after = this_thread();
    assert_eq!(after - before, 0, "allocations");
    assert_eq!(shown, 4, "each due frame taken and shown");
    assert_eq!(planes.bytes[0], 4, "the newest frame's bytes");
    // The positive control: an allocation on this thread is counted.
    let v = std::hint::black_box(vec![0_u8; 16]);
    assert!(this_thread() > after);
    drop(v);
}

/// A stall before a movie's first frame loses it at the preparation
/// deadline, opaque black; once its first frame is staged the deadline no
/// longer governs it (design.md "Threat Model" T02): a peer staged before
/// the deadline keeps its frame through a successor's stall past it, and
/// after it the ring refilled with late frames shows the newest of them,
/// late, rather than nothing, until its due frame comes.
// Revert: a peek taking the oldest due frame -> "the newest late frame
// after the stall".
#[test]
fn a_first_frame_stall_loses_the_movie_and_a_later_one_keeps_its_frame() {
    let start = Instant::now();
    let (a, b) = (Fake::new(16, 8), Fake::new(16, 8));
    a.set(Status::Live);
    b.set(Status::Live);
    let period = 16_667;
    b.publish(1, 0, 46);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    m.admit(&mut planes, start);
    let (settled, _) = m.prepare(&mut planes, start);
    assert!(!settled, "a has no first frame");
    // Past the deadline: a, whose first frame never came, is lost; b,
    // staged, is not.
    let late = preparation_deadline(start) + Duration::from_millis(1);
    let shows = m.frame(&mut planes, 1_000_000, late);
    m.presented(true, drawn(shows));
    assert_eq!(shows[0], MovieShow::Lost, "the first frame's stall");
    assert_eq!(a.loss(), Some(Loss::Preparation));
    assert!(matches!(shows[1], MovieShow::Frame { .. }));
    assert_eq!(b.loss(), None);
    // b's successor stalls: nothing new while the clock runs on.
    for at in [1_500_000, 4_000_000] {
        assert!(matches!(
            m.frame(&mut planes, at, late)[1],
            MovieShow::Frame { .. }
        ));
        assert!(holds(&planes, 1, 46), "the frame kept");
    }
    // The ring refills with frames from 800 ms while b's clock is at 1 s.
    for (seq, id) in (2..=5).zip(50..) {
        let i = i64::try_from(seq - 2).unwrap();
        b.publish(seq, 800_000 + i * period, id);
    }
    let _ = m.frame(&mut planes, 2_000_000, late);
    assert!(
        holds(&planes, 1, 53),
        "the newest late frame after the stall"
    );
    b.publish(6, 983_333, 60);
    let _ = m.frame(&mut planes, 2_000_000, late);
    assert!(holds(&planes, 1, 60), "the due frame");
    assert_eq!(b.loss(), None);
}
