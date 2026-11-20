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
//! None of this waits on the session: entering stores a word and an open
//! only queues a request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use render::SkinBundle;
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
        })
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
    hub: Arc<MediaHub>,
    kind: ScreenKind,
    /// The skin it was opened for.
    bundle: Arc<SkinBundle>,
    /// The screen's lifetime it was opened in (a selection's entry), 0
    /// for play.
    lifetime: u64,
    /// The screen's clock at its opening (µs), from which a selection
    /// frame's movie time counts.
    pub origin: i64,
    /// When its preparation started, on the monotonic clock.
    pub start: Instant,
    pub movies: Movies,
}

impl MovieSet {
    /// Opens `bundle`'s movies for a `kind` screen of `lifetime`, its
    /// preparation starting at `start` on the monotonic clock and at
    /// `origin` µs on the screen's: `None` when the skin has none.
    pub fn open(
        hub: &Arc<MediaHub>,
        kind: ScreenKind,
        bundle: &Arc<SkinBundle>,
        lifetime: u64,
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
                    log::warn!(
                        "skin movie {name}: a missing or unreadable file ({e}), so it is not played"
                    );
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
        Some(Self {
            hub: Arc::clone(hub),
            kind,
            bundle: Arc::clone(bundle),
            lifetime,
            origin,
            start,
            movies: Movies::new(movies, start),
        })
    }

    /// Whether it is the movies of `bundle`'s `kind` screen of `lifetime`.
    pub fn is_for(&self, kind: ScreenKind, bundle: &Arc<SkinBundle>, lifetime: u64) -> bool {
        self.kind == kind && self.lifetime == lifetime && Arc::ptr_eq(&self.bundle, bundle)
    }

    /// The screen kind it was opened for.
    pub const fn kind(&self) -> ScreenKind {
        self.kind
    }
}

impl Drop for MovieSet {
    fn drop(&mut self) {
        // The screen is left: its players close. A session never started
        // has none.
        if let Some(Some(s)) = self.hub.session.get() {
            s.enter(self.hub.next_generation());
        }
    }
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

    #[test]
    fn final_path_is_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        std::fs::write(&path, b"x").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(final_path(&file).unwrap(), path.canonicalize().unwrap());
    }
}
