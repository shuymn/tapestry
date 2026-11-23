//! The render side's skin movie hook (design.md "Rendering and Skins",
//! skin movies; "Threat Model" T02): on every play frame and every
//! selection frame, before the skin's build, and on every poll of a play's
//! preparation, it reads each movie of the screen in slot order and says
//! what its destinations show ([`MovieShows`]).
//!
//! The screen's pixel total is the media session's, applied at each
//! movie's probe before its ring is made
//! ([`crate::media_host::session::total`]): a movie it refuses is lost for
//! [`Loss::OverTotal`] with no ring, and draws nothing. Every movie that
//! goes live therefore fits the total with the screen's other rings, lost
//! or not, and so do the planes made for it.
//!
//! Admission is a boundary of its own ([`Movies::admit`]), never part of a
//! frame nor of a preparation's poll: the render thread's pass that finds
//! no frame due, before it waits for the frame's grant. There, in slot
//! order, a lost movie's planes are released, and each live movie not
//! admitted yet takes its ring's reader once and has its planes reserved
//! at its size, never another slot's: that allocates on the GPU, and a
//! peer's admission leaves a movie's planes and its shown frame as they
//! are. A pass that never finds that slack (a grant ready at every pass)
//! gives a movie one admission turn ([`Movies::turn`]) after a frame is
//! drawn and before the next grant is taken: a live movie seen waiting at
//! one turn is admitted at the next, once.
//!
//! The frame's hook ([`Movies::frame`]), per slot: a lost movie shows
//! [`MovieShow::Lost`], a refused one nothing. An admitted movie whose
//! planes are reserved reads its ring on its own clock ([`Clock`]) and,
//! when a frame is due, takes it straight into the renderer's upload
//! buffer ([`Planes::write`]); a fault loses it in the same frame. Until a
//! frame is taken it shows nothing, so what is below shows.
//!
//! A movie's clock starts at the first presented frame that draws it.
//! Before, a poll takes its run's oldest frame once
//! ([`skin_model::media::Reader::peek_first`]) and holds it, whatever
//! polls come, the first frame staged. The first frame then presented
//! whose build drew a quad of the movie ([`Movies::presented`]: the batch's
//! marks and the frame's outcome) starts its clock at the frame time its
//! hook was given, and from then each visible hook peeks at the first
//! frame's stamp plus the frame time since that origin: it takes the
//! newest frame the ring holds at or before it, late or not, a stutter
//! rather than a blank panel, and the frame taken last stays shown until a
//! newer one is due. A first frame whose take ends past the preparation
//! deadline loses its movie (L5): it is judged when the frame is accepted,
//! not when the poll began; once staged, the deadline no longer governs
//! the movie. Under dev-tools the app region covers each peek and each
//! take, inside the upload's mapping, and nothing of wgpu's.
//!
//! A play's preparation polls ([`Movies::prepare`]) are readiness only:
//! they take each movie's first frame and nothing after it, and start no
//! clock. A visible loading state polls the movies at its frame time
//! ([`Movies::poll`]), the clock READY and the play frames go on with, so
//! a movie first drawn there keeps moving through READY. Each poll ends
//! with a fresh clock sample: past the deadline, every movie not settled
//! is lost and shows black in that poll, however long its takes ran.
//!
//! A play's chart videos (design.md "Rendering and Skins", BGA) take the
//! chart's slots ([`CHART_SLOTS`]) and run on chart time ([`Chart`]), not
//! on a skin movie's clock: their first frame is staged at the preparation
//! as a skin movie's is, and each play frame ([`Movies::play_frame`]) finds
//! the start in effect by binary search over the slot's starts (the
//! latest at or before the frame's chart time, at or after the attempt's
//! boundary). A new start plays the run cued for it, or starts a new run
//! ([`Reader::start_run`]), writes it to the video's want word and wakes
//! the session, which seeks it; the frame due is the newest at or before
//! the clip's first stamp plus the chart time since the start times the
//! speed. Until a restart's first frame is taken the frame before stays
//! shown; a run ends once the worker reported it, nothing of it is left in
//! the ring, and the time due passed its last frame's end (or its seek
//! found no frame), and an ended run with a start ahead is cued at once. A
//! chart video lost for its own reason loses the other with
//! [`Loss::Peer`]. Under dev-tools one app region covers each chart slot's
//! start lookup, state change, run start, want, wake and peek, and the take
//! its own inside the upload's mapping.
//!
//! Every slot's state is made when the screen's movies are opened: the
//! frame's hook allocates nothing, panics nowhere and never waits on the
//! media session.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use skin_model::bga::ChartVideos;
use skin_model::media::control::{Matrix, Range};
use skin_model::media::{Reader, Window};
use skin_model::movie::{CHART_SLOTS, MovieShow, MovieShows, SLOTS};

use crate::media_host::session::{Loss, Movie, Status, Waker, preparation_deadline};

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
    /// Whether it was refused at the screen's pixel total: it draws
    /// nothing, never black.
    fn refused(&self) -> bool;
    /// Why it was lost first.
    fn loss(&self) -> Option<Loss>;
    /// The render side wants run `run` sought ([`crate::media_host::movie::Runs::want`]).
    fn want(&self, run: u64);
    /// The newest run whose end the worker reported ([`Movie::ended`]).
    fn ended(&self) -> u64;
    /// The newest run whose seek found no frame ([`Movie::empty`]).
    fn empty(&self) -> u64;
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

    fn refused(&self) -> bool {
        Self::loss(self) == Some(Loss::OverTotal)
    }

    fn loss(&self) -> Option<Loss> {
        Self::loss(self)
    }

    fn want(&self, run: u64) {
        self.runs.want(run);
    }

    fn ended(&self) -> u64 {
        self.runs.ended()
    }

    fn empty(&self) -> u64 {
        self.runs.empty()
    }
}

/// A play frame's chart time and practice speed, which the chart videos
/// run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChartAt {
    /// The frame's chart time, µs of playback.
    pub now: i64,
    /// `k` of the speed `k / 20`.
    pub k: u32,
}

/// The stamps a chart video's runs may carry: any.
const RUN_WINDOW: Window = Window {
    first: 0,
    last: i64::MAX,
};

/// Where a chart video stands (design.md "Rendering and Skins", BGA).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chart {
    /// Its first frame not staged yet.
    Unstaged,
    /// Run `run` cued, its first frame `staged` or not, its start ahead:
    /// nothing shown.
    Waiting { run: u64, staged: bool },
    /// Run `run` plays from the start at chart time `start`.
    Playing { run: u64, start: i64 },
    /// Run `run`, started at `start`, has no frame taken yet: the frame of
    /// the run before stays shown when `hold`.
    Restarting { run: u64, start: i64, hold: bool },
    /// Its run ended: nothing shown.
    Ended,
}

/// The frame a chart slot's planes hold: its run, stamp and duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    run: u64,
    stamp: i64,
    duration: i64,
}

/// The media time due at chart time `at` for a run started at `start`, the
/// clip's first frame stamped `first`, at the speed `k / 20`:
/// `first + ⌊(now − start) · k / 20⌋`, in 128 bits, saturated.
fn chart_due(first: i64, start: i64, at: ChartAt) -> i64 {
    let since = i128::from(at.now) - i128::from(start);
    let media = (since * i128::from(at.k)).div_euclid(20);
    let due = i128::from(first).saturating_add(media);
    i64::try_from(due).unwrap_or(if due < 0 { i64::MIN } else { i64::MAX })
}

/// Whether run `run`, its end reported, had ended by media time `due`, as a
/// frame drawn at that time would have judged it: the ring holds no frame
/// of the run newer than the one due then, and that frame, or `held` when
/// none is due, lasts no further than `due`; a run with no frame ended at
/// once. A peek that fails leaves the run playing: the next peek returns
/// the fault. `freed` is set when the peek freed slots. Allocates nothing.
fn ended_by<W: skin_model::media::Words + ?Sized>(
    reader: &mut Reader,
    ring: &W,
    held: Option<Held>,
    run: u64,
    due: i64,
    freed: &mut bool,
) -> bool {
    let Ok(peek) = reader.peek(ring, due) else {
        return false;
    };
    *freed |= peek.freed;
    if peek.left {
        return false;
    }
    let last = peek
        .due
        .map(|d| (d.stamp(), d.duration()))
        .or_else(|| held.filter(|h| h.run == run).map(|h| (h.stamp, h.duration)));
    last.is_none_or(|(stamp, duration)| due >= stamp.saturating_add(duration))
}

/// Where a movie's clock stands (design.md "Skin movies").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clock {
    /// No frame taken yet.
    Unstaged,
    /// Its first frame, stamped `first`, is in the planes and no presented
    /// frame has drawn it yet: no more is taken.
    Staged { first: i64 },
    /// First drawn by the presented frame whose hook had frame time
    /// `origin`: the frame due at frame time `t` is the newest at or
    /// before `first + (t - origin)`.
    Running { first: i64, origin: i64 },
}

impl Clock {
    /// The media time due at frame time `at`, once running; never before
    /// the first frame's stamp.
    fn due(self, at: i64) -> Option<i64> {
        match self {
            Self::Running { first, origin } => {
                Some(first.saturating_add(at.saturating_sub(origin).max(0)))
            }
            Self::Unstaged | Self::Staged { .. } => None,
        }
    }
}

/// One slot of the screen's movies.
struct Slot<S> {
    movie: Option<Arc<S>>,
    reader: Option<Reader>,
    /// Admitted at a boundary: its reader taken, its size known.
    admitted: bool,
    size: [u32; 2],
    bt601: bool,
    full: bool,
    /// A frame was taken into the planes on this screen (and the planes
    /// have not been replaced since).
    shown: bool,
    clock: Clock,
    /// Seen live and not admitted at an admission turn: the next turn
    /// admits it.
    waited: bool,
    /// A chart video's state, its clip's first stamp, the frame its planes
    /// hold and the index of the start in effect.
    chart: Chart,
    first: Option<i64>,
    held: Option<Held>,
    current: Option<usize>,
    /// The stamp of the frame its planes hold (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    stamp: Option<i64>,
}

impl<S: Source> Slot<S> {
    const fn new(movie: Option<Arc<S>>) -> Self {
        Self {
            movie,
            reader: None,
            admitted: false,
            size: [0, 0],
            bt601: false,
            full: false,
            shown: false,
            clock: Clock::Unstaged,
            waited: false,
            chart: Chart::Unstaged,
            first: None,
            held: None,
            current: None,
            #[cfg(all(test, feature = "dev-tools"))]
            stamp: None,
        }
    }
}

/// A screen's movies, by slot, with what the hook keeps of each.
pub struct Movies<S = Movie> {
    slots: [Slot<S>; SLOTS],
    /// Each slot's planes' size as last reserved through this value; zero
    /// before and once released.
    reserved: [[u32; 2]; SLOTS],
    /// The screen's preparation deadline (L5).
    deadline: Instant,
    /// The frame time the last visible hook was given, until the frame it
    /// built is judged ([`presented`](Self::presented)).
    hook_at: Option<i64>,
    /// The chart's videos' starts, by slot, and the attempt's boundary:
    /// starts before it are not counted.
    videos: Option<Arc<ChartVideos>>,
    boundary: i64,
    /// Wakes the media session when a chart video wants a run sought.
    waker: Option<Waker>,
    /// The wakes made (tests).
    #[cfg(test)]
    wakes: u32,
}

impl<S: Source> Movies<S> {
    /// The movies of a screen whose preparation started at `start`, by
    /// slot (`None` for an absent one).
    pub fn new(movies: [Option<Arc<S>>; SLOTS], start: Instant) -> Self {
        Self {
            slots: movies.map(Slot::new),
            reserved: [[0; 2]; SLOTS],
            deadline: preparation_deadline(start),
            hook_at: None,
            videos: None,
            boundary: i64::MIN,
            waker: None,
            #[cfg(test)]
            wakes: 0,
        }
    }

    /// Puts `movie` in slot `i`, empty until now: the next admission
    /// boundary admits it.
    pub fn put(&mut self, i: usize, movie: Arc<S>) {
        if let Some(slot) = self.slots.get_mut(i)
            && slot.movie.is_none()
        {
            *slot = Slot::new(Some(movie));
        }
    }

    /// The chart's videos' starts (`videos`, by slot) and the attempt's
    /// boundary: a full play's is `i64::MIN`, every start counted; a
    /// practice round's its lead-in's start, an earlier start not counted.
    /// Set at the preparation, before any play frame.
    pub fn set_chart(&mut self, videos: Arc<ChartVideos>, boundary: i64) {
        self.videos = Some(videos);
        self.boundary = boundary;
    }

    /// Wakes the media session through `waker` when a chart video wants a
    /// run sought.
    pub fn set_waker(&mut self, waker: Waker) {
        self.waker = Some(waker);
    }

    /// The session wakes made (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn wakes(&self) -> u32 {
        self.wakes
    }

    /// The skin's movies of a screen whose preparation started at `start`,
    /// in its first slots, the chart's slots empty (tests).
    #[cfg(test)]
    pub fn skin(movies: [Option<Arc<S>>; skin_model::movie::MAX_MOVIES], start: Instant) -> Self {
        let mut skin = movies.into_iter();
        Self::new(std::array::from_fn(|_| skin.next().flatten()), start)
    }

    /// A preparation poll at `now`, readiness only: each movie's first
    /// frame taken once, and nothing after it, no clock started. It admits
    /// and reserves nothing, so a poll made with a frame's grant held never
    /// allocates before that frame. As [`poll`](Self::poll) otherwise.
    pub fn prepare(&mut self, planes: &mut dyn Planes, now: Instant) -> (bool, MovieShows) {
        self.poll_at(planes, None, now)
    }

    /// A visible poll at frame time `at` (µs) and `now`: the frame's hook;
    /// then, at a fresh clock sample, the preparation deadline applied
    /// again, so a movie it loses while the takes ran shows black in this
    /// poll. Whether every movie is settled, and what each slot's
    /// destinations show (for the loading frames). It admits nothing.
    pub fn poll(&mut self, planes: &mut dyn Planes, at: i64, now: Instant) -> (bool, MovieShows) {
        self.poll_at(planes, Some(at), now)
    }

    fn poll_at(
        &mut self,
        planes: &mut dyn Planes,
        at: Option<i64>,
        now: Instant,
    ) -> (bool, MovieShows) {
        let mut shows = self.hook(planes, at, None, now);
        let settled = self.settle(Instant::now().max(now));
        for (show, slot) in shows.iter_mut().zip(&self.slots) {
            if slot
                .movie
                .as_ref()
                .is_some_and(|m| m.status() == Status::Lost && !m.refused())
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

    /// The admission boundary at `now`, never a frame's: the preparation
    /// deadline applied, then in slot order a lost or refused movie's
    /// planes released, and a live movie not admitted yet admitted, its
    /// reader taken and its planes reserved. It allocates on the GPU only.
    pub fn admit(&mut self, planes: &mut dyn Planes, now: Instant) {
        let _ = self.settle(now);
        for i in 0..SLOTS {
            self.slot_admit(planes, i);
        }
    }

    /// The admission turn at `now`, after a frame drawn and before the
    /// next grant is taken, for a render loop whose passes found no slack
    /// for the boundary: each live movie not admitted is admitted if it was
    /// seen so at the turn before, and marked seen otherwise, so the
    /// boundary has a frame's interval to admit it first. Once admitted,
    /// a movie takes no turn again. It releases nothing.
    pub fn turn(&mut self, planes: &mut dyn Planes, now: Instant) {
        let _ = self.settle(now);
        for i in 0..SLOTS {
            let Some(slot) = self.slots.get_mut(i) else {
                continue;
            };
            let waiting = !slot.admitted
                && slot
                    .movie
                    .as_ref()
                    .is_some_and(|m| m.status() == Status::Live);
            if waiting && std::mem::replace(&mut slot.waited, true) {
                self.slot_admit(planes, i);
            }
        }
    }

    fn slot_admit(&mut self, planes: &mut dyn Planes, i: usize) {
        let Some(slot) = self.slots.get(i) else {
            return;
        };
        let Some(movie) = slot.movie.clone() else {
            return;
        };
        match movie.status() {
            Status::Lost | Status::Absent => {
                // Gone for the rest of the screen: its planes go.
                self.release(planes, i);
                return;
            }
            Status::Pending => return,
            Status::Live => {}
        }
        let Some(live) = movie.live() else {
            return;
        };
        if !slot.admitted {
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
                s.admitted = true;
                s.reader = Some(reader);
                s.size = [live.width, live.height];
                s.bt601 = live.bt601;
                s.full = live.full;
            }
        }
        // Its planes, at its admission.
        if let Some(&Slot { size: [w, h], .. }) = self.slots.get(i) {
            self.reserve(planes, i, w, h);
        }
    }

    /// The frame's hook at frame time `at` (µs): each admitted slot read
    /// and taken into `planes` in slot order on its own clock, after the
    /// preparation deadline is applied at `now`; what each slot's
    /// destinations show. The frame it builds is then judged by
    /// [`presented`](Self::presented). It admits and reserves nothing: a
    /// movie not admitted at a boundary yet shows nothing.
    pub fn frame(&mut self, planes: &mut dyn Planes, at: i64, now: Instant) -> MovieShows {
        self.hook(planes, Some(at), None, now)
    }

    /// A play frame's hook at frame time `at` (µs) and chart time `chart`:
    /// as [`frame`](Self::frame), the chart's videos run on chart time.
    pub fn play_frame(
        &mut self,
        planes: &mut dyn Planes,
        at: i64,
        chart: ChartAt,
        now: Instant,
    ) -> MovieShows {
        self.hook(planes, Some(at), Some(chart), now)
    }

    fn hook(
        &mut self,
        planes: &mut dyn Planes,
        at: Option<i64>,
        chart: Option<ChartAt>,
        now: Instant,
    ) -> MovieShows {
        let _ = self.settle(now);
        if at.is_some() {
            self.hook_at = at;
        }
        let mut shows = MovieShows::default();
        for i in 0..SLOTS {
            let show = if CHART_SLOTS.contains(&i) {
                self.chart_frame(planes, i, chart)
            } else {
                self.slot_frame(planes, i, at)
            };
            if let Some(s) = shows.get_mut(i) {
                *s = show;
            }
        }
        self.lose_peers(&mut shows);
        shows
    }

    /// A chart video lost for its own reason (not its peer's, not the
    /// pixel total) loses every other chart video pending or live with
    /// [`Loss::Peer`], each shown lost from this frame: the chart's videos
    /// are one panel. An absent or refused one stays as it is.
    fn lose_peers(&self, shows: &mut MovieShows) {
        let movie = |i: usize| self.slots.get(i).and_then(|s| s.movie.as_deref());
        let lost = CHART_SLOTS.iter().any(|&i| {
            movie(i).is_some_and(|m| {
                m.status() == Status::Lost
                    && m.loss()
                        .is_some_and(|why| !matches!(why, Loss::Peer | Loss::OverTotal))
            })
        });
        if !lost {
            return;
        }
        for &i in &CHART_SLOTS {
            let Some(m) = movie(i) else { continue };
            if matches!(m.status(), Status::Pending | Status::Live) {
                m.lose(Loss::Peer);
            }
            if m.status() == Status::Lost
                && !m.refused()
                && let Some(show) = shows.get_mut(i)
            {
                *show = MovieShow::Lost;
            }
        }
    }

    /// The frame the last visible hook was given for has been drawn:
    /// `presented` whether it reached the target, `drawn` which slots its
    /// build drew a quad of. A slot whose first frame is staged and drawn
    /// by a presented frame starts its clock at that hook's frame time;
    /// nothing else changes. A frame drawn with no visible hook before it
    /// starts nothing.
    pub fn presented(&mut self, presented: bool, drawn: [bool; SLOTS]) {
        let Some(origin) = self.hook_at.take().filter(|_| presented) else {
            return;
        };
        for (i, (slot, drawn)) in self.slots.iter_mut().zip(drawn).enumerate() {
            // A chart video runs on chart time: no presentation starts it.
            if CHART_SLOTS.contains(&i) {
                continue;
            }
            if let Clock::Staged { first } = slot.clock
                && drawn
                && slot.shown
            {
                slot.clock = Clock::Running { first, origin };
            }
        }
    }

    /// A frame is built without this set's hook, or the frame its last
    /// hook was given for is not drawn: that hook's frame time starts no
    /// clock.
    pub fn unhook(&mut self) {
        self.hook_at = None;
    }

    /// One slot's read at frame time `at` (`None` for a preparation's
    /// poll): before its first frame, the run's oldest; once running and
    /// visible, the newest due on its clock; otherwise nothing.
    fn slot_frame(&mut self, planes: &mut dyn Planes, i: usize, at: Option<i64>) -> MovieShow {
        let reserved = self.reserved.get(i).copied();
        let deadline = self.deadline;
        let Some(slot) = self.slots.get_mut(i) else {
            return MovieShow::Nothing;
        };
        let Some(movie) = slot.movie.clone() else {
            return MovieShow::Nothing;
        };
        match movie.status() {
            Status::Lost if movie.refused() => return MovieShow::Nothing,
            Status::Lost => return MovieShow::Lost,
            Status::Live => {}
            Status::Pending | Status::Absent => return MovieShow::Nothing,
        }
        let Some(live) = movie.live() else {
            return MovieShow::Nothing;
        };
        // Admitted at a boundary, its planes reserved at its size.
        if !slot.admitted || reserved != Some(slot.size) {
            return MovieShow::Nothing;
        }
        let layer = u32::try_from(i).unwrap_or(u32::MAX);
        let Some(reader) = slot.reader.as_mut() else {
            return MovieShow::Nothing;
        };
        let [width, height] = slot.size;
        let staging = slot.clock == Clock::Unstaged;
        let due = at.and_then(|at| slot.clock.due(at));
        if staging || due.is_some() {
            let peeked = {
                #[cfg(feature = "dev-tools")]
                let _region = crate::alloc_count::AppRegion::enter();
                match due {
                    Some(due) => reader.peek(live.words, due),
                    None => reader.peek_first(live.words),
                }
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
                    took = Some(taken.map(|f| f.stamp).ok());
                    ok
                });
                match took {
                    Some(None) => {
                        movie.lose(Loss::Fault);
                        return MovieShow::Lost;
                    }
                    Some(Some(stamp)) => {
                        movie.freed(reader.freed());
                        if written {
                            slot.shown = true;
                            #[cfg(all(test, feature = "dev-tools"))]
                            {
                                slot.stamp = Some(stamp);
                            }
                            if staging {
                                // Accepted past the deadline: lost, never
                                // ready, however early the poll began.
                                if Instant::now() >= deadline {
                                    movie.expire();
                                    return MovieShow::Lost;
                                }
                                slot.clock = Clock::Staged { first: stamp };
                                movie.staged_first();
                            }
                        }
                    }
                    // The frame did not fit the planes or the buffer could
                    // not be mapped: it stays in the ring for a later frame.
                    None => {}
                }
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

    /// One chart slot's read: before its first frame (a preparation's poll
    /// included), the run's oldest; in a play frame at `at`, its start in
    /// effect found and acted on, the newest frame due on chart time taken,
    /// its end judged; otherwise nothing taken. What its destinations show.
    #[expect(
        clippy::too_many_lines,
        reason = "one slot's states, read in the order the end rule needs"
    )]
    fn chart_frame(&mut self, planes: &mut dyn Planes, i: usize, at: Option<ChartAt>) -> MovieShow {
        let reserved = self.reserved.get(i).copied();
        let deadline = self.deadline;
        let boundary = self.boundary;
        let Self {
            slots,
            videos,
            waker,
            #[cfg(test)]
            wakes,
            ..
        } = self;
        let Some(slot) = slots.get_mut(i) else {
            return MovieShow::Nothing;
        };
        let Some(movie) = slot.movie.clone() else {
            return MovieShow::Nothing;
        };
        match movie.status() {
            Status::Lost if movie.refused() => return MovieShow::Nothing,
            Status::Lost => return MovieShow::Lost,
            Status::Live => {}
            Status::Pending | Status::Absent => return MovieShow::Nothing,
        }
        let Some(live) = movie.live() else {
            return MovieShow::Nothing;
        };
        if !slot.admitted || reserved != Some(slot.size) {
            return MovieShow::Nothing;
        }
        let layer = u32::try_from(i).unwrap_or(u32::MAX);
        let Some(reader) = slot.reader.as_mut() else {
            return MovieShow::Nothing;
        };
        let [width, height] = slot.size;
        let starts = videos.as_deref().map_or(&[][..], |v| v.starts(i));
        // The worker publishes every frame of a run before it reports the
        // run's end: read first, so the peek below sees them all.
        let ended = movie.ended();
        let empty = movie.empty();
        let first = slot.first.unwrap_or(0);
        let mut freed = false;
        let (due, peeked) = {
            #[cfg(feature = "dev-tools")]
            let _region = crate::alloc_count::AppRegion::enter();
            let mut wanted = None;
            if let Some(at) = at {
                // The start in effect: the latest at or before now, counted
                // when at or after the boundary.
                let n = starts.partition_point(|s| s.0 <= at.now);
                let current = n
                    .checked_sub(1)
                    .filter(|j| starts.get(*j).is_some_and(|s| s.0 >= boundary));
                if let Some(j) = current
                    && slot.current != Some(j)
                    && let Some(e) = starts.get(j).map(|s| s.0)
                {
                    slot.current = Some(j);
                    // Source: SF-025
                    slot.chart = match slot.chart {
                        Chart::Waiting { run, .. } => Chart::Playing { run, start: e },
                        Chart::Unstaged => Chart::Playing {
                            run: reader.run(),
                            start: e,
                        },
                        before @ (Chart::Playing { .. }
                        | Chart::Restarting { .. }
                        | Chart::Ended) => {
                            let hold = match before {
                                // Held only while the run before still
                                // plays at the new start: its end judged by
                                // the start's time, not this frame's.
                                Chart::Playing { run, start } => {
                                    slot.held.is_some_and(|h| h.run == run)
                                        && !(ended == run
                                            && ended_by(
                                                reader,
                                                live.words,
                                                slot.held,
                                                run,
                                                chart_due(first, start, ChartAt { now: e, ..at }),
                                                &mut freed,
                                            ))
                                }
                                Chart::Restarting { hold, .. } => hold,
                                _ => false,
                            };
                            let run = reader.start_run(RUN_WINDOW);
                            wanted = Some(run);
                            Chart::Restarting {
                                run,
                                start: e,
                                hold,
                            }
                        }
                    };
                }
                // An ended run with a start ahead is cued at once.
                if slot.chart == Chart::Ended
                    && starts
                        .get(n..)
                        .is_some_and(|ahead| ahead.iter().any(|s| s.0 >= boundary))
                {
                    let run = reader.start_run(RUN_WINDOW);
                    wanted = Some(run);
                    slot.chart = Chart::Waiting { run, staged: false };
                }
            }
            if let Some(run) = wanted {
                movie.want(run);
                if let Some(w) = waker.as_ref() {
                    w.wake();
                }
                #[cfg(test)]
                {
                    *wakes += 1;
                }
            }
            let (pick, due) = match (slot.chart, at) {
                (Chart::Unstaged | Chart::Waiting { staged: false, .. }, _) => (None, None),
                (Chart::Playing { start, .. } | Chart::Restarting { start, .. }, Some(at)) => {
                    let due = chart_due(first, start, at);
                    (Some(due), Some(due))
                }
                // Nothing due: the look frees the slots of earlier runs.
                _ => (Some(i64::MIN), None),
            };
            let peeked = match pick {
                Some(due) => reader.peek(live.words, due),
                None => reader.peek_first(live.words),
            };
            (due, peeked)
        };
        let Ok(peek) = peeked else {
            movie.lose(Loss::Fault);
            return MovieShow::Lost;
        };
        if peek.freed || freed {
            movie.freed(reader.freed());
        }
        // Whether a frame of the run stays in the ring: one newer than the
        // frame due, or the frame due itself while its take is refused.
        let mut left = peek.left;
        if let Some(d) = peek.due {
            left = true;
            let mut took = None;
            let words = live.words;
            let written = planes.write(layer, width, height, &mut |sink, pitch| {
                #[cfg(feature = "dev-tools")]
                let _region = crate::alloc_count::AppRegion::enter();
                let taken = reader.take(words, d, pitch, sink);
                let ok = taken.is_ok();
                took = Some(taken.map(|f| (f.stamp, f.duration)).ok());
                ok
            });
            match took {
                Some(None) => {
                    movie.lose(Loss::Fault);
                    return MovieShow::Lost;
                }
                Some(Some((stamp, duration))) => {
                    left = peek.left;
                    movie.freed(reader.freed());
                    if written {
                        slot.shown = true;
                        let run = reader.run();
                        slot.held = Some(Held {
                            run,
                            stamp,
                            duration,
                        });
                        #[cfg(all(test, feature = "dev-tools"))]
                        {
                            slot.stamp = Some(stamp);
                        }
                        if slot.first.is_none() {
                            // Accepted past the deadline: lost, never ready.
                            if Instant::now() >= deadline {
                                movie.expire();
                                return MovieShow::Lost;
                            }
                            slot.first = Some(stamp);
                            movie.staged_first();
                        }
                        slot.chart = match slot.chart {
                            Chart::Unstaged => Chart::Waiting { run, staged: true },
                            Chart::Waiting { run: r, .. } => Chart::Waiting {
                                run: r,
                                staged: true,
                            },
                            Chart::Restarting { run: r, start, .. } if r == run => {
                                Chart::Playing { run, start }
                            }
                            other => other,
                        };
                    }
                }
                // The frame did not fit the planes or the buffer could
                // not be mapped: it stays in the ring for a later frame.
                None => {}
            }
        }
        // The end: reported, nothing of the run left in the ring, and the
        // time due past its last frame's end, or its seek found no frame.
        // Source: SF-027
        if let (Chart::Playing { run, .. } | Chart::Restarting { run, .. }, Some(due)) =
            (slot.chart, due)
            && ended == run
            && !left
        {
            let done = match slot.held {
                Some(h) if h.run == run => due >= h.stamp.saturating_add(h.duration),
                _ => empty == run,
            };
            if done {
                slot.chart = Chart::Ended;
            }
        }
        // Source: SF-028
        let shows = match slot.chart {
            Chart::Playing { run, .. } => slot.held.is_some_and(|h| h.run == run),
            Chart::Restarting { run, hold, .. } => hold || slot.held.is_some_and(|h| h.run == run),
            Chart::Unstaged | Chart::Waiting { .. } | Chart::Ended => false,
        };
        if shows && slot.shown {
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

    /// The stamp of the frame each slot's planes hold (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn stamps(&self) -> [Option<i64>; SLOTS] {
        self.slots.each_ref().map(|s| s.stamp.filter(|_| s.shown))
    }

    /// Each slot's clock origin on the frame clock, once started (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn origins(&self) -> [Option<i64>; SLOTS] {
        self.slots.each_ref().map(|s| match s.clock {
            Clock::Running { origin, .. } => Some(origin),
            Clock::Unstaged | Clock::Staged { .. } => None,
        })
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
