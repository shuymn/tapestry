//! The hook against a portable ring: the worker's side written with
//! [`Writer`] into words of the test's own, and planes that keep what each
//! take wrote.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use skin_model::media::{Geometry, Reader, SLOT_HEADER_WORDS, Window, Writer};
use skin_model::movie::{MAX_MOVIE_PIXELS, MovieShow};

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
            .publish(self.words.as_slice(), slot, &frame, seq, 1, stamp)
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
}

/// Planes that keep the luma rows each write took, by layer.
#[derive(Default)]
struct Kept {
    reserved: Vec<[u32; 3]>,
    luma: [Vec<u8>; 2],
    refuse: bool,
}

impl Planes for Kept {
    fn reserve(&mut self, width: u32, height: u32, layers: u32) {
        self.reserved.push([width, height, layers]);
    }

    fn write(&mut self, layer: u32, width: u32, height: u32, fill: &mut Fill<'_>) -> bool {
        if self.refuse {
            return false;
        }
        let pitch = [width as usize, width.div_ceil(2) as usize];
        let len = pitch[0] * height as usize + 2 * pitch[1] * height.div_ceil(2) as usize;
        let mut bytes = vec![0; len];
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

    fn uv(&self, width: u32, height: u32) -> [f32; 4] {
        [0.0, 0.0, width as f32, height as f32]
    }
}

fn movies(a: Option<&Arc<Fake>>, b: Option<&Arc<Fake>>, start: Instant) -> Movies<Fake> {
    Movies::new([a.cloned(), b.cloned()], start)
}

#[test]
fn second_movie_that_fits_is_admitted() {
    let hd = 1920 * 1080;
    assert_eq!(admit(&[Earlier::Counts(hd)], hd), Verdict::Admit);
    assert!(2 * hd <= MAX_MOVIE_PIXELS);
}

#[test]
fn second_movie_over_the_total_is_refused() {
    assert_eq!(
        admit(&[Earlier::Counts(2560 * 1440)], 1280 * 720),
        Verdict::Refuse
    );
    // A refused or lost earlier slot counts nothing.
    assert_eq!(admit(&[Earlier::Free], MAX_MOVIE_PIXELS), Verdict::Admit);
}

#[test]
fn first_movie_alone_is_judged_at_the_boundary() {
    assert_eq!(admit(&[], MAX_MOVIE_PIXELS), Verdict::Admit);
    assert_eq!(admit(&[], MAX_MOVIE_PIXELS + 1), Verdict::Refuse);
}

#[test]
fn an_unsettled_earlier_slot_defers_the_decision() {
    assert_eq!(admit(&[Earlier::Unsettled], 1), Verdict::Defer);
}

#[test]
fn each_state_shows_what_it_should() {
    let start = Instant::now();
    let (a, b) = (Fake::new(64, 32), Fake::new(16, 8));
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    // Both pending: nothing.
    assert_eq!(
        m.frame(&mut planes, 0, start),
        [MovieShow::Nothing, MovieShow::Nothing]
    );
    // Slot 1 live while slot 0 is pending: deferred, its reader kept.
    b.set(Status::Live);
    b.publish(1, 0, 9);
    assert_eq!(
        m.frame(&mut planes, 0, start),
        [MovieShow::Nothing, MovieShow::Nothing]
    );
    assert!(b.reader.lock().unwrap().is_some());
    // Slot 0 absent: slot 1 admitted, its frame taken and shown.
    a.set(Status::Absent);
    let shows = m.frame(&mut planes, 0, start);
    assert_eq!(shows[0], MovieShow::Nothing);
    assert_eq!(
        shows[1],
        MovieShow::Frame {
            uv: [0.0, 0.0, 16.0, 8.0],
            layer: 1,
            size: [16, 8],
            bt601: true,
            full: false,
        }
    );
    assert!(b.staged.load(Ordering::SeqCst));
    assert_eq!(b.freed.load(Ordering::SeqCst), 1);
    assert_eq!(planes.reserved, [[16, 8, 2]]);
    assert!(planes.luma[1].iter().all(|&v| v == 9));
    // Live with no new frame: the last one still shows.
    assert!(matches!(
        m.frame(&mut planes, 10, start)[1],
        MovieShow::Frame { .. }
    ));
    // Lost: black.
    b.lose(Loss::Failed);
    assert_eq!(m.frame(&mut planes, 20, start)[1], MovieShow::Lost);
}

#[test]
fn a_live_movie_shows_nothing_before_its_first_frame() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    assert_eq!(m.frame(&mut planes, 0, start)[0], MovieShow::Nothing);
    // A frame not due yet stays in the ring.
    a.publish(1, 1_000, 3);
    assert_eq!(m.frame(&mut planes, 999, start)[0], MovieShow::Nothing);
    assert!(!a.staged.load(Ordering::SeqCst));
    // Planes that refuse the write keep it in the ring too.
    planes.refuse = true;
    assert_eq!(m.frame(&mut planes, 1_000, start)[0], MovieShow::Nothing);
    planes.refuse = false;
    assert!(matches!(
        m.frame(&mut planes, 1_000, start)[0],
        MovieShow::Frame { .. }
    ));
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
    let _ = m.frame(&mut planes, 1_500, start);
    assert!(planes.luma[0].iter().all(|&v| v == 2));
    let _ = m.frame(&mut planes, 2_000, start);
    assert!(planes.luma[0].iter().all(|&v| v == 3));
}

#[test]
fn a_movie_past_the_total_is_refused_and_draws_nothing() {
    let start = Instant::now();
    let (a, b) = (Fake::new(2560, 1440), Fake::new(1280, 720));
    a.set(Status::Live);
    b.set(Status::Live);
    let mut m = movies(Some(&a), Some(&b), start);
    let mut planes = Kept::default();
    let shows = m.frame(&mut planes, 0, start);
    assert_eq!(shows[1], MovieShow::Nothing);
    assert_eq!(b.loss(), Some(Loss::OverTotal));
    // Refused stays nothing, never black.
    assert_eq!(m.frame(&mut planes, 10, start)[1], MovieShow::Nothing);
    assert_eq!(planes.reserved, [[2560, 1440, 2]]);
}

#[test]
fn a_ring_fault_loses_the_movie_in_that_frame() {
    let start = Instant::now();
    let a = Fake::new(16, 8);
    a.set(Status::Live);
    a.publish(1, 0, 1);
    let mut m = movies(Some(&a), None, start);
    let mut planes = Kept::default();
    assert!(matches!(
        m.frame(&mut planes, 0, start)[0],
        MovieShow::Frame { .. }
    ));
    // Slot 1's state word set to one the protocol has not.
    let state = a.geometry.frame_start(1) - SLOT_HEADER_WORDS;
    a.words[state].store(99, Ordering::SeqCst);
    assert_eq!(m.frame(&mut planes, 10, start)[0], MovieShow::Lost);
    assert_eq!(a.loss(), Some(Loss::Fault));
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
        m.frame(&mut planes, 0, deadline + Duration::from_millis(1)),
        [MovieShow::Lost, MovieShow::Lost]
    );
}
