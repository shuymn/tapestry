//! A screen's skin movies opened through the run's media session
//! (design.md "Rendering and Skins", skin movies; "Threat Model" T02).
//!
//! The run has one session ([`MediaHub`]), started the first time a screen
//! with movies is prepared and kept until the run's screens and their
//! factory are dropped, which quits its worker. Each screen that shows
//! movies takes a new generation and enters it, then hands the session
//! each of its movie files in slot order, opened inside the skin folder
//! ([`skin_model::Sheet::open_movie`]) and checked on the handle: its
//! final path must be the canonical path the folder check passed, so a
//! name swapped for a link between the check and the open is refused. A
//! slot whose file cannot be opened or checked is absent, logged once.
//! Dropping the screen's [`MovieSet`] leaves it: a new generation is
//! entered, and the session closes the players of the one left.
//!
//! A set belongs to exactly one screen instance ([`Holder`]): one
//! selection entry, or one play preparation that becomes one play screen.
//! Every entry and every preparation (each practice round, each retry)
//! opens its own set, so its movies start from their start; nothing is
//! handed back to a request, kept for a later screen or taken over by
//! another. The screen holds its set through [`ScreenMovies`], whose
//! [`retire`](ScreenMovies::retire) is the one place a set is let go while
//! its renderer lives: the set (its generation left, so the session closes
//! its players and their rings go), the renderer's planes of every slot and
//! the loading frames' shows go together, when the instance ends by
//! whichever path: another set opened, a frame of another screen drawn
//! ([`Drawn`]), its preparation cancelled or discarded before its commit,
//! its committed play left before it began
//! ([`abandon`](ScreenMovies::abandon)), its skin's installation removed. A screen dropped (its device lost, or
//! the run's end) drops its set with its renderer; the screen built again
//! opens new sets, whose movies start again, a movie lost before included.
//!
//! A play preparation always opens a set, with or without skin movies, and
//! enters its generation at once; its skin's movies are declared but not
//! opened until the attempt's video manifest (the chart's video files, from
//! the BGA job) is there: then the chart's videos are opened in the chart's
//! slots, not looping, and the skin's movies after them, so the session's
//! declaration order, and the pixel total's, takes the chart's first
//! (design.md "Rendering and Skins", skin movies and BGA). The manifest
//! belongs to the BGA generation the preparation began in: one of another
//! generation is not taken. A manifest that has not come within
//! [`MANIFEST_WAIT`] of the preparation's start is not waited for: the
//! skin's movies are opened alone, with the rest of the preparation's
//! deadline to stage their first frames, and the chart's videos are
//! absent. The screen keeps the latest manifest for a set of its
//! generation opened later, until a committed set of that generation is
//! retired ([`ScreenMovies::retire`]) or a newer manifest replaces it.
//!
//! None of this waits on the session: entering stores a word and an open
//! only queues a request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use render::{Renderer, SkinBundle};
use skin_model::ScreenKind;
#[cfg(all(test, feature = "dev-tools"))]
use skin_model::bga::MAX_CHART_FILES;
use skin_model::movie::{CHART_SLOTS, MAX_MOVIES};

use crate::media_host::launch::Setup;
use crate::media_host::session::{Deadlines, Launching, Loss, Movie, Session};
use crate::movie_frame::Movies;

/// How long a play's set waits for the attempt's video manifest from its
/// preparation's start: half the preparation deadline
/// ([`PREPARATION`](crate::media_host::session::PREPARATION)), so the skin's
/// movies, opened alone past it, still have the other half to stage their
/// first frames (design.md "Load Pipeline", movies at READY).
pub const MANIFEST_WAIT: std::time::Duration = std::time::Duration::from_millis(1_500);

/// The run's media session, made when a screen first needs it.
pub struct MediaHub {
    /// How the worker is launched; `None` when the installation could not
    /// be found, and every movie is then lost.
    launching: Option<Launching>,
    session: OnceLock<Option<Session>>,
    generation: AtomicU64,
    /// The sets open (tests).
    #[cfg(test)]
    open: AtomicU64,
    /// The movies the next sets opened play, in place of their files'
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    supply: std::sync::Mutex<std::collections::VecDeque<[Option<Arc<Movie>>; MAX_MOVIES]>>,
    /// The chart videos the next sets' manifests play, in place of their
    /// files' (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    supply_chart:
        std::sync::Mutex<std::collections::VecDeque<[Option<Arc<Movie>>; MAX_CHART_FILES]>>,
    /// Each video asked of the session, in order: its generation, whether
    /// it is a chart video and whether it loops (tests).
    #[cfg(test)]
    asked: std::sync::Mutex<Vec<(u64, bool, bool)>>,
}

impl MediaHub {
    /// The hub of a run whose worker is installed beside the running
    /// executable and stages its copies in the data folder `data`.
    pub fn new(data: &Path) -> Arc<Self> {
        let installation = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        if installation.is_none() {
            log::warn!("skin movies: the game's folder cannot be found, so none is played");
        }
        Self::with(installation.map(|installation| Launching {
            installation,
            data: data.to_path_buf(),
            setup: Setup::recipe(),
        }))
    }

    /// The hub launching as `launching` says.
    pub fn with(launching: Option<Launching>) -> Arc<Self> {
        Arc::new(Self {
            launching,
            session: OnceLock::new(),
            generation: AtomicU64::new(0),
            #[cfg(test)]
            open: AtomicU64::new(0),
            #[cfg(all(test, feature = "dev-tools"))]
            supply: std::sync::Mutex::default(),
            #[cfg(all(test, feature = "dev-tools"))]
            supply_chart: std::sync::Mutex::default(),
            #[cfg(test)]
            asked: std::sync::Mutex::default(),
        })
    }

    /// The next set's manifest plays `movies` in place of its files
    /// (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn supply_chart(&self, movies: [Option<Arc<Movie>>; MAX_CHART_FILES]) {
        self.supply_chart.lock().unwrap().push_back(movies);
    }

    /// Each video asked of the session so far, in order: its generation,
    /// whether it is a chart video, whether it loops (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn asked(&self) -> Vec<(u64, bool, bool)> {
        self.asked.lock().unwrap().clone()
    }

    /// Asks the session, if any, to play `file` as `movie`: lost when the
    /// run has no session.
    fn ask(
        &self,
        session: Option<&Session>,
        movie: &Arc<Movie>,
        file: std::fs::File,
        looping: bool,
    ) {
        #[cfg(test)]
        self.asked
            .lock()
            .unwrap()
            .push((movie.generation(), movie.is_chart(), looping));
        match session {
            Some(s) => s.open(movie, file, looping),
            None => {
                let _ = movie.lose(Loss::Launch);
            }
        }
    }

    /// The next set opened plays `movies` in place of its files' (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn supply(&self, movies: [Option<Arc<Movie>>; MAX_MOVIES]) {
        self.supply.lock().unwrap().push_back(movies);
    }

    /// How many sets are open (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn open_sets(&self) -> u64 {
        self.open.load(Ordering::SeqCst)
    }

    /// The newest generation taken (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The session, started on first use.
    fn session(&self) -> Option<&Session> {
        self.session
            .get_or_init(|| {
                let launching = self.launching.clone()?;
                Session::start(launching, Deadlines::GAME)
                    .inspect_err(|e| log::warn!("skin movies: the media session cannot start: {e}"))
                    .ok()
            })
            .as_ref()
    }

    /// The next screen's generation.
    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// A screen's movies: what the session plays for it, and what the render
/// side's hook keeps of each. Dropped, the screen is left.
pub struct MovieSet {
    hub: Weak<MediaHub>,
    kind: ScreenKind,
    /// The generation it entered.
    generation: u64,
    /// A play's: the loading lifetime its preparation's request was
    /// published in. Only that lifetime's loading frames show its movies;
    /// a frame of another (a later Start's) shows none of them.
    pub loading: Option<u64>,
    pub movies: Movies,
    /// A play's: the BGA generation its preparation began in.
    bga: Option<u64>,
    /// A play's, until its manifest is taken: what it waits for.
    awaiting: Option<Awaiting>,
}

/// What a play's set waits for before it opens its movies.
struct Awaiting {
    /// The BGA generation the preparation began in: only its manifest is
    /// taken.
    bga: u64,
    /// The skin whose movies open after the chart's videos.
    bundle: Option<Arc<SkinBundle>>,
    /// The attempt's boundary: a start before it is not counted
    /// ([`Movies::set_chart`]).
    boundary: i64,
    /// Until when the manifest is waited for ([`MANIFEST_WAIT`]).
    by: Instant,
}

impl MovieSet {
    /// Opens `bundle`'s movies for a `kind` screen, its preparation
    /// starting at `start` on the monotonic clock: `None` when the skin has
    /// none. Each movie's clock starts at the first presented frame that
    /// draws it ([`Movies::presented`]).
    pub fn open(
        hub: &Arc<MediaHub>,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        start: Instant,
    ) -> Option<Self> {
        if bundle.skin().sheet().movies().is_empty() {
            return None;
        }
        #[cfg(all(test, feature = "dev-tools"))]
        if let Some(movies) = hub.supply.lock().unwrap().pop_front() {
            return Some(Self::of(hub, kind, movies, start));
        }
        let mut set = Self::entered(hub, kind, start);
        set.open_skin(hub, bundle);
        Some(set)
    }

    /// A play preparation's set, opened at `start` with `bundle` (if any),
    /// waiting for the video manifest of BGA generation `bga`: its movies,
    /// the chart's then the skin's, are opened once that manifest is there
    /// ([`chart`](Self::chart)); a practice round's starts before
    /// `boundary` are not counted.
    pub fn play(
        hub: &Arc<MediaHub>,
        bundle: Option<Arc<SkinBundle>>,
        bga: u64,
        boundary: i64,
        start: Instant,
    ) -> Self {
        let mut set = Self::entered(hub, ScreenKind::Play, start);
        set.bga = Some(bga);
        set.awaiting = Some(Awaiting {
            bga,
            bundle,
            boundary,
            by: start + MANIFEST_WAIT,
        });
        set
    }

    /// A set of a new generation, entered, with no movie yet.
    fn entered(hub: &Arc<MediaHub>, kind: ScreenKind, start: Instant) -> Self {
        let generation = hub.next_generation();
        #[cfg(test)]
        hub.open.fetch_add(1, Ordering::SeqCst);
        let mut movies = Movies::new(Default::default(), start);
        if let Some(s) = hub.session() {
            s.enter(generation);
            movies.set_waker(s.waker());
        }
        Self {
            hub: Arc::downgrade(hub),
            kind,
            generation,
            loading: None,
            movies,
            bga: None,
            awaiting: None,
        }
    }

    /// Opens `bundle`'s movies in the skin's slots.
    fn open_skin(&mut self, hub: &Arc<MediaHub>, bundle: &Arc<SkinBundle>) {
        #[cfg(all(test, feature = "dev-tools"))]
        if let Some(movies) = hub.supply.lock().unwrap().pop_front() {
            for (i, m) in movies.into_iter().enumerate() {
                if let Some(m) = m {
                    self.movies.put(i, m);
                }
            }
            return;
        }
        let sheet = bundle.skin().sheet();
        let session = hub.session();
        for (slot, name) in sheet.movies().iter().enumerate().take(MAX_MOVIES) {
            let file = match sheet.open_movie(slot) {
                Ok((file, canonical)) => match final_path(&file) {
                    Ok(at) if at == canonical => file,
                    Ok(at) => {
                        log::warn!(
                            "skin movie {name}: the file opened is {} rather than {}, so it is not played",
                            at.display(),
                            canonical.display()
                        );
                        continue;
                    }
                    Err(e) => {
                        log::warn!("skin movie {name}: its opened path cannot be read: {e}");
                        continue;
                    }
                },
                Err(e) => {
                    log::warn!("{}", missing_note(name, &e));
                    continue;
                }
            };
            let movie = Movie::new(self.generation);
            hub.ask(session, &movie, file, true);
            self.movies.put(slot, movie);
        }
    }

    /// The BGA generation whose manifest a play's set waits for, if it
    /// waits (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn awaits(&self) -> Option<u64> {
        self.awaiting.as_ref().map(|a| a.bga)
    }

    /// Takes `manifest`, BGA generation `bga`'s (`None`: none came within
    /// [`MANIFEST_WAIT`]), unless the set waits for another generation's or
    /// none: its chart videos are opened in the chart's slots, not
    /// looping, each from a duplicate of the manifest's handle, and then
    /// the skin's movies, so the session declares the chart's first. A file
    /// whose handle cannot be duplicated is absent. The chart's videos it
    /// took, which the attempt's play draws.
    pub fn chart(
        &mut self,
        bga: u64,
        manifest: Option<&crate::bga::Manifest>,
    ) -> Option<Arc<skin_model::bga::ChartVideos>> {
        let awaiting = self.awaiting.take_if(|a| a.bga == bga)?;
        let hub = self.hub.upgrade()?;
        let taken = manifest.map(|manifest| {
            self.movies
                .set_chart(Arc::clone(&manifest.videos), awaiting.boundary);
            self.open_chart(&hub, manifest);
            Arc::clone(&manifest.videos)
        });
        if let Some(bundle) = &awaiting.bundle {
            self.open_skin(&hub, bundle);
        }
        taken
    }

    /// Opens `manifest`'s chart videos in the chart's slots.
    fn open_chart(&mut self, hub: &Arc<MediaHub>, manifest: &crate::bga::Manifest) {
        #[cfg(all(test, feature = "dev-tools"))]
        if let Some(movies) = hub.supply_chart.lock().unwrap().pop_front() {
            for (m, slot) in movies.into_iter().zip(CHART_SLOTS) {
                if let Some(m) = m {
                    self.movies.put(slot, m);
                }
            }
            return;
        }
        let session = hub.session();
        for (file, slot) in manifest.files.iter().zip(CHART_SLOTS) {
            let Some(file) = file else { continue };
            let file = match file.try_clone() {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("chart video {slot}: its handle cannot be duplicated: {e}");
                    continue;
                }
            };
            let movie = Movie::chart(self.generation);
            hub.ask(session, &movie, file, false);
            self.movies.put(slot, movie);
        }
    }

    /// Past [`MANIFEST_WAIT`] at `now` with no manifest taken, a play's set
    /// opens its skin's movies alone, with the rest of the preparation's
    /// deadline to stage them: the chart's videos are absent. Whether its
    /// manifest is settled.
    pub fn settle_chart(&mut self, now: Instant) -> bool {
        if let Some(a) = &self.awaiting
            && now >= a.by
        {
            let bga = a.bga;
            log::warn!(
                "chart videos: no manifest within {} ms of the preparation's start; they are absent and the skin's movies open alone",
                MANIFEST_WAIT.as_millis()
            );
            #[cfg(feature = "dev-tools")]
            crate::dev::loop_probe::mark(
                "movies:manifest_missed_ms",
                i64::try_from(MANIFEST_WAIT.as_millis()).unwrap_or(i64::MAX),
            );
            let _ = self.chart(bga, None);
        }
        self.awaiting.is_none()
    }

    /// A set of `movies` for a `kind` screen, opened at `start`, as
    /// [`open`](Self::open) makes it with no session (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn of(
        hub: &Arc<MediaHub>,
        kind: ScreenKind,
        movies: [Option<Arc<Movie>>; MAX_MOVIES],
        start: Instant,
    ) -> Self {
        hub.open.fetch_add(1, Ordering::SeqCst);
        Self {
            hub: Arc::downgrade(hub),
            kind,
            generation: hub.next_generation(),
            loading: None,
            movies: Movies::skin(movies, start),
            bga: None,
            awaiting: None,
        }
    }

    /// The generation it entered (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The screen kind it was opened for.
    pub const fn kind(&self) -> ScreenKind {
        self.kind
    }
}

impl Drop for MovieSet {
    fn drop(&mut self) {
        // The screen is left: its players close. A session never started
        // has none, and a hub gone took its session with it.
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        #[cfg(test)]
        hub.open.fetch_sub(1, Ordering::SeqCst);
        let generation = hub.next_generation();
        if let Some(Some(s)) = hub.session.get() {
            s.enter(generation);
        }
    }
}

/// The screen instance a set of movies belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The selection entry of this lifetime.
    Selection(u64),
    /// The play preparation of this request, not committed yet.
    Preparing(u64),
    /// That preparation committed, its play not begun.
    Committed(u64),
    /// The play screen it became.
    Playing,
}

/// The screen a frame drawn belongs to, as a set's holder judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawn {
    /// The selection skin's frame of this lifetime.
    Selection(u64),
    /// Selection's built-in page, drawn when no selection skin draws (none
    /// installed, or not the one its state is for): selection was entered
    /// all the same.
    BuiltInSelection,
    /// A play's frame.
    Play,
    /// Anything else: a page (the decision or practice page), a loading
    /// frame, a result, the end page, a splash or a text.
    Other,
}

impl Holder {
    /// Whether its screen instance goes on while a `drawn` frame is drawn:
    /// a selection entry's while its own skinned frames draw; a play's
    /// preparation whatever draws meanwhile (its cancel ends it), and once
    /// committed until a selection frame draws, skinned or built-in; a
    /// play screen while play frames draw.
    const fn keeps(self, drawn: Drawn) -> bool {
        match (self, drawn) {
            (Self::Selection(l), Drawn::Selection(d)) => l == d,
            (Self::Preparing(_), _)
            | (Self::Committed(_), Drawn::Play | Drawn::Other)
            | (Self::Playing, Drawn::Play) => true,
            _ => false,
        }
    }
}

/// The movies of the screen instance shown or being prepared, with their
/// holder; the renderer's part of them goes with them. It also keeps the
/// latest video manifest, by its BGA generation, for a play's set of that
/// generation, whichever comes first.
pub struct ScreenMovies {
    held: Option<(Holder, MovieSet)>,
    manifest: Option<(u64, crate::bga::Manifest)>,
}

impl ScreenMovies {
    /// None held.
    pub const fn new() -> Self {
        Self {
            held: None,
            manifest: None,
        }
    }

    /// Keeps `manifest`, BGA generation `generation`'s, in place of the one
    /// kept before (its files closed), for [`offer`](Self::offer).
    pub fn keep_manifest(&mut self, generation: u64, manifest: crate::bga::Manifest) {
        self.manifest = Some((generation, manifest));
    }

    /// The play preparation's set held, waiting for the manifest kept,
    /// takes it ([`MovieSet::chart`]): the chart's videos it took.
    pub fn offer(&mut self) -> Option<Arc<skin_model::bga::ChartVideos>> {
        let (Some((Holder::Preparing(_), set)), Some((generation, manifest))) =
            (&mut self.held, &self.manifest)
        else {
            return None;
        };
        set.chart(*generation, Some(manifest))
    }

    /// The chart is left: the manifest kept goes, its files closed.
    pub fn forget_manifest(&mut self) {
        self.manifest = None;
    }

    /// Whether a manifest is kept (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub const fn keeps_manifest(&self) -> bool {
        self.manifest.is_some()
    }

    /// The set held and its holder.
    pub fn get(&self) -> Option<(Holder, &MovieSet)> {
        self.held.as_ref().map(|(h, s)| (*h, s))
    }

    /// The set held and its holder.
    pub fn get_mut(&mut self) -> Option<(Holder, &mut MovieSet)> {
        self.held.as_mut().map(|(h, s)| (*h, s))
    }

    /// Its screen instance ends: the set is dropped (its generation left,
    /// its players closed), and `renderer` releases every slot's planes
    /// and shows no movie in its loading frames or play builds. The one
    /// place a set is let go while its renderer lives. A committed play's
    /// set takes the manifest kept for its BGA generation with it, its
    /// files closed: the next attempt is a new generation, with a manifest
    /// of its own. A preparation's, cancelled before its commit, leaves it
    /// for the next preparation of the same generation, which reads
    /// nothing again.
    pub fn retire(&mut self, renderer: &mut Renderer) {
        let Some((holder, set)) = self.held.take() else {
            return;
        };
        if !matches!(holder, Holder::Preparing(_))
            && let Some(bga) = set.bga
        {
            self.manifest.take_if(|(g, _)| *g == bga);
        }
        drop(set);
        renderer.retire_movies();
    }

    /// The set held retired, then `open`'s held for `holder`, opened once
    /// the one before it is left.
    pub fn open(
        &mut self,
        holder: Holder,
        renderer: &mut Renderer,
        open: impl FnOnce() -> Option<MovieSet>,
    ) {
        self.retire(renderer);
        self.held = open().map(|set| (holder, set));
    }

    /// A `drawn` frame is drawn: the set held is retired unless its screen
    /// instance goes on ([`Holder::keeps`]).
    pub fn drawn(&mut self, drawn: Drawn, renderer: &mut Renderer) {
        if self.held.as_ref().is_some_and(|(h, _)| !h.keeps(drawn)) {
            self.retire(renderer);
        }
    }

    /// Play preparation `request` (`None`: whichever) is cancelled or
    /// discarded: the set it holds, not committed, is retired.
    pub fn cancel(&mut self, request: Option<u64>, renderer: &mut Renderer) {
        if let Some((Holder::Preparing(r), _)) = &self.held
            && request.is_none_or(|q| q == *r)
        {
            self.retire(renderer);
        }
    }

    /// The committed play of preparation `request` is left before it
    /// began (a quit after its commit and before READY): the set it holds
    /// is retired. A set of another request, or one not committed (a
    /// preparation kept for a local retry), is left as it is.
    pub fn abandon(&mut self, request: u64, renderer: &mut Renderer) {
        if matches!(&self.held, Some((Holder::Committed(r), _)) if *r == request) {
            self.retire(renderer);
        }
    }

    /// Play preparation `request` is committed: the set it holds is its
    /// play's.
    pub fn commit(&mut self, request: u64) {
        if let Some((holder, _)) = &mut self.held
            && *holder == Holder::Preparing(request)
        {
            *holder = Holder::Committed(request);
        }
    }

    /// The committed play begins: its screen draws its movies from here,
    /// each on the clock it started in a loading frame, or starting at the
    /// first play frame presented that draws it.
    pub fn begin_play(&mut self, renderer: &mut Renderer) {
        self.drawn(Drawn::Play, renderer);
        if let Some((holder, _)) = &mut self.held
            && matches!(holder, Holder::Committed(_))
        {
            *holder = Holder::Playing;
        }
    }
}

/// The note of movie `name`'s file that could not be opened again at
/// screen entry (`e`): the import's note for a missing asset, in the form
/// the skin's import log writes its notes (where, severity, kind).
fn missing_note(name: &str, e: &dyn std::fmt::Display) -> String {
    format!(
        "skin movie {name}: {:?}: {} ({e})",
        skin_beatoraja::Severity::Dropped,
        skin_beatoraja::NoteKind::MissingAsset
    )
}

/// The final path of the file `file` has open, as the system resolves it
/// (`GetFinalPathNameByHandleW`, normalized, with a drive letter): the
/// form [`std::fs::canonicalize`] gives.
pub(crate) fn final_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};
    let handle = HANDLE(file.as_raw_handle());
    let mut buf = vec![0_u16; 512];
    loop {
        // SAFETY: `handle` is the open file's; the buffer is writable for
        // its length.
        let n = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, VOLUME_NAME_DOS) } as usize;
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
        }
        // Too small: `n` is the length it needs, nul included.
        buf.resize(n, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file gone between the import and the screen's entry is noted as
    /// the import notes a missing asset, and takes no player.
    #[test]
    fn a_failed_reopen_is_noted_as_a_missing_asset() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        let note = missing_note("a.mp4", &e);
        assert!(
            note.starts_with("skin movie a.mp4: Dropped: a missing or unreadable file ("),
            "{note}"
        );
    }

    #[test]
    fn final_path_is_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        std::fs::write(&path, b"x").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(final_path(&file).unwrap(), path.canonicalize().unwrap());
    }
}
