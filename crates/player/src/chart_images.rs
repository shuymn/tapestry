//! The chart's own images on play (S10-12 design §5): the stage file and
//! back image an attempt's labels name, read from the chart's folder under
//! T08's rules and fitted into the play installation's chart layer on one
//! owned worker, then published by the render thread, and settled before
//! the attempt's final readiness.
//!
//! - **The set** ([`ChartImages`]) is the two reads, each with the
//!   observation its pixels came from. A later attempt re-resolves both
//!   names and keeps a read whose observation is unchanged (design.md
//!   "Load Pipeline", Freshness): only a changed or new file is decoded.
//! - **The worker** ([`ChartImageWorker`]) runs at most one job, never
//!   beginning one while another's thread has not ended; a job of a chart
//!   left is cancelled through the choice's scope and kept until its
//!   thread ends. A panic, or a thread that cannot start, makes the
//!   images it had not finished absent and never ends the run.
//! - **The target** is the play installation the attempt draws with: the
//!   committed one, or a pending skin's specialization staged with an
//!   array of its own. The render thread answers what each publication
//!   left ([`Published`]); an installation with no chart layer is given
//!   the images absent, so a skin's "no image" presentation shows.
//! - **Direct launch:** with no coordinator, the images are read
//!   synchronously on the game thread in the attempt's preparation.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use game::coord::{
    Class, Coordinator, JobKey, JobKind, Outcome, OwnedJob, Owner, Scope, Settled, Spawner,
};
use skin_model::chart_image::{
    self, Absent, ChartDir, ChartImage, ChartTable, FileIdentity, Fitted, Read,
};

/// What a publication left ([`render::ChartOutcome`] when it failed).
pub type Published = Result<ChartTable, render::ChartOutcome>;

/// The chart's stage file and back image as read, by [`ChartImage::index`].
#[derive(Debug)]
pub struct ChartImages {
    reads: [Arc<Read>; 2],
}

impl ChartImages {
    /// Both absent for `why`.
    #[must_use]
    pub fn absent(why: &Absent) -> Self {
        let read = || {
            Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(why.clone()),
            })
        };
        Self {
            reads: [read(), read()],
        }
    }

    /// The fitted images, absent ones `None`.
    #[must_use]
    pub fn fitted(&self) -> [Option<&Fitted>; 2] {
        self.reads.each_ref().map(|r| r.image.as_ref().ok())
    }

    /// The read of `which`.
    #[must_use]
    pub fn read(&self, which: ChartImage) -> &Read {
        let [stage, back] = &self.reads;
        match which {
            ChartImage::Stage => stage,
            ChartImage::Back => back,
        }
    }

    /// The bytes of its fitted levels, for the memory witness.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.fitted()
            .iter()
            .flatten()
            .flat_map(|f| f.levels().iter())
            .map(|l| u64::try_from(l.len()).unwrap_or(u64::MAX))
            .fold(0, u64::saturating_add)
    }
}

/// What an attempt asks of the images: the chart's folder, its stage file
/// and back image names as its labels hold them, and the page side of the
/// target's chart layer; `None` when the target keeps no chart layer, whose
/// images are published absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub dir: PathBuf,
    pub names: [String; 2],
    pub side: Option<u32>,
}

/// Reads `want`'s images, re-resolving each name and keeping `prior`'s
/// read of an unchanged file (both observations equal, identities
/// available), and asking `cancel` at the reader's checkpoints. One image at
/// a time.
#[must_use]
pub fn read_set(
    want: &Want,
    side: u32,
    prior: Option<&ChartImages>,
    cancel: &dyn Fn() -> bool,
) -> ChartImages {
    let dir = match ChartDir::new(&want.dir) {
        Ok(dir) => dir,
        Err(e) => {
            return ChartImages::absent(&Absent::Unresolved(chart_image::Unresolved::Io(e.kind())));
        }
    };
    let identity: chart_image::Identity<'_> = &file_identity;
    let mut reads = Vec::with_capacity(2);
    for (which, name) in ChartImage::ALL.into_iter().zip(&want.names) {
        let Some((_, slot)) = which.slot(side) else {
            reads.push(Arc::new(Read {
                binding: chart_image::Binding::NoHeader,
                image: Err(Absent::NoHeader),
            }));
            continue;
        };
        if let Some(prior) = prior {
            let kept = prior.read(which);
            let now = chart_image::resolve(&dir, name, &[], identity);
            if now.same_as(&kept.binding) && kept.image.is_ok() {
                reads.push(Arc::clone(&prior.reads[which.index()]));
                continue;
            }
        }
        reads.push(Arc::new(chart_image::read(
            &dir,
            name,
            &[],
            slot,
            cancel,
            identity,
        )));
    }
    let mut it = reads.into_iter();
    match (it.next(), it.next()) {
        (Some(stage), Some(back)) => ChartImages {
            reads: [stage, back],
        },
        _ => ChartImages::absent(&Absent::Failed),
    }
}

/// An open file's identity: its volume's serial number and its file
/// index (the NTFS file id), `None` when the system cannot give it.
#[must_use]
pub fn file_identity(file: &File) -> Option<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is the open file's for the call's duration, and
    // `info` is a valid out-parameter of the call's type.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &raw mut info) }.ok()?;
    Some(FileIdentity(
        (u128::from(info.dwVolumeSerialNumber) << 64)
            | (u128::from(info.nFileIndexHigh) << 32)
            | u128::from(info.nFileIndexLow),
    ))
}

/// The job's thread's bound at the run's end, and between attempts the
/// slice a wait for the worker parks.
pub const STOP_WAIT: Duration = Duration::from_secs(2);

/// A publication asked for and its answer.
struct Publication {
    /// The attempt request it was for (`None`: the committed installation
    /// before any staging).
    request: Option<u64>,
    /// The set published, `None` for absent.
    set: Option<Arc<ChartImages>>,
    /// The answer, until it came.
    rx: Option<mpsc::Receiver<Published>>,
    /// The answer.
    answer: Option<Published>,
}

/// The run's one owner of the chart-image worker (module doc).
#[derive(Default)]
pub struct ChartImageWorker {
    /// What the current attempt asks for.
    want: Option<Want>,
    /// The job reading `want`'s images, its thread not yet joined.
    job: Option<(Want, OwnedJob<Arc<ChartImages>>)>,
    /// A job of an earlier want, its thread not yet joined.
    obsolete: Option<OwnedJob<Arc<ChartImages>>>,
    /// The images last read, and what they were read for.
    set: Option<(Want, Arc<ChartImages>)>,
    /// The set was drawn by a committed attempt: the next attempt of the
    /// same want re-resolves its files first.
    used: bool,
    /// The next step re-resolves the set's files.
    refresh: bool,
    /// The last publication.
    publication: Option<Publication>,
    /// Jobs begun (tests count decodes by it).
    pub begun: u64,
}

impl std::fmt::Debug for ChartImageWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChartImageWorker")
            .field("want", &self.want)
            .field("job", &self.job.as_ref().map(|(w, _)| w))
            .field("obsolete", &self.obsolete.is_some())
            .field("set", &self.set.as_ref().map(|(w, _)| w))
            .finish_non_exhaustive()
    }
}

impl ChartImageWorker {
    /// Asks for `want`'s images for the attempt being prepared. A new want
    /// drops the last publication, which is published again for it. The
    /// same want after an attempt drew the set (a retry or a practice
    /// round) re-resolves its files, keeping what did not change, and
    /// publishes again (design C11, P22).
    pub fn want(&mut self, want: Want) {
        if self.want.as_ref() != Some(&want) {
            self.publication = None;
        }
        if self.used && self.set.as_ref().is_some_and(|(w, _)| *w == want) {
            self.refresh = true;
            self.used = false;
            self.publication = None;
        }
        self.want = Some(want);
    }

    /// The current set was drawn by an attempt that committed.
    pub fn committed(&mut self) {
        self.used = self.current().is_some();
    }

    /// Whether the last chart asked for `want`'s folder and names: the
    /// same chart's images, whatever the target.
    #[must_use]
    pub fn wants(&self, want: &Want) -> bool {
        self.want
            .as_ref()
            .is_some_and(|w| w.dir == want.dir && w.names == want.names)
    }

    /// Forgets what the last chart asked for: a new chart was chosen. Its
    /// job, if running, was cancelled through the choice's scope and is
    /// kept until its thread ends.
    pub fn forget(&mut self) {
        self.want = None;
        self.publication = None;
        self.used = false;
        self.refresh = false;
        if let Some((_, job)) = self.job.take() {
            self.retire(job);
        }
    }

    fn retire(&mut self, job: OwnedJob<Arc<ChartImages>>) {
        if let Some(old) = self.obsolete.take() {
            // Two ended jobs at once: the older is settled now; its
            // thread has ended or ends promptly once cancelled.
            settle_logged(old);
        }
        self.obsolete = Some(job);
    }

    /// Advances the worker without blocking: joins ended jobs, takes a
    /// finished job's set, and begins the job `want` needs once no job's
    /// thread is alive. `coord` `None` (the direct launch) reads at once on
    /// this thread, asking `cancel`.
    pub fn step(&mut self, coord: Option<(&Coordinator, Spawner)>, cancel: &dyn Fn() -> bool) {
        if self.obsolete.as_ref().is_some_and(OwnedJob::finished)
            && let Some(old) = self.obsolete.take()
        {
            settle_logged(old);
        }
        if let Some((w, job)) = self.job.take() {
            if job.finished() {
                let set = match job.poll().map(|o| o.outcome) {
                    Some(Outcome::Done(set)) => set,
                    Some(Outcome::Panicked(p)) => {
                        log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
                        Arc::new(ChartImages::absent(&Absent::Failed))
                    }
                    Some(Outcome::Cancelled) | None => {
                        Arc::new(ChartImages::absent(&Absent::Cancelled))
                    }
                };
                settle_logged(job);
                if self.want.as_ref() == Some(&w) {
                    log_absent(&set);
                    self.set = Some((w, set));
                    self.refresh = false;
                    log::info!(
                        "chart images: read; {} bytes retained",
                        self.retained_bytes()
                    );
                }
            } else {
                self.job = Some((w, job));
            }
        }
        let Some(want) = self.want.clone() else {
            return;
        };
        let Some(side) = want.side else {
            return;
        };
        let current = self.set.as_ref().is_some_and(|(w, _)| *w == want);
        if (current && !self.refresh) || self.job.is_some() || self.obsolete.is_some() {
            return;
        }
        // Kept while its files are re-resolved: an unchanged file's read is
        // kept, not decoded again.
        let prior = self.set.as_ref().map(|(_, s)| Arc::clone(s));
        self.begun += 1;
        let Some((coord, spawner)) = coord else {
            let set = Arc::new(read_set(&want, side, prior.as_deref(), cancel));
            log_absent(&set);
            self.set = Some((want, set));
            self.refresh = false;
            log::info!(
                "chart images: read; {} bytes retained",
                self.retained_bytes()
            );
            return;
        };
        let key = JobKey {
            kind: JobKind::ChartImages,
            path: want.dir.clone(),
            speed: bms_model::Speed::NORMAL,
        };
        let read = want.clone();
        let spawned = coord.spawn_owned(
            spawner,
            key,
            (Class::Foreground, false, Owner::Foreground),
            coord.subscriber(Scope::Choice),
            |_| {},
            move |ctx| Arc::new(read_set(&read, side, prior.as_deref(), &|| ctx.cancelled())),
        );
        match spawned {
            Ok(job) => self.job = Some((want, job)),
            Err(e) => {
                // A thread that cannot start: the images absent, the run
                // goes on (design C13).
                log::warn!("chart images: {e}");
                self.set = Some((want, Arc::new(ChartImages::absent(&Absent::Failed))));
                self.refresh = false;
            }
        }
    }

    /// The set `want` asked for, once read (and re-resolved, when asked).
    #[must_use]
    pub fn current(&self) -> Option<&Arc<ChartImages>> {
        self.set
            .as_ref()
            .filter(|(w, _)| !self.refresh && Some(w) == self.want.as_ref())
            .map(|(_, s)| s)
    }

    /// Publishes the current set (or absent, for a target with no chart
    /// layer) for the attempt of `request` once it is read, and takes its
    /// answer: `Some` once the publication for that request has returned.
    pub fn publish(
        &mut self,
        render: &crate::render_side::RenderLink,
        request: Option<u64>,
    ) -> Option<&Published> {
        let want = self.want.as_ref()?;
        let set = match want.side {
            Some(_) => Some(Arc::clone(self.current()?)),
            None => None,
        };
        let same = self.publication.as_ref().is_some_and(|p| {
            p.request == request
                && match (&p.set, &set) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        });
        if !same {
            let rx = render.publish_chart_images(request, set.clone());
            self.publication = Some(Publication {
                request,
                set,
                rx: Some(rx),
                answer: None,
            });
        }
        let p = self.publication.as_mut()?;
        if let Some(rx) = &p.rx {
            match rx.try_recv() {
                Ok(answer) => {
                    match &answer {
                        Ok(t) => log::info!(
                            "chart images: published (stage file {}, back image {})",
                            t.present(ChartImage::Stage),
                            t.present(ChartImage::Back)
                        ),
                        Err(o) => log::info!("chart images: not published: {o:?}"),
                    }
                    p.answer = Some(answer);
                    p.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    p.answer = Some(Err(render::ChartOutcome::Unavailable));
                    p.rx = None;
                }
            }
        }
        p.answer.as_ref()
    }

    /// Whether no job's thread is alive.
    #[must_use]
    pub fn idle(&self) -> bool {
        self.job.is_none() && self.obsolete.is_none()
    }

    /// The bytes of the sets it keeps: the current one and, while the
    /// render thread may still hold it, the last published.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let set = self.set.as_ref().map_or(0, |(_, s)| s.bytes());
        let published = self
            .publication
            .as_ref()
            .and_then(|p| p.set.as_ref())
            .filter(|p| self.set.as_ref().is_none_or(|(_, s)| !Arc::ptr_eq(p, s)))
            .map_or(0, |s| s.bytes());
        set.saturating_add(published)
    }

    /// At the run's end: every job is stopped and joined within
    /// [`STOP_WAIT`], or left to the process's exit (design P17); a panic
    /// is logged, never the run's.
    pub fn end(&mut self) {
        self.publication = None;
        let deadline = Instant::now() + STOP_WAIT;
        for job in [self.job.take().map(|(_, j)| j), self.obsolete.take()]
            .into_iter()
            .flatten()
        {
            let bound = deadline.saturating_duration_since(Instant::now());
            match job.settle(bound, &Instant::now, &mut || Some(())) {
                Settled::Joined {
                    after_panic: Some(p),
                    ..
                } => log::warn!("chart images: the worker stopped unexpectedly: {}", p.0),
                Settled::Joined { .. } => {}
                Settled::Abandoned { .. } => {
                    log::warn!("chart images: the worker was left to end with the process");
                }
            }
        }
    }
}

/// Settles an ended job, logging a panic after its publication.
fn settle_logged(job: OwnedJob<Arc<ChartImages>>) {
    if let Settled::Joined {
        after_panic: Some(p),
        ..
    } = job.settle(STOP_WAIT, &Instant::now, &mut || Some(()))
    {
        log::warn!("chart images: the worker stopped unexpectedly: {}", p.0);
    }
}

/// One load warning per image a chart names and could not be drawn
/// (S10-12 design §4.1).
fn log_absent(set: &ChartImages) {
    for which in ChartImage::ALL {
        match &set.read(which).image {
            Ok(_) | Err(Absent::NoHeader | Absent::Cancelled) => {}
            Err(why) => {
                let name = match which {
                    ChartImage::Stage => "stage file",
                    ChartImage::Back => "back image",
                };
                log::warn!("chart: the {name} is not shown: {why}");
                eprintln!("chart: the {name} is not shown: {why}");
            }
        }
    }
}

/// The chart's folder of `path`.
#[must_use]
pub fn folder(path: &Path) -> PathBuf {
    path.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use game::coord::{Caps, Coordinator, JobKind, Scope, Spawner};
    use skin_model::chart_image::{Absent, ChartImage};

    use super::{ChartImageWorker, Want};

    const CAPS: Caps = Caps {
        foreground: 4,
        speculative: 1,
    };
    const SIDE: u32 = 2_048;

    /// A `w`×`h` PNG of `rgb` at `dir/name`.
    fn png(dir: &Path, name: &str, [w, h]: [u32; 2], [r, g, b]: [u8; 3]) {
        let file = std::fs::File::create(dir.join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let texels = [r, g, b, 255].repeat(usize::try_from(w * h).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
    }

    fn want(dir: &Path, side: Option<u32>) -> Want {
        Want {
            dir: dir.to_path_buf(),
            names: ["s.png".to_owned(), "b.png".to_owned()],
            side,
        }
    }

    /// Steps `w` until `done` or 10 s.
    fn step_until(
        w: &mut ChartImageWorker,
        coord: &Coordinator,
        spawner: Spawner,
        done: impl Fn(&ChartImageWorker) -> bool,
    ) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(w) {
            assert!(Instant::now() < end, "{w:?}");
            w.step(Some((coord, spawner)), &|| false);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A chart's stage file (640×480) and back image (512×384) are read on
    /// the worker, one job, and its thread is joined once it ended.
    #[test]
    fn the_worker_reads_both_images() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [640, 480]
        );
        assert_eq!(
            set.read(ChartImage::Back).image.as_ref().unwrap().native(),
            [512, 384]
        );
        assert_eq!(w.begun, 1);
        assert!(set.bytes() > 0);
    }

    /// A retry after an attempt drew the set re-resolves both files: an
    /// unchanged one keeps its read (the same pixels, not decoded again),
    /// a rewritten one is read again, and a name missing before and
    /// present now is read.
    #[test]
    fn a_retry_reads_only_what_changed() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
        let ready = |w: &ChartImageWorker| w.current().is_some() && w.idle();
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let first = std::sync::Arc::clone(w.current().unwrap());
        assert!(matches!(
            first.read(ChartImage::Back).image,
            Err(Absent::Unresolved(_))
        ));
        // Not drawn yet: the same want is the same set.
        w.want(want(d.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
        assert!(std::sync::Arc::ptr_eq(w.current().unwrap(), &first));
        // Drawn, then the back image appears: the stage file kept.
        w.committed();
        png(d.path(), "b.png", [512, 384], [40, 50, 60]);
        w.want(want(d.path(), Some(SIDE)));
        assert!(w.current().is_none(), "unavailable while re-resolved");
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let second = std::sync::Arc::clone(w.current().unwrap());
        assert!(std::ptr::eq(
            first.read(ChartImage::Stage),
            second.read(ChartImage::Stage)
        ));
        assert_eq!(
            second
                .read(ChartImage::Back)
                .image
                .as_ref()
                .unwrap()
                .native(),
            [512, 384]
        );
        // Drawn again, the stage file rewritten (another size): read again,
        // the back image kept.
        w.committed();
        std::thread::sleep(Duration::from_millis(20));
        png(d.path(), "s.png", [320, 240], [70, 80, 90]);
        w.want(want(d.path(), Some(SIDE)));
        step_until(&mut w, &coord, Spawner::SYSTEM, ready);
        let third = w.current().unwrap();
        assert_eq!(
            third
                .read(ChartImage::Stage)
                .image
                .as_ref()
                .unwrap()
                .native(),
            [320, 240]
        );
        assert!(std::ptr::eq(
            second.read(ChartImage::Back),
            third.read(ChartImage::Back)
        ));
        assert_eq!(w.begun, 3);
    }

    /// A thread that cannot start leaves both images absent as failed,
    /// never a pending wait and never the run's failure.
    #[test]
    fn a_refused_thread_leaves_the_images_absent() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        w.step(
            Some((&coord, Spawner::refusing_owned(&[JobKind::ChartImages]))),
            &|| false,
        );
        assert!(w.idle());
        let set = w.current().unwrap();
        assert_eq!(set.read(ChartImage::Stage).image, Err(Absent::Failed));
        assert_eq!(set.read(ChartImage::Back).image, Err(Absent::Failed));
    }

    /// A chart left while its job runs: the job, cancelled through the
    /// choice's scope, is kept until its thread ends, and the next chart's
    /// job begins only then (one worker at a time).
    #[test]
    fn the_next_job_waits_for_the_last_ones_thread() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        png(a.path(), "s.png", [640, 480], [10, 20, 30]);
        png(b.path(), "s.png", [320, 240], [70, 80, 90]);
        let coord = Coordinator::new(CAPS);
        let mut w = ChartImageWorker::default();
        w.want(want(a.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
        assert!(!w.idle(), "A's job runs");
        // The chart is left: its scope advanced, its want forgotten.
        coord.advance(Scope::Choice);
        w.forget();
        w.want(want(b.path(), Some(SIDE)));
        w.step(Some((&coord, Spawner::SYSTEM)), &|| false);
        // Either A's thread already ended and B's began, or B waits: never
        // two jobs alive.
        step_until(&mut w, &coord, Spawner::SYSTEM, |w| {
            w.current().is_some() && w.idle()
        });
        let set = w.current().unwrap();
        assert_eq!(
            set.read(ChartImage::Stage).image.as_ref().unwrap().native(),
            [320, 240]
        );
        assert_eq!(w.begun, 2);
    }

    /// With no coordinator (the direct launch) the images are read at once
    /// on the calling thread; a target with no chart layer reads none.
    #[test]
    fn the_direct_launch_reads_at_once() {
        let d = tempfile::tempdir().unwrap();
        png(d.path(), "s.png", [640, 480], [10, 20, 30]);
        let mut w = ChartImageWorker::default();
        w.want(want(d.path(), Some(SIDE)));
        w.step(None, &|| false);
        assert!(w.idle());
        let set = w.current().unwrap();
        assert!(set.read(ChartImage::Stage).image.is_ok());
        let mut none = ChartImageWorker::default();
        none.want(want(d.path(), None));
        none.step(None, &|| false);
        assert!(none.current().is_none());
        assert_eq!(none.begun, 0);
    }
}
