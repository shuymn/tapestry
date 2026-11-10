//! The practice page's own jobs (design.md "State Model", the practice
//! page; "Load Pipeline"): the chosen chart's 1× bank and its loudness
//! measurement as owned jobs ([`game::coord::OwnedJob`]), begun for the
//! choice and going on as speculative work on the page.
//!
//! - **Their thread's mode** follows the class they compute in
//!   ([`JobMode`]): it enters Windows background mode at a speculative
//!   class and leaves it at the foreground, each call checked. A refused
//!   entry runs no more of the job and is logged once; a refused exit is
//!   logged once and the job goes on in the foreground at background
//!   priority. A call that unwinds is the job's failure (its coordinator
//!   keeps it).
//! - **They move at their own checkpoints**: the bank's decode at each of
//!   its own thread's files ([`crate::selection::decode_on_permits`]), its
//!   other workers retiring at their next file once the foreground is not
//!   wanted; the measurement before each block.
//! - **Their stops are their coordinator's and their mode's alone**: the
//!   job's cancel and its owner's stop (`Ctx::cancelled`), and a refused
//!   entry. They read none of the run's controls, so a Start's quit, which
//!   ends only the Start, never cancels them.
//!
//! The page that owns them comes with the decision screen (docs/stage3.md
//! S10); until then nothing begins them.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the practice page begins them with the decision screen"
    )
)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bms_model::{Chart, Speed};
use game::coord::{
    Class, Coordinator, Ctx, JobKey, JobKind, LoadFailure, OwnedJob, Owner, Spawner, Subscriber,
};

use crate::priority::ThreadMode;

/// What the page's 1× bank job gives: the bank, or why there is none (a
/// cap, or its cancel).
pub type PageBank = Result<Arc<audio::SoundBank>, audio::BankError>;

/// What the page's measurement job gives: the measurement, none when the
/// chart plays at the headroom, or its cancel.
pub type PageMeasure = Result<Option<audio::Measurement>, crate::loudness::Cancelled>;

/// Where a page job calls its test probe (tests).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageAt {
    /// A decode's worker asks before a file: its index (0 is the job's own
    /// thread).
    File(usize),
    /// The bank's decode asks its cancel (before each file, packet and
    /// chunk).
    Cancel,
    /// That ask said stop.
    Stopped,
    /// The measurement asks before a block.
    Block,
}

/// A page job's test probe (tests).
#[cfg(test)]
pub type PageProbe = Arc<dyn Fn(PageAt) + Send + Sync>;

/// A page job thread's mode, changed on the job's thread at each change of
/// the class it computes in (its coordinator's class hook).
pub struct JobMode {
    mode: Arc<dyn ThreadMode>,
    /// What the job is, for its log line.
    what: String,
    /// The thread is in background mode.
    background: AtomicBool,
    /// An entry was refused: the job runs no more.
    refused: AtomicBool,
    /// A refused call was logged.
    logged: AtomicBool,
    /// Refused calls, and lines logged for them (tests).
    #[cfg(test)]
    refusals: (
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    ),
}

impl std::fmt::Debug for JobMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobMode")
            .field("what", &self.what)
            .field("refused", &self.refused())
            .finish_non_exhaustive()
    }
}

impl JobMode {
    /// The mode of a job doing `what` (its log lines begin with it), through
    /// `mode`; its thread starts in the normal mode.
    pub fn new(mode: Arc<dyn ThreadMode>, what: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            mode,
            what: what.into(),
            background: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            logged: AtomicBool::new(false),
            #[cfg(test)]
            refusals: Default::default(),
        })
    }

    /// The class hook: background mode entered at a speculative class, left
    /// at the foreground. A refused entry marks the job refused; a refused
    /// exit leaves it in the foreground at background priority. Either is
    /// logged once.
    pub fn on_class(&self, class: Class) {
        let background = self.background.load(Ordering::Relaxed);
        match class {
            Class::Foreground if background => match self.mode.leave_background() {
                Ok(()) => self.background.store(false, Ordering::Relaxed),
                Err(e) => self.refusal(&format!(
                    "{}: background mode could not be left ({e}); it goes on at background priority",
                    self.what
                )),
            },
            Class::Speculative | Class::Maintenance if !background && !self.refused() => {
                match self.mode.enter_background() {
                    Ok(()) => self.background.store(true, Ordering::Relaxed),
                    Err(e) => {
                        self.refused.store(true, Ordering::Relaxed);
                        self.refusal(&format!(
                            "{}: background mode refused ({e}); it runs no background work",
                            self.what
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether an entry was refused: the job runs no more speculative work.
    pub fn refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }

    /// Logs a refused call, the first one only.
    fn refusal(&self, line: &str) {
        #[cfg(test)]
        self.refusals.0.fetch_add(1, Ordering::Relaxed);
        if !self.logged.swap(true, Ordering::Relaxed) {
            #[cfg(test)]
            self.refusals.1.fetch_add(1, Ordering::Relaxed);
            crate::loudness::log_line(line, true);
        }
    }

    /// The lines logged for refused calls, and the refused calls (tests).
    #[cfg(test)]
    pub fn refusals(&self) -> (usize, usize) {
        (
            self.refusals.1.load(Ordering::Relaxed),
            self.refusals.0.load(Ordering::Relaxed),
        )
    }
}

/// Begins the 1× bank of `chart` (at `path`) at `rate` as an owned job of
/// base class speculative for `subscriber`, promoted from the start when
/// `promoted`, answered for by `owner`; its thread's mode through `mode`.
///
/// # Errors
///
/// As [`Coordinator::spawn_owned`]: its key runs, or its thread could not
/// start.
pub fn spawn_page_bank(
    coord: &Coordinator,
    spawner: Spawner,
    subscriber: Subscriber,
    (path, chart, rate): (PathBuf, Arc<Chart>, u32),
    (promoted, owner): (bool, Owner),
    mode: Arc<JobMode>,
    #[cfg(test)] probe: Option<PageProbe>,
) -> Result<OwnedJob<PageBank>, LoadFailure> {
    let key = JobKey {
        kind: JobKind::Bank,
        path,
        speed: Speed::NORMAL,
    };
    let hook = Arc::clone(&mode);
    let pool = coord.clone();
    coord.spawn_owned(
        spawner,
        key,
        (Class::Speculative, promoted, owner),
        subscriber,
        move |class| hook.on_class(class),
        move |ctx: &Ctx<'_>| {
            #[cfg(test)]
            let at = |p: PageAt| {
                if let Some(probe) = &probe {
                    probe(p);
                }
            };
            #[cfg(test)]
            let at_file = |worker: usize| at(PageAt::File(worker));
            let cancel = || {
                #[cfg(test)]
                at(PageAt::Cancel);
                let stop = ctx.cancelled() || mode.refused();
                #[cfg(test)]
                if stop {
                    at(PageAt::Stopped);
                }
                stop
            };
            crate::selection::decode_on_permits(
                ctx,
                &pool,
                (&chart, rate, Speed::NORMAL),
                &|_, _| {},
                &cancel,
                #[cfg(test)]
                crate::selection::DecodeProbe {
                    load: None,
                    at_file: Some(&at_file),
                },
            )
            .map(Arc::new)
        },
    )
}

/// Begins the measurement of `chart` (at `path`, named `name` in the log)
/// over its 1× `bank` as an owned job of base class speculative for
/// `subscriber`, promoted from the start when `promoted`, answered for by
/// `owner`; its thread's mode through `mode`.
///
/// # Errors
///
/// As [`Coordinator::spawn_owned`]: its key runs, or its thread could not
/// start.
pub fn spawn_page_measure(
    coord: &Coordinator,
    spawner: Spawner,
    subscriber: Subscriber,
    (path, chart, bank, name): (PathBuf, Arc<Chart>, Arc<audio::SoundBank>, String),
    (promoted, owner): (bool, Owner),
    mode: Arc<JobMode>,
    #[cfg(test)] probe: Option<PageProbe>,
) -> Result<OwnedJob<PageMeasure>, LoadFailure> {
    let key = JobKey {
        kind: JobKind::Measure,
        path,
        speed: Speed::NORMAL,
    };
    let hook = Arc::clone(&mode);
    coord.spawn_owned(
        spawner,
        key,
        (Class::Speculative, promoted, owner),
        subscriber,
        move |class| hook.on_class(class),
        move |ctx: &Ctx<'_>| {
            let mut host = PageHost {
                ctx,
                mode: &mode,
                #[cfg(test)]
                probe: probe.as_ref(),
            };
            crate::loudness::measure(&chart, &bank, None, &name, &mut host)
        },
    )
}

/// What the page's measurement lends the analysis, on the job's thread.
struct PageHost<'a, 'c> {
    ctx: &'a Ctx<'c>,
    mode: &'a JobMode,
    #[cfg(test)]
    probe: Option<&'a PageProbe>,
}

impl crate::loudness::Host for PageHost<'_, '_> {
    fn at_normal(&mut self) -> anyhow::Result<(Chart, Arc<audio::SoundBank>)> {
        anyhow::bail!("the page measures its 1× bank only")
    }

    fn measuring(&mut self, _name: &str) {}

    /// Its own block-boundary checkpoint: the permit moves to the class the
    /// job wants (the mode changed with it); it stops at its cancel, its
    /// owner's stop, a class with no permit, or a refused entry.
    fn cancelled(&mut self) -> bool {
        #[cfg(test)]
        if let Some(probe) = self.probe {
            probe(PageAt::Block);
        }
        self.ctx.checkpoint() || self.mode.refused()
    }
}

#[cfg(test)]
mod tests;
