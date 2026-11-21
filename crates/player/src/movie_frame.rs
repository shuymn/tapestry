//! The render side's skin movie hook (design.md "Rendering and Skins",
//! skin movies; "Threat Model" T02): on every play frame and every
//! selection frame, before the skin's build, and on every poll of a play's
//! preparation, it reads each movie of the screen in slot order and says
//! what its destinations show ([`MovieShows`]).
//!
//! Admission is a preparation boundary of its own ([`Movies::admit`]),
//! never part of a frame: a play's preparation polls and visible loading
//! frames, each selection frame before its preparation and build, and a
//! screen built again after its device's recovery. There, in slot order, a
//! lost or refused movie's planes are released first, then each live movie
//! not judged yet is admitted against the screen's pixel total
//! ([`admit`]): once every earlier slot is settled, a movie whose probed
//! size with the earlier slots' admitted live movies passes
//! [`MAX_MOVIE_PIXELS`] is refused (lost for [`Loss::OverTotal`]) and
//! draws nothing. An admitted movie takes its ring's reader once and its
//! planes are reserved at its size, never another slot's: that allocates
//! on the GPU, and a peer's admission leaves a movie's planes and its shown
//! frame as they are. A slot's planes are released before a later slot is
//! judged, so the planes the GPU holds are those of the movies the total
//! admitted.
//!
//! The frame's hook ([`Movies::frame`]), per slot: a lost movie shows
//! [`MovieShow::Lost`]. An admitted movie whose planes are reserved peeks
//! at the screen's time and, when a frame is due, takes it straight into
//! the renderer's upload buffer ([`Planes::write`]); a fault loses it in
//! the same frame. It shows its newest frame once one was taken on the
//! screen, and nothing before. A first frame whose take ends past the
//! preparation deadline loses its movie (L5): it is judged when the frame
//! is accepted, not when the poll began. Anything else shows nothing.
//! Under dev-tools the app region covers each peek and each take, inside
//! the upload's mapping, and nothing of wgpu's.
//!
//! A play's preparation polls ([`Movies::prepare`]) select frames at the
//! time since the movies opened, bounded by the earliest screen time READY
//! can open at (the play skin's loading minimum, `floor`): with no loading
//! lifetime the screen clock's origin is anchored the loading minimum
//! before READY, so a bound below every screen time READY can start at
//! keeps the frame held at READY from ever being ahead of the screen
//! clock. A visible loading state polls them at its own screen time
//! instead ([`Movies::poll`]), the clock READY opens on, so its frames move
//! while it draws. Each poll ends with a fresh clock sample: past the
//! deadline, every movie not settled is lost and shows black in that poll,
//! however long its takes ran.
//!
//! Every slot's state is made when the screen's movies are opened: the
//! frame's hook allocates nothing, panics nowhere and never waits on the
//! media session.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use skin_model::media::Reader;
use skin_model::media::control::{Matrix, Range};
use skin_model::movie::{MAX_MOVIE_PIXELS, MAX_MOVIES, MovieShow, MovieShows};

use crate::media_host::session::{Loss, Movie, Status, preparation_deadline};

/// A take into the planes: it writes a frame through the sink it is given
/// (a byte offset and the bytes), each plane's rows at the pitch it is
/// given (luma, chroma), and says whether the frame is whole.
pub type Fill<'a> = dyn FnMut(&mut dyn FnMut(usize, &[u8]) -> bool, [usize; 2]) -> bool + 'a;

/// Where a movie's frames go: the renderer's movie planes, or a test's.
pub trait Planes {
    /// Replaces slot `slot`'s planes with ones of a movie of `width` ×
    /// `height`, the other slot's left as they are
    /// ([`render::Renderer::video_reserve`]).
    fn reserve(&mut self, slot: u32, width: u32, height: u32);
    /// Releases slot `slot`'s planes, the other slot's left as they are
    /// ([`render::Renderer::video_release`]).
    fn release(&mut self, slot: u32);
    /// Writes a frame of `width` × `height` into slot `slot` through
    /// `fill` ([`render::Renderer::video_write_with`]): whether it did.
    fn write(&mut self, slot: u32, width: u32, height: u32, fill: &mut Fill<'_>) -> bool;
}

impl Planes for render::Renderer {
    fn reserve(&mut self, slot: u32, width: u32, height: u32) {
        self.video_reserve(slot, width, height);
    }

    fn release(&mut self, slot: u32) {
        self.video_release(slot);
    }

    fn write(&mut self, slot: u32, width: u32, height: u32, fill: &mut Fill<'_>) -> bool {
        self.video_write_with(slot, width, height, |sink, pitch| fill(sink, pitch))
    }
}

/// A live movie's ring and probe as the hook reads them.
pub struct LiveView<'a> {
    pub words: &'a [AtomicU64],
    pub reader: &'a Mutex<Option<Reader>>,
    pub width: u32,
    pub height: u32,
    pub bt601: bool,
    pub full: bool,
}

/// A movie as the hook sees it: the media session's [`Movie`], or a
/// test's.
pub trait Source {
    /// Where it stands.
    fn status(&self) -> Status;
    /// Its ring and probe, once live.
    fn live(&self) -> Option<LiveView<'_>>;
    /// Loses it for `why`, unless lost already.
    fn lose(&self, why: Loss);
    /// Its first frame was staged.
    fn staged_first(&self);
    /// The reader freed `total` slots over the ring's life; a rise signals
    /// the worker.
    fn freed(&self, total: u64);
    /// The preparation deadline passed.
    fn expire(&self);
    /// Whether it reached a terminal outcome for readiness.
    fn settled(&self) -> bool;
}

impl Source for Movie {
    fn status(&self) -> Status {
        Self::status(self)
    }

    fn live(&self) -> Option<LiveView<'_>> {
        Self::live(self).map(|l| LiveView {
            words: l.ring.words(),
            reader: &l.reader,
            width: l.width,
            height: l.height,
            bt601: l.matrix == Matrix::Bt601,
            full: l.range == Range::Full,
        })
    }

    fn lose(&self, why: Loss) {
        let _ = Self::lose(self, why);
    }

    fn staged_first(&self) {
        Self::staged_first(self);
    }

    fn freed(&self, total: u64) {
        Self::freed(self, total);
    }

    fn expire(&self) {
        Self::expire(self);
    }

    fn settled(&self) -> bool {
        Self::settled(self)
    }
}

/// An earlier slot, as the pixel total judges a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Earlier {
    /// Not settled yet: pending, or live and not judged yet.
    Unsettled,
    /// Admitted and live, of this many luma pixels.
    Counts(u64),
    /// No movie, absent, lost or refused: it counts nothing.
    Free,
}

/// The pixel total's verdict on a movie going live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// An earlier slot is not settled: asked again at the next boundary.
    Defer,
    /// It fits with the earlier slots' live movies.
    Admit,
    /// It passes the total: refused, it draws nothing.
    Refuse,
}

/// Judges a movie of `pixels` luma pixels after the `earlier` slots, in
/// slot order: deferred while one is not settled, admitted when the
/// earlier slots' live movies and it total at most [`MAX_MOVIE_PIXELS`],
/// refused past it.
pub fn admit(earlier: &[Earlier], pixels: u64) -> Verdict {
    let mut total = pixels;
    for e in earlier {
        match e {
            Earlier::Unsettled => return Verdict::Defer,
            Earlier::Counts(p) => total = total.saturating_add(*p),
            Earlier::Free => {}
        }
    }
    if total <= MAX_MOVIE_PIXELS {
        Verdict::Admit
    } else {
        Verdict::Refuse
    }
}

/// A slot's admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    Undecided,
    Admitted,
    Refused,
}

/// One slot of the screen's movies.
struct Slot<S> {
    movie: Option<Arc<S>>,
    reader: Option<Reader>,
    admission: Admission,
    size: [u32; 2],
    bt601: bool,
    full: bool,
    /// A frame was taken into the planes on this screen (and the planes
    /// have not been replaced since).
    shown: bool,
    /// Its first frame was staged.
    staged: bool,
}

impl<S: Source> Slot<S> {
    const fn new(movie: Option<Arc<S>>) -> Self {
        Self {
            movie,
            reader: None,
            admission: Admission::Undecided,
            size: [0, 0],
            bt601: false,
            full: false,
            shown: false,
            staged: false,
        }
    }

    /// How a later slot's admission sees it.
    fn earlier(&self) -> Earlier {
        let Some(movie) = &self.movie else {
            return Earlier::Free;
        };
        match (self.admission, movie.status()) {
            (Admission::Refused, _) | (_, Status::Absent | Status::Lost) => Earlier::Free,
            (Admission::Admitted, Status::Live) => {
                let [w, h] = self.size;
                Earlier::Counts(u64::from(w) * u64::from(h))
            }
            _ => Earlier::Unsettled,
        }
    }
}

/// A screen's movies, by slot, with what the hook keeps of each.
pub struct Movies<S = Movie> {
    slots: [Slot<S>; MAX_MOVIES],
    /// Each slot's planes' size as last reserved through this value; zero
    /// before and once released.
    reserved: [[u32; 2]; MAX_MOVIES],
    /// The screen's preparation deadline (L5).
    deadline: Instant,
    /// When its preparation started.
    start: Instant,
    /// The latest movie time a preparation poll selects at (µs): the
    /// earliest screen time READY can open at.
    floor: i64,
}

impl<S: Source> Movies<S> {
    /// The movies of a screen whose preparation started at `start`, by
    /// slot (`None` for an absent one).
    pub fn new(movies: [Option<Arc<S>>; MAX_MOVIES], start: Instant) -> Self {
        let [a, b] = movies;
        Self {
            slots: [Slot::new(a), Slot::new(b)],
            reserved: [[0; 2]; MAX_MOVIES],
            deadline: preparation_deadline(start),
            start,
            floor: i64::MAX,
        }
    }

    /// These movies with a preparation whose polls select frames at most
    /// `floor` µs into the movie: the earliest screen time READY can open
    /// at.
    #[must_use]
    pub const fn floored(mut self, floor: i64) -> Self {
        self.floor = floor;
        self
    }

    /// A preparation poll at `now`: [`poll`](Self::poll) at the time since
    /// the movies opened, bounded by the floor.
    pub fn prepare(&mut self, planes: &mut dyn Planes, now: Instant) -> (bool, MovieShows) {
        let since = i64::try_from(now.saturating_duration_since(self.start).as_micros())
            .unwrap_or(i64::MAX);
        self.poll(planes, since.min(self.floor), now)
    }

    /// A poll of the preparation at movie time `due` (µs) and `now`: the
    /// admission boundary, then the frame's hook; then, at a fresh clock
    /// sample, the preparation deadline applied again, so a movie it loses
    /// while the takes ran shows black in this poll. Whether every movie is
    /// settled, and what each slot's destinations show (for the loading
    /// frames).
    pub fn poll(&mut self, planes: &mut dyn Planes, due: i64, now: Instant) -> (bool, MovieShows) {
        self.admit(planes, now);
        let mut shows = self.frame(planes, due, now);
        let settled = self.settle(Instant::now().max(now));
        for (show, slot) in shows.iter_mut().zip(&self.slots) {
            if slot.admission != Admission::Refused
                && slot
                    .movie
                    .as_ref()
                    .is_some_and(|m| m.status() == Status::Lost)
            {
                *show = MovieShow::Lost;
            }
        }
        (settled, shows)
    }

    /// Past the preparation deadline at `now`, every movie not settled is
    /// lost; then whether every movie is settled. Never waits.
    pub fn settle(&self, now: Instant) -> bool {
        let movies = self.slots.iter().filter_map(|s| s.movie.as_deref());
        if now >= self.deadline {
            for m in movies.clone() {
                m.expire();
            }
        }
        movies.clone().all(|m| m.settled())
    }

    /// The admission boundary at `now`, a preparation's and never a
    /// frame's: the preparation deadline applied, then in slot order a lost
    /// or refused movie's planes released, and a live movie not judged yet
    /// admitted or refused against the earlier slots, its planes reserved
    /// once admitted (again after the device's recovery). It allocates on
    /// the GPU only.
    pub fn admit(&mut self, planes: &mut dyn Planes, now: Instant) {
        let _ = self.settle(now);
        for i in 0..MAX_MOVIES {
            self.slot_admit(planes, i);
        }
    }

    fn slot_admit(&mut self, planes: &mut dyn Planes, i: usize) {
        let mut earlier = [Earlier::Free; MAX_MOVIES];
        for (e, s) in earlier.iter_mut().zip(self.slots.iter().take(i)) {
            *e = s.earlier();
        }
        let earlier = earlier.get(..i).unwrap_or_default();
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        let Some(movie) = slot.movie.clone() else {
            return;
        };
        let status = movie.status();
        if slot.admission == Admission::Refused || status == Status::Lost {
            // Gone for the rest of the screen: its planes go before a
            // later slot is judged.
            self.release(planes, i);
            return;
        }
        if status != Status::Live {
            return;
        }
        let Some(live) = movie.live() else {
            return;
        };
        if slot.admission == Admission::Undecided {
            let pixels = u64::from(live.width) * u64::from(live.height);
            match admit(earlier, pixels) {
                Verdict::Defer => return,
                Verdict::Refuse => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.admission = Admission::Refused;
                    }
                    movie.lose(Loss::OverTotal);
                    return;
                }
                Verdict::Admit => {
                    let reader = live
                        .reader
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take();
                    let Some(reader) = reader else {
                        // Its reader went elsewhere: it cannot be read.
                        movie.lose(Loss::Fault);
                        return;
                    };
                    if let Some(s) = self.slots.get_mut(i) {
                        s.admission = Admission::Admitted;
                        s.reader = Some(reader);
                        s.size = [live.width, live.height];
                        s.bt601 = live.bt601;
                        s.full = live.full;
                    }
                }
            }
        }
        // Its planes, at its admission or after the device's recovery.
        if let Some(&Slot { size: [w, h], .. }) = self.slots.get(i) {
            self.reserve(planes, i, w, h);
        }
    }

    /// The frame's hook at `due`, the screen's time in µs: each admitted
    /// slot peeked and taken into `planes` in slot order, after the
    /// preparation deadline is applied at `now`; what each slot's
    /// destinations show. It admits and reserves nothing: a movie not
    /// admitted at a boundary yet shows nothing.
    pub fn frame(&mut self, planes: &mut dyn Planes, due: i64, now: Instant) -> MovieShows {
        let _ = self.settle(now);
        let mut shows = MovieShows::default();
        for i in 0..MAX_MOVIES {
            let show = self.slot_frame(planes, i, due);
            if let Some(s) = shows.get_mut(i) {
                *s = show;
            }
        }
        shows
    }

    fn slot_frame(&mut self, planes: &mut dyn Planes, i: usize, due: i64) -> MovieShow {
        let reserved = self.reserved.get(i).copied();
        let deadline = self.deadline;
        let Some(slot) = self.slots.get_mut(i) else {
            return MovieShow::Nothing;
        };
        let Some(movie) = slot.movie.clone() else {
            return MovieShow::Nothing;
        };
        if slot.admission == Admission::Refused {
            return MovieShow::Nothing;
        }
        match movie.status() {
            Status::Lost => return MovieShow::Lost,
            Status::Live => {}
            Status::Pending | Status::Absent => return MovieShow::Nothing,
        }
        let Some(live) = movie.live() else {
            return MovieShow::Nothing;
        };
        // Admitted at a boundary, its planes reserved at its size.
        if slot.admission != Admission::Admitted || reserved != Some(slot.size) {
            return MovieShow::Nothing;
        }
        let layer = u32::try_from(i).unwrap_or(u32::MAX);
        let Some(reader) = slot.reader.as_mut() else {
            return MovieShow::Nothing;
        };
        let [width, height] = slot.size;
        let peeked = {
            #[cfg(feature = "dev-tools")]
            let _region = crate::alloc_count::AppRegion::enter();
            reader.peek(live.words, due)
        };
        let peek = match peeked {
            Ok(p) => p,
            Err(_) => {
                movie.lose(Loss::Fault);
                return MovieShow::Lost;
            }
        };
        if peek.freed {
            movie.freed(reader.freed());
        }
        if let Some(d) = peek.due {
            let mut took = None;
            let words = live.words;
            let written = planes.write(layer, width, height, &mut |sink, pitch| {
                #[cfg(feature = "dev-tools")]
                let _region = crate::alloc_count::AppRegion::enter();
                let taken = reader.take(words, d, pitch, sink);
                let ok = taken.is_ok();
                took = Some(ok);
                ok
            });
            match took {
                Some(false) => {
                    movie.lose(Loss::Fault);
                    return MovieShow::Lost;
                }
                Some(true) => {
                    movie.freed(reader.freed());
                    if written {
                        slot.shown = true;
                        if !slot.staged {
                            // Accepted past the deadline: lost, never
                            // ready, however early the poll began.
                            if Instant::now() >= deadline {
                                movie.expire();
                                return MovieShow::Lost;
                            }
                            slot.staged = true;
                            movie.staged_first();
                        }
                    }
                }
                // The frame did not fit the planes or the buffer could not
                // be mapped: it stays in the ring for a later frame.
                None => {}
            }
        }
        if slot.shown {
            MovieShow::Frame {
                uv: render::VIDEO_UV,
                layer: u16::try_from(i).unwrap_or(u16::MAX),
                size: [width, height],
                bt601: slot.bt601,
                full: slot.full,
            }
        } else {
            MovieShow::Nothing
        }
    }

    /// The device was made again: no slot's planes are reserved or hold
    /// a frame, and each admitted live movie reserves its own again at the
    /// next admission boundary. Admissions, readers and the movies' states
    /// stay.
    pub fn rebuilt(&mut self) {
        self.reserved = [[0; 2]; MAX_MOVIES];
        for s in &mut self.slots {
            s.shown = false;
        }
    }

    /// Makes slot `i`'s planes those of its admitted movie of `width` ×
    /// `height`, unless they are already: that slot's frame is then gone
    /// from them until its next take. Another slot's planes and frame stay.
    fn reserve(&mut self, planes: &mut dyn Planes, i: usize, width: u32, height: u32) {
        let Some(reserved) = self.reserved.get_mut(i) else {
            return;
        };
        if *reserved == [width, height] {
            return;
        }
        planes.reserve(u32::try_from(i).unwrap_or(u32::MAX), width, height);
        *reserved = [width, height];
        if let Some(s) = self.slots.get_mut(i) {
            s.shown = false;
        }
    }

    /// Releases slot `i`'s planes, if this value holds them reserved: its
    /// frame goes with them. Another slot's planes and frame stay.
    fn release(&mut self, planes: &mut dyn Planes, i: usize) {
        let Some(reserved) = self.reserved.get_mut(i) else {
            return;
        };
        if *reserved == [0, 0] {
            return;
        }
        planes.release(u32::try_from(i).unwrap_or(u32::MAX));
        *reserved = [0, 0];
        if let Some(s) = self.slots.get_mut(i) {
            s.shown = false;
        }
    }
}

#[cfg(test)]
mod tests;
