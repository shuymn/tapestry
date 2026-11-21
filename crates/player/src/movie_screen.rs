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
//! Each set has one owner: a selection's entry (its lifetime), or a play
//! state (its number), so a direct play's retry, which starts a new play
//! state, opens its movies again from their start while a practice
//! session's rounds keep theirs. Everything of a screen's movies lives as
//! long as that owner: the set and its session generation, the renderer's
//! planes of each slot, and the shows of the loading frames. The screen
//! holds them through [`ScreenMovies`], whose only ways to let a set go
//! retire the renderer's part with it ([`ScreenMovies::retire`]); they are
//! retired when their owner leaves: another screen's set opened, the play
//! or selection screen left, a play preparation's request cancelled or
//! discarded before its commit (a set belongs to its request until then).
//!
//! A committed set outlives the screen object across a device's recovery:
//! the screen dropped parks it in the hub ([`MediaHub::park`]) and the one
//! built again takes it back, so its movies keep their lifetime, clock
//! origin and losses, and only their GPU planes are made again
//! ([`Movies::rebuilt`]), at that screen's build. A set still preparing is
//! dropped with the screen instead: its preparation's answer names the
//! lost device, so nothing can commit it, and a cancel arriving while no
//! screen exists has none to reach.
//!
//! None of this waits on the session: entering stores a word and an open
//! only queues a request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Instant;

use render::{Renderer, SkinBundle};
use skin_model::ScreenKind;
use skin_model::movie::MAX_MOVIES;

use crate::media_host::launch::Setup;
use crate::media_host::session::{Deadlines, Launching, Loss, Movie, Session};
use crate::movie_frame::Movies;

/// The run's media session, made when a screen first needs it.
pub struct MediaHub {
    /// How the worker is launched; `None` when the installation could not
    /// be found, and every movie is then lost.
    launching: Option<Launching>,
    session: OnceLock<Option<Session>>,
    generation: AtomicU64,
    /// The committed set of a screen dropped for its device's recovery,
    /// until the screen built again takes it.
    parked: Mutex<Option<MovieSet>>,
    /// The sets open (tests).
    #[cfg(test)]
    open: AtomicU64,
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
            parked: Mutex::new(None),
            #[cfg(test)]
            open: AtomicU64::new(0),
        })
    }

    /// Keeps `set` for the screen built next ([`unpark`](Self::unpark)),
    /// dropping one kept before.
    pub fn park(&self, set: MovieSet) {
        let old = self
            .parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(set);
        drop(old);
    }

    /// The set parked for a screen built again, its GPU state to be made
    /// again on that screen's device.
    pub fn unpark(&self) -> Option<MovieSet> {
        let mut set = self
            .parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        set.movies.rebuilt();
        Some(set)
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

    /// Whether a set is parked (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn parked(&self) -> bool {
        self.parked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
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
    /// Weak: a set parked in its hub must not keep the hub alive.
    hub: Weak<MediaHub>,
    kind: ScreenKind,
    /// The skin it was opened for.
    bundle: Arc<SkinBundle>,
    /// Its owner: the selection's entry (its lifetime) it was opened in,
    /// or the play state (its number) it plays for.
    owner: u64,
    /// The generation it entered (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    generation: u64,
    /// The screen's clock at its opening (µs), from which a selection
    /// frame's movie time counts.
    pub origin: i64,
    /// A play's screen clock origin as the first loading frame drawn with
    /// it gave it (that loading lifetime's beginning), from which the
    /// loading frames' movie time counts; kept by the later rounds of its
    /// play state, as the screen clock is.
    pub loading_origin: Option<i64>,
    /// The play preparation request it belongs to until that is
    /// committed: its cancel or discard drops the set.
    pub preparing: Option<u64>,
    pub movies: Movies,
}

impl MovieSet {
    /// Opens `bundle`'s movies for a `kind` screen of `owner`, its
    /// preparation starting at `start` on the monotonic clock and at
    /// `origin` µs on the screen's: `None` when the skin has none.
    pub fn open(
        hub: &Arc<MediaHub>,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        owner: u64,
        start: Instant,
        origin: i64,
    ) -> Option<Self> {
        let sheet = bundle.skin().sheet();
        let names = sheet.movies();
        if names.is_empty() {
            return None;
        }
        let session = hub.session();
        let generation = hub.next_generation();
        if let Some(s) = session {
            s.enter(generation);
        }
        let mut movies: [Option<Arc<Movie>>; MAX_MOVIES] = Default::default();
        for (slot, (name, out)) in names.iter().zip(movies.iter_mut()).enumerate() {
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
            let movie = Movie::new(generation);
            match session {
                Some(s) => s.open(&movie, file, true),
                None => {
                    let _ = movie.lose(Loss::Launch);
                }
            }
            *out = Some(movie);
        }
        #[cfg(test)]
        hub.open.fetch_add(1, Ordering::SeqCst);
        Some(Self {
            hub: Arc::downgrade(hub),
            kind,
            bundle: Arc::clone(bundle),
            owner,
            #[cfg(all(test, feature = "dev-tools"))]
            generation,
            origin,
            loading_origin: None,
            preparing: None,
            // A play's READY opens at least the skin's loading minimum
            // into its screen clock (design.md "Timers").
            movies: match kind {
                ScreenKind::Play => Movies::new(movies, start)
                    .floored(game::loading_minimum(bundle.skin().loadend()).0),
                _ => Movies::new(movies, start),
            },
        })
    }

    /// A set of `movies` for `bundle`'s `kind` screen of `owner`, opened
    /// at `start` and screen time `origin`, as [`open`](Self::open) makes
    /// it with no session (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn of(
        hub: &Arc<MediaHub>,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        owner: u64,
        movies: [Option<Arc<Movie>>; MAX_MOVIES],
        start: Instant,
    ) -> Self {
        hub.open.fetch_add(1, Ordering::SeqCst);
        Self {
            hub: Arc::downgrade(hub),
            kind,
            bundle: Arc::clone(bundle),
            owner,
            generation: hub.next_generation(),
            origin: 0,
            loading_origin: None,
            preparing: None,
            movies: match kind {
                ScreenKind::Play => Movies::new(movies, start)
                    .floored(game::loading_minimum(bundle.skin().loadend()).0),
                _ => Movies::new(movies, start),
            },
        }
    }

    /// Whether it is the movies of `bundle`'s `kind` screen of `owner`.
    pub fn is_for(&self, kind: ScreenKind, bundle: &Arc<SkinBundle>, owner: u64) -> bool {
        self.kind == kind && self.owner == owner && Arc::ptr_eq(&self.bundle, bundle)
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

/// The movies of the screen shown or being prepared, held with the
/// renderer's part of them: a set let go retires the renderer's planes and
/// loading shows with it.
pub struct ScreenMovies {
    set: Option<MovieSet>,
}

impl ScreenMovies {
    /// None held.
    pub const fn new() -> Self {
        Self { set: None }
    }

    /// The set held.
    pub const fn get(&self) -> Option<&MovieSet> {
        self.set.as_ref()
    }

    /// The set held.
    pub const fn get_mut(&mut self) -> Option<&mut MovieSet> {
        self.set.as_mut()
    }

    /// Their owner leaves: the set is dropped (its generation left, its
    /// players closed), and `renderer` releases every slot's planes and
    /// shows no movie in its loading frames or play builds.
    pub fn retire(&mut self, renderer: &mut Renderer) {
        if self.set.take().is_some() {
            renderer.retire_movies();
        }
    }

    /// The set held retired, then `open`'s, opened once the one before
    /// it is left.
    pub fn replace(&mut self, renderer: &mut Renderer, open: impl FnOnce() -> Option<MovieSet>) {
        self.retire(renderer);
        self.set = open();
    }

    /// Holds `set`, taken back from the hub by a screen built again over
    /// `renderer`, and reserves its admitted movies' planes there: the
    /// screen's build is their admission boundary.
    pub fn restore(&mut self, set: Option<MovieSet>, renderer: &mut Renderer) {
        self.retire(renderer);
        self.set = set;
        if let Some(set) = &mut self.set {
            set.movies.admit(renderer, Instant::now());
        }
    }

    /// The screen is dropped (its device's recovery builds another): a
    /// committed set is parked in `hub` for the screen built next; one
    /// still preparing is dropped, since its preparation can no longer
    /// commit.
    pub fn park(&mut self, hub: &MediaHub) {
        match self.set.take() {
            Some(set) if set.preparing.is_none() => hub.park(set),
            set => drop(set),
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
fn final_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
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
