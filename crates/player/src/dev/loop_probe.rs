//! The transition probe (#611): marks of the render loop's steps, the
//! game's transitions and the renderer's uploads, drops and first uses
//! (`render::probe`), on one `Instant` base, buffered and printed after a
//! scenario's run as `dev: probe <µs> <tag> <value>` lines. Off unless
//! `TAPESTRY_LOOP_PROBE` is set when the process starts ([`init`]).
//!
//! A mark allocates nothing: the buffer is reserved at [`init`], before any
//! play, and a mark past its room is counted, not kept (the count is
//! printed with the marks). Off, a mark is one atomic load.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// The marks one run keeps.
const ROOM: usize = 1 << 20;

/// One mark: µs since the recorder's base, its tag and its value.
pub type Mark = (u64, &'static str, i64);

/// Bounded storage of marks: reserved once when turned on, it keeps marks
/// while it has room and counts the rest, so a mark never allocates.
pub struct Recorder {
    on: AtomicBool,
    base: OnceLock<Instant>,
    log: Mutex<Vec<Mark>>,
    dropped: AtomicU64,
}

impl Recorder {
    pub const fn new() -> Self {
        Self {
            on: AtomicBool::new(false),
            base: OnceLock::new(),
            log: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// Reserves room for `room` marks, takes the time base and turns the
    /// recorder on. Allocates.
    pub fn turn_on(&self, room: usize) {
        if let Ok(mut l) = self.log.lock() {
            let kept = l.len();
            l.reserve_exact(room.saturating_sub(kept));
        }
        let _ = self.base.set(Instant::now());
        self.on.store(true, Ordering::Release);
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// Keeps the mark if there is room, else counts it as dropped.
    pub fn mark(&self, tag: &'static str, v: i64) {
        if !self.on() {
            return;
        }
        let Some(base) = self.base.get() else {
            return;
        };
        let t = u64::try_from(base.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Ok(mut l) = self.log.lock() {
            if l.len() < l.capacity() {
                l.push((t, tag, v));
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The marks not kept for want of room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// `f` on the marks kept so far.
    pub fn with_marks<R>(&self, f: impl FnOnce(&[Mark]) -> R) -> Option<R> {
        self.log.lock().ok().map(|l| f(&l))
    }
}

static REC: Recorder = Recorder::new();

/// Turns the probe on when `TAPESTRY_LOOP_PROBE` is set: its buffer
/// reserved, its time base taken and the renderer's marks routed here.
/// Called once, at the start of the process.
pub fn init() {
    if std::env::var_os("TAPESTRY_LOOP_PROBE").is_none() {
        return;
    }
    turn_on();
}

/// [`init`]'s work, whatever the environment.
fn turn_on() {
    REC.turn_on(ROOM);
    render::probe::set_sink(mark);
}

pub fn on() -> bool {
    REC.on()
}

pub fn mark(tag: &'static str, v: i64) {
    REC.mark(tag, v);
}

pub fn us(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// Result frames left to probe after a result's first view installed.
pub static RESULT_FRAMES: AtomicU32 = AtomicU32::new(0);

/// Whether the current result frame is one of those probed.
pub static RF_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Every level of every page of `pages`, in bytes (RGBA8, CPU side).
pub fn pages_level_bytes(pages: &skin_model::PackedPages) -> i64 {
    let n: usize = pages
        .pages()
        .iter()
        .flat_map(|p| (0..skin_model::MIP_LEVELS).filter_map(|k| p.level(k)))
        .map(<[u8]>::len)
        .sum();
    i64::try_from(n).unwrap_or(-1)
}

/// Tags render-side uploads with a caller until dropped (the probe on).
pub struct Caller(Option<u8>);

impl Caller {
    pub fn set(c: u8) -> Self {
        Self(on().then(|| render::probe::set_caller(c)))
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        if let Some(c) = self.0 {
            render::probe::set_caller(c);
        }
    }
}

/// Prints the marks kept, then how many did not fit.
pub fn dump() {
    if !on() {
        return;
    }
    REC.with_marks(|l| {
        for (t, tag, v) in l {
            println!("dev: probe {t} {tag} {v}");
        }
    });
    println!("dev: probe-dropped {}", REC.dropped());
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::AtomicU32;

    use super::Recorder;
    use crate::alloc_count::this_thread;

    /// Turns the process's probe on, as `TAPESTRY_LOOP_PROBE` would at
    /// start: for a test run in a process of its own (nextest).
    pub(crate) fn turn_on() {
        super::turn_on();
    }

    /// The marks kept so far.
    pub(crate) fn marks() -> Vec<super::Mark> {
        super::REC
            .with_marks(<[super::Mark]>::to_vec)
            .unwrap_or_default()
    }

    /// The marks not kept for want of room.
    pub(crate) fn dropped() -> u64 {
        super::REC.dropped()
    }

    /// The probe's in-play hooks, on, allocate nothing: a mark kept, marks
    /// past the room (counted as dropped, not kept), a result frame's
    /// countdown and the in-play switch; with a positive control.
    #[test]
    fn transition_probe_marks_alloc() {
        let rec = Recorder::new();
        rec.turn_on(4);
        let frames = AtomicU32::new(2);
        let before = this_thread();
        for k in 0..6 {
            rec.mark("loop", k);
        }
        let counted = [
            render::probe::count_down(&frames),
            render::probe::count_down(&frames),
            render::probe::count_down(&frames),
        ];
        render::probe::set_in_play(true);
        render::probe::set_in_play(false);
        let after = this_thread();
        assert_eq!(after - before, 0, "allocations");
        assert_eq!(counted, [true, true, false]);
        assert_eq!(
            rec.with_marks(|l| l.iter().map(|m| m.2).collect::<Vec<_>>()),
            Some(vec![0, 1, 2, 3])
        );
        assert_eq!(rec.dropped(), 2);
        // The positive control: an allocation on this thread is counted.
        let v = std::hint::black_box(vec![0_u8; 16]);
        assert!(this_thread() > after);
        drop(v);
    }
}
