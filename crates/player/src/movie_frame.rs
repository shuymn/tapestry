//! The render side's skin movie hook (design.md "Rendering and Skins",
//! skin movies; "Threat Model" T02): on every play frame and every
//! selection frame, before the skin's build, and on every poll of a play's
//! preparation, it reads each movie of the screen in slot order and says
//! what its destinations show ([`MovieShows`]).
//!
//! Per slot: a lost movie shows [`MovieShow::Lost`]. A live one is first
//! admitted against the screen's pixel total ([`admit`]): judged in slot
//! order, once every earlier slot is settled, a movie whose probed size
//! with the earlier slots' admitted live movies passes
//! [`MAX_MOVIE_PIXELS`] is refused (lost for [`Loss::OverTotal`]) and
//! draws nothing. An admitted movie takes its ring's reader once, peeks at
//! the screen's time and, when a frame is due, takes it straight into the
//! renderer's upload buffer ([`Planes::write`]); a fault loses it in the
//! same frame. It shows its newest frame once one was taken on the screen,
//! and nothing before. Anything else shows nothing.
//!
//! A movie's planes are reserved at its slot's size when it is admitted,
//! never otherwise and never another slot's: that is the movie's
//! preparation, which allocates on the GPU, and stays outside the app
//! region. A peer's admission leaves a movie's planes and its shown frame
//! as they are. Under dev-tools the region covers each peek and each take,
//! inside the upload's mapping, and nothing of wgpu's.
//!
//! Every slot's state is made when the screen's movies are opened: the hook
//! allocates nothing, panics nowhere and never waits on the media session.

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
    /// Writes a frame of `width` × `height` into slot `slot` through
    /// `fill` ([`render::Renderer::video_write_with`]): whether it did.
    fn write(&mut self, slot: u32, width: u32, height: u32, fill: &mut Fill<'_>) -> bool;
}

impl Planes for render::Renderer {
    fn reserve(&mut self, slot: u32, width: u32, height: u32) {
        self.video_reserve(slot, width, height);
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
    /// An earlier slot is not settled: asked again next frame.
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
    /// before.
    reserved: [[u32; 2]; MAX_MOVIES],
    /// The screen's preparation deadline (L5).
    deadline: Instant,
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
        }
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

    /// The frame's hook at `due`, the screen's time in µs: each slot read,
    /// admitted, peeked and taken into `planes` in slot order, after the
    /// preparation deadline is applied at `now`; what each slot's
    /// destinations show.
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
        let mut earlier = [Earlier::Free; MAX_MOVIES];
        for (e, s) in earlier.iter_mut().zip(self.slots.iter().take(i)) {
            *e = s.earlier();
        }
        let earlier = earlier.get(..i).unwrap_or_default();
        let Some(slot) = self.slots.get(i) else {
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
        if slot.admission == Admission::Undecided {
            let pixels = u64::from(live.width) * u64::from(live.height);
            match admit(earlier, pixels) {
                Verdict::Defer => return MovieShow::Nothing,
                Verdict::Refuse => {
                    if let Some(s) = self.slots.get_mut(i) {
                        s.admission = Admission::Refused;
                    }
                    movie.lose(Loss::OverTotal);
                    return MovieShow::Nothing;
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
                        return MovieShow::Lost;
                    };
                    if let Some(s) = self.slots.get_mut(i) {
                        s.admission = Admission::Admitted;
                        s.reader = Some(reader);
                        s.size = [live.width, live.height];
                        s.bt601 = live.bt601;
                        s.full = live.full;
                    }
                    self.reserve(planes, i, live.width, live.height);
                }
            }
        }
        let layer = u32::try_from(i).unwrap_or(u32::MAX);
        let Some(slot) = self.slots.get_mut(i) else {
            return MovieShow::Nothing;
        };
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
}

#[cfg(test)]
mod tests;
